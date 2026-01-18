mod git_poll;
mod lifecycle;

use crate::actor::job::{JobActor, Shutdown, SyncNeeded, Update};
use crate::config::{self, Job, RunnerConfig};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::task::JoinHandle;
use tracing::{error, info, warn};
use xtra::prelude::*;
use xtra::refcount::Weak;

const CONFIG_FILE: &str = "rollcron.yaml";

/// Runner Actor - manages the lifecycle of all job actors
pub struct RunnerActor {
    pull_interval: Duration,
    sot_path: PathBuf,
    runner_config: RunnerConfig,
    job_actors: HashMap<String, Address<JobActor>>,
    poll_handle: Option<JoinHandle<()>>,
    supervisor_handle: Option<JoinHandle<()>>,
    self_addr: Option<Address<Self, Weak>>,
}

impl RunnerActor {
    pub fn new(
        pull_interval: Duration,
        sot_path: PathBuf,
        runner_config: RunnerConfig,
    ) -> Self {
        Self {
            pull_interval,
            sot_path,
            runner_config,
            job_actors: HashMap::new(),
            poll_handle: None,
            supervisor_handle: None,
            self_addr: None,
        }
    }

    fn spawn_job_actor(&mut self, job: Job) {
        let job_id = job.id.clone();
        let runner_addr = self.self_addr.clone();
        let actor = JobActor::new(
            job,
            self.sot_path.clone(),
            self.runner_config.clone(),
            runner_addr,
        );
        let addr = xtra::spawn_tokio(actor, Mailbox::unbounded());
        self.job_actors.insert(job_id, addr);
    }
}

impl Actor for RunnerActor {
    type Stop = ();

    async fn started(&mut self, mailbox: &Mailbox<Self>) -> Result<(), Self::Stop> {
        let addr = mailbox.address();
        self.self_addr = Some(addr.clone());

        // Start git poll loop
        let sot_path = self.sot_path.clone();
        let pull_interval = self.pull_interval;
        let poll_addr = addr.clone();
        self.poll_handle = Some(tokio::spawn(async move {
            git_poll::run(sot_path, pull_interval, poll_addr).await;
        }));

        // Start supervisor loop
        let supervisor_addr = addr.clone();
        self.supervisor_handle = Some(tokio::spawn(async move {
            lifecycle::supervise(supervisor_addr).await;
        }));

        info!(target: "rollcron::runner", "Runner actor started");
        Ok(())
    }

    async fn stopped(mut self) -> Self::Stop {
        if let Some(handle) = self.poll_handle.take() {
            handle.abort();
        }
        if let Some(handle) = self.supervisor_handle.take() {
            handle.abort();
        }

        // Shutdown all job actors (fire-and-forget)
        for (_, addr) in self.job_actors.drain() {
            tokio::spawn(async move {
                let _ = addr.send(Shutdown).await;
            });
        }

        info!(target: "rollcron::runner", "Runner actor stopped");
    }
}

// === Messages ===

/// Initialize with jobs from config
pub struct Initialize {
    pub jobs: Vec<Job>,
}

impl Handler<Initialize> for RunnerActor {
    type Return = ();

    async fn handle(&mut self, msg: Initialize, _ctx: &mut Context<Self>) {
        for job in msg.jobs {
            let job_id = job.id.clone();
            // Job actor will handle initial build/sync via pending_sync flag
            info!(target: "rollcron::runner", job_id = %job_id, "Spawning job actor");
            self.spawn_job_actor(job);
        }
    }
}

/// Config update after git pull
pub struct ConfigUpdate {
    pub sot_path: PathBuf,
    pub runner: RunnerConfig,
    pub jobs: Vec<Job>,
}

impl Handler<ConfigUpdate> for RunnerActor {
    type Return = ();

    async fn handle(&mut self, msg: ConfigUpdate, _ctx: &mut Context<Self>) {
        self.sot_path = msg.sot_path.clone();
        self.runner_config = msg.runner;

        let new_job_ids: HashMap<String, Job> = msg.jobs.into_iter().map(|j| (j.id.clone(), j)).collect();

        // Find jobs to remove
        let to_remove: Vec<String> = self
            .job_actors
            .keys()
            .filter(|id| !new_job_ids.contains_key(*id))
            .cloned()
            .collect();

        // Remove deleted jobs (fire-and-forget)
        for job_id in to_remove {
            if let Some(addr) = self.job_actors.remove(&job_id) {
                info!(target: "rollcron::runner", job_id = %job_id, "Removing job actor");
                tokio::spawn(async move {
                    let _ = addr.send(Shutdown).await;
                });
            }
        }

        // Update or create jobs
        for (job_id, job) in new_job_ids {
            if let Some(addr) = self.job_actors.get(&job_id) {
                // Update existing job (fire-and-forget)
                let addr = addr.clone();
                let sot_path = msg.sot_path.clone();
                let runner = self.runner_config.clone();
                tokio::spawn(async move {
                    let _ = addr.send(SyncNeeded { sot_path }).await;
                    let _ = addr.send(Update { job, runner }).await;
                });
            } else {
                // Create new job - job actor will handle initial build/sync
                info!(target: "rollcron::runner", job_id = %job_id, "Spawning new job actor");
                self.spawn_job_actor(job);
            }
        }
    }
}

/// Get actor addresses for supervision
pub struct GetJobActors;

impl Handler<GetJobActors> for RunnerActor {
    type Return = HashMap<String, Address<JobActor>>;

    async fn handle(&mut self, _msg: GetJobActors, _ctx: &mut Context<Self>) -> Self::Return {
        self.job_actors.clone()
    }
}

/// Get runner config for webhook notifications
pub struct GetRunnerConfig;

impl Handler<GetRunnerConfig> for RunnerActor {
    type Return = RunnerConfig;

    async fn handle(&mut self, _msg: GetRunnerConfig, _ctx: &mut Context<Self>) -> Self::Return {
        self.runner_config.clone()
    }
}

/// Respawn a job actor that died unexpectedly
pub struct RespawnJob {
    pub job_id: String,
}

impl Handler<RespawnJob> for RunnerActor {
    type Return = ();

    async fn handle(&mut self, msg: RespawnJob, _ctx: &mut Context<Self>) {
        warn!(target: "rollcron::runner", job_id = %msg.job_id, "Respawning job actor after unexpected stop");

        // Re-read config to get job definition
        let config_path = self.sot_path.join(CONFIG_FILE);
        let content = match std::fs::read_to_string(&config_path) {
            Ok(c) => c,
            Err(e) => {
                error!(target: "rollcron::runner", error = %e, "Failed to read config for respawn");
                return;
            }
        };

        let (_, jobs) = match config::parse_config(&content) {
            Ok(c) => c,
            Err(e) => {
                error!(target: "rollcron::runner", error = %e, "Failed to parse config for respawn");
                return;
            }
        };

        if let Some(job) = jobs.into_iter().find(|j| j.id == msg.job_id) {
            self.spawn_job_actor(job);
        }
    }
}

/// Get all job IDs for cleanup
pub struct GetJobIds;

impl Handler<GetJobIds> for RunnerActor {
    type Return = Vec<String>;

    async fn handle(&mut self, _msg: GetJobIds, _ctx: &mut Context<Self>) -> Self::Return {
        self.job_actors.keys().cloned().collect()
    }
}

/// Graceful shutdown
pub struct GracefulShutdown;

impl Handler<GracefulShutdown> for RunnerActor {
    type Return = ();

    async fn handle(&mut self, _msg: GracefulShutdown, ctx: &mut Context<Self>) {
        info!(target: "rollcron::runner", "Initiating graceful shutdown");

        // Send GracefulStop to all job actors and wait
        for (job_id, addr) in &self.job_actors {
            info!(target: "rollcron::runner", job_id = %job_id, "Sending graceful stop");
            let _ = addr.send(crate::actor::job::GracefulStop).await;
        }

        ctx.stop_self();
    }
}

/// Job execution completed successfully
pub struct JobCompleted {
    pub job_id: String,
}

impl Handler<JobCompleted> for RunnerActor {
    type Return = ();

    async fn handle(&mut self, msg: JobCompleted, _ctx: &mut Context<Self>) {
        info!(target: "rollcron::runner", job_id = %msg.job_id, "Job completed");
    }
}

/// Job execution failed after all retries
pub struct JobFailed {
    pub job_id: String,
}

impl Handler<JobFailed> for RunnerActor {
    type Return = ();

    async fn handle(&mut self, msg: JobFailed, _ctx: &mut Context<Self>) {
        warn!(target: "rollcron::runner", job_id = %msg.job_id, "Job failed");
    }
}

/// Build completed for a job
pub struct BuildCompleted {
    pub job_id: String,
}

impl Handler<BuildCompleted> for RunnerActor {
    type Return = ();

    async fn handle(&mut self, msg: BuildCompleted, _ctx: &mut Context<Self>) {
        info!(target: "rollcron::runner", job_id = %msg.job_id, "Build completed");
    }
}
