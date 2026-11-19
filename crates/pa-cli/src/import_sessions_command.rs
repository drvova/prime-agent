//! `import-sessions`: run a distribution build's history import now, in the
//! foreground (again, past the marker; still merging, never overwriting), or
//! `--skip` it for good. The daemon runs the same import once, in the
//! background, on its own.

use pa_core::settings::{
    plan_official_history_import, skip_official_history_import, HistoryImportCheck,
    HistoryImportReport, HistoryImportRun,
};
use pa_types::platform::StateLayout;

use crate::config::COMMAND_NAME;

/// Run the command; the exit code.
pub(crate) fn run_import_sessions_command(args: &[String]) -> i32 {
    let skip = match args {
        [] => false,
        [flag] if flag == "--skip" => true,
        _ => {
            eprintln!("Usage: {COMMAND_NAME} import-sessions [--skip]");
            return 2;
        }
    };
    let agent_dir = crate::config::get_agent_dir();
    let Some(home) = pa_types::platform::home_dir() else {
        eprintln!("Error: the home directory could not be resolved.");
        return 1;
    };
    let official = StateLayout::Official.home_agent_dir(&home);
    if matches!(StateLayout::BUILD, StateLayout::Official)
        || agent_dir != StateLayout::BUILD.home_agent_dir(&home)
    {
        println!(
            "Nothing to import: {COMMAND_NAME} uses {} itself, and only a distribution build's own data dir imports from {}.",
            agent_dir.display(),
            official.display()
        );
        return 0;
    }
    if skip {
        return match skip_official_history_import(&agent_dir) {
            Ok(()) => {
                println!(
                    "The chat import from {} is off for {}; run `{COMMAND_NAME} import-sessions` to import anyway.",
                    official.display(),
                    agent_dir.display()
                );
                0
            }
            Err(error) => {
                eprintln!("Error: could not write the import marker: {error}");
                1
            }
        };
    }
    let plan = match plan_official_history_import(
        StateLayout::BUILD,
        &home,
        &agent_dir,
        HistoryImportRun::Again,
    ) {
        Ok(HistoryImportCheck::Pending(plan)) => plan,
        Ok(HistoryImportCheck::Busy) => {
            println!("An import is already running (the daemon starts one on its own); its chats appear in /resume when it is done.");
            return 0;
        }
        Ok(
            HistoryImportCheck::NothingToImport
            | HistoryImportCheck::NotApplicable
            | HistoryImportCheck::AlreadyImported,
        ) => {
            println!("Nothing to import from {}.", official.display());
            return 0;
        }
        Err(error) => {
            eprintln!("Error: could not read {}: {error}", official.display());
            return 1;
        }
    };
    println!(
        "Importing {} chat(s) ({}) from {} into {}...",
        plan.chats(),
        human_bytes(plan.bytes()),
        official.display(),
        agent_dir.display()
    );
    let started = std::time::Instant::now();
    let result = plan.run(&mut |_| {});
    let elapsed = started.elapsed().as_millis() as u64;
    let (code, report, outcome) = match result {
        Ok(report) => {
            println!(
                "Imported {} chat(s) ({} file(s), {}) into {}; /resume lists them.",
                report.chats_imported,
                report.files_imported,
                human_bytes(report.bytes_imported),
                agent_dir.display()
            );
            if !report.kept.is_empty() {
                println!(
                    "Kept {} file(s) already there as they are (never overwritten).",
                    report.kept.len()
                );
            }
            if !report.config_added.is_empty() {
                println!("Added configuration: {}.", report.config_added.join(", "));
            }
            if !report.config_skipped.is_empty() {
                println!(
                    "Not added (over the size budget, or created meanwhile; copy by hand if needed): {}.",
                    report.config_skipped.join(", ")
                );
            }
            (0, report, "completed")
        }
        Err(error) => {
            eprintln!(
                "Error: the import stopped ({error}); run `{COMMAND_NAME} import-sessions` again to finish it."
            );
            (1, HistoryImportReport::default(), "failed")
        }
    };
    // The command's `agent sessions imported` event, after the copy (it never delays it), unless
    // telemetry is off.
    let cwd = std::env::current_dir().unwrap_or_default();
    let settings = pa_core::settings::SettingsManager::create(&cwd, &agent_dir);
    if !crate::mode::telemetry_disabled(&settings) {
        if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            runtime.block_on(async {
                let client =
                    pa_core::session_engine::telemetry::build_client(&settings, &agent_dir);
                pa_core::session_engine::telemetry::track_sessions_imported(
                    &client, "command", outcome, &report, elapsed,
                );
                let _ = client.shutdown().await;
            });
        }
    }
    code
}

/// `12.3 MB`-style size.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    if value < 1024.0 {
        return format!("{bytes} B");
    }
    let mut unit = "B";
    for next in UNITS {
        if value < 1024.0 {
            break;
        }
        value /= 1024.0;
        unit = next;
    }
    format!("{value:.1} {unit}")
}
