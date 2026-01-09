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
    run: command              # Shell command
    timeout: 10s              # Optional (default: 10s)
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
  env_file: .env
  env:
    NODE_ENV: production

jobs:
  test:
    name: "Run Tests"
    schedule:
      cron: "0 */6 * * *"
    run: npm test
    working_dir: ./frontend
    env:
      NODE_ENV: test       # Override for testing
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
