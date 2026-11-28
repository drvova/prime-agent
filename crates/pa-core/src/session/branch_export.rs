//! The `/export <file>.jsonl` export (TS `exportToJsonl`): the session's
//! current branch re-chained into one linear JSONL file that resumes as a
//! straight session. Both daemons answer `export_jsonl` with it: the old
//! worker from its store and the runtime daemon's `session.export` from the
//! session's manager.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

/// Where an export writes: `output_path` as given when absolute, else under
/// the session's `cwd` (TS `resolve`); none names `session-<now>.jsonl`
/// there.
#[must_use]
pub fn output_file(cwd: &Path, output_path: Option<&str>) -> PathBuf {
    match output_path.map(Path::new) {
        Some(path) if path.is_absolute() => path.to_path_buf(),
        Some(path) => cwd.join(path),
        None => cwd.join(format!(
            "session-{}.jsonl",
            crate::session::manager::format_iso_now().replace([':', '.'], "-")
        )),
    }
}

/// The export's body: a fresh session header for `session_id` in `cwd`, then
/// the `branch` rows root to leaf, each one's `parentId` the row before it.
///
/// # Errors
///
/// A row does not serialize.
pub fn branch_body(session_id: &str, cwd: &str, branch: Vec<Value>) -> Result<String> {
    let header = json!({
        "type": "session",
        "version": crate::session::CURRENT_SESSION_VERSION,
        "id": session_id,
        "timestamp": crate::session::manager::format_iso_now(),
        "cwd": cwd,
    });
    let mut lines = vec![serde_json::to_string(&header)?];
    let mut previous: Option<String> = None;
    for mut value in branch {
        if let Some(map) = value.as_object_mut() {
            map.insert(
                "parentId".to_string(),
                previous.clone().map_or(Value::Null, Value::String),
            );
        }
        previous = value.get("id").and_then(Value::as_str).map(str::to_string);
        lines.push(serde_json::to_string(&value)?);
    }
    Ok(format!("{}\n", lines.join("\n")))
}

/// Writes one export's `body` to `file_path`, creating its directory, and
/// returns the path as the reply carries it (`{path}`). A file is replaced
/// whole, so a failed export leaves the previous one as it was; a pipe or a
/// device (`/dev/stdout`) cannot be replaced and is written through.
///
/// # Errors
///
/// The directory or the file cannot be written.
pub fn write(file_path: &Path, body: &str) -> Result<String> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    if std::fs::metadata(file_path).is_ok_and(|metadata| !metadata.is_file()) {
        std::fs::write(file_path, body)
    } else {
        pa_types::platform::replace_file(
            file_path,
            body.as_bytes(),
            pa_types::platform::ReplaceOptions {
                durability: pa_types::platform::Durability::Unsynced,
                mode: pa_types::platform::FileMode::KeepTarget,
                symlinks: pa_types::platform::Symlinks::FollowToTarget,
            },
        )
    }
    .with_context(|| format!("writing {}", file_path.display()))?;
    Ok(file_path.to_string_lossy().into_owned())
}
