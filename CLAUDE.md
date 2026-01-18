# CLAUDE.md

## Project Overview

rollcron is a Rust CLI tool that functions as a self-updating cron scheduler, similar to GitHub Actions but for local/remote git repositories.

## Directory Structure

```
src/
├── main.rs                 # Entry point, CLI parsing
├── actor/
│   ├── runner/             # Runner Actor - lifecycle management
│   │   ├── mod.rs          # Actor definition, messages
│   │   ├── git_poll.rs     # git fetch/reset loop
│   │   └── lifecycle.rs    # Job Actor supervision
│   └── job/                # Job Actor - single job control
│       ├── mod.rs          # Actor definition, state machine
│       ├── tick.rs         # cron schedule evaluation
│       └── executor.rs     # command execution, retry, timeout
├── config.rs               # YAML config parsing, Job struct
├── git.rs                  # Git operations (clone, pull, archive)
├── env.rs                  # Environment variable handling
├── logging.rs              # Logging setup
└── webhook.rs              # Discord webhook notifications
```

## Key Types

```rust
// config.rs - Raw config structs (deserialized from YAML)
struct BuildConfigRaw {
    sh: String,            // Build command (runs in build/ dir)
    timeout: Option<String>,
    env_file: Option<String>,
    env: Option<HashMap<String, String>>,
    working_dir: Option<String>,
}

struct RunConfigRaw {
    sh: String,            // Run command (runs in run/ dir)
    timeout: String,       // Default: "1h"
    concurrency: Concurrency,
    retry: Option<RetryConfigRaw>,
    working_dir: Option<String>,
    env_file: Option<String>,
    env: Option<HashMap<String, String>>,
}

struct LogConfigRaw {
    file: Option<String>,  // Path to log file (relative to run dir)
    max_size: String,      // Default: "10M"
}

struct JobConfig {
    name: Option<String>,
    schedule: ScheduleConfig,
    build: Option<BuildConfigRaw>,
    run: RunConfigRaw,
    log: Option<LogConfigRaw>,
    enabled: Option<bool>,
    env_file: Option<String>,
    env: Option<HashMap<String, String>>,
    working_dir: Option<String>,
    webhook: Vec<WebhookConfig>,
}

// config.rs - Runtime structs (parsed and validated)
struct BuildConfig {
    command: String,      // Build command (runs in build/ dir)
    timeout: Duration,    // Timeout for build (defaults to run.timeout)
    env_file: Option<String>,     // From build.env_file
    env: Option<HashMap<String, String>>,  // From build.env
    working_dir: Option<String>,  // build.working_dir || job.working_dir
}

struct Job {
    id: String,           // Key from YAML (used for directories)
    name: String,         // Display name (defaults to id)
    schedule: croner::Cron,
    build: Option<BuildConfig>,
    command: String,      // From run.sh
    timeout: Duration,    // From run.timeout
    concurrency: Concurrency,
    retry: Option<RetryConfig>,
    working_dir: Option<String>,  // run.working_dir || job.working_dir
    log_file: Option<String>,     // From log.file
    log_max_size: u64,            // From log.max_size
    env_file: Option<String>,     // Job-level (shared by build & run)
    env: Option<HashMap<String, String>>,
    run_env_file: Option<String>, // From run.env_file
    run_env: Option<HashMap<String, String>>,  // From run.env
    webhook: Vec<WebhookConfig>,
}

struct WebhookConfig {
    webhook_type: String,  // Currently only "discord" (default)
    url: String,           // Webhook URL (supports $ENV_VAR expansion)
}

struct RetryConfig {
    max: u32,             // Max retry attempts
    delay: Duration,      // Initial delay (exponential backoff)
    jitter: Option<Duration>,  // Random variation added to retry delay (0 to jitter)
                               // Auto-inferred as 25% of delay when not set
}

struct RunnerConfig {
    timezone: TimezoneConfig,
    env_file: Option<String>,  // Path to .env file (relative to repo root)
    env: Option<HashMap<String, String>>,  // Inline env vars
}

struct ScheduleConfig { cron: String, timezone: Option<String> }
```

## Config Format

```yaml
runner:                        # Optional: global settings
  timezone: Asia/Tokyo         # Optional: IANA name, "inherit" (system), or omit for UTC
  env_file: .env               # Optional: load env vars from file
  env:                         # Optional: inline env vars
    KEY: value
  webhook:                     # Optional: failure notifications (inherited by all jobs)
    - url: $DISCORD_WEBHOOK_URL  # URL or $ENV_VAR (loaded from runner.env_file)
    - type: discord              # Optional: defaults to "discord"
      url: https://discord.com/api/webhooks/...

jobs:
  <job-id>:                  # Key = ID (used for directories)
    name: "Display Name"     # Optional (defaults to job-id)
    schedule:
      cron: "*/5 * * * *"
      timezone: Asia/Tokyo   # Optional: job-level timezone override
    build:                   # Optional: build configuration
      sh: cargo build        # Build command (runs in build/ directory)
      timeout: 30m           # Optional: timeout for build (defaults to run.timeout)
      working_dir: ./subdir  # Optional: working directory (relative to build dir)
    run:                     # Run configuration
      sh: ./target/debug/app # Run command (runs in run/ directory)
      timeout: 10s           # Optional (default: 1h)
      concurrency: skip      # Optional: parallel|wait|skip|replace (default: skip)
      working_dir: ./subdir  # Optional: working directory (relative to run dir)
      retry:                 # Optional
        max: 3               # Max retry attempts
        delay: 1s            # Initial delay (default: 1s), exponential backoff
        jitter: 500ms        # Optional: random variation 0-500ms added to retry delay
                             # If omitted, auto-inferred as 25% of delay (e.g., 250ms for 1s delay)
    log:                     # Optional: logging configuration
      file: output.log       # File path for stdout/stderr
      max_size: 10M          # Max size before rotation (default: 10M)
    working_dir: ./subdir    # Optional: working directory for build & run (can be overridden)
    env_file: .env           # Optional: load env vars from file (relative to job dir)
    env:                     # Optional: inline env vars
      KEY: value
    webhook:                 # Optional: job-level webhooks (extend runner webhooks)
      - url: https://...
```

## Runtime Directory Layout

```
~/.cache/rollcron/
├── <repo>-<random>/                    # SoT: git repository (random suffix per run)
└── <repo>-<random>@<job-id>/
    ├── build/                          # Git worktree for building (preserves build cache)
    └── run/                            # Execution directory (copied from build/)
```

**Important**:
- Directory names use `job.id` (the YAML key), not `job.name`
- Each run creates new directories with a random suffix (cleaned up on exit)
- `build/` is a git worktree - gitignored files (build artifacts) are preserved between syncs
- `run/` is copied from `build/` after successful build (excludes `.git`)

## Toolchain

[mise](https://mise.jdx.dev/) manages the Rust toolchain version (Rust 1.85).

```bash
mise exec -- cargo build    # Run cargo with mise-managed Rust
mise exec -- cargo test     # Run tests
```

## Assumptions

1. **Git available**: `git` command must be in PATH
2. **Tar available**: `tar` command for archive extraction
3. **Shell available**: Jobs run via `sh -c "<command>"`
4. **Remote auth**: SSH keys or credentials pre-configured for remote repos
5. **Cron format**: Standard 5-field cron (via `croner` crate)

## Key Flows

### Startup
1. Parse CLI args (repo, interval)
2. Clone repo to cache via `git clone` (both local and remote)
3. Load config from `rollcron.yaml`
4. Start pull task + scheduler
5. Each job actor triggers initial build/sync

### Pull Cycle (async task)
1. `git fetch` + `git reset --hard @{upstream}`
2. Parse config
3. Notify job actors of config change (triggers build)
4. Send new jobs to scheduler via watch channel

### Build Flow (per job)
1. Sync build/ directory via git worktree
2. Run build command (if configured) with build.timeout
3. On success: copy build/ to run/ (atomic, excludes .git)
4. On failure: send webhook notification, keep old run/

### Job Execution
1. Each job calculates next occurrence and sleeps until scheduled time
2. When scheduled time arrives: spawn task in run/ directory with timeout
3. On failure: apply exponential backoff + retry jitter before retry
4. After job completes: try to copy pending build if any

### Shutdown (Ctrl+C)
1. Wait for running builds to complete
2. Wait for running jobs to complete (graceful stop)
3. Stop all job actors
4. Remove all cache directories (worktrees + job dirs)

## Logging

When `log.file` is set, command stdout/stderr is appended to the specified file. If not set, output is discarded.

```yaml
jobs:
  backup:
    run:
      sh: ./scripts/backup.sh
    log:
      file: backup.log        # Written to <job_dir>/backup.log
      max_size: 50M           # Rotate when file exceeds 50MB
```

**Rotation**: When log file exceeds `log.max_size`:
1. `backup.log` → `backup.log.old`
2. Previous `.old` is deleted
3. New `backup.log` created

**Size format**: `10M` (megabytes), `1G` (gigabytes), `512K` (kilobytes), or bytes

## Webhooks

Webhooks send Discord notifications for:
- **Job failures** (after all retries exhausted)
- **Build failures** (when build command fails)
- **Config parse errors** (runner-level webhooks only)

```yaml
runner:
  env_file: .env               # Load DISCORD_WEBHOOK_URL from here
  webhook:
    - url: $DISCORD_WEBHOOK_URL  # Expanded from env_file
```

**Format**: `{ type?: "discord", url: string }` where `type` defaults to "discord".

**Payloads**:
- Job failure: Discord embed (red) with Job, Attempts, Error, Stderr fields
- Build failure: Discord embed (orange) with Job, Error, Stderr fields
- Config error: Discord embed (orange) with Error field

**Inheritance**: Job webhooks extend runner webhooks (both are notified on job/build failure).

## Environment Variables

Environment variables can be set at runner, job, build, or run level.

```yaml
runner:
  env_file: .env.global      # Loaded from repo root
  env:
    GLOBAL_VAR: value

jobs:
  my-job:
    schedule:
      cron: "* * * * *"
    build:
      sh: cargo build
      env_file: .env.build   # Build-specific
      env:
        CARGO_INCREMENTAL: "1"
    run:
      sh: ./app
      env_file: .env.run     # Run-specific
      env:
        NODE_ENV: production
    env_file: .env.job       # Shared by build and run
    env:
      JOB_VAR: value
```

**Priority for build** (later overrides earlier):
```
host ENV < runner.env_file < runner.env < job.env_file < job.env < build.env_file < build.env
```

**Priority for run** (later overrides earlier):
```
host ENV < runner.env_file < runner.env < job.env_file < job.env < run.env_file < run.env
```

**Shell expansion**: Values support `~` and `$VAR` / `${VAR}` expansion.

## Constraints

- `cargo build` must pass
- `cargo test` must pass

## Testing

```bash
mise exec -- cargo test                    # Run all tests
mise exec -- cargo run -- --help          # Check CLI
mise exec -- cargo run -- . -i 10         # Test with local repo
```

## Common Modifications

### Add new config field
1. Update `JobConfig` in `config.rs`
2. Update `Job` struct if runtime field
3. Add to `parse_config()` conversion
4. Add test case

### Change sync mechanism
- Edit `sync_to_build_dir()` and `copy_build_to_run()` in `git.rs`
- Build sync: `git worktree add/reset` (preserves gitignored files)
- Run copy: `tar --exclude=.git -c | tar -x` (atomic swap)

### Add CLI flag
1. Add field to `Args` struct in `main.rs`
2. Use `#[arg(...)]` attribute for clap
