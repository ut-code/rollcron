mod executor;
mod tick;

use crate::config::{Concurrency, Job, RunnerConfig};
use crate::git;
use std::path::PathBuf;
use tokio::task::JoinHandle;
use tracing::{error, info};
use xtra::prelude::*;

use executor::execute_job;
use tick::is_job_due;

/// Job Actor - manages a single job's lifecycle
pub struct JobActor {
    job: Job,
    sot_path: PathBuf,
    runner: RunnerConfig,
    pending_sync: bool,
    handles: Vec<JoinHandle<()>>,
    tick_handle: Option<JoinHandle<()>>,
    stopping: bool,
}

impl JobActor {
    pub fn new(job: Job, sot_path: PathBuf, runner: RunnerConfig) -> Self {
        Self {
            job,
            sot_path,
            runner,
            pending_sync: true, // Initial sync needed
            handles: Vec::new(),
            tick_handle: None,
            stopping: false,
        }
    }

    fn try_sync(&mut self) -> anyhow::Result<bool> {
        if self.pending_sync {
            info!(target: "rollcron::job", job_id = %self.job.id, "Syncing directory");
            let job_dir = git::get_job_dir(&self.sot_path, &self.job.id);
            git::sync_to_job_dir(&self.sot_path, &job_dir)?;
            self.pending_sync = false;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn cleanup_finished_handles(&mut self) {
        self.handles.retain(|h| !h.is_finished());
    }

    fn running_count(&self) -> usize {
        self.handles.len()
    }
}

impl Actor for JobActor {
    type Stop = ();

    async fn started(&mut self, mailbox: &Mailbox<Self>) -> Result<(), Self::Stop> {
        let addr = mailbox.address();
        self.tick_handle = Some(tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                if addr.send(Tick).await.is_err() {
                    break;
                }
            }
        }));
        info!(target: "rollcron::job", job_id = %self.job.id, "Job actor started");
        Ok(())
    }

    async fn stopped(mut self) -> Self::Stop {
        if let Some(handle) = self.tick_handle.take() {
            handle.abort();
        }
        for handle in self.handles.drain(..) {
            handle.abort();
        }
        info!(target: "rollcron::job", job_id = %self.job.id, "Job actor stopped");
    }
}

// === Messages ===

/// Internal tick to check cron schedule
struct Tick;

impl Handler<Tick> for JobActor {
    type Return = ();

    async fn handle(&mut self, _msg: Tick, _ctx: &mut Context<Self>) {
        if self.stopping || !self.job.enabled {
            return;
        }

        if !is_job_due(&self.job, &self.runner) {
            return;
        }

        info!(target: "rollcron::job", job_id = %self.job.id, "Job triggered");

        // Sync if needed
        if let Err(e) = self.try_sync() {
            error!(target: "rollcron::job", job_id = %self.job.id, error = %e, "Sync failed");
            return;
        }

        self.handle_trigger().await;
    }
}

/// Update job configuration
pub struct Update(pub Job);

impl Handler<Update> for JobActor {
    type Return = ();

    async fn handle(&mut self, msg: Update, _ctx: &mut Context<Self>) {
        self.job = msg.0;
        info!(target: "rollcron::job", job_id = %self.job.id, "Job config updated");
    }
}

/// Mark job as needing sync
pub struct SyncNeeded {
    pub sot_path: PathBuf,
}

impl Handler<SyncNeeded> for JobActor {
    type Return = ();

    async fn handle(&mut self, msg: SyncNeeded, _ctx: &mut Context<Self>) {
        self.sot_path = msg.sot_path;
        self.pending_sync = true;
    }
}

/// Immediate shutdown
pub struct Shutdown;

impl Handler<Shutdown> for JobActor {
    type Return = ();

    async fn handle(&mut self, _msg: Shutdown, ctx: &mut Context<Self>) {
        self.stopping = true;
        ctx.stop_self();
    }
}

/// Graceful stop - wait for current execution
pub struct GracefulStop;

impl Handler<GracefulStop> for JobActor {
    type Return = ();

    async fn handle(&mut self, _msg: GracefulStop, ctx: &mut Context<Self>) {
        self.stopping = true;
        // Wait for running tasks
        for handle in self.handles.drain(..) {
            let _ = handle.await;
        }
        ctx.stop_self();
    }
}

// === Job Execution Logic ===

impl JobActor {
    async fn handle_trigger(&mut self) {
        self.cleanup_finished_handles();
        let running_count = self.running_count();

        match self.job.concurrency {
            Concurrency::Parallel => {
                self.spawn_job();
            }
            Concurrency::Wait => {
                if running_count > 0 {
                    info!(
                        target: "rollcron::job",
                        job_id = %self.job.id,
                        running_count,
                        "Waiting for previous run(s) to complete"
                    );
                    self.spawn_waiting_job();
                } else {
                    self.spawn_job();
                }
            }
            Concurrency::Skip => {
                if running_count > 0 {
                    info!(
                        target: "rollcron::job",
                        job_id = %self.job.id,
                        running_count,
                        "Skipped (instances still active)"
                    );
                } else {
                    self.spawn_job();
                }
            }
            Concurrency::Replace => {
                if running_count > 0 {
                    info!(
                        target: "rollcron::job",
                        job_id = %self.job.id,
                        running_count,
                        "Replacing previous run(s)"
                    );
                    for handle in self.handles.drain(..) {
                        handle.abort();
                    }
                }
                self.spawn_job();
            }
        }
    }

    fn spawn_job(&mut self) {
        let job = self.job.clone();
        let sot_path = self.sot_path.clone();
        let runner = self.runner.clone();

        let handle = tokio::spawn(async move {
            execute_job(&job, &sot_path, &runner).await;
        });

        self.handles.push(handle);
    }

    fn spawn_waiting_job(&mut self) {
        let job = self.job.clone();
        let sot_path = self.sot_path.clone();
        let runner = self.runner.clone();
        let previous_handles = std::mem::take(&mut self.handles);

        let handle = tokio::spawn(async move {
            for prev_handle in previous_handles {
                let _ = prev_handle.await;
            }
            execute_job(&job, &sot_path, &runner).await;
        });

        self.handles.push(handle);
    }
}
