# rollcron

Self-updating cron scheduler that pulls job definitions from git.

## Installation

### Cargo

```bash
cargo install --path .
```

### mise

This project uses [mise](https://mise.jdx.dev/) for tool version management. After installing mise:

```bash
mise install
cargo install --path .
```

## Quick Start

1. Create `rollcron.yaml` in your repository:

```yaml
jobs:
  hello:
    schedule:
      cron: "*/5 * * * *"
    run: echo "Hello from rollcron!"

  backup:
    name: "Daily Backup"
    schedule:
      cron: "0 2 * * *"
    run: ./scripts/backup.sh
    timeout: 5m
```

2. Run rollcron:

```bash
# Local repository
rollcron /path/to/repo

# Remote repository (auto-clones)
rollcron https://github.com/user/my-cron-jobs

# Custom pull interval (5 minutes)
rollcron https://github.com/user/repo --pull-interval 300
```

or, use this repo:

```sh
rollcron https://github.com/ut-code/rollcron
```

## Configuration

### rollcron.yaml

```yaml
runner:                       # Optional: global settings
  timezone: Asia/Tokyo        # Timezone for cron schedules (default: UTC)
                              # Use "inherit" to use system timezone
  env_file: .env              # Optional: global .env file (relative to repo root)
  env:                        # Optional: global environment variables
    KEY: value
  webhook:                    # Optional: default webhooks for all jobs
    - url: $SLACK_WEBHOOK     # URL format (env vars expanded)

jobs:
  <job-id>:                   # Key is the job ID (used for directories)
    name: "Display Name"      # Optional: display name (defaults to job ID)
    schedule:
      cron: "* * * * *"       # Cron expression (5 fields)
    build: make               # Optional: build command (runs in build/ dir, preserves cache)
    build_timeout: 30m        # Optional: timeout for build (defaults to timeout)
    run: ./app                # Shell command (runs in run/ dir)
    timeout: 10s              # Optional (default: 1h)
    jitter: 30s               # Optional: random delay 0-30s before execution
    concurrency: skip         # Optional: parallel|wait|skip|replace (default: skip)
    working_dir: ./subdir     # Optional: working directory (relative to job snapshot dir)
    env_file: .env.local      # Optional: .env file (relative to job snapshot dir)
    env:                      # Optional: inline environment variables
      KEY: value
    webhook:                  # Optional: webhook list (extends runner webhooks)
      - url: https://...      # URL format
      - id: "..."             # Discord format
        token: "..."
    retry:                    # Optional
      max: 3                  # Max retry attempts
      delay: 1s               # Initial delay (default: 1s), exponential backoff
      jitter: 500ms           # Optional: random variation (default: 25% of delay)
    log_file: output.log      # Optional: file for stdout/stderr (relative to job dir)
    log_max_size: 10M         # Optional: rotate when exceeded (default: 10M)
```

### Runner

Global settings that apply to all jobs:

| Field | Description |
|-------|-------------|
| `timezone` | Timezone for cron schedule interpretation. Use IANA names (e.g., `Asia/Tokyo`, `America/New_York`), `inherit` (system timezone), or omit for UTC |
| `env_file` | Path to .env file (relative to repo root) |
| `env` | Inline key-value environment variables |
| `webhook` | List of webhooks for failure notifications (default for all jobs) |

### Webhooks

Send notifications when jobs fail (after all retries exhausted):

```yaml
runner:
  webhook:                                    # Default webhooks for all jobs
    - url: $SLACK_WEBHOOK_URL                 # URL format (supports $VAR expansion)

jobs:
  critical-job:
    webhook:                                  # Job webhooks extend runner webhooks
      - url: https://hooks.slack.com/...      # Direct URL
      - id: $DISCORD_WEBHOOK_ID               # Discord format: id + token
        token: $DISCORD_WEBHOOK_TOKEN
```

Webhook formats:
- `{ url: "https://..." }` - Direct URL (Slack, custom endpoints)
- `{ id: "...", token: "..." }` - Discord webhook (constructs URL automatically)

Environment variables (`$VAR`, `${VAR}`) are expanded in webhook URLs, so you don't need to commit credentials.

Job webhooks are **added to** runner webhooks (not overriding). If runner has 1 webhook and job has 1, the job will notify both.

Payload format (JSON POST):
```json
{
  "text": "[rollcron] Job 'job-name' failed",
  "job_id": "my-job",
  "job_name": "My Job",
  "error": "exit code 1",
  "stderr": "Error output...",
  "attempts": 3
}
```

### Environment Variables

Jobs can access environment variables from multiple sources. Variables are merged with the following priority (highest wins):

1. `job.env` - Inline variables in job config
2. `job.env_file` - Job-specific .env file (relative to job snapshot dir)
3. `runner.env` - Inline variables in runner config
4. `runner.env_file` - Global .env file (relative to repo root)
5. Host environment - Inherited from the system

**Shell Expansion**: Paths (`env_file`, `working_dir`) and env values support `~` (home directory) and `$VAR` / `${VAR}` (environment variables).

Example:

```yaml
runner:
  env_file: .env                # Load from repo root
  env:
    API_URL: https://api.example.com
    DEBUG: "true"

jobs:
  my-job:
    schedule:
      cron: "*/5 * * * *"
    run: echo $API_URL $JOB_SECRET
    env_file: .env.local        # Load from job snapshot dir
    env:
      DEBUG: "false"            # Overrides runner.env
```

### Concurrency

Controls behavior when a job is triggered while a previous instance is still running:

| Mode | Behavior |
|------|----------|
| `parallel` | Run new instance alongside existing one |
| `wait` | Queue new instance to run after current finishes |
| `skip` | Skip this trigger (default) |
| `replace` | Kill running instance, start new one |

### Jitter

Random delay to prevent thundering herd problems:

- **Task jitter**: Random delay (0 to `jitter`) before job execution
- **Retry jitter**: Random variation added to backoff delay (defaults to 25% of delay if not specified)

### Retry

Jobs can automatically retry on failure with exponential backoff:

```yaml
retry:
  max: 3        # Retry up to 3 times
  delay: 2s     # Initial delay (doubles each retry: 2s, 4s, 8s)
  jitter: 500ms # Optional: random variation (default: 25% of delay)
```

### Build Step

Jobs can optionally include a build step that runs before execution. Build artifacts are cached between syncs:

```yaml
jobs:
  my-app:
    build: cargo build --release    # Runs in build/ directory
    build_timeout: 30m              # Optional (defaults to timeout)
    run: ./target/release/app       # Runs in run/ directory
    timeout: 10s
    schedule:
      cron: "0 * * * *"
```

**Key features**:
- Build runs in a git worktree (`build/`), so `.gitignore`d files (like `target/`, `node_modules/`) are preserved
- After successful build, the result is atomically copied to `run/` for execution
- If build fails, the previous `run/` directory is kept (jobs continue running old version)
- Build failures send webhook notifications (if configured)

**When to use**:
- Compiled languages (Rust, Go, C++)
- Projects with npm/yarn build steps
- Any job that benefits from cached build artifacts

### Logging

Capture job output to a file:

```yaml
jobs:
  backup:
    run: ./backup.sh
    log_file: backup.log    # Written to <job_dir>/backup.log
    log_max_size: 50M       # Rotate when file exceeds 50MB (default: 10M)
```

When `log_file` is set, stdout/stderr is appended to the file. Without it, output is discarded.

**Rotation**: When the log exceeds `log_max_size`, it's renamed to `backup.log.old` (previous `.old` is deleted).

**Size format**: `10M` (megabytes), `1G` (gigabytes), `512K` (kilobytes), or plain bytes.

### Cron Expression

| Field | Values |
|-------|--------|
| Minute | 0-59 |
| Hour | 0-23 |
| Day of Month | 1-31 |
| Month | 1-12 |
| Day of Week | 0-6 (Sun=0) |

Examples:
- `*/5 * * * *` - every 5 minutes
- `0 * * * *` - hourly
- `0 0 * * *` - daily at midnight
- `0 0 * * 0` - weekly on Sunday

### Duration Format

- `500ms` - 500 milliseconds
- `30s` - 30 seconds
- `5m` - 5 minutes
- `1h` - 1 hour

## How It Works

1. Clones/pulls your repository periodically
2. Reads `rollcron.yaml` for job definitions
3. Creates isolated build/run directories per job
4. Runs build commands (if configured) with cache preservation
5. Copies build artifacts to run directory
6. Executes jobs according to their schedules

```
~/.cache/rollcron/
├── repo-abc123/              # Source of truth (git repo)
└── repo-abc123@hello/
    ├── build/                # Git worktree (build cache preserved)
    └── run/                  # Execution directory (copied from build/)
```

**Build workflow**:
- `build/` is a git worktree - gitignored files (build artifacts) are preserved between git syncs
- After successful build, `build/` is atomically copied to `run/` (excluding `.git`)
- Jobs execute in `run/`, which stays stable during builds
- If build fails, the previous `run/` directory is kept

Jobs run in their own snapshot directories, so git pulls and builds don't interfere with running jobs.

## CLI Reference

```
rollcron [OPTIONS] <REPO>

Arguments:
  <REPO>  Local path or remote URL (required)

Options:
      --pull-interval <SECS>  Pull interval in seconds [default: 3600]
  -h, --help                  Print help
```

## Example: GitHub Actions-style Workflow

```yaml
runner:
  timezone: America/New_York
  env_file: .env
  env:
    NODE_ENV: production
  webhook:
    - url: $DISCORD_WEBHOOK_URL

jobs:
  build-and-run:
    name: "Build & Run App"
    schedule:
      cron: "0 * * * *"
    build: cargo build --release    # Cached between syncs
    build_timeout: 30m
    run: ./target/release/my-app
    timeout: 5m

  test:
    name: "Run Tests"
    schedule:
      cron: "0 */6 * * *"
    build: npm ci                   # Install dependencies
    run: npm test
    working_dir: ./frontend
    env:
      NODE_ENV: test
    retry:
      max: 2
      delay: 30s

  deploy:
    name: "Deploy to Production"
    schedule:
      cron: "0 0 * * *"
    run: ./deploy.sh
    timeout: 10m
    concurrency: skip      # Don't deploy if previous deploy is running
    env_file: .env.deploy

  cleanup:
    schedule:
      cron: "0 3 * * 0"
    run: find /tmp -mtime +7 -delete
    concurrency: replace   # Kill old cleanup, start fresh
```

## License

MIT
