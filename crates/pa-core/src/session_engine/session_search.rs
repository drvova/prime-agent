//! The kernel's read-only recall over this project's saved sessions: `rlm.session.search`
//! (`await rlm.session.search(query)`) finds past sessions whose messages mention every query
//! term, and `rlm.session.saved_messages` (`rlm.session.messages(session=hit)`) reads one hit's
//! transcript in the same row shape as the live reader.
//!
//! Both scan the session directory `/resume` lists (header-only, then the file's lines), keep
//! the sessions whose header cwd is this session's, and only ever READ: no repair pass, no
//! resume, no edit, no delete. Every scan runs on a blocking thread.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context};
use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};
use crate::session::discovery::scan_session_headers;
use crate::session::manager::SessionManager;

/// Default and maximum hits per search.
const DEFAULT_LIMIT: usize = 10;
const MAX_LIMIT: usize = 50;
/// The newest saved sessions of the project a search reads.
const MAX_SCANNED_SESSIONS: usize = 300;
/// A session file larger than this is skipped by search (still readable by id).
const MAX_SEARCHED_BYTES: u64 = 32 * 1024 * 1024;
/// Matching snippets per hit, and characters of context on each side of a match.
const SNIPPETS_PER_HIT: usize = 3;
const SNIPPET_CONTEXT_CHARS: usize = 90;
/// Title length when it falls back to the first user message.
const TITLE_CHARS: usize = 80;

/// One finished `rlm.session.*` recall call, as adoption telemetry counts it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionRecallFact {
    /// A search and how many sessions it returned.
    Searched { hits: usize },
    /// A saved session's transcript was read.
    Read,
    /// The call was refused or failed.
    Failed,
}

/// Observes each finished call (adoption telemetry); never sees a query or a session.
pub(crate) type SessionRecallObserver = Arc<dyn Fn(SessionRecallFact) + Send + Sync>;

/// Register `rlm.session.search` and `rlm.session.saved_messages` over `session`'s session
/// directory and cwd (read per request: a fork or switch moves them).
pub(crate) fn register(
    handlers: &mut HostRequestHandlers,
    session: &Arc<tokio::sync::Mutex<SessionManager>>,
    observer: Option<&SessionRecallObserver>,
) {
    for request_type in ["rlm.session.search", "rlm.session.saved_messages"] {
        let session = Arc::clone(session);
        let observer = observer.cloned();
        handlers.register(
            request_type,
            host_handler(move |payload| {
                let session = Arc::clone(&session);
                let observer = observer.clone();
                async move {
                    let scope = {
                        let session = session.lock().await;
                        ProjectScope {
                            session_dir: session.get_session_dir().to_path_buf(),
                            cwd: session.get_cwd().to_path_buf(),
                            current_session_id: session.get_session_id().to_string(),
                        }
                    };
                    let data = payload.data;
                    let outcome = tokio::task::spawn_blocking(move || match request_type {
                        "rlm.session.search" => search(&scope, &data),
                        _ => saved_messages(&scope, &data),
                    })
                    .await
                    .context("session recall thread")?;
                    if let Some(observer) = &observer {
                        observer(match &outcome {
                            Ok((_, fact)) => *fact,
                            Err(_) => SessionRecallFact::Failed,
                        });
                    }
                    outcome.map(|(reply, _)| reply)
                }
            }),
        );
    }
}

/// Where a recall looks: the session directory, the project cwd, and the live session (which a
/// search leaves out: its own transcript is `rlm.session.messages()`).
struct ProjectScope {
    session_dir: PathBuf,
    cwd: PathBuf,
    current_session_id: String,
}

impl ProjectScope {
    /// The project's saved session files (header cwd is this cwd), newest first.
    fn sessions(&self) -> Vec<(PathBuf, String, std::time::SystemTime)> {
        let cwd = normalize(&self.cwd);
        let mut sessions: Vec<_> = scan_session_headers(&self.session_dir)
            .into_iter()
            .filter(|header| normalize(Path::new(&header.cwd)) == cwd)
            .filter_map(|header| {
                let modified = std::fs::metadata(&header.path).and_then(|meta| meta.modified());
                Some((header.path, header.id, modified.ok()?))
            })
            .collect();
        sessions.sort_by_key(|(_, _, modified)| std::cmp::Reverse(*modified));
        sessions
    }
}

fn normalize(path: &Path) -> PathBuf {
    pa_types::platform::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

/// One saved session read from disk: its header, title, and active-branch messages.
struct SavedSession {
    id: String,
    created_at: String,
    title: String,
    /// `(entry id, message value)` on the active branch, oldest first.
    messages: Vec<(String, Value)>,
}

/// Parse a session file read-only: the active branch is the parent chain of the last entry.
fn read_saved_session(path: &Path) -> anyhow::Result<SavedSession> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("read saved session {}", path.display()))?;
    let mut lines = content.lines();
    let header: Value = lines
        .next()
        .and_then(|line| serde_json::from_str(line).ok())
        .context("saved session has no header")?;
    let entries: Vec<Value> = lines
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|entry| entry.get("id").and_then(Value::as_str).is_some())
        .collect();
    let by_id: std::collections::HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|entry| Some((entry.get("id")?.as_str()?, entry)))
        .collect();
    let mut branch = Vec::new();
    let mut cursor = entries.last();
    while let Some(entry) = cursor {
        branch.push(entry);
        cursor = entry
            .get("parentId")
            .and_then(Value::as_str)
            .and_then(|parent| by_id.get(parent).copied());
        if branch.len() > entries.len() {
            break; // a parent cycle in a damaged file
        }
    }
    branch.reverse();
    let name = entries
        .iter()
        .rev()
        .find(|entry| entry.get("type").and_then(Value::as_str) == Some("session_info"))
        .and_then(|entry| entry.get("name").and_then(Value::as_str))
        .map(str::to_string);
    let messages: Vec<(String, Value)> = branch
        .into_iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("message"))
        .filter_map(|entry| {
            Some((
                entry.get("id")?.as_str()?.to_string(),
                entry.get("message")?.clone(),
            ))
        })
        .collect();
    let title = name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| {
            messages
                .iter()
                .find(|(_, message)| role(message) == "user")
                .map_or_default(|(_, message)| {
                    let text = text_of(message);
                    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
                    line.chars().take(TITLE_CHARS).collect()
                })
        });
    Ok(SavedSession {
        id: header
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        created_at: header
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        title,
        messages,
    })
}

fn role(message: &Value) -> &str {
    message
        .get("role")
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// A message's plain text: string content, or its text parts joined.
fn text_of(message: &Value) -> String {
    match message.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// The text around the first occurrence of `term` (a lowercase term), on char boundaries.
fn snippet(text: &str, lower: &str, term: &str) -> String {
    let at = lower.find(term).unwrap_or(0);
    // Lowercasing can shift byte offsets for some scripts: map through char counts.
    let char_at = lower[..at].chars().count();
    let chars: Vec<char> = text.chars().collect();
    let start = char_at.saturating_sub(SNIPPET_CONTEXT_CHARS);
    let end = (char_at + term.chars().count() + SNIPPET_CONTEXT_CHARS).min(chars.len());
    let body: String = chars[start.min(chars.len())..end].iter().collect();
    let body = body.split_whitespace().collect::<Vec<_>>().join(" ");
    format!(
        "{}{body}{}",
        if start > 0 { "..." } else { "" },
        if end < chars.len() { "..." } else { "" }
    )
}

fn iso(time: std::time::SystemTime) -> String {
    let millis = time
        .duration_since(std::time::UNIX_EPOCH)
        .map_or_default(|duration| duration.as_millis());
    crate::session::manager::format_iso(i64::try_from(millis).unwrap_or(i64::MAX))
}

fn search(scope: &ProjectScope, data: &Value) -> anyhow::Result<(Value, SessionRecallFact)> {
    let query = data
        .get("query")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let terms: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    if terms.is_empty() {
        bail!("rlm.session.search query must contain a word");
    }
    let limit = match data.get("limit") {
        None | Some(Value::Null) => DEFAULT_LIMIT,
        Some(value) => match value.as_u64() {
            Some(limit) if (1..=MAX_LIMIT as u64).contains(&limit) => {
                usize::try_from(limit).unwrap_or(MAX_LIMIT)
            }
            _ => bail!("rlm.session.search limit must be an integer from 1 to {MAX_LIMIT}"),
        },
    };
    let mut hits = Vec::new();
    for (path, id, modified) in scope.sessions().into_iter().take(MAX_SCANNED_SESSIONS) {
        if id == scope.current_session_id
            || std::fs::metadata(&path).is_ok_and(|meta| meta.len() > MAX_SEARCHED_BYTES)
        {
            continue;
        }
        let Ok(saved) = read_saved_session(&path) else {
            continue;
        };
        let mut matches = 0;
        let mut snippets = Vec::new();
        for (index, (_, message)) in saved.messages.iter().enumerate() {
            if !matches!(role(message), "user" | "assistant") {
                continue;
            }
            let text = text_of(message);
            let lower = text.to_lowercase();
            if !terms.iter().all(|term| lower.contains(term.as_str())) {
                continue;
            }
            matches += 1;
            if snippets.len() < SNIPPETS_PER_HIT {
                snippets.push(json!({
                    "index": index,
                    "role": role(message),
                    "text": snippet(&text, &lower, &terms[0]),
                }));
            }
        }
        if matches > 0 {
            hits.push((matches, modified, saved, snippets));
        }
    }
    // Most matching messages first, then the most recently active session.
    hits.sort_by(|left, right| right.0.cmp(&left.0).then(right.1.cmp(&left.1)));
    hits.truncate(limit);
    let count = hits.len();
    let hits: Vec<Value> = hits
        .into_iter()
        .map(|(matches, modified, saved, snippets)| {
            json!({
                "session_id": saved.id,
                "title": saved.title,
                "created_at": saved.created_at,
                "updated_at": iso(modified),
                "match_count": matches,
                "message_count": saved.messages.len(),
                "snippets": snippets,
            })
        })
        .collect();
    Ok((
        json!({ "hits": hits }),
        SessionRecallFact::Searched { hits: count },
    ))
}

/// `rows[start:][:limit]` over one saved session's active-branch messages whose role matches a
/// `roles` glob: the live `rlm.session.messages` reply shape.
fn saved_messages(
    scope: &ProjectScope,
    data: &Value,
) -> anyhow::Result<(Value, SessionRecallFact)> {
    let session_id = data
        .get("session")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if session_id.is_empty() {
        bail!("rlm.session.saved_messages requires a session id");
    }
    let Some((path, _, _)) = scope
        .sessions()
        .into_iter()
        .find(|(_, id, _)| id == session_id)
    else {
        bail!("no saved session {session_id:?} in this project");
    };
    let start = data.get("start").and_then(Value::as_i64).unwrap_or(-10);
    let limit = data
        .get("limit")
        .and_then(Value::as_u64)
        .and_then(|limit| usize::try_from(limit).ok())
        .unwrap_or(10);
    let roles: Vec<globset::GlobMatcher> = match data.get("roles").and_then(Value::as_array) {
        Some(roles) => roles
            .iter()
            .filter_map(Value::as_str)
            .map(|role| globset::Glob::new(role).map(|glob| glob.compile_matcher()))
            .collect::<Result<_, _>>()
            .context("rlm.session.saved_messages roles must be globs")?,
        None => vec![globset::Glob::new("*")?.compile_matcher()],
    };
    let saved = read_saved_session(&path)?;
    let rows: Vec<&(String, Value)> = saved
        .messages
        .iter()
        .filter(|(_, message)| roles.iter().any(|glob| glob.is_match(role(message))))
        .collect();
    let total = rows.len();
    let begin = if start < 0 {
        total.saturating_sub(usize::try_from(start.unsigned_abs()).unwrap_or(usize::MAX))
    } else {
        usize::try_from(start).map_or(total, |start| start.min(total))
    };
    let messages: Vec<Value> = rows
        .iter()
        .enumerate()
        .skip(begin)
        .take(limit)
        .map(|(index, (id, message))| json!({ "index": index, "id": id, "message": message }))
        .collect();
    Ok((
        json!({ "messages": messages, "total": total, "agent": saved.title }),
        SessionRecallFact::Read,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_session(dir: &Path, id: &str, cwd: &Path, lines: &[Value]) -> PathBuf {
        let path = dir.join(format!("{id}.jsonl"));
        let mut body = json!({
            "type": "session", "version": 3, "id": id,
            "timestamp": "2026-01-02T03:04:05.000Z", "cwd": cwd.to_string_lossy(),
        })
        .to_string();
        for line in lines {
            body.push('\n');
            body.push_str(&line.to_string());
        }
        body.push('\n');
        std::fs::write(&path, body).unwrap();
        path
    }

    fn message(id: &str, parent: Option<&str>, role: &str, text: &str) -> Value {
        json!({
            "type": "message", "id": id, "parentId": parent,
            "message": { "role": role, "content": [{ "type": "text", "text": text }] },
        })
    }

    /// A project with: an older session that mentions the query twice (once on an abandoned
    /// branch), a named session mentioning it once, the live session, and another project's.
    fn project() -> (tempfile::TempDir, ProjectScope) {
        let root = tempfile::tempdir().unwrap();
        let sessions = root.path().join("sessions");
        let cwd = root.path().join("project");
        let elsewhere = root.path().join("elsewhere");
        for dir in [&sessions, &cwd, &elsewhere] {
            std::fs::create_dir_all(dir).unwrap();
        }
        write_session(
            &sessions,
            "s-old",
            &cwd,
            &[
                message("a", None, "user", "Why is the parser test flaky?"),
                message(
                    "dead",
                    Some("a"),
                    "assistant",
                    "an abandoned flaky parser answer",
                ),
                message(
                    "b",
                    Some("a"),
                    "assistant",
                    "The parser test is flaky because of a race.",
                ),
                message("c", Some("b"), "toolResult", "flaky parser output"),
            ],
        );
        write_session(
            &sessions,
            "s-named",
            &cwd,
            &[
                json!({ "type": "session_info", "id": "i", "parentId": null, "name": "release prep" }),
                message(
                    "a",
                    Some("i"),
                    "user",
                    "fix the flaky Parser test before release",
                ),
            ],
        );
        write_session(
            &sessions,
            "s-live",
            &cwd,
            &[message("a", None, "user", "flaky parser again")],
        );
        write_session(
            &sessions,
            "s-other",
            &elsewhere,
            &[message("a", None, "user", "flaky parser elsewhere")],
        );
        let scope = ProjectScope {
            session_dir: sessions,
            cwd,
            current_session_id: "s-live".to_string(),
        };
        (root, scope)
    }

    /// A search finds this project's past sessions mentioning every word (the active branch's
    /// user and assistant messages only), ranks by matching messages, and leaves out the live
    /// session and other projects.
    #[test]
    fn search_ranks_this_projects_past_sessions() {
        let (_root, scope) = project();
        let (mut reply, fact) = search(&scope, &json!({ "query": "Flaky  parser" })).unwrap();
        for hit in reply["hits"].as_array_mut().unwrap() {
            hit.as_object_mut().unwrap().remove("updated_at");
        }
        assert_eq!(fact, SessionRecallFact::Searched { hits: 2 });
        assert_eq!(
            reply,
            json!({ "hits": [
                {
                    "session_id": "s-old",
                    "title": "Why is the parser test flaky?",
                    "created_at": "2026-01-02T03:04:05.000Z",
                    "match_count": 2,
                    "message_count": 3,
                    "snippets": [
                        { "index": 0, "role": "user", "text": "Why is the parser test flaky?" },
                        { "index": 1, "role": "assistant", "text": "The parser test is flaky because of a race." },
                    ],
                },
                {
                    "session_id": "s-named",
                    "title": "release prep",
                    "created_at": "2026-01-02T03:04:05.000Z",
                    "match_count": 1,
                    "message_count": 1,
                    "snippets": [
                        { "index": 0, "role": "user", "text": "fix the flaky Parser test before release" },
                    ],
                },
            ] })
        );
        assert_eq!(
            search(&scope, &json!({ "query": "  " }))
                .unwrap_err()
                .to_string(),
            "rlm.session.search query must contain a word"
        );
    }

    /// A hit reads in the live reader's row shape (role-filtered, sliced), and a session of
    /// another project is not found by id. Nothing on disk changes.
    #[test]
    fn saved_messages_read_a_hit_read_only() {
        let (_root, scope) = project();
        let before = std::fs::read(scope.session_dir.join("s-old.jsonl")).unwrap();
        let (reply, fact) = saved_messages(
            &scope,
            &json!({ "session": "s-old", "start": 0, "limit": 5, "roles": ["assistant", "tool*"] }),
        )
        .unwrap();
        assert_eq!(fact, SessionRecallFact::Read);
        assert_eq!(
            reply,
            json!({
                "messages": [
                    { "index": 0, "id": "b", "message": { "role": "assistant", "content": [{ "type": "text", "text": "The parser test is flaky because of a race." }] } },
                    { "index": 1, "id": "c", "message": { "role": "toolResult", "content": [{ "type": "text", "text": "flaky parser output" }] } },
                ],
                "total": 2,
                "agent": "Why is the parser test flaky?",
            })
        );
        assert_eq!(
            saved_messages(&scope, &json!({ "session": "s-other" }))
                .unwrap_err()
                .to_string(),
            "no saved session \"s-other\" in this project"
        );
        assert_eq!(
            std::fs::read(scope.session_dir.join("s-old.jsonl")).unwrap(),
            before
        );
    }
}
