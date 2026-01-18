use anyhow::{anyhow, Result};
use chrono_tz::Tz;
use croner::Cron;
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;
use tracing::warn;

fn validate_job_id(id: &str) -> Result<()> {
    if id.is_empty() {
        anyhow::bail!("Job ID cannot be empty");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    {
        anyhow::bail!(
            "Invalid job ID '{}': must contain only alphanumeric characters, underscores, and hyphens",
            id
        );
    }
    Ok(())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub enum TimezoneConfig {
    #[default]
    Utc,
    Inherit,
    Named(Tz),
}

/// Webhook configuration for failure notifications
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WebhookConfig {
    /// Webhook type (currently only "discord" supported)
    #[serde(rename = "type", default = "default_webhook_type")]
    pub webhook_type: String,
    /// Webhook URL (supports $ENV_VAR expansion)
    pub url: String,
}

fn default_webhook_type() -> String {
    "discord".to_string()
}

impl WebhookConfig {
    /// Convert to URL string, expanding environment variables.
    /// If env_vars is provided, uses those for $VAR expansion.
    /// Falls back to process environment for undefined variables.
    pub fn to_url(&self, env_vars: Option<&std::collections::HashMap<String, String>>) -> String {
        expand_with_env(&self.url, env_vars)
    }
}

/// Expand shell-like variables in a string.
/// Uses provided env_vars first, then falls back to process environment.
fn expand_with_env(
    s: &str,
    env_vars: Option<&std::collections::HashMap<String, String>>,
) -> String {
    match env_vars {
        Some(vars) => {
            let home_dir = || dirs::home_dir().map(|p| p.to_string_lossy().into_owned());
            shellexpand::full_with_context_no_errors(s, home_dir, |var| {
                vars.get(var)
                    .map(|v| std::borrow::Cow::Borrowed(v.as_str()))
                    .or_else(|| std::env::var(var).ok().map(std::borrow::Cow::Owned))
            })
            .into_owned()
        }
        None => shellexpand::full(s)
            .map(|s| s.into_owned())
            .unwrap_or_else(|_| s.to_string()),
    }
}

#[derive(Debug, Clone, Default)]
pub struct RunnerConfig {
    pub timezone: TimezoneConfig,
    pub env_file: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub webhook: Vec<WebhookConfig>,
}

#[derive(Debug, Deserialize, Default)]
struct RunnerConfigRaw {
    timezone: Option<String>,
    env_file: Option<String>,
    env: Option<HashMap<String, String>>,
    #[serde(default)]
    webhook: Vec<WebhookConfig>,
}

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Concurrency {
    Parallel,
    Wait,
    #[default]
    Skip,
    Replace,
}

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default)]
    runner: RunnerConfigRaw,
    jobs: HashMap<String, JobConfig>,
}

#[derive(Debug, Deserialize)]
pub struct JobConfig {
    pub name: Option<String>,
    pub schedule: ScheduleConfig,
    pub build: Option<String>,
    pub build_timeout: Option<String>,
    pub run: String,
    #[serde(default = "default_timeout")]
    pub timeout: String,
    #[serde(default)]
    pub concurrency: Concurrency,
    pub retry: Option<RetryConfigRaw>,
    pub working_dir: Option<String>,
    pub enabled: Option<bool>,
    pub env_file: Option<String>,
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub webhook: Vec<WebhookConfig>,
    pub log_file: Option<String>,
    #[serde(default = "default_log_max_size")]
    pub log_max_size: String,
}

#[derive(Debug, Deserialize)]
pub struct RetryConfigRaw {
    #[serde(default)]
    pub max: u32,
    #[serde(default = "default_retry_delay")]
    pub delay: String,
    pub jitter: Option<String>,
}

fn default_retry_delay() -> String {
    "1s".to_string()
}

#[derive(Debug, Deserialize)]
pub struct ScheduleConfig {
    pub cron: String,
    pub timezone: Option<String>,
}

fn default_timeout() -> String {
    "1h".to_string()
}

fn default_log_max_size() -> String {
    "10M".to_string()
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub schedule: Cron,
    pub build: Option<String>,
    pub build_timeout: Duration,
    pub command: String,
    pub timeout: Duration,
    pub concurrency: Concurrency,
    pub retry: Option<RetryConfig>,
    pub working_dir: Option<String>,
    pub enabled: bool,
    pub timezone: Option<TimezoneConfig>,
    pub env_file: Option<String>,
    pub env: Option<HashMap<String, String>>,
    pub webhook: Vec<WebhookConfig>,
    pub log_file: Option<String>,
    pub log_max_size: u64,
}

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max: u32,
    pub delay: Duration,
    pub jitter: Option<Duration>,
}

pub fn parse_config(content: &str) -> Result<(RunnerConfig, Vec<Job>)> {
    let config: Config =
        serde_yaml::from_str(content).map_err(|e| anyhow!("Failed to parse YAML: {}", e))?;

    let timezone = match config.runner.timezone {
        None => TimezoneConfig::Utc,
        Some(s) if s == "inherit" => TimezoneConfig::Inherit,
        Some(s) => TimezoneConfig::Named(
            s.parse::<Tz>()
                .map_err(|e| anyhow!("Invalid timezone '{}': {}", s, e))?,
        ),
    };

    // Extract runner webhook for use in job defaults
    let runner_webhook = config.runner.webhook;

    let runner = RunnerConfig {
        timezone: timezone.clone(),
        env_file: config.runner.env_file,
        env: config.runner.env,
        webhook: runner_webhook.clone(),
    };

    let jobs = config
        .jobs
        .into_iter()
        .filter_map(
            |(id, job)| match parse_job(&id, job, &timezone, &runner_webhook) {
                Ok(job) => Some(job),
                Err(e) => {
                    warn!("Skipping job '{}': {}", id, e);
                    None
                }
            },
        )
        .collect();

    Ok((runner, jobs))
}

fn parse_job(
    id: &str,
    job: JobConfig,
    timezone: &TimezoneConfig,
    runner_webhook: &[WebhookConfig],
) -> Result<Job> {
    validate_job_id(id)?;

    let schedule = Cron::from_str(&job.schedule.cron)
        .map_err(|e| anyhow!("Invalid cron '{}': {}", job.schedule.cron, e))?;

    let timeout = parse_duration(&job.timeout)
        .map_err(|e| anyhow!("Invalid timeout '{}': {}", job.timeout, e))?;

    let build_timeout = job
        .build_timeout
        .map(|bt| parse_duration(&bt).map_err(|e| anyhow!("Invalid build_timeout '{}': {}", bt, e)))
        .transpose()?
        .unwrap_or(timeout);

    let name = job.name.unwrap_or_else(|| id.to_string());

    let retry = job
        .retry
        .map(|r| {
            if r.max == 0 {
                anyhow::bail!(
                    "Invalid retry.max '0': must be at least 1 (use no retry config to disable retries)"
                );
            }
            let delay = parse_duration(&r.delay)
                .map_err(|e| anyhow!("Invalid retry delay '{}': {}", r.delay, e))?;
            let jitter = r
                .jitter
                .map(|j| parse_duration(&j).map_err(|e| anyhow!("Invalid retry jitter '{}': {}", j, e)))
                .transpose()?;
            Ok::<_, anyhow::Error>(RetryConfig {
                max: r.max,
                delay,
                jitter,
            })
        })
        .transpose()?;

    let job_timezone = job
        .schedule
        .timezone
        .map(|tz| {
            if tz == "inherit" {
                Ok(TimezoneConfig::Inherit)
            } else {
                tz.parse::<Tz>()
                    .map(TimezoneConfig::Named)
                    .map_err(|e| anyhow!("Invalid timezone '{}': {}", tz, e))
            }
        })
        .transpose()?
        .or(Some(timezone.clone()));

    // Job webhooks extend runner webhooks
    let mut webhook = runner_webhook.to_vec();
    webhook.extend(job.webhook);

    let log_max_size =
        parse_size(&job.log_max_size).map_err(|e| anyhow!("Invalid log_max_size: {}", e))?;

    Ok(Job {
        id: id.to_string(),
        name,
        schedule,
        build: job.build,
        build_timeout,
        command: job.run,
        timeout,
        concurrency: job.concurrency,
        retry,
        working_dir: job.working_dir,
        enabled: job.enabled.unwrap_or(true),
        timezone: job_timezone,
        env_file: job.env_file,
        env: job.env,
        webhook,
        log_file: job.log_file,
        log_max_size,
    })
}

fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if let Some(millis) = s.strip_suffix("ms") {
        Ok(Duration::from_millis(millis.parse()?))
    } else if let Some(secs) = s.strip_suffix('s') {
        Ok(Duration::from_secs(secs.parse()?))
    } else if let Some(mins) = s.strip_suffix('m') {
        Ok(Duration::from_secs(mins.parse::<u64>()? * 60))
    } else if let Some(hours) = s.strip_suffix('h') {
        Ok(Duration::from_secs(hours.parse::<u64>()? * 3600))
    } else {
        Ok(Duration::from_secs(s.parse()?))
    }
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('G') {
        Ok(n.parse::<u64>()? * 1024 * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('M') {
        Ok(n.parse::<u64>()? * 1024 * 1024)
    } else if let Some(n) = s.strip_suffix('K') {
        Ok(n.parse::<u64>()? * 1024)
    } else {
        Ok(s.parse()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_config() {
        let yaml = r#"
jobs:
  hello:
    schedule:
      cron: "*/5 * * * *"
    run: echo hello
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "hello");
        assert_eq!(jobs[0].name, "hello");
        assert_eq!(jobs[0].command, "echo hello");
        assert_eq!(jobs[0].timeout, Duration::from_secs(3600));
    }

    #[test]
    fn parse_cron_with_day_of_week() {
        // Regression test: 5-field cron with day-of-week should parse correctly
        let yaml = r#"
jobs:
  weekly:
    schedule:
      cron: "0 7 * * 0"
    run: echo sunday
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "weekly");
    }

    #[test]
    fn parse_with_display_name() {
        let yaml = r#"
jobs:
  build:
    name: "Build Project"
    schedule:
      cron: "0 * * * *"
    run: cargo build
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].id, "build");
        assert_eq!(jobs[0].name, "Build Project");
    }

    #[test]
    fn parse_with_timeout() {
        let yaml = r#"
jobs:
  slow:
    schedule:
      cron: "0 * * * *"
    run: sleep 30
    timeout: 60s
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].timeout, Duration::from_secs(60));
    }

    #[test]
    fn parse_multiple_jobs() {
        let yaml = r#"
jobs:
  job1:
    schedule:
      cron: "*/5 * * * *"
    run: echo one
  job2:
    name: "Second Job"
    schedule:
      cron: "0 * * * *"
    run: echo two
    timeout: 30s
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn parse_duration_variants() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }

    #[test]
    fn parse_concurrency_default() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].concurrency, Concurrency::Skip);
    }

    #[test]
    fn parse_concurrency_all_variants() {
        let yaml = r#"
jobs:
  parallel_job:
    schedule:
      cron: "* * * * *"
    run: echo 1
    concurrency: parallel
  wait_job:
    schedule:
      cron: "* * * * *"
    run: echo 2
    concurrency: wait
  skip_job:
    schedule:
      cron: "* * * * *"
    run: echo 3
    concurrency: skip
  replace_job:
    schedule:
      cron: "* * * * *"
    run: echo 4
    concurrency: replace
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        let find = |id: &str| jobs.iter().find(|j| j.id == id).unwrap();

        assert_eq!(find("parallel_job").concurrency, Concurrency::Parallel);
        assert_eq!(find("wait_job").concurrency, Concurrency::Wait);
        assert_eq!(find("skip_job").concurrency, Concurrency::Skip);
        assert_eq!(find("replace_job").concurrency, Concurrency::Replace);
    }

    #[test]
    fn parse_retry_config() {
        let yaml = r#"
jobs:
  with_retry:
    schedule:
      cron: "* * * * *"
    run: echo test
    retry:
      max: 3
      delay: 2s
  no_retry:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        let find = |id: &str| jobs.iter().find(|j| j.id == id).unwrap();

        let retry = find("with_retry").retry.as_ref().unwrap();
        assert_eq!(retry.max, 3);
        assert_eq!(retry.delay, Duration::from_secs(2));

        assert!(find("no_retry").retry.is_none());
    }

    #[test]
    fn parse_retry_default_delay() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
    retry:
      max: 2
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        let retry = jobs[0].retry.as_ref().unwrap();
        assert_eq!(retry.max, 2);
        assert_eq!(retry.delay, Duration::from_secs(1)); // default delay
    }

    #[test]
    fn parse_runner_config() {
        let yaml = r#"
runner:
  timezone: Asia/Tokyo
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (runner, _) = parse_config(yaml).unwrap();
        assert_eq!(
            runner.timezone,
            TimezoneConfig::Named(chrono_tz::Asia::Tokyo)
        );
    }

    #[test]
    fn parse_runner_config_defaults() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (runner, _) = parse_config(yaml).unwrap();
        assert_eq!(runner.timezone, TimezoneConfig::Utc);
    }

    #[test]
    fn parse_timezone_inherit() {
        let yaml = r#"
runner:
  timezone: inherit
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (runner, _) = parse_config(yaml).unwrap();
        assert_eq!(runner.timezone, TimezoneConfig::Inherit);
    }

    #[test]
    fn parse_working_dir() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
    working_dir: ./scripts
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].working_dir.as_deref(), Some("./scripts"));
    }

    #[test]
    fn parse_invalid_timezone() {
        let yaml = r#"
runner:
  timezone: Invalid/Zone
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        assert!(parse_config(yaml).is_err());
    }

    #[test]
    fn parse_retry_jitter() {
        let yaml = r#"
jobs:
  with_retry_jitter:
    schedule:
      cron: "* * * * *"
    run: echo test
    retry:
      max: 3
      delay: 1s
      jitter: 500ms
  retry_no_jitter:
    schedule:
      cron: "* * * * *"
    run: echo test
    retry:
      max: 2
      delay: 1s
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        let find = |id: &str| jobs.iter().find(|j| j.id == id).unwrap();

        let retry1 = find("with_retry_jitter").retry.as_ref().unwrap();
        assert_eq!(retry1.jitter, Some(Duration::from_millis(500)));

        let retry2 = find("retry_no_jitter").retry.as_ref().unwrap();
        assert!(retry2.jitter.is_none());
    }

    #[test]
    fn parse_duration_milliseconds() {
        assert_eq!(parse_duration("500ms").unwrap(), Duration::from_millis(500));
        assert_eq!(
            parse_duration("1000ms").unwrap(),
            Duration::from_millis(1000)
        );
    }

    #[test]
    fn skip_invalid_job_id() {
        let yaml = r#"
jobs:
  "../escape":
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert!(jobs.is_empty()); // Invalid job is skipped
    }

    #[test]
    fn skip_job_id_with_slash() {
        let yaml = r#"
jobs:
  "foo/bar":
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert!(jobs.is_empty()); // Invalid job is skipped
    }

    #[test]
    fn accept_valid_job_id() {
        let yaml = r#"
jobs:
  my-job_123:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].id, "my-job_123");
    }

    #[test]
    fn skip_retry_max_zero() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
    retry:
      max: 0
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert!(jobs.is_empty()); // Invalid job is skipped
    }

    #[test]
    fn parse_enabled_false() {
        let yaml = r#"
jobs:
  test_disabled:
    schedule:
      cron: "* * * * *"
    run: echo test
    enabled: false
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].enabled, false);
    }

    #[test]
    fn parse_enabled_true() {
        let yaml = r#"
jobs:
  test_enabled:
    schedule:
      cron: "* * * * *"
    run: echo test
    enabled: true
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].enabled, true);
    }

    #[test]
    fn parse_enabled_default() {
        let yaml = r#"
jobs:
  test_default:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].enabled, true);
    }

    #[test]
    fn parse_job_with_timezone() {
        let yaml = r#"
runner:
  timezone: Asia/Tokyo
jobs:
  job1:
    schedule:
      cron: "* * * * *"
      timezone: America/New_York
    run: echo test
  job2:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (runner, jobs) = parse_config(yaml).unwrap();
        assert_eq!(
            runner.timezone,
            TimezoneConfig::Named(chrono_tz::Asia::Tokyo)
        );
        let find = |id: &str| jobs.iter().find(|j| j.id == id).unwrap();
        assert_eq!(
            find("job1").timezone,
            Some(TimezoneConfig::Named(chrono_tz::America::New_York))
        );
        assert_eq!(
            find("job2").timezone,
            Some(TimezoneConfig::Named(chrono_tz::Asia::Tokyo))
        );
    }

    #[test]
    fn parse_job_timezone_inherit() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
      timezone: inherit
    run: echo test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].timezone, Some(TimezoneConfig::Inherit));
    }

    #[test]
    fn skip_invalid_job_timezone() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
      timezone: Invalid/Zone
    run: echo test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert!(jobs.is_empty()); // Invalid job is skipped
    }

    #[test]
    fn parse_runner_env() {
        let yaml = r#"
runner:
  env:
    FOO: bar
    BAZ: qux
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (runner, _) = parse_config(yaml).unwrap();
        let env = runner.env.as_ref().unwrap();
        assert_eq!(env.get("FOO"), Some(&"bar".to_string()));
        assert_eq!(env.get("BAZ"), Some(&"qux".to_string()));
    }

    #[test]
    fn parse_runner_env_file() {
        let yaml = r#"
runner:
  env_file: .env.global
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (runner, _) = parse_config(yaml).unwrap();
        assert_eq!(runner.env_file.as_deref(), Some(".env.global"));
    }

    #[test]
    fn parse_job_env() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
    env:
      KEY1: value1
      KEY2: value2
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        let env = jobs[0].env.as_ref().unwrap();
        assert_eq!(env.get("KEY1"), Some(&"value1".to_string()));
        assert_eq!(env.get("KEY2"), Some(&"value2".to_string()));
    }

    #[test]
    fn parse_job_env_file() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
    env_file: .env.job
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].env_file.as_deref(), Some(".env.job"));
    }

    #[test]
    fn parse_full_env_config() {
        let yaml = r#"
runner:
  env_file: .env.global
  env:
    GLOBAL_VAR: global_value
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
    env_file: .env.local
    env:
      LOCAL_VAR: local_value
"#;
        let (runner, jobs) = parse_config(yaml).unwrap();
        assert_eq!(runner.env_file.as_deref(), Some(".env.global"));
        assert_eq!(
            runner.env.as_ref().unwrap().get("GLOBAL_VAR"),
            Some(&"global_value".to_string())
        );
        assert_eq!(jobs[0].env_file.as_deref(), Some(".env.local"));
        assert_eq!(
            jobs[0].env.as_ref().unwrap().get("LOCAL_VAR"),
            Some(&"local_value".to_string())
        );
    }

    #[test]
    fn parse_runner_webhook() {
        let yaml = r#"
runner:
  webhook:
    - url: https://hooks.slack.com/test
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        // Job inherits runner webhook
        assert_eq!(jobs[0].webhook.len(), 1);
        assert_eq!(jobs[0].webhook[0].to_url(None), "https://hooks.slack.com/test");
    }

    #[test]
    fn parse_job_webhook_extends_runner() {
        let yaml = r#"
runner:
  webhook:
    - url: https://hooks.slack.com/default
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
    webhook:
      - url: https://discord.com/api/webhooks/custom
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        // Job webhook extends runner webhook (both included)
        assert_eq!(jobs[0].webhook.len(), 2);
    }

    #[test]
    fn parse_no_webhook() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert!(jobs[0].webhook.is_empty());
    }

    #[test]
    fn parse_discord_webhook() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
    webhook:
      - url: https://discord.com/api/webhooks/123456/abcdef
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].webhook.len(), 1);
        assert_eq!(jobs[0].webhook[0].webhook_type, "discord");
        assert_eq!(
            jobs[0].webhook[0].to_url(None),
            "https://discord.com/api/webhooks/123456/abcdef"
        );
    }

    #[test]
    fn parse_multiple_webhooks() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
    webhook:
      - url: https://hooks.slack.com/first
      - url: https://hooks.slack.com/second
      - type: discord
        url: https://discord.com/api/webhooks/id/token
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].webhook.len(), 3);
    }

    #[test]
    fn parse_webhook_with_explicit_type() {
        let yaml = r#"
jobs:
  test:
    schedule:
      cron: "* * * * *"
    run: echo test
    webhook:
      - type: discord
        url: https://discord.com/api/webhooks/test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].webhook.len(), 1);
        assert_eq!(jobs[0].webhook[0].webhook_type, "discord");
    }

    #[test]
    fn webhook_env_var_expansion() {
        let mut env_vars = HashMap::new();
        env_vars.insert(
            "DISCORD_WEBHOOK".to_string(),
            "https://discord.com/api/webhooks/from_env".to_string(),
        );

        let webhook = WebhookConfig {
            webhook_type: "discord".to_string(),
            url: "$DISCORD_WEBHOOK".to_string(),
        };

        assert_eq!(
            webhook.to_url(Some(&env_vars)),
            "https://discord.com/api/webhooks/from_env"
        );
    }

    #[test]
    fn webhook_undefined_env_var_kept_as_is() {
        let env_vars = HashMap::new(); // empty

        let webhook = WebhookConfig {
            webhook_type: "discord".to_string(),
            url: "$UNDEFINED_VAR".to_string(),
        };

        // Undefined vars are kept as-is (caller should validate)
        assert_eq!(webhook.to_url(Some(&env_vars)), "$UNDEFINED_VAR");
    }

    #[test]
    fn parse_build_command() {
        let yaml = r#"
jobs:
  build-job:
    build: cargo build --release
    schedule:
      cron: "0 * * * *"
    run: ./target/release/app
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].build, Some("cargo build --release".to_string()));
        assert_eq!(jobs[0].command, "./target/release/app");
    }

    #[test]
    fn parse_build_timeout() {
        let yaml = r#"
jobs:
  build-job:
    build: cargo build --release
    build_timeout: 30m
    timeout: 10s
    schedule:
      cron: "0 * * * *"
    run: ./target/release/app
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert_eq!(jobs[0].build_timeout, Duration::from_secs(30 * 60));
        assert_eq!(jobs[0].timeout, Duration::from_secs(10));
    }

    #[test]
    fn parse_build_timeout_defaults_to_timeout() {
        let yaml = r#"
jobs:
  build-job:
    build: cargo build --release
    timeout: 5m
    schedule:
      cron: "0 * * * *"
    run: ./target/release/app
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        // build_timeout should default to timeout when not specified
        assert_eq!(jobs[0].build_timeout, Duration::from_secs(5 * 60));
        assert_eq!(jobs[0].timeout, Duration::from_secs(5 * 60));
    }

    #[test]
    fn parse_no_build_command() {
        let yaml = r#"
jobs:
  simple-job:
    schedule:
      cron: "0 * * * *"
    run: echo hello
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        assert!(jobs[0].build.is_none());
    }
}
