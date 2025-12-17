# CLAUDE.md

## Project Overview

rollcron is a Rust CLI tool that functions as a self-updating cron scheduler, similar to GitHub Actions but for local/remote git repositories.

## Directory Structure

```
src/
├── main.rs       # Entry point, CLI parsing, orchestration
├── config.rs     # YAML config parsing, Job struct
├── git.rs        # Git operations (clone, pull, archive)
└── scheduler.rs  # Cron scheduling, job execution
```

## Key Types

```rust
// config.rs
struct Job {
    id: String,           // Key from YAML (used for directories)
    name: String,         // Display name (defaults to id)
    schedule: cron::Schedule,
    command: String,
    timeout: Duration,
    concurrency: Concurrency,
    retry: Option<RetryConfig>,
    jitter: Option<Duration>,  // Random delay before execution (0 to jitter)
}

struct RetryConfig {
    max: u32,             // Max retry attempts
    delay: Duration,      // Initial delay (exponential backoff)
    jitter: Option<Duration>,  // Random variation added to retry delay (0 to jitter)
                               // Auto-inferred as 25% of delay when not set
}

// Parsed from rollcron.yaml
struct Config { jobs: HashMap<String, JobConfig> }
struct JobConfig { name: Option<String>, schedule: ScheduleConfig, run, timeout, jitter }
struct ScheduleConfig { cron: String }
```

## Config Format

```yaml
jobs:
  <job-id>:                  # Key = ID (used for directories)
    name: "Display Name"     # Optional (defaults to job-id)
    schedule:
      cron: "*/5 * * * *"
    run: echo hello
    timeout: 10s             # Optional (default: 10s)
    jitter: 30s              # Optional: random delay 0-30s before execution
    concurrency: skip        # Optional: parallel|wait|skip|replace (default: skip)
    retry:                   # Optional
      max: 3                 # Max retry attempts
      delay: 1s              # Initial delay (default: 1s), exponential backoff
      jitter: 500ms          # Optional: random variation 0-500ms added to retry delay
                             # If omitted, auto-inferred as 25% of delay (e.g., 250ms for 1s delay)
```

## Runtime Directory Layout

```
~/.cache/rollcron/
├── <repo>-<hash>/              # SoT: git repository
└── <repo>-<hash>@<job-id>/     # Per-job snapshot (no .git)
```

**Important**: Directory names use `job.id` (the YAML key), not `job.name`.

## Assumptions

1. **Git available**: `git` command must be in PATH
2. **Tar available**: `tar` command for archive extraction
3. **Shell available**: Jobs run via `sh -c "<command>"`
4. **Remote auth**: SSH keys or credentials pre-configured for remote repos
5. **Cron format**: Standard 5-field cron (internally converted to 6-field for `cron` crate)

## Key Flows

### Startup
1. Parse CLI args (repo, interval)
2. Clone if remote URL, otherwise use local path
3. Load config from `rollcron.yaml`
4. Sync job directories via `git archive` (using job ID)
5. Start pull task + scheduler

### Pull Cycle (async task)
1. `git pull --ff-only`
2. Parse config
3. Sync all job dirs (by job ID)
4. Send new jobs to scheduler via watch channel

### Job Execution
1. Scheduler polls every 1 second
2. Check each job's cron schedule
3. If due: spawn task in job's directory (by ID) with timeout
4. Apply task jitter (random delay 0 to jitter) before first execution
5. On failure: apply exponential backoff + retry jitter before retry
6. Log using display name

## Constraints

- `cargo build` must pass
- `cargo test` must pass

## Testing

```bash
cargo test                    # Run all tests
cargo run -- --help          # Check CLI
cargo run -- . -i 10         # Test with local repo
```

## Common Modifications

### Add new config field
1. Update `JobConfig` in `config.rs`
2. Update `Job` struct if runtime field
3. Add to `parse_config()` conversion
4. Add test case

### Change sync mechanism
- Edit `sync_to_job_dir()` in `git.rs`
- Currently: `git archive HEAD | tar -x`

### Add CLI flag
1. Add field to `Args` struct in `main.rs`
2. Use `#[arg(...)]` attribute for clap
