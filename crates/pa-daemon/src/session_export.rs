//! Session export commands: the worker-side handlers for `export_html` and `export_jsonl`
//! (TS `session.exportToHtml` / `exportToJsonl`). The HTML file is built by pa-core's
//! exporter (the embedded template plus the session data); the JSONL export is the current
//! branch re-chained into a linear file.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context as _, Result};
use serde_json::{json, Value};

use crate::engine::SessionEngine;
use crate::protocol::{response_failure, response_success, DaemonResponse};
use crate::worker::SessionCore;

/// The `export_*` command set: the engine (system prompt), the session store
/// (entries), the settings dirs (theme resolution), the session cwd.
pub(crate) struct ExportCommands {
    engine: Arc<dyn SessionEngine>,
    core: Arc<Mutex<SessionCore>>,
    agent_dir: PathBuf,
}

impl ExportCommands {
    pub(crate) fn new(
        engine: Arc<dyn SessionEngine>,
        core: Arc<Mutex<SessionCore>>,
        agent_dir: PathBuf,
    ) -> Self {
        ExportCommands {
            engine,
            core,
            agent_dir,
        }
    }

    /// `export_html`: render the session to a standalone HTML file; the
    /// response carries the written path (TS `{ path }`).
    pub(crate) async fn export_html(&self, payload: &Value) -> DaemonResponse {
        let output_path = payload
            .get("outputPath")
            .and_then(Value::as_str)
            .map(str::to_string);
        match self.export_html_impl(output_path.as_deref()).await {
            Ok(path) => response_success(None, "export_html", Some(json!({ "path": path }))),
            Err(error) => response_failure(None, "export_html", &format!("{error:#}"), None),
        }
    }

    async fn export_html_impl(&self, output_path: Option<&str>) -> Result<String> {
        // The store snapshot under the core lock: the std guard is not `Send`, so the
        // engine reads below await outside the lock.
        let (header, entries, leaf_id, session_file, theme_name) = {
            let core = self.core.lock().unwrap();
            let store = core
                .store
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("Session is still initializing"))?;
            let theme_name = self
                .resolved_theme_name(&core)
                .context("resolving the export theme")?;
            let header = serde_json::to_value(&store.header)?;
            let header = match header {
                Value::Object(map) => {
                    // The export's header is the `type: "session"` file line, not just
                    // the typed struct.
                    let mut with_type = serde_json::Map::new();
                    with_type.insert("type".to_string(), Value::String("session".to_string()));
                    with_type.extend(map);
                    Value::Object(with_type)
                }
                other => other,
            };
            let entries: Vec<Value> = store
                .entries()
                .iter()
                .map(serde_json::to_value)
                .collect::<Result<_, serde_json::Error>>()
                .context("serializing session entries")?;
            (
                header,
                entries,
                store.leaf_id().map(str::to_string),
                store.path.clone(),
                theme_name,
            )
        };
        // The tools read comes first: an absent session builds here (the TS state
        // exists from create), so the best-effort prompt read below then sees it too.
        let tools = self.engine.export_tools().await;
        let rendered_tools = self.engine.export_rendered_tools(&entries).await;
        let data = pa_core::export_html::SessionExportData {
            header,
            entries,
            leaf_id,
            system_prompt: self.engine.export_system_prompt(),
            tools,
            rendered_tools,
        };
        pa_core::export_html::export_session_to_html(
            &data,
            theme_name.as_deref(),
            &self.agent_dir,
            &session_file,
            output_path,
        )
    }

    /// The session's configured theme name (TS `settingsManager.getTheme()`)
    /// resolved against the session's cwd and the agent dir.
    fn resolved_theme_name(&self, core: &SessionCore) -> Result<Option<String>> {
        let cwd = if core.cwd.is_empty() {
            pa_types::session_scope::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(&core.cwd)
        };
        let settings = pa_core::settings::SettingsManager::create(&cwd, &self.agent_dir);
        Ok(settings.get_theme().map(str::to_string))
    }

    /// `export_jsonl`: the current branch re-chained linearly into a JSONL
    /// file; the response carries the resolved path (TS `{ path }`). The
    /// branch is copied under the session's state lock and written after it
    /// is released, off the runtime: a slow target (a network mount, a pipe
    /// nobody reads) delays this command's own answer and nothing else.
    pub(crate) async fn export_jsonl(&self, payload: &Value) -> DaemonResponse {
        let output_path = payload.get("outputPath").and_then(Value::as_str);
        let written = match self.jsonl_snapshot(output_path) {
            // The test seam: the write happens under the state lock, on this thread.
            Ok((file_path, body))
                if cfg!(debug_assertions)
                    && pa_types::session_scope::var(EXPORT_HOLDS_SESSION_SEAM_ENV).as_deref()
                        == Ok("1") =>
            {
                let _held = self.core.lock().unwrap();
                write_export(&file_path, &body)
            }
            Ok((file_path, body)) => {
                tokio::task::spawn_blocking(move || write_export(&file_path, &body))
                    .await
                    .unwrap_or_else(|error| {
                        Err(anyhow::anyhow!("the export writer failed: {error}"))
                    })
            }
            Err(error) => Err(error),
        };
        match written {
            Ok(path) => response_success(None, "export_jsonl", Some(json!({ "path": path }))),
            Err(error) => response_failure(None, "export_jsonl", &format!("{error:#}"), None),
        }
    }

    /// Where an `export_jsonl` writes and what: read under the state lock.
    fn jsonl_snapshot(&self, output_path: Option<&str>) -> Result<(PathBuf, String)> {
        let core = self.core.lock().unwrap();
        let store = core
            .store
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Session is still initializing"))?;
        // A relative (or omitted) output path lands in the daemon process
        // cwd, like the TS `resolve` default.
        let cwd = if core.cwd.is_empty() {
            pa_types::session_scope::current_dir().unwrap_or_default()
        } else {
            PathBuf::from(&core.cwd)
        };
        let file_path = match output_path {
            Some(path) => {
                let absolute = Path::new(path);
                if absolute.is_absolute() {
                    absolute.to_path_buf()
                } else {
                    cwd.join(path)
                }
            }
            None => cwd.join(format!(
                "session-{}.jsonl",
                pa_core::session::manager::format_iso_now().replace([':', '.'], "-")
            )),
        };
        let header = json!({
            "type": "session",
            "version": pa_core::session::CURRENT_SESSION_VERSION,
            "id": store.session_id(),
            "timestamp": pa_core::session::manager::format_iso_now(),
            "cwd": core.cwd,
        });
        let mut lines = vec![serde_json::to_string(&header)?];
        // The branch walks root-to-leaf; parent ids re-chain linearly so
        // the exported file resumes as one straight session.
        let mut previous: Option<String> = None;
        for entry in store.branch() {
            let mut value = serde_json::to_value(entry)?;
            if let Some(map) = value.as_object_mut() {
                match previous.clone() {
                    Some(parent) => map.insert("parentId".to_string(), Value::String(parent)),
                    None => map.insert("parentId".to_string(), Value::Null),
                };
            }
            previous = value.get("id").and_then(Value::as_str).map(str::to_string);
            lines.push(serde_json::to_string(&value)?);
        }
        Ok((file_path, format!("{}\n", lines.join("\n"))))
    }
}

/// Set to `1` in a session's launch environment, in a debug build,
/// `export_jsonl` writes its file while it holds the session's state lock, on
/// the session's own runtime thread. An export into a pipe nobody reads then
/// stops everything of the session that needs its state until the pipe is
/// drained: the tests' stand-in for a session that does not answer (what
/// SIGSTOP was to a worker process). Release builds ignore it.
pub const EXPORT_HOLDS_SESSION_SEAM_ENV: &str = "PRIME_AGENT_TEST_EXPORT_HOLDS_SESSION";

/// Write one export's `body` to `file_path`, creating its directory, and
/// return the path as the response carries it.
fn write_export(file_path: &Path, body: &str) -> Result<String> {
    if let Some(parent) = file_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    // A file is replaced whole, so a failed export leaves the previous one
    // as it was; a pipe or a device (`/dev/stdout`) cannot be replaced and
    // is written through.
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

/// The export's custom-tool renderer: resolves a tool by name against the
/// session's live registry. The Rust tool surface carries no render functions,
/// so the export falls back to the template's generic tool rendering.
pub(crate) struct ExportToolRenderer<'a> {
    /// The session's live tool registry (TS `getToolDefinition` source).
    pub tools: &'a [std::sync::Arc<dyn pa_agent::types::AgentTool>],
}

impl pa_core::export_html::ToolHtmlRenderer for ExportToolRenderer<'_> {
    fn render_call(&self, _tool_call_id: &str, tool_name: &str, _args: &Value) -> Option<String> {
        // Registry lookup first (TS `getToolDefinition`): an unregistered tool never
        // renders; a registered one has no render function.
        self.tools.iter().find(|tool| tool.name() == tool_name)?;
        None
    }

    fn render_result(
        &self,
        _tool_call_id: &str,
        tool_name: &str,
        _result: &[Value],
        _details: &Value,
        _is_error: bool,
    ) -> Option<pa_core::export_html::RenderedToolResult> {
        self.tools.iter().find(|tool| tool.name() == tool_name)?;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn core_with_session(dir: &Path) -> SessionCore {
        let file = dir.join("sess.jsonl");
        std::fs::write(
            &file,
            concat!(
                r##"{"type":"session","id":"sess-1","version":3,"timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"##,
                "\n",
                r##"{"type":"message","id":"e1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hi"}}"##,
                "\n",
                r##"{"type":"message","id":"e2","parentId":"e1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hello"}]}}"##,
                "\n",
            ),
        )
        .expect("write session file");
        let store = crate::session_store::SessionFile::open(&file).expect("open session");
        SessionCore::test_core(Some(store), dir.display().to_string())
    }

    #[tokio::test]
    async fn export_html_writes_the_session_data() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let core = Arc::new(Mutex::new(core_with_session(dir.path())));
        let engine: Arc<dyn SessionEngine> = Arc::new(crate::engine::ScriptedEngine::default());
        let exports = ExportCommands::new(engine, core, dir.path().join("agent"));
        let out = dir.path().join("export.html");
        let payload = json!({ "outputPath": out.display().to_string() });
        let response = exports.export_html(&payload).await;
        assert!(response.success, "export failed: {:?}", response.error);
        let path = response
            .data
            .as_ref()
            .and_then(|data| data.get("path"))
            .and_then(Value::as_str)
            .expect("path in response")
            .to_string();
        assert_eq!(path, out.display().to_string());
        let html = std::fs::read_to_string(&out).expect("read export");
        assert!(html.contains("Session Export"));
        assert!(html.contains("--accent:"));
    }

    #[tokio::test]
    async fn export_jsonl_rechains_the_branch() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let core = Arc::new(Mutex::new(core_with_session(dir.path())));
        let engine: Arc<dyn SessionEngine> = Arc::new(crate::engine::ScriptedEngine::default());
        let exports = ExportCommands::new(engine, core, dir.path().join("agent"));
        let response = exports.export_jsonl(&json!({})).await;
        assert!(response.success, "export failed: {:?}", response.error);
        let path = response
            .data
            .as_ref()
            .and_then(|data| data.get("path"))
            .and_then(Value::as_str)
            .expect("path in response")
            .to_string();
        let body = std::fs::read_to_string(&path).expect("read export");
        let lines: Vec<&str> = body.lines().collect();
        assert_eq!(lines.len(), 3);
        let header: Value = serde_json::from_str(lines[0]).expect("header line");
        assert_eq!(header["type"], "session");
        assert_eq!(header["id"], "sess-1");
        assert_eq!(header["cwd"], dir.path().display().to_string());
        let first: Value = serde_json::from_str(lines[1]).expect("first entry");
        assert_eq!(first["id"], "e1");
        assert_eq!(first["parentId"], Value::Null);
        let second: Value = serde_json::from_str(lines[2]).expect("second entry");
        assert_eq!(second["id"], "e2");
        assert_eq!(second["parentId"], "e1");
    }

    /// A target that takes no bytes (a pipe nobody reads) holds the export
    /// alone: the session's state stays free for everything else meanwhile.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_stalled_export_target_leaves_the_session_state_free() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let core = Arc::new(Mutex::new(core_with_session(dir.path())));
        let engine: Arc<dyn SessionEngine> = Arc::new(crate::engine::ScriptedEngine::default());
        let exports = Arc::new(ExportCommands::new(
            engine,
            Arc::clone(&core),
            dir.path().join("agent"),
        ));
        let pipe = dir.path().join("stalled.pipe");
        let made = std::process::Command::new("mkfifo").arg(&pipe).status();
        assert!(made.is_ok_and(|status| status.success()), "mkfifo failed");
        let exporting = {
            let (exports, pipe) = (Arc::clone(&exports), pipe.clone());
            tokio::spawn(async move {
                exports
                    .export_jsonl(&json!({ "outputPath": pipe.display().to_string() }))
                    .await
            })
        };
        // Opening the read end returns once the export holds the write end.
        let mut reader = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::fs::File::open(&pipe),
        )
        .await
        .expect("the export never opened its target")
        .expect("open the pipe's read end");
        let state_free = core.try_lock().is_ok();
        let mut body = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut reader, &mut body)
            .await
            .expect("drain the export");
        let response = exporting.await.expect("export task");
        assert_eq!(
            (state_free, response.success, body.lines().count()),
            (true, true, 3)
        );
    }

    #[tokio::test]
    async fn export_requires_the_session_store() {
        let dir = tempfile::TempDir::new().expect("temp dir");
        let mut core = core_with_session(dir.path());
        core.store = None;
        let engine: Arc<dyn SessionEngine> = Arc::new(crate::engine::ScriptedEngine::default());
        let exports =
            ExportCommands::new(engine, Arc::new(Mutex::new(core)), dir.path().join("agent"));
        let response = exports.export_html(&json!({})).await;
        assert!(!response.success);
        assert_eq!(
            response.error.as_deref(),
            Some("Session is still initializing")
        );
    }
}
