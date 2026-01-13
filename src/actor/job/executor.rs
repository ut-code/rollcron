use rand::Rng;
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::config::{Job, RetryConfig, RunnerConfig};
use crate::env;
use crate::git;
use crate::webhook::{self, JobFailure};

/// Default jitter ratio when not explicitly configured (25% of base delay)
const AUTO_JITTER_RATIO: u32 = 25;

pub async fn execute_job(job: &Job, sot_path: &PathBuf, runner: &RunnerConfig) -> bool {
    let job_dir = git::get_job_dir(sot_path, &job.id);
    let work_dir = resolve_work_dir(sot_path, &job.id, &job.working_dir);
    let mut log_file = job
        .log_file
        .as_ref()
        .and_then(|p| create_log_file(&job_dir, p, job.log_max_size));

    let max_attempts = job.retry.as_ref().map(|r| r.max + 1).unwrap_or(1);
    let mut last_result: Option<CommandResult> = None;

    for attempt in 0..max_attempts {
        if attempt > 0 {
            if let Some(retry) = job.retry.as_ref() {
                let delay = calculate_backoff(retry, attempt - 1);
                info!(
                    target: "rollcron::job",
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
            target: "rollcron::job",
            job_id = %job.id,
            name = %job.name,
            command = %job.command,
            "Starting job"
        );

        let result = run_command(job, &work_dir, sot_path, runner).await;
        let success = handle_result(job, &result, log_file.as_mut());

        if success {
            return true;
        }

        last_result = Some(result);

        if attempt + 1 < max_attempts {
            debug!(target: "rollcron::job", job_id = %job.id, "Will retry...");
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
            Some(CommandResult::Timeout) => {
                (format!("timeout after {:?}", job.timeout), String::new())
            }
            None => ("unknown error".to_string(), String::new()),
        };

        let failure = JobFailure {
            job_id: &job.id,
            job_name: &job.name,
            error,
            stderr,
            attempts: max_attempts,
        };

        let runner_env = load_runner_env_vars(sot_path, runner);
        for wh in &job.webhook {
            let url = wh.to_url(runner_env.as_ref());
            if url.contains('$') {
                warn!(
                    target: "rollcron::webhook",
                    job_id = %job.id,
                    url = %url,
                    "Webhook URL contains unexpanded variable, skipping"
                );
                continue;
            }
            if !url.starts_with("http://") && !url.starts_with("https://") {
                warn!(
                    target: "rollcron::webhook",
                    job_id = %job.id,
                    url = %url,
                    "Webhook URL must start with http:// or https://, skipping"
                );
                continue;
            }
            webhook::send_job_failure(&url, &failure).await;
        }
    }

    false
}

fn resolve_work_dir(sot_path: &PathBuf, job_id: &str, working_dir: &Option<String>) -> PathBuf {
    let job_dir = git::get_job_dir(sot_path, job_id);
    match working_dir {
        Some(dir) => {
            let expanded = env::expand_string(dir);
            let work_path = job_dir.join(&expanded);
            match (work_path.canonicalize(), job_dir.canonicalize()) {
                (Ok(resolved), Ok(base)) if resolved.starts_with(&base) => resolved,
                _ => {
                    warn!(
                        target: "rollcron::job",
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

async fn run_command(
    job: &Job,
    work_dir: &PathBuf,
    sot_path: &PathBuf,
    runner: &RunnerConfig,
) -> CommandResult {
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

/// Load runner-level env vars for webhook URL expansion.
/// Returns None on error (webhook will fall back to process env).
fn load_runner_env_vars(
    sot_path: &PathBuf,
    runner: &RunnerConfig,
) -> Option<HashMap<String, String>> {
    let mut env_vars = HashMap::new();

    // Load runner.env_file
    if let Some(env_file_path) = &runner.env_file {
        let expanded = env::expand_string(env_file_path);
        let full_path = sot_path.join(&expanded);
        match env::load_env_from_path(&full_path) {
            Ok(vars) => env_vars.extend(vars),
            Err(e) => {
                warn!(target: "rollcron::webhook", error = %e, "Failed to load runner env_file");
                return None;
            }
        }
    }

    // Merge runner.env
    if let Some(runner_env) = &runner.env {
        for (k, v) in runner_env {
            env_vars.insert(k.clone(), env::expand_string(v));
        }
    }

    Some(env_vars)
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

fn handle_result(job: &Job, result: &CommandResult, log_file: Option<&mut File>) -> bool {
    match result {
        CommandResult::Completed(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);

            if let Some(file) = log_file {
                let _ = file.write_all(stdout.as_bytes());
                let _ = file.write_all(stderr.as_bytes());
            }

            if output.status.success() {
                info!(target: "rollcron::job", job_id = %job.id, "Completed");
                true
            } else {
                error!(
                    target: "rollcron::job",
                    job_id = %job.id,
                    exit_code = ?output.status.code(),
                    "Failed"
                );
                false
            }
        }
        CommandResult::ExecError(e) => {
            error!(target: "rollcron::job", job_id = %job.id, error = %e, "Failed to execute");
            if let Some(file) = log_file {
                let _ = writeln!(file, "[rollcron] Error: {}", e);
            }
            false
        }
        CommandResult::Timeout => {
            error!(target: "rollcron::job", job_id = %job.id, timeout = ?job.timeout, "Timeout");
            if let Some(file) = log_file {
                let _ = writeln!(file, "[rollcron] Timeout after {:?}", job.timeout);
            }
            false
        }
    }
}

// === Backoff ===

fn calculate_backoff(retry: &RetryConfig, attempt: u32) -> Duration {
    let base_delay = retry.delay.saturating_mul(2u32.saturating_pow(attempt));
    let jitter_max =
        retry.jitter.unwrap_or_else(|| retry.delay.saturating_mul(AUTO_JITTER_RATIO) / 100);
    base_delay.saturating_add(generate_jitter(jitter_max))
}

fn generate_jitter(max: Duration) -> Duration {
    if max.is_zero() {
        return Duration::ZERO;
    }
    let millis = max.as_millis();
    if millis == 0 {
        return Duration::ZERO;
    }
    let jitter_millis = rand::thread_rng().gen_range(0..=millis);
    Duration::from_millis(jitter_millis as u64)
}

// === Logging ===

fn rotate_log_file(path: &PathBuf, max_size: u64) {
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() >= max_size {
            let old_path = path.with_extension("log.old");
            let _ = fs::remove_file(&old_path);
            let _ = fs::rename(path, &old_path);
        }
    }
}

fn create_log_file(job_dir: &PathBuf, log_path: &str, max_size: u64) -> Option<File> {
    let expanded = env::expand_string(log_path);
    let full_path = job_dir.join(&expanded);

    if let Some(parent) = full_path.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            warn!(target: "rollcron::job", error = %e, "Failed to create log directory");
            return None;
        }
    }

    rotate_log_file(&full_path, max_size);

    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&full_path)
    {
        Ok(f) => Some(f),
        Err(e) => {
            warn!(target: "rollcron::job", error = %e, "Failed to create log file");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Concurrency, TimezoneConfig};
    use croner::Cron;
    use std::str::FromStr;
    use tempfile::tempdir;

    fn make_job(cmd: &str, timeout_secs: u64) -> Job {
        Job {
            id: "test".to_string(),
            name: "Test Job".to_string(),
            schedule: Cron::from_str("* * * * *").unwrap(),
            command: cmd.to_string(),
            timeout: Duration::from_secs(timeout_secs),
            concurrency: Concurrency::Skip,
            retry: None,
            working_dir: None,
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
            webhook: vec![],
        }
    }

    #[tokio::test]
    async fn execute_simple_job() {
        let job = make_job("echo test", 10);
        let dir = tempdir().unwrap();
        let runner = make_runner();
        execute_job(&job, &dir.path().to_path_buf(), &runner).await;
    }

    #[tokio::test]
    async fn job_timeout() {
        let job = make_job("sleep 10", 1);
        let dir = tempdir().unwrap();
        let runner = make_runner();
        execute_job(&job, &dir.path().to_path_buf(), &runner).await;
    }

    #[test]
    fn exponential_backoff_calculation() {
        let retry = RetryConfig {
            max: 5,
            delay: Duration::from_secs(1),
            jitter: None,
        };
        let backoff_0 = calculate_backoff(&retry, 0);
        assert!(backoff_0 >= Duration::from_secs(1));
        assert!(backoff_0 <= Duration::from_millis(1250));

        let backoff_1 = calculate_backoff(&retry, 1);
        assert!(backoff_1 >= Duration::from_secs(2));
        assert!(backoff_1 <= Duration::from_millis(2250));
    }

    #[test]
    fn generate_jitter_bounds() {
        let max = Duration::from_millis(100);
        for _ in 0..10 {
            let jitter = generate_jitter(max);
            assert!(jitter <= max);
        }
    }

    #[test]
    fn generate_jitter_zero() {
        let jitter = generate_jitter(Duration::ZERO);
        assert_eq!(jitter, Duration::ZERO);
    }
}
