//! A TUI create on the runtime daemon (phase 8e2), sent through pa-tui's
//! `wire2_compat` (`pa_tui::wire2_request_frames`), builds the session the
//! old daemon (the `pa-daemon` supervisor) builds for the same create, over
//! one seeded agent dir each: a default key, a named credential `work`, a
//! profile `work` that maps the provider to it, and a loopback
//! chat-completions provider that records every request's bearer. For each
//! variant (a credential pin, a profile, an API key, a model scope, a scoped
//! child under a pinned parent, a session dir, the client's launch
//! environment) the session's header (its pin, profile, parent and depth),
//! its state (model, level, scope), where its transcript went, the key its
//! turn sent and what its `!` command sees are equal on both daemons, and so
//! is the refusal of a create whose credential or profile does not resolve.
//! An API key the runtime daemon was given is written nowhere: not under its
//! agent dir, nor in its database.
//!
//! Every daemon runs offline (`PI_OFFLINE=1`) on real engines: the faux
//! script would hide the model and key a create chooses.
//!
//! The old daemon's side is frozen: its answers are read from a golden
//! ([`super::old_reference`]).

use std::io::{Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pa_types::daemon::DaemonCommand;
use pa_types::wire2::{ClientFrame, Since};
use serde_json::{json, Value};

use super::old_reference::OldReference;
use super::saved_list::old_list;
use super::switches::read_as_the_tui_does;
use super::{Bed, Client, Daemon};

const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// A loopback chat-completions endpoint answering each POST with one
/// streamed `pinned answer`; the bearers it was sent, in order.
fn completions() -> (String, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let bearers: Arc<Mutex<Vec<String>>> = Arc::default();
    let recorded = Arc::clone(&bearers);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { return };
            let mut request = Vec::new();
            let mut chunk = [0u8; 8192];
            let head_end = loop {
                let Ok(read) = stream.read(&mut chunk) else {
                    break None;
                };
                if read == 0 {
                    break None;
                }
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break Some(end + 4);
                }
            };
            let Some(head_end) = head_end else { continue };
            let head = String::from_utf8_lossy(&request[..head_end]).into_owned();
            let header = |name: &str| {
                head.lines().find_map(|line| {
                    let (key, value) = line.split_once(':')?;
                    key.trim()
                        .eq_ignore_ascii_case(name)
                        .then(|| value.trim().to_string())
                })
            };
            let length: usize = header("content-length")
                .and_then(|value| value.parse().ok())
                .unwrap_or(0);
            while request.len() < head_end + length {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                }
            }
            if !head.starts_with("POST ") {
                let _ = stream.write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                );
                continue;
            }
            recorded
                .lock()
                .unwrap()
                .push(header("authorization").unwrap_or_default());
            let chunk = |delta: Value, finish: Value| {
                json!({ "id": "e2e", "object": "chat.completion.chunk", "created": 0,
                        "model": "e2e-model",
                        "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }] })
            };
            let mut done = chunk(json!({}), json!("stop"));
            done["usage"] =
                json!({ "prompt_tokens": 1, "completion_tokens": 2, "total_tokens": 3 });
            let body = format!(
                "data: {}\n\ndata: {done}\n\ndata: [DONE]\n\n",
                chunk(
                    json!({ "role": "assistant", "content": "pinned answer" }),
                    Value::Null
                )
            );
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\
                 Connection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (origin, bearers)
}

/// The agent dir both daemons read: two models of one provider at the
/// loopback endpoint (the saved default `e2e-model`, and `e2e-think`, which
/// thinks), its default key, the named credential `work`, and the profile
/// `work` that maps the provider to it.
fn seed(bed: &Bed, origin: &str) {
    let agent = bed.agent_dir();
    let write = |name: &str, value: Value| {
        let path = agent.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, value.to_string()).unwrap();
    };
    let model = |id: &str, reasoning: bool| {
        json!({ "id": id, "api": "openai-completions", "reasoning": reasoning,
                "contextWindow": 128_000, "maxTokens": 4096 })
    };
    write(
        "settings.json",
        json!({ "defaultProvider": "openai", "defaultModel": "e2e-model" }),
    );
    write(
        "models.json",
        json!({ "providers": { "openai": {
            "baseUrl": format!("{origin}/v1"),
            "models": [model("e2e-model", false), model("e2e-think", true)],
        } } }),
    );
    write(
        "auth.json",
        json!({ "openai": { "type": "api_key", "key": "key-default" } }),
    );
    write(
        "auth-accounts.json",
        json!({ "version": 1, "providers": { "openai": [{
            "id": "c_00000000e2e0work", "label": "work", "addedAt": 0,
            "credential": { "type": "api_key", "key": "key-work" },
        }] } }),
    );
    write(
        "profiles/work.json",
        json!({ "version": 1, "credentials": { "openai": "c_00000000e2e0work" }, "settings": {} }),
    );
    std::fs::create_dir_all(bed.dir.path().join("project")).unwrap();
}

/// The creates the TUI sends, in order (`create_config()` plus the
/// command's own members): `{bed}` is the bed's dir, `{parent}` the first
/// session's transcript.
fn creates() -> Vec<Value> {
    let config = |extra: Value| {
        let mut config = json!({ "cwd": "{bed}/project", "executionMode": "interactive" });
        config
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({ "type": "create", "config": config })
    };
    let mut launched = config(json!({}));
    launched["launchEnv"] = json!({ "PA_MARKER": "from-the-client", "RLM_DEPTH": "3" });
    vec![
        config(json!({ "credential": "work" })),
        config(json!({ "profile": "work" })),
        config(json!({ "apiKey": "key-given" })),
        config(json!({ "models": ["openai/e2e-think:high"] })),
        config(json!({ "parentSessionPath": "{parent}", "rlmDepth": 1 })),
        config(json!({ "sessionDir": "{bed}/elsewhere" })),
        launched,
    ]
}

/// The creates both daemons refuse before opening anything: a credential
/// that is not stored, and a profile that does not exist.
fn refused_creates() -> Vec<Value> {
    ["credential", "profile"]
        .map(|key| json!({ "type": "create", "config": { "cwd": "{bed}/project", key: "nope" } }))
        .to_vec()
}

/// `create` with its placeholders filled.
fn filled(create: &Value, bed: &Path, parent: Option<&Path>) -> Value {
    let text = create
        .to_string()
        .replace("{bed}", &bed.display().to_string());
    let text = match parent {
        Some(parent) => text.replace("{parent}", &parent.display().to_string()),
        None => text,
    };
    serde_json::from_str(&text).unwrap()
}

/// What one create built, with the bed's dir and the first session's
/// transcript (`parent`) taken out of its paths.
fn built(bed: &Bed, file: &Path, parent: &Path, state: &Value, bearers: &[String]) -> Value {
    let (saved, marker) = saved_with_bash(&bed.agent_dir(), file);
    let Some(pa_types::session::FileEntry::Header { header }) = saved
        .lines()
        .next()
        .and_then(|line| serde_json::from_str(line).ok())
    else {
        panic!("no header in {}: {saved}", file.display());
    };
    let bed = bed.dir.path();
    let relative = |path: &str| {
        path.replace(&parent.display().to_string(), "{parent}")
            .replace(&bed.display().to_string(), "{bed}")
    };
    let scoped: Vec<Value> = state["scopedModels"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .map(|entry| {
            json!([
                entry["model"]["provider"],
                entry["model"]["id"],
                entry["thinkingLevel"]
            ])
        })
        .collect();
    json!({
        "pin": header.credential_pin(),
        "profile": header.profile(),
        "parent": header.parent_session.as_deref().map(relative),
        "rlmDepth": header.rlm_depth,
        "dir": relative(&file.parent().unwrap().display().to_string()),
        "model": [state["model"]["provider"], state["model"]["id"]],
        "thinkingLevel": state["thinkingLevel"],
        "scopedModels": scoped,
        "bearers": bearers,
        "marker": marker,
    })
}

/// Session `file`'s transcript, as the daemon on `agent_dir` keeps it, once
/// the `!` command it ran is saved, and that command's output.
fn saved_with_bash(agent_dir: &Path, file: &Path) -> (String, String) {
    let store =
        pa_core::session::store::open_database(&pa_core::session::store::database_path(agent_dir))
            .expect("open the transcript database");
    let deadline = std::time::Instant::now() + std::time::Duration::from_mins(1);
    loop {
        let saved = store.read(file).map_or_else(
            |_| String::new(),
            |bytes| String::from_utf8_lossy(&bytes).into_owned(),
        );
        let output = saved
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find(|row| row["message"]["role"] == "bashExecution")
            .and_then(|row| row["message"]["output"].as_str().map(str::to_string));
        if let Some(output) = output {
            return (saved, output.trim().to_string());
        }
        assert!(
            std::time::Instant::now() < deadline,
            "no saved `!` command in {}: {saved}",
            file.display()
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

/// The `!` command each session runs: what it sees of the client's
/// environment and of the recursion marker.
const MARKER_COMMAND: &str = "echo \"${PA_MARKER:-unset} ${RLM_DEPTH:-unset}\"";

/// What the old daemon builds for each create.
fn old_builds(bed: &Bed, bearers: &Arc<Mutex<Vec<String>>>) -> Vec<Value> {
    let _daemon = bed.old_daemon_with(OFFLINE);
    let socket = bed.old_socket();
    let mut parent: Option<PathBuf> = None;
    let mut builds = Vec::new();
    for (index, create) in creates().iter().enumerate() {
        let create = filled(create, bed.dir.path(), parent.as_deref());
        let created = old_list(&socket, &format!("c{index}"), &create);
        let created = created.last().unwrap();
        assert_eq!(created["success"], true, "{created}");
        let active = created["data"]["id"].as_str().unwrap().to_string();
        let file = PathBuf::from(created["data"]["sessionFile"].as_str().unwrap());
        parent.get_or_insert_with(|| file.clone());
        let state = old_list(
            &socket,
            &format!("s{index}"),
            &json!({ "type": "get_state", "activeSessionId": active }),
        );
        let state = read_as_the_tui_does(state.last().unwrap())["data"].clone();
        bearers.lock().unwrap().clear();
        let turn = old_list(
            &socket,
            &format!("t{index}"),
            &json!({ "type": "prompt_and_wait", "activeSessionId": active, "message": "hello" }),
        );
        assert_eq!(turn.last().unwrap()["success"], true, "{turn:?}");
        let sent = std::mem::take(&mut *bearers.lock().unwrap());
        let bash = old_list(
            &socket,
            &format!("b{index}"),
            &json!({ "type": "execute_bash", "activeSessionId": active,
                     "command": MARKER_COMMAND }),
        );
        assert_eq!(bash.last().unwrap()["success"], true, "{bash:?}");
        builds.push(built(bed, &file, parent.as_deref().unwrap(), &state, &sent));
    }
    for (index, create) in refused_creates().iter().enumerate() {
        let create = filled(create, bed.dir.path(), None);
        let refused = old_list(&socket, &format!("r{index}"), &create);
        let mut refused = read_as_the_tui_does(refused.last().unwrap());
        // The supervisor wraps its worker's refusal; the runtime daemon has no worker to name.
        if let Some(error) = refused["error"].as_str() {
            refused["error"] = json!(error.trim_start_matches("session worker create failed: "));
        }
        builds.push(refused);
    }
    builds
}

/// The runtime daemon on real engines (no faux script), offline.
fn real_runtime_daemon(bed: &Bed) -> Daemon {
    let mut command = bed.runtime_daemon_command(&Value::Null, &bed.old_socket());
    command
        .env_remove("PRIME_AGENT_FAUX_SCRIPT")
        .envs(OFFLINE.iter().copied());
    let mut child = command.spawn().expect("spawn the runtime daemon");
    super::support::await_listening(&bed.runtime_socket(), &mut child);
    Daemon(child)
}

/// `command` sent as the TUI's transport sends it; the old frame its reply
/// becomes, as the TUI reads it.
async fn through_the_compat_layer(client: &mut Client, request: &str, command: &Value) -> Value {
    let kind = command["type"].as_str().unwrap().to_string();
    let typed: DaemonCommand = serde_json::from_value(command.clone()).unwrap();
    let mut frames =
        pa_tui::wire2_request_frames(request.to_string(), typed).expect("a mapped command");
    let outcome = match (frames.pop(), frames.pop()) {
        (Some(ClientFrame::Call { method, args, .. }), None) => client.call(&method, args).await,
        (Some(ClientFrame::Query { name, args, .. }), None) => client.query(&name, args).await,
        other => panic!("{kind} is one call or query: {other:?}"),
    };
    read_as_the_tui_does(&pa_tui::wire2_response_frame(&kind, outcome).unwrap())
}

/// What the runtime daemon builds for each create.
async fn runtime_builds(bed: &Bed, bearers: &Arc<Mutex<Vec<String>>>) -> Vec<Value> {
    let _daemon = real_runtime_daemon(bed);
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let mut parent: Option<PathBuf> = None;
    let mut builds = Vec::new();
    for (index, create) in creates().iter().enumerate() {
        let create = filled(create, bed.dir.path(), parent.as_deref());
        let created = through_the_compat_layer(&mut client, &format!("c{index}"), &create).await;
        assert_eq!(created["success"], true, "{created}");
        let active = created["data"]["activeSessionId"]
            .as_str()
            .unwrap()
            .to_string();
        let file = PathBuf::from(created["data"]["sessionFile"].as_str().unwrap());
        parent.get_or_insert_with(|| file.clone());
        let state = through_the_compat_layer(
            &mut client,
            &format!("s{index}"),
            &json!({ "type": "get_state", "activeSessionId": active }),
        )
        .await["data"]
            .clone();
        client
            .subscribe(&format!("agent/{active}/#"), Since::Now)
            .await;
        bearers.lock().unwrap().clear();
        super::run_turn(&mut client, &active, "hello").await;
        let sent = std::mem::take(&mut *bearers.lock().unwrap());
        let bash = through_the_compat_layer(
            &mut client,
            &format!("b{index}"),
            &json!({ "type": "execute_bash", "activeSessionId": active,
                     "command": MARKER_COMMAND }),
        )
        .await;
        assert_eq!(bash["success"], true, "{bash}");
        builds.push(built(bed, &file, parent.as_deref().unwrap(), &state, &sent));
    }
    for (index, create) in refused_creates().iter().enumerate() {
        let create = filled(create, bed.dir.path(), None);
        builds.push(through_the_compat_layer(&mut client, &format!("r{index}"), &create).await);
    }
    builds
}

/// The files under `dir` that hold `needle`.
fn written_under(dir: &Path, needle: &[u8]) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                pending.push(path);
            } else if kind.is_file()
                && std::fs::read(&path)
                    .is_ok_and(|bytes| bytes.windows(needle.len()).any(|window| window == needle))
            {
                found.push(path);
            }
        }
    }
    found
}

#[tokio::test(flavor = "multi_thread")]
async fn a_tui_create_builds_on_the_runtime_daemon_what_it_builds_on_the_old_daemon() {
    let (old_bed, runtime_bed) = (Bed::new(), Bed::new());
    let (old_origin, old_bearers) = completions();
    let (runtime_origin, runtime_bearers) = completions();
    seed(&old_bed, &old_origin);
    seed(&runtime_bed, &runtime_origin);
    let mut reference = OldReference::open("tui_create", &old_bed);
    let old: Vec<Value> = reference.take("builds", || old_builds(&old_bed, &old_bearers));
    let runtime = runtime_builds(&runtime_bed, &runtime_bearers).await;
    let pin = json!({ "provider": "openai", "id": "c_00000000e2e0work", "label": "work" });
    // The variants do what they say on the old daemon.
    assert_eq!(
        old.iter()
            .map(|build| json!([build["pin"], build["profile"], build["bearers"]]))
            .collect::<Vec<_>>()[..3],
        [
            json!([pin, null, ["Bearer key-work"]]),
            json!([pin, "work", ["Bearer key-work"]]),
            json!([null, null, ["Bearer key-given"]]),
        ]
    );
    assert_eq!(
        (
            &old[3]["model"],
            &old[3]["thinkingLevel"],
            &old[4]["parent"],
            &old[4]["rlmDepth"],
            &old[4]["pin"],
            &old[5]["dir"],
            &old[6]["marker"],
            [&old[7]["success"], &old[8]["success"]],
        ),
        (
            &json!(["openai", "e2e-think"]),
            &json!("high"),
            &json!("{parent}"),
            &json!(1),
            &pin,
            &json!("{bed}/elsewhere"),
            &json!("from-the-client unset"),
            [&json!(false), &json!(false)],
        )
    );
    // And the runtime daemon builds the same.
    let sent: Vec<Value> = creates().into_iter().chain(refused_creates()).collect();
    assert_eq!(runtime.len(), old.len());
    for (index, (old, runtime)) in old.iter().zip(&runtime).enumerate() {
        assert_eq!(runtime, old, "create {index}: {}", sent[index]);
    }
    // Its stores, its log and its transcripts included.
    let database = pa_core::session::store::database_path(&runtime_bed.agent_dir());
    let mut written = written_under(runtime_bed.dir.path(), b"key-given");
    written.extend(
        written_under(database.parent().unwrap(), b"key-given")
            .into_iter()
            .filter(|path| {
                path.to_string_lossy()
                    .starts_with(&*database.to_string_lossy())
            }),
    );
    assert_eq!(
        (database.exists(), written),
        (true, Vec::<PathBuf>::new()),
        "the runtime daemon wrote the API key"
    );
}

/// A session on an agent dir with no provider signed in opens on the runtime
/// daemon as on the old one, on the catalog's first model (the old worker's
/// fallback): its attach answers, where its build used to fail with "No
/// authenticated models available" and the TUI could not open it.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_opens_with_no_provider_signed_in() {
    let bed = Bed::new();
    let _daemon = real_runtime_daemon(&bed);
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = through_the_compat_layer(
        &mut client,
        "create",
        &json!({ "type": "create", "config": { "cwd": bed.dir.path() } }),
    )
    .await;
    assert_eq!(created["success"], true, "{created}");
    let active = created["data"]["activeSessionId"].as_str().unwrap();
    let state = through_the_compat_layer(
        &mut client,
        "state",
        &json!({ "type": "get_state", "activeSessionId": active }),
    )
    .await;
    assert_eq!(
        (state["success"].clone(), state["data"]["model"].is_object()),
        (json!(true), true),
        "{state}"
    );
}

/// A runtime daemon started from an agent's command (its own environment
/// carries `RLM_DEPTH`) hides it from its sessions, as the old supervisor
/// hid it from its workers: the `!` command a session runs sees none.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_never_sees_the_daemons_rlm_depth() {
    let bed = Bed::new();
    let (origin, _bearers) = completions();
    seed(&bed, &origin);
    let mut command = bed.runtime_daemon_command(&Value::Null, &bed.old_socket());
    command
        .env_remove("PRIME_AGENT_FAUX_SCRIPT")
        .envs(OFFLINE.iter().copied())
        .env("RLM_DEPTH", "3");
    let mut child = command.spawn().expect("spawn the runtime daemon");
    super::support::await_listening(&bed.runtime_socket(), &mut child);
    let _daemon = Daemon(child);
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = through_the_compat_layer(
        &mut client,
        "create",
        &filled(&creates()[0], bed.dir.path(), None),
    )
    .await;
    assert_eq!(created["success"], true, "{created}");
    let active = created["data"]["activeSessionId"].as_str().unwrap();
    let file = PathBuf::from(created["data"]["sessionFile"].as_str().unwrap());
    let bash = through_the_compat_layer(
        &mut client,
        "bash",
        &json!({ "type": "execute_bash", "activeSessionId": active,
                 "command": MARKER_COMMAND }),
    )
    .await;
    assert_eq!(bash["success"], true, "{bash}");
    assert_eq!(saved_with_bash(&bed.agent_dir(), &file).1, "unset unset");
}

/// A new session whose saved default is not available starts on the startup
/// chain's pick and says so, as the old worker's summary did: its state names
/// the default it could not start on and the model it runs on instead, the
/// notice the TUI shows as the session opens.
#[tokio::test(flavor = "multi_thread")]
async fn a_session_off_an_unavailable_default_says_which_model_it_runs_on() {
    let bed = Bed::new();
    // A private Prime Inference id no signed-in account lists: never rebuilt
    // from its provider's template.
    std::fs::write(
        bed.agent_dir().join("settings.json"),
        json!({ "defaultProvider": "prime-inference", "defaultModel": "internal/glm-5.3-fast" })
            .to_string(),
    )
    .unwrap();
    let _daemon = real_runtime_daemon(&bed);
    let (mut client, _) = Client::connect(&bed.runtime_socket()).await;
    let created = through_the_compat_layer(
        &mut client,
        "create",
        &json!({ "type": "create", "config": { "cwd": bed.dir.path() } }),
    )
    .await;
    assert_eq!(created["success"], true, "{created}");
    let active = created["data"]["activeSessionId"].as_str().unwrap();
    let state = through_the_compat_layer(
        &mut client,
        "state",
        &json!({ "type": "get_state", "activeSessionId": active }),
    )
    .await;
    assert_eq!(state["success"], true, "{state}");
    let state = state["data"].clone();
    assert_eq!(
        state["modelFallbackMessage"],
        json!(format!(
            "Default model prime-inference/internal/glm-5.3-fast is not available. Using {}/{}",
            state["model"]["provider"].as_str().unwrap(),
            state["model"]["id"].as_str().unwrap()
        )),
        "{state}"
    );
}
