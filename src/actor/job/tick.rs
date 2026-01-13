use crate::config::{Job, RunnerConfig, TimezoneConfig};
use chrono::{DateTime, Local, TimeZone, Utc};
use croner::Cron;

/// Returns the next scheduled time for a job, or None if no future occurrence.
pub fn next_occurrence(job: &Job, runner: &RunnerConfig) -> Option<DateTime<Utc>> {
    let tz_config = job.timezone.as_ref().unwrap_or(&runner.timezone);
    match tz_config {
        TimezoneConfig::Utc => find_next(&job.schedule, Utc),
        TimezoneConfig::Inherit => find_next(&job.schedule, Local),
        TimezoneConfig::Named(tz) => find_next(&job.schedule, *tz),
    }
}

fn find_next<Z: TimeZone>(schedule: &Cron, tz: Z) -> Option<DateTime<Utc>>
where
    Z::Offset: std::fmt::Display,
{
    let now = Utc::now().with_timezone(&tz);
    schedule
        .find_next_occurrence(&now, false)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
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
