# rollcron Specification

## Overview

rollcron is a self-updating cron scheduler that pulls job definitions from a git repository and executes them according to their schedules.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                        rollcron                             │
├─────────────────────────────────────────────────────────────┤
│  ┌─────────────┐    ┌─────────────┐    ┌─────────────────┐ │
│  │  Pull Task  │───▶│   Config    │───▶│   Scheduler     │ │
│  │  (interval) │    │   Parser    │    │   (per-second)  │ │
│  └─────────────┘    └─────────────┘    └─────────────────┘ │
│         │                                      │            │
│         ▼                                      ▼            │
│  ┌─────────────┐                       ┌─────────────────┐ │
│  │  git pull   │                       │  Job Executor   │ │
│  │  + archive  │                       │  (with timeout) │ │
│  └─────────────┘                       └─────────────────┘ │
└─────────────────────────────────────────────────────────────┘
```

## Directory Structure

```
~/.cache/rollcron/
├── <repo>-<hash>/              # Source of Truth (SoT)
│   ├── .git/
│   └── rollcron.yaml
├── <repo>-<hash>@<job-id>/     # Job working directory (by ID)
│   └── (git archive snapshot, no .git)
└── <repo>-<hash>@<job-id-2>/
    └── (git archive snapshot, no .git)
```

## Configuration Format

File: `rollcron.yaml`

```yaml
jobs:
  <job-id>:                    # Required: unique job identifier (key)
    name: <string>             # Optional: display name (defaults to job-id)
    schedule:
      cron: "<cron-expr>"      # Required: 5-field cron expression
    run: <string>              # Required: shell command to execute
    timeout: <duration>        # Optional: default "10s"
    concurrency: <strategy>    # Optional: default "skip"
```

### Concurrency Strategies

| Strategy | Behavior |
|----------|----------|
| `parallel` | Allow concurrent runs |
| `wait` | Wait for previous run to complete |
| `skip` | Skip if previous run still active (default) |
| `replace` | Kill previous run, start new |

### Example

```yaml
jobs:
  build:
    name: "Build Project"
    schedule:
      cron: "0 * * * *"
    run: cargo build --release
    timeout: 5m
    concurrency: wait

  health-check:
    schedule:
      cron: "*/5 * * * *"
    run: curl -f http://localhost/health
    concurrency: skip
```

### Cron Expression

Standard 5-field format: `minute hour day-of-month month day-of-week`

Examples:
- `*/5 * * * *` - every 5 minutes
- `0 * * * *` - every hour
- `0 2 * * *` - daily at 2:00 AM
- `0 0 * * 0` - weekly on Sunday

### Duration Format

- `<n>s` - seconds (e.g., `30s`)
- `<n>m` - minutes (e.g., `5m`)
- `<n>h` - hours (e.g., `1h`)

## Execution Model

### Pull Cycle

1. Wait for pull interval
2. Execute `git pull --ff-only` on SoT
3. Parse `rollcron.yaml`
4. For each job: `git archive HEAD | tar -x` to job directory (by job ID)
5. Notify scheduler of config update

### Job Execution

1. Scheduler checks every second
2. If job schedule matches current time (within 1s window):
   - Spawn async task
   - Execute command in job's working directory
   - Apply timeout
   - Log result with display name

### Atomicity

- Jobs run in isolated directories (snapshots)
- Pull/sync happens independently of job execution
- Jobs can run during pull without interference
- Config reload triggers scheduler restart

## CLI Interface

```
rollcron [OPTIONS] [REPO]

Arguments:
  [REPO]  Path to local repo or remote URL [default: .]

Options:
  -i, --interval <SECONDS>  Pull interval [default: 60]
  -h, --help               Print help
```

## Error Handling

| Error | Behavior |
|-------|----------|
| git pull fails | Log error, retry next interval |
| Config parse fails | Log error, keep previous config |
| Job sync fails | Log error, skip config reload |
| Job timeout | Kill job, log timeout error |
| Job command fails | Log stderr, continue scheduler |

## Dependencies

- `git` - for clone, pull, archive operations
- `tar` - for extracting archives
- `sh` - for executing job commands
