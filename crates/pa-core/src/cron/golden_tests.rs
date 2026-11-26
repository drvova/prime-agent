//! The scheduled-jobs store's schedule answers, pinned: for each expression
//! form on fixed clocks, the parsed schedule, its first run and the runs
//! after it. Recorded from the store's own evaluator before it moved into
//! `pa_sources::schedule`; the shared evaluator must reproduce every row.

use super::{next_run_at_for_schedule, parse_agent_cron_schedule, store::iso_from_millis};

/// 2023-11-14T22:13:20Z, a Tuesday, between minutes.
const TUESDAY: u64 = 1_700_000_000_000;
/// 2026-11-25T09:30:00Z, a Wednesday, on a minute.
const WEDNESDAY: u64 = 1_795_599_000_000;
/// 2024-02-28T23:59:30.500Z, the day before a leap day.
const LEAP_EVE: u64 = 1_709_164_770_500;
/// 2026-12-31T23:59:59.999Z, the last millisecond of a year.
const YEAR_END: u64 = 1_798_761_599_999;

const CLOCKS: [u64; 4] = [TUESDAY, WEDNESDAY, LEAP_EVE, YEAR_END];

const EXPRESSIONS: &[&str] = &[
    "in 10m",
    "IN 2H",
    "in 1 hour",
    "in 3days",
    "in 0m",
    "'in 10m'",
    "in 30s",
    "in 10",
    "every 10s",
    "each 30s",
    "\"every 15 m\"",
    "every 10 sec",
    "Every 5 Minutes",
    "every 2 hours",
    "every 1d",
    "every 5s",
    "every 1h30m",
    "at 2100-01-01T00:00:00Z",
    "AT 2100-01-01T00:00:00Z",
    "at 2027-03-01T10:30:00.250+01:00",
    "at 2027-03-01T09:30:00-05:00",
    "at 2027-03-01T09:30Z",
    "at 2027-03-01T09:30",
    "at 1999-01-01T00:00:00Z",
    "at tomorrow",
    "at 2027-02-30T00:00:00Z",
    "@hourly",
    "@daily",
    "@weekly",
    "@monthly",
    "0 12 * * *",
    "*/15 9-17 * * 1-5",
    "0 0 * * 7",
    "0 0 * * 1-7",
    "0 0 13 * 5",
    "0 0 31 * *",
    "30 2 1,15 * *",
    "5-10/2 * * * *",
    "59 23 31 12 *",
    "0 0 29 2 *",
    "@yearly",
    "0 0 30 2 *",
    "*/60 * * * *",
    "* * * *",
    "60 * * * *",
    "a * * * *",
    "*/0 * * * *",
    "",
];

/// One expression's answer on one clock: refused, or the schedule with its
/// first run, the two runs after it, and the run after a claim one hour and
/// a bit past the clock.
fn answer(expression: &str, now: u64) -> String {
    let Ok((schedule, first)) = parse_agent_cron_schedule(expression, now) else {
        return "refused".to_string();
    };
    let mut runs = vec![iso_from_millis(first)];
    let mut after = first;
    for _ in 0..2 {
        match next_run_at_for_schedule(&schedule, after) {
            Ok(Some(next)) => {
                runs.push(iso_from_millis(next));
                after = next;
            }
            Ok(None) => runs.push("-".to_string()),
            Err(_) => runs.push("error".to_string()),
        }
    }
    let claimed = match next_run_at_for_schedule(&schedule, now + 3_601_234) {
        Ok(Some(next)) => iso_from_millis(next),
        Ok(None) => "-".to_string(),
        Err(_) => "error".to_string(),
    };
    format!(
        "{:?} {:?} {:?} runs={} claimed={claimed}",
        schedule.kind,
        schedule.expression,
        schedule.interval_ms,
        runs.join(",")
    )
}

#[test]
fn the_store_answers_every_form_as_recorded() {
    let answers: Vec<(&str, u64, String)> = EXPRESSIONS
        .iter()
        .flat_map(|expression| CLOCKS.map(|now| (*expression, now, answer(expression, now))))
        .collect();
    let golden: Vec<(&str, u64, String)> = GOLDEN
        .iter()
        .map(|(expression, now, answer)| (*expression, *now, (*answer).to_string()))
        .collect();
    assert_eq!(answers, golden);
}

/// The answers, recorded from the store's evaluator before the move. A row
/// the shared evaluator answers differently says what the store answered.
#[rustfmt::skip]
const GOLDEN: &[(&str, u64, &str)] = &[
    ("in 10m", TUESDAY, "Once \"in 10m\" None runs=2023-11-14T22:23:20.000Z,-,- claimed=-"),
    ("in 10m", WEDNESDAY, "Once \"in 10m\" None runs=2026-11-25T09:40:00.000Z,-,- claimed=-"),
    ("in 10m", LEAP_EVE, "Once \"in 10m\" None runs=2024-02-29T00:09:30.500Z,-,- claimed=-"),
    ("in 10m", YEAR_END, "Once \"in 10m\" None runs=2027-01-01T00:09:59.999Z,-,- claimed=-"),
    ("IN 2H", TUESDAY, "Once \"IN 2H\" None runs=2023-11-15T00:13:20.000Z,-,- claimed=-"),
    ("IN 2H", WEDNESDAY, "Once \"IN 2H\" None runs=2026-11-25T11:30:00.000Z,-,- claimed=-"),
    ("IN 2H", LEAP_EVE, "Once \"IN 2H\" None runs=2024-02-29T01:59:30.500Z,-,- claimed=-"),
    ("IN 2H", YEAR_END, "Once \"IN 2H\" None runs=2027-01-01T01:59:59.999Z,-,- claimed=-"),
    ("in 1 hour", TUESDAY, "Once \"in 1 hour\" None runs=2023-11-14T23:13:20.000Z,-,- claimed=-"),
    ("in 1 hour", WEDNESDAY, "Once \"in 1 hour\" None runs=2026-11-25T10:30:00.000Z,-,- claimed=-"),
    ("in 1 hour", LEAP_EVE, "Once \"in 1 hour\" None runs=2024-02-29T00:59:30.500Z,-,- claimed=-"),
    ("in 1 hour", YEAR_END, "Once \"in 1 hour\" None runs=2027-01-01T00:59:59.999Z,-,- claimed=-"),
    ("in 3days", TUESDAY, "Once \"in 3days\" None runs=2023-11-17T22:13:20.000Z,-,- claimed=-"),
    ("in 3days", WEDNESDAY, "Once \"in 3days\" None runs=2026-11-28T09:30:00.000Z,-,- claimed=-"),
    ("in 3days", LEAP_EVE, "Once \"in 3days\" None runs=2024-03-02T23:59:30.500Z,-,- claimed=-"),
    ("in 3days", YEAR_END, "Once \"in 3days\" None runs=2027-01-03T23:59:59.999Z,-,- claimed=-"),
    ("in 0m", TUESDAY, "Once \"in 0m\" None runs=2023-11-14T22:13:20.000Z,-,- claimed=-"),
    ("in 0m", WEDNESDAY, "Once \"in 0m\" None runs=2026-11-25T09:30:00.000Z,-,- claimed=-"),
    ("in 0m", LEAP_EVE, "Once \"in 0m\" None runs=2024-02-28T23:59:30.500Z,-,- claimed=-"),
    ("in 0m", YEAR_END, "Once \"in 0m\" None runs=2026-12-31T23:59:59.999Z,-,- claimed=-"),
    ("'in 10m'", TUESDAY, "Once \"in 10m\" None runs=2023-11-14T22:23:20.000Z,-,- claimed=-"),
    ("'in 10m'", WEDNESDAY, "Once \"in 10m\" None runs=2026-11-25T09:40:00.000Z,-,- claimed=-"),
    ("'in 10m'", LEAP_EVE, "Once \"in 10m\" None runs=2024-02-29T00:09:30.500Z,-,- claimed=-"),
    ("'in 10m'", YEAR_END, "Once \"in 10m\" None runs=2027-01-01T00:09:59.999Z,-,- claimed=-"),
    ("in 30s", TUESDAY, "refused"),
    ("in 30s", WEDNESDAY, "refused"),
    ("in 30s", LEAP_EVE, "refused"),
    ("in 30s", YEAR_END, "refused"),
    ("in 10", TUESDAY, "refused"),
    ("in 10", WEDNESDAY, "refused"),
    ("in 10", LEAP_EVE, "refused"),
    ("in 10", YEAR_END, "refused"),
    ("every 10s", TUESDAY, "Interval \"every 10s\" Some(10000) runs=2023-11-14T22:13:30.000Z,2023-11-14T22:13:40.000Z,2023-11-14T22:13:50.000Z claimed=2023-11-14T23:13:31.234Z"),
    ("every 10s", WEDNESDAY, "Interval \"every 10s\" Some(10000) runs=2026-11-25T09:30:10.000Z,2026-11-25T09:30:20.000Z,2026-11-25T09:30:30.000Z claimed=2026-11-25T10:30:11.234Z"),
    ("every 10s", LEAP_EVE, "Interval \"every 10s\" Some(10000) runs=2024-02-28T23:59:40.500Z,2024-02-28T23:59:50.500Z,2024-02-29T00:00:00.500Z claimed=2024-02-29T00:59:41.734Z"),
    ("every 10s", YEAR_END, "Interval \"every 10s\" Some(10000) runs=2027-01-01T00:00:09.999Z,2027-01-01T00:00:19.999Z,2027-01-01T00:00:29.999Z claimed=2027-01-01T01:00:11.233Z"),
    ("each 30s", TUESDAY, "Interval \"each 30s\" Some(30000) runs=2023-11-14T22:13:50.000Z,2023-11-14T22:14:20.000Z,2023-11-14T22:14:50.000Z claimed=2023-11-14T23:13:51.234Z"),
    ("each 30s", WEDNESDAY, "Interval \"each 30s\" Some(30000) runs=2026-11-25T09:30:30.000Z,2026-11-25T09:31:00.000Z,2026-11-25T09:31:30.000Z claimed=2026-11-25T10:30:31.234Z"),
    ("each 30s", LEAP_EVE, "Interval \"each 30s\" Some(30000) runs=2024-02-29T00:00:00.500Z,2024-02-29T00:00:30.500Z,2024-02-29T00:01:00.500Z claimed=2024-02-29T01:00:01.734Z"),
    ("each 30s", YEAR_END, "Interval \"each 30s\" Some(30000) runs=2027-01-01T00:00:29.999Z,2027-01-01T00:00:59.999Z,2027-01-01T00:01:29.999Z claimed=2027-01-01T01:00:31.233Z"),
    ("\"every 15 m\"", TUESDAY, "Interval \"every 15 m\" Some(900000) runs=2023-11-14T22:28:20.000Z,2023-11-14T22:43:20.000Z,2023-11-14T22:58:20.000Z claimed=2023-11-14T23:28:21.234Z"),
    ("\"every 15 m\"", WEDNESDAY, "Interval \"every 15 m\" Some(900000) runs=2026-11-25T09:45:00.000Z,2026-11-25T10:00:00.000Z,2026-11-25T10:15:00.000Z claimed=2026-11-25T10:45:01.234Z"),
    ("\"every 15 m\"", LEAP_EVE, "Interval \"every 15 m\" Some(900000) runs=2024-02-29T00:14:30.500Z,2024-02-29T00:29:30.500Z,2024-02-29T00:44:30.500Z claimed=2024-02-29T01:14:31.734Z"),
    ("\"every 15 m\"", YEAR_END, "Interval \"every 15 m\" Some(900000) runs=2027-01-01T00:14:59.999Z,2027-01-01T00:29:59.999Z,2027-01-01T00:44:59.999Z claimed=2027-01-01T01:15:01.233Z"),
    ("every 10 sec", TUESDAY, "Interval \"every 10 sec\" Some(10000) runs=2023-11-14T22:13:30.000Z,2023-11-14T22:13:40.000Z,2023-11-14T22:13:50.000Z claimed=2023-11-14T23:13:31.234Z"),
    ("every 10 sec", WEDNESDAY, "Interval \"every 10 sec\" Some(10000) runs=2026-11-25T09:30:10.000Z,2026-11-25T09:30:20.000Z,2026-11-25T09:30:30.000Z claimed=2026-11-25T10:30:11.234Z"),
    ("every 10 sec", LEAP_EVE, "Interval \"every 10 sec\" Some(10000) runs=2024-02-28T23:59:40.500Z,2024-02-28T23:59:50.500Z,2024-02-29T00:00:00.500Z claimed=2024-02-29T00:59:41.734Z"),
    ("every 10 sec", YEAR_END, "Interval \"every 10 sec\" Some(10000) runs=2027-01-01T00:00:09.999Z,2027-01-01T00:00:19.999Z,2027-01-01T00:00:29.999Z claimed=2027-01-01T01:00:11.233Z"),
    ("Every 5 Minutes", TUESDAY, "Interval \"Every 5 Minutes\" Some(300000) runs=2023-11-14T22:18:20.000Z,2023-11-14T22:23:20.000Z,2023-11-14T22:28:20.000Z claimed=2023-11-14T23:18:21.234Z"),
    ("Every 5 Minutes", WEDNESDAY, "Interval \"Every 5 Minutes\" Some(300000) runs=2026-11-25T09:35:00.000Z,2026-11-25T09:40:00.000Z,2026-11-25T09:45:00.000Z claimed=2026-11-25T10:35:01.234Z"),
    ("Every 5 Minutes", LEAP_EVE, "Interval \"Every 5 Minutes\" Some(300000) runs=2024-02-29T00:04:30.500Z,2024-02-29T00:09:30.500Z,2024-02-29T00:14:30.500Z claimed=2024-02-29T01:04:31.734Z"),
    ("Every 5 Minutes", YEAR_END, "Interval \"Every 5 Minutes\" Some(300000) runs=2027-01-01T00:04:59.999Z,2027-01-01T00:09:59.999Z,2027-01-01T00:14:59.999Z claimed=2027-01-01T01:05:01.233Z"),
    ("every 2 hours", TUESDAY, "Interval \"every 2 hours\" Some(7200000) runs=2023-11-15T00:13:20.000Z,2023-11-15T02:13:20.000Z,2023-11-15T04:13:20.000Z claimed=2023-11-15T01:13:21.234Z"),
    ("every 2 hours", WEDNESDAY, "Interval \"every 2 hours\" Some(7200000) runs=2026-11-25T11:30:00.000Z,2026-11-25T13:30:00.000Z,2026-11-25T15:30:00.000Z claimed=2026-11-25T12:30:01.234Z"),
    ("every 2 hours", LEAP_EVE, "Interval \"every 2 hours\" Some(7200000) runs=2024-02-29T01:59:30.500Z,2024-02-29T03:59:30.500Z,2024-02-29T05:59:30.500Z claimed=2024-02-29T02:59:31.734Z"),
    ("every 2 hours", YEAR_END, "Interval \"every 2 hours\" Some(7200000) runs=2027-01-01T01:59:59.999Z,2027-01-01T03:59:59.999Z,2027-01-01T05:59:59.999Z claimed=2027-01-01T03:00:01.233Z"),
    ("every 1d", TUESDAY, "Interval \"every 1d\" Some(86400000) runs=2023-11-15T22:13:20.000Z,2023-11-16T22:13:20.000Z,2023-11-17T22:13:20.000Z claimed=2023-11-15T23:13:21.234Z"),
    ("every 1d", WEDNESDAY, "Interval \"every 1d\" Some(86400000) runs=2026-11-26T09:30:00.000Z,2026-11-27T09:30:00.000Z,2026-11-28T09:30:00.000Z claimed=2026-11-26T10:30:01.234Z"),
    ("every 1d", LEAP_EVE, "Interval \"every 1d\" Some(86400000) runs=2024-02-29T23:59:30.500Z,2024-03-01T23:59:30.500Z,2024-03-02T23:59:30.500Z claimed=2024-03-01T00:59:31.734Z"),
    ("every 1d", YEAR_END, "Interval \"every 1d\" Some(86400000) runs=2027-01-01T23:59:59.999Z,2027-01-02T23:59:59.999Z,2027-01-03T23:59:59.999Z claimed=2027-01-02T01:00:01.233Z"),
    ("every 5s", TUESDAY, "refused"),
    ("every 5s", WEDNESDAY, "refused"),
    ("every 5s", LEAP_EVE, "refused"),
    ("every 5s", YEAR_END, "refused"),
    // Was refused: the store read one `<count> <unit>` part; the shared duration grammar adds parts.
    ("every 1h30m", TUESDAY, "Interval \"every 1h30m\" Some(5400000) runs=2023-11-14T23:43:20.000Z,2023-11-15T01:13:20.000Z,2023-11-15T02:43:20.000Z claimed=2023-11-15T00:43:21.234Z"),
    ("every 1h30m", WEDNESDAY, "Interval \"every 1h30m\" Some(5400000) runs=2026-11-25T11:00:00.000Z,2026-11-25T12:30:00.000Z,2026-11-25T14:00:00.000Z claimed=2026-11-25T12:00:01.234Z"),
    ("every 1h30m", LEAP_EVE, "Interval \"every 1h30m\" Some(5400000) runs=2024-02-29T01:29:30.500Z,2024-02-29T02:59:30.500Z,2024-02-29T04:29:30.500Z claimed=2024-02-29T02:29:31.734Z"),
    ("every 1h30m", YEAR_END, "Interval \"every 1h30m\" Some(5400000) runs=2027-01-01T01:29:59.999Z,2027-01-01T02:59:59.999Z,2027-01-01T04:29:59.999Z claimed=2027-01-01T02:30:01.233Z"),
    ("at 2100-01-01T00:00:00Z", TUESDAY, "Once \"at 2100-01-01T00:00:00Z\" None runs=2100-01-01T00:00:00.000Z,-,- claimed=-"),
    ("at 2100-01-01T00:00:00Z", WEDNESDAY, "Once \"at 2100-01-01T00:00:00Z\" None runs=2100-01-01T00:00:00.000Z,-,- claimed=-"),
    ("at 2100-01-01T00:00:00Z", LEAP_EVE, "Once \"at 2100-01-01T00:00:00Z\" None runs=2100-01-01T00:00:00.000Z,-,- claimed=-"),
    ("at 2100-01-01T00:00:00Z", YEAR_END, "Once \"at 2100-01-01T00:00:00Z\" None runs=2100-01-01T00:00:00.000Z,-,- claimed=-"),
    ("AT 2100-01-01T00:00:00Z", TUESDAY, "Once \"AT 2100-01-01T00:00:00Z\" None runs=2100-01-01T00:00:00.000Z,-,- claimed=-"),
    ("AT 2100-01-01T00:00:00Z", WEDNESDAY, "Once \"AT 2100-01-01T00:00:00Z\" None runs=2100-01-01T00:00:00.000Z,-,- claimed=-"),
    ("AT 2100-01-01T00:00:00Z", LEAP_EVE, "Once \"AT 2100-01-01T00:00:00Z\" None runs=2100-01-01T00:00:00.000Z,-,- claimed=-"),
    ("AT 2100-01-01T00:00:00Z", YEAR_END, "Once \"AT 2100-01-01T00:00:00Z\" None runs=2100-01-01T00:00:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T10:30:00.250+01:00", TUESDAY, "Once \"at 2027-03-01T10:30:00.250+01:00\" None runs=2027-03-01T09:30:00.250Z,-,- claimed=-"),
    ("at 2027-03-01T10:30:00.250+01:00", WEDNESDAY, "Once \"at 2027-03-01T10:30:00.250+01:00\" None runs=2027-03-01T09:30:00.250Z,-,- claimed=-"),
    ("at 2027-03-01T10:30:00.250+01:00", LEAP_EVE, "Once \"at 2027-03-01T10:30:00.250+01:00\" None runs=2027-03-01T09:30:00.250Z,-,- claimed=-"),
    ("at 2027-03-01T10:30:00.250+01:00", YEAR_END, "Once \"at 2027-03-01T10:30:00.250+01:00\" None runs=2027-03-01T09:30:00.250Z,-,- claimed=-"),
    ("at 2027-03-01T09:30:00-05:00", TUESDAY, "Once \"at 2027-03-01T09:30:00-05:00\" None runs=2027-03-01T14:30:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T09:30:00-05:00", WEDNESDAY, "Once \"at 2027-03-01T09:30:00-05:00\" None runs=2027-03-01T14:30:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T09:30:00-05:00", LEAP_EVE, "Once \"at 2027-03-01T09:30:00-05:00\" None runs=2027-03-01T14:30:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T09:30:00-05:00", YEAR_END, "Once \"at 2027-03-01T09:30:00-05:00\" None runs=2027-03-01T14:30:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T09:30Z", TUESDAY, "Once \"at 2027-03-01T09:30Z\" None runs=2027-03-01T09:30:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T09:30Z", WEDNESDAY, "Once \"at 2027-03-01T09:30Z\" None runs=2027-03-01T09:30:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T09:30Z", LEAP_EVE, "Once \"at 2027-03-01T09:30Z\" None runs=2027-03-01T09:30:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T09:30Z", YEAR_END, "Once \"at 2027-03-01T09:30Z\" None runs=2027-03-01T09:30:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T09:30", TUESDAY, "Once \"at 2027-03-01T09:30\" None runs=2027-03-01T09:30:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T09:30", WEDNESDAY, "Once \"at 2027-03-01T09:30\" None runs=2027-03-01T09:30:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T09:30", LEAP_EVE, "Once \"at 2027-03-01T09:30\" None runs=2027-03-01T09:30:00.000Z,-,- claimed=-"),
    ("at 2027-03-01T09:30", YEAR_END, "Once \"at 2027-03-01T09:30\" None runs=2027-03-01T09:30:00.000Z,-,- claimed=-"),
    ("at 1999-01-01T00:00:00Z", TUESDAY, "refused"),
    ("at 1999-01-01T00:00:00Z", WEDNESDAY, "refused"),
    ("at 1999-01-01T00:00:00Z", LEAP_EVE, "refused"),
    ("at 1999-01-01T00:00:00Z", YEAR_END, "refused"),
    ("at tomorrow", TUESDAY, "refused"),
    ("at tomorrow", WEDNESDAY, "refused"),
    ("at tomorrow", LEAP_EVE, "refused"),
    ("at tomorrow", YEAR_END, "refused"),
    // Was 2027-03-02: the store rolled an impossible date over; it is refused now.
    ("at 2027-02-30T00:00:00Z", TUESDAY, "refused"),
    ("at 2027-02-30T00:00:00Z", WEDNESDAY, "refused"),
    ("at 2027-02-30T00:00:00Z", LEAP_EVE, "refused"),
    ("at 2027-02-30T00:00:00Z", YEAR_END, "refused"),
    ("@hourly", TUESDAY, "Cron \"0 * * * *\" None runs=2023-11-14T23:00:00.000Z,2023-11-15T00:00:00.000Z,2023-11-15T01:00:00.000Z claimed=2023-11-15T00:00:00.000Z"),
    ("@hourly", WEDNESDAY, "Cron \"0 * * * *\" None runs=2026-11-25T10:00:00.000Z,2026-11-25T11:00:00.000Z,2026-11-25T12:00:00.000Z claimed=2026-11-25T11:00:00.000Z"),
    ("@hourly", LEAP_EVE, "Cron \"0 * * * *\" None runs=2024-02-29T00:00:00.000Z,2024-02-29T01:00:00.000Z,2024-02-29T02:00:00.000Z claimed=2024-02-29T01:00:00.000Z"),
    ("@hourly", YEAR_END, "Cron \"0 * * * *\" None runs=2027-01-01T00:00:00.000Z,2027-01-01T01:00:00.000Z,2027-01-01T02:00:00.000Z claimed=2027-01-01T02:00:00.000Z"),
    ("@daily", TUESDAY, "Cron \"0 0 * * *\" None runs=2023-11-15T00:00:00.000Z,2023-11-16T00:00:00.000Z,2023-11-17T00:00:00.000Z claimed=2023-11-15T00:00:00.000Z"),
    ("@daily", WEDNESDAY, "Cron \"0 0 * * *\" None runs=2026-11-26T00:00:00.000Z,2026-11-27T00:00:00.000Z,2026-11-28T00:00:00.000Z claimed=2026-11-26T00:00:00.000Z"),
    ("@daily", LEAP_EVE, "Cron \"0 0 * * *\" None runs=2024-02-29T00:00:00.000Z,2024-03-01T00:00:00.000Z,2024-03-02T00:00:00.000Z claimed=2024-03-01T00:00:00.000Z"),
    ("@daily", YEAR_END, "Cron \"0 0 * * *\" None runs=2027-01-01T00:00:00.000Z,2027-01-02T00:00:00.000Z,2027-01-03T00:00:00.000Z claimed=2027-01-02T00:00:00.000Z"),
    ("@weekly", TUESDAY, "Cron \"0 0 * * 0\" None runs=2023-11-19T00:00:00.000Z,2023-11-26T00:00:00.000Z,2023-12-03T00:00:00.000Z claimed=2023-11-19T00:00:00.000Z"),
    ("@weekly", WEDNESDAY, "Cron \"0 0 * * 0\" None runs=2026-11-29T00:00:00.000Z,2026-12-06T00:00:00.000Z,2026-12-13T00:00:00.000Z claimed=2026-11-29T00:00:00.000Z"),
    ("@weekly", LEAP_EVE, "Cron \"0 0 * * 0\" None runs=2024-03-03T00:00:00.000Z,2024-03-10T00:00:00.000Z,2024-03-17T00:00:00.000Z claimed=2024-03-03T00:00:00.000Z"),
    ("@weekly", YEAR_END, "Cron \"0 0 * * 0\" None runs=2027-01-03T00:00:00.000Z,2027-01-10T00:00:00.000Z,2027-01-17T00:00:00.000Z claimed=2027-01-03T00:00:00.000Z"),
    ("@monthly", TUESDAY, "Cron \"0 0 1 * *\" None runs=2023-12-01T00:00:00.000Z,2024-01-01T00:00:00.000Z,2024-02-01T00:00:00.000Z claimed=2023-12-01T00:00:00.000Z"),
    ("@monthly", WEDNESDAY, "Cron \"0 0 1 * *\" None runs=2026-12-01T00:00:00.000Z,2027-01-01T00:00:00.000Z,2027-02-01T00:00:00.000Z claimed=2026-12-01T00:00:00.000Z"),
    ("@monthly", LEAP_EVE, "Cron \"0 0 1 * *\" None runs=2024-03-01T00:00:00.000Z,2024-04-01T00:00:00.000Z,2024-05-01T00:00:00.000Z claimed=2024-03-01T00:00:00.000Z"),
    ("@monthly", YEAR_END, "Cron \"0 0 1 * *\" None runs=2027-01-01T00:00:00.000Z,2027-02-01T00:00:00.000Z,2027-03-01T00:00:00.000Z claimed=2027-02-01T00:00:00.000Z"),
    ("0 12 * * *", TUESDAY, "Cron \"0 12 * * *\" None runs=2023-11-15T12:00:00.000Z,2023-11-16T12:00:00.000Z,2023-11-17T12:00:00.000Z claimed=2023-11-15T12:00:00.000Z"),
    ("0 12 * * *", WEDNESDAY, "Cron \"0 12 * * *\" None runs=2026-11-25T12:00:00.000Z,2026-11-26T12:00:00.000Z,2026-11-27T12:00:00.000Z claimed=2026-11-25T12:00:00.000Z"),
    ("0 12 * * *", LEAP_EVE, "Cron \"0 12 * * *\" None runs=2024-02-29T12:00:00.000Z,2024-03-01T12:00:00.000Z,2024-03-02T12:00:00.000Z claimed=2024-02-29T12:00:00.000Z"),
    ("0 12 * * *", YEAR_END, "Cron \"0 12 * * *\" None runs=2027-01-01T12:00:00.000Z,2027-01-02T12:00:00.000Z,2027-01-03T12:00:00.000Z claimed=2027-01-01T12:00:00.000Z"),
    ("*/15 9-17 * * 1-5", TUESDAY, "Cron \"*/15 9-17 * * 1-5\" None runs=2023-11-15T09:00:00.000Z,2023-11-15T09:15:00.000Z,2023-11-15T09:30:00.000Z claimed=2023-11-15T09:00:00.000Z"),
    ("*/15 9-17 * * 1-5", WEDNESDAY, "Cron \"*/15 9-17 * * 1-5\" None runs=2026-11-25T09:45:00.000Z,2026-11-25T10:00:00.000Z,2026-11-25T10:15:00.000Z claimed=2026-11-25T10:45:00.000Z"),
    ("*/15 9-17 * * 1-5", LEAP_EVE, "Cron \"*/15 9-17 * * 1-5\" None runs=2024-02-29T09:00:00.000Z,2024-02-29T09:15:00.000Z,2024-02-29T09:30:00.000Z claimed=2024-02-29T09:00:00.000Z"),
    ("*/15 9-17 * * 1-5", YEAR_END, "Cron \"*/15 9-17 * * 1-5\" None runs=2027-01-01T09:00:00.000Z,2027-01-01T09:15:00.000Z,2027-01-01T09:30:00.000Z claimed=2027-01-01T09:00:00.000Z"),
    ("0 0 * * 7", TUESDAY, "Cron \"0 0 * * 7\" None runs=2023-11-19T00:00:00.000Z,2023-11-26T00:00:00.000Z,2023-12-03T00:00:00.000Z claimed=2023-11-19T00:00:00.000Z"),
    ("0 0 * * 7", WEDNESDAY, "Cron \"0 0 * * 7\" None runs=2026-11-29T00:00:00.000Z,2026-12-06T00:00:00.000Z,2026-12-13T00:00:00.000Z claimed=2026-11-29T00:00:00.000Z"),
    ("0 0 * * 7", LEAP_EVE, "Cron \"0 0 * * 7\" None runs=2024-03-03T00:00:00.000Z,2024-03-10T00:00:00.000Z,2024-03-17T00:00:00.000Z claimed=2024-03-03T00:00:00.000Z"),
    ("0 0 * * 7", YEAR_END, "Cron \"0 0 * * 7\" None runs=2027-01-03T00:00:00.000Z,2027-01-10T00:00:00.000Z,2027-01-17T00:00:00.000Z claimed=2027-01-03T00:00:00.000Z"),
    ("0 0 * * 1-7", TUESDAY, "Cron \"0 0 * * 1-7\" None runs=2023-11-15T00:00:00.000Z,2023-11-16T00:00:00.000Z,2023-11-17T00:00:00.000Z claimed=2023-11-15T00:00:00.000Z"),
    ("0 0 * * 1-7", WEDNESDAY, "Cron \"0 0 * * 1-7\" None runs=2026-11-26T00:00:00.000Z,2026-11-27T00:00:00.000Z,2026-11-28T00:00:00.000Z claimed=2026-11-26T00:00:00.000Z"),
    ("0 0 * * 1-7", LEAP_EVE, "Cron \"0 0 * * 1-7\" None runs=2024-02-29T00:00:00.000Z,2024-03-01T00:00:00.000Z,2024-03-02T00:00:00.000Z claimed=2024-03-01T00:00:00.000Z"),
    ("0 0 * * 1-7", YEAR_END, "Cron \"0 0 * * 1-7\" None runs=2027-01-01T00:00:00.000Z,2027-01-02T00:00:00.000Z,2027-01-03T00:00:00.000Z claimed=2027-01-02T00:00:00.000Z"),
    ("0 0 13 * 5", TUESDAY, "Cron \"0 0 13 * 5\" None runs=2024-09-13T00:00:00.000Z,2024-12-13T00:00:00.000Z,2025-06-13T00:00:00.000Z claimed=2024-09-13T00:00:00.000Z"),
    // Was an error after the first run, which left the job with no next run: the store searched one year ahead, and the next Friday the 13th is 14 months on.
    ("0 0 13 * 5", WEDNESDAY, "Cron \"0 0 13 * 5\" None runs=2027-08-13T00:00:00.000Z,2028-10-13T00:00:00.000Z,2029-04-13T00:00:00.000Z claimed=2027-08-13T00:00:00.000Z"),
    ("0 0 13 * 5", LEAP_EVE, "Cron \"0 0 13 * 5\" None runs=2024-09-13T00:00:00.000Z,2024-12-13T00:00:00.000Z,2025-06-13T00:00:00.000Z claimed=2024-09-13T00:00:00.000Z"),
    ("0 0 13 * 5", YEAR_END, "Cron \"0 0 13 * 5\" None runs=2027-08-13T00:00:00.000Z,2028-10-13T00:00:00.000Z,2029-04-13T00:00:00.000Z claimed=2027-08-13T00:00:00.000Z"),
    ("0 0 31 * *", TUESDAY, "Cron \"0 0 31 * *\" None runs=2023-12-31T00:00:00.000Z,2024-01-31T00:00:00.000Z,2024-03-31T00:00:00.000Z claimed=2023-12-31T00:00:00.000Z"),
    ("0 0 31 * *", WEDNESDAY, "Cron \"0 0 31 * *\" None runs=2026-12-31T00:00:00.000Z,2027-01-31T00:00:00.000Z,2027-03-31T00:00:00.000Z claimed=2026-12-31T00:00:00.000Z"),
    ("0 0 31 * *", LEAP_EVE, "Cron \"0 0 31 * *\" None runs=2024-03-31T00:00:00.000Z,2024-05-31T00:00:00.000Z,2024-07-31T00:00:00.000Z claimed=2024-03-31T00:00:00.000Z"),
    ("0 0 31 * *", YEAR_END, "Cron \"0 0 31 * *\" None runs=2027-01-31T00:00:00.000Z,2027-03-31T00:00:00.000Z,2027-05-31T00:00:00.000Z claimed=2027-01-31T00:00:00.000Z"),
    ("30 2 1,15 * *", TUESDAY, "Cron \"30 2 1,15 * *\" None runs=2023-11-15T02:30:00.000Z,2023-12-01T02:30:00.000Z,2023-12-15T02:30:00.000Z claimed=2023-11-15T02:30:00.000Z"),
    ("30 2 1,15 * *", WEDNESDAY, "Cron \"30 2 1,15 * *\" None runs=2026-12-01T02:30:00.000Z,2026-12-15T02:30:00.000Z,2027-01-01T02:30:00.000Z claimed=2026-12-01T02:30:00.000Z"),
    ("30 2 1,15 * *", LEAP_EVE, "Cron \"30 2 1,15 * *\" None runs=2024-03-01T02:30:00.000Z,2024-03-15T02:30:00.000Z,2024-04-01T02:30:00.000Z claimed=2024-03-01T02:30:00.000Z"),
    ("30 2 1,15 * *", YEAR_END, "Cron \"30 2 1,15 * *\" None runs=2027-01-01T02:30:00.000Z,2027-01-15T02:30:00.000Z,2027-02-01T02:30:00.000Z claimed=2027-01-01T02:30:00.000Z"),
    ("5-10/2 * * * *", TUESDAY, "Cron \"5-10/2 * * * *\" None runs=2023-11-14T23:05:00.000Z,2023-11-14T23:07:00.000Z,2023-11-14T23:09:00.000Z claimed=2023-11-15T00:05:00.000Z"),
    ("5-10/2 * * * *", WEDNESDAY, "Cron \"5-10/2 * * * *\" None runs=2026-11-25T10:05:00.000Z,2026-11-25T10:07:00.000Z,2026-11-25T10:09:00.000Z claimed=2026-11-25T11:05:00.000Z"),
    ("5-10/2 * * * *", LEAP_EVE, "Cron \"5-10/2 * * * *\" None runs=2024-02-29T00:05:00.000Z,2024-02-29T00:07:00.000Z,2024-02-29T00:09:00.000Z claimed=2024-02-29T01:05:00.000Z"),
    ("5-10/2 * * * *", YEAR_END, "Cron \"5-10/2 * * * *\" None runs=2027-01-01T00:05:00.000Z,2027-01-01T00:07:00.000Z,2027-01-01T00:09:00.000Z claimed=2027-01-01T01:05:00.000Z"),
    ("59 23 31 12 *", TUESDAY, "Cron \"59 23 31 12 *\" None runs=2023-12-31T23:59:00.000Z,2024-12-31T23:59:00.000Z,2025-12-31T23:59:00.000Z claimed=2023-12-31T23:59:00.000Z"),
    ("59 23 31 12 *", WEDNESDAY, "Cron \"59 23 31 12 *\" None runs=2026-12-31T23:59:00.000Z,2027-12-31T23:59:00.000Z,2028-12-31T23:59:00.000Z claimed=2026-12-31T23:59:00.000Z"),
    ("59 23 31 12 *", LEAP_EVE, "Cron \"59 23 31 12 *\" None runs=2024-12-31T23:59:00.000Z,2025-12-31T23:59:00.000Z,2026-12-31T23:59:00.000Z claimed=2024-12-31T23:59:00.000Z"),
    ("59 23 31 12 *", YEAR_END, "Cron \"59 23 31 12 *\" None runs=2027-12-31T23:59:00.000Z,2028-12-31T23:59:00.000Z,2029-12-31T23:59:00.000Z claimed=2027-12-31T23:59:00.000Z"),
    // Was refused, or an error after the first run: the store searched one year ahead, and the next 29 February is up to four years on.
    ("0 0 29 2 *", TUESDAY, "Cron \"0 0 29 2 *\" None runs=2024-02-29T00:00:00.000Z,2028-02-29T00:00:00.000Z,2032-02-29T00:00:00.000Z claimed=2024-02-29T00:00:00.000Z"),
    ("0 0 29 2 *", WEDNESDAY, "Cron \"0 0 29 2 *\" None runs=2028-02-29T00:00:00.000Z,2032-02-29T00:00:00.000Z,2036-02-29T00:00:00.000Z claimed=2028-02-29T00:00:00.000Z"),
    ("0 0 29 2 *", LEAP_EVE, "Cron \"0 0 29 2 *\" None runs=2024-02-29T00:00:00.000Z,2028-02-29T00:00:00.000Z,2032-02-29T00:00:00.000Z claimed=2028-02-29T00:00:00.000Z"),
    ("0 0 29 2 *", YEAR_END, "Cron \"0 0 29 2 *\" None runs=2028-02-29T00:00:00.000Z,2032-02-29T00:00:00.000Z,2036-02-29T00:00:00.000Z claimed=2028-02-29T00:00:00.000Z"),
    // Was refused: the store knew @hourly, @daily, @weekly and @monthly only.
    ("@yearly", TUESDAY, "Cron \"0 0 1 1 *\" None runs=2024-01-01T00:00:00.000Z,2025-01-01T00:00:00.000Z,2026-01-01T00:00:00.000Z claimed=2024-01-01T00:00:00.000Z"),
    ("@yearly", WEDNESDAY, "Cron \"0 0 1 1 *\" None runs=2027-01-01T00:00:00.000Z,2028-01-01T00:00:00.000Z,2029-01-01T00:00:00.000Z claimed=2027-01-01T00:00:00.000Z"),
    ("@yearly", LEAP_EVE, "Cron \"0 0 1 1 *\" None runs=2025-01-01T00:00:00.000Z,2026-01-01T00:00:00.000Z,2027-01-01T00:00:00.000Z claimed=2025-01-01T00:00:00.000Z"),
    ("@yearly", YEAR_END, "Cron \"0 0 1 1 *\" None runs=2027-01-01T00:00:00.000Z,2028-01-01T00:00:00.000Z,2029-01-01T00:00:00.000Z claimed=2028-01-01T00:00:00.000Z"),
    ("0 0 30 2 *", TUESDAY, "refused"),
    ("0 0 30 2 *", WEDNESDAY, "refused"),
    ("0 0 30 2 *", LEAP_EVE, "refused"),
    ("0 0 30 2 *", YEAR_END, "refused"),
    ("*/60 * * * *", TUESDAY, "refused"),
    ("*/60 * * * *", WEDNESDAY, "refused"),
    ("*/60 * * * *", LEAP_EVE, "refused"),
    ("*/60 * * * *", YEAR_END, "refused"),
    ("* * * *", TUESDAY, "refused"),
    ("* * * *", WEDNESDAY, "refused"),
    ("* * * *", LEAP_EVE, "refused"),
    ("* * * *", YEAR_END, "refused"),
    ("60 * * * *", TUESDAY, "refused"),
    ("60 * * * *", WEDNESDAY, "refused"),
    ("60 * * * *", LEAP_EVE, "refused"),
    ("60 * * * *", YEAR_END, "refused"),
    ("a * * * *", TUESDAY, "refused"),
    ("a * * * *", WEDNESDAY, "refused"),
    ("a * * * *", LEAP_EVE, "refused"),
    ("a * * * *", YEAR_END, "refused"),
    ("*/0 * * * *", TUESDAY, "refused"),
    ("*/0 * * * *", WEDNESDAY, "refused"),
    ("*/0 * * * *", LEAP_EVE, "refused"),
    ("*/0 * * * *", YEAR_END, "refused"),
    ("", TUESDAY, "refused"),
    ("", WEDNESDAY, "refused"),
    ("", LEAP_EVE, "refused"),
    ("", YEAR_END, "refused"),
];
