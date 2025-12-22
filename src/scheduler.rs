use crate::config::{Concurrency, Job, RetryConfig, RunnerConfig, TimezoneConfig};
use crate::git;
use chrono::{Local, TimeZone, Utc};
use rand::Rng;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use xtra::prelude::*;
use xtra::refcount::Weak;

/// Unified scheduler actor
pub struct Scheduler {
    // Config
    jobs: Vec<Job>,
    sot_path: PathBuf,
    runner: RunnerConfig,

    // Sync state
    pending_syncs: HashSet<String>,

    // Job state
    running_jobs: HashMap<String, usize>,
    job_handles: HashMap<String, Vec<JoinHandle<()>>>,
}

impl Scheduler {
    pub fn new(jobs: Vec<Job>, sot_path: PathBuf, runner: RunnerConfig) -> Self {
        Self {
            jobs,
            sot_path,
            runner,
            pending_syncs: HashSet::new(),
            running_jobs: HashMap::new(),
            job_handles: HashMap::new(),
        }
    }

    fn try_sync_job(&mut self, job_id: &str) -> anyhow::Result<bool> {
        if self.pending_syncs.remove(job_id) {
            println!("[job:{}] Syncing directory", job_id);
            let job_dir = git::get_job_dir(&self.sot_path, job_id);
            git::sync_to_job_dir(&self.sot_path, &job_dir)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn cleanup_finished_handles(&mut self, job_id: &str) {
        if let Some(handles) = self.job_handles.get_mut(job_id) {
            handles.retain(|h| !h.is_finished());
            if handles.is_empty() {
                self.job_handles.remove(job_id);
            }
        }
    }

    fn running_count(&self, job_id: &str) -> usize {
        self.job_handles.get(job_id).map(|v| v.len()).unwrap_or(0)
    }
}

impl Actor for Scheduler {
    type Stop = ();

    async fn started(&mut self, mailbox: &Mailbox<Self>) -> Result<(), Self::Stop> {
        // Spawn internal ticker
        let addr = mailbox.address();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                if addr.send(Tick).await.is_err() {
                    break;
                }
            }
        });
        Ok(())
    }

    async fn stopped(self) -> Self::Stop {}
}

// === Messages ===

/// Internal tick to check cron schedules
pub struct Tick;

impl Handler<Tick> for Scheduler {
    type Return = ();

    async fn handle(&mut self, _msg: Tick, ctx: &mut Context<Self>) {
        let addr = ctx.mailbox().address();

        for job in &self.jobs.clone() {
            let is_due = match &self.runner.timezone {
                TimezoneConfig::Utc => is_job_due(&job.schedule, Utc),
                TimezoneConfig::Inherit => is_job_due(&job.schedule, Local),
                TimezoneConfig::Named(tz) => is_job_due(&job.schedule, *tz),
            };

            if is_due {
                // Try sync before running
                if let Err(e) = self.try_sync_job(&job.id) {
                    eprintln!("[job:{}] Sync failed: {}", job.id, e);
                    continue;
                }

                self.handle_job_trigger(job, addr.clone()).await;
            }
        }
    }
}

/// Mark jobs as needing sync (sent after git pull)
pub struct SyncRequest {
    pub job_ids: Vec<String>,
    pub sot_path: PathBuf,
}

impl Handler<SyncRequest> for Scheduler {
    type Return = ();

    async fn handle(&mut self, msg: SyncRequest, _ctx: &mut Context<Self>) {
        self.sot_path = msg.sot_path;
        self.pending_syncs.extend(msg.job_ids);
    }
}

/// Update config (sent after git pull with new config)
pub struct ConfigUpdate {
    pub jobs: Vec<Job>,
    pub runner: RunnerConfig,
}

impl Handler<ConfigUpdate> for Scheduler {
    type Return = ();

    async fn handle(&mut self, msg: ConfigUpdate, _ctx: &mut Context<Self>) {
        self.jobs = msg.jobs;
        self.runner = msg.runner;
        println!("[rollcron] Config updated");
    }
}

/// Notification when a job completes
pub struct JobCompleted {
    pub job_id: String,
}

impl Handler<JobCompleted> for Scheduler {
    type Return = ();

    async fn handle(&mut self, msg: JobCompleted, _ctx: &mut Context<Self>) {
        if let Some(count) = self.running_jobs.get_mut(&msg.job_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                self.running_jobs.remove(&msg.job_id);
            }
        }
    }
}

// === Job Execution Logic ===

impl Scheduler {
    async fn handle_job_trigger(&mut self, job: &Job, addr: Address<Scheduler, Weak>) {
        let tag = format!("[job:{}]", job.id);

        self.cleanup_finished_handles(&job.id);
        let running_count = self.running_count(&job.id);

        match job.concurrency {
            Concurrency::Parallel => {
                self.spawn_job(job, addr);
            }
            Concurrency::Wait => {
                if running_count > 0 {
                    println!("{} Waiting for {} previous run(s) to complete", tag, running_count);
                    if let Some(handles) = self.job_handles.remove(&job.id) {
                        for handle in handles {
                            let _ = handle.await;
                        }
                    }
                    self.cleanup_finished_handles(&job.id);
                }
                self.spawn_job(job, addr);
            }
            Concurrency::Skip => {
                if running_count > 0 {
                    println!("{} Skipped ({} instance(s) still active)", tag, running_count);
                } else {
                    self.spawn_job(job, addr);
                }
            }
            Concurrency::Replace => {
                if running_count > 0 {
                    println!("{} Replacing {} previous run(s)", tag, running_count);
                    if let Some(handles) = self.job_handles.remove(&job.id) {
                        let abort_count = handles.len();
                        for handle in handles {
                            handle.abort();
                        }
                        if let Some(count) = self.running_jobs.get_mut(&job.id) {
                            *count = count.saturating_sub(abort_count);
                            if *count == 0 {
                                self.running_jobs.remove(&job.id);
                            }
                        }
                    }
                }
                self.spawn_job(job, addr);
            }
        }
    }

    fn spawn_job(&mut self, job: &Job, addr: Address<Scheduler, Weak>) {
        let job_id = job.id.clone();
        let job = job.clone();
        let work_dir = resolve_work_dir(&self.sot_path, &job.id, &job.working_dir);

        // Increment running count
        *self.running_jobs.entry(job_id.clone()).or_insert(0) += 1;

        let handle = tokio::spawn(async move {
            execute_job(&job, &work_dir).await;
            // Notify completion
            let _ = addr.send(JobCompleted { job_id: job.id }).await;
        });

        self.job_handles.entry(job_id).or_default().push(handle);
    }
}

// === Helper Functions ===

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
    let child = match Command::new("sh")
        .args(["-c", &job.command])
        .current_dir(work_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return CommandResult::ExecError(e.to_string()),
    };

    let result = tokio::time::timeout(job.timeout, child.wait_with_output()).await;

    match result {
        Ok(Ok(output)) => CommandResult::Completed(output),
        Ok(Err(e)) => CommandResult::ExecError(e.to_string()),
        Err(_) => CommandResult::Timeout,
    }
}

enum CommandResult {
    Completed(std::process::Output),
    ExecError(String),
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
        let backoff_0 = calculate_backoff(&retry, 0);
        assert!(backoff_0 >= Duration::from_secs(1));
        assert!(backoff_0 <= Duration::from_millis(1250));

        let backoff_1 = calculate_backoff(&retry, 1);
        assert!(backoff_1 >= Duration::from_secs(2));
        assert!(backoff_1 <= Duration::from_millis(2250));

        let backoff_2 = calculate_backoff(&retry, 2);
        assert!(backoff_2 >= Duration::from_secs(4));
        assert!(backoff_2 <= Duration::from_millis(4250));

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
            assert!(backoff >= Duration::from_secs(1));
            assert!(backoff <= Duration::from_millis(1500));
        }
    }

    #[test]
    fn backoff_auto_inferred_jitter() {
        let retry = RetryConfig {
            max: 3,
            delay: Duration::from_secs(10),
            jitter: None,
        };

        for _ in 0..10 {
            let backoff = calculate_backoff(&retry, 0);
            assert!(backoff >= Duration::from_secs(10));
            assert!(backoff <= Duration::from_millis(12500));
        }

        for _ in 0..10 {
            let backoff = calculate_backoff(&retry, 1);
            assert!(backoff >= Duration::from_secs(20));
            assert!(backoff <= Duration::from_millis(22500));
        }
    }
}
