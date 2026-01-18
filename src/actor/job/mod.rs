mod executor;
mod tick;

use crate::actor::runner::{BuildCompleted as RunnerBuildCompleted, JobCompleted, JobFailed, RunnerActor};
use crate::config::{Concurrency, Job, RunnerConfig};
use crate::git;
use chrono::Utc;
use std::path::PathBuf;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tokio::time::{sleep_until, Instant};
use tracing::{error, info, warn};
use xtra::prelude::*;
use xtra::refcount::Weak;

use executor::{execute_build, execute_job, BuildResult};
use tick::next_occurrence;

/// Job Actor - manages a single job's lifecycle
pub struct JobActor {
    job: Job,
    sot_path: PathBuf,
    runner: RunnerConfig,
    runner_addr: Option<Address<RunnerActor, Weak>>,
    self_addr: Option<Address<Self, Weak>>,
    pending_sync: bool,
    handles: Vec<JoinHandle<()>>,
    scheduler_handle: Option<JoinHandle<()>>,
    config_tx: watch::Sender<(Job, RunnerConfig)>,
    stopping: bool,
    // Build state
    build_in_progress: bool,
    build_handle: Option<JoinHandle<()>>,
    pending_copy: bool, // build done, waiting for execution to finish
    run_dir_ready: bool, // run/ directory exists and is ready for execution
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
            self_addr: None,
            pending_sync: true, // Initial sync needed
            handles: Vec::new(),
            scheduler_handle: None,
            config_tx,
            stopping: false,
            build_in_progress: false,
            build_handle: None,
            pending_copy: false,
            run_dir_ready: false,
        }
    }

    fn try_copy(&mut self) -> anyhow::Result<bool> {
        if self.pending_copy && self.running_count() == 0 {
            info!(target: "rollcron::job", job_id = %self.job.id, "Copying build to run directory");
            let build_dir = git::get_build_dir(&self.sot_path, &self.job.id);
            let run_dir = git::get_run_dir(&self.sot_path, &self.job.id);
            git::copy_build_to_run(&build_dir, &run_dir)?;
            self.pending_copy = false;
            self.run_dir_ready = true;
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
        let addr = mailbox.address();
        self.self_addr = Some(addr.clone());
        self.start_scheduler(addr);
        info!(target: "rollcron::job", job_id = %self.job.id, "Job actor started");
        Ok(())
    }

    async fn stopped(mut self) -> Self::Stop {
        if let Some(handle) = self.scheduler_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.build_handle.take() {
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

        let Some(addr) = self.self_addr.clone() else {
            return;
        };

        info!(target: "rollcron::job", job_id = %self.job.id, "Job triggered");

        // Trigger build/sync if pending
        if self.pending_sync && !self.build_in_progress {
            self.start_build(addr.clone());
        }

        // Check if run directory is ready
        if !self.run_dir_ready {
            warn!(
                target: "rollcron::job",
                job_id = %self.job.id,
                "Skipped: run directory not ready (build in progress or first run)"
            );
            return;
        }

        self.handle_trigger(addr).await;
    }
}

/// Update job configuration
pub struct Update {
    pub job: Job,
    pub runner: RunnerConfig,
}

impl Handler<Update> for JobActor {
    type Return = ();

    async fn handle(&mut self, msg: Update, _ctx: &mut Context<Self>) {
        self.job = msg.job;
        self.runner = msg.runner;
        self.update_config();
        info!(target: "rollcron::job", job_id = %self.job.id, "Job config updated");
    }
}

/// Mark job as needing sync (triggers build on next opportunity)
pub struct SyncNeeded {
    pub sot_path: PathBuf,
}

impl Handler<SyncNeeded> for JobActor {
    type Return = ();

    async fn handle(&mut self, msg: SyncNeeded, _ctx: &mut Context<Self>) {
        self.sot_path = msg.sot_path;
        self.pending_sync = true;

        // Start build immediately if not already in progress
        if !self.build_in_progress {
            if let Some(addr) = self.self_addr.clone() {
                self.start_build(addr);
            }
        }
    }
}

/// Internal message: build completed
struct BuildCompleted {
    success: bool,
}

impl Handler<BuildCompleted> for JobActor {
    type Return = ();

    async fn handle(&mut self, msg: BuildCompleted, _ctx: &mut Context<Self>) {
        self.build_in_progress = false;
        self.build_handle = None;

        if msg.success {
            info!(target: "rollcron::job", job_id = %self.job.id, "Build succeeded, scheduling copy");
            self.pending_copy = true;

            // Try to copy immediately if no jobs running
            if let Err(e) = self.try_copy() {
                error!(target: "rollcron::job", job_id = %self.job.id, error = %e, "Copy failed");
            }

            // Notify runner
            if let Some(addr) = &self.runner_addr {
                let _ = addr.send(RunnerBuildCompleted { job_id: self.job.id.clone() }).await;
            }
        } else {
            warn!(target: "rollcron::job", job_id = %self.job.id, "Build failed, keeping old run directory");
        }

        // If there's another pending sync (config changed during build), start another build
        if self.pending_sync && !self.build_in_progress {
            if let Some(addr) = self.self_addr.clone() {
                self.start_build(addr);
            }
        }
    }
}

/// Internal message: try to copy build to run
struct TryCopy;

impl Handler<TryCopy> for JobActor {
    type Return = ();

    async fn handle(&mut self, _msg: TryCopy, _ctx: &mut Context<Self>) {
        if let Err(e) = self.try_copy() {
            error!(target: "rollcron::job", job_id = %self.job.id, error = %e, "Copy failed");
        }
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

/// Graceful stop - wait for current execution and build
pub struct GracefulStop;

impl Handler<GracefulStop> for JobActor {
    type Return = ();

    async fn handle(&mut self, _msg: GracefulStop, ctx: &mut Context<Self>) {
        self.stopping = true;

        // Wait for build if in progress
        if let Some(handle) = self.build_handle.take() {
            let _ = handle.await;
        }

        // Wait for running tasks
        for handle in self.handles.drain(..) {
            let _ = handle.await;
        }
        ctx.stop_self();
    }
}

// === Job Execution Logic ===

impl JobActor {
    fn start_build(&mut self, addr: Address<Self, Weak>) {
        if self.build_in_progress {
            return;
        }

        self.build_in_progress = true;
        self.pending_sync = false;

        let job = self.job.clone();
        let sot_path = self.sot_path.clone();
        let runner = self.runner.clone();

        info!(target: "rollcron::job", job_id = %job.id, "Starting build process");

        let handle = tokio::spawn(async move {
            // Step 1: Sync build directory
            let build_dir = git::get_build_dir(&sot_path, &job.id);
            if let Err(e) = git::sync_to_build_dir(&sot_path, &build_dir) {
                error!(target: "rollcron::job", job_id = %job.id, error = %e, "Build sync failed");
                let _ = addr.send(BuildCompleted { success: false }).await;
                return;
            }

            // Step 2: Run build command (if configured)
            let result = execute_build(&job, &sot_path, &runner).await;

            let success = match result {
                BuildResult::Success => true,
                BuildResult::NoBuild => true, // No build command, treat as success
                BuildResult::Failed { .. } => false,
            };

            let _ = addr.send(BuildCompleted { success }).await;
        });

        self.build_handle = Some(handle);
    }

    async fn handle_trigger(&mut self, addr: Address<Self, Weak>) {
        self.cleanup_finished_handles();
        let running_count = self.running_count();

        match self.job.concurrency {
            Concurrency::Parallel => {
                self.spawn_job(addr);
            }
            Concurrency::Wait => {
                if running_count > 0 {
                    info!(
                        target: "rollcron::job",
                        job_id = %self.job.id,
                        running_count,
                        "Waiting for previous run(s) to complete"
                    );
                    self.spawn_waiting_job(addr);
                } else {
                    self.spawn_job(addr);
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
                    self.spawn_job(addr);
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
                self.spawn_job(addr);
            }
        }
    }

    fn spawn_job(&mut self, self_addr: Address<Self, Weak>) {
        let job_id = self.job.id.clone();
        let job = self.job.clone();
        let sot_path = self.sot_path.clone();
        let runner = self.runner.clone();
        let runner_addr = self.runner_addr.clone();

        let handle = tokio::spawn(async move {
            let success = execute_job(&job, &sot_path, &runner).await;

            // Notify runner
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

            // Try to copy pending build (if any)
            let _ = self_addr.send(TryCopy).await;
        });

        self.handles.push(handle);
    }

    fn spawn_waiting_job(&mut self, self_addr: Address<Self, Weak>) {
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

            // Notify runner
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

            // Try to copy pending build (if any)
            let _ = self_addr.send(TryCopy).await;
        });

        self.handles.push(handle);
    }
}

enum Either<L, R> {
    Left(L),
    Right(R),
}
