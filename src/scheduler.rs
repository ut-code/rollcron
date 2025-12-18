use crate::config::{Concurrency, Job, RetryConfig, RunnerConfig, TimezoneConfig};
use crate::git;
use anyhow::Result;
use chrono::{Local, TimeZone, Utc};
use rand::Rng;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;

pub async fn run_scheduler(jobs: Vec<Job>, sot_path: PathBuf, runner: RunnerConfig) -> Result<()> {
    let running: Arc<Mutex<HashMap<String, Vec<JoinHandle<()>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    loop {
        for job in &jobs {
            let is_due = match &runner.timezone {
                TimezoneConfig::Utc => is_job_due(&job.schedule, Utc),
                TimezoneConfig::Inherit => is_job_due(&job.schedule, Local),
                TimezoneConfig::Named(tz) => is_job_due(&job.schedule, *tz),
            };

            if is_due {
                let work_dir = resolve_work_dir(&sot_path, &job.id, &job.working_dir);
                handle_job_trigger(job, work_dir, &running).await;
            }
        }

        sleep(Duration::from_secs(1)).await;
    }
}

fn is_job_due<Z: TimeZone>(schedule: &cron::Schedule, tz: Z) -> bool
where
    Z::Offset: std::fmt::Display,
{
    let now = Utc::now().with_timezone(&tz);
    if let Some(next) = schedule.upcoming(tz).next() {
        let until_next = (next - now).num_milliseconds();
        until_next <= 1000 && until_next >= 0
    } else {
        false
    }
}

fn resolve_work_dir(sot_path: &PathBuf, job_id: &str, working_dir: &Option<String>) -> PathBuf {
    let job_dir = git::get_job_dir(sot_path, job_id);
    match working_dir {
        Some(dir) => job_dir.join(dir),
        None => job_dir,
    }
}

async fn handle_job_trigger(
    job: &Job,
    work_dir: PathBuf,
    running: &Arc<Mutex<HashMap<String, Vec<JoinHandle<()>>>>>,
) {
    let tag = format!("[job:{}]", job.id);
    let mut map = running.lock().await;

    // Clean up finished jobs
    if let Some(handles) = map.get_mut(&job.id) {
        handles.retain(|h| !h.is_finished());
        if handles.is_empty() {
            map.remove(&job.id);
        }
    }

    let running_count = map.get(&job.id).map(|v| v.len()).unwrap_or(0);

    match job.concurrency {
        Concurrency::Parallel => {
            spawn_job(job, work_dir, &mut map);
        }
        Concurrency::Wait => {
            if running_count > 0 {
                println!("{} Waiting for {} previous run(s) to complete", tag, running_count);
                let handles = map.remove(&job.id).unwrap();
                drop(map); // Release lock while waiting
                for handle in handles {
                    let _ = handle.await;
                }
                let mut map = running.lock().await;
                spawn_job(job, work_dir, &mut map);
            } else {
                spawn_job(job, work_dir, &mut map);
            }
        }
        Concurrency::Skip => {
            if running_count > 0 {
                println!("{} Skipped ({} instance(s) still active)", tag, running_count);
            } else {
                spawn_job(job, work_dir, &mut map);
            }
        }
        Concurrency::Replace => {
            if running_count > 0 {
                println!("{} Replacing {} previous run(s)", tag, running_count);
                let handles = map.remove(&job.id).unwrap();
                for handle in handles {
                    handle.abort();
                }
            }
            spawn_job(job, work_dir, &mut map);
        }
    }
}

fn spawn_job(
    job: &Job,
    work_dir: PathBuf,
    map: &mut HashMap<String, Vec<JoinHandle<()>>>,
) {
    let job_id = job.id.clone();
    let job = job.clone();
    let handle = tokio::spawn(async move {
        execute_job(&job, &work_dir).await;
    });

    map.entry(job_id).or_default().push(handle);
}

async fn execute_job(job: &Job, work_dir: &PathBuf) {
    let tag = format!("[job:{}]", job.id);

    // Apply task jitter before first execution
    if let Some(jitter_max) = job.jitter {
        let jitter = generate_jitter(jitter_max);
        if jitter > Duration::ZERO {
            println!("{} Applying jitter: {:?}", tag, jitter);
            sleep(jitter).await;
        }
    }

    let max_attempts = job.retry.as_ref().map(|r| r.max + 1).unwrap_or(1);

    for attempt in 0..max_attempts {
        if attempt > 0 {
            let delay = calculate_backoff(job.retry.as_ref().unwrap(), attempt - 1);
            println!("{} Retry {}/{} after {:?}", tag, attempt, max_attempts - 1, delay);
            sleep(delay).await;
        }

        println!("{} Starting '{}'", tag, job.name);
        println!("{}   command: {}", tag, job.command);

        let result = run_command(job, work_dir).await;
        let success = handle_result(&tag, job, &result);

        if success {
            return;
        }

        if attempt + 1 < max_attempts {
            println!("{} Will retry...", tag);
        }
    }
}

fn calculate_backoff(retry: &RetryConfig, attempt: u32) -> Duration {
    let base_delay = retry.delay.saturating_mul(2u32.saturating_pow(attempt));
    let jitter_max = retry.jitter.unwrap_or_else(|| {
        // Auto-infer jitter as 25% of retry delay when not explicitly set
        retry.delay.saturating_mul(25) / 100
    });
    base_delay.saturating_add(generate_jitter(jitter_max))
}

fn generate_jitter(max: Duration) -> Duration {
    if max.is_zero() {
        return Duration::ZERO;
    }
    let millis = max.as_millis();
    if millis == 0 {
        return Duration::ZERO;
    }
    let jitter_millis = rand::thread_rng().gen_range(0..=millis);
    Duration::from_millis(jitter_millis as u64)
}

async fn run_command(job: &Job, work_dir: &PathBuf) -> CommandResult {
    let result = tokio::time::timeout(job.timeout, async {
        tokio::task::spawn_blocking({
            let cmd = job.command.clone();
            let dir = work_dir.clone();
            move || {
                Command::new("sh")
                    .args(["-c", &cmd])
                    .current_dir(&dir)
                    .output()
            }
        })
        .await
    })
    .await;

    match result {
        Ok(Ok(Ok(output))) => CommandResult::Completed(output),
        Ok(Ok(Err(e))) => CommandResult::ExecError(e.to_string()),
        Ok(Err(e)) => CommandResult::TaskError(e.to_string()),
        Err(_) => CommandResult::Timeout,
    }
}

enum CommandResult {
    Completed(std::process::Output),
    ExecError(String),
    TaskError(String),
    Timeout,
}

fn handle_result(tag: &str, job: &Job, result: &CommandResult) -> bool {
    match result {
        CommandResult::Completed(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            if output.status.success() {
                println!("{} ✓ Completed", tag);
                if !stdout.trim().is_empty() {
                    for line in stdout.lines() {
                        println!("{}   | {}", tag, line);
                    }
                }
                true
            } else {
                eprintln!("{} ✗ Failed (exit code: {:?})", tag, output.status.code());
                if !stderr.trim().is_empty() {
                    for line in stderr.lines() {
                        eprintln!("{}   | {}", tag, line);
                    }
                }
                if !stdout.trim().is_empty() {
                    for line in stdout.lines() {
                        eprintln!("{}   | {}", tag, line);
                    }
                }
                false
            }
        }
        CommandResult::ExecError(e) => {
            eprintln!("{} ✗ Failed to execute: {}", tag, e);
            false
        }
        CommandResult::TaskError(e) => {
            eprintln!("{} ✗ Task error: {}", tag, e);
            false
        }
        CommandResult::Timeout => {
            eprintln!("{} ✗ Timeout after {:?}", tag, job.timeout);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cron::Schedule;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn make_job(cmd: &str, timeout_secs: u64) -> Job {
        Job {
            id: "test".to_string(),
            name: "Test Job".to_string(),
            schedule: Schedule::from_str("* * * * * *").unwrap(),
            command: cmd.to_string(),
            timeout: Duration::from_secs(timeout_secs),
            concurrency: Concurrency::Skip,
            retry: None,
            working_dir: None,
            jitter: None,
        }
    }

    #[tokio::test]
    async fn execute_simple_job() {
        let job = make_job("echo test", 10);
        let dir = tempdir().unwrap();
        execute_job(&job, &dir.path().to_path_buf()).await;
    }

    #[tokio::test]
    async fn job_timeout() {
        let job = make_job("sleep 10", 1);
        let dir = tempdir().unwrap();
        execute_job(&job, &dir.path().to_path_buf()).await;
    }

    #[test]
    fn exponential_backoff_calculation() {
        let retry = RetryConfig {
            max: 5,
            delay: Duration::from_secs(1),
            jitter: None,
        };
        // With auto-inferred jitter (25% of delay), backoff is base + random(0..25%)
        // For attempt 0: base=1s, jitter=0-250ms -> 1000-1250ms
        let backoff_0 = calculate_backoff(&retry, 0);
        assert!(backoff_0 >= Duration::from_secs(1));
        assert!(backoff_0 <= Duration::from_millis(1250));

        // For attempt 1: base=2s, jitter=0-250ms -> 2000-2250ms
        let backoff_1 = calculate_backoff(&retry, 1);
        assert!(backoff_1 >= Duration::from_secs(2));
        assert!(backoff_1 <= Duration::from_millis(2250));

        // For attempt 2: base=4s, jitter=0-250ms -> 4000-4250ms
        let backoff_2 = calculate_backoff(&retry, 2);
        assert!(backoff_2 >= Duration::from_secs(4));
        assert!(backoff_2 <= Duration::from_millis(4250));

        // For attempt 3: base=8s, jitter=0-250ms -> 8000-8250ms
        let backoff_3 = calculate_backoff(&retry, 3);
        assert!(backoff_3 >= Duration::from_secs(8));
        assert!(backoff_3 <= Duration::from_millis(8250));
    }

    #[tokio::test]
    async fn job_retry_on_failure() {
        let mut job = make_job("exit 1", 10);
        job.retry = Some(RetryConfig {
            max: 2,
            delay: Duration::from_millis(10),
            jitter: None,
        });
        let dir = tempdir().unwrap();
        let start = std::time::Instant::now();
        execute_job(&job, &dir.path().to_path_buf()).await;
        // Should have waited at least 10ms + 20ms = 30ms for 2 retries
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    #[tokio::test]
    async fn job_success_no_retry() {
        let mut job = make_job("echo ok", 10);
        job.retry = Some(RetryConfig {
            max: 3,
            delay: Duration::from_millis(100),
            jitter: None,
        });
        let dir = tempdir().unwrap();
        let start = std::time::Instant::now();
        execute_job(&job, &dir.path().to_path_buf()).await;
        // Should complete quickly without retries
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn generate_jitter_bounds() {
        let max = Duration::from_millis(100);
        for _ in 0..10 {
            let jitter = generate_jitter(max);
            assert!(jitter <= max);
        }
    }

    #[test]
    fn generate_jitter_zero() {
        let jitter = generate_jitter(Duration::ZERO);
        assert_eq!(jitter, Duration::ZERO);
    }

    #[tokio::test]
    async fn job_with_task_jitter() {
        let mut job = make_job("echo ok", 10);
        job.jitter = Some(Duration::from_millis(50));
        let dir = tempdir().unwrap();
        let start = std::time::Instant::now();
        execute_job(&job, &dir.path().to_path_buf()).await;
        // Should have applied some jitter (at least started the delay)
        // Can't assert exact timing due to randomness, but we verify it doesn't error
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn backoff_with_jitter() {
        let retry = RetryConfig {
            max: 3,
            delay: Duration::from_secs(1),
            jitter: Some(Duration::from_millis(500)),
        };

        for _ in 0..5 {
            let backoff = calculate_backoff(&retry, 0);
            // Base delay is 1s, jitter adds 0-500ms, so total should be 1000-1500ms
            assert!(backoff >= Duration::from_secs(1));
            assert!(backoff <= Duration::from_millis(1500));
        }
    }

    #[test]
    fn backoff_auto_inferred_jitter() {
        let retry = RetryConfig {
            max: 3,
            delay: Duration::from_secs(10),
            jitter: None, // No explicit jitter
        };

        for _ in 0..10 {
            let backoff = calculate_backoff(&retry, 0);
            // Auto-inferred jitter = 25% of delay = 2.5s
            // Base delay is 10s, jitter adds 0-2500ms, so total should be 10000-12500ms
            assert!(backoff >= Duration::from_secs(10));
            assert!(backoff <= Duration::from_millis(12500));
        }

        // Test with different attempt numbers
        for _ in 0..10 {
            let backoff = calculate_backoff(&retry, 1);
            // Base delay is 20s (10s * 2^1), jitter adds 0-2500ms, so total should be 20000-22500ms
            assert!(backoff >= Duration::from_secs(20));
            assert!(backoff <= Duration::from_millis(22500));
        }
    }
}
