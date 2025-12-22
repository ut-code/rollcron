mod backoff;
mod executor;

use crate::config::{Concurrency, Job, RunnerConfig, TimezoneConfig};
use crate::git;
use chrono::{Local, TimeZone, Utc};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::task::JoinHandle;
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
}

impl Scheduler {
    pub fn new(jobs: Vec<Job>, sot_path: PathBuf, runner: RunnerConfig) -> Self {
        Self {
            jobs,
            sot_path,
            runner,
            pending_syncs: HashSet::new(),
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

        // Collect due jobs to avoid borrow conflicts
        let due_jobs: Vec<Job> = self
            .jobs
            .iter()
            .filter(|job| match &self.runner.timezone {
                TimezoneConfig::Utc => is_job_due(&job.schedule, Utc),
                TimezoneConfig::Inherit => is_job_due(&job.schedule, Local),
                TimezoneConfig::Named(tz) => is_job_due(&job.schedule, *tz),
            })
            .cloned()
            .collect();

        for job in due_jobs {
            if let Err(e) = self.try_sync_job(&job.id) {
                eprintln!("[job:{}] Sync failed: {}", job.id, e);
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
        println!("[rollcron] Config updated");
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
        let tag = format!("[job:{}]", job.id);

        self.cleanup_finished_handles(&job.id);
        let running_count = self.running_count(&job.id);

        match job.concurrency {
            Concurrency::Parallel => {
                self.spawn_job(job, addr);
            }
            Concurrency::Wait => {
                if running_count > 0 {
                    println!(
                        "{} Waiting for {} previous run(s) to complete",
                        tag, running_count
                    );
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
        let work_dir = resolve_work_dir(&self.sot_path, &job.id, &job.working_dir);

        let handle = tokio::spawn(async move {
            execute_job(&job, &work_dir).await;
            let _ = addr.send(JobCompleted).await;
        });

        self.job_handles.entry(job_id).or_default().push(handle);
    }
}

// === Helper Functions ===

/// Tolerance for schedule matching (accounts for 1-second tick interval)
const SCHEDULE_TOLERANCE_MS: i64 = 1000;

fn is_job_due<Z: TimeZone>(schedule: &cron::Schedule, tz: Z) -> bool
where
    Z::Offset: std::fmt::Display,
{
    let now = Utc::now().with_timezone(&tz);
    if let Some(next) = schedule.upcoming(tz).next() {
        let until_next = (next - now).num_milliseconds();
        until_next <= SCHEDULE_TOLERANCE_MS && until_next >= 0
    } else {
        false
    }
}
