use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;

use crate::config::{Job, RunnerConfig};
use crate::env;
use crate::git;

use super::backoff::{calculate_backoff, generate_jitter};

/// Webhook notification payload
#[derive(Debug, Serialize)]
pub struct WebhookPayload {
    pub text: String,
    pub job_id: String,
    pub job_name: String,
    pub error: String,
    pub stderr: String,
    pub attempts: u32,
}

/// Send webhook notification for job failure
async fn send_webhook(url: &str, payload: &WebhookPayload) {
    let client = reqwest::Client::new();
    match client.post(url).json(payload).send().await {
        Ok(resp) if resp.status().is_success() => {
            println!("[webhook] Notification sent to {}", url);
        }
        Ok(resp) => {
            eprintln!("[webhook] Failed to send notification: HTTP {}", resp.status());
        }
        Err(e) => {
            eprintln!("[webhook] Failed to send notification: {}", e);
        }
    }
}

pub fn resolve_work_dir(sot_path: &PathBuf, job_id: &str, working_dir: &Option<String>) -> PathBuf {
    let job_dir = git::get_job_dir(sot_path, job_id);
    match working_dir {
        Some(dir) => {
            let expanded = env::expand_string(dir);
            let work_path = job_dir.join(&expanded);
            // Canonicalize to resolve .. and symlinks, then verify path is within job_dir
            match (work_path.canonicalize(), job_dir.canonicalize()) {
                (Ok(resolved), Ok(base)) if resolved.starts_with(&base) => resolved,
                _ => {
                    eprintln!(
                        "[job:{}] Invalid working_dir '{}': path traversal or non-existent",
                        job_id, dir
                    );
                    job_dir
                }
            }
        }
        None => job_dir,
    }
}

pub async fn execute_job(job: &Job, work_dir: &PathBuf, sot_path: &PathBuf, runner: &RunnerConfig) {
    let tag = format!("[job:{}]", job.id);

    // Apply task jitter before first execution
    if let Some(jitter_max) = job.jitter {
        let jitter = generate_jitter(jitter_max);
        if jitter > Duration::ZERO {
            println!("{} Applying jitter: {:?}", tag, jitter);
            sleep(jitter).await;
        }
    }

    let max_attempts = job.retry.as_ref().map(|r| r.max + 1).unwrap_or(1);
    let mut last_result: Option<CommandResult> = None;

    for attempt in 0..max_attempts {
        if attempt > 0 {
            if let Some(retry) = job.retry.as_ref() {
                let delay = calculate_backoff(retry, attempt - 1);
                println!("{} Retry {}/{} after {:?}", tag, attempt, max_attempts - 1, delay);
                sleep(delay).await;
            }
        }

        println!("{} Starting '{}'", tag, job.name);
        println!("{}   command: {}", tag, job.command);

        let result = run_command(job, work_dir, sot_path, runner).await;
        let success = handle_result(&tag, job, &result);

        if success {
            return;
        }

        last_result = Some(result);

        if attempt + 1 < max_attempts {
            println!("{} Will retry...", tag);
        }
    }

    // All retries exhausted - send webhook notification if configured
    if let Some(webhook_url) = &job.webhook {
        let (error, stderr) = match &last_result {
            Some(CommandResult::Completed(output)) => {
                let err = format!("exit code {:?}", output.status.code());
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                (err, stderr)
            }
            Some(CommandResult::ExecError(e)) => (format!("exec error: {}", e), String::new()),
            Some(CommandResult::Timeout) => (format!("timeout after {:?}", job.timeout), String::new()),
            None => ("unknown error".to_string(), String::new()),
        };

        let payload = WebhookPayload {
            text: format!("[rollcron] Job '{}' failed", job.name),
            job_id: job.id.clone(),
            job_name: job.name.clone(),
            error,
            stderr,
            attempts: max_attempts,
        };

        send_webhook(webhook_url, &payload).await;
    }
}

async fn run_command(job: &Job, work_dir: &PathBuf, sot_path: &PathBuf, runner: &RunnerConfig) -> CommandResult {
    // Merge environment variables in priority order:
    // host ENV < runner.env_file < runner.env < job.env_file < job.env
    let env_vars = match merge_env_vars(job, work_dir, sot_path, runner) {
        Ok(vars) => vars,
        Err(e) => {
            return CommandResult::ExecError(format!("Failed to load environment: {}", e));
        }
    };

    let mut cmd = Command::new("sh");
    cmd.args(["-c", &job.command])
        .current_dir(work_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    // Apply environment variables
    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    let child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return CommandResult::ExecError(e.to_string()),
    };

    let result = tokio::time::timeout(job.timeout, child.wait_with_output()).await;

    match result {
        Ok(Ok(output)) => CommandResult::Completed(output),
        Ok(Err(e)) => CommandResult::ExecError(e.to_string()),
        Err(_) => CommandResult::Timeout,
    }
}

fn merge_env_vars(
    job: &Job,
    work_dir: &PathBuf,
    sot_path: &PathBuf,
    runner: &RunnerConfig,
) -> anyhow::Result<HashMap<String, String>> {
    let mut env_vars = HashMap::new();

    // 1. Start with runner.env_file (loaded from sot_path)
    if let Some(env_file_path) = &runner.env_file {
        let expanded = env::expand_string(env_file_path);
        let full_path = sot_path.join(&expanded);
        let vars = env::load_env_from_path(&full_path)?;
        env_vars.extend(vars);
    }

    // 2. Merge runner.env (with shell expansion on values)
    if let Some(runner_env) = &runner.env {
        for (k, v) in runner_env {
            env_vars.insert(k.clone(), env::expand_string(v));
        }
    }

    // 3. Merge job.env_file (loaded from work_dir)
    if let Some(env_file_path) = &job.env_file {
        let expanded = env::expand_string(env_file_path);
        let full_path = work_dir.join(&expanded);
        let vars = env::load_env_from_path(&full_path)?;
        env_vars.extend(vars);
    }

    // 4. Merge job.env (with shell expansion on values)
    if let Some(job_env) = &job.env {
        for (k, v) in job_env {
            env_vars.insert(k.clone(), env::expand_string(v));
        }
    }

    Ok(env_vars)
}

enum CommandResult {
    Completed(std::process::Output),
    ExecError(String),
    Timeout,
}

fn print_output_lines(tag: &str, output: &str, use_stderr: bool) {
    if output.trim().is_empty() {
        return;
    }
    for line in output.lines() {
        if use_stderr {
            eprintln!("{}   | {}", tag, line);
        } else {
            println!("{}   | {}", tag, line);
        }
    }
}

fn handle_result(tag: &str, job: &Job, result: &CommandResult) -> bool {
    match result {
        CommandResult::Completed(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            if output.status.success() {
                println!("{} ✓ Completed", tag);
                print_output_lines(tag, &stdout, false);
                true
            } else {
                eprintln!("{} ✗ Failed (exit code: {:?})", tag, output.status.code());
                print_output_lines(tag, &stderr, true);
                print_output_lines(tag, &stdout, true);
                false
            }
        }
        CommandResult::ExecError(e) => {
            eprintln!("{} ✗ Failed to execute: {}", tag, e);
            false
        }
        CommandResult::Timeout => {
            eprintln!("{} ✗ Timeout after {:?}", tag, job.timeout);
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Concurrency, RetryConfig, TimezoneConfig};
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
            retry: None,
            working_dir: None,
            jitter: None,
            enabled: true,
            timezone: None,
            env_file: None,
            env: None,
            webhook: None,
        }
    }

    fn make_runner() -> RunnerConfig {
        RunnerConfig {
            timezone: TimezoneConfig::Utc,
            env_file: None,
            env: None,
            webhook: None,
        }
    }

    #[tokio::test]
    async fn execute_simple_job() {
        let job = make_job("echo test", 10);
        let dir = tempdir().unwrap();
        let runner = make_runner();
        execute_job(&job, &dir.path().to_path_buf(), &dir.path().to_path_buf(), &runner).await;
    }

    #[tokio::test]
    async fn job_timeout() {
        let job = make_job("sleep 10", 1);
        let dir = tempdir().unwrap();
        let runner = make_runner();
        execute_job(&job, &dir.path().to_path_buf(), &dir.path().to_path_buf(), &runner).await;
    }

    #[tokio::test]
    async fn job_retry_on_failure() {
        let mut job = make_job("exit 1", 10);
        job.retry = Some(RetryConfig {
            max: 2,
            delay: Duration::from_millis(10),
            jitter: None,
        });
        let dir = tempdir().unwrap();
        let runner = make_runner();
        let start = std::time::Instant::now();
        execute_job(&job, &dir.path().to_path_buf(), &dir.path().to_path_buf(), &runner).await;
        assert!(start.elapsed() >= Duration::from_millis(30));
    }

    #[tokio::test]
    async fn job_success_no_retry() {
        let mut job = make_job("echo ok", 10);
        job.retry = Some(RetryConfig {
            max: 3,
            delay: Duration::from_millis(100),
            jitter: None,
        });
        let dir = tempdir().unwrap();
        let runner = make_runner();
        let start = std::time::Instant::now();
        execute_job(&job, &dir.path().to_path_buf(), &dir.path().to_path_buf(), &runner).await;
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn job_with_task_jitter() {
        let mut job = make_job("echo ok", 10);
        job.jitter = Some(Duration::from_millis(50));
        let dir = tempdir().unwrap();
        let runner = make_runner();
        let start = std::time::Instant::now();
        execute_job(&job, &dir.path().to_path_buf(), &dir.path().to_path_buf(), &runner).await;
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn job_with_env_file() {
        let dir = tempdir().unwrap();
        let env_path = dir.path().join(".env");
        std::fs::write(&env_path, "TEST_VAR=hello\nOTHER_VAR=world").unwrap();

        let job = make_job("echo $TEST_VAR $OTHER_VAR", 10);
        let runner = make_runner();
        execute_job(&job, &dir.path().to_path_buf(), &dir.path().to_path_buf(), &runner).await;
    }

    #[tokio::test]
    async fn job_without_env_file() {
        let dir = tempdir().unwrap();
        let runner = make_runner();
        let job = make_job("echo no env file", 10);
        execute_job(&job, &dir.path().to_path_buf(), &dir.path().to_path_buf(), &runner).await;
    }

    #[tokio::test]
    async fn job_with_runner_env() {
        let dir = tempdir().unwrap();
        let job = make_job("echo $RUNNER_VAR", 10);
        let mut runner = make_runner();
        let mut env = HashMap::new();
        env.insert("RUNNER_VAR".to_string(), "from_runner".to_string());
        runner.env = Some(env);
        execute_job(&job, &dir.path().to_path_buf(), &dir.path().to_path_buf(), &runner).await;
    }

    #[tokio::test]
    async fn job_with_job_env() {
        let dir = tempdir().unwrap();
        let mut job = make_job("echo $JOB_VAR", 10);
        let mut env = HashMap::new();
        env.insert("JOB_VAR".to_string(), "from_job".to_string());
        job.env = Some(env);
        let runner = make_runner();
        execute_job(&job, &dir.path().to_path_buf(), &dir.path().to_path_buf(), &runner).await;
    }

    #[tokio::test]
    async fn job_env_priority() {
        let dir = tempdir().unwrap();

        // Create runner.env_file
        let runner_env_file = dir.path().join("runner.env");
        std::fs::write(&runner_env_file, "VAR=from_runner_file").unwrap();

        // Create job.env_file
        let job_env_file = dir.path().join("job.env");
        std::fs::write(&job_env_file, "VAR=from_job_file").unwrap();

        let mut runner = make_runner();
        runner.env_file = Some("runner.env".to_string());
        let mut runner_env = HashMap::new();
        runner_env.insert("VAR".to_string(), "from_runner_env".to_string());
        runner.env = Some(runner_env);

        let mut job = make_job("echo $VAR", 10);
        job.env_file = Some("job.env".to_string());
        let mut job_env = HashMap::new();
        job_env.insert("VAR".to_string(), "from_job_env".to_string());
        job.env = Some(job_env);

        // Priority: host < runner.env_file < runner.env < job.env_file < job.env
        // Expected: job.env should win
        execute_job(&job, &dir.path().to_path_buf(), &dir.path().to_path_buf(), &runner).await;
    }

    #[test]
    fn test_merge_env_vars_priority() {
        let dir = tempdir().unwrap();

        // Create runner.env_file
        let runner_env_file = dir.path().join("runner.env");
        std::fs::write(&runner_env_file, "VAR1=from_runner_file\nVAR2=from_runner_file\nVAR3=from_runner_file\nVAR4=from_runner_file").unwrap();

        // Create job.env_file
        let job_env_file = dir.path().join("job.env");
        std::fs::write(&job_env_file, "VAR3=from_job_file\nVAR4=from_job_file").unwrap();

        let mut runner = make_runner();
        runner.env_file = Some("runner.env".to_string());
        let mut runner_env = HashMap::new();
        runner_env.insert("VAR2".to_string(), "from_runner_env".to_string());
        runner_env.insert("VAR3".to_string(), "from_runner_env".to_string());
        runner_env.insert("VAR4".to_string(), "from_runner_env".to_string());
        runner.env = Some(runner_env);

        let mut job = make_job("echo test", 10);
        job.env_file = Some("job.env".to_string());
        let mut job_env = HashMap::new();
        job_env.insert("VAR4".to_string(), "from_job_env".to_string());
        job.env = Some(job_env);

        let result = merge_env_vars(&job, &dir.path().to_path_buf(), &dir.path().to_path_buf(), &runner).unwrap();

        // VAR1: only in runner.env_file
        assert_eq!(result.get("VAR1"), Some(&"from_runner_file".to_string()));
        // VAR2: in runner.env_file (overridden by runner.env)
        assert_eq!(result.get("VAR2"), Some(&"from_runner_env".to_string()));
        // VAR3: in runner.env_file, runner.env (overridden by job.env_file)
        assert_eq!(result.get("VAR3"), Some(&"from_job_file".to_string()));
        // VAR4: in all layers (job.env wins)
        assert_eq!(result.get("VAR4"), Some(&"from_job_env".to_string()));
    }

    #[test]
    fn test_webhook_payload_serialization() {
        let payload = WebhookPayload {
            text: "[rollcron] Job 'test' failed".to_string(),
            job_id: "test".to_string(),
            job_name: "Test Job".to_string(),
            error: "exit code 1".to_string(),
            stderr: "Error output".to_string(),
            attempts: 3,
        };

        let json = serde_json::to_string(&payload).unwrap();
        assert!(json.contains("\"text\":\"[rollcron] Job 'test' failed\""));
        assert!(json.contains("\"job_id\":\"test\""));
        assert!(json.contains("\"attempts\":3"));
    }
}
