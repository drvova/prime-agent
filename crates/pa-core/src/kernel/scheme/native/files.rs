//! `fs-read`, `fs-grep`, `fs-glob`, `fs-find`, and `fs-edit`: the session's
//! file tools, run in process by [`crate::fs_search`] and [`crate::fs_edit`].
//!
//! Each call's options hash becomes the payload those modules parse: option
//! names map to payload keys, an unknown option is refused instead of
//! ignored, and `path` resolves against the session's working directory.

use std::path::Path;

use anyhow::{anyhow, bail};
use serde_json::{json, Map, Value};

use crate::tools::path_utils::resolve_to_cwd;
use crate::{fs_edit, fs_search};

/// The options a tool accepts besides `path`: each one's Scheme name and the
/// payload key it fills.
type Accepted = &'static [(&'static str, &'static str)];

const GREP: Accepted = &[
    ("pattern", "pattern"),
    ("glob", "globs"),
    ("type", "types"),
    ("case", "case"),
    ("literal", "literal"),
    ("word", "word"),
    ("multiline", "multiline"),
    ("context", "context"),
    ("before", "before"),
    ("after", "after"),
    ("hidden", "hidden"),
    ("gitignore", "gitignore"),
    ("max-results", "maxResults"),
    ("max-per-file", "maxPerFile"),
    ("max-columns", "maxColumns"),
];

const GLOB: Accepted = &[
    ("pattern", "pattern"),
    ("hidden", "hidden"),
    ("gitignore", "gitignore"),
    ("max-results", "maxResults"),
];

const FIND: Accepted = &[
    ("name", "name"),
    ("kind", "kind"),
    ("ignore-case", "ignoreCase"),
    ("max-depth", "maxDepth"),
    ("hidden", "hidden"),
    ("gitignore", "gitignore"),
    ("max-results", "maxResults"),
];

const READ: Accepted = &[("start", "start"), ("end", "end")];

const EDIT: Accepted = &[("edits", "edits")];

/// The operations one edit may name, as Scheme spells them.
const EDIT_OPERATIONS: [&str; 5] = [
    "replace",
    "delete",
    "insert-after",
    "insert-before",
    "append",
];

/// Turn a call's options into the payload its tool parses: accepted options
/// under their payload keys, the absolute target under `path_key`, and `cwd`.
fn payload(
    options: &Value,
    accepted: Accepted,
    path_key: &str,
    cwd: &Path,
) -> anyhow::Result<Map<String, Value>> {
    let fields = options
        .as_object()
        .ok_or_else(|| anyhow!("options must be a hash"))?;
    let cwd = cwd.to_string_lossy();
    let mut payload = Map::new();
    let mut path = ".";
    for (name, value) in fields {
        if name == "path" {
            path = value
                .as_str()
                .filter(|path| !path.is_empty())
                .ok_or_else(|| anyhow!("path must be a non-empty string"))?;
        } else if let Some((_, key)) = accepted.iter().find(|(option, _)| option == name) {
            payload.insert((*key).to_string(), value.clone());
        } else {
            let names: Vec<&str> = accepted.iter().map(|(option, _)| *option).collect();
            bail!(
                "unknown option '{name}' (accepted: path, {})",
                names.join(", ")
            );
        }
    }
    payload.insert(path_key.to_string(), json!(resolve_to_cwd(path, &cwd)));
    payload.insert("cwd".to_string(), json!(cwd));
    Ok(payload)
}

/// `(fs-read path [options])`: a file's lines under their edit anchors.
///
/// # Errors
///
/// Bad options, a file that is missing, too large, or not text, or a range
/// past its end.
pub(super) fn read(cwd: &Path, options: &Value) -> anyhow::Result<Value> {
    let payload = Value::Object(payload(options, READ, "path", cwd)?);
    Ok(fs_edit::read(&fs_edit::parse_read(&payload)?)?.0)
}

/// `(fs-grep pattern [options])`: regex matches under a path.
///
/// # Errors
///
/// Bad options, an invalid pattern, glob, or type, or an unreadable root.
pub(super) fn grep(cwd: &Path, options: &Value) -> anyhow::Result<Value> {
    let mut payload = payload(options, GREP, "root", cwd)?;
    // `context` is both sides' default, as in `rg -C`.
    if let Some(context) = payload.remove("context") {
        for side in ["before", "after"] {
            payload
                .entry(side.to_string())
                .or_insert_with(|| context.clone());
        }
    }
    let request = fs_search::parse_grep(&Value::Object(payload))?;
    Ok(fs_search::grep(&request)?.to_json(request.max_results))
}

/// `(fs-glob pattern [options])`: paths matching a glob under a path.
///
/// # Errors
///
/// Bad options, an invalid glob, or an unreadable root.
pub(super) fn glob(cwd: &Path, options: &Value) -> anyhow::Result<Value> {
    let request = fs_search::parse_glob(&Value::Object(payload(options, GLOB, "root", cwd)?))?;
    Ok(fs_search::glob(&request)?.to_json(request.max_results))
}

/// `(fs-find [options])`: entries under a path by file name, kind, and depth.
///
/// # Errors
///
/// Bad options, an invalid name glob, or an unreadable root.
pub(super) fn find(cwd: &Path, options: &Value) -> anyhow::Result<Value> {
    let request = fs_search::parse_find(&Value::Object(payload(options, FIND, "root", cwd)?))?;
    Ok(fs_search::find(&request)?.to_json(request.max_results))
}

/// `(fs-edit path edits)`: one atomic patch of anchored edits.
///
/// # Errors
///
/// A malformed edit, a file that cannot be edited, or anchors that no longer
/// match the file: the error then lists them with the current lines, and
/// nothing is written.
pub(super) fn edit(cwd: &Path, options: &Value) -> anyhow::Result<Value> {
    let mut payload = payload(options, EDIT, "path", cwd)?;
    let edits = match payload.remove("edits") {
        Some(Value::Array(edits)) => edits,
        Some(one @ Value::Object(_)) => vec![one],
        Some(_) | None => bail!("edits must be a hash or a list of hashes"),
    };
    let mut wire = Vec::new();
    for (index, edit) in edits.iter().enumerate() {
        let number = index + 1;
        let fields = edit.as_object().ok_or_else(|| {
            anyhow!("edit {number} must be a hash like (hash 'replace \"12:abc\" 'text \"...\")")
        })?;
        let named: Vec<&str> = EDIT_OPERATIONS
            .into_iter()
            .filter(|operation| fields.contains_key(*operation))
            .collect();
        let [operation] = named[..] else {
            bail!(
                "edit {number} needs exactly one of {}",
                EDIT_OPERATIONS.join(", ")
            );
        };
        if let Some(unknown) = fields
            .keys()
            .find(|key| *key != operation && (operation == "append" || *key != "text"))
        {
            bail!("edit {number}: unknown key '{unknown}'");
        }
        let mut entry = Map::new();
        entry.insert("op".to_string(), json!(operation.replace('-', "_")));
        match (operation, &fields[operation]) {
            ("append", text) => {
                entry.insert("text".to_string(), text.clone());
            }
            ("replace" | "delete", Value::Array(range)) => {
                let [first, last] = &range[..] else {
                    bail!("edit {number}: a range is (list first-anchor last-anchor)");
                };
                entry.insert("anchor".to_string(), first.clone());
                entry.insert("end".to_string(), last.clone());
            }
            (_, anchor) => {
                entry.insert("anchor".to_string(), anchor.clone());
            }
        }
        if let Some(text) = fields.get("text") {
            entry.insert("text".to_string(), text.clone());
        }
        wire.push(Value::Object(entry));
    }
    payload.insert("edits".to_string(), Value::Array(wire));
    let (reply, _, _) = fs_edit::edit(&fs_edit::parse_edit(&Value::Object(payload))?)?;
    if reply["applied"] == json!(false) {
        bail!(
            "{}",
            reply["text"]
                .as_str()
                .unwrap_or("rejected: stale anchors; nothing was written")
        );
    }
    Ok(reply)
}
