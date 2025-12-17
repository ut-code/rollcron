mod config;
mod git;
mod scheduler;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use std::time::Duration;
use tokio::time::interval;

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
    let sot_path = git::ensure_repo(&source)?;
    println!("[rollcron] Cache: {}", sot_path.display());

    let (initial_runner, initial_jobs) = load_config(&sot_path)?;
    sync_job_dirs(&sot_path, &initial_jobs)?;

    let (tx, mut rx) = tokio::sync::watch::channel((initial_runner, initial_jobs));

    // Spawn auto-sync task
    let source_clone = source.clone();
    let pull_interval = args.pull_interval;
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(pull_interval));
        loop {
            ticker.tick().await;

            let sot = match git::ensure_repo(&source_clone) {
                Ok(p) => p,
                Err(e) => {
                    eprintln!("[rollcron] Sync failed: {}", e);
                    continue;
                }
            };
            println!("[rollcron] Synced from upstream");

            match load_config(&sot) {
                Ok((runner, jobs)) => {
                    if let Err(e) = sync_job_dirs(&sot, &jobs) {
                        eprintln!("[rollcron] Failed to sync job dirs: {}", e);
                        continue;
                    }
                    let _ = tx.send((runner, jobs));
                    println!("[rollcron] Synced job directories");
                }
                Err(e) => eprintln!("[rollcron] Failed to reload config: {}", e),
            }
        }
    });

    // Main scheduler loop
    loop {
        let (runner, jobs) = rx.borrow_and_update().clone();
        let sot = sot_path.clone();
        tokio::select! {
            _ = scheduler::run_scheduler(jobs, sot, runner) => {}
            _ = rx.changed() => {
                println!("[rollcron] Config updated, restarting scheduler");
            }
        }
    }
}

fn load_config(sot_path: &PathBuf) -> Result<(config::RunnerConfig, Vec<config::Job>)> {
    let config_path = sot_path.join(CONFIG_FILE);
    let content = std::fs::read_to_string(&config_path)
        .map_err(|e| anyhow::anyhow!("Failed to read {}: {}", config_path.display(), e))?;
    config::parse_config(&content)
}

fn sync_job_dirs(sot_path: &PathBuf, jobs: &[config::Job]) -> Result<()> {
    for job in jobs {
        let job_dir = git::get_job_dir(sot_path, &job.id);
        git::sync_to_job_dir(sot_path, &job_dir)?;
    }
    Ok(())
}
