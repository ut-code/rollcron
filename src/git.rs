use anyhow::Result;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Ensures repo is cloned/synced to cache. Returns cache path.
pub fn ensure_repo(source: &str) -> Result<PathBuf> {
    let cache_dir = get_cache_dir(source)?;
    std::fs::create_dir_all(cache_dir.parent().unwrap())?;

    if cache_dir.exists() {
        sync_repo(source, &cache_dir)?;
    } else {
        clone_repo(source, &cache_dir)?;
    }

    Ok(cache_dir)
}

fn is_remote(source: &str) -> bool {
    source.starts_with("https://")
        || source.starts_with("git@")
        || source.starts_with("ssh://")
        || source.starts_with("git://")
}

fn clone_repo(source: &str, dest: &Path) -> Result<()> {
    if is_remote(source) {
        let output = Command::new("git")
            .args(["clone", source, dest.to_str().unwrap()])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("git clone failed: {}", stderr);
        }
    } else {
        // Local: rsync entire directory (including uncommitted changes)
        rsync_local(source, dest)?;
    }

    Ok(())
}

fn sync_repo(source: &str, dest: &Path) -> Result<()> {
    if is_remote(source) {
        // Remote: git pull
        let has_upstream = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "@{upstream}"])
            .current_dir(dest)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

        if has_upstream {
            let output = Command::new("git")
                .args(["pull", "--ff-only"])
                .current_dir(dest)
                .output()?;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                anyhow::bail!("git pull failed: {}", stderr);
            }
        }
    } else {
        // Local: rsync (syncs uncommitted changes too)
        rsync_local(source, dest)?;
    }

    Ok(())
}

fn rsync_local(source: &str, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;

    let output = Command::new("rsync")
        .args([
            "-a",
            "--delete",
            "--exclude", ".git",
            &format!("{}/", source),
            dest.to_str().unwrap(),
        ])
        .output()?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("rsync failed: {}", stderr);
    }

    Ok(())
}

fn get_cache_dir(source: &str) -> Result<PathBuf> {
    let cache_base = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("rollcron");

    let repo_name = source
        .trim_end_matches('/')
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or("repo");

    let hash = &format!("{:x}", hash_str(source))[..8];

    Ok(cache_base.join(format!("{}-{}", repo_name, hash)))
}

fn hash_str(input: &str) -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    input.hash(&mut hasher);
    hasher.finish()
}

pub fn get_job_dir(sot_path: &Path, job_id: &str) -> PathBuf {
    let cache_base = dirs::cache_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("rollcron");

    let sot_name = sot_path.file_name().unwrap().to_str().unwrap();

    cache_base.join(format!("{}@{}", sot_name, job_id))
}

pub fn sync_to_job_dir(sot_path: &Path, job_dir: &Path) -> Result<()> {
    if job_dir.exists() {
        std::fs::remove_dir_all(job_dir)?;
    }
    std::fs::create_dir_all(job_dir)?;

    // Check if .git exists (remote repos have it, local rsync'd repos don't)
    if sot_path.join(".git").exists() {
        // Use git archive for git repos
        let archive = Command::new("git")
            .args(["archive", "HEAD"])
            .current_dir(sot_path)
            .output()?;

        if !archive.status.success() {
            let stderr = String::from_utf8_lossy(&archive.stderr);
            anyhow::bail!("git archive failed: {}", stderr);
        }

        let extract = Command::new("tar")
            .args(["-x"])
            .current_dir(job_dir)
            .stdin(std::process::Stdio::piped())
            .spawn()?;

        use std::io::Write;
        extract.stdin.unwrap().write_all(&archive.stdout)?;
    } else {
        // For non-git dirs (rsync'd local repos), use rsync
        let output = Command::new("rsync")
            .args([
                "-a",
                "--delete",
                &format!("{}/", sot_path.to_str().unwrap()),
                job_dir.to_str().unwrap(),
            ])
            .output()?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("rsync failed: {}", stderr);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_remote_urls() {
        assert!(is_remote("https://github.com/user/repo"));
        assert!(is_remote("git@github.com:user/repo.git"));
        assert!(!is_remote("/home/user/repo"));
        assert!(!is_remote("."));
    }

    #[test]
    fn cache_dir_from_url() {
        let dir = get_cache_dir("https://github.com/user/myrepo.git").unwrap();
        assert!(dir.to_str().unwrap().contains("myrepo"));
    }
}
