//! The daemon-backed command runner behind the public commands `list`,
//! `sessions`, `stop`, `rename`, `send`, and `schedule`: argument parsing,
//! request shaping, and output rendering. `schedule` maps to the internal
//! `cron` command and `stop` to `kill`.
//!
//! Each command speaks wire2 to the runtime daemon on `daemon2.sock` beside
//! the socket it was given, and renders its answers the way the old
//! supervisor's were rendered:
//!
//! | command | wire2 |
//! | --- | --- |
//! | `list`, `sessions` | query `sessions.list` (`{"open": true}` without `--all`) |
//! | `stop` | call `session.stop` |
//! | `rename` | call `session.rename` |
//! | `send` | call `session.send` (a saved session is woken for it) |
//! | `schedule list` | query `schedules` |
//! | `schedule add` | call `schedule.add` |
//! | `schedule cancel` | call `schedule.cancel` |
//!
//! The command resolves a session selector against `sessions.list` by the
//! old daemon's rules: an
//! open session by its id, name or an unambiguous id suffix, and, for
//! `send` only, a saved session by its exact id or name.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use pa_types::daemon::DaemonCommand;
use serde_json::{json, Value};

use crate::daemon_session_list::{
    format_session_list_table, get_session_summaries, matches_session_id_suffix,
};
use crate::runtime_daemon_client::RuntimeDaemonClient;

#[derive(Debug)]
struct ParsedDaemonCommand {
    socket_path: PathBuf,
    json: bool,
    positionals: Vec<String>,
    /// `--help`/`-h` becomes the internal no-op `help` command, exactly like the TS
    /// `runDaemonClientCommand` fallthrough.
    help: bool,
}

/// How far a session selector reaches.
#[derive(Clone, Copy)]
enum Reach {
    /// The open sessions only.
    Open,
    /// The open sessions, then the saved ones by exact id or name.
    OpenOrSaved,
}

/// Run one internal daemon command. Errors are printed by the public command
/// router exactly like the `handleDaemonCommand` catch in the TS CLI.
pub(crate) fn run_daemon_command(command: &str, args: &[String]) -> Result<()> {
    let parsed = parse_daemon_command(command, args)?;
    if parsed.help {
        return Ok(());
    }
    let mut daemon =
        RuntimeDaemonClient::connect(&pa_runtime::daemon::socket_path(&parsed.socket_path))?;
    match command {
        "list" => run_list(&mut daemon, &parsed.positionals, parsed.json),
        "sessions" => run_sessions(&mut daemon, &parsed.positionals, parsed.json),
        "kill" => run_kill(&mut daemon, &parsed.positionals, parsed.json),
        "rename" => run_rename(&mut daemon, &parsed.positionals, parsed.json),
        "send" => run_send(&mut daemon, &parsed.positionals, parsed.json),
        "cron" => run_cron(&mut daemon, &parsed.positionals, parsed.json),
        other => bail!("Unknown daemon command: {other}"),
    }
}

/// Option scanning with `--` passthrough semantics (`send`/`cron` keep the
/// separator as an operand, the others consume it).
fn parse_daemon_command(command: &str, args: &[String]) -> Result<ParsedDaemonCommand> {
    let mut socket_path = default_socket_path();
    let mut json = false;
    let mut positionals: Vec<String> = Vec::new();
    let mut passthrough = false;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        index += 1;
        if passthrough {
            positionals.push(arg.to_string());
            continue;
        }
        if arg == "--" {
            if command == "cron" || command == "send" {
                positionals.push(arg.to_string());
            }
            passthrough = true;
            continue;
        }

        if arg == "--help" || arg == "-h" {
            return Ok(ParsedDaemonCommand {
                socket_path,
                json,
                positionals,
                help: true,
            });
        }
        if arg == "--socket" || arg == "--daemon-socket" {
            let value = args
                .get(index)
                .ok_or_else(|| anyhow!("{arg} requires a value"))?;
            index += 1;
            socket_path = normalize_socket_path(value);
            continue;
        }
        if arg == "--json" {
            json = true;
            continue;
        }
        positionals.push(arg.to_string());
    }
    Ok(ParsedDaemonCommand {
        socket_path,
        json,
        positionals,
        help: false,
    })
}

fn default_socket_path() -> PathBuf {
    crate::config::resolve_daemon_socket_path(None)
}

/// Lexically resolve against the current directory.
fn normalize_socket_path(value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    }
}

fn print_json(value: &Value) {
    match serde_json::to_string_pretty(value) {
        Ok(text) => println!("{text}"),
        Err(error) => eprintln!("Error: {error}"),
    }
}

fn require_active_session_id(args: &[String]) -> Result<String> {
    args.first()
        .cloned()
        .ok_or_else(|| anyhow!("Missing agent id or name"))
}

fn run_list(daemon: &mut RuntimeDaemonClient, args: &[String], json: bool) -> Result<()> {
    let mut all = false;
    for arg in args {
        if arg == "-a" || arg == "--all" {
            all = true;
            continue;
        }
        bail!("Unknown list option: {arg}");
    }
    let data = daemon.query("sessions.list", json!({ "open": !all }))?;
    if json {
        print_json(&data);
        return Ok(());
    }
    let Some(sessions) = get_session_summaries(&data) else {
        print_json(&data);
        return Ok(());
    };
    if sessions.is_empty() {
        println!(
            "{}",
            if all {
                "No agents."
            } else {
                "No active agents."
            }
        );
        return Ok(());
    }
    println!("{}", format_session_list_table(&sessions));
    Ok(())
}

/// The same list RPC as `prime-agent list`, rendered as the
/// one-line-per-agent operator table.
fn run_sessions(daemon: &mut RuntimeDaemonClient, args: &[String], json: bool) -> Result<()> {
    let mut all = false;
    for arg in args {
        if arg == "-a" || arg == "--all" {
            all = true;
            continue;
        }
        bail!("Unknown sessions option: {arg}");
    }
    let data = daemon.query("sessions.list", json!({ "open": !all }))?;
    if json {
        print_json(&data);
        return Ok(());
    }
    let Some(sessions) = crate::daemon_session_list::validated_session_values(&data) else {
        print_json(&data);
        return Ok(());
    };
    if sessions.is_empty() {
        println!(
            "{}",
            if all {
                "No agents."
            } else {
                "No active agents."
            }
        );
        return Ok(());
    }
    println!(
        "{}",
        crate::sessions_table_format::format_sessions_table(
            &sessions,
            crate::daemon_session_list::now_ms()
        )
    );
    Ok(())
}

/// The session `selector` names: an open one by the
/// old daemon's live rules, then (`reach`) a saved one by exact id or name.
fn resolve_runtime_session(
    daemon: &mut RuntimeDaemonClient,
    selector: &str,
    reach: Reach,
) -> Result<String> {
    let data = daemon.query(
        "sessions.list",
        json!({ "open": matches!(reach, Reach::Open) }),
    )?;
    let rows = data
        .get("sessions")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("Daemon returned an invalid list response"))?;
    let (open, saved): (Vec<&Value>, Vec<&Value>) = rows
        .iter()
        .partition(|row| row.get("open").and_then(Value::as_bool) == Some(true));
    if let Some(id) = match_live_session(&open, selector)? {
        return Ok(id);
    }
    let unknown = || anyhow!("Unknown active session: {selector}");
    match reach {
        Reach::Open => return Err(unknown()),
        Reach::OpenOrSaved => {}
    }
    let named: Vec<&str> = saved
        .iter()
        .filter(|row| {
            row.get("sessionId").and_then(Value::as_str) == Some(selector)
                || row.get("sessionName").and_then(Value::as_str) == Some(selector)
        })
        .filter_map(|row| row.get("sessionId").and_then(Value::as_str))
        .collect();
    match named.as_slice() {
        [id] => Ok((*id).to_string()),
        [] => Err(unknown()),
        _ => Err(anyhow!("Ambiguous active session \"{selector}\"")),
    }
}

/// The active id of the one live session `selector` names: by active id,
/// session id or name, else by an unambiguous hex suffix of either id;
/// `None` when none matches.
fn match_live_session(sessions: &[&Value], selector: &str) -> Result<Option<String>> {
    let mut exact: Vec<String> = Vec::new();
    let mut suffix: Vec<String> = Vec::new();
    for session in sessions {
        let Some(active_session_id) = session.get("activeSessionId").and_then(Value::as_str) else {
            continue;
        };
        let session_id = session.get("sessionId").and_then(Value::as_str);
        let session_name = session.get("sessionName").and_then(Value::as_str);
        if active_session_id == selector
            || session_id == Some(selector)
            || session_name == Some(selector)
        {
            exact.push(active_session_id.to_string());
            continue;
        }
        if matches_session_id_suffix(active_session_id, selector)
            || session_id.is_some_and(|id| matches_session_id_suffix(id, selector))
        {
            suffix.push(active_session_id.to_string());
        }
    }
    let matches = if exact.is_empty() { suffix } else { exact };
    match matches.as_slice() {
        [id] => Ok(Some(id.clone())),
        [] => Ok(None),
        _ => Err(anyhow!("Ambiguous active session \"{selector}\"")),
    }
}

fn run_kill(daemon: &mut RuntimeDaemonClient, args: &[String], json: bool) -> Result<()> {
    let active_session_id = require_active_session_id(args)?;
    let id = resolve_runtime_session(daemon, &active_session_id, Reach::Open)?;
    let result = daemon.call("session.stop", json!({ "session": id }))?;
    if json {
        print_json(&result);
    } else {
        println!("ok");
    }
    Ok(())
}

fn run_rename(daemon: &mut RuntimeDaemonClient, args: &[String], json: bool) -> Result<()> {
    let active_session_id = require_active_session_id(args)?;
    let name = args[1..].join(" ").trim().to_string();
    if name.is_empty() {
        bail!("Usage: prime-agent rename <agent> <name>");
    }
    let id = resolve_runtime_session(daemon, &active_session_id, Reach::Open)?;
    let result = daemon.call("session.rename", json!({ "session": id, "name": name }))?;
    if json {
        print_json(&result);
    } else {
        println!("Renamed {id} to {name}");
    }
    Ok(())
}

fn run_send(daemon: &mut RuntimeDaemonClient, args: &[String], json: bool) -> Result<()> {
    let request = parse_send_args(args)?;
    let DaemonCommand::SendMessage {
        target_active_session_id,
        message,
        from_active_session_id,
        delivery_mode,
        ..
    } = request
    else {
        bail!("invalid send request");
    };
    let mut call = json!({
        "session": resolve_runtime_session(
            daemon,
            &target_active_session_id,
            Reach::OpenOrSaved,
        )?,
        "message": message,
        "client": daemon.client_name(),
    });
    if let Some(from) = from_active_session_id {
        call["from"] = json!(resolve_runtime_session(daemon, &from, Reach::Open)?);
    }
    if let Some(delivery) = delivery_mode {
        call["delivery"] = delivery;
    }
    let data = daemon.call("session.send", call)?;
    if json {
        print_json(&data);
        return Ok(());
    }
    // A target block with a routable active id.
    let receipt_target = data
        .get("target")
        .filter(|target| {
            target
                .get("activeSessionId")
                .and_then(Value::as_str)
                .is_some()
        })
        .and_then(|target| {
            target
                .get("sessionName")
                .and_then(Value::as_str)
                .or_else(|| target.get("activeSessionId").and_then(Value::as_str))
        })
        .map(str::to_string);
    let Some(target) = receipt_target else {
        println!("ok");
        return Ok(());
    };
    let queued = data.get("deliveryStatus").and_then(Value::as_str) == Some("queued");
    println!(
        "{}",
        if queued {
            format!("Queued for {target}")
        } else {
            format!("Sent to {target}")
        }
    );
    Ok(())
}

fn parse_send_args(args: &[String]) -> Result<DaemonCommand> {
    let mut from_active_session_id: Option<String> = None;
    let mut delivery_mode: Option<&str> = None;
    let mut target_active_session_id: Option<String> = None;
    let mut explicit_message: Option<String> = None;
    let mut message_parts: Vec<String> = Vec::new();
    let mut parse_options = true;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        index += 1;
        if parse_options && arg == "--" {
            parse_options = false;
            continue;
        }
        if parse_options && arg == "--from" {
            let value = args
                .get(index)
                .ok_or_else(|| anyhow!("--from requires a session id or name"))?;
            index += 1;
            from_active_session_id = Some(value.clone());
            continue;
        }
        if parse_options && arg == "--steer" {
            delivery_mode = Some("steer");
            continue;
        }
        if parse_options && arg == "--follow-up" {
            delivery_mode = Some("follow_up");
            continue;
        }
        if parse_options && arg == "--message" {
            if target_active_session_id.is_none() {
                bail!("--message must appear after the target session");
            }
            let value = args
                .get(index)
                .ok_or_else(|| anyhow!("--message requires message text"))?;
            index += 1;
            explicit_message = Some(value.clone());
            parse_options = false;
            continue;
        }
        if parse_options && arg.starts_with("--") {
            bail!("Unknown option for send: {arg} (use -- before message text starting with --)");
        }
        if target_active_session_id.is_none() {
            target_active_session_id = Some(arg.to_string());
            continue;
        }
        message_parts.push(arg.to_string());
    }
    if explicit_message.is_some() && !message_parts.is_empty() {
        bail!("Usage: prime-agent send [--from <agent>] <agent> [--message <message>|<message>]");
    }
    let message = explicit_message
        .unwrap_or_else(|| message_parts.join(" "))
        .trim()
        .to_string();
    if target_active_session_id.is_none() || message.is_empty() {
        bail!("Usage: prime-agent send [--from <agent>] <agent> [--message <message>|<message>]");
    }
    Ok(DaemonCommand::SendMessage {
        id: None,
        target_active_session_id: target_active_session_id.expect("checked above"),
        message,
        from_active_session_id,
        agent_origin: None,
        delivery_mode: delivery_mode.map(Value::from),
        rest: serde_json::Map::new(),
    })
}

fn run_cron(daemon: &mut RuntimeDaemonClient, args: &[String], json: bool) -> Result<()> {
    let subcommand = args.first().map_or("list", String::as_str);
    match subcommand {
        "list" => run_cron_list(daemon, args, json),
        "add" | "schedule" => run_cron_add(daemon, args, json),
        "cancel" | "delete" | "remove" => run_cron_cancel(daemon, args, json),
        other => bail!("Unknown schedule command: {other}"),
    }
}

fn run_cron_list(daemon: &mut RuntimeDaemonClient, args: &[String], json: bool) -> Result<()> {
    let include_inactive = args.iter().skip(1).any(|arg| arg == "--all" || arg == "-a");
    let selector = args
        .iter()
        .skip(1)
        .find(|arg| !arg.starts_with('-') && arg.as_str() != "list");
    let session = match selector {
        Some(selector) => Some(resolve_runtime_session(daemon, selector, Reach::Open)?),
        None => None,
    };
    let data = daemon.query(
        "schedules",
        json!({ "session": session, "all": include_inactive }),
    )?;
    if json {
        print_json(&data);
        return Ok(());
    }
    let Some(jobs) = data.get("jobs").and_then(Value::as_array) else {
        print_json(&data);
        return Ok(());
    };
    if jobs.is_empty() {
        println!("No scheduled prompts.");
        return Ok(());
    }
    for job in jobs {
        println!("{}", format_agent_cron_job(job));
    }
    Ok(())
}

fn run_cron_add(daemon: &mut RuntimeDaemonClient, args: &[String], json: bool) -> Result<()> {
    let usage = "Usage: prime-agent schedule add <agent> <schedule> -- <message>";
    let separator = args
        .iter()
        .position(|arg| arg == "--")
        .ok_or_else(|| anyhow!("{usage}"))?;
    let Some(active_session_id) = args.get(1) else {
        bail!("{usage}");
    };
    if separator < 2 {
        bail!("{usage}");
    }
    let schedule = args[2..separator].join(" ").trim().to_string();
    let message = args[separator + 1..].join(" ").trim().to_string();
    if schedule.is_empty() || message.is_empty() {
        bail!("{usage}");
    }
    let session = resolve_runtime_session(daemon, active_session_id, Reach::Open)?;
    let data = daemon.call(
        "schedule.add",
        json!({ "session": session, "schedule": schedule, "prompt": message }),
    )?;
    if json {
        print_json(&data);
        return Ok(());
    }
    match cron_job_id_and_next_run(&data) {
        Some((id, next_run)) => println!("Scheduled {id} next={next_run}"),
        None => println!("Scheduled prompt."),
    }
    Ok(())
}

fn run_cron_cancel(daemon: &mut RuntimeDaemonClient, args: &[String], json: bool) -> Result<()> {
    let Some(job_id) = args.get(1) else {
        bail!("Usage: prime-agent schedule cancel <job-id>");
    };
    let data = daemon.call("schedule.cancel", json!({ "job": job_id }))?;
    if json {
        print_json(&data);
        return Ok(());
    }
    match cron_job_id_and_next_run(&data) {
        Some((id, _)) => println!("Cancelled {id}"),
        None => println!("Cancelled cron job."),
    }
    Ok(())
}

/// The job id and next run when the response carries a job.
fn cron_job_id_and_next_run(data: &Value) -> Option<(String, String)> {
    let job = data.get("job")?;
    let id = job.get("id").and_then(Value::as_str)?.to_string();
    let next_run = job
        .get("nextRunAt")
        .and_then(Value::as_str)
        .map_or_else(|| "-".to_string(), str::to_string);
    Some((id, next_run))
}

fn format_agent_cron_job(job: &Value) -> String {
    let id = job.get("id").and_then(Value::as_str).unwrap_or_default();
    let status = job
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let label = job
        .get("label")
        .and_then(Value::as_str)
        .map_or_default(|label| format!(" label=\"{label}\""));
    let next = cron_datetime(job.get("nextRunAt"));
    let last = cron_datetime(job.get("lastRunAt"));
    let skipped = job
        .get("lastSkippedAt")
        .map_or_default(|value| format!(" skipped={}", cron_datetime(Some(value))));
    let run_count = job
        .get("runCount")
        .and_then(Value::as_f64)
        .unwrap_or_default();
    let expression = job
        .get("schedule")
        .and_then(|schedule| schedule.get("expression"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    let prompt: String = job
        .get("prompt")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let preview: String = prompt.chars().take(80).collect();
    let error = job
        .get("lastError")
        .and_then(Value::as_str)
        .map_or_default(|error| format!(" error={error}"));
    format!(
        "{id} {status}{label} next={next} last={last}{skipped} runs={run_count} schedule=\"{expression}\" prompt=\"{preview}\"{error}"
    )
}

/// `toLocaleString()` for a cron timestamp: en-US date with a 12-hour clock,
/// e.g. `9/16/2026, 6:36:59 PM`. Rendered in UTC.
fn cron_datetime(value: Option<&Value>) -> String {
    let Some(value) = value.and_then(Value::as_str) else {
        return "-".to_string();
    };
    let Some(ms) = crate::daemon_session_list::parse_iso_ms(value) else {
        return "-".to_string();
    };
    let seconds_of_day = (ms / 1000) % 86_400;
    let hour24 = seconds_of_day / 3600;
    let minute = (seconds_of_day % 3600) / 60;
    let second = seconds_of_day % 60;
    let (hour12, meridiem) = match hour24 % 12 {
        0 => (12, if hour24 < 12 { "AM" } else { "PM" }),
        other => (other, if hour24 < 12 { "AM" } else { "PM" }),
    };
    let days = (ms / 1000) / 86_400;
    let (year, month, day) = civil_from_days(days);
    format!("{month}/{day}/{year}, {hour12}:{minute:02}:{second:02} {meridiem}")
}

/// Civil date from days since the epoch (Howard Hinnant's algorithm).
fn civil_from_days(days: u64) -> (i64, u32, u32) {
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { y + 1 } else { y }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(std::string::ToString::to_string).collect()
    }

    // The socket-path/separator shapes are unix paths (the windows transport
    // is named pipes; the tilde/socket spellings do not exist there).
    #[cfg(unix)]
    #[test]
    fn parse_consumes_socket_json_and_separator() {
        let parsed =
            parse_daemon_command("list", &args(&["--json", "--socket", "/s", "-a"])).unwrap();
        assert_eq!(parsed.socket_path, PathBuf::from("/s"));
        assert!(parsed.json);
        assert_eq!(parsed.positionals, args(&["-a"]));

        let missing = parse_daemon_command("list", &args(&["--daemon-socket"])).unwrap_err();
        assert_eq!(missing.to_string(), "--daemon-socket requires a value");
    }

    #[test]
    fn separator_passes_through_only_for_send_and_cron() {
        let send = parse_daemon_command("send", &args(&["target", "--", "hi", "there"])).unwrap();
        assert_eq!(send.positionals, args(&["target", "--", "hi", "there"]));
        let list = parse_daemon_command("list", &args(&["--", "-a"])).unwrap();
        assert_eq!(list.positionals, args(&["-a"]));
    }

    fn send_request(list: &[&str]) -> Value {
        serde_json::to_value(parse_send_args(&args(list)).unwrap()).unwrap()
    }

    #[test]
    fn send_args_match_ts_usage_rules() {
        let parsed = send_request(&["--from", "me", "target", "hello", "there"]);
        assert_eq!(parsed["targetActiveSessionId"], "target");
        assert_eq!(parsed["fromActiveSessionId"], "me");
        assert_eq!(parsed["message"], "hello there");

        let flag = send_request(&["target", "--message", "queued"]);
        assert_eq!(flag["message"], "queued");

        let explicit_and_positional =
            parse_send_args(&args(&["target", "--message", "queued", "extra"]));
        assert!(explicit_and_positional.is_err());

        let unknown = parse_send_args(&args(&["target", "--bogus"]));
        assert!(unknown.is_err());

        let no_target = parse_send_args(&args(&["--message", "text"]));
        assert!(no_target.is_err());
    }

    #[test]
    fn send_steer_flag_requests_steer_delivery() {
        let request = send_request(&["--steer", "target", "hello"]);
        assert_eq!(request["deliveryMode"], "steer");
    }

    #[test]
    fn send_follow_up_flag_requests_follow_up_delivery() {
        let request = send_request(&["target", "--follow-up", "hello"]);
        assert_eq!(request["deliveryMode"], "follow_up");
    }

    #[test]
    fn cron_job_line_matches_captured_ts_golden() {
        let job = json!({
            "id": "0451e5a9-951c-457b-ae76-ea3142ea5d25",
            "status": "active",
            "nextRunAt": "2026-09-16T18:36:59.287Z",
            "runCount": 0,
            "schedule": { "expression": "every 5 minutes" },
            "prompt": "do   the thing",
        });
        assert_eq!(
            format_agent_cron_job(&job),
            "0451e5a9-951c-457b-ae76-ea3142ea5d25 active next=9/16/2026, 6:36:59 PM last=- runs=0 schedule=\"every 5 minutes\" prompt=\"do the thing\""
        );
    }
}
