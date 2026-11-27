//! The whole-session replacements both daemons run, where a session goes on
//! under the same id on another transcript: `fork {entryId, position}` (the
//! session's path to an entry copied into a new transcript) and
//! `import_jsonl` (a transcript file copied into the sessions directory).
//! The old worker (`pa-daemon` `branch_navigation.rs`,
//! `session_navigation.rs`) and the runtime daemon (`pa-runtime`
//! `daemon/handlers/replace.rs`) both apply these rules: where a fork
//! starts and the text it gives back, where an import is copied to, and
//! the working directory the replacement runs in, with the errors a client
//! acts on.

use std::path::{Path, PathBuf};

use pa_types::daemon::DaemonErrorInfo;
use serde_json::{json, Value};

/// A replacement refused before anything was replaced: the error's message,
/// and what a client can act on beyond it (the old response's `errorInfo`).
#[derive(Debug, Clone, PartialEq)]
pub struct ReplacementRefused {
    pub message: String,
    pub info: Option<DaemonErrorInfo>,
}

impl ReplacementRefused {
    fn plain(message: &str) -> Self {
        ReplacementRefused {
            message: message.to_string(),
            info: None,
        }
    }
}

impl std::fmt::Display for ReplacementRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ReplacementRefused {}

/// Where a fork's new transcript ends, and the text it hands back to the
/// editor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkPoint {
    /// The entry the copied path ends at; `None` forks at the root (a new
    /// transcript with no entries).
    pub leaf: Option<String>,
    /// The forked-from user message's text (`selectedText`), for a fork
    /// before it.
    pub selected_text: Option<String>,
}

impl ForkPoint {
    /// Where a `fork` onto the entry `target` (its stored row, `None` when
    /// the session has no such entry) starts: `position` `"at"` keeps the
    /// entry and everything before it; any other position forks before a
    /// user message, which is handed back.
    ///
    /// # Errors
    ///
    /// The entry is missing, or a fork before it is asked and it is not a
    /// user message: `Invalid entry ID for forking`.
    pub fn of(target: Option<&Value>, position: Option<&str>) -> Result<Self, ReplacementRefused> {
        let invalid = || ReplacementRefused::plain("Invalid entry ID for forking");
        let target = target.ok_or_else(invalid)?;
        if position == Some("at") {
            return Ok(ForkPoint {
                leaf: target["id"].as_str().map(str::to_string),
                selected_text: None,
            });
        }
        let text = super::navigation::user_row_text(target).ok_or_else(invalid)?;
        Ok(ForkPoint {
            leaf: target["parentId"].as_str().map(str::to_string),
            selected_text: Some(text),
        })
    }

    /// The `fork` reply once the session runs on its fork: `{cancelled:
    /// false, selectedText?}`.
    #[must_use]
    pub fn reply(&self) -> Value {
        let mut reply = json!({ "cancelled": false });
        if let Some(text) = &self.selected_text {
            reply["selectedText"] = json!(text);
        }
        reply
    }
}

/// Where `import_jsonl` copies the file at `input` (`inputPath`): into
/// `session_dir`, the sessions directory of the session it replaces, under
/// its own name.
///
/// # Errors
///
/// `input` is not a file: `File not found: <input>`, with the
/// `session_import_file_not_found` info; else the session keeps no
/// transcript (`session_dir` is `None`): `Session is still initializing`.
pub fn import_target(
    input: &Path,
    session_dir: Option<&Path>,
) -> Result<PathBuf, ReplacementRefused> {
    if !input.is_file() {
        return Err(ReplacementRefused {
            message: format!("File not found: {}", input.display()),
            info: Some(DaemonErrorInfo::SessionImportFileNotFound {
                file_path: input.display().to_string(),
            }),
        });
    }
    let session_dir =
        session_dir.ok_or_else(|| ReplacementRefused::plain("Session is still initializing"))?;
    Ok(session_dir.join(input.file_name().map_or_else(
        || "imported.jsonl".to_string(),
        |name| name.to_string_lossy().into_owned(),
    )))
}

/// The working directory a session replaced by the transcript at `path`
/// runs in: `cwd_override` when given, else the transcript's own
/// (`header_cwd`); `None` keeps the session's, `fallback`.
///
/// # Errors
///
/// That directory does not exist: the missing-cwd message, with the
/// `missing_session_cwd` info a client offers `fallback` from.
pub fn replacement_cwd(
    path: &Path,
    header_cwd: &str,
    cwd_override: Option<String>,
    fallback: &Path,
) -> Result<Option<String>, ReplacementRefused> {
    let cwd = cwd_override.or_else(|| (!header_cwd.is_empty()).then(|| header_cwd.to_string()));
    if let Some(cwd) = cwd.as_deref() {
        if !Path::new(cwd).is_dir() {
            let (path, fallback) = (path.display(), fallback.display());
            return Err(ReplacementRefused {
                message: format!(
                    "Stored session working directory does not exist: {cwd}\nSession file: {path}\nCurrent working directory: {fallback}"
                ),
                info: Some(DaemonErrorInfo::MissingSessionCwd {
                    issue: json!({
                        "sessionFile": path.to_string(),
                        "sessionCwd": cwd,
                        "fallbackCwd": fallback.to_string(),
                    }),
                }),
            });
        }
    }
    Ok(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fork_starts_by_its_position_and_target() {
        let user = json!({ "type": "message", "id": "u", "parentId": "p",
            "message": { "role": "user", "content": "again" } });
        let reply = json!({ "type": "message", "id": "r", "parentId": "u",
            "message": { "role": "assistant", "content": "x" } });
        let invalid = Err(ReplacementRefused::plain("Invalid entry ID for forking"));
        assert_eq!(
            [
                ForkPoint::of(Some(&user), None),
                ForkPoint::of(Some(&reply), Some("at")),
                ForkPoint::of(Some(&reply), Some("before")),
                ForkPoint::of(None, Some("at")),
            ],
            [
                Ok(ForkPoint {
                    leaf: Some("p".to_string()),
                    selected_text: Some("again".to_string()),
                }),
                Ok(ForkPoint {
                    leaf: Some("r".to_string()),
                    selected_text: None,
                }),
                invalid.clone(),
                invalid,
            ]
        );
    }

    #[test]
    fn an_import_needs_its_file_and_its_working_directory() {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("saved.jsonl");
        std::fs::write(&input, "").unwrap();
        let missing = dir.path().join("gone.jsonl");
        let gone = dir.path().join("gone");
        let issue = json!({ "sessionFile": input.display().to_string(),
            "sessionCwd": gone.display().to_string(), "fallbackCwd": "/fallback" });
        assert_eq!(
            (
                import_target(&input, Some(Path::new("/sessions"))),
                import_target(&missing, None).map_err(|refused| refused.info),
                replacement_cwd(&input, "", None, Path::new("/fallback")),
                replacement_cwd(
                    &input,
                    &gone.display().to_string(),
                    None,
                    Path::new("/fallback")
                )
                .map_err(|refused| refused.info),
                replacement_cwd(
                    &input,
                    &gone.display().to_string(),
                    Some(dir.path().display().to_string()),
                    Path::new("/fallback"),
                ),
            ),
            (
                Ok(PathBuf::from("/sessions/saved.jsonl")),
                Err(Some(DaemonErrorInfo::SessionImportFileNotFound {
                    file_path: missing.display().to_string(),
                })),
                Ok(None),
                Err(Some(DaemonErrorInfo::MissingSessionCwd { issue })),
                Ok(Some(dir.path().display().to_string())),
            )
        );
    }
}
