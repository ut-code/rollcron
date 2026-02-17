use chrono::{Local, Utc};
use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::process::Command;
use tracing::{debug, error, info, warn};

use crate::config::{Job, RunnerConfig, TimezoneConfig};
use crate::env;
use crate::git;
use crate::webhook::{self, BuildFailure, JobFailure};

/// Grace period to wait after SIGTERM before sending SIGKILL
const GRACEFUL_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

/// Result of a build operation.
#[derive(Debug)]
pub enum BuildResult {
    Success,
    Failed {
        #[allow(dead_code)]
        error: String,
        #[allow(dead_code)]
        stderr: String,
    },
    NoBuild,
}

/// Executes the build command for a job.
/// Returns BuildResult::NoBuild if no build command is configured.
pub async fn execute_build(job: &Job, sot_path: &Path, runner: &RunnerConfig) -> BuildResult {
    let build_config = match &job.build {
        Some(config) => config,
        None => return BuildResult::NoBuild,
    };

    let build_dir = git::get_build_dir(sot_path, &job.id);
    let job_dir = git::get_job_dir(sot_path, &job.id);
    let mut log_file = job
        .log_file
        .as_ref()
        .and_then(|p| create_log_file(&job_dir, p, job.log_max_size));

    info!(
        target: "rollcron::job",
        job_id = %job.id,
        command = %build_config.command,
        "Starting build"
    );

    if let Some(ref mut file) = log_file {
        write_log_marker(file, &runner.timezone, job.timezone.as_ref(), "Build started");
    }

    let start_time = Instant::now();
    let result = run_build_command(job, build_config, &build_dir, sot_path, runner).await;
    let duration = start_time.elapsed();

    match &result {
        BuildCommandResult::Completed(output) if output.status.success() => {
            info!(target: "rollcron::job", job_id = %job.id, "Build completed");
            if let Some(ref mut file) = log_file {
                let _ = file.write_all(&output.stdout);
                let _ = file.write_all(&output.stderr);
                let marker = format!("Build finished (success) [{}]", format_duration(duration));
                write_log_marker(file, &runner.timezone, job.timezone.as_ref(), &marker);
            }
            BuildResult::Success
        }
        BuildCommandResult::Completed(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr).to_string();
            error!(
                target: "rollcron::job",
                job_id = %job.id,
                exit_code = ?output.status.code(),
                "Build failed"
            );

            if let Some(ref mut file) = log_file {
                let _ = file.write_all(&output.stdout);
                let _ = file.write_all(&output.stderr);
                let marker = format!("Build finished (failed, exit code {:?}) [{}]", output.status.code(), format_duration(duration));
                write_log_marker(file, &runner.timezone, job.timezone.as_ref(), &marker);
            }

            // Send webhook notifications
            if !job.webhook.is_empty() {
                let failure = BuildFailure {
                    job_id: &job.id,
                    job_name: &job.name,
                    error: format!("exit code {:?}", output.status.code()),
                    stderr: stderr.clone(),
                };

                let runner_env = load_runner_env_vars(sot_path, runner);
                for wh in &job.webhook {
                    let url = wh.to_url(runner_env.as_ref());
                    if url.contains('$') || (!url.starts_with("http://") && !url.starts_with("https://")) {
                        continue;
                    }
                    webhook::send_build_failure(&url, &failure).await;
                }
            }

            BuildResult::Failed {
                error: format!("exit code {:?}", output.status.code()),
                stderr,
            }
        }
        BuildCommandResult::ExecError(e) => {
            error!(target: "rollcron::job", job_id = %job.id, error = %e, "Build failed to execute");

            if let Some(ref mut file) = log_file {
                let _ = writeln!(file, "[rollcron] Error: {}", e);
                let marker = format!("Build finished (error: {}) [{}]", e, format_duration(duration));
                write_log_marker(file, &runner.timezone, job.timezone.as_ref(), &marker);
            }

            if !job.webhook.is_empty() {
                let failure = BuildFailure {
                    job_id: &job.id,
                    job_name: &job.name,
                    error: format!("exec error: {}", e),
                    stderr: String::new(),
                };

                let runner_env = load_runner_env_vars(sot_path, runner);
                for wh in &job.webhook {
                    let url = wh.to_url(runner_env.as_ref());
                    if url.contains('$') || (!url.starts_with("http://") && !url.starts_with("https://")) {
                        continue;
                    }
                    webhook::send_build_failure(&url, &failure).await;
                }
            }

            BuildResult::Failed {
                error: format!("exec error: {}", e),
                stderr: String::new(),
            }
        }
        BuildCommandResult::Timeout => {
            error!(target: "rollcron::job", job_id = %job.id, timeout = ?build_config.timeout, "Build timeout");

            if let Some(ref mut file) = log_file {
                let _ = writeln!(file, "[rollcron] Timeout after {:?}", build_config.timeout);
                let marker = format!("Build finished (timeout after {:?}) [{}]", build_config.timeout, format_duration(duration));
                write_log_marker(file, &runner.timezone, job.timezone.as_ref(), &marker);
            }

            if !job.webhook.is_empty() {
                let failure = BuildFailure {
                    job_id: &job.id,
                    job_name: &job.name,
                    error: format!("timeout after {:?}", build_config.timeout),
                    stderr: String::new(),
                };

                let runner_env = load_runner_env_vars(sot_path, runner);
                for wh in &job.webhook {
                    let url = wh.to_url(runner_env.as_ref());
                    if url.contains('$') || (!url.starts_with("http://") && !url.starts_with("https://")) {
                        continue;
                    }
                    webhook::send_build_failure(&url, &failure).await;
                }
            }

            BuildResult::Failed {
                error: format!("timeout after {:?}", build_config.timeout),
                stderr: String::new(),
            }
        }
    }
}

async fn run_build_command(
    job: &Job,
    build_config: &crate::config::BuildConfig,
    build_dir: &Path,
    sot_path: &Path,
    runner: &RunnerConfig,
) -> BuildCommandResult {
    let env_vars = match merge_env_vars_for_build(job, build_dir, sot_path, runner) {
        Ok(vars) => vars,
        Err(e) => {
            return BuildCommandResult::ExecError(format!("Failed to load environment: {}", e));
        }
    };

    let work_dir = resolve_work_dir(build_dir, &job.id, &build_config.working_dir);

    let mut cmd = Command::new("sh");
    cmd.args(["-c", &build_config.command])
        .current_dir(&work_dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return BuildCommandResult::ExecError(e.to_string()),
    };

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let stdout_task = tokio::spawn(async move {
        match stdout {
            Some(mut out) => {
                let mut buf = Vec::new();
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut out, &mut buf).await;
                buf
            }
            None => Vec::new(),
        }
    });
    let stderr_task = tokio::spawn(async move {
        match stderr {
            Some(mut err) => {
                let mut buf = Vec::new();
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut err, &mut buf).await;
                buf
            }
            None => Vec::new(),
        }
    });

    let wait_result = tokio::time::timeout(build_config.timeout, child.wait()).await;

    match wait_result {
        Ok(Ok(status)) => {
            let stdout = stdout_task.await.unwrap_or_default();
            let stderr = stderr_task.await.unwrap_or_default();
            BuildCommandResult::Completed(std::process::Output {
                status,
                stdout,
                stderr,
            })
        }
        Ok(Err(e)) => BuildCommandResult::ExecError(e.to_string()),
        Err(_) => {
            graceful_kill(&mut child, &job.id).await;
            BuildCommandResult::Timeout
        }
    }
}

enum BuildCommandResult {
    Completed(std::process::Output),
    ExecError(String),
    Timeout,
}

fn merge_env_vars_for_build(
    job: &Job,
    build_dir: &Path,
    sot_path: &Path,
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

    // 3. Merge job.env_file (loaded from build_dir)
    if let Some(env_file_path) = &job.env_file {
        let expanded = env::expand_string(env_file_path);
        let full_path = build_dir.join(&expanded);
        let vars = env::load_env_from_path(&full_path)?;
        env_vars.extend(vars);
    }

    // 4. Merge job.env (with shell expansion on values)
    if let Some(job_env) = &job.env {
        for (k, v) in job_env {
            env_vars.insert(k.clone(), env::expand_string(v));
        }
    }

    // 5. Merge build.env_file (loaded from build_dir)
    if let Some(build) = &job.build {
        if let Some(env_file_path) = &build.env_file {
            let expanded = env::expand_string(env_file_path);
            let full_path = build_dir.join(&expanded);
            let vars = env::load_env_from_path(&full_path)?;
            env_vars.extend(vars);
        }

        // 6. Merge build.env (with shell expansion on values)
        if let Some(build_env) = &build.env {
            for (k, v) in build_env {
                env_vars.insert(k.clone(), env::expand_string(v));
            }
        }
    }

    Ok(env_vars)
}

pub async fn execute_job(job: &Job, sot_path: &Path, runner: &RunnerConfig) -> bool {
    let run_dir = git::get_run_dir(sot_path, &job.id);
    let job_dir = git::get_job_dir(sot_path, &job.id);
    let work_dir = resolve_work_dir(&run_dir, &job.id, &job.working_dir);
    let mut log_file = job
        .log_file
        .as_ref()
        .and_then(|p| create_log_file(&job_dir, p, job.log_max_size));

    info!(
        target: "rollcron::job",
        job_id = %job.id,
        name = %job.name,
        command = %job.command,
        "Starting job"
    );

    if let Some(ref mut file) = log_file {
        write_log_marker(file, &runner.timezone, job.timezone.as_ref(), "Job started");
    }

    let start_time = Instant::now();
    let result = run_command(job, &work_dir, sot_path, runner).await;
    let duration = start_time.elapsed();
    let success = handle_result(job, &result, log_file.as_mut(), &runner.timezone, duration);

    if success {
        return true;
    }

    // Job failed - send webhook notifications if configured
    if !job.webhook.is_empty() {
        let (error, stderr) = match &result {
            CommandResult::Completed(output) => {
                let err = format!("exit code {:?}", output.status.code());
                let stderr = String::from_utf8_lossy(&output.stderr).to_string();
                (err, stderr)
            }
            CommandResult::ExecError(e) => (format!("exec error: {}", e), String::new()),
            CommandResult::Timeout => {
                (format!("timeout after {:?}", job.timeout), String::new())
            }
        };

        let failure = JobFailure {
            job_id: &job.id,
            job_name: &job.name,
            error,
            stderr,
            attempts: 1,
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

fn resolve_work_dir(base_dir: &Path, job_id: &str, working_dir: &Option<String>) -> PathBuf {
    match working_dir {
        Some(dir) => {
            let expanded = env::expand_string(dir);
            let work_path = base_dir.join(&expanded);
            match (work_path.canonicalize(), base_dir.canonicalize()) {
                (Ok(resolved), Ok(base)) if resolved.starts_with(&base) => resolved,
                _ => {
                    warn!(
                        target: "rollcron::job",
                        job_id = %job_id,
                        working_dir = %dir,
                        "Invalid working_dir: path traversal or non-existent"
                    );
                    base_dir.to_path_buf()
                }
            }
        }
        None => base_dir.to_path_buf(),
    }
}

async fn run_command(
    job: &Job,
    work_dir: &Path,
    sot_path: &Path,
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
        .stderr(std::process::Stdio::piped());

    for (key, value) in env_vars {
        cmd.env(key, value);
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => return CommandResult::ExecError(e.to_string()),
    };

    // Take stdout/stderr handles before waiting
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    // Spawn tasks to read output concurrently (prevents buffer deadlock)
    let stdout_task = tokio::spawn(async move {
        match stdout {
            Some(mut out) => {
                let mut buf = Vec::new();
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut out, &mut buf).await;
                buf
            }
            None => Vec::new(),
        }
    });
    let stderr_task = tokio::spawn(async move {
        match stderr {
            Some(mut err) => {
                let mut buf = Vec::new();
                let _ = tokio::io::AsyncReadExt::read_to_end(&mut err, &mut buf).await;
                buf
            }
            None => Vec::new(),
        }
    });

    // Wait for process with timeout
    let wait_result = tokio::time::timeout(job.timeout, child.wait()).await;

    match wait_result {
        Ok(Ok(status)) => {
            let stdout = stdout_task.await.unwrap_or_default();
            let stderr = stderr_task.await.unwrap_or_default();
            CommandResult::Completed(std::process::Output {
                status,
                stdout,
                stderr,
            })
        }
        Ok(Err(e)) => CommandResult::ExecError(e.to_string()),
        Err(_) => {
            // Timeout occurred - attempt graceful shutdown
            graceful_kill(&mut child, &job.id).await;
            CommandResult::Timeout
        }
    }
}

/// Attempts graceful shutdown: SIGTERM first, then SIGKILL after grace period.
#[cfg(unix)]
async fn graceful_kill(child: &mut tokio::process::Child, job_id: &str) {
    use nix::sys::signal::{kill, Signal};
    use nix::unistd::Pid;

    let Some(pid) = child.id() else {
        return; // Process already exited
    };
    let pid = Pid::from_raw(pid as i32);

    // Send SIGTERM for graceful shutdown
    if kill(pid, Signal::SIGTERM).is_ok() {
        debug!(target: "rollcron::job", job_id = %job_id, "Sent SIGTERM, waiting for graceful exit");

        // Wait for process to exit gracefully
        if tokio::time::timeout(GRACEFUL_SHUTDOWN_TIMEOUT, child.wait())
            .await
            .is_ok()
        {
            debug!(target: "rollcron::job", job_id = %job_id, "Process exited gracefully after SIGTERM");
            return;
        }

        // Grace period expired - force kill
        warn!(target: "rollcron::job", job_id = %job_id, "Grace period expired, sending SIGKILL");
    }

    // Send SIGKILL
    let _ = child.kill().await;
    let _ = child.wait().await;
}

#[cfg(not(unix))]
async fn graceful_kill(child: &mut tokio::process::Child, _job_id: &str) {
    // On non-Unix platforms, just kill immediately
    let _ = child.kill().await;
    let _ = child.wait().await;
}

/// Load runner-level env vars for webhook URL expansion.
/// Returns None on error (webhook will fall back to process env).
fn load_runner_env_vars(
    sot_path: &Path,
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
    work_dir: &Path,
    sot_path: &Path,
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

    // 5. Merge run.env_file (loaded from work_dir)
    if let Some(env_file_path) = &job.run_env_file {
        let expanded = env::expand_string(env_file_path);
        let full_path = work_dir.join(&expanded);
        let vars = env::load_env_from_path(&full_path)?;
        env_vars.extend(vars);
    }

    // 6. Merge run.env (with shell expansion on values)
    if let Some(run_env) = &job.run_env {
        for (k, v) in run_env {
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

fn handle_result(job: &Job, result: &CommandResult, log_file: Option<&mut File>, runner_tz: &TimezoneConfig, duration: Duration) -> bool {
    match result {
        CommandResult::Completed(output) => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let success = output.status.success();

            if let Some(file) = log_file {
                let _ = file.write_all(stdout.as_bytes());
                let _ = file.write_all(stderr.as_bytes());
                let marker = if success {
                    format!("Job finished (success) [{}]", format_duration(duration))
                } else {
                    format!("Job finished (failed, exit code {:?}) [{}]", output.status.code(), format_duration(duration))
                };
                write_log_marker(file, runner_tz, job.timezone.as_ref(), &marker);
            }

            if success {
                info!(target: "rollcron::job", job_id = %job.id, "Completed");
            } else {
                error!(
                    target: "rollcron::job",
                    job_id = %job.id,
                    exit_code = ?output.status.code(),
                    "Failed"
                );
            }
            success
        }
        CommandResult::ExecError(e) => {
            error!(target: "rollcron::job", job_id = %job.id, error = %e, "Failed to execute");
            if let Some(file) = log_file {
                let _ = writeln!(file, "[rollcron] Error: {}", e);
                let marker = format!("Job finished (error: {}) [{}]", e, format_duration(duration));
                write_log_marker(file, runner_tz, job.timezone.as_ref(), &marker);
            }
            false
        }
        CommandResult::Timeout => {
            error!(target: "rollcron::job", job_id = %job.id, timeout = ?job.timeout, "Timeout");
            if let Some(file) = log_file {
                let _ = writeln!(file, "[rollcron] Timeout after {:?}", job.timeout);
                let marker = format!("Job finished (timeout after {:?}) [{}]", job.timeout, format_duration(duration));
                write_log_marker(file, runner_tz, job.timezone.as_ref(), &marker);
            }
            false
        }
    }
}

// === Logging ===

fn rotate_log_file(path: &Path, max_size: u64) {
    if let Ok(meta) = fs::metadata(path) {
        if meta.len() >= max_size {
            let old_path = path.with_extension("log.old");
            let _ = fs::remove_file(&old_path);
            let _ = fs::rename(path, &old_path);
        }
    }
}

fn create_log_file(job_dir: &Path, log_path: &str, max_size: u64) -> Option<File> {
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

fn format_timestamp(runner_tz: &TimezoneConfig, job_tz: Option<&TimezoneConfig>) -> String {
    let fmt = "%Y-%m-%d %H:%M:%S %Z";
    let tz = job_tz.unwrap_or(runner_tz);
    match tz {
        TimezoneConfig::Utc => Utc::now().format(fmt).to_string(),
        TimezoneConfig::Inherit => Local::now().format(fmt).to_string(),
        TimezoneConfig::Named(tz) => Utc::now().with_timezone(tz).format(fmt).to_string(),
    }
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    let millis = d.subsec_millis();
    if secs >= 3600 {
        format!("{}h {}m {}s", secs / 3600, (secs % 3600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m {}s", secs / 60, secs % 60)
    } else if secs > 0 {
        format!("{}.{:03}s", secs, millis)
    } else {
        format!("{}ms", millis)
    }
}

fn write_log_marker(file: &mut File, runner_tz: &TimezoneConfig, job_tz: Option<&TimezoneConfig>, marker: &str) {
    let timestamp = format_timestamp(runner_tz, job_tz);
    let _ = writeln!(file, "\n[{timestamp}] === {marker} ===");
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
            build: None,
            command: cmd.to_string(),
            timeout: Duration::from_secs(timeout_secs),
            concurrency: Concurrency::Skip,
            working_dir: None,
            enabled: true,
            timezone: None,
            env_file: None,
            env: None,
            run_env_file: None,
            run_env: None,
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
    fn format_duration_milliseconds() {
        assert_eq!(format_duration(Duration::from_millis(0)), "0ms");
        assert_eq!(format_duration(Duration::from_millis(1)), "1ms");
        assert_eq!(format_duration(Duration::from_millis(500)), "500ms");
        assert_eq!(format_duration(Duration::from_millis(999)), "999ms");
    }

    #[test]
    fn format_duration_seconds() {
        assert_eq!(format_duration(Duration::from_secs(1)), "1.000s");
        assert_eq!(format_duration(Duration::from_millis(1500)), "1.500s");
        assert_eq!(format_duration(Duration::from_millis(59999)), "59.999s");
    }

    #[test]
    fn format_duration_minutes() {
        assert_eq!(format_duration(Duration::from_secs(60)), "1m 0s");
        assert_eq!(format_duration(Duration::from_secs(90)), "1m 30s");
        assert_eq!(format_duration(Duration::from_secs(3599)), "59m 59s");
    }

    #[test]
    fn format_duration_hours() {
        assert_eq!(format_duration(Duration::from_secs(3600)), "1h 0m 0s");
        assert_eq!(format_duration(Duration::from_secs(3661)), "1h 1m 1s");
        assert_eq!(format_duration(Duration::from_secs(7325)), "2h 2m 5s");
    }
}
