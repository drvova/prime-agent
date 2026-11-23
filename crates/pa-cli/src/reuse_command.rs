//! `prime-agent reuse`: which stored skills and global harness entries
//! later sessions reuse, and the user's prune of the ones they pick.
//!
//! pa-core's reuse ledger (`pa_core::reuse`) owns the counts, the listing
//! and the removal; this command resolves the skills a session in the
//! directory would see, prints, and reports its one telemetry event.

use pa_core::resources::{load_resources, ResourceLoaderOptions};
use pa_core::reuse::{ReuseRow, Standing};

use crate::config::get_agent_dir;

/// The parsed `prime-agent reuse` invocation.
#[derive(Debug, PartialEq, Eq)]
enum ReuseCommand {
    List {
        cwd: Option<String>,
        json: bool,
    },
    Prune {
        keys: Vec<String>,
        cwd: Option<String>,
        json: bool,
    },
}

/// Parse the subcommand and its flags; `Err` carries a usage hint, empty
/// when no subcommand was given.
fn parse_reuse_command(args: &[String]) -> Result<ReuseCommand, String> {
    let Some(subcommand) = args.first() else {
        return Err(String::new());
    };
    let mut json = false;
    let mut cwd = None;
    let mut operands: Vec<String> = Vec::new();
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--cwd" => {
                let Some(value) = rest.next() else {
                    return Err("--cwd requires a directory".to_string());
                };
                cwd = Some(value.clone());
            }
            other if other.starts_with('-') => return Err(format!("unknown option {other:?}")),
            other => operands.push(other.to_string()),
        }
    }
    match subcommand.as_str() {
        "list" if operands.is_empty() => Ok(ReuseCommand::List { cwd, json }),
        "list" => Err("Usage: prime-agent reuse list [--cwd <dir>] [--json]".to_string()),
        "prune" if !operands.is_empty() => Ok(ReuseCommand::Prune {
            keys: operands,
            cwd,
            json,
        }),
        "prune" => {
            Err("Usage: prime-agent reuse prune <key>... [--cwd <dir>] [--json]".to_string())
        }
        other => {
            let children: Vec<&str> = crate::command_registry::get_child_command_specs(&["reuse"])
                .into_iter()
                .map(|spec| spec.path[spec.path.len() - 1])
                .collect();
            let suggestion = crate::command_registry::find_command_suggestion(other, &children);
            Err(format!(
                "Unknown reuse command: {other}{}",
                suggestion.map_or_else(String::new, |s| format!(" (did you mean \"{s}\"?)"))
            ))
        }
    }
}

/// Report the command's `agent reuse listing` event, unless telemetry is
/// off. It runs after the output is printed, so it never delays it.
fn track(cwd: &std::path::Path, action: &str, rows: &[ReuseRow], pruned: usize) {
    let agent_dir = get_agent_dir();
    let settings = pa_core::settings::SettingsManager::create(cwd, &agent_dir);
    if crate::mode::telemetry_disabled(&settings) {
        return;
    }
    let count = |standing: Standing| rows.iter().filter(|row| row.standing == standing).count();
    let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };
    runtime.block_on(async {
        let client = pa_core::session_engine::telemetry::build_client(&settings, &agent_dir);
        let mut properties = pa_telemetry::base_properties("cli");
        properties.set("action", serde_json::Value::from(action));
        properties.set("entries", serde_json::Value::from(rows.len()));
        properties.set("reused", serde_json::Value::from(count(Standing::Reused)));
        properties.set(
            "used_once",
            serde_json::Value::from(count(Standing::UsedOnce)),
        );
        properties.set("unused", serde_json::Value::from(count(Standing::Unused)));
        properties.set("pruned", serde_json::Value::from(pruned));
        client.track("agent reuse listing", properties);
        let _ = client.shutdown().await;
    });
}

/// Run the `reuse` command; returns the process exit code.
pub fn run_reuse_command(args: &[String]) -> i32 {
    let command = match parse_reuse_command(args) {
        Ok(command) => command,
        Err(hint) => {
            if hint.is_empty() {
                eprintln!("Missing reuse command.");
            } else {
                eprintln!("Error: {hint}");
            }
            eprintln!("Run `prime-agent help reuse` for usage.");
            return 1;
        }
    };
    let (cwd, json) = match &command {
        ReuseCommand::List { cwd, json } | ReuseCommand::Prune { cwd, json, .. } => {
            (cwd.clone(), *json)
        }
    };
    let cwd = cwd
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let agent_dir = get_agent_dir();
    let skills = match load_resources(ResourceLoaderOptions::new(cwd.clone(), agent_dir.clone())) {
        Ok(resources) => resources.skills,
        Err(error) => {
            eprintln!("Error: the skills could not be read: {error:#}");
            return 1;
        }
    };
    match command {
        ReuseCommand::List { .. } => {
            let rows = pa_core::reuse::report(&agent_dir, &skills, None);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&rows).unwrap_or_default()
                );
            } else if rows.is_empty() {
                println!("No installed skills or global harness entries.");
            } else {
                println!("Counts leave out the session that created each entry.");
                println!(
                    "{:<10} {:>8} {:>6} {:>5} {:>6}  {:<24}  key",
                    "standing", "used/off", "uses", "own", "tokens", "last used"
                );
                for row in &rows {
                    println!(
                        "{:<10} {:>8} {:>6} {:>5} {:>6}  {:<24}  {}{}",
                        row.standing.label(),
                        format!("{}/{}", row.used_sessions, row.offered_sessions),
                        row.uses,
                        row.own_uses,
                        row.prompt_tokens,
                        row.last_used_at.as_deref().unwrap_or("never"),
                        row.key,
                        if row.removable {
                            ""
                        } else {
                            "  (not prunable here)"
                        },
                    );
                }
            }
            track(&cwd, "list", &rows, 0);
            0
        }
        ReuseCommand::Prune { keys, .. } => {
            match pa_core::reuse::prune(&agent_dir, &skills, &keys) {
                Ok(pruned) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&pruned).unwrap_or_default()
                        );
                    } else {
                        for removed in &pruned {
                            println!("Removed {} ({})", removed.key, removed.removed);
                        }
                    }
                    let rows = pa_core::reuse::report(&agent_dir, &skills, None);
                    track(&cwd, "prune", &rows, pruned.len());
                    0
                }
                Err(error) => {
                    eprintln!("Error: {error:#}");
                    1
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(words: &[&str]) -> Vec<String> {
        words.iter().map(|word| (*word).to_string()).collect()
    }

    #[test]
    fn the_subcommands_parse_with_their_flags() {
        assert_eq!(
            [
                parse_reuse_command(&args(&["list", "--json", "--cwd", "/p"])),
                parse_reuse_command(&args(&["prune", "skill/a", "harness/memory/b"])),
                parse_reuse_command(&args(&["prune"])),
                parse_reuse_command(&args(&["lst"])),
                parse_reuse_command(&args(&[])),
            ],
            [
                Ok(ReuseCommand::List {
                    cwd: Some("/p".to_string()),
                    json: true
                }),
                Ok(ReuseCommand::Prune {
                    keys: args(&["skill/a", "harness/memory/b"]),
                    cwd: None,
                    json: false
                }),
                Err("Usage: prime-agent reuse prune <key>... [--cwd <dir>] [--json]".to_string()),
                Err("Unknown reuse command: lst (did you mean \"list\"?)".to_string()),
                Err(String::new()),
            ]
        );
    }
}
