# rollcron

Self-updating cron scheduler that pulls job definitions from git.

## Installation

### Cargo

```bash
cargo install --path .
```

### Nix

```bash
# Run directly
nix run github:ut-code/rollcron -- /path/to/repo

# Install
nix profile install github:ut-code/rollcron

# Open a shell
nix shell github:ut-code/rollcron
```

I recommend installing this via https://github.com/aster-void/nix-repository for build cache.

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

jobs:
  <job-id>:                   # Key is the job ID (used for directories)
    name: "Display Name"      # Optional: display name (defaults to job ID)
    schedule:
      cron: "* * * * *"       # Cron expression (5 fields)
    run: command              # Shell command
    timeout: 10s              # Optional (default: 10s)
    jitter: 30s               # Optional: random delay 0-30s before execution
    concurrency: skip         # Optional: parallel|wait|skip|replace (default: skip)
    working_dir: ./subdir     # Optional: working directory (relative to job snapshot dir)
    retry:                    # Optional
      max: 3                  # Max retry attempts
      delay: 1s               # Initial delay (default: 1s), exponential backoff
      jitter: 500ms           # Optional: random variation (default: 25% of delay)
```

### Runner

Global settings that apply to all jobs:

| Field | Description |
|-------|-------------|
| `timezone` | Timezone for cron schedule interpretation. Use IANA names (e.g., `Asia/Tokyo`, `America/New_York`), `inherit` (system timezone), or omit for UTC |

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
3. Creates isolated working directories per job
4. Executes jobs according to their schedules

```
~/.cache/rollcron/
├── repo-abc123/           # Source of truth (git repo)
├── repo-abc123@hello/     # Job "hello" working dir
└── repo-abc123@backup/    # Job "backup" working dir
```

Jobs run in their own snapshot directories, so git pulls don't interfere with running jobs.

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

jobs:
  test:
    name: "Run Tests"
    schedule:
      cron: "0 */6 * * *"
    run: npm test
    working_dir: ./frontend
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

  cleanup:
    schedule:
      cron: "0 3 * * 0"
    run: find /tmp -mtime +7 -delete
    concurrency: replace   # Kill old cleanup, start fresh
```

## License

MIT
