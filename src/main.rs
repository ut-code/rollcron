mod actor;
mod config;
mod env;
mod git;
mod logging;
mod webhook;

use actor::runner::{GracefulShutdown, Initialize, RunnerActor};
use anyhow::{Context, Result};
use clap::Parser;
use std::path::PathBuf;
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
    #[arg(long, default_value = "3600")]
    pull_interval: u64,
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

    // Initial sync
    let (sot_path, _) = git::ensure_repo(&source)?;
    info!(cache = %sot_path.display(), "Repository ready");

    let (initial_runner, initial_jobs) = load_config(&sot_path)?;

    // Spawn Runner actor
    let runner = xtra::spawn_tokio(
        RunnerActor::new(
            source,
            Duration::from_secs(args.pull_interval),
            sot_path,
            initial_runner,
        ),
        Mailbox::unbounded(),
    );

    // Initialize with jobs
    if let Err(e) = runner.send(Initialize { jobs: initial_jobs }).await {
        error!(error = %e, "Failed to initialize jobs");
        return Ok(());
    }

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    // Graceful shutdown
    let _ = runner.send(GracefulShutdown).await;

    Ok(())
}

fn load_config(sot_path: &PathBuf) -> Result<(config::RunnerConfig, Vec<config::Job>)> {
    let config_path = sot_path.join(CONFIG_FILE);
    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", config_path.display(), e))?;
    config::parse_config(&content)
}
