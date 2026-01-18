use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// RAII guard that removes a directory on drop unless disarmed.
struct TempDirGuard<'a> {
    path: &'a Path,
    keep: bool,
}

impl<'a> TempDirGuard<'a> {
    fn new(path: &'a Path) -> Self {
        Self { path, keep: false }
    }

    fn disarm(&mut self) {
        self.keep = true;
    }
}

impl Drop for TempDirGuard<'_> {
    fn drop(&mut self) {
        if !self.keep && self.path.exists() {
            let _ = std::fs::remove_dir_all(self.path);
        }
    }
}

/// Generates a cache directory path with random suffix.
pub fn generate_cache_path(source: &str) -> PathBuf {
    let cache_base = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("rollcron");

    let repo_name = source
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or("repo");

    let random_suffix = generate_random_suffix();
    cache_base.join(format!("{}-{}", repo_name, random_suffix))
}

/// Clones repo to specified cache path.
pub fn clone_to(source: &str, cache_dir: &Path) -> Result<()> {
    if let Some(parent) = cache_dir.parent() {
        std::fs::create_dir_all(parent)?;
    }
    clone_repo(source, cache_dir)
}

fn clone_repo(source: &str, dest: &Path) -> Result<()> {
    let dest_str = dest
        .to_str()
        .context("Destination path contains invalid UTF-8")?;
    let output = Command::new("git")
        .args(["clone", source, dest_str])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git clone failed: {}", stderr);
    }

    Ok(())
}

/// Syncs an existing repo. Returns commit range (e.g. "abc123..def456") if new commits were fetched.
pub fn sync_repo(dest: &Path) -> Result<Option<String>> {
    // git clone sets up tracking branches for both local and remote repos
    let has_upstream = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "@{upstream}"])
        .current_dir(dest)
        .env("LC_ALL", "C") // Ensure consistent English output
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    if has_upstream {
        // Get current HEAD before fetch
        let old_head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dest)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

        // Fetch latest from remote
        let fetch = Command::new("git")
            .args(["fetch"])
            .current_dir(dest)
            .env("LC_ALL", "C")
            .output()?;

        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            anyhow::bail!("git fetch failed: {}", stderr);
        }

        // Reset to upstream (handles diverged history)
        let reset = Command::new("git")
            .args(["reset", "--hard", "@{upstream}"])
            .current_dir(dest)
            .env("LC_ALL", "C")
            .output()?;

        if !reset.status.success() {
            let stderr = String::from_utf8_lossy(&reset.stderr);
            anyhow::bail!("git reset failed: {}", stderr);
        }

        // Get new HEAD after reset
        let new_head = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(dest)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

        // Compare old and new HEAD to detect changes
        match (old_head, new_head) {
            (Some(old), Some(new)) if old == new => return Ok(None),
            (Some(old), Some(new)) => {
                let short_old = &old[..7.min(old.len())];
                let short_new = &new[..7.min(new.len())];
                return Ok(Some(format!("{}..{}", short_old, short_new)));
            }
            _ => return Ok(Some("updated".to_string())),
        }
    }

    Ok(None)
}

fn generate_random_suffix() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Use lower 32 bits of nanoseconds XOR'd with process ID for uniqueness
    let unique = (nanos as u32) ^ std::process::id();
    format!("{:08x}", unique)
}

/// Returns the base job directory: ~/.cache/rollcron/<repo>@<job-id>/
pub fn get_job_dir(sot_path: &Path, job_id: &str) -> PathBuf {
    let cache_base = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("rollcron");

    let sot_name = sot_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    cache_base.join(format!("{}@{}", sot_name, job_id))
}

/// Returns the build directory: ~/.cache/rollcron/<repo>@<job-id>/build/
pub fn get_build_dir(sot_path: &Path, job_id: &str) -> PathBuf {
    get_job_dir(sot_path, job_id).join("build")
}

/// Returns the run directory: ~/.cache/rollcron/<repo>@<job-id>/run/
pub fn get_run_dir(sot_path: &Path, job_id: &str) -> PathBuf {
    get_job_dir(sot_path, job_id).join("run")
}

#[allow(dead_code)]
#[deprecated(note = "Use sync_to_build_dir and copy_build_to_run instead")]
pub fn sync_to_job_dir(sot_path: &Path, job_dir: &Path) -> Result<()> {
    let job_dir_str = job_dir
        .to_str()
        .context("Job directory path contains invalid UTF-8")?;

    // Use atomic temp directory to avoid TOCTOU race condition
    let temp_dir = job_dir.with_extension("tmp");
    let temp_dir_str = temp_dir
        .to_str()
        .context("Temp directory path contains invalid UTF-8")?;

    // Clean up any leftover temp directory
    if temp_dir.exists() {
        std::fs::remove_dir_all(&temp_dir)?;
    }
    std::fs::create_dir_all(&temp_dir)?;

    // RAII guard ensures temp_dir is cleaned up on any error path
    let mut temp_guard = TempDirGuard::new(&temp_dir);

    // Use git archive for all repos (both local and remote use git clone now)
    let archive = Command::new("git")
        .args(["archive", "HEAD"])
        .current_dir(sot_path)
        .output()?;

    if !archive.status.success() {
        // temp_guard will clean up on drop
        let stderr = String::from_utf8_lossy(&archive.stderr);
        anyhow::bail!("git archive failed: {}", stderr);
    }

    // Extract (git archive output uses relative paths, tar strips leading '/' by default)
    let mut extract = Command::new("tar")
        .args(["-x"])
        .current_dir(&temp_dir)
        .stdin(std::process::Stdio::piped())
        .spawn()?;

    {
        use std::io::Write;
        let stdin = extract
            .stdin
            .as_mut()
            .context("Failed to open tar stdin")?;
        stdin.write_all(&archive.stdout)?;
    }

    let status = extract.wait()?;
    if !status.success() {
        // temp_guard will clean up on drop
        anyhow::bail!("tar extraction failed with exit code: {:?}", status.code());
    }

    // Disarm the guard before rename - we'll handle cleanup manually from here
    temp_guard.disarm();

    // Safe swap: rename old to backup, rename temp to target, then remove backup.
    // This minimizes the window where job_dir doesn't exist.
    let backup_dir = job_dir.with_extension("old");

    // Clean up any leftover backup directory
    if backup_dir.exists() {
        let _ = std::fs::remove_dir_all(&backup_dir);
    }

    if job_dir.exists() {
        // Move existing to backup first
        std::fs::rename(job_dir, &backup_dir).with_context(|| {
            format!("Failed to rename {} to backup", job_dir_str)
        })?;
    }

    // Move new directory into place
    std::fs::rename(&temp_dir, job_dir).with_context(|| {
        format!("Failed to rename {} to {}", temp_dir_str, job_dir_str)
    })?;

    // Remove backup (ignore errors as it's cleanup)
    if backup_dir.exists() {
        let _ = std::fs::remove_dir_all(&backup_dir);
    }

    Ok(())
}

/// Syncs the build directory using git worktree.
/// First run: `git worktree add --detach <build_dir>`
/// Subsequent: `git -C <build_dir> fetch && git -C <build_dir> reset --hard @{upstream}`
/// Gitignored files (build cache) are preserved.
pub fn sync_to_build_dir(sot_path: &Path, build_dir: &Path) -> Result<()> {
    if build_dir.join(".git").exists() {
        // Worktree already exists - update it
        let fetch = Command::new("git")
            .args(["fetch", "--all"])
            .current_dir(build_dir)
            .env("LC_ALL", "C")
            .output()?;

        if !fetch.status.success() {
            let stderr = String::from_utf8_lossy(&fetch.stderr);
            anyhow::bail!("git fetch failed: {}", stderr);
        }

        // Get the upstream ref from the main repo
        let upstream_ref = Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(sot_path)
            .output()?;

        if !upstream_ref.status.success() {
            anyhow::bail!("Failed to get HEAD from main repo");
        }

        let commit = String::from_utf8_lossy(&upstream_ref.stdout).trim().to_string();

        let reset = Command::new("git")
            .args(["reset", "--hard", &commit])
            .current_dir(build_dir)
            .env("LC_ALL", "C")
            .output()?;

        if !reset.status.success() {
            let stderr = String::from_utf8_lossy(&reset.stderr);
            anyhow::bail!("git reset failed: {}", stderr);
        }
    } else {
        // First run - create worktree
        if let Some(parent) = build_dir.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let build_dir_str = build_dir
            .to_str()
            .context("Build directory path contains invalid UTF-8")?;

        let worktree = Command::new("git")
            .args(["worktree", "add", "--detach", build_dir_str])
            .current_dir(sot_path)
            .env("LC_ALL", "C")
            .output()?;

        if !worktree.status.success() {
            let stderr = String::from_utf8_lossy(&worktree.stderr);
            anyhow::bail!("git worktree add failed: {}", stderr);
        }
    }

    Ok(())
}

/// Copies build directory to run directory atomically.
/// Excludes .git directory.
pub fn copy_build_to_run(build_dir: &Path, run_dir: &Path) -> Result<()> {
    let run_dir_str = run_dir
        .to_str()
        .context("Run directory path contains invalid UTF-8")?;

    // Use atomic temp directory
    let temp_dir = run_dir.with_extension("tmp");
    let temp_dir_str = temp_dir
        .to_str()
        .context("Temp directory path contains invalid UTF-8")?;

    // Clean up any leftover temp directory
    if temp_dir.exists() {
        std::fs::remove_dir_all(&temp_dir)?;
    }

    // RAII guard ensures temp_dir is cleaned up on any error path
    let mut temp_guard = TempDirGuard::new(&temp_dir);

    // Copy using rsync-like approach with tar (excludes .git)
    let archive = Command::new("tar")
        .args(["--exclude=.git", "-c", "."])
        .current_dir(build_dir)
        .output()?;

    if !archive.status.success() {
        let stderr = String::from_utf8_lossy(&archive.stderr);
        anyhow::bail!("tar archive failed: {}", stderr);
    }

    std::fs::create_dir_all(&temp_dir)?;

    let mut extract = Command::new("tar")
        .args(["-x"])
        .current_dir(&temp_dir)
        .stdin(std::process::Stdio::piped())
        .spawn()?;

    {
        use std::io::Write;
        let stdin = extract
            .stdin
            .as_mut()
            .context("Failed to open tar stdin")?;
        stdin.write_all(&archive.stdout)?;
    }

    let status = extract.wait()?;
    if !status.success() {
        anyhow::bail!("tar extraction failed with exit code: {:?}", status.code());
    }

    // Disarm the guard before rename
    temp_guard.disarm();

    // Safe swap: rename old to backup, rename temp to target, then remove backup
    let backup_dir = run_dir.with_extension("old");

    if backup_dir.exists() {
        let _ = std::fs::remove_dir_all(&backup_dir);
    }

    if run_dir.exists() {
        std::fs::rename(run_dir, &backup_dir).with_context(|| {
            format!("Failed to rename {} to backup", run_dir_str)
        })?;
    }

    std::fs::rename(&temp_dir, run_dir).with_context(|| {
        format!("Failed to rename {} to {}", temp_dir_str, run_dir_str)
    })?;

    if backup_dir.exists() {
        let _ = std::fs::remove_dir_all(&backup_dir);
    }

    Ok(())
}

/// Removes the sot_path and all associated job directories.
pub fn cleanup_cache_dir(sot_path: &Path, job_ids: &[String]) {
    use tracing::{info, warn};

    // Remove job directories
    for job_id in job_ids {
        let job_dir = get_job_dir(sot_path, job_id);
        let build_dir = get_build_dir(sot_path, job_id);

        // Remove git worktree first (if it exists)
        if build_dir.join(".git").exists() {
            let build_dir_str = build_dir.to_string_lossy();
            let result = Command::new("git")
                .args(["worktree", "remove", "--force", &*build_dir_str])
                .current_dir(sot_path)
                .output();

            if let Err(e) = result {
                warn!(path = %build_dir.display(), error = %e, "Failed to remove git worktree");
            }
        }

        if job_dir.exists() {
            info!(path = %job_dir.display(), "Removing job directory");
            let _ = std::fs::remove_dir_all(&job_dir);
        }

        // Also remove temp/old variants for run dir
        let run_dir = get_run_dir(sot_path, job_id);
        let _ = std::fs::remove_dir_all(run_dir.with_extension("tmp"));
        let _ = std::fs::remove_dir_all(run_dir.with_extension("old"));
    }

    // Remove sot_path
    if sot_path.exists() {
        info!(path = %sot_path.display(), "Removing cache directory");
        let _ = std::fs::remove_dir_all(sot_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_path_from_url() {
        let dir = generate_cache_path("https://github.com/user/myrepo.git");
        assert!(dir.to_str().unwrap().contains("myrepo"));
    }

    #[test]
    fn cache_path_is_random() {
        let dir1 = generate_cache_path("https://github.com/user/repo.git");
        std::thread::sleep(std::time::Duration::from_millis(1));
        let dir2 = generate_cache_path("https://github.com/user/repo.git");
        assert_ne!(dir1, dir2);
    }
}
