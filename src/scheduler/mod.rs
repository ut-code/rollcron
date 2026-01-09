mod backoff;
mod executor;

use crate::config::{Concurrency, Job, RunnerConfig, TimezoneConfig};
use crate::git;
use chrono::{Local, TimeZone, Utc};
use croner::Cron;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use xtra::prelude::*;
use xtra::refcount::Weak;

use executor::{execute_job, resolve_work_dir};

/// Unified scheduler actor
pub struct Scheduler {
    jobs: Vec<Job>,
    sot_path: PathBuf,
    runner: RunnerConfig,
    pending_syncs: HashSet<String>,
    job_handles: HashMap<String, Vec<JoinHandle<()>>>,
    tick_handle: Option<JoinHandle<()>>,
}

impl Scheduler {
    pub fn new(jobs: Vec<Job>, sot_path: PathBuf, runner: RunnerConfig) -> Self {
        Self {
            jobs,
            sot_path,
            runner,
            pending_syncs: HashSet::new(),
            job_handles: HashMap::new(),
            tick_handle: None,
        }
    }

    fn try_sync_job(&mut self, job_id: &str) -> anyhow::Result<bool> {
        if self.pending_syncs.remove(job_id) {
            info!(target: "rollcron::scheduler", job_id = %job_id, "Syncing directory");
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
        let addr = mailbox.address();
        self.tick_handle = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                if addr.send(Tick).await.is_err() {
                    break;
                }
            }
        }));
        Ok(())
    }

    async fn stopped(mut self) -> Self::Stop {
        // Abort tick timer
        if let Some(handle) = self.tick_handle.take() {
            handle.abort();
        }
        // Abort all running jobs
        for (_, handles) in self.job_handles.drain() {
            for handle in handles {
                handle.abort();
            }
        }
    }
}

// === Messages ===

/// Internal tick to check cron schedules
pub struct Tick;

impl Handler<Tick> for Scheduler {
    type Return = ();

    async fn handle(&mut self, _msg: Tick, ctx: &mut Context<Self>) {
        let addr = ctx.mailbox().address();

        // Collect due jobs to avoid borrow conflicts
        let due_jobs: Vec<Job> = self
            .jobs
            .iter()
            .filter(|job| {
                if !job.enabled {
                    return false;
                }
                let tz_config = job.timezone.as_ref().unwrap_or(&self.runner.timezone);
                let is_due = match tz_config {
                    TimezoneConfig::Utc => is_job_due(&job.schedule, Utc),
                    TimezoneConfig::Inherit => is_job_due(&job.schedule, Local),
                    TimezoneConfig::Named(tz) => is_job_due(&job.schedule, *tz),
                };
                if is_due {
                    info!(
                        target: "rollcron::scheduler",
                        job_id = %job.id,
                        timezone = ?tz_config,
                        now_utc = %Utc::now(),
                        "Job triggered"
                    );
                }
                is_due
            })
            .cloned()
            .collect();

        for job in due_jobs {
            if let Err(e) = self.try_sync_job(&job.id) {
                error!(target: "rollcron::scheduler", job_id = %job.id, error = %e, "Sync failed");
                continue;
            }

            self.handle_job_trigger(job, addr.clone()).await;
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
        info!(target: "rollcron::scheduler", "Config updated");
    }
}

/// Notification when a job completes
struct JobCompleted;

impl Handler<JobCompleted> for Scheduler {
    type Return = ();

    async fn handle(&mut self, _msg: JobCompleted, _ctx: &mut Context<Self>) {
        // Cleanup is done via cleanup_finished_handles
    }
}

// === Job Execution Logic ===

impl Scheduler {
    async fn handle_job_trigger(&mut self, job: Job, addr: Address<Scheduler, Weak>) {
        self.cleanup_finished_handles(&job.id);
        let running_count = self.running_count(&job.id);

        match job.concurrency {
            Concurrency::Parallel => {
                self.spawn_job(job, addr);
            }
            Concurrency::Wait => {
                if running_count > 0 {
                    info!(
                        target: "rollcron::scheduler",
                        job_id = %job.id,
                        running_count,
                        "Waiting for previous run(s) to complete"
                    );
                    // Spawn waiting job asynchronously to avoid blocking the actor
                    self.spawn_waiting_job(job, addr);
                } else {
                    self.spawn_job(job, addr);
                }
            }
            Concurrency::Skip => {
                if running_count > 0 {
                    info!(
                        target: "rollcron::scheduler",
                        job_id = %job.id,
                        running_count,
                        "Skipped (instances still active)"
                    );
                } else {
                    self.spawn_job(job, addr);
                }
            }
            Concurrency::Replace => {
                if running_count > 0 {
                    info!(
                        target: "rollcron::scheduler",
                        job_id = %job.id,
                        running_count,
                        "Replacing previous run(s)"
                    );
                    if let Some(handles) = self.job_handles.remove(&job.id) {
                        for handle in handles {
                            handle.abort();
                        }
                    }
                }
                self.spawn_job(job, addr);
            }
        }
    }

    fn spawn_job(&mut self, job: Job, addr: Address<Scheduler, Weak>) {
        let job_id = job.id.clone();
        let job_id_for_log = job.id.clone();
        let work_dir = resolve_work_dir(&self.sot_path, &job.id, &job.working_dir);
        let sot_path = self.sot_path.clone();
        let runner = self.runner.clone();

        let handle = tokio::spawn(async move {
            execute_job(&job, &work_dir, &sot_path, &runner).await;
            if let Err(e) = addr.send(JobCompleted).await {
                warn!(target: "rollcron::scheduler", job_id = %job_id_for_log, error = %e, "Failed to notify completion");
            }
        });

        self.job_handles.entry(job_id).or_default().push(handle);
    }

    /// Spawn a job that waits for previous runs to complete before executing.
    /// This avoids blocking the actor by spawning a separate task that handles the waiting.
    fn spawn_waiting_job(&mut self, job: Job, addr: Address<Scheduler, Weak>) {
        let job_id = job.id.clone();
        let job_id_for_log = job.id.clone();
        let work_dir = resolve_work_dir(&self.sot_path, &job.id, &job.working_dir);
        let sot_path = self.sot_path.clone();
        let runner = self.runner.clone();

        // Take ownership of existing handles to wait on them
        let previous_handles = self.job_handles.remove(&job.id).unwrap_or_default();

        let handle = tokio::spawn(async move {
            // Wait for all previous runs to complete
            for prev_handle in previous_handles {
                let _ = prev_handle.await;
            }

            // Now execute the new job
            execute_job(&job, &work_dir, &sot_path, &runner).await;
            if let Err(e) = addr.send(JobCompleted).await {
                warn!(target: "rollcron::scheduler", job_id = %job_id_for_log, error = %e, "Failed to notify completion");
            }
        });

        self.job_handles.entry(job_id).or_default().push(handle);
    }
}

// === Helper Functions ===

/// Tolerance for schedule matching (accounts for 1-second tick interval)
const SCHEDULE_TOLERANCE_MS: i64 = 1000;

fn is_job_due<Z: TimeZone>(schedule: &Cron, tz: Z) -> bool
where
    Z::Offset: std::fmt::Display,
{
    let now = Utc::now().with_timezone(&tz);
    if let Some(next) = schedule.find_next_occurrence(&now, false).ok() {
        let until_next = (next.clone() - now.clone()).num_milliseconds();
        let is_due = until_next <= SCHEDULE_TOLERANCE_MS && until_next >= -SCHEDULE_TOLERANCE_MS;
        if is_due {
            info!(
                target: "rollcron::scheduler",
                now = %now,
                next = %next,
                until_next_ms = until_next,
                "Schedule matched"
            );
        }
        is_due
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use chrono_tz::Asia::Tokyo;
    use std::str::FromStr;

    #[test]
    fn debug_timezone_schedule() {
        // "0 8 * * *" (8:00 AM) - standard 5-field cron
        let schedule = Cron::from_str("0 8 * * *").unwrap();

        let now_utc = Utc::now();
        let now_tokyo = now_utc.with_timezone(&Tokyo);

        println!("Current UTC: {}", now_utc);
        println!("Current Tokyo: {}", now_tokyo);

        // Get next scheduled time in Tokyo timezone
        let next_tokyo = schedule.find_next_occurrence(&now_tokyo, false).unwrap();
        println!("Next scheduled (Tokyo): {}", next_tokyo);

        // Get next scheduled time in UTC
        let next_utc = schedule.find_next_occurrence(&now_utc, false).unwrap();
        println!("Next scheduled (UTC): {}", next_utc);

        // The difference should reflect timezone
        println!("Tokyo next in UTC: {}", next_tokyo.with_timezone(&Utc));
    }

    #[test]
    fn debug_is_job_due_at_specific_time() {
        // Simulate: it's 8:00 AM Tokyo, job should be due
        let schedule = Cron::from_str("0 8 * * *").unwrap();

        // 8:00 AM Tokyo = 23:00 UTC (previous day)
        let fake_now_utc = chrono::Utc.with_ymd_and_hms(2026, 1, 9, 23, 0, 0).unwrap();
        let fake_now_tokyo = fake_now_utc.with_timezone(&Tokyo);
        println!("Fake now UTC: {}", fake_now_utc);
        println!("Fake now Tokyo: {}", fake_now_tokyo);

        // What does find_next_occurrence return at this time?
        let next = schedule.find_next_occurrence(&fake_now_tokyo, false).unwrap();
        println!("Next scheduled: {}", next);

        // Check: is 8:00 AM Tokyo the next time?
        let expected = Tokyo.with_ymd_and_hms(2026, 1, 10, 8, 0, 0).unwrap();
        println!("Expected: {}", expected);
        println!("Match: {}", next == expected);
    }

    #[test]
    fn debug_what_triggers_at_2108() {
        // User reports job triggers at 21:08 JST (9:08 PM)
        // Let's see what schedule would match at this time
        let schedule = Cron::from_str("0 8 * * *").unwrap();

        // 21:08 JST = 12:08 UTC
        println!("=== Testing at 21:08 JST ===");
        let time_2108_jst = Tokyo.with_ymd_and_hms(2026, 1, 9, 21, 8, 0).unwrap();
        println!("Time in JST: {}", time_2108_jst);
        println!("Time in UTC: {}", time_2108_jst.with_timezone(&Utc));

        let next_tokyo = schedule.find_next_occurrence(&time_2108_jst, false).unwrap();
        let next_utc = schedule
            .find_next_occurrence(&time_2108_jst.with_timezone(&Utc), false)
            .unwrap();
        println!("Next (Tokyo tz): {}", next_tokyo);
        println!("Next (UTC tz): {}", next_utc);

        // Check if 21:08 would match any interpretation
        println!("\n=== What cron would trigger at 21:08? ===");
        // "8 21 * * *" = 21:08
        let schedule_2108 = Cron::from_str("8 21 * * *").unwrap();
        let next_2108 = schedule_2108
            .find_next_occurrence(&time_2108_jst, false)
            .unwrap();
        println!("Schedule '8 21 * * *' next: {}", next_2108);
    }
}
