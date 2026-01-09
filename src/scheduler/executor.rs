use serde::Serialize;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

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
            info!(target: "rollcron::webhook", url = %url, "Notification sent");
        }
        Ok(resp) => {
            error!(target: "rollcron::webhook", url = %url, status = %resp.status(), "Failed to send notification");
        }
        Err(e) => {
            error!(target: "rollcron::webhook", url = %url, error = %e, "Failed to send notification");
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
                    warn!(
                        target: "rollcron::scheduler",
                        job_id = %job_id,
                        working_dir = %dir,
                        "Invalid working_dir: path traversal or non-existent"
                    );
                    job_dir
                }
            }
        }
        None => job_dir,
    }
}

/// Rotate log file if it exceeds max_size: .log -> .log.old
fn rotate_log_file(path: &PathBuf, max_size: u64) {
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() >= max_size {
            let old_path = path.with_extension("log.old");
            let _ = fs::remove_file(&old_path);
            let _ = fs::rename(path, &old_path);
        }
    }
}

/// Create log file for job output
fn create_log_file(job_dir: &PathBuf, log_path: &str, max_size: u64) -> Option<File> {
    let expanded = env::expand_string(log_path);
    let full_path = job_dir.join(&expanded);

    // Create parent directories if needed
    if let Some(parent) = full_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            warn!(target: "rollcron::scheduler", error = %e, "Failed to create log directory");
            return None;
        }
    }

    rotate_log_file(&full_path, max_size);

    match OpenOptions::new().create(true).append(true).open(&full_path) {
        Ok(f) => Some(f),
        Err(e) => {
            warn!(target: "rollcron::scheduler", error = %e, "Failed to create log file");
            None
        }
    }
}

pub async fn execute_job(job: &Job, work_dir: &PathBuf, sot_path: &PathBuf, runner: &RunnerConfig) {
    let job_dir = git::get_job_dir(sot_path, &job.id);
    let mut log_file = job.log_file.as_ref().and_then(|p| create_log_file(&job_dir, p, job.log_max_size));

    // Apply task jitter before first execution
    if let Some(jitter_max) = job.jitter {
        let jitter = generate_jitter(jitter_max);
        if jitter > Duration::ZERO {
            debug!(target: "rollcron::scheduler", job_id = %job.id, jitter = ?jitter, "Applying jitter");
            sleep(jitter).await;
        }
    }

    let max_attempts = job.retry.as_ref().map(|r| r.max + 1).unwrap_or(1);
    let mut last_result: Option<CommandResult> = None;

    for attempt in 0..max_attempts {
        if attempt > 0 {
            if let Some(retry) = job.retry.as_ref() {
                let delay = calculate_backoff(retry, attempt - 1);
                info!(
                    target: "rollcron::scheduler",
                    job_id = %job.id,
                    attempt,
                    max_retries = max_attempts - 1,
                    delay = ?delay,
                    "Retrying"
                );
                sleep(delay).await;
            }
        }

        info!(
            target: "rollcron::scheduler",
            job_id = %job.id,
            name = %job.name,
            command = %job.command,
            "Starting job"
        );

        let result = run_command(job, work_dir, sot_path, runner).await;
        let success = handle_result(job, &result, log_file.as_mut());

        if success {
            return;
        }

        last_result = Some(result);

        if attempt + 1 < max_attempts {
            debug!(target: "rollcron::scheduler", job_id = %job.id, "Will retry...");
        }
    }

    // All retries exhausted - send webhook notifications if configured
    if !job.webhook.is_empty() {
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

        for webhook in &job.webhook {
            send_webhook(&webhook.to_url(), &payload).await;
        }
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

/// Validate that a path stays within the base directory (prevents path traversal).
fn validate_env_file_path(
    env_file_path: &str,
    base_dir: &PathBuf,
    context: &str,
) -> anyhow::Result<PathBuf> {
    let expanded = env::expand_string(env_file_path);
    let full_path = base_dir.join(&expanded);

    // Canonicalize to resolve .. and symlinks
    match (full_path.canonicalize(), base_dir.canonicalize()) {
        (Ok(resolved), Ok(base)) if resolved.starts_with(&base) => Ok(resolved),
        (Ok(_), Ok(_)) => {
            anyhow::bail!(
                "{} env_file '{}': path traversal detected (must be within base directory)",
                context,
                env_file_path
            )
        }
        (Err(e), _) => {
            anyhow::bail!(
                "{} env_file '{}': file not found or inaccessible: {}",
                context,
                env_file_path,
                e
            )
        }
        (_, Err(e)) => {
            anyhow::bail!(
                "{} base directory not accessible: {}",
                context,
                e
            )
        }
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
        let validated_path = validate_env_file_path(env_file_path, sot_path, "runner")?;
        let vars = env::load_env_from_path(&validated_path)?;
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
        let validated_path = validate_env_file_path(env_file_path, work_dir, &format!("job:{}", job.id))?;
        let vars = env::load_env_from_path(&validated_path)?;
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

fn handle_result(job: &Job, result: &CommandResult, log_file: Option<&mut File>) -> bool {
    match result {
        CommandResult::Completed(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            // Write stdout/stderr to log file
            if let Some(file) = log_file {
                let _ = file.write_all(stdout.as_bytes());
                let _ = file.write_all(stderr.as_bytes());
            }

            if output.status.success() {
                info!(target: "rollcron::scheduler", job_id = %job.id, "Completed");
                true
            } else {
                error!(
                    target: "rollcron::scheduler",
                    job_id = %job.id,
                    exit_code = ?output.status.code(),
                    "Failed"
                );
                false
            }
        }
        CommandResult::ExecError(e) => {
            error!(target: "rollcron::scheduler", job_id = %job.id, error = %e, "Failed to execute");
            false
        }
        CommandResult::Timeout => {
            error!(target: "rollcron::scheduler", job_id = %job.id, timeout = ?job.timeout, "Timeout");
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
            webhook: vec![],
            log_file: None,
            log_max_size: 10 * 1024 * 1024,
        }
    }

    fn make_runner() -> RunnerConfig {
        RunnerConfig {
            timezone: TimezoneConfig::Utc,
            env_file: None,
            env: None,
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
