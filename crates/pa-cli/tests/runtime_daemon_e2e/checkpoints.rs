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
use super::saved_list::old_list;
use super::switches::read_as_the_tui_does;
use super::turns::{cell, Attached};
use super::Bed;

/// Both daemons run offline.
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

/// The old daemon's answer to the navigation, and the files after it.
fn old_rewind(bed: &Bed) -> (Value, Value, Value) {
    let workspace = bed.dir.path().join("old-project");
    std::fs::create_dir_all(&workspace).unwrap();
    let script = bed.dir.path().join("old-script.json");
    std::fs::write(
        &script,
        json!({ "engine": "faux", "modelId": "faux-1", "modelName": "Faux Model",
                "reasoning": false, "contextWindow": 128_000, "responses": responses() })
        .to_string(),
    )
    .unwrap();
    let _daemon = bed.old_daemon_with(OFFLINE);
    let socket = bed.old_socket();
    let created = old_list(
        &socket,
        "create",
        &json!({ "type": "create", "config": {
            "cwd": workspace, "sessionDir": bed.agent_dir().join("sessions"), "script": script,
        } }),
    );
    let created = created.last().unwrap();
    assert_eq!(created["success"], true, "{created}");
    let active = created["data"]["id"].as_str().unwrap().to_string();
    let send = |id: &str, mut command: Value| {
        command["activeSessionId"] = json!(active);
        old_list(&socket, id, &command).pop().unwrap()
    };
    for prompt in ["first", "second"] {
        let ran = send(
            prompt,
            json!({ "type": "prompt_and_wait", "message": prompt }),
        );
        assert_eq!(ran["success"], true, "{ran}");
    }
    let written = files(&workspace);
    let forkable = send(
        "forkable",
        json!({ "type": "get_user_messages_for_forking" }),
    );
    let rewound = send("back", back_onto(&forkable["data"], "second"));
    (written, read_as_the_tui_does(&rewound), files(&workspace))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_rewind_restores_the_files_as_the_old_daemons_does() {
    let bed = Bed::new();
    let mut reference = OldReference::open("checkpoint_rewind", &bed);
    let old: (Value, Value, Value) = reference.take("rewind", || old_rewind(&bed));

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
