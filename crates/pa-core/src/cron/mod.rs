//! Scheduled jobs: their schedules (read and evaluated by
//! `pa_sources::schedule`, the grammar timer sources share) and `/heartbeat`
//! command parsing; the file-backed job store lives in the `store`
//! submodule.

pub mod heartbeat_commands;
pub mod scheduler;
pub mod store;

use std::time::Duration;

use pa_sources::schedule::{parse_duration, Cron, DurationUnits, Schedule};
use serde::{Deserialize, Serialize};

pub(crate) use pa_sources::schedule::parse_instant;

pub const DEFAULT_HEARTBEAT_SCHEDULE: &str = "every 5m";
pub const DEFAULT_HEARTBEAT_DELIVERY_MODE: DeliveryMode = DeliveryMode::Steer;
/// The shortest interval a job's `every` schedule takes.
const MIN_JOB_INTERVAL: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Active,
    Paused,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    Once,
    Cron,
    Interval,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    Steer,
    FollowUp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCronSchedule {
    pub kind: ScheduleKind,
    pub expression: String,
    /// Only for `interval` schedules.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_ms: Option<u64>,
}

/// One scheduled job (persisted wire shape). Session identity fields are
/// required (the TS guard rejects records without them).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCronJob {
    pub id: String,
    pub status: JobStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_kind: Option<String>,
    /// Delivery mode for heartbeat jobs when the session is busy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivery_mode: Option<DeliveryMode>,
    pub active_session_id: String,
    pub session_id: String,
    pub session_file: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub prompt: String,
    pub schedule: AgentCronSchedule,
    #[serde(rename = "createdAt")]
    pub created_at: String,
    #[serde(rename = "updatedAt")]
    pub updated_at: String,
    #[serde(rename = "nextRunAt", default, skip_serializing_if = "Option::is_none")]
    pub next_run_at: Option<String>,
    #[serde(rename = "lastRunAt", default, skip_serializing_if = "Option::is_none")]
    pub last_run_at: Option<String>,
    #[serde(
        rename = "lastSkippedAt",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub last_skipped_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(rename = "runCount", default)]
    pub run_count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ParsedHeartbeatCommand {
    Status,
    Pause,
    Resume,
    Clear,
    Set {
        schedule: String,
        instruction: String,
        delivery_mode: Option<DeliveryMode>,
    },
}

/// Session activity snapshot used by heartbeat deferral.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "the booleans mirror the TS API shape: they are the product's own surface"
)]
pub struct HeartbeatSessionActivity {
    pub is_streaming: bool,
    pub is_compacting: bool,
    pub is_retrying: bool,
    pub is_bash_running: bool,
    pub has_pending_session_work: bool,
    pub unfinished_action_count: usize,
}

/// Parse a schedule expression into a schedule plus its first run time.
/// `now_millis` is the epoch time in milliseconds.
///
/// # Errors
///
/// Returns an error when the expression is empty, an `at` one-shot is invalid,
/// or the cron schedule has no next run.
pub fn parse_agent_cron_schedule(
    input: &str,
    now_millis: u64,
) -> anyhow::Result<(AgentCronSchedule, u64)> {
    let parsed = Schedule::parse(input, now_millis, MIN_JOB_INTERVAL)?;
    let (kind, interval_ms, first_run) = match &parsed.schedule {
        Schedule::At { unix_ms } => (ScheduleKind::Once, None, Some(*unix_ms)),
        Schedule::Every { interval_ms } => (
            ScheduleKind::Interval,
            Some(*interval_ms),
            parsed.schedule.next_after(now_millis),
        ),
        Schedule::Cron(_) => (
            ScheduleKind::Cron,
            None,
            parsed.schedule.next_after(now_millis),
        ),
    };
    let first_run =
        first_run.ok_or_else(|| anyhow::anyhow!("Schedule {:?} never runs", parsed.expression))?;
    Ok((
        AgentCronSchedule {
            kind,
            expression: parsed.expression,
            interval_ms,
        },
        first_run,
    ))
}

pub fn normalize_heartbeat_schedule(input: Option<&str>) -> String {
    let text = input.map(str::trim).filter(|text| !text.is_empty());
    let Some(text) = text else {
        return DEFAULT_HEARTBEAT_SCHEDULE.to_string();
    };
    if parse_duration(text, DurationUnits::WithSeconds).is_some() {
        return format!("every {text}");
    }
    text.to_string()
}

/// Normalize an optional heartbeat delivery mode string.
///
/// # Errors
///
/// Returns an error when `value` is `Some` but neither `"steer"` nor `"follow_up"`.
pub fn normalize_heartbeat_delivery_mode(
    value: Option<&str>,
) -> anyhow::Result<Option<DeliveryMode>> {
    match value {
        None => Ok(None),
        Some("steer") => Ok(Some(DeliveryMode::Steer)),
        Some("follow_up") => Ok(Some(DeliveryMode::FollowUp)),
        Some(_) => Err(anyhow::anyhow!(
            "Heartbeat delivery mode must be \"steer\" or \"follow_up\""
        )),
    }
}

#[must_use]
pub fn resolve_heartbeat_streaming_behavior(delivery_mode: Option<DeliveryMode>) -> &'static str {
    match delivery_mode.unwrap_or(DEFAULT_HEARTBEAT_DELIVERY_MODE) {
        DeliveryMode::FollowUp => "followUp",
        DeliveryMode::Steer => "steer",
    }
}

/// Parse a `/heartbeat ...` command body.
///
/// # Errors
///
/// Returns an error when the command body is malformed.
pub fn parse_heartbeat_command(input: &str) -> anyhow::Result<ParsedHeartbeatCommand> {
    let text = input
        .strip_prefix("/heartbeat")
        .filter(|rest| rest.chars().next().is_none_or(char::is_whitespace))
        .map_or_else(|| input.trim(), str::trim_start);
    if text.is_empty() || text == "status" {
        return Ok(ParsedHeartbeatCommand::Status);
    }
    if text == "pause" {
        return Ok(ParsedHeartbeatCommand::Pause);
    }
    if text == "resume" {
        return Ok(ParsedHeartbeatCommand::Resume);
    }
    if text == "clear" || text == "stop" {
        return Ok(ParsedHeartbeatCommand::Clear);
    }
    let leading = consume_delivery_option(text)?;
    let mut delivery_mode = leading.0;
    let rest = leading.1;

    if let Some(option) = consume_every_option(&rest)? {
        let trailing = consume_delivery_option(&option.1)?;
        delivery_mode = trailing.0.or(delivery_mode);
        if trailing.1.is_empty() {
            anyhow::bail!(
                "Usage: /heartbeat [--every <interval>] [--steer|--follow-up] <instruction>"
            );
        }
        return Ok(ParsedHeartbeatCommand::Set {
            schedule: normalize_heartbeat_schedule(Some(&option.0)),
            instruction: trailing.1,
            delivery_mode,
        });
    }
    if let Some(leading_schedule) = consume_leading_every_schedule(&rest) {
        let trailing = consume_delivery_option(&leading_schedule.1)?;
        delivery_mode = trailing.0.or(delivery_mode);
        if trailing.1.is_empty() {
            anyhow::bail!(
                "Usage: /heartbeat [--every <interval>] [--steer|--follow-up] <instruction>"
            );
        }
        return Ok(ParsedHeartbeatCommand::Set {
            schedule: normalize_heartbeat_schedule(Some(&leading_schedule.0)),
            instruction: trailing.1,
            delivery_mode,
        });
    }
    let rest = rest.trim();
    if rest.is_empty() {
        anyhow::bail!("Usage: /heartbeat [--every <interval>] [--steer|--follow-up] <instruction>");
    }
    Ok(ParsedHeartbeatCommand::Set {
        schedule: DEFAULT_HEARTBEAT_SCHEDULE.to_string(),
        instruction: rest.to_string(),
        delivery_mode,
    })
}

fn consume_delivery_option(text: &str) -> anyhow::Result<(Option<DeliveryMode>, String)> {
    let mut rest = text.trim().to_string();
    let lower = rest.to_lowercase();
    if lower == "--deliver" || lower.ends_with(" --deliver") {
        return Err(anyhow::anyhow!(
            "Heartbeat delivery mode must be \"steer\" or \"follow_up\""
        ));
    }
    let mut delivery_mode: Option<DeliveryMode> = None;
    while let Some((mode, remaining)) = consume_leading_delivery_flag(&rest)? {
        delivery_mode = Some(mode);
        rest = remaining.trim().to_string();
    }
    let mut trailing_delivery_mode: Option<DeliveryMode> = None;
    while let Some((mode, remaining)) = consume_trailing_delivery_flag(&rest)? {
        // Consume all trailing flags but keep the rightmost (textually latest).
        trailing_delivery_mode = trailing_delivery_mode.or(Some(mode));
        rest = remaining.trim().to_string();
    }
    Ok((trailing_delivery_mode.or(delivery_mode), rest))
}

fn consume_leading_delivery_flag(text: &str) -> anyhow::Result<Option<(DeliveryMode, String)>> {
    let trimmed = text.trim_start();
    let Some(after_dash) = trimmed.strip_prefix("--") else {
        return Ok(None);
    };
    if let Some(value) = after_dash.strip_prefix("deliver") {
        let value = value
            .strip_prefix('=')
            .map(String::from)
            .or_else(|| value.strip_prefix(char::is_whitespace).map(String::from))
            .unwrap_or_default();
        if value.is_empty() {
            anyhow::bail!("Heartbeat delivery mode must be \"steer\" or \"follow_up\"");
        }
        let (token, remainder) = split_first_token(&value);
        let mode = parse_delivery_mode_token(token)?;
        return Ok(Some((mode, remainder.to_string())));
    }
    for (token, mode) in [
        ("steer", DeliveryMode::Steer),
        ("follow-up", DeliveryMode::FollowUp),
        ("follow_up", DeliveryMode::FollowUp),
    ] {
        if let Some(rest) = after_dash.strip_prefix(token) {
            check_standalone(after_dash, token)?;
            return Ok(Some((mode, rest.to_string())));
        }
    }
    Ok(None)
}

fn check_standalone(rest: &str, token: &str) -> anyhow::Result<()> {
    let after = &rest[token.len()..];
    if !after.is_empty() && !after.starts_with(char::is_whitespace) {
        anyhow::bail!("Heartbeat delivery mode must be \"steer\" or \"follow_up\"");
    }
    Ok(())
}

fn split_first_token(value: &str) -> (&str, &str) {
    match value.find(char::is_whitespace) {
        Some(index) => (&value[..index], value[index..].trim_start()),
        None => (value, ""),
    }
}

fn consume_trailing_delivery_flag(text: &str) -> anyhow::Result<Option<(DeliveryMode, String)>> {
    let lower = text.to_lowercase();
    if let Some(index) = lower.rfind(" --deliver ") {
        let token = text[index + " --deliver ".len()..].trim();
        if !token.is_empty() && !token.contains(char::is_whitespace) {
            return Ok(Some((
                parse_delivery_mode_token(token)?,
                text[..index].to_string(),
            )));
        }
    }
    if let Some(index) = lower.rfind(" --deliver=") {
        let token = text[index + " --deliver=".len()..].trim();
        if !token.is_empty() && !token.contains(char::is_whitespace) {
            return Ok(Some((
                parse_delivery_mode_token(token)?,
                text[..index].to_string(),
            )));
        }
    }
    for token in ["--follow-up", "--follow_up", "--steer"] {
        if let Some(index) = lower.rfind(&format!(" {token}")) {
            if index + token.len() + 1 == text.len() {
                return Ok(Some((
                    parse_delivery_mode_token(&token[2..])?,
                    text[..index].to_string(),
                )));
            }
        }
    }
    Ok(None)
}

fn parse_delivery_mode_token(token: &str) -> anyhow::Result<DeliveryMode> {
    let normalized = token.to_lowercase().replace('-', "_");
    match normalized.as_str() {
        "steer" => Ok(DeliveryMode::Steer),
        "follow_up" => Ok(DeliveryMode::FollowUp),
        _ => Err(anyhow::anyhow!(
            "Heartbeat delivery mode must be \"steer\" or \"follow_up\""
        )),
    }
}

fn consume_every_option(text: &str) -> anyhow::Result<Option<(String, String)>> {
    let trimmed = text.trim_start();
    let Some(rest) = trimmed.strip_prefix("--every") else {
        return Ok(None);
    };
    let rest = match rest.strip_prefix('=') {
        Some(value) => value.to_string(),
        None => match rest.strip_prefix(char::is_whitespace) {
            Some(value) => value.to_string(),
            // No separator: "--everyfoo" is not an every option.
            None if !rest.is_empty() => return Ok(None),
            None => String::new(),
        },
    };
    let (interval, remainder): (String, String) = if let Some(stripped) = rest.strip_prefix('"') {
        match stripped.find('"') {
            Some(index) => (
                stripped[..index].to_string(),
                stripped[index + 1..].to_string(),
            ),
            None => (stripped.to_string(), String::new()),
        }
    } else if let Some(stripped) = rest.strip_prefix('\'') {
        match stripped.find('\'') {
            Some(index) => (
                stripped[..index].to_string(),
                stripped[index + 1..].to_string(),
            ),
            None => (stripped.to_string(), String::new()),
        }
    } else {
        let (token, rest) = split_first_token(&rest);
        (token.to_string(), rest.to_string())
    };
    if interval.is_empty() {
        anyhow::bail!("Usage: /heartbeat [--every <interval>] [--steer|--follow-up] <instruction>");
    }
    Ok(Some((interval, remainder.trim_start().to_string())))
}

fn consume_leading_every_schedule(text: &str) -> Option<(String, String)> {
    let lower = text.to_lowercase();
    for prefix in ["every ", "each "] {
        if let Some(rest) = lower.strip_prefix(prefix) {
            let first_token = rest.split_whitespace().next()?;
            if parse_duration(first_token, DurationUnits::WithSeconds).is_none() {
                continue;
            }
            {
                let consumed = prefix.len() + first_token.len();
                let consumed = consumed.min(text.len());
                let remainder = text[consumed..].trim();
                // Strip a standalone "--" separator, never a flag.
                let remainder = remainder
                    .strip_prefix("--")
                    .filter(|after| after.is_empty() || after.starts_with(char::is_whitespace))
                    .map_or(remainder, str::trim)
                    .trim()
                    .to_string();
                let interval = format!("{} {}", prefix.trim(), first_token);
                return Some((interval, remainder));
            }
        }
    }
    None
}

#[must_use]
pub fn is_heartbeat_cron_job(job: &AgentCronJob) -> bool {
    matches!(job.source.as_deref(), Some("heartbeat" | "rlm_heartbeat"))
}

#[must_use]
pub fn should_defer_heartbeat_cron_job(
    job: &AgentCronJob,
    activity: &HeartbeatSessionActivity,
) -> bool {
    if !is_heartbeat_cron_job(job) {
        return false;
    }
    let busy_besides_streaming = activity.is_compacting
        || activity.is_retrying
        || activity.is_bash_running
        || activity.has_pending_session_work
        || (!activity.is_streaming && activity.unfinished_action_count > 0);
    if busy_besides_streaming {
        return true;
    }
    if resolve_heartbeat_streaming_behavior(job.delivery_mode) == "steer" {
        return false;
    }
    activity.is_streaming
}

/// Next run time for a schedule after `after_millis`.
///
/// # Errors
///
/// Returns an error when the interval is zero or the cron expression does not parse.
pub fn next_run_at_for_schedule(
    schedule: &AgentCronSchedule,
    after_millis: u64,
) -> anyhow::Result<Option<u64>> {
    match schedule.kind {
        ScheduleKind::Once => Ok(None),
        ScheduleKind::Interval => {
            let Some(interval_ms) = schedule.interval_ms else {
                anyhow::bail!("Invalid interval schedule: {}", schedule.expression);
            };
            if interval_ms == 0 {
                anyhow::bail!("Invalid interval schedule: {}", schedule.expression);
            }
            Ok(Schedule::Every { interval_ms }.next_after(after_millis))
        }
        ScheduleKind::Cron => {
            Ok(Schedule::Cron(Cron::parse(&schedule.expression)?).next_after(after_millis))
        }
    }
}

/// One-line job summary (TS format; the local-rendered timestamps are
/// approximated by UTC).
#[must_use]
pub fn format_agent_cron_job(job: &AgentCronJob) -> String {
    let next = job.next_run_at.as_deref().unwrap_or("-");
    let last = job.last_run_at.as_deref().unwrap_or("-");
    let preview: String = job.prompt.split_whitespace().collect::<Vec<_>>().join(" ");
    let preview: String = preview.chars().take(80).collect();
    let error = job
        .last_error
        .as_ref()
        .map_or_default(|error| format!(" error={error}"));
    let label = job
        .label
        .as_ref()
        .map_or_default(|label| format!(" label=\"{label}\""));
    let skipped = job
        .last_skipped_at
        .as_ref()
        .map_or_default(|skipped| format!(" skipped={skipped}"));
    let status = match job.status {
        JobStatus::Active => "active",
        JobStatus::Paused => "paused",
        JobStatus::Completed => "completed",
        JobStatus::Cancelled => "cancelled",
    };
    format!(
        "{} {status}{} next={next} last={last}{skipped} runs={} schedule=\"{}\" prompt=\"{preview}\"{error}",
        job.id, label, job.run_count, job.schedule.expression
    )
}

pub fn is_due_job(job: &AgentCronJob, now_millis: u64) -> bool {
    if job.status != JobStatus::Active {
        return false;
    }
    job.next_run_at
        .as_deref()
        .and_then(parse_instant)
        .is_some_and(|next| next <= now_millis)
}

#[cfg(test)]
mod golden_tests;

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: u64 = 1_700_000_000_000; // 2023-11-14T22:13:20Z

    #[test]
    fn parse_one_shot_and_interval_schedules() {
        let (schedule, next) = parse_agent_cron_schedule("in 10m", BASE).unwrap();
        assert_eq!(schedule.kind, ScheduleKind::Once);
        assert_eq!(next, BASE + 600_000);
        let (schedule, next) = parse_agent_cron_schedule("IN 2H", BASE).unwrap();
        assert_eq!(schedule.kind, ScheduleKind::Once);
        assert_eq!(next, BASE + 7_200_000);
        let (schedule, next) = parse_agent_cron_schedule("each 30s", BASE).unwrap();
        assert_eq!(schedule.kind, ScheduleKind::Interval);
        assert_eq!(schedule.interval_ms, Some(30_000));
        assert_eq!(next, BASE + 30_000);
        assert!(parse_agent_cron_schedule("every 5s", BASE)
            .unwrap_err()
            .to_string()
            .contains("at least 10 seconds"));
        assert!(parse_agent_cron_schedule("", BASE).is_err());
        let (_, next) = parse_agent_cron_schedule("at 2100-01-01T00:00:00Z", BASE).unwrap();
        assert_eq!(next, 4_102_444_800_000);
        assert!(parse_agent_cron_schedule("at 1999-01-01T00:00:00Z", BASE).is_err());
        let (schedule, _) = parse_agent_cron_schedule("'in 10m'", BASE).unwrap();
        assert_eq!(schedule.expression, "in 10m");
    }

    #[test]
    fn next_run_rollover() {
        let (_, next) = parse_agent_cron_schedule("@daily", BASE).unwrap();
        let schedule = AgentCronSchedule {
            kind: ScheduleKind::Cron,
            expression: "0 0 * * *".to_string(),
            interval_ms: None,
        };
        assert_eq!(
            next_run_at_for_schedule(&schedule, next).unwrap(),
            Some(next + 24 * 60 * 60_000)
        );
        let interval = AgentCronSchedule {
            kind: ScheduleKind::Interval,
            expression: "every 10m".to_string(),
            interval_ms: Some(600_000),
        };
        assert_eq!(
            next_run_at_for_schedule(&interval, BASE).unwrap(),
            Some(BASE + 600_000)
        );
        let once = AgentCronSchedule {
            kind: ScheduleKind::Once,
            expression: "in 10m".to_string(),
            interval_ms: None,
        };
        assert_eq!(next_run_at_for_schedule(&once, BASE).unwrap(), None);
    }

    #[test]
    fn heartbeat_command_parsing() {
        use ParsedHeartbeatCommand::*;
        assert_eq!(parse_heartbeat_command("status").unwrap(), Status);
        assert_eq!(parse_heartbeat_command("/heartbeat").unwrap(), Status);
        assert_eq!(
            parse_heartbeat_command("/heartbeat status").unwrap(),
            Status
        );
        assert_eq!(parse_heartbeat_command("/heartbeat pause").unwrap(), Pause);
        assert_eq!(
            parse_heartbeat_command("/heartbeat resume").unwrap(),
            Resume
        );
        assert_eq!(parse_heartbeat_command("/heartbeat stop").unwrap(), Clear);
        let Set {
            schedule,
            instruction,
            delivery_mode,
        } = parse_heartbeat_command("/heartbeat check the build").unwrap()
        else {
            panic!("expected set");
        };
        assert_eq!(schedule, "every 5m");
        assert_eq!(instruction, "check the build");
        assert_eq!(delivery_mode, None);
        let Set {
            schedule,
            instruction,
            ..
        } = parse_heartbeat_command("/heartbeat --every 10m keep going").unwrap()
        else {
            panic!("expected set");
        };
        assert_eq!(schedule, "every 10m");
        assert_eq!(instruction, "keep going");
        let Set {
            schedule,
            instruction,
            ..
        } = parse_heartbeat_command("/heartbeat --every=\"15 m\" watch things").unwrap()
        else {
            panic!("expected set");
        };
        assert_eq!(schedule, "every 15 m");
        assert_eq!(instruction, "watch things");
        let Set {
            schedule,
            instruction,
            ..
        } = parse_heartbeat_command("/heartbeat every 2h sweep logs").unwrap()
        else {
            panic!("expected set");
        };
        assert_eq!(schedule, "every 2h");
        assert_eq!(instruction, "sweep logs");
        let Set { delivery_mode, .. } =
            parse_heartbeat_command("/heartbeat --steer --every 5m ping").unwrap()
        else {
            panic!("expected set");
        };
        assert_eq!(delivery_mode, Some(DeliveryMode::Steer));
        let Set { delivery_mode, .. } =
            parse_heartbeat_command("/heartbeat --every 5m ping --follow-up").unwrap()
        else {
            panic!("expected set");
        };
        assert_eq!(delivery_mode, Some(DeliveryMode::FollowUp));
        let Set { delivery_mode, .. } =
            parse_heartbeat_command("/heartbeat --deliver=steer --every 5m ping").unwrap()
        else {
            panic!("expected set");
        };
        assert_eq!(delivery_mode, Some(DeliveryMode::Steer));
        let Set { delivery_mode, .. } =
            parse_heartbeat_command("/heartbeat --deliver follow_up --every 5m ping").unwrap()
        else {
            panic!("expected set");
        };
        assert_eq!(delivery_mode, Some(DeliveryMode::FollowUp));
        assert!(parse_heartbeat_command("/heartbeat --every 10m").is_err());
    }

    #[test]
    fn heartbeat_deferral_rules() {
        let job = |source: &str, mode: Option<DeliveryMode>| AgentCronJob {
            id: "j1".to_string(),
            status: JobStatus::Active,
            source: Some(source.to_string()),
            runtime_kind: Some("top-level".to_string()),
            delivery_mode: mode,
            active_session_id: "active".to_string(),
            session_id: "session".to_string(),
            session_file: "/s/file.jsonl".to_string(),
            cwd: "/w".to_string(),
            label: None,
            schedule: AgentCronSchedule {
                kind: ScheduleKind::Interval,
                expression: "every 5m".to_string(),
                interval_ms: Some(300_000),
            },
            created_at: String::new(),
            updated_at: String::new(),
            next_run_at: Some("2000-01-01T00:00:00Z".to_string()),
            last_run_at: None,
            last_skipped_at: None,
            run_count: 0,
            prompt: "ping".to_string(),
            last_error: None,
        };
        let idle = HeartbeatSessionActivity::default();
        let streaming = HeartbeatSessionActivity {
            is_streaming: true,
            ..Default::default()
        };
        assert!(!should_defer_heartbeat_cron_job(
            &job("cron", None),
            &streaming
        ));
        assert!(!should_defer_heartbeat_cron_job(
            &job("heartbeat", Some(DeliveryMode::Steer)),
            &streaming
        ));
        assert!(should_defer_heartbeat_cron_job(
            &job("heartbeat", Some(DeliveryMode::FollowUp)),
            &streaming
        ));
        let compacting = HeartbeatSessionActivity {
            is_compacting: true,
            is_streaming: true,
            ..Default::default()
        };
        assert!(should_defer_heartbeat_cron_job(
            &job("heartbeat", Some(DeliveryMode::Steer)),
            &compacting
        ));
        let unfinished = HeartbeatSessionActivity {
            unfinished_action_count: 2,
            ..Default::default()
        };
        assert!(should_defer_heartbeat_cron_job(
            &job("rlm_heartbeat", None),
            &unfinished
        ));
        assert!(!should_defer_heartbeat_cron_job(
            &job("heartbeat", None),
            &idle
        ));
    }

    #[test]
    fn due_jobs_and_formatting() {
        let mut job = AgentCronJob {
            id: "job1".to_string(),
            status: JobStatus::Active,
            source: Some("cron".to_string()),
            runtime_kind: Some("top-level".to_string()),
            delivery_mode: None,
            active_session_id: "active".to_string(),
            session_id: "session".to_string(),
            session_file: "/s/file.jsonl".to_string(),
            cwd: "/w".to_string(),
            created_at: String::new(),
            updated_at: String::new(),
            label: Some("sweep".to_string()),
            schedule: AgentCronSchedule {
                kind: ScheduleKind::Interval,
                expression: "every 5m".to_string(),
                interval_ms: Some(300_000),
            },
            next_run_at: Some("2000-01-01T00:00:00Z".to_string()),
            last_run_at: None,
            last_skipped_at: None,
            run_count: 3,
            prompt: "keep   working".to_string(),
            last_error: Some("boom".to_string()),
        };
        assert!(is_due_job(&job, 946_684_800_001));
        assert!(!is_due_job(&job, 946_684_799_999));
        job.status = JobStatus::Paused;
        assert!(!is_due_job(&job, 946_684_800_001));
        job.status = JobStatus::Active;
        let formatted = format_agent_cron_job(&job);
        assert!(formatted.starts_with("job1 active label=\"sweep\" next=2000-01-01T00:00:00Z"));
        assert!(
            formatted.contains("runs=3 schedule=\"every 5m\" prompt=\"keep working\" error=boom")
        );
    }
}
