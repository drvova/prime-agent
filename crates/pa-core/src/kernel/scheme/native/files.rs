//! The session's file tools: `fs-read`, `fs-grep`, `fs-glob`, `fs-find`,
//! `fs-symbol`, `fs-edit`, and `edit`.
//!
//! Every `fs-*` call is answered by the session's `fs.*` host request
//! handler, the one the Python kernel's `rlm.fs` client reached: a whole-file
//! re-read of a file the model has seen comes back as `unchanged since turn
//! N` or a diff (with `'seen` naming the state, the turn, and a handle to the
//! whole view), and each call is counted by the session's telemetry. A kernel
//! whose session registers no such handler answers from handlers of its own
//! over [`crate::fs_search`], [`crate::fs_edit`], and
//! [`crate::symbol_index`], which shape and count nothing.
//!
//! Each call's options hash becomes the payload those handlers parse: option
//! names map to payload keys, an unknown option is refused instead of
//! ignored, and `path` resolves against the session's working directory. A
//! call ends with its cell: a stopped cell does not wait for a long search.
//!
//! Beyond what `native.rs` lists:
//! - `(fs-symbol name [options])` returns where one identifier is defined and
//!   used: `'name`, `'definitions` and `'references` (each a hash of `'path`,
//!   `'line`, `'text`; a definition also `'kind` and `'handle`, its source for
//!   `ctx-page`), `'truncated`, `'partial` (the index is still building),
//!   `'text`. Option: `'max-references` (default 50).
//! - `(fs-edit path edits [options])` also takes a row of `fs-read`'s
//!   `'lines` (or of a hunk) wherever it takes an anchor. Its result carries
//!   `'check`: the edited file's check (`[check: ...]` and any diagnostics,
//!   also the last lines of `'text`), or `#f`. With `(hash 'on-stale
//!   'return)` a patch with stale anchors returns its rejection (`'applied`
//!   `#f`, `'stale`, `'lines`, `'text`) instead of raising.
//! - `(fs-anchor view line)` is the anchor of a 1-based line of an `fs-read`
//!   view.
//! - `(edit path old-str new-str)` replaces one exact, unique occurrence of a
//!   string: `'path`, `'start-line`, `'check`, `'text`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use serde_json::{json, Map, Value};

use super::edit_check::EditChecks;
use super::edit_replace;
use crate::fs_edit::FsEditHost;
use crate::fs_search::FsSearchHost;
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload, KernelDiffDisplay};
use crate::symbol_index::{register_symbol_handler, SymbolIndex};
use crate::tools::path_utils::resolve_to_cwd;
use crate::tools::tool_definition::AbortSignal;

/// The Scheme side of the file tools, defined in every session engine after
/// the native tools' `%native`.
pub(super) const TOOLS: &str = r#"
(define (%fs-options rest) (if (null? rest) (hash) (car rest)))

(define (fs-symbol name . rest) (%native "fs-symbol" (hash-insert (%fs-options rest) 'name name)))
(define (fs-edit path edits . rest)
  (%native "fs-edit" (hash-insert (hash-insert (%fs-options rest) 'path path) 'edits edits)))
(define (edit path old-str new-str)
  (%native "edit" (hash 'path path 'old-str old-str 'new-str new-str)))

(define (fs-anchor view line)
  (let ([rows (filter (lambda (row) (equal? (hash-ref row 'line) line)) (hash-ref view 'lines))])
    (if (null? rows)
        (error (string-append "fs-anchor: line " (number->string line) " is not in this view (lines "
                              (number->string (hash-ref view 'start)) "-"
                              (number->string (hash-ref view 'end)) ")"))
        (hash-ref (car rows) 'anchor))))
"#;

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

const EDIT: Accepted = &[("edits", "edits"), ("on-stale", "onStale")];
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

/// One session kernel's file tools.
pub(super) struct Files {
    cwd: PathBuf,
    runtime: tokio::runtime::Handle,
    /// The `fs.*` handlers that answer: the session's, and the kernel's own
    /// for each request the session registers none for.
    handlers: HostRequestHandlers,
    checks: EditChecks,
}

impl Files {
    /// The file tools of a kernel working in `cwd`, answered by `session`'s
    /// handlers on `runtime`. What the post-edit checks learn is kept under
    /// the session's `agent_dir`, and nowhere without one.
    pub(super) fn new(
        cwd: PathBuf,
        session: HostRequestHandlers,
        runtime: tokio::runtime::Handle,
        agent_dir: Option<PathBuf>,
    ) -> Self {
        let mut handlers = HostRequestHandlers::new();
        FsSearchHost { observer: None }.register(&mut handlers);
        FsEditHost {
            observer: None,
            seen: None,
        }
        .register(&mut handlers);
        // The session's index is the project's one index; only a kernel
        // without it opens its own.
        if session.get("fs.symbol").is_none() {
            register_symbol_handler(
                &mut handlers,
                SymbolIndex::for_project(&cwd, /*agent_dir*/ None),
                /*store*/ None,
                /*observer*/ None,
            );
        }
        handlers.merge(session.clone());
        let state_dir = agent_dir.map(|directory| directory.join("edit-checks"));
        Self {
            cwd,
            checks: EditChecks::new(runtime.clone(), session, state_dir),
            runtime,
            handlers,
        }
    }

    /// Send one `fs.*` host request and wait for its reply, or for `cell` to
    /// stop.
    fn request(&self, request: &str, payload: Value, cell: &AbortSignal) -> anyhow::Result<Value> {
        let handler = self
            .handlers
            .get(request)
            .ok_or_else(|| anyhow!("this session has no {request} handler"))?;
        let reply = handler(HostRequestPayload { data: payload });
        self.runtime
            .block_on(async {
                tokio::select! {
                    biased;
                    () = cell.cancelled() => Err(anyhow!("stopped with its cell")),
                    reply = reply => reply,
                }
            })
            .map_err(|error| {
                // The caller names the tool; the handler's own label would repeat it.
                let reason = format!("{error:#}");
                let label = format!("{request}: ");
                anyhow!("{}", reason.strip_prefix(&label).unwrap_or(&reason))
            })
    }

    /// Run the file tool `operation` with a call's `options`.
    ///
    /// # Errors
    ///
    /// Bad options, the tool's own failure (a missing, binary, or oversized
    /// file, an invalid pattern, a range past the file's end, anchors that no
    /// longer match: nothing is written then), or `cell` stopping first.
    pub(super) fn run(
        &self,
        operation: &str,
        cell: &AbortSignal,
        options: &Value,
    ) -> anyhow::Result<Value> {
        match operation {
            "fs-read" => self.request(
                "fs.read",
                Value::Object(payload(options, READ, "path", &self.cwd)?),
                cell,
            ),
            "fs-grep" => {
                let mut payload = payload(options, GREP, "root", &self.cwd)?;
                // `context` is both sides' default, as in `rg -C`.
                if let Some(context) = payload.remove("context") {
                    for side in ["before", "after"] {
                        payload
                            .entry(side.to_string())
                            .or_insert_with(|| context.clone());
                    }
                }
                self.request("fs.grep", Value::Object(payload), cell)
            }
            "fs-glob" => self.request(
                "fs.glob",
                Value::Object(payload(options, GLOB, "root", &self.cwd)?),
                cell,
            ),
            "fs-find" => self.request(
                "fs.find",
                Value::Object(payload(options, FIND, "root", &self.cwd)?),
                cell,
            ),
            "fs-symbol" => self.symbol(cell, options),
            "fs-edit" => self.edit(cell, options),
            "edit" => {
                let (file, mut reply) = edit_replace::replace(&self.cwd, options)?;
                // The edit shows on the running cell's card, as the Python
                // kernel's `edit` skill displayed it.
                if let Some(kernel) = crate::kernel::scheme::cell::current() {
                    let text = |name: &str| options[name].as_str().unwrap_or_default().to_string();
                    kernel.record(crate::kernel::scheme::cell::CellRecord::Diff(
                        KernelDiffDisplay {
                            path: file.to_string_lossy().into_owned(),
                            old_str: text("old-str"),
                            new_str: text("new-str"),
                            start_line: reply["startLine"].as_u64(),
                        },
                    ));
                }
                self.attach_check(&mut reply, &file, cell);
                Ok(reply)
            }
            unknown => Err(anyhow!("unknown file tool: {unknown}")),
        }
    }

    /// Run the check of `file`, just edited, and attach what it says to the
    /// edit's `reply`: as `check`, and as the last lines of `text`.
    fn attach_check(&self, reply: &mut Value, file: &Path, cell: &AbortSignal) {
        let check = self.checks.after_edit(&self.cwd, file, cell);
        if let Some(check) = &check {
            reply["text"] = json!(format!(
                "{}\n{check}",
                reply["text"].as_str().unwrap_or_default()
            ));
        }
        reply["check"] = json!(check);
    }

    /// `(fs-symbol name [options])`: where one identifier is defined and used.
    fn symbol(&self, cell: &AbortSignal, options: &Value) -> anyhow::Result<Value> {
        let fields = options
            .as_object()
            .ok_or_else(|| anyhow!("options must be a hash"))?;
        if let Some(unknown) = fields
            .keys()
            .find(|key| !matches!(key.as_str(), "name" | "max-references"))
        {
            bail!("unknown option '{unknown}' (accepted: max-references)");
        }
        let name = fields
            .get("name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty() && !name.chars().any(char::is_whitespace))
            .ok_or_else(|| {
                anyhow!("name must be one identifier, like \"SpillStore\" or \"load\"")
            })?;
        let mut payload = json!({ "name": name });
        if let Some(max) = fields.get("max-references") {
            payload["maxReferences"] = json!(max
                .as_u64()
                .filter(|max| *max >= 1)
                .ok_or_else(|| anyhow!("max-references must be an integer >= 1"))?);
        }
        let mut reply = self.request("fs.symbol", payload, cell)?;
        let rows = |key: &str| reply[key].as_array().cloned().unwrap_or_default();
        let (definitions, references) = (rows("definitions"), rows("references"));
        let mut lines = vec![format!(
            "{name}: {} definitions, {}{} references{}",
            definitions.len(),
            references.len(),
            if reply["truncated"] == json!(true) {
                "+"
            } else {
                ""
            },
            if reply["partial"] == json!(true) {
                " (partial: the index is still building)"
            } else {
                ""
            },
        )];
        let field = |row: &Value, key: &str| row[key].as_str().unwrap_or_default().to_string();
        for found in &definitions {
            let handle = found["handle"]
                .as_str()
                .map(|handle| format!(" · {handle}"))
                .unwrap_or_default();
            lines.push(format!(
                "def {}:{} [{}] {}{handle}",
                field(found, "path"),
                found["line"],
                field(found, "kind"),
                field(found, "text"),
            ));
        }
        for found in &references {
            lines.push(format!(
                "ref {}:{} {}",
                field(found, "path"),
                found["line"],
                field(found, "text"),
            ));
        }
        reply["name"] = json!(name);
        reply["text"] = json!(lines.join("\n"));
        Ok(reply)
    }

    /// `(fs-edit path edits [options])`: one atomic patch of anchored edits,
    /// then the edited file's check.
    fn edit(&self, cell: &AbortSignal, options: &Value) -> anyhow::Result<Value> {
        let mut payload = payload(options, EDIT, "path", &self.cwd)?;
        let return_stale = match payload.remove("onStale") {
            None => false,
            Some(Value::String(choice)) if choice == "raise" => false,
            Some(Value::String(choice)) if choice == "return" => true,
            Some(_) => bail!("on-stale must be 'raise or 'return"),
        };
        let edits = match payload.remove("edits") {
            Some(Value::Array(edits)) => edits,
            Some(one @ Value::Object(_)) => vec![one],
            Some(_) | None => bail!("edits must be a hash or a list of hashes"),
        };
        let mut wire = Vec::new();
        for (index, edit) in edits.iter().enumerate() {
            let number = index + 1;
            let fields = edit.as_object().ok_or_else(|| {
                anyhow!(
                    "edit {number} must be a hash like (hash 'replace \"12:abc\" 'text \"...\")"
                )
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
            // An anchor, or the row of a view or a hunk that carries one.
            let anchor = |place: &Value| {
                match place {
                Value::Object(row) => row.get("anchor").cloned().ok_or_else(|| {
                    anyhow!(
                        "edit {number}: an anchor is a string like \"12:abc\", or a row of fs-read's lines"
                    )
                }),
                Value::Null
                | Value::Bool(_)
                | Value::Number(_)
                | Value::String(_)
                | Value::Array(_) => Ok(place.clone()),
            }
            };
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
                    entry.insert("anchor".to_string(), anchor(first)?);
                    entry.insert("end".to_string(), anchor(last)?);
                }
                (_, place) => {
                    entry.insert("anchor".to_string(), anchor(place)?);
                }
            }
            if let Some(text) = fields.get("text") {
                entry.insert("text".to_string(), text.clone());
            }
            wire.push(Value::Object(entry));
        }
        payload.insert("edits".to_string(), Value::Array(wire));
        let file = PathBuf::from(payload["path"].as_str().unwrap_or_default());
        let mut reply = self.request("fs.edit", Value::Object(payload), cell)?;
        if reply["applied"] == json!(false) {
            if return_stale {
                return Ok(reply);
            }
            bail!(
                "{}",
                reply["text"]
                    .as_str()
                    .unwrap_or("rejected: stale anchors; nothing was written")
            );
        }
        self.attach_check(&mut reply, &file, cell);
        Ok(reply)
    }
}

#[cfg(test)]
mod tests;
