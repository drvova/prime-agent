//! The folder-watch source: reports changes under a path on
//! `source/<name>/changed`, with the payload
//! `{ "paths": [<path>, ...], "kinds": ["create" | "modify" | "rename" | "remove" | "other" | "any", ...] }`.
//!
//! Changes are coalesced: the first change opens a window of `debounce_ms`,
//! and everything that changes inside it is published as one event when it
//! closes, each path and kind once, sorted. Reads (access events) are not
//! changes. With a `glob`, only matching paths are reported: a pattern
//! without `/` matches a file's name at any depth (`*.md`), and one with `/`
//! matches the path relative to the watched folder (`notes/**/*.md`).
//!
//! When the platform watcher reports an error (an event queue that
//! overflowed, a watched folder that went away), it is published on
//! `source/<name>/error` as `{ "error": <message> }` and the watch goes on.
//! Trust is the options' `trust`, Owner by default.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use globset::GlobBuilder;
use notify::event::ModifyKind;
use notify::{EventKind, RecursiveMode, Watcher};
use pa_types::event_log::Trust;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::{source_event, source_topic, spawn, Source, SourceHandle, StartContext};

/// A folder watch's options.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WatchOptions {
    /// The folder (or file) to watch; it must exist when the watch starts.
    pub path: PathBuf,
    /// Only paths matching this glob are reported.
    #[serde(default)]
    pub glob: Option<String>,
    /// Whether changes in subfolders are reported too.
    #[serde(default = "watch_subfolders")]
    pub recursive: bool,
    /// How long after the first change the batch it opened is published.
    #[serde(default = "default_debounce_ms")]
    pub debounce_ms: u64,
    #[serde(default = "owner")]
    pub trust: Trust,
}

fn watch_subfolders() -> bool {
    true
}

fn default_debounce_ms() -> u64 {
    200
}

fn owner() -> Trust {
    Trust::Owner
}

/// Starts folder watches.
#[derive(Debug, Clone, Copy, Default)]
pub struct WatchSource;

impl Source for WatchSource {
    type Options = WatchOptions;

    fn options_schema() -> Value {
        json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "watch",
            "description": "Publishes source/<name>/changed with the paths and kinds of change under a folder, coalesced over debounce_ms.",
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "The folder (or file) to watch; it must exist."
                },
                "glob": {
                    "type": "string",
                    "description": "Report only matching paths: without `/` it matches a file name at any depth, with `/` the path relative to the folder."
                },
                "recursive": {
                    "type": "boolean",
                    "default": true,
                    "description": "Whether changes in subfolders are reported."
                },
                "debounce_ms": {
                    "type": "integer",
                    "minimum": 0,
                    "default": 200,
                    "description": "How long after the first change its batch is published."
                },
                "trust": {
                    "enum": ["owner", "untrusted"],
                    "default": "owner"
                }
            },
            "required": ["path"],
            "additionalProperties": false
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one start: the checks, the platform watch and the debounce loop it feeds; split, each part would be a helper used once"
    )]
    fn start(
        &self,
        name: &str,
        options: WatchOptions,
        context: StartContext,
    ) -> anyhow::Result<SourceHandle> {
        let changed = source_topic(name, format!("source/{name}/changed"))?;
        let failed = source_topic(name, format!("source/{name}/error"))?;
        let root = pa_types::platform::canonicalize(&options.path).map_err(|error| {
            anyhow::anyhow!("watch {name:?}: {}: {error}", options.path.display())
        })?;
        let glob = match &options.glob {
            Some(pattern) => Some((
                GlobBuilder::new(pattern)
                    .literal_separator(true)
                    .build()
                    .map_err(|error| anyhow::anyhow!("watch {name:?}: glob: {error}"))?
                    .compile_matcher(),
                pattern.contains('/'),
            )),
            None => None,
        };
        let (sender, mut received) = mpsc::unbounded_channel();
        let mut watcher = notify::recommended_watcher(move |result| {
            // The receiver is gone only once the watch's work ended, and the
            // watcher with it.
            let _ = sender.send(result);
        })?;
        let mode = if options.recursive {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        watcher
            .watch(&root, mode)
            .map_err(|error| anyhow::anyhow!("watch {name:?}: {}: {error}", root.display()))?;
        let debounce = Duration::from_millis(options.debounce_ms);
        let (name, trust) = (name.to_string(), options.trust);
        let reported = move |path: &Path| match &glob {
            None => true,
            Some((matcher, false)) => path
                .file_name()
                .is_some_and(|file_name| matcher.is_match(file_name)),
            Some((matcher, true)) => matcher.is_match(path.strip_prefix(&root).unwrap_or(path)),
        };
        spawn(None, move |cancel| async move {
            // Held for as long as the watch runs; dropping it removes the
            // platform watch.
            let _watcher = watcher;
            let mut paths = BTreeSet::new();
            let mut kinds = BTreeSet::new();
            let mut window: Option<Instant> = None;
            loop {
                let deadline = window;
                let closes = async move {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    () = cancel.cancelled() => return Ok(()),
                    () = closes => {
                        let payload = json!({
                            "paths": std::mem::take(&mut paths),
                            "kinds": std::mem::take(&mut kinds),
                        });
                        context
                            .publisher
                            .publish(source_event(changed.clone(), &name, payload, trust))?;
                        window = None;
                    }
                    result = received.recv() => {
                        let Some(result) = result else {
                            anyhow::bail!("watch {name:?}: the platform watcher stopped");
                        };
                        let event = match result {
                            Ok(event) => event,
                            Err(error) => {
                                let payload = json!({ "error": error.to_string() });
                                context
                                    .publisher
                                    .publish(source_event(failed.clone(), &name, payload, trust))?;
                                continue;
                            }
                        };
                        let kind = match event.kind {
                            EventKind::Access(_) => continue,
                            EventKind::Any => "any",
                            EventKind::Create(_) => "create",
                            EventKind::Modify(ModifyKind::Name(_)) => "rename",
                            EventKind::Modify(
                                ModifyKind::Any
                                | ModifyKind::Data(_)
                                | ModifyKind::Metadata(_)
                                | ModifyKind::Other,
                            ) => "modify",
                            EventKind::Remove(_) => "remove",
                            EventKind::Other => "other",
                        };
                        let matched: Vec<String> = event
                            .paths
                            .iter()
                            .filter(|path| reported(path))
                            .map(|path| path.to_string_lossy().into_owned())
                            .collect();
                        if matched.is_empty() {
                            continue;
                        }
                        paths.extend(matched);
                        kinds.insert(kind);
                        window.get_or_insert_with(|| Instant::now() + debounce);
                    }
                }
            }
        })
    }
}

#[cfg(test)]
mod tests;
