mod executor;
mod tick;

use crate::actor::runner::{JobCompleted, JobFailed, RunnerActor};
use crate::config::{Concurrency, Job, RunnerConfig};
use crate::git;
use chrono::Utc;
use std::path::PathBuf;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant};
use tracing::{error, info};
use xtra::prelude::*;
use xtra::refcount::Weak;

use executor::execute_job;
use tick::next_occurrence;

/// Job Actor - manages a single job's lifecycle
pub struct JobActor {
    job: Job,
    sot_path: PathBuf,
    runner: RunnerConfig,
    runner_addr: Option<Address<RunnerActor, Weak>>,
    pending_sync: bool,
    handles: Vec<JoinHandle<()>>,
    scheduler_handle: Option<JoinHandle<()>>,
    config_tx: watch::Sender<(Job, RunnerConfig)>,
    stopping: bool,
}

impl JobActor {
    pub fn new(
        job: Job,
        sot_path: PathBuf,
        runner: RunnerConfig,
        runner_addr: Option<Address<RunnerActor, Weak>>,
    ) -> Self {
        let (config_tx, _) = watch::channel((job.clone(), runner.clone()));
        Self {
            job,
            sot_path,
            runner,
            runner_addr,
            pending_sync: true, // Initial sync needed
            handles: Vec::new(),
            scheduler_handle: None,
            config_tx,
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

    fn start_scheduler(&mut self, addr: Address<Self, Weak>) {
        // Abort existing scheduler if any
        if let Some(handle) = self.scheduler_handle.take() {
            handle.abort();
        }

        let mut config_rx = self.config_tx.subscribe();

        self.scheduler_handle = Some(tokio::spawn(async move {
            loop {
                let (job, runner) = config_rx.borrow_and_update().clone();

                if !job.enabled {
                    // Disabled - wait for config change
                    if config_rx.changed().await.is_err() {
                        break;
                    }
                    continue;
                }

                let next = match next_occurrence(&job, &runner) {
                    Some(dt) => dt,
                    None => {
                        // No future occurrence, wait for config change
                        if config_rx.changed().await.is_err() {
                            break;
                        }
                        continue;
                    }
                };

                let now = Utc::now();
                let wait_duration = if next > now {
                    (next - now).to_std().unwrap_or_default()
                } else {
                    std::time::Duration::ZERO
                };
                let deadline = Instant::now() + wait_duration;

                info!(
                    target: "rollcron::job",
                    job_id = %job.id,
                    next = %next,
                    wait_secs = wait_duration.as_secs(),
                    "Scheduled"
                );

                tokio::select! {
                    _ = sleep_until(deadline) => {
                        // Time to execute - send tick
                        if addr.send(Tick).await.is_err() {
                            break;
                        }
                    }
                    result = config_rx.changed() => {
                        if result.is_err() {
                            break;
                        }
                        // Config changed, recalculate next occurrence
                        continue;
                    }
                }
            }
        }));
    }

    fn update_config(&mut self) {
        let _ = self.config_tx.send((self.job.clone(), self.runner.clone()));
    }
}

impl Actor for JobActor {
    type Stop = ();

    async fn started(&mut self, mailbox: &Mailbox<Self>) -> Result<(), Self::Stop> {
        self.start_scheduler(mailbox.address());
        info!(target: "rollcron::job", job_id = %self.job.id, "Job actor started");
        Ok(())
    }

    async fn stopped(mut self) -> Self::Stop {
        if let Some(handle) = self.scheduler_handle.take() {
            handle.abort();
        }
        for handle in self.handles.drain(..) {
            handle.abort();
        }
        info!(target: "rollcron::job", job_id = %self.job.id, "Job actor stopped");
    }
}

// === Messages ===

/// Internal tick to trigger job execution
struct Tick;

impl Handler<Tick> for JobActor {
    type Return = ();

    async fn handle(&mut self, _msg: Tick, _ctx: &mut Context<Self>) {
        if self.stopping || !self.job.enabled {
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
        self.update_config();
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
        let job_id = self.job.id.clone();
        let job = self.job.clone();
        let sot_path = self.sot_path.clone();
        let runner = self.runner.clone();
        let runner_addr = self.runner_addr.clone();

        let handle = tokio::spawn(async move {
            let success = execute_job(&job, &sot_path, &runner).await;
            if let Some(addr) = runner_addr {
                let msg = if success {
                    Either::Left(JobCompleted { job_id })
                } else {
                    Either::Right(JobFailed { job_id })
                };
                match msg {
                    Either::Left(m) => { let _ = addr.send(m).await; }
                    Either::Right(m) => { let _ = addr.send(m).await; }
                }
            }
        });

        self.handles.push(handle);
    }

    fn spawn_waiting_job(&mut self) {
        let job_id = self.job.id.clone();
        let job = self.job.clone();
        let sot_path = self.sot_path.clone();
        let runner = self.runner.clone();
        let runner_addr = self.runner_addr.clone();
        let previous_handles = std::mem::take(&mut self.handles);

        let handle = tokio::spawn(async move {
            for prev_handle in previous_handles {
                let _ = prev_handle.await;
            }
            let success = execute_job(&job, &sot_path, &runner).await;
            if let Some(addr) = runner_addr {
                let msg = if success {
                    Either::Left(JobCompleted { job_id })
                } else {
                    Either::Right(JobFailed { job_id })
                };
                match msg {
                    Either::Left(m) => { let _ = addr.send(m).await; }
                    Either::Right(m) => { let _ = addr.send(m).await; }
                }
            }
        });

        self.handles.push(handle);
    }
}

enum Either<L, R> {
    Left(L),
    Right(R),
}
