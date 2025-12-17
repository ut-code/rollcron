use crate::config::Job;
use crate::git;
use anyhow::Result;
use chrono::Utc;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;
use tokio::time::sleep;

pub async fn run_scheduler(jobs: Vec<Job>, sot_path: PathBuf) -> Result<()> {
    loop {
        let now = Utc::now();

        for job in &jobs {
            if let Some(next) = job.schedule.upcoming(Utc).next() {
                let until_next = (next - now).num_milliseconds();
                if until_next <= 1000 && until_next >= 0 {
                    let job = job.clone();
                    let work_dir = git::get_job_dir(&sot_path, &job.id);
                    tokio::spawn(async move {
                        execute_job(&job, &work_dir).await;
                    });
                }
            }
        }

        sleep(Duration::from_secs(1)).await;
    }
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
