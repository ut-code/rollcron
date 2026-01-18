use crate::config::{Job, RunnerConfig, TimezoneConfig};
use chrono::{DateTime, Local, TimeZone, Utc};
use chrono_tz::Tz;
use croner::Cron;

/// Returns the next scheduled time for a job, or None if no future occurrence.
pub fn next_occurrence(job: &Job, runner: &RunnerConfig) -> Option<DateTime<Utc>> {
    next_occurrence_from(job, runner, Utc::now())
}

/// Pure function: returns next scheduled time given a reference time.
pub fn next_occurrence_from(
    job: &Job,
    runner: &RunnerConfig,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>> {
    let tz_config = job.timezone.as_ref().unwrap_or(&runner.timezone);
    match tz_config {
        TimezoneConfig::Utc => find_next_from(&job.schedule, Utc, now),
        TimezoneConfig::Inherit => find_next_from(&job.schedule, Local, now),
        TimezoneConfig::Named(tz) => find_next_from(&job.schedule, *tz, now),
    }
}

/// Pure function: calculates next occurrence for a cron schedule in a given timezone.
pub fn schedule_next_from(schedule: &Cron, tz: Tz, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    find_next_from(schedule, tz, now)
}

fn find_next_from<Z: TimeZone>(
    schedule: &Cron,
    tz: Z,
    now: DateTime<Utc>,
) -> Option<DateTime<Utc>>
where
    Z::Offset: std::fmt::Display,
{
    let now_in_tz = now.with_timezone(&tz);
    schedule
        .find_next_occurrence(&now_in_tz, false)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_config;
    use chrono::TimeZone;
    use chrono_tz::{America::New_York, Asia::Tokyo, Europe::London, UTC};
    use std::str::FromStr;

    /// Helper to parse schedule (cron or English phrase)
    fn parse_schedule(expr: &str) -> Cron {
        Cron::from_str(expr).unwrap_or_else(|_| {
            let converted = english_to_cron::str_cron_syntax(expr).unwrap();
            Cron::from_str(&converted).unwrap()
        })
    }

    // ============================================================
    // Basic cron schedule tests
    // ============================================================

    #[test]
    fn every_minute_returns_next_minute() {
        let schedule = parse_schedule("* * * * *");
        // 2025-01-15 10:30:00 UTC
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 30, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // Should be 10:31:00
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 10, 31, 0).unwrap());
    }

    #[test]
    fn every_5_minutes_cron() {
        let schedule = parse_schedule("*/5 * * * *");
        // 2025-01-15 10:32:00 UTC
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 32, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // Should be 10:35:00
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 10, 35, 0).unwrap());
    }

    #[test]
    fn daily_at_8am() {
        let schedule = parse_schedule("0 8 * * *");
        // 2025-01-15 10:00:00 UTC (already past 8am)
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // Should be next day 8:00:00
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 16, 8, 0, 0).unwrap());
    }

    #[test]
    fn before_scheduled_time_same_day() {
        let schedule = parse_schedule("0 8 * * *");
        // 2025-01-15 05:00:00 UTC (before 8am)
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 5, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // Should be same day 8:00:00
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 8, 0, 0).unwrap());
    }

    // ============================================================
    // English phrase schedule tests
    // ============================================================

    #[test]
    fn english_every_5_minutes() {
        let schedule = parse_schedule("every 5 minutes");
        // 2025-01-15 10:32:00 UTC
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 32, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // Should be 10:35:00
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 10, 35, 0).unwrap());
    }

    #[test]
    fn english_every_minute() {
        let schedule = parse_schedule("every minute");
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 30, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 10, 31, 0).unwrap());
    }

    #[test]
    fn english_every_hour() {
        let schedule = parse_schedule("every hour");
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 30, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 11, 0, 0).unwrap());
    }

    #[test]
    fn english_every_day_at_4pm() {
        let schedule = parse_schedule("every day at 4:00 pm");
        // 2025-01-15 10:00:00 UTC (before 4pm)
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // Should be same day 16:00:00
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 16, 0, 0).unwrap());
    }

    #[test]
    fn english_every_day_at_4pm_after() {
        let schedule = parse_schedule("every day at 4:00 pm");
        // 2025-01-15 18:00:00 UTC (after 4pm)
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 18, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // Should be next day 16:00:00
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 16, 16, 0, 0).unwrap());
    }

    #[test]
    fn english_at_10am() {
        let schedule = parse_schedule("at 10:00 am");
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 8, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap());
    }

    #[test]
    fn english_12am_is_midnight() {
        let schedule = parse_schedule("every day at 12:00 am");
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // 12 am = midnight = 00:00, so next occurrence is next day
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 16, 0, 0, 0).unwrap());
    }

    #[test]
    fn english_12pm_is_noon() {
        let schedule = parse_schedule("every day at 12:00 pm");
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // 12 pm = noon = 12:00
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap());
    }

    #[test]
    fn english_7pm_every_thursday() {
        let schedule = parse_schedule("7pm every Thursday");
        // 2025-01-15 is Wednesday
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // Next Thursday is 2025-01-16 at 19:00
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 16, 19, 0, 0).unwrap());
    }

    #[test]
    fn english_sunday_at_noon() {
        let schedule = parse_schedule("Sunday at 12:00");
        // 2025-01-15 is Wednesday
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // Next Sunday is 2025-01-19 at 12:00
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 19, 12, 0, 0).unwrap());
    }

    // ============================================================
    // Timezone support tests
    // ============================================================

    #[test]
    fn timezone_tokyo_8am_from_utc_midnight() {
        let schedule = parse_schedule("0 8 * * *");
        // 2025-01-15 00:00:00 UTC = 2025-01-15 09:00:00 Tokyo (past 8am Tokyo)
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 0, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, Tokyo, now).unwrap();
        // Next 8am Tokyo = 2025-01-15 23:00:00 UTC (8am - 9h offset)
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 23, 0, 0).unwrap());
    }

    #[test]
    fn timezone_tokyo_8am_before_in_tokyo() {
        let schedule = parse_schedule("0 8 * * *");
        // 2025-01-14 20:00:00 UTC = 2025-01-15 05:00:00 Tokyo (before 8am Tokyo)
        let now = Utc.with_ymd_and_hms(2025, 1, 14, 20, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, Tokyo, now).unwrap();
        // Same day 8am Tokyo = 2025-01-14 23:00:00 UTC
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 14, 23, 0, 0).unwrap());
    }

    #[test]
    fn timezone_new_york_9am() {
        let schedule = parse_schedule("0 9 * * *");
        // 2025-01-15 10:00:00 UTC = 2025-01-15 05:00:00 New York (EST, UTC-5)
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, New_York, now).unwrap();
        // 9am New York = 14:00:00 UTC
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 14, 0, 0).unwrap());
    }

    #[test]
    fn timezone_london_midnight() {
        let schedule = parse_schedule("0 0 * * *");
        // 2025-01-15 01:00:00 UTC = 2025-01-15 01:00:00 London (GMT, same as UTC in winter)
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 1, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, London, now).unwrap();
        // Next midnight London = 2025-01-16 00:00:00 UTC
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 16, 0, 0, 0).unwrap());
    }

    #[test]
    fn different_timezones_same_cron_different_utc() {
        let schedule = parse_schedule("0 12 * * *"); // noon
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 0, 0, 0).unwrap();

        let next_utc = schedule_next_from(&schedule, UTC, now).unwrap();
        let next_tokyo = schedule_next_from(&schedule, Tokyo, now).unwrap();
        let next_ny = schedule_next_from(&schedule, New_York, now).unwrap();

        // Noon UTC = 12:00 UTC
        assert_eq!(next_utc, Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap());
        // Noon Tokyo = 03:00 UTC (12:00 - 9h)
        assert_eq!(next_tokyo, Utc.with_ymd_and_hms(2025, 1, 15, 3, 0, 0).unwrap());
        // Noon New York = 17:00 UTC (12:00 + 5h)
        assert_eq!(next_ny, Utc.with_ymd_and_hms(2025, 1, 15, 17, 0, 0).unwrap());
    }

    // ============================================================
    // Integration tests with Job config
    // ============================================================

    #[test]
    fn job_with_utc_timezone() {
        let yaml = r#"
jobs:
  test:
    schedule: "0 12 * * *"
    run: echo test
"#;
        let (runner, jobs) = parse_config(yaml).unwrap();
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap();
        let next = next_occurrence_from(&jobs[0], &runner, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap());
    }

    #[test]
    fn job_with_named_timezone() {
        let yaml = r#"
runner:
  timezone: Asia/Tokyo
jobs:
  test:
    schedule: "0 8 * * *"
    run: echo test
"#;
        let (runner, jobs) = parse_config(yaml).unwrap();
        // 2025-01-15 00:00:00 UTC = 09:00 Tokyo (past 8am)
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 0, 0, 0).unwrap();
        let next = next_occurrence_from(&jobs[0], &runner, now).unwrap();
        // Next 8am Tokyo = 23:00 UTC
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 23, 0, 0).unwrap());
    }

    #[test]
    fn job_timezone_overrides_runner() {
        let yaml = r#"
runner:
  timezone: Asia/Tokyo
jobs:
  test:
    schedule:
      cron: "0 12 * * *"
      timezone: America/New_York
    run: echo test
"#;
        let (runner, jobs) = parse_config(yaml).unwrap();
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap();
        let next = next_occurrence_from(&jobs[0], &runner, now).unwrap();
        // Noon New York = 17:00 UTC (EST)
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 17, 0, 0).unwrap());
    }

    #[test]
    fn job_with_english_schedule_and_timezone() {
        let yaml = r#"
runner:
  timezone: Asia/Tokyo
jobs:
  test:
    schedule: "every day at 4:00 pm"
    run: echo test
"#;
        let (runner, jobs) = parse_config(yaml).unwrap();
        // 2025-01-15 00:00:00 UTC = 09:00 Tokyo
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 0, 0, 0).unwrap();
        let next = next_occurrence_from(&jobs[0], &runner, now).unwrap();
        // 4pm Tokyo = 07:00 UTC
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 7, 0, 0).unwrap());
    }

    #[test]
    fn job_every_5_minutes_consistency() {
        let yaml = r#"
jobs:
  test:
    schedule: "every 5 minutes"
    run: echo test
"#;
        let (runner, jobs) = parse_config(yaml).unwrap();
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 32, 0).unwrap();
        let next = next_occurrence_from(&jobs[0], &runner, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 15, 10, 35, 0).unwrap());
    }

    // ============================================================
    // Edge cases
    // ============================================================

    #[test]
    fn exact_schedule_time_returns_next() {
        let schedule = parse_schedule("0 12 * * *");
        // Exactly at noon
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 12, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // Should return next day's noon (not the same time)
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 16, 12, 0, 0).unwrap());
    }

    #[test]
    fn year_boundary() {
        let schedule = parse_schedule("0 0 1 1 *"); // Jan 1st midnight
        let now = Utc.with_ymd_and_hms(2025, 12, 31, 23, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn month_boundary() {
        let schedule = parse_schedule("0 0 1 * *"); // 1st of every month
        let now = Utc.with_ymd_and_hms(2025, 1, 31, 12, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 2, 1, 0, 0, 0).unwrap());
    }

    #[test]
    fn day_of_week_sunday_0() {
        let schedule = parse_schedule("0 7 * * 0"); // Sunday 7am
        // 2025-01-15 is Wednesday
        let now = Utc.with_ymd_and_hms(2025, 1, 15, 10, 0, 0).unwrap();
        let next = schedule_next_from(&schedule, UTC, now).unwrap();
        // Next Sunday is 2025-01-19
        assert_eq!(next, Utc.with_ymd_and_hms(2025, 1, 19, 7, 0, 0).unwrap());
    }
}
