mod config;
mod git;
mod scheduler;

use anyhow::Result;
use clap::Parser;
use scheduler::{ConfigUpdate, Scheduler, SyncRequest};
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::interval;
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
    let args = Args::parse();

    // Canonicalize local paths
    let source = if args.repo.starts_with('/') || args.repo.starts_with('.') {
        PathBuf::from(&args.repo)
            .canonicalize()?
            .to_str()
            .unwrap()
            .to_string()
    } else {
        args.repo.clone()
    };

    println!("[rollcron] Source: {}", source);
    println!("[rollcron] Pull interval: {}s", args.pull_interval);

    // Initial sync
    let (sot_path, _) = git::ensure_repo(&source)?;
    println!("[rollcron] Cache: {}", sot_path.display());

    let (initial_runner, initial_jobs) = load_config(&sot_path)?;

    // Initial sync of all job directories
    for job in &initial_jobs {
        let job_dir = git::get_job_dir(&sot_path, &job.id);
        git::sync_to_job_dir(&sot_path, &job_dir)?;
    }

    // Spawn Scheduler actor
    let scheduler = xtra::spawn_tokio(
        Scheduler::new(initial_jobs, sot_path.clone(), initial_runner),
        Mailbox::unbounded(),
    );

    // Spawn auto-sync task
    let source_clone = source.clone();
    let pull_interval = args.pull_interval;
    let scheduler_clone = scheduler.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(pull_interval));
        loop {
            ticker.tick().await;

            let (sot, update_info) = match git::ensure_repo(&source_clone) {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("[rollcron] Sync failed: {}", e);
                    continue;
                }
            };

            let Some(range) = update_info else {
                continue;
            };

            println!("[rollcron] Pulled {}", range);

            match load_config(&sot) {
                Ok((runner, jobs)) => {
                    // Mark all jobs as needing sync
                    let job_ids: Vec<String> = jobs.iter().map(|j| j.id.clone()).collect();
                    if let Err(e) = scheduler_clone
                        .send(SyncRequest {
                            job_ids,
                            sot_path: sot,
                        })
                        .await
                    {
                        eprintln!("[rollcron] Failed to queue sync: {}", e);
                        continue;
                    }
                    // Update config
                    if let Err(e) = scheduler_clone.send(ConfigUpdate { jobs, runner }).await {
                        eprintln!("[rollcron] Failed to update config: {}", e);
                    }
                }
                Err(e) => eprintln!("[rollcron] Failed to reload config: {}", e),
            }
        }
    });

    // Keep main alive while scheduler runs
    // The scheduler actor runs indefinitely with its internal ticker
    std::future::pending::<()>().await;

    Ok(())
}

fn load_config(sot_path: &PathBuf) -> Result<(config::RunnerConfig, Vec<config::Job>)> {
    let config_path = sot_path.join(CONFIG_FILE);
    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", config_path.display(), e))?;
    config::parse_config(&content)
}
