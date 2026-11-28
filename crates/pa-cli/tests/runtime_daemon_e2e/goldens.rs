//! The inbound goldens replayed on the runtime daemon (phase 8b-3): each of
//! pa-tui's five recorded supervisor streams (`pa-tui/tests/goldens/inbound`)
//! is driven again, from the faux script and the inputs its turns show
//! (`tests/fixtures/inbound_goldens/<name>.json`), against the real runtime
//! daemon; the events its session's stream carried go through pa-tui's
//! `wire2_compat` (`pa_tui::wire2_live_frames`, the layer's test seam) and
//! the old frames that come out are compared with the recorded
//! `session_event` frames under the recordings' mask.
//!
//! - **The harness.** pa-tui links nothing above pa-types, so the daemon is
//!   driven here, in the composition root's tests, and only the layer's pure
//!   mapping crosses into pa-tui, through one `#[doc(hidden)]` function: the
//!   frame log is the events this client received from the daemon.
//! - **The mask.** As the recordings were normalized and compared (#228, and
//!   #277's re-recording): the test's directories read `/tmp/pa-golden` (the
//!   agent dir and the cwd as the recording laid them out) and the checkout
//!   `/home/user/prime-agent`; then ids (8, 12 and 32 hex digits, UUIDs),
//!   ISO times, faux and tool call ids, 13-digit numbers and the volatile
//!   keys' numbers (`timestamp`, `emittedAt`, pids and durations) are masked.
//!   The frame's `meta.cursor` is left out, as the layer documents (wire2
//!   has no cursor generation).
//! - **What a replay cannot reproduce.** #277 compared two recordings made
//!   on one machine; a replay runs elsewhere, so three things the recording
//!   fixed by its machine and its timing are compared by their form instead
//!   of their value: the token counts (`usage`, `contextUsage` and
//!   `lastRequest` readings, a compaction's `tokensBefore`) are the faux
//!   provider's estimate of the request's length, and the system prompt
//!   names the checkout, the agent dir and the cwd; a reply's run of
//!   `message_update` frames and a summary's run of
//!   `compaction_summary_delta` frames are cut by the flush timer and the
//!   faux provider's chunking, so a run compares as its last update and its
//!   summary as one delta; and `meta.id` and `meta.sequence` (the numbers a
//!   superseded update consumed follow the same timer) are compared by their
//!   form, in the list's first line. `attach_paging`'s recording ran where
//!   Amazon Bedrock was signed in (its machine's AWS credentials, which a
//!   replay's bed scrubs): its reopened session, whose saved faux model
//!   does not restore, fell back to a Bedrock model and said so
//!   (`modelFallbackMessage`). Its script's `env` gives the replay a
//!   Bedrock profile, so the fallback lands where the recording's did. A
//!   collect's `queued_ms` is only there
//!   when the child waited a millisecond or more for its start (both
//!   daemons leave a zero out: pa-core `RlmCollectResult`), which a replay's
//!   timing decides, so an entry that holds a count is left out on both
//!   sides; one that holds anything else stays, and differs.
//! - **The verdict.** What still differs is listed, frame by frame, in
//!   `<name>.diff` beside the script, and must be exactly that: each line a
//!   difference between the two daemons that is understood (a frame only
//!   one of them sends, or a field that differs). A new difference, or one
//!   that went away, fails the test; `PA_BLESS_INBOUND_GOLDENS=1` rewrites
//!   the lists after a deliberate change.
//! - **The replies.** The session reads the recordings answered through
//!   the compat layer (`attach`, `roster_subscribe`, `get_state`,
//!   `get_messages`) are sent through it too (`pa_tui::wire2_request_frames`),
//!   and the old responses
//!   their replies become (`pa_tui::wire2_response_frame`) are compared,
//!   each with the recording's first response to the same command, under
//!   the same mask; their differences follow the frames' in the list, a
//!   leaf per line, an array aligned on its equal items (a row one side
//!   has more is one line). The
//!   replay attaches and reads the state as the session opens, as the
//!   recording did; `attach_paging` opens the recording's seeded transcript
//!   (`seed` in its script: 260 question-and-answer turns) and pages to its
//!   start, as the TUI scrolled there.
//! - **The feeds.** The replay follows the roster and the agent traffic
//!   through the layer as the TUI's agents view does, and waits until the
//!   roster it keeps (its `roster_subscribe` reply, the `roster_update`
//!   frames the layer makes of the feed applied) is the one `runtime.roster`
//!   reads. Which updates a turn makes follows the feed's timing, as a
//!   reply's run of updates follows the flush timer, so what is compared is
//!   the roster each side's client ends with (the recording's reply with its
//!   updates applied), a leaf per line, after the stream's lines; then the
//!   `agent_traffic` frames, aligned by kind and outcome (the recordings
//!   followed no traffic, and their sessions sent no agent message: a
//!   traffic frame is a difference). A live event's id counts the feeds'
//!   events too, so the first line compares it by its form.
//! - **The scenario's reads.** A scenario's `reads` (`prompt_stream`'s:
//!   `get_model_catalog` and `get_commands`, phase 8d-1b) are sent through
//!   the layer after its steps, as the recording's TUI sent them after its
//!   first prompt, and their replies compared as the session reads' are. A
//!   model catalog's listing (`models`, `configuredProviders`, `billing`)
//!   is the one the daemon's machine carries, so it compares by its form:
//!   whether it lists anything (the recordings' listed nothing). The replay
//!   runs offline (`PI_OFFLINE=1`), as the recordings did, so no catalog
//!   is fetched.
//! - **What this cannot replay.** The recordings' other replies
//!   (`get_session_stats`, `heartbeats_list`, `detach`) have no wire2 form
//!   yet (phase 8d). `list_kernel_bash` (phase 8d-4a) is sent after the
//!   roster's, before the first turn, and its reply compared as the session
//!   reads' are.
use std::path::Path;

use serde_json::{json, Map, Value};

use super::turns::Attached;
use super::{ok, Bed, Client};

/// The fixtures beside this suite.
fn fixture_path(name: &str, extension: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/inbound_goldens")
        .join(format!("{name}.{extension}"))
}

/// The session reads whose replies the replay compares, in the order it
/// sends them.
const READS: [&str; 5] = [
    "attach",
    "roster_subscribe",
    "list_kernel_bash",
    "get_state",
    "get_messages",
];

/// The reads beside the session's turns a scenario's `reads` names (phase
/// 8d-1b): sent after its steps, as the recording's TUI sent them after its
/// first prompt, and compared as the session reads are.
const SCENARIO_READS: [&str; 2] = ["get_model_catalog", "get_commands"];

/// The topics the compat layer follows the agents view's feeds on.
const FEED_TOPICS: [&str; 2] = ["runtime/roster", "runtime/traffic"];

/// The roster a client keeps: the entries of its `roster_subscribe` reply
/// (`followed`), each of the `updates` (`roster_update` frames, or their
/// payloads) applied in order, by agent, in the order they first appeared.
fn kept_roster<'a>(followed: &Value, updates: impl IntoIterator<Item = &'a Value>) -> Vec<Value> {
    let mut kept: Vec<Value> = followed["data"]["roster"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    for update in updates {
        for entry in update["changed"].as_array().into_iter().flatten() {
            match kept
                .iter_mut()
                .find(|kept| kept["agentId"] == entry["agentId"])
            {
                Some(kept) => kept.clone_from(entry),
                None => kept.push(entry.clone()),
            }
        }
        for agent in update["removed"].as_array().into_iter().flatten() {
            kept.retain(|kept| kept["agentId"] != *agent);
        }
    }
    kept
}

/// Every frame the recorded supervisor stream received.
fn recorded_frames(name: &str) -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../pa-tui/tests/goldens/inbound")
        .join(format!("{name}.jsonl"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a recorded line")["frame"].clone())
        .collect()
}

/// The recorded supervisor stream's frames of the `kinds`.
fn recorded(name: &str, kinds: &[&str]) -> Vec<Value> {
    recorded_frames(name)
        .into_iter()
        .filter(|frame| kinds.iter().any(|kind| frame["type"] == *kind))
        .collect()
}

/// The recording's first response to each of [`READS`] and
/// [`SCENARIO_READS`], by command, without the request id the supervisor's
/// link added.
fn recorded_replies(name: &str) -> Vec<(String, Value)> {
    let frames = recorded_frames(name);
    READS
        .iter()
        .chain(&SCENARIO_READS)
        .filter_map(|command| {
            let mut frame = frames
                .iter()
                .find(|frame| frame["type"] == "response" && frame["command"] == *command)?
                .clone();
            frame.as_object_mut()?.remove("id");
            Some(((*command).to_string(), frame))
        })
        .collect()
}

/// Writes the transcript `seed` describes under `root/seed`: the
/// recording's header, model and level rows, then `turns` question and
/// answer pairs (`u0001`/`a0001`, ...) a millisecond apart; its path.
fn write_seed(root: &Path, cwd: &Path, seed: &Value) -> std::path::PathBuf {
    let id = seed["session"].as_str().expect("the seed's session id");
    let created = seed["created"].as_str().expect("the seed's header time");
    let mut ms = seed["firstTimestamp"]
        .as_u64()
        .expect("the seed's first time");
    let mut rows = vec![
        json!({ "type": "session", "version": 3, "id": id, "timestamp": created,
                "cwd": cwd }),
        json!({ "type": "model_change", "id": "seedmodl", "parentId": null,
                "timestamp": created, "provider": "faux", "modelId": "faux-1" }),
        json!({ "type": "thinking_level_change", "id": "seedthnk", "parentId": "seedmodl",
                "timestamp": created, "thinkingLevel": "off" }),
    ];
    let mut parent = "seedthnk".to_string();
    for turn in 1..=seed["turns"].as_u64().expect("the seed's turn count") {
        let (user, assistant) = (format!("u{turn:04}"), format!("a{turn:04}"));
        rows.push(json!({ "type": "message", "id": user, "parentId": parent,
            "timestamp": created, "message": { "role": "user",
                "content": [{ "type": "text", "text": format!("q{turn:03} asked") }],
                "timestamp": ms } }));
        rows.push(
            json!({ "type": "message", "id": assistant, "parentId": user,
            "timestamp": created, "message": { "role": "assistant",
                "content": [{ "type": "text", "text": format!("answer {turn:03}") }],
                "api": "faux:1", "provider": "faux", "model": "faux-1",
                "usage": { "input": 1, "output": 1, "cacheRead": 0, "cacheWrite": 0,
                           "totalTokens": 2, "cost": { "input": 0, "output": 0,
                           "cacheRead": 0, "cacheWrite": 0, "total": 0 } },
                "stopReason": "stop", "timestamp": ms + 1 } }),
        );
        parent = assistant;
        ms += 2;
    }
    let dir = root.join("seed");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{id}.jsonl"));
    let mut text = rows
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    std::fs::write(&path, text).unwrap();
    path
}

/// Sends `command` through the compat layer on `session`'s connection (a
/// read's query, or a feed's subscription and query); the old response its
/// reply becomes.
async fn read(session: &mut Attached, command: &Value) -> Value {
    let command: pa_types::daemon::DaemonCommand =
        serde_json::from_value(command.clone()).expect("a TUI command");
    let name = serde_json::to_value(&command).unwrap()["type"]
        .as_str()
        .unwrap()
        .to_string();
    let request = session.request_id();
    let frames = pa_tui::wire2_request_frames(request, command).expect("a mapped command");
    let outcome = match frames.as_slice() {
        [query] => session.request(query).await,
        [_, _] => session.follow(&frames).await,
        other => panic!("the layer carries {name} as {other:?}"),
    };
    pa_tui::wire2_response_frame(&name, outcome).expect("a response frame")
}

/// Reads `session`'s frames until the roster the client keeps (the
/// `followed` reply to its `roster_subscribe`, each later update applied)
/// has caught up with the daemon: its entry for each agent is the one
/// `runtime.roster` reads now (the feed composes its entries after the
/// events that make them due, so the last ones may still be on their way
/// when a turn is done).
async fn roster_settled(session: &mut Attached, followed: &Value) {
    loop {
        let id = session.request_id();
        let roster = match session
            .request(&pa_types::wire2::ClientFrame::Query {
                id,
                name: "runtime.roster".to_string(),
                args: json!({}),
            })
            .await
        {
            pa_types::wire2::Outcome::Ok { result } => result["roster"].clone(),
            pa_types::wire2::Outcome::Error { message, .. } => panic!("runtime.roster: {message}"),
        };
        let events = session.feed_events("runtime/roster");
        let kept = kept_roster(followed, events.iter().map(|event| &event.event.payload));
        let caught_up = roster.as_array().is_some_and(|roster| {
            roster.len() == kept.len() && roster.iter().all(|entry| kept.contains(entry))
        });
        if caught_up {
            return;
        }
        let seen = session.feed_events("runtime/roster").len();
        session
            .until(|session| session.feed_events("runtime/roster").len() > seen)
            .await;
    }
}

/// What a replay received through the compat layer: the stream's frames,
/// the roster the client keeps once the feed settled, the traffic frames,
/// and the responses to the session reads, by command.
struct Replayed {
    frames: Vec<Value>,
    roster: Vec<Value>,
    traffic: Vec<Value>,
    replies: Vec<(String, Value)>,
    /// The scenario's own reads ([`SCENARIO_READS`]) it sent.
    reads: Vec<String>,
}

/// Runs scenario `name` on the runtime daemon; the old frames its session's
/// stream became and the old responses to its reads, through the compat
/// layer, paths normalized as the recording's were.
#[expect(
    clippy::too_many_lines,
    reason = "a replay's steps read in the order the recording took them"
)]
async fn replay(name: &str) -> Replayed {
    let scenario: Value =
        serde_json::from_str(&std::fs::read_to_string(fixture_path(name, "json")).unwrap())
            .unwrap();
    let bed = Bed::new();
    let root = bed.dir.path();
    let prefix = scenario["prefix"].as_str().unwrap();
    let cwd = root.join(prefix).join("project");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::write(
        bed.agent_dir().join("settings.json"),
        scenario["settings"].to_string(),
    )
    .unwrap();
    let marker = root.join("subagent_roster.marker");
    let script: Value = serde_json::from_str(
        &scenario["script"]
            .to_string()
            .replace("{marker}", &marker.display().to_string()),
    )
    .unwrap();
    // Offline, as the recordings ran: the model catalog is the one the
    // daemon carries, never a fetched one. A scenario's `env` is what the
    // recording's machine carried beside it.
    let mut env = vec![("PI_OFFLINE", "1")];
    if let Some(recorded) = scenario["env"].as_object() {
        env.extend(
            recorded
                .iter()
                .map(|(key, value)| (key.as_str(), value.as_str().unwrap())),
        );
    }
    let _daemon = bed.runtime_daemon_with(&script, &env);
    let steps = scenario["steps"].as_array().unwrap();
    let mut session = match scenario.get("seed") {
        Some(seed) => {
            let path = write_seed(root, &cwd, seed);
            Attached::create(&bed, json!({ "path": path, "cwd": cwd })).await
        }
        None => Attached::open(&bed, &cwd).await,
    };
    // The TUI's attach as the recording sent it: its client id and
    // capabilities, then its state read.
    let recorded_reads = recorded_replies(name);
    let recorded_client = &recorded_reads
        .iter()
        .find(|(command, _)| command == "attach")
        .expect("the recording attached")
        .1["data"]["client"];
    let mut replies = Vec::new();
    let id = session.id.clone();
    let attach = read(
        &mut session,
        &json!({ "type": "attach", "activeSessionId": id,
                 "clientId": recorded_client["id"],
                 "capabilities": recorded_client["capabilities"] }),
    )
    .await;
    replies.push(("attach".to_string(), attach.clone()));
    // The agents view's feeds, as the TUI followed the roster after its
    // attach (the recording has no traffic: its sessions sent no message).
    let roster = read(&mut session, &json!({ "type": "roster_subscribe" })).await;
    replies.push(("roster_subscribe".to_string(), roster.clone()));
    read(&mut session, &json!({ "type": "agent_traffic_subscribe" })).await;
    // The agents view's kernel activity poll, before the first turn (phase 8d-4a).
    let kernel_bash = read(
        &mut session,
        &json!({ "type": "list_kernel_bash", "activeSessionId": id }),
    )
    .await;
    replies.push(("list_kernel_bash".to_string(), kernel_bash));
    let state = read(
        &mut session,
        &json!({ "type": "get_state", "activeSessionId": id }),
    )
    .await;
    replies.push(("get_state".to_string(), state));
    for step in steps {
        if let Some(text) = step["prompt"].as_str() {
            let accepted = session.publish(text, None).await;
            session.done(&accepted["task"]).await;
            if step["until"] == "rest" {
                session.call("session.wait_idle", json!({})).await;
            }
        } else if let Some(text) = step["queued"].as_str() {
            // Admitted while the lane is held, so it waits as the recording's
            // input did behind the running turn; then it runs.
            let (mut holder, _) = Client::connect(&bed.runtime_socket()).await;
            let hold = ok(holder
                .call(
                    "session.pause_input",
                    json!({ "session": session.id, "key": "golden" }),
                )
                .await);
            let accepted = session.publish(text, step["priority"].as_str()).await;
            ok(holder
                .call(
                    "session.release_input",
                    json!({ "session": session.id, "pauseId": hold["pauseId"] }),
                )
                .await);
            session.done(&accepted["task"]).await;
        }
    }
    let reads: Vec<String> = scenario["reads"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|read| read.as_str().expect("a read's command").to_string())
        .collect();
    for command in &reads {
        assert!(
            SCENARIO_READS.contains(&command.as_str()),
            "{name}: {command} is not a scenario read"
        );
        let reply = read(
            &mut session,
            &json!({ "type": command, "activeSessionId": id }),
        )
        .await;
        replies.push((command.clone(), reply));
    }
    roster_settled(&mut session, &roster).await;
    // A scrolled-up transcript pages back to its start, as the TUI's view
    // asked for it: the head before the window's first row, on the
    // attached branch.
    let snapshot = &attach["data"]["snapshot"];
    if let Some(before) = snapshot["historyBeforeEntryId"].as_str() {
        let page = read(
            &mut session,
            &json!({ "type": "get_messages", "activeSessionId": id,
                     "page": { "from": "start", "until": before, "limit": 256 },
                     "capabilities": recorded_client["capabilities"],
                     "historyLeafId": snapshot["state"]["leafId"] }),
        )
        .await;
        replies.push(("get_messages".to_string(), page));
    }
    let frames = pa_tui::wire2_live_frames(&session.events("stream")).expect("mapped frames");
    let feeds = pa_tui::wire2_live_frames(
        &FEED_TOPICS
            .iter()
            .flat_map(|topic| session.feed_events(topic))
            .collect::<Vec<_>>(),
    )
    .expect("mapped feed frames");
    let (updates, traffic): (Vec<Value>, Vec<Value>) = feeds
        .into_iter()
        .partition(|frame| frame["type"] == "roster_update");
    let followed = &replies
        .iter()
        .find(|(command, _)| command == "roster_subscribe")
        .expect("the replay followed the roster")
        .1;
    let roster = kept_roster(followed, &updates);
    let agent_dir = bed.agent_dir().display().to_string();
    let root = root.display().to_string();
    let checkout =
        pa_types::platform::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .unwrap()
            .display()
            .to_string();
    let normalized = |frame: Value| -> Value {
        let text = frame
            .to_string()
            .replace(&agent_dir, &format!("/tmp/pa-golden/{prefix}/agent"))
            .replace(&root, "/tmp/pa-golden")
            .replace(&checkout, "/home/user/prime-agent");
        serde_json::from_str(&text).unwrap()
    };
    Replayed {
        frames: frames.into_iter().map(normalized).collect(),
        roster: roster.into_iter().map(normalized).collect(),
        traffic: traffic.into_iter().map(normalized).collect(),
        replies: replies
            .into_iter()
            .map(|(command, reply)| (command, normalized(reply)))
            .collect(),
        reads,
    }
}

/// The numbers the recordings' comparison masks by key.
const VOLATILE: [&str; 14] = [
    "timestamp",
    "workerPid",
    "supervisorPid",
    "pid",
    "durationMs",
    "duration_ms",
    "lastUserPromptMs",
    "expiresAt",
    "emittedAt",
    "created",
    "modified",
    "lastActivityAt",
    "startedAt",
    "lastHeartbeatAt",
];

/// The token counts a replay cannot reproduce, by the object that holds
/// them.
fn is_token_count(parent: Option<&str>, key: &str) -> bool {
    match parent {
        Some("usage" | "lastRequest") => {
            matches!(
                key,
                "input"
                    | "output"
                    | "cacheRead"
                    | "cacheWrite"
                    | "totalTokens"
                    | "inputTokens"
                    | "outputTokens"
            )
        }
        Some("contextUsage") => matches!(key, "tokens" | "percent"),
        _ => key == "tokensBefore",
    }
}

/// `value` under the recordings' mask, the token counts as `#`, without
/// the frame's cursor, id and sequence.
fn masked(value: &Value, key: Option<&str>) -> Value {
    match value {
        Value::Object(object) => Value::Object(
            object
                .iter()
                .filter(|(name, item)| {
                    let unnumbered = key == Some("meta")
                        && matches!(name.as_str(), "cursor" | "id" | "sequence");
                    let timed = name.as_str() == "queued_ms" && item.is_u64();
                    !(unnumbered || timed)
                })
                .map(|(name, item)| {
                    let item = match item {
                        Value::Number(_) if is_token_count(key, name) => json!("#"),
                        _ => masked(item, Some(name)),
                    };
                    (name.clone(), item)
                })
                .collect::<Map<_, _>>(),
        ),
        Value::Array(items) => Value::Array(items.iter().map(|item| masked(item, None)).collect()),
        Value::String(text) => Value::String(masked_text(text)),
        Value::Number(number)
            if key.is_some_and(|key| VOLATILE.contains(&key))
                || number
                    .as_u64()
                    .is_some_and(|number| number > 100_000_000_000) =>
        {
            Value::String("N".to_string())
        }
        Value::Number(_) | Value::Bool(_) | Value::Null => value.clone(),
    }
}

/// `frames` with each run of one reply's `message_update` frames as its
/// last, and each run of `compaction_summary_delta` frames as one delta.
fn runs_collapsed(frames: &[Value]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::new();
    for frame in frames {
        let kind = &frame["event"]["type"];
        match out.last_mut() {
            Some(last) if *kind == "message_update" && last["event"]["type"] == *kind => {
                last.clone_from(frame);
            }
            Some(last) if *kind == "compaction_summary_delta" && last["event"]["type"] == *kind => {
                let delta = format!(
                    "{}{}",
                    last["event"]["delta"].as_str().unwrap_or_default(),
                    frame["event"]["delta"].as_str().unwrap_or_default()
                );
                last["event"]["delta"] = json!(delta);
            }
            _ => out.push(frame.clone()),
        }
    }
    out
}

/// How a side numbers and names its frames: whether the numbers rise, where
/// they start, and its first frame's id, masked (how many frames a reply's
/// updates take follows the flush timer).
fn numbering(frames: &[Value]) -> String {
    let sequences: Vec<u64> = frames
        .iter()
        .filter_map(|frame| frame["meta"]["sequence"].as_u64())
        .collect();
    // A live event's number counts every live event of the bus, the
    // feeds' too, so it is compared by its form.
    let first_id = frames.first().map_or_else(
        || "-".to_string(),
        |frame| {
            let id = frame["meta"]["id"].as_str().unwrap_or_default();
            match id.strip_prefix("live-") {
                Some(number) if number.bytes().all(|byte| byte.is_ascii_digit()) => {
                    "live-N".to_string()
                }
                _ => masked_text(id),
            }
        },
    );
    format!(
        "numbers rising {}, from {}, ids like {first_id}",
        sequences.windows(2).all(|pair| pair[0] < pair[1]),
        sequences
            .first()
            .map_or_else(|| "-".to_string(), u64::to_string),
    )
}

fn is_hex(byte: u8) -> bool {
    byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)
}

fn digits(text: &str) -> usize {
    text.bytes().take_while(u8::is_ascii_digit).count()
}

/// `text` with the recordings' masks applied: faux and tool call ids, ids,
/// ISO times, the durations a cell prints, then 13-digit numbers.
fn masked_text(text: &str) -> String {
    // Faux and tool call ids first: their hex part's length varies, and an
    // id mask that took part of one would leave the rest unmasked.
    let text = replace_matches(text, |rest| {
        let tail = rest
            .strip_prefix("tool:")
            .or_else(|| rest.strip_prefix("faux:"))?;
        let count = digits(tail);
        let hex_tail = tail[count..].strip_prefix(':').filter(|_| count > 0)?;
        let hex = hex_tail.bytes().take_while(|byte| is_hex(*byte)).count();
        (hex > 0).then(|| (5 + count + 1 + hex, "T".to_string()))
    });
    let text = masked_ids(&text);
    let text = replace_matches(&text, |rest| {
        let template = b"dddd-dd-ddTdd:dd:dd.dddZ";
        let bytes = rest.as_bytes();
        (bytes.len() >= template.len()
            && template.iter().zip(bytes).all(|(want, got)| match want {
                b'd' => got.is_ascii_digit(),
                other => other == got,
            }))
        .then(|| (template.len(), "ISO".to_string()))
    });
    let text = replace_matches(&text, |rest| {
        ["'duration_ms: ", "'queued_ms: "]
            .iter()
            .find_map(|prefix| {
                let count = digits(rest.strip_prefix(prefix)?);
                (count > 0).then(|| (prefix.len() + count, format!("{prefix}N")))
            })
    });
    let text = replace_matches(&text, |rest| {
        (digits(rest) >= 13).then(|| (13, "T".to_string()))
    });
    // A printed collect's `queued_ms` entry, when it holds a count: present
    // only when the child's start waited (see the module docs).
    let text = text.replace("\n    'queued_ms: N,", "");
    entries_sorted(&text)
}

/// `text` with each printed Scheme hash's entries (a run of lines that
/// start `    '`) in order: the kernel prints a hash in its process's hash
/// order, which a replay does not share.
fn entries_sorted(text: &str) -> String {
    if !text.contains("#<hashmap {") {
        return text.to_string();
    }
    let mut lines: Vec<&str> = Vec::new();
    let mut run: Vec<&str> = Vec::new();
    for line in text.split('\n') {
        if line.starts_with("    '") {
            run.push(line);
            continue;
        }
        run.sort_unstable();
        lines.append(&mut run);
        lines.push(line);
    }
    run.sort_unstable();
    lines.append(&mut run);
    lines.join("\n")
}

/// `text` with every leftmost, non-overlapping match of `at` (the length it
/// matches at a position, and its replacement) replaced.
fn replace_matches(text: &str, at: impl Fn(&str) -> Option<(usize, String)>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while let Some(next) = text[index..].chars().next() {
        if let Some((length, replacement)) = at(&text[index..]) {
            out.push_str(&replacement);
            index += length;
        } else {
            out.push(next);
            index += next.len_utf8();
        }
    }
    out
}

/// `text` with every id the recordings mask, a whole word of 12, 8 or 32
/// lower hex digits or a UUID, as `H`.
fn masked_ids(text: &str) -> String {
    let is_word = |c: char| c.is_ascii_alphanumeric() || c == '_';
    // The text as its words and the separators between them.
    let mut pieces: Vec<(bool, &str)> = Vec::new();
    let mut start = 0;
    let mut previous = None;
    for (index, c) in text.char_indices() {
        let word = is_word(c);
        if previous.is_some_and(|previous| previous != word) {
            pieces.push((!word, &text[start..index]));
            start = index;
        }
        previous = Some(word);
    }
    if let Some(word) = previous {
        pieces.push((word, &text[start..]));
    }
    let hex_word = |at: usize, length: usize| {
        pieces.get(at).is_some_and(|(word, piece)| {
            *word && piece.len() == length && piece.bytes().all(is_hex)
        })
    };
    let dash = |at: usize| pieces.get(at) == Some(&(false, "-"));
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while let Some((_, piece)) = pieces.get(index) {
        let uuid = hex_word(index, 8)
            && [4, 4, 4, 12].iter().enumerate().all(|(group, length)| {
                dash(index + 1 + 2 * group) && hex_word(index + 2 + 2 * group, *length)
            });
        if uuid {
            out.push('H');
            index += 9;
        } else if hex_word(index, 12) || hex_word(index, 8) || hex_word(index, 32) {
            out.push('H');
            index += 1;
        } else {
            out.push_str(piece);
            index += 1;
        }
    }
    out
}

/// A traffic frame's signature: its entry's kind and outcome.
fn traffic_signature(frame: &Value) -> String {
    format!(
        "agent_traffic {} {}",
        frame["entry"]["kind"].as_str().unwrap_or_default(),
        frame["entry"]["outcome"].as_str().unwrap_or_default()
    )
}

/// One frame's signature: its kind, and the role of the row it carries.
fn signature(frame: &Value) -> String {
    let event = &frame["event"];
    let mut kind = event["type"].as_str().unwrap_or_default().to_string();
    if let Some(role) = event["message"]["role"].as_str() {
        kind = format!("{kind} {role}");
    }
    kind
}

/// How [`leaf_diffs`] compares two arrays.
#[derive(Clone, Copy)]
enum Arrays {
    /// Item by item when their lengths agree, else as one leaf, both values
    /// whole (a frame's).
    ByIndex,
    /// Aligned on their equal items (a longest common subsequence), as a
    /// reply's transcript window is: a row one side has more is one line,
    /// not a shift of every row after it; between two equal items, the
    /// unequal ones pair up by position, the rest stand alone.
    Aligned,
}

/// Where two masked values differ: one line per differing leaf, by path.
fn leaf_diffs(path: &str, golden: &Value, runtime: &Value, arrays: Arrays, out: &mut Vec<String>) {
    match (golden, runtime) {
        (Value::Object(left), Value::Object(right)) => {
            let mut keys: Vec<&String> = left.keys().chain(right.keys()).collect();
            keys.sort();
            keys.dedup();
            for key in keys {
                leaf_diffs(
                    &format!("{path}.{key}"),
                    left.get(key).unwrap_or(&Value::Null),
                    right.get(key).unwrap_or(&Value::Null),
                    arrays,
                    out,
                );
            }
        }
        (Value::Array(left), Value::Array(right)) if matches!(arrays, Arrays::Aligned) => {
            aligned_diffs(path, left, right, out);
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                leaf_diffs(&format!("{path}[{index}]"), left, right, arrays, out);
            }
        }
        _ if golden == runtime => {}
        _ => out.push(format!("{path}: golden {golden} | runtime {runtime}")),
    }
}

/// [`leaf_diffs`] of two arrays aligned on their equal items (see
/// [`Arrays::Aligned`]); a path's index is the golden side's, or the
/// runtime's for an item only it has.
fn aligned_diffs(path: &str, left: &[Value], right: &[Value], out: &mut Vec<String>) {
    let mut table = vec![vec![0usize; right.len() + 1]; left.len() + 1];
    for i in (0..left.len()).rev() {
        for j in (0..right.len()).rev() {
            table[i][j] = if left[i] == right[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let (mut lefts, mut rights): (Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new());
    let flush = |lefts: &mut Vec<usize>, rights: &mut Vec<usize>, out: &mut Vec<String>| {
        for pair in 0..lefts.len().max(rights.len()) {
            match (lefts.get(pair), rights.get(pair)) {
                (Some(&l), Some(&r)) => {
                    leaf_diffs(
                        &format!("{path}[{l}]"),
                        &left[l],
                        &right[r],
                        Arrays::Aligned,
                        out,
                    );
                }
                (Some(&l), None) => {
                    out.push(format!("{path}[{l}]: golden {} | runtime -", left[l]));
                }
                (None, Some(&r)) => {
                    out.push(format!("{path}[{r}]: golden - | runtime {}", right[r]));
                }
                (None, None) => {}
            }
        }
        lefts.clear();
        rights.clear();
    };
    while i < left.len() || j < right.len() {
        if i < left.len() && j < right.len() && left[i] == right[j] {
            flush(&mut lefts, &mut rights, out);
            i += 1;
            j += 1;
        } else if j < right.len() && (i == left.len() || table[i][j + 1] >= table[i + 1][j]) {
            rights.push(j);
            j += 1;
        } else {
            lefts.push(i);
            i += 1;
        }
    }
    flush(&mut lefts, &mut rights, out);
}

/// The differences between the recorded stream frames and the replayed
/// ones, under the mask: a run of updates as its last ([`runs_collapsed`]),
/// then [`aligned_frames`].
fn differences(golden: &[Value], runtime: &[Value]) -> Vec<String> {
    let summary = format!(
        "meta: golden {} | runtime {}",
        numbering(golden),
        numbering(runtime)
    );
    let mut out = vec![summary];
    out.extend(aligned_frames(
        &runs_collapsed(golden),
        &runs_collapsed(runtime),
        signature,
        |frame| &frame["event"],
    ));
    out
}

/// The differences between two sides' frames under the mask: aligned by
/// `signature` (a longest common subsequence), a frame only one side has is
/// one line (its `body`), and a pair that differs is one line per differing
/// leaf.
fn aligned_frames(
    golden: &[Value],
    runtime: &[Value],
    signature: fn(&Value) -> String,
    body: fn(&Value) -> &Value,
) -> Vec<String> {
    let (golden, runtime): (Vec<Value>, Vec<Value>) = (
        golden.iter().map(|frame| masked(frame, None)).collect(),
        runtime.iter().map(|frame| masked(frame, None)).collect(),
    );
    let (left, right): (Vec<String>, Vec<String>) = (
        golden.iter().map(signature).collect(),
        runtime.iter().map(signature).collect(),
    );
    let mut table = vec![vec![0usize; right.len() + 1]; left.len() + 1];
    for i in (0..left.len()).rev() {
        for j in (0..right.len()).rev() {
            table[i][j] = if left[i] == right[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }
    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < left.len() || j < right.len() {
        if i < left.len() && j < right.len() && left[i] == right[j] {
            let mut leaves = Vec::new();
            leaf_diffs("", &golden[i], &runtime[j], Arrays::ByIndex, &mut leaves);
            for leaf in leaves {
                out.push(format!("~ {}: {leaf}", left[i]));
            }
            i += 1;
            j += 1;
        } else if j < right.len() && (i == left.len() || table[i][j + 1] >= table[i + 1][j]) {
            out.push(format!("+ {}: {}", right[j], body(&runtime[j])));
            j += 1;
        } else {
            out.push(format!("- {}: {}", left[i], body(&golden[i])));
            i += 1;
        }
    }
    out
}

/// The differences between the recorded responses to the session reads (and
/// the scenario's own `reads`) and the replayed ones, under the mask: a read
/// only one side answered is one line, and a pair that differs is one line
/// per differing leaf.
fn reply_differences(
    golden: &[(String, Value)],
    runtime: &[(String, Value)],
    reads: &[String],
) -> Vec<String> {
    let mut out = Vec::new();
    for command in READS
        .iter()
        .copied()
        .chain(reads.iter().map(String::as_str))
    {
        let find = |side: &[(String, Value)]| {
            side.iter()
                .find(|(read, _)| read == command)
                .map(|(_, reply)| masked(&catalog_by_form(command, reply), None))
        };
        match (find(golden), find(runtime)) {
            (Some(golden), Some(runtime)) => {
                let mut leaves = Vec::new();
                leaf_diffs("", &golden, &runtime, Arrays::Aligned, &mut leaves);
                out.extend(
                    leaves
                        .into_iter()
                        .map(|leaf| format!("~ reply {command}: {leaf}")),
                );
            }
            (Some(_), None) => out.push(format!("- reply {command}")),
            (None, Some(_)) => out.push(format!("+ reply {command}")),
            (None, None) => {}
        }
    }
    out
}

/// `reply` with a model catalog's listing (`models`, `configuredProviders`,
/// `billing`) by its form, `"listed"` or `"none"`: what the catalog lists is
/// the one the daemon's machine carries (the recordings' ran with none),
/// not the daemon's to decide. Any other reply as it is.
fn catalog_by_form(command: &str, reply: &Value) -> Value {
    let mut reply = reply.clone();
    if command == "get_model_catalog" {
        for key in ["models", "configuredProviders", "billing"] {
            let listing = &mut reply["data"][key];
            let listed = match listing {
                Value::Array(items) => !items.is_empty(),
                Value::Object(items) => !items.is_empty(),
                _ => continue,
            };
            *listing = json!(if listed { "listed" } else { "none" });
        }
    }
    reply
}

/// Replays `name` and checks what differs from its recording is exactly the
/// committed list.
async fn verify(name: &str) {
    let runtime = replay(name).await;
    let mut found = differences(&recorded(name, &["session_event"]), &runtime.frames);
    // The roster each side's client ends with: the recording's reply and
    // updates, the replay's once its feed settled.
    let golden_replies = recorded_replies(name);
    let golden_roster = golden_replies
        .iter()
        .find(|(command, _)| command == "roster_subscribe")
        .map_or_else(Vec::new, |(_, reply)| {
            kept_roster(reply, &recorded(name, &["roster_update"]))
        });
    let mut roster = Vec::new();
    leaf_diffs(
        "",
        &masked(&Value::Array(golden_roster), None),
        &masked(&Value::Array(runtime.roster.clone()), None),
        Arrays::Aligned,
        &mut roster,
    );
    found.extend(
        roster
            .into_iter()
            .map(|leaf| format!("~ roster kept: {leaf}")),
    );
    found.extend(aligned_frames(
        &recorded(name, &["agent_traffic"]),
        &runtime.traffic,
        traffic_signature,
        |frame| frame,
    ));
    found.extend(reply_differences(
        &golden_replies,
        &runtime.replies,
        &runtime.reads,
    ));
    let path = fixture_path(name, "diff");
    let mut text = found.join("\n");
    text.push('\n');
    if std::env::var_os("PA_BLESS_INBOUND_GOLDENS").is_some() {
        std::fs::write(&path, &text).unwrap();
        return;
    }
    let listed = std::fs::read_to_string(&path).unwrap_or_default();
    let (found, listed): (Vec<&str>, Vec<&str>) =
        (text.lines().collect(), listed.lines().collect());
    assert_eq!(
        found
            .iter()
            .filter(|line| !listed.contains(line))
            .collect::<Vec<_>>(),
        listed
            .iter()
            .filter(|line| !found.contains(line))
            .collect::<Vec<_>>(),
        "{name}: the replay's differences from the recording changed: new (left) and gone \
         (right); the list in full: {found:#?}"
    );
    assert_eq!(found, listed, "{name}: the differences' order changed");
}

#[tokio::test(flavor = "multi_thread")]
async fn the_prompt_stream_golden_replays_on_the_runtime_daemon() {
    verify("prompt_stream").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_eval_cell_golden_replays_on_the_runtime_daemon() {
    verify("eval_cell").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_compaction_golden_replays_on_the_runtime_daemon() {
    verify("compaction").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_subagent_roster_golden_replays_on_the_runtime_daemon() {
    verify("subagent_roster").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_attach_paging_golden_replays_on_the_runtime_daemon() {
    verify("attach_paging").await;
}
