mod actor;
mod config;
mod env;
mod git;
mod logging;
mod webhook;

use actor::runner::{GetJobIds, GracefulShutdown, Initialize, RunnerActor};
use anyhow::{Context, Result};
use clap::Parser;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tracing::{error, info};
use xtra::prelude::*;

const CONFIG_FILE: &str = "rollcron.yaml";

#[derive(Parser)]
#[command(name = "rollcron", about = "Auto-pulling cron scheduler")]
struct Args {
    /// Path to local repo or remote URL (https://... or git@...)
    repo: String,

    /// Pull interval in seconds
    #[arg(short = 'i', long, default_value = "3600")]
    pull_interval: u64,
}

/// RAII guard that cleans up cache directories on drop.
struct CacheGuard {
    sot_path: PathBuf,
    job_ids: Vec<String>,
}

impl CacheGuard {
    fn new(sot_path: PathBuf) -> Self {
        Self {
            sot_path,
            job_ids: Vec::new(),
        }
    }

    fn set_job_ids(&mut self, ids: Vec<String>) {
        self.job_ids = ids;
    }

    /// Disarm the guard (cleanup already handled manually).
    #[allow(dead_code)]
    fn disarm(self) {
        std::mem::forget(self);
    }
}

impl Drop for CacheGuard {
    fn drop(&mut self) {
        git::cleanup_cache_dir(&self.sot_path, &self.job_ids);
    }
}

/// Waits for either SIGINT (Ctrl+C) or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("Failed to register SIGTERM handler");
        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        ctrl_c.await.ok();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    logging::init();
    let args = Args::parse();

    // Expand shell variables (~, $VAR) and canonicalize local paths
    let expanded_repo = env::expand_string(&args.repo);
    let source = if expanded_repo.starts_with('/') || expanded_repo.starts_with('.') {
        PathBuf::from(&expanded_repo)
            .canonicalize()?
            .to_str()
            .context("Path contains invalid UTF-8")?
            .to_string()
    } else {
        expanded_repo
    };

    info!(source = %source, pull_interval = args.pull_interval, "Starting rollcron");

    // Initial clone
    let sot_path = git::generate_cache_path(&source);
    git::clone_to(&source, &sot_path)?;
    info!(cache = %sot_path.display(), "Repository ready");

    // RAII guard ensures sot_path is cleaned up even on early exit
    let mut cache_guard = CacheGuard::new(sot_path.clone());

    let (initial_runner, initial_jobs) = load_config(&sot_path)?;

    // Spawn Runner actor
    let runner = xtra::spawn_tokio(
        RunnerActor::new(
            Duration::from_secs(args.pull_interval),
            sot_path.clone(),
            initial_runner,
        ),
        Mailbox::unbounded(),
    );

    // Initialize with jobs
    if let Err(e) = runner.send(Initialize { jobs: initial_jobs }).await {
        error!(error = %e, "Failed to initialize jobs");
        return Ok(());
    }

    // Wait for shutdown signal (SIGINT or SIGTERM)
    shutdown_signal().await;
    info!("Shutting down...");

    // Get job IDs for cleanup (fall back to filesystem scan if actor is dead)
    let job_ids = runner.send(GetJobIds).await.unwrap_or_default();
    cache_guard.set_job_ids(job_ids);

    // Graceful shutdown
    let _ = runner.send(GracefulShutdown).await;

    // Cleanup via guard drop (disarm and run manually to log any issues)
    // The guard will run cleanup in its Drop, so just let it go out of scope.
    drop(cache_guard);

    Ok(())
}

fn load_config(sot_path: &Path) -> Result<(config::RunnerConfig, Vec<config::Job>)> {
    let config_path = sot_path.join(CONFIG_FILE);
    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", config_path.display(), e))?;
    config::parse_config(&content)
}
