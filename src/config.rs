use anyhow::{anyhow, Result};
use chrono_tz::Tz;
use cron::Schedule;
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, Clone, Default)]
pub struct RunnerConfig {
    pub timezone: Option<Tz>,
}

#[derive(Debug, Deserialize, Default)]
struct RunnerConfigRaw {
    timezone: Option<String>,
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
    pub run: String,
    #[serde(default = "default_timeout")]
    pub timeout: String,
    #[serde(default)]
    pub concurrency: Concurrency,
    pub retry: Option<RetryConfigRaw>,
    pub working_dir: Option<String>,
    pub jitter: Option<String>,
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
}

fn default_timeout() -> String {
    "10s".to_string()
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub schedule: Schedule,
    pub command: String,
    pub timeout: Duration,
    pub concurrency: Concurrency,
    pub retry: Option<RetryConfig>,
    pub working_dir: Option<String>,
    pub jitter: Option<Duration>,
}

#[derive(Debug, Clone)]
pub struct RetryConfig {
    pub max: u32,
    pub delay: Duration,
    pub jitter: Option<Duration>,
}

pub fn parse_config(content: &str) -> Result<(RunnerConfig, Vec<Job>)> {
    let config: Config = serde_yaml::from_str(content)
        .map_err(|e| anyhow!("Failed to parse YAML: {}", e))?;

    let timezone = config
        .runner
        .timezone
        .map(|s| s.parse::<Tz>())
        .transpose()
        .map_err(|e| anyhow!("Invalid timezone: {}", e))?;

    let runner = RunnerConfig { timezone };

    let jobs = config
        .jobs
        .into_iter()
        .map(|(id, job)| {
            let schedule_str = format!("{} *", job.schedule.cron);
            let schedule = Schedule::from_str(&schedule_str)
                .map_err(|e| anyhow!("Invalid cron '{}' in job '{}': {}", job.schedule.cron, id, e))?;

            let timeout = parse_duration(&job.timeout)
                .map_err(|e| anyhow!("Invalid timeout '{}' in job '{}': {}", job.timeout, id, e))?;

            let name = job.name.unwrap_or_else(|| id.clone());

            let retry = job
                .retry
                .map(|r| {
                    let delay = parse_duration(&r.delay)
                        .map_err(|e| anyhow!("Invalid retry delay '{}' in job '{}': {}", r.delay, id, e))?;
                    let jitter = r.jitter
                        .map(|j| parse_duration(&j)
                            .map_err(|e| anyhow!("Invalid retry jitter '{}' in job '{}': {}", j, id, e)))
                        .transpose()?;
                    Ok::<_, anyhow::Error>(RetryConfig { max: r.max, delay, jitter })
                })
                .transpose()?;

            let jitter = job
                .jitter
                .map(|j| parse_duration(&j)
                    .map_err(|e| anyhow!("Invalid jitter '{}' in job '{}': {}", j, id, e)))
                .transpose()?;

            Ok(Job {
                id,
                name,
                schedule,
                command: job.run,
                timeout,
                concurrency: job.concurrency,
                retry,
                working_dir: job.working_dir,
                jitter,
            })
        })
        .collect::<Result<Vec<_>>>()?;

    Ok((runner, jobs))
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
        assert_eq!(jobs[0].timeout, Duration::from_secs(10));
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
        assert_eq!(runner.timezone, Some(chrono_tz::Asia::Tokyo));
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
        assert!(runner.timezone.is_none());
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
    fn parse_jitter() {
        let yaml = r#"
jobs:
  with_jitter:
    schedule:
      cron: "* * * * *"
    run: echo test
    jitter: 30s
  no_jitter:
    schedule:
      cron: "* * * * *"
    run: echo test
"#;
        let (_, jobs) = parse_config(yaml).unwrap();
        let find = |id: &str| jobs.iter().find(|j| j.id == id).unwrap();

        assert_eq!(find("with_jitter").jitter, Some(Duration::from_secs(30)));
        assert!(find("no_jitter").jitter.is_none());
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
        assert_eq!(parse_duration("1000ms").unwrap(), Duration::from_millis(1000));
    }
}
