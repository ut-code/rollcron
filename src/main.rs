mod config;
mod git;
mod scheduler;

use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::interval;

/// Tracks running job counts (supports multiple concurrent instances)
pub type RunningJobs = Arc<RwLock<HashMap<String, usize>>>;

/// Tracks running job handles (persists across scheduler restarts)
pub type JobHandles = Arc<tokio::sync::Mutex<HashMap<String, Vec<tokio::task::JoinHandle<()>>>>>;

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
    sync_job_dirs(&sot_path, &initial_jobs, &HashMap::new())?;

    let (tx, mut rx) = tokio::sync::watch::channel((initial_runner, initial_jobs));

    // Shared map of currently running job IDs -> instance count
    let running_jobs: RunningJobs = Arc::new(RwLock::new(HashMap::new()));

    // Shared map of job handles (persists across scheduler restarts)
    let job_handles: JobHandles = Arc::new(tokio::sync::Mutex::new(HashMap::new()));

    // Spawn auto-sync task
    let source_clone = source.clone();
    let pull_interval = args.pull_interval;
    let running_jobs_clone = Arc::clone(&running_jobs);
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
                    let running = running_jobs_clone.read().await;
                    if let Err(e) = sync_job_dirs(&sot, &jobs, &running) {
                        eprintln!("[rollcron] Failed to sync job dirs: {}", e);
                        continue;
                    }
                    drop(running);
                    let _ = tx.send((runner, jobs));
                }
                Err(e) => eprintln!("[rollcron] Failed to reload config: {}", e),
            }
        }
    });

    // Main scheduler loop
    loop {
        let (runner, jobs) = rx.borrow_and_update().clone();
        let sot = sot_path.clone();
        let running_jobs_ref = Arc::clone(&running_jobs);
        let job_handles_ref = Arc::clone(&job_handles);
        tokio::select! {
            result = scheduler::run_scheduler(jobs, sot, runner, running_jobs_ref, job_handles_ref) => {
                match result {
                    Ok(()) => {
                        // Scheduler exited normally (shouldn't happen with infinite loop)
                        eprintln!("[rollcron] Scheduler exited unexpectedly, restarting...");
                    }
                    Err(e) => {
                        eprintln!("[rollcron] Scheduler error: {}", e);
                        eprintln!("[rollcron] Restarting scheduler in 5 seconds...");
                        tokio::time::sleep(Duration::from_secs(5)).await;
                    }
                }
            }
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

fn sync_job_dirs(
    sot_path: &PathBuf,
    jobs: &[config::Job],
    running_jobs: &HashMap<String, usize>,
) -> Result<()> {
    for job in jobs {
        if running_jobs.get(&job.id).copied().unwrap_or(0) > 0 {
            println!(
                "[rollcron] Skipping sync for job '{}' (currently running)",
                job.id
            );
            continue;
        }
        let job_dir = git::get_job_dir(sot_path, &job.id);
        git::sync_to_job_dir(sot_path, &job_dir)?;
    }
    Ok(())
}
