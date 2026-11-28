//! A session's workspace checkpoints on the runtime daemon (phase 8d-2d):
//! two turns write files through their cells, and a `navigate_tree` with
//! `restoreFiles` back onto the second turn's prompt, sent through pa-tui's
//! `wire2_compat`, answers as the old daemon (the `pa-daemon` supervisor)
//! answers it on the same turns, and leaves the same files on disk: the
//! workspace as it was before that turn.
//!
//! The old daemon's side is frozen: its answers are read from a golden
//! ([`super::old_reference`]).

use std::path::Path;

use serde_json::{json, Value};

use super::info::runtime_answers;
use super::old_reference::OldReference;
use super::turns::{cell, Attached};
use super::Bed;

/// The runtime daemon runs offline, as the old daemon did for its golden.
const OFFLINE: &[(&str, &str)] = &[("PI_OFFLINE", "1")];

/// Two turns, each a cell that writes files and a reply.
fn responses() -> Value {
    json!([
        cell("Writing one.", "(bash \"echo one > a.txt\")"),
        "wrote one",
        cell(
            "Writing two.",
            "(bash \"echo two > a.txt; echo new > b.txt\")"
        ),
        "wrote two",
    ])
}

/// The files the turns write, as `workspace` holds them.
fn files(workspace: &Path) -> Value {
    let read = |name: &str| std::fs::read_to_string(workspace.join(name)).ok();
    json!({ "a.txt": read("a.txt"), "b.txt": read("b.txt") })
}

/// The navigation back onto the prompt with text `prompt`, among `forkable`
/// (a `get_user_messages_for_forking` answer's data).
fn back_onto(forkable: &Value, prompt: &str) -> Value {
    let entry = forkable["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["text"] == prompt)
        .unwrap_or_else(|| panic!("no prompt {prompt:?} in {forkable}"))["entryId"]
        .clone();
    json!({ "type": "navigate_tree", "targetId": entry, "summarize": false,
            "restoreFiles": true })
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_restores_the_files_as_the_old_daemons_does() {
    let bed = Bed::new();
    let old: (Value, Value, Value) = OldReference::open("checkpoint_rewind", &bed).take("rewind");

    let workspace = bed.dir.path().join("runtime-project");
    std::fs::create_dir_all(&workspace).unwrap();
    let _daemon = bed.runtime_daemon_with(&json!({ "responses": responses() }), OFFLINE);
    let mut session = Attached::open(&bed, &workspace).await;
    for prompt in ["first", "second"] {
        let accepted = session.publish(prompt, None).await;
        session.done(&accepted["task"]).await;
    }
    let written = files(&workspace);
    let id = session.id.clone();
    let (mut client, _) = super::Client::connect(&bed.runtime_socket()).await;
    let forkable = runtime_answers(
        &mut client,
        &id,
        vec![json!({ "type": "get_user_messages_for_forking" })],
    )
    .await;
    let rewound = runtime_answers(
        &mut client,
        &id,
        vec![back_onto(&forkable[0]["data"], "second")],
    )
    .await;
    let restored = files(&workspace);

    let expected = (
        json!({ "a.txt": "two\n", "b.txt": "new\n" }),
        json!({ "type": "response", "command": "navigate_tree", "success": true,
                "data": { "cancelled": false, "editorText": "second",
                          "files": { "restored": 2, "conflicts": [] } } }),
        json!({ "a.txt": "one\n", "b.txt": null }),
    );
    assert_eq!(old, expected);
    assert_eq!((written, rewound[0].clone(), restored), expected);
}
