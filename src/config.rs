use anyhow::{anyhow, Result};
use cron::Schedule;
use serde::Deserialize;
use std::collections::HashMap;
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, Deserialize)]
pub struct Config {
    pub jobs: HashMap<String, JobConfig>,
}

#[derive(Debug, Deserialize)]
pub struct JobConfig {
    pub name: Option<String>,
    pub schedule: ScheduleConfig,
    pub run: String,
    #[serde(default = "default_timeout")]
    pub timeout: String,
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
}

pub fn parse_config(content: &str) -> Result<Vec<Job>> {
    let config: Config = serde_yaml::from_str(content)
        .map_err(|e| anyhow!("Failed to parse YAML: {}", e))?;

    config
        .jobs
        .into_iter()
        .map(|(id, job)| {
            let schedule_str = format!("{} *", job.schedule.cron);
            let schedule = Schedule::from_str(&schedule_str)
                .map_err(|e| anyhow!("Invalid cron '{}' in job '{}': {}", job.schedule.cron, id, e))?;

            let timeout = parse_duration(&job.timeout)
                .map_err(|e| anyhow!("Invalid timeout '{}' in job '{}': {}", job.timeout, id, e))?;

            let name = job.name.unwrap_or_else(|| id.clone());

            Ok(Job {
                id,
                name,
                schedule,
                command: job.run,
                timeout,
            })
        })
        .collect()
}

fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if let Some(secs) = s.strip_suffix('s') {
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
        let jobs = parse_config(yaml).unwrap();
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
        let jobs = parse_config(yaml).unwrap();
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
        let jobs = parse_config(yaml).unwrap();
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
        let jobs = parse_config(yaml).unwrap();
        assert_eq!(jobs.len(), 2);
    }

    #[test]
    fn parse_duration_variants() {
        assert_eq!(parse_duration("10s").unwrap(), Duration::from_secs(10));
        assert_eq!(parse_duration("5m").unwrap(), Duration::from_secs(300));
        assert_eq!(parse_duration("1h").unwrap(), Duration::from_secs(3600));
    }
}
