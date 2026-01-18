# rollcron

Self-updating cron scheduler that pulls job definitions from git.

## Quickstart

1. Install:

```bash
cargo install --path .
```

2. Create `rollcron.yaml` in your repository:

```yaml
jobs:
  hello:
    schedule: "*/5 * * * *"
    run: echo "Hello from rollcron!"
```

3. Run:

```bash
# Local repository
rollcron /path/to/repo

# Remote repository
rollcron https://github.com/user/my-cron-jobs

# Custom pull interval (5 minutes)
rollcron https://github.com/user/repo --pull-interval 300
```

## Examples

### Basic job with retry

```yaml
jobs:
  backup:
    name: "Daily Backup"
    schedule: "0 2 * * *"
    run:
      sh: ./scripts/backup.sh
      timeout: 5m
      retry:
        max: 3
        delay: 10s
```

### Human-readable schedules

```yaml
jobs:
  weekly:
    schedule: "7pm every Thursday"
    run: ./weekly-report.sh

  daily:
    schedule: "every day at 4:00 pm"
    run: ./backup.sh

  frequent:
    schedule: "every 5 minutes"
    run: ./health-check.sh
```

### Build and run (compiled languages)

```yaml
jobs:
  my-app:
    schedule: "0 * * * *"
    build: cargo build --release
    run: ./target/release/app
```

### Docker

```yaml
jobs:
  docker-job:
    schedule: "*/10 * * * *"
    run:
      sh: docker run --rm -v $(pwd):/app -w /app node:20 npm test
      timeout: 5m
```

### With webhooks and environment variables

```yaml
runner:
  timezone: Asia/Tokyo
  env_file: .env
  webhook:
    - url: $DISCORD_WEBHOOK_URL

jobs:
  deploy:
    schedule: "0 0 * * *"
    run:
      sh: ./deploy.sh
      timeout: 10m
      concurrency: skip
    env:
      NODE_ENV: production
```

### Full example

```yaml
runner:
  timezone: America/New_York
  env_file: .env
  env:
    GLOBAL_VAR: value
  webhook:
    - url: $SLACK_WEBHOOK_URL

jobs:
  build-and-run:
    name: "Build & Run App"
    schedule:
      cron: "0 * * * *"
      timezone: Asia/Tokyo
    build:
      sh: cargo build --release
      timeout: 30m
      working_dir: ./src
      env:
        CARGO_INCREMENTAL: "1"
    run:
      sh: ./target/release/my-app
      timeout: 5m
      concurrency: skip
      working_dir: ./dist
      retry:
        max: 3
        delay: 5s
        jitter: 1s
      env_file: .env.run
      env:
        NODE_ENV: production
    log:
      file: output.log
      max_size: 50M
    working_dir: ./app
    env_file: .env.job
    env:
      DEBUG: "false"
    webhook:
      - url: https://hooks.slack.com/custom
```

## All Options

### CLI

```
rollcron [OPTIONS] <REPO>

Arguments:
  <REPO>                      Local path or remote URL

Options:
      --pull-interval <SECS>  Pull interval in seconds [default: 3600]
```

### Configuration (`rollcron.yaml`)

#### `runner` (optional)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `timezone` | string, optional | `UTC` | IANA timezone name (e.g., `Asia/Tokyo`) or `inherit` for system timezone |
| `env_file` | string, optional | - | Path to .env file (relative to repo root) |
| `env` | map, optional | - | Inline environment variables |
| `webhook` | list, optional | - | Default webhooks for all jobs |

#### `jobs.<job-id>`

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string, optional | job-id | Display name |
| `schedule` | string or object | **required** | Cron expression or `{ cron, timezone? }` |
| `build` | string or object, optional | - | Build command or full config |
| `run` | string or object | **required** | Run command or full config |
| `log` | string or object, optional | - | Log file path or full config |
| `enabled` | bool, optional | `true` | Enable/disable job |
| `working_dir` | string, optional | - | Working directory for build and run (can be overridden) |
| `env_file` | string, optional | - | Shared .env file for build and run |
| `env` | map, optional | - | Shared environment variables for build and run |
| `webhook` | list, optional | - | Job-specific webhooks (extends runner webhooks) |

#### `jobs.<job-id>.schedule`

Shorthand: `schedule: "*/5 * * * *"` or `schedule: "7pm every Thursday"`

Full form:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `cron` | string | **required** | Cron expression or English phrase |
| `timezone` | string, optional | runner's | Job-specific timezone override |

#### `jobs.<job-id>.build` (optional)

Shorthand: `build: "cargo build --release"`

Full form:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `sh` | string | **required** | Build command (runs in `build/` directory) |
| `timeout` | duration, optional | run.timeout | Build timeout |
| `working_dir` | string, optional | job's | Working directory (relative to build dir) |
| `env_file` | string, optional | - | Build-specific .env file |
| `env` | map, optional | - | Build-specific environment variables |

#### `jobs.<job-id>.run`

Shorthand: `run: "./app"`

Full form:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `sh` | string | **required** | Run command (runs in `run/` directory) |
| `timeout` | duration, optional | `1h` | Execution timeout |
| `concurrency` | string, optional | `skip` | `parallel`, `wait`, `skip`, or `replace` |
| `working_dir` | string, optional | job's | Working directory (relative to run dir) |
| `env_file` | string, optional | - | Run-specific .env file |
| `env` | map, optional | - | Run-specific environment variables |

#### `jobs.<job-id>.run.retry` (optional)

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `max` | int | **required** | Max retry attempts (must be ≥ 1) |
| `delay` | duration, optional | `1s` | Initial delay (doubles each retry) |
| `jitter` | duration, optional | 25% of delay | Random variation added to delay |

#### `jobs.<job-id>.log` (optional)

Shorthand: `log: "output.log"`

Full form:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `file` | string, optional | - | Log file path (relative to job dir) |
| `max_size` | size, optional | `10M` | Rotate when exceeded |

#### `webhook` entry

| Field | Type | Description |
|-------|------|-------------|
| `type` | string, optional | Webhook type (default: `discord`) |
| `url` | string | Webhook URL (supports `$VAR` expansion) |

### Formats

**Duration**: `500ms`, `30s`, `5m`, `1h`

**Size**: `512K`, `10M`, `1G`, or bytes

**Schedule**: Cron or English phrase

- Cron: `min hour day month weekday` (e.g., `*/5 * * * *` = every 5 minutes)
- English examples:
  - `every 5 minutes`
  - `every day at 16:00`
  - `7pm every Thursday`
  - `Sunday at 12:00`
  - `midnight on Tuesdays`
- 24-hour format preferred
- 12-hour edge case: `12 am` = midnight (00:00), `12 pm` = noon (12:00)

### Environment variable priority

Higher priority overrides lower:

**For build**: `build.env` > `build.env_file` > `job.env` > `job.env_file` > `runner.env` > `runner.env_file` > host

**For run**: `run.env` > `run.env_file` > `job.env` > `job.env_file` > `runner.env` > `runner.env_file` > host

### Concurrency modes

| Mode | Behavior |
|------|----------|
| `parallel` | Run alongside existing instance |
| `wait` | Queue after current finishes |
| `skip` | Skip this trigger (default) |
| `replace` | Kill running instance, start new |

## License

MIT
