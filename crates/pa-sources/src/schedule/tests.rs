use std::time::Duration;

use super::{parse_duration, parse_instant, Cron, DurationUnits, ParsedSchedule, Schedule};

const MINUTE: u64 = 60_000;
const DAY: u64 = 24 * 60 * MINUTE;
/// 2026-11-25T09:30:00Z, a Wednesday.
const NOW: u64 = 1_795_599_000_000;
const TEN_SECONDS: Duration = Duration::from_secs(10);

fn cron(text: &str) -> Schedule {
    Schedule::Cron(Cron::parse(text).unwrap())
}

#[test]
fn instants_read_iso_times_with_or_without_seconds_and_offsets() {
    assert_eq!(parse_instant("2026-11-25T09:30:00Z"), Some(NOW));
    assert_eq!(parse_instant("2026-11-25t09:30:00z"), Some(NOW));
    assert_eq!(
        parse_instant("2026-11-25T10:30:00.250+01:00"),
        Some(NOW + 250)
    );
    assert_eq!(parse_instant("2026-11-25T04:30-05"), Some(NOW));
    assert_eq!(parse_instant("2026-11-25T09:30"), Some(NOW));
    for refused in [
        "2026-02-29T00:00:00Z",
        "2026-11-25 09:30:00",
        "1969-12-31T23:59:59Z",
        "2026-11-25T09:30:00+0100",
        "2026-11-25T",
    ] {
        assert_eq!(parse_instant(refused), None, "{refused:?}");
    }
}

#[test]
fn durations_add_their_parts_in_the_units_allowed() {
    let with_seconds = |text| parse_duration(text, DurationUnits::WithSeconds);
    assert_eq!(with_seconds("1h30m"), Some(90 * MINUTE));
    assert_eq!(with_seconds("1 hour 30 minutes"), Some(90 * MINUTE));
    assert_eq!(with_seconds("30 sec"), Some(30_000));
    assert_eq!(parse_duration("30s", DurationUnits::FromMinutes), None);
    for refused in ["", "10", "5x", "m", "1H", "10m5", "99999999999999999999d"] {
        assert_eq!(with_seconds(refused), None, "{refused:?}");
    }
}

#[test]
fn a_sentence_reads_in_every_at_and_cron_forms() {
    let parse = |text| Schedule::parse(text, NOW, TEN_SECONDS);
    assert_eq!(
        parse("'In 2 Hours'"),
        Ok(ParsedSchedule {
            schedule: Schedule::At {
                unix_ms: NOW + 120 * MINUTE
            },
            expression: "In 2 Hours".to_string(),
        })
    );
    assert_eq!(
        parse("each 30s"),
        Ok(ParsedSchedule {
            schedule: Schedule::Every {
                interval_ms: 30_000
            },
            expression: "each 30s".to_string(),
        })
    );
    assert_eq!(
        parse("@hourly"),
        Ok(ParsedSchedule {
            schedule: cron("0 * * * *"),
            expression: "0 * * * *".to_string(),
        })
    );
    assert_eq!(
        parse("every 5s").unwrap_err().to_string(),
        "Recurring interval must be at least 10 seconds"
    );
    assert!(parse("at 2026-11-25T09:30:00Z").is_err(), "not after now");
    assert!(parse("in 30s").is_err(), "`in` has no seconds");
    assert!(parse("").is_err());
}

#[test]
fn cron_finds_the_next_and_latest_matching_minute() {
    let weekday_mornings = cron("*/15 9-10 * * 1-5");
    // Wednesday 09:30 → 09:45; Friday 10:50 → Monday 09:00.
    assert_eq!(weekday_mornings.next_after(NOW), Some(NOW + 15 * MINUTE));
    let friday_late = NOW + 2 * DAY + 80 * MINUTE;
    let monday_nine = NOW + 5 * DAY - 30 * MINUTE;
    assert_eq!(weekday_mornings.next_after(friday_late), Some(monday_nine));
    // Missed from Wednesday 09:45 until Friday 10:50: the latest is Friday 10:45.
    assert_eq!(
        weekday_mornings.latest_due(NOW + 15 * MINUTE, friday_late),
        friday_late - 5 * MINUTE
    );
    // A weekday of 7 is Sunday: 2026-11-29.
    assert_eq!(
        cron("0 0 * * 7").next_after(NOW),
        Some(NOW + 4 * DAY - 570 * MINUTE)
    );
    // Friday the 13th more than a year away is still found: 2027-08-13, then 2028-10-13.
    let friday_13th = cron("0 0 13 * 5");
    let august = friday_13th.next_after(NOW).unwrap();
    assert_eq!(parse_instant("2027-08-13T00:00:00Z"), Some(august));
    assert_eq!(
        friday_13th.next_after(august),
        parse_instant("2028-10-13T00:00:00Z")
    );
    for refused in [
        "0 0 30 2 *",
        "60 * * * *",
        "*/0 * * * *",
        "*/60 * * * *",
        "* * * *",
    ] {
        assert!(Cron::parse(refused).is_err(), "{refused:?}");
    }
    assert_eq!(Cron::parse("@daily"), Cron::parse("0 0 * * *"));
}
