# rollcron

Self-updating cron scheduler that pulls job definitions from git.

## Installation

```bash
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

# Custom pull interval (30 seconds)
rollcron https://github.com/user/repo -i 30
```

## Configuration

### rollcron.yaml

```yaml
jobs:
  <job-id>:                   # Key is the job ID (used for directories)
    name: "Display Name"      # Optional: display name (defaults to job ID)
    schedule:
      cron: "* * * * *"       # Cron expression (5 fields)
    run: command              # Shell command
    timeout: 10s              # Optional (default: 10s)
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

### Timeout

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
rollcron [OPTIONS] [REPO]

Arguments:
  [REPO]  Local path or remote URL [default: .]

Options:
  -i, --interval <SECS>  Pull interval in seconds [default: 60]
  -h, --help             Print help
```

## Example: GitHub Actions-style Workflow

```yaml
jobs:
  test:
    name: "Run Tests"
    schedule:
      cron: "0 */6 * * *"
    run: |
      npm install
      npm test

  deploy:
    name: "Deploy to Production"
    schedule:
      cron: "0 0 * * *"
    run: ./deploy.sh
    timeout: 10m

  cleanup:
    schedule:
      cron: "0 3 * * 0"
    run: find /tmp -mtime +7 -delete
```

## License

MIT
