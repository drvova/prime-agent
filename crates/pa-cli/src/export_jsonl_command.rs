//! `export-jsonl`: write saved sessions out as JSONL session files (the v3
//! format), byte for byte what the transcript store holds: the way back to
//! the files a build before the SQLite store reads.

use std::path::PathBuf;

use pa_core::session::store::{export_jsonl, session_trees, ExportReport, ExportSelection};

use crate::config::COMMAND_NAME;

/// The command's arguments.
struct Request {
    session: Option<String>,
    out: PathBuf,
}

fn parse(args: &[String]) -> Option<Request> {
    let mut session = None;
    let mut all = false;
    let mut out = None;
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--session" => session = Some(rest.next()?.clone()),
            "--all" => all = true,
            "--out" => out = Some(PathBuf::from(rest.next()?)),
            _ => return None,
        }
    }
    match (session, all) {
        (Some(session), false) => Some(Request {
            session: Some(session),
            out: out?,
        }),
        (None, true) => Some(Request {
            session: None,
            out: out?,
        }),
        (Some(_), true) | (None, false) => None,
    }
}

/// Run the command; the exit code.
pub(crate) fn run_export_jsonl_command(args: &[String]) -> i32 {
    let Some(request) = parse(args) else {
        eprintln!("Usage: {COMMAND_NAME} export-jsonl (--session <id> | --all) --out <dir>");
        return 2;
    };
    let agent_dir = crate::config::get_agent_dir();
    let selection = match &request.session {
        Some(session) => ExportSelection::Session(session),
        None => ExportSelection::All,
    };
    let report = match export_jsonl(
        &session_trees(&agent_dir),
        &agent_dir,
        selection,
        &request.out,
    ) {
        Ok(report) => report,
        Err(error) => {
            eprintln!("Error: {error}");
            return 1;
        }
    };
    for (locator, error) in &report.failed {
        eprintln!("Error: {}: {error}", locator.display());
    }
    println!(
        "Exported {} session file(s) to {} ({} already there, {} failed).",
        report.written.len(),
        request.out.display(),
        report.unchanged.len(),
        report.failed.len()
    );
    track(&agent_dir, &request, &report);
    i32::from(!report.failed.is_empty())
}

/// The command's `agent sessions exported` event, after the export (it never
/// delays it), unless telemetry is off.
fn track(agent_dir: &std::path::Path, request: &Request, report: &ExportReport) {
    let cwd = std::env::current_dir().unwrap_or_default();
    let settings = pa_core::settings::SettingsManager::create(&cwd, agent_dir);
    if crate::mode::telemetry_disabled(&settings) {
        return;
    }
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(async {
        let client = pa_core::session_engine::telemetry::build_client(&settings, agent_dir);
        pa_core::session_engine::telemetry::track_sessions_exported(
            &client,
            if request.session.is_some() {
                "session"
            } else {
                "all"
            },
            report,
        );
        let _ = client.shutdown().await;
    });
}
