//! The session's file tools: `fs-read`, `fs-grep`, `fs-find`, `fs-symbol`,
//! `fs-edit`, and `fs-write`.
//!
//! Every `fs-*` call is answered by the session's `fs.*` host request
//! handler: a whole-file
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
//! - `(fs-read path [options])` in a session that keeps paged-out output
//!   also returns `'related`: the file's likely next reads (its tests, the
//!   workspace files it imports, the files calling what it defines), each a
//!   hash of `'path`, `'why` (`"test"`, `"import"`, `"caller"`) and `'handle`
//!   (the whole file, for `ctx-page`), named in the last line of `'text`.
//! - `(fs-symbol name [options])` returns where one identifier is defined and
//!   used: `'name`, `'definitions` and `'references` (each a hash of `'path`,
//!   `'line`, `'text`; a definition also `'kind` and `'handle`, its source for
//!   `ctx-page`), `'truncated`, `'partial` (the index is still building),
//!   `'text`. Option: `'max-references` (default 50).
//! - `(fs-edit path edits [options])` also takes a row of `fs-read`'s
//!   `'lines` (or of a hunk) wherever it takes an anchor, and with the option
//!   `'view` (an `fs-read` view) a 1-based line number of that view. Its
//!   result carries
//!   `'check`: the edited file's check (`[check: ...]` and any diagnostics,
//!   also the last lines of `'text`), or `#f`. With `(hash 'on-stale
//!   'return)` a patch with stale anchors returns its rejection (`'applied`
//!   `#f`, `'stale`, `'lines`, `'text`) instead of raising.
//! - `(fs-write path text)` creates the file, or replaces all of it, with
//!   `text` (missing directories are created): `'path`, `'created`,
//!   `'total-lines`, `'bytes`, `'check`, `'text`.
//! - An `fs-edit` or `fs-write` that changed a text file shows the change
//!   on the cell's card.
//! - `(fs-edit path (hash 'old old-str 'new new-str))` replaces one unique
//!   occurrence of a string instead, found exactly or, failing that, by the
//!   first normalization that finds it (`edit_replace`): `'path`,
//!   `'start-line`, `'matched-by`, `'check`, `'text`.
//! - `(fs-find (hash 'glob pattern ...))` lists the paths matching a glob
//!   (`"**/*.rs"`) instead of matching file names.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use serde_json::{json, Map, Value};

use super::edit_check::EditChecks;
use super::edit_replace;
use crate::fs_edit::{FsEditHost, REWRITE_KEY};
use crate::fs_search::FsSearchHost;
use crate::kernel::scheme::contracts::{self, Spelling};
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload, KernelDiffDisplay};
use crate::symbol_index::{register_symbol_handler, SymbolIndex};
use crate::tools::path_utils::resolve_to_cwd;
use crate::tools::tool_definition::AbortSignal;

/// The Scheme side of the file tools, defined in every session engine after
/// the native tools' `%native`.
pub(super) const TOOLS: &str = r#"
(define (fs-symbol name [options (hash)]) (%native "fs-symbol" (hash-insert options 'name name)))
(define (fs-edit path edits [options (hash)])
  (%native "fs-edit" (hash-insert (hash-insert options 'path path) 'edits edits)))
(define (fs-write path text) (%native "fs-write" (hash 'path path 'text text)))
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

/// `fs-find`'s options when it is given a `'glob`.
const GLOB: Accepted = &[
    ("glob", "pattern"),
    ("hidden", "hidden"),
    ("gitignore", "gitignore"),
    ("max-results", "maxResults"),
];

/// `fs-find`'s options; `'glob` is listed so a refusal names it, and a call
/// that gives it takes [`GLOB`] instead.
const FIND: Accepted = &[
    ("glob", "pattern"),
    ("name", "name"),
    ("kind", "kind"),
    ("ignore-case", "ignoreCase"),
    ("max-depth", "maxDepth"),
    ("hidden", "hidden"),
    ("gitignore", "gitignore"),
    ("max-results", "maxResults"),
];

const READ: Accepted = &[("start", "start"), ("end", "end")];

const EDIT: Accepted = &[
    ("edits", "edits"),
    ("on-stale", "onStale"),
    ("view", "view"),
];

const WRITE: Accepted = &[("text", "text")];
/// The operations one edit may name, as Scheme spells them.
const EDIT_OPERATIONS: [&str; 5] = [
    "replace",
    "delete",
    "insert-after",
    "insert-before",
    "append",
];

/// Turn the options of a call to `tool` into the payload it parses: accepted
/// options under their payload keys, the absolute target under `path_key`,
/// and `cwd`.
fn payload(
    tool: &str,
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
            let names: Vec<&str> = std::iter::once("path")
                .chain(accepted.iter().map(|(option, _)| *option))
                .collect();
            return Err(contracts::undeclared(tool, Spelling::HashKey, name, &names).into());
        }
    }
    payload.insert(path_key.to_string(), json!(resolve_to_cwd(path, &cwd)));
    payload.insert("cwd".to_string(), json!(cwd));
    Ok(payload)
}

/// The file tools that only read, for a caller outside a kernel cell (the
/// `/better` helper's gate): no session handler answers them, so what they
/// read never counts as the session's reads.
pub(crate) struct FileReads(Files);

impl FileReads {
    /// The readers of the project in `cwd`, answering on `runtime`.
    pub(crate) fn new(cwd: PathBuf, runtime: tokio::runtime::Handle) -> Self {
        Self(Files::new(
            cwd,
            HostRequestHandlers::new(),
            runtime,
            /*agent_dir*/ None,
            &crate::kernel::call_policy::Admission::default(),
        ))
    }

    /// Run the reader `operation` (`fs-read`, `fs-grep`, `fs-find`,
    /// `fs-symbol`) with a call's `options`. Blocks until the
    /// reply: call it off the async threads.
    ///
    /// # Errors
    ///
    /// Any other operation, bad options, or the reader's own failure.
    pub(crate) fn run(&self, operation: &str, options: &Value) -> anyhow::Result<Value> {
        match operation {
            "fs-read" | "fs-grep" | "fs-find" | "fs-symbol" => {
                self.0.run(operation, &AbortSignal::new(), options)
            }
            other => Err(anyhow!("{other} is not a read")),
        }
    }
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
        admission: &crate::kernel::call_policy::Admission,
    ) -> Self {
        let mut handlers = HostRequestHandlers::new();
        FsSearchHost { observer: None }.register(&mut handlers);
        FsEditHost {
            observer: None,
            seen: None,
            prefetch: None,
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
        // The kernel's own handlers pass the policy as the session's do.
        let mut handlers = handlers.gated(admission);
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
            "fs-read" => {
                let payload = payload("fs-read", options, READ, "path", &self.cwd)?;
                let file = PathBuf::from(payload["path"].as_str().unwrap_or_default());
                let reply = self.request("fs.read", Value::Object(payload), cell)?;
                // The read rides the cell's result, as an edit's diff does:
                // a compaction reads the session's latest files from them.
                if let Some(kernel) = crate::kernel::scheme::cell::current() {
                    let path = pa_types::platform::canonicalize(&file).unwrap_or(file);
                    kernel.record(crate::kernel::scheme::cell::CellRecord::FileRead {
                        path: path.to_string_lossy().into_owned(),
                    });
                }
                Ok(reply)
            }
            "fs-grep" => {
                let mut payload = payload("fs-grep", options, GREP, "root", &self.cwd)?;
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
            "fs-find" if options.get("glob").is_some() => self.request(
                "fs.glob",
                Value::Object(payload("fs-find", options, GLOB, "root", &self.cwd)?),
                cell,
            ),
            "fs-find" => self.request(
                "fs.find",
                Value::Object(payload("fs-find", options, FIND, "root", &self.cwd)?),
                cell,
            ),
            "fs-symbol" => self.symbol(cell, options),
            "fs-edit" if options["edits"].get("old").is_some() => self.replace(cell, options),
            "fs-edit" => self.edit(cell, options),
            "fs-write" => {
                let payload = payload("fs-write", options, WRITE, "path", &self.cwd)?;
                let file = PathBuf::from(payload["path"].as_str().unwrap_or_default());
                let mut reply = self.request("fs.write", Value::Object(payload), cell)?;
                show_rewrite(&mut reply, &file);
                self.attach_check(&mut reply, &file, cell);
                Ok(reply)
            }
            unknown => Err(anyhow!("unknown file tool: {unknown}")),
        }
    }

    /// `(fs-edit path (hash 'old old-str 'new new-str))`: replace one unique
    /// occurrence of `old-str`, found exactly or by the first normalization
    /// that finds it.
    fn replace(&self, cell: &AbortSignal, options: &Value) -> anyhow::Result<Value> {
        if let Some(unknown) = options.as_object().and_then(|fields| {
            fields
                .keys()
                .find(|key| !matches!(key.as_str(), "path" | "edits"))
        }) {
            bail!("the option '{unknown}' does not apply to an 'old/'new edit");
        }
        let edit = &options["edits"];
        if let Some(unknown) = edit.as_object().and_then(|fields| {
            fields
                .keys()
                .find(|key| !matches!(key.as_str(), "old" | "new"))
        }) {
            bail!("an 'old/'new edit takes no '{unknown}'");
        }
        let mut wire = Map::new();
        wire.insert("path".to_string(), options["path"].clone());
        wire.insert("old-str".to_string(), edit["old"].clone());
        wire.insert("new-str".to_string(), edit["new"].clone());
        let replaced = edit_replace::replace(&self.cwd, &Value::Object(wire));
        // How it matched, or why nothing did (adoption telemetry):
        // nothing waits for the count, and one the host refuses
        // changes nothing here.
        let outcome = match &replaced {
            Ok(replaced) => replaced.reply["matchedBy"].as_str(),
            Err(error) => {
                error
                    .downcast_ref::<edit_replace::Miss>()
                    .map(|miss| match miss.outcome {
                        edit_replace::MissOutcome::AnchorPrefix => "anchor-prefix",
                        edit_replace::MissOutcome::Ambiguous => "ambiguous",
                        edit_replace::MissOutcome::NotFound => "not-found",
                        edit_replace::MissOutcome::AlreadyApplied => "already-applied",
                    })
            }
        };
        if let (Some(outcome), Some(handler)) = (
            outcome,
            self.handlers
                .get(crate::kernel::edit_matches::EDIT_FACT_REQUEST_TYPE),
        ) {
            let counted = handler(HostRequestPayload {
                data: json!({ "outcome": outcome }),
            });
            self.runtime.spawn(async move {
                if let Err(error) = counted.await {
                    tracing::debug!("edit fact was refused: {error:#}");
                }
            });
        }
        let edit_replace::Replaced {
            file,
            mut reply,
            old,
            new,
        } = replaced?;
        // The edit shows on the running cell's card, as the Python
        // kernel's `edit` skill displayed it: the text that was
        // replaced, which a normalized match spells differently from
        // `old-str`.
        if let Some(kernel) = crate::kernel::scheme::cell::current() {
            kernel.record(crate::kernel::scheme::cell::CellRecord::Diff(
                KernelDiffDisplay {
                    path: file.to_string_lossy().into_owned(),
                    old_str: old,
                    new_str: new,
                    start_line: reply["startLine"].as_u64(),
                },
            ));
        }
        self.attach_check(&mut reply, &file, cell);
        Ok(reply)
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
            return Err(contracts::undeclared(
                "fs-symbol",
                Spelling::HashKey,
                unknown,
                &["max-references"],
            )
            .into());
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
                .map_or_default(|handle| format!(" · {handle}"));
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
        let mut payload = payload("fs-edit", options, EDIT, "path", &self.cwd)?;
        let return_stale = match payload.remove("onStale") {
            None => false,
            Some(Value::String(choice)) if choice == "raise" => false,
            Some(Value::String(choice)) if choice == "return" => true,
            Some(_) => bail!("on-stale must be 'raise or 'return"),
        };
        // Line numbers of a view stand for that view's anchors.
        let view_rows = match payload.remove("view") {
            None => None,
            Some(view) => Some(
                view.get("lines")
                    .and_then(Value::as_array)
                    .cloned()
                    .ok_or_else(|| anyhow!("view must be an fs-read view, with its 'lines"))?,
            ),
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
                Value::Number(line) => match &view_rows {
                    Some(rows) => rows
                        .iter()
                        .find(|row| row.get("line").and_then(Value::as_u64) == line.as_u64())
                        .and_then(|row| row.get("anchor").cloned())
                        .ok_or_else(|| anyhow!("edit {number}: line {line} is not in the view")),
                    None => Ok(place.clone()),
                },
                Value::Null | Value::Bool(_) | Value::String(_) | Value::Array(_) => {
                    Ok(place.clone())
                }
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
        show_rewrite(&mut reply, &file);
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

/// Take the text before and after off the reply of an `fs.edit` or
/// `fs.write` that changed `file`, and show the change on the running cell's
/// card as `edit` shows its own: under the canonical path, from line 1.
fn show_rewrite(reply: &mut Value, file: &Path) {
    let Some(mut rewrite) = reply
        .as_object_mut()
        .and_then(|fields| fields.remove(REWRITE_KEY))
    else {
        return;
    };
    if let Some(kernel) = crate::kernel::scheme::cell::current() {
        // Moved out of the reply, not copied: a side is the whole file.
        let mut text =
            |side: &str| serde_json::from_value::<String>(rewrite[side].take()).unwrap_or_default();
        let path = pa_types::platform::canonicalize(file).unwrap_or_else(|_| file.to_path_buf());
        kernel.record(crate::kernel::scheme::cell::CellRecord::Diff(
            KernelDiffDisplay {
                path: path.to_string_lossy().into_owned(),
                old_str: text("before"),
                new_str: text("after"),
                start_line: Some(1),
            },
        ));
    }
}

#[cfg(test)]
mod tests;
