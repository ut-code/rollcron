use crate::config::{Concurrency, Job, RetryConfig, RunnerConfig};
use crate::git;
use anyhow::Result;
use chrono::{TimeZone, Utc};
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
            let is_due = match runner.timezone {
                Some(tz) => is_job_due(&job.schedule, tz),
                None => is_job_due(&job.schedule, Utc),
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
    retry.delay.saturating_mul(2u32.saturating_pow(attempt))
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
        };
        assert_eq!(calculate_backoff(&retry, 0), Duration::from_secs(1));
        assert_eq!(calculate_backoff(&retry, 1), Duration::from_secs(2));
        assert_eq!(calculate_backoff(&retry, 2), Duration::from_secs(4));
        assert_eq!(calculate_backoff(&retry, 3), Duration::from_secs(8));
    }

    #[tokio::test]
    async fn job_retry_on_failure() {
        let mut job = make_job("exit 1", 10);
        job.retry = Some(RetryConfig {
            max: 2,
            delay: Duration::from_millis(10),
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
        });
        let dir = tempdir().unwrap();
        let start = std::time::Instant::now();
        execute_job(&job, &dir.path().to_path_buf()).await;
        // Should complete quickly without retries
        assert!(start.elapsed() < Duration::from_millis(100));
    }
}
