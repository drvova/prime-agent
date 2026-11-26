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
//!   form, in the list's first line.
//! - **The verdict.** What still differs is listed, frame by frame, in
//!   `<name>.diff` beside the script, and must be exactly that: each line a
//!   difference between the two daemons that is understood (a frame only
//!   one of them sends, or a field that differs). A new difference, or one
//!   that went away, fails the test; `PA_BLESS_INBOUND_GOLDENS=1` rewrites
//!   the lists after a deliberate change.
//! - **What this cannot replay.** The recordings' replies (`attach`,
//!   `get_state`, `get_messages`, `get_commands`, `get_model_catalog`,
//!   `get_session_stats`, `heartbeats_list`, `list_kernel_bash`,
//!   `roster_subscribe`, `detach`) and `roster_update` frames have no wire2
//!   form yet (phases 8c and 8d): only the session stream is compared.

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

/// The recorded supervisor stream's `session_event` frames.
fn recorded(name: &str) -> Vec<Value> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../pa-tui/tests/goldens/inbound")
        .join(format!("{name}.jsonl"));
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{}: {error}", path.display()))
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).expect("a recorded line")["frame"].clone())
        .filter(|frame| frame["type"] == "session_event")
        .collect()
}

/// Runs scenario `name` on the runtime daemon; the old frames its session's
/// stream became through the compat layer, paths normalized as the
/// recording's were.
async fn replay(name: &str) -> Vec<Value> {
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
    let _daemon = bed.runtime_daemon(&script);
    let steps = scenario["steps"].as_array().unwrap();
    if steps.is_empty() {
        return Vec::new();
    }
    let mut session = Attached::open(&bed, &cwd).await;
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
    let frames = pa_tui::wire2_live_frames(&session.events("stream")).expect("mapped frames");
    let agent_dir = bed.agent_dir().display().to_string();
    let root = root.display().to_string();
    let checkout =
        pa_types::platform::canonicalize(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."))
            .unwrap()
            .display()
            .to_string();
    frames
        .into_iter()
        .map(|frame| {
            let text = frame
                .to_string()
                .replace(&agent_dir, &format!("/tmp/pa-golden/{prefix}/agent"))
                .replace(&root, "/tmp/pa-golden")
                .replace(&checkout, "/home/user/prime-agent");
            serde_json::from_str(&text).unwrap()
        })
        .collect()
}

/// The numbers the recordings' comparison masks by key.
const VOLATILE: [&str; 15] = [
    "timestamp",
    "workerPid",
    "supervisorPid",
    "pid",
    "durationMs",
    "duration_ms",
    "queued_ms",
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
                "input" | "output" | "cacheRead" | "cacheWrite" | "totalTokens"
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
                .filter(|(name, _)| {
                    !(key == Some("meta") && matches!(name.as_str(), "cursor" | "id" | "sequence"))
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
    let first_id = frames.first().map_or_else(
        || "-".to_string(),
        |frame| masked_text(frame["meta"]["id"].as_str().unwrap_or_default()),
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

/// One frame's signature: its kind, and the role of the row it carries.
fn signature(frame: &Value) -> String {
    let event = &frame["event"];
    let mut kind = event["type"].as_str().unwrap_or_default().to_string();
    if let Some(role) = event["message"]["role"].as_str() {
        kind = format!("{kind} {role}");
    }
    kind
}

/// Where two masked values differ: one line per differing leaf, by path.
fn leaf_diffs(path: &str, golden: &Value, runtime: &Value, out: &mut Vec<String>) {
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
                    out,
                );
            }
        }
        (Value::Array(left), Value::Array(right)) if left.len() == right.len() => {
            for (index, (left, right)) in left.iter().zip(right).enumerate() {
                leaf_diffs(&format!("{path}[{index}]"), left, right, out);
            }
        }
        _ if golden == runtime => {}
        _ => out.push(format!("{path}: golden {golden} | runtime {runtime}")),
    }
}

/// The differences between the recorded frames and the replayed ones, under
/// the mask: the frames are aligned by signature (a longest common
/// subsequence), a frame only one side has is one line, and a pair that
/// differs is one line per differing leaf.
fn differences(golden: &[Value], runtime: &[Value]) -> Vec<String> {
    let summary = format!(
        "meta: golden {} | runtime {}",
        numbering(golden),
        numbering(runtime)
    );
    let (golden, runtime): (Vec<Value>, Vec<Value>) = (
        runs_collapsed(golden)
            .iter()
            .map(|frame| masked(frame, None))
            .collect(),
        runs_collapsed(runtime)
            .iter()
            .map(|frame| masked(frame, None))
            .collect(),
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
    let mut out = vec![summary];
    let (mut i, mut j) = (0, 0);
    while i < left.len() || j < right.len() {
        if i < left.len() && j < right.len() && left[i] == right[j] {
            let mut leaves = Vec::new();
            leaf_diffs("", &golden[i], &runtime[j], &mut leaves);
            for leaf in leaves {
                out.push(format!("~ {}: {leaf}", left[i]));
            }
            i += 1;
            j += 1;
        } else if j < right.len() && (i == left.len() || table[i][j + 1] >= table[i + 1][j]) {
            out.push(format!("+ {}: {}", right[j], runtime[j]["event"]));
            j += 1;
        } else {
            out.push(format!("- {}: {}", left[i], golden[i]["event"]));
            i += 1;
        }
    }
    out
}

/// Replays `name` and checks what differs from its recording is exactly the
/// committed list.
async fn verify(name: &str) {
    let runtime = replay(name).await;
    let found = differences(&recorded(name), &runtime);
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
