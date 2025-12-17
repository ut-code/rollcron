use crate::config::{Concurrency, Job};
use crate::git;
use anyhow::Result;
use chrono::Utc;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio::time::sleep;

pub async fn run_scheduler(jobs: Vec<Job>, sot_path: PathBuf) -> Result<()> {
    let running: Arc<Mutex<HashMap<String, Vec<JoinHandle<()>>>>> =
        Arc::new(Mutex::new(HashMap::new()));

    loop {
        let now = Utc::now();

        for job in &jobs {
            if let Some(next) = job.schedule.upcoming(Utc).next() {
                let until_next = (next - now).num_milliseconds();
                if until_next <= 1000 && until_next >= 0 {
                    let work_dir = git::get_job_dir(&sot_path, &job.id);
                    handle_job_trigger(job, work_dir, &running).await;
                }
            }
        }

        sleep(Duration::from_secs(1)).await;
    }
}

async fn handle_job_trigger(
    job: &Job,
    work_dir: PathBuf,
    running: &Arc<Mutex<HashMap<String, Vec<JoinHandle<()>>>>>,
) {
    let tag = format!("[job:{}]", job.id);
    let mut map = running.lock().await;

    // Clean up finished jobs
    if let Some(handles) = map.get_mut(&job.id) {
        handles.retain(|h| !h.is_finished());
        if handles.is_empty() {
            map.remove(&job.id);
        }
    }

    let running_count = map.get(&job.id).map(|v| v.len()).unwrap_or(0);

    match job.concurrency {
        Concurrency::Parallel => {
            spawn_job(job, work_dir, &mut map);
        }
        Concurrency::Wait => {
            if running_count > 0 {
                println!("{} Waiting for {} previous run(s) to complete", tag, running_count);
                let handles = map.remove(&job.id).unwrap();
                drop(map); // Release lock while waiting
                for handle in handles {
                    let _ = handle.await;
                }
                let mut map = running.lock().await;
                spawn_job(job, work_dir, &mut map);
            } else {
                spawn_job(job, work_dir, &mut map);
            }
        }
        Concurrency::Skip => {
            if running_count > 0 {
                println!("{} Skipped ({} instance(s) still active)", tag, running_count);
            } else {
                spawn_job(job, work_dir, &mut map);
            }
        }
        Concurrency::Replace => {
            if running_count > 0 {
                println!("{} Replacing {} previous run(s)", tag, running_count);
                let handles = map.remove(&job.id).unwrap();
                for handle in handles {
                    handle.abort();
                }
            }
            spawn_job(job, work_dir, &mut map);
        }
    }
}

fn spawn_job(
    job: &Job,
    work_dir: PathBuf,
    map: &mut HashMap<String, Vec<JoinHandle<()>>>,
) {
    let job_id = job.id.clone();
    let job = job.clone();
    let handle = tokio::spawn(async move {
        execute_job(&job, &work_dir).await;
    });

    map.entry(job_id).or_default().push(handle);
}

async fn execute_job(job: &Job, work_dir: &PathBuf) {
    let tag = format!("[job:{}]", job.id);

    println!("{} Starting '{}'", tag, job.name);
    println!("{}   command: {}", tag, job.command);

    let result = tokio::time::timeout(job.timeout, async {
        tokio::task::spawn_blocking({
            let cmd = job.command.clone();
            let dir = work_dir.clone();
            move || {
                Command::new("sh")
                    .args(["-c", &cmd])
                    .current_dir(&dir)
                    .output()
            }
        })
        .await
    })
    .await;

    match result {
        Ok(Ok(Ok(output))) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            if output.status.success() {
                println!("{} ✓ Completed", tag);
                if !stdout.trim().is_empty() {
                    for line in stdout.lines() {
                        println!("{}   | {}", tag, line);
                    }
                }
            } else {
                eprintln!("{} ✗ Failed (exit code: {:?})", tag, output.status.code());
                if !stderr.trim().is_empty() {
                    for line in stderr.lines() {
                        eprintln!("{}   | {}", tag, line);
                    }
                }
                if !stdout.trim().is_empty() {
                    for line in stdout.lines() {
                        eprintln!("{}   | {}", tag, line);
                    }
                }
            }
        }
        Ok(Ok(Err(e))) => {
            eprintln!("{} ✗ Failed to execute: {}", tag, e);
        }
        Ok(Err(e)) => {
            eprintln!("{} ✗ Task error: {}", tag, e);
        }
        Err(_) => {
            eprintln!("{} ✗ Timeout after {:?}", tag, job.timeout);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cron::Schedule;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn make_job(cmd: &str, timeout_secs: u64) -> Job {
        Job {
            id: "test".to_string(),
            name: "Test Job".to_string(),
            schedule: Schedule::from_str("* * * * * *").unwrap(),
            command: cmd.to_string(),
            timeout: Duration::from_secs(timeout_secs),
            concurrency: Concurrency::Skip,
        }
    }

    #[tokio::test]
    async fn execute_simple_job() {
        let job = make_job("echo test", 10);
        let dir = tempdir().unwrap();
        execute_job(&job, &dir.path().to_path_buf()).await;
    }

    #[tokio::test]
    async fn job_timeout() {
        let job = make_job("sleep 10", 1);
        let dir = tempdir().unwrap();
        execute_job(&job, &dir.path().to_path_buf()).await;
    }
}
