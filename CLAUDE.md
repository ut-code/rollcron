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
// All support shorthand (string) or full (object) form via #[serde(untagged)]

enum ScheduleConfigRaw {
    Simple(String),        // "*/5 * * * *" or "7pm every Thursday"
    Full { cron: String, timezone: Option<String> },  // cron also accepts English
}

enum BuildConfigRaw {
    Simple(String),        // "cargo build"
    Full {
        sh: String,
        timeout: Option<String>,
        env_file: Option<String>,
        env: Option<HashMap<String, String>>,
        working_dir: Option<String>,
    },
}

enum RunConfigRaw {
    Simple(String),        // "./app"
    Full {
        sh: String,
        timeout: String,       // Default: "1h"
        concurrency: Concurrency,
        retry: Option<RetryConfigRaw>,
        working_dir: Option<String>,
        env_file: Option<String>,
        env: Option<HashMap<String, String>>,
    },
}

enum LogConfigRaw {
    Simple(String),        // "output.log"
    Full {
        file: Option<String>,
        max_size: String,      // Default: "10M"
    },
}

struct JobConfig {
    name: Option<String>,
    schedule: ScheduleConfigRaw,
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
```

## Config Format

### Minimal (shorthand syntax)

```yaml
jobs:
  hello:
    schedule: "*/5 * * * *"   # Shorthand for { cron: "..." }
    run: echo hello           # Shorthand for { sh: "..." }
```

### With build step

```yaml
jobs:
  my-app:
    schedule: "0 * * * *"
    build: cargo build        # Shorthand for { sh: "..." }
    run: ./target/debug/app
    log: output.log           # Shorthand for { file: "..." }
```

### English schedule (alternative to cron)

```yaml
jobs:
  weekly:
    schedule: "7pm every Thursday"    # Human-readable schedule
    run: ./weekly-report.sh

  daily:
    schedule: "every day at 4:00 pm"  # Also supported
    run: ./backup.sh

  frequent:
    schedule: "every 5 minutes"       # Interval-based
    run: ./health-check.sh
```

Supported patterns (via [english-to-cron](https://github.com/kaplanelad/english-to-cron)):
- `"every minute"`, `"every 5 minutes"`
- `"every day at 4:00 pm"`, `"at 10:00 am"`
- `"7pm every Thursday"`, `"Sunday at 12:00"`
- `"midnight on Tuesdays"`, `"midnight on the 1st and 15th"`
- `"noon every 3 days"` (interval-based)

**Not supported** (silently misparsed):
- `"midnight every weekday"`, `"noon every 2 weeks"`

Standard cron syntax is tried first; English is used as fallback.

### Full syntax (all options)

```yaml
runner:
  timezone: Asia/Tokyo         # IANA name, "inherit" (system), or omit for UTC
  env_file: .env
  env: { KEY: value }
  webhook:
    - url: $DISCORD_WEBHOOK_URL

jobs:
  <job-id>:
    name: "Display Name"
    schedule: { cron: "*/5 * * * *", timezone: Asia/Tokyo }
    build: { sh: cargo build, timeout: 30m, working_dir: ./subdir }
    run: { sh: ./app, timeout: 10s, concurrency: skip, working_dir: ./subdir,
           retry: { max: 3, delay: 1s, jitter: 500ms } }
    log: { file: output.log, max_size: 10M }
    working_dir: ./subdir
    env_file: .env
    env: { KEY: value }
    webhook: [{ url: https://... }]
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
5. **Schedule format**: Standard cron or English phrases (via `croner` + `english-to-cron`)

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

When `log` is set, command stdout/stderr is appended to the specified file. If not set, output is discarded.

```yaml
jobs:
  backup:
    schedule: "0 2 * * *"
    run: ./scripts/backup.sh
    log: backup.log           # Shorthand: written to <job_dir>/backup.log

  # Or with rotation settings:
  backup-full:
    schedule: "0 2 * * *"
    run: ./scripts/backup.sh
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
    schedule: "* * * * *"
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
