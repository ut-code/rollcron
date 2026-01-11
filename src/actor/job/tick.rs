use crate::config::{Job, RunnerConfig, TimezoneConfig};
use chrono::{Local, TimeZone, Utc};
use croner::Cron;
use tracing::info;

/// Tolerance for schedule matching (accounts for 1-second tick interval)
const SCHEDULE_TOLERANCE_MS: i64 = 1000;

pub fn is_job_due(job: &Job, runner: &RunnerConfig) -> bool {
    let tz_config = job.timezone.as_ref().unwrap_or(&runner.timezone);
    match tz_config {
        TimezoneConfig::Utc => is_schedule_due(&job.schedule, Utc),
        TimezoneConfig::Inherit => is_schedule_due(&job.schedule, Local),
        TimezoneConfig::Named(tz) => is_schedule_due(&job.schedule, *tz),
    }
}

fn is_schedule_due<Z: TimeZone>(schedule: &Cron, tz: Z) -> bool
where
    Z::Offset: std::fmt::Display,
{
    let now = Utc::now().with_timezone(&tz);
    if let Some(next) = schedule.find_next_occurrence(&now, false).ok() {
        let until_next = (next.clone() - now.clone()).num_milliseconds();
        let is_due = until_next <= SCHEDULE_TOLERANCE_MS && until_next >= -SCHEDULE_TOLERANCE_MS;
        if is_due {
            info!(
                target: "rollcron::job",
                now = %now,
                next = %next,
                until_next_ms = until_next,
                "Schedule matched"
            );
        }
        is_due
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Asia::Tokyo;
    use std::str::FromStr;

    #[test]
    fn debug_timezone_schedule() {
        let schedule = Cron::from_str("0 8 * * *").unwrap();

        let now_utc = Utc::now();
        let now_tokyo = now_utc.with_timezone(&Tokyo);

        println!("Current UTC: {}", now_utc);
        println!("Current Tokyo: {}", now_tokyo);

        let next_tokyo = schedule.find_next_occurrence(&now_tokyo, false).unwrap();
        println!("Next scheduled (Tokyo): {}", next_tokyo);

        let next_utc = schedule.find_next_occurrence(&now_utc, false).unwrap();
        println!("Next scheduled (UTC): {}", next_utc);

        println!("Tokyo next in UTC: {}", next_tokyo.with_timezone(&Utc));
    }
}
