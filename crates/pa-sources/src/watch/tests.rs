use pa_types::event_log::{Topic, Trust};
use serde_json::json;

use super::{WatchOptions, WatchSource};
use crate::test_support::{context, PATIENCE};
use crate::{source_event, Source};

fn options(options: serde_json::Value) -> WatchOptions {
    serde_json::from_value(options).expect("valid watch options")
}

#[tokio::test]
async fn a_burst_of_matching_changes_is_published_once() {
    let folder = tempfile::tempdir().unwrap();
    let root = pa_types::platform::canonicalize(folder.path()).unwrap();
    std::fs::create_dir(root.join("sub")).unwrap();
    let (context, mut published) = context(None);
    // `start` returns once the platform watch is installed.
    let watch = WatchSource
        .start(
            "inbox",
            options(json!({ "path": root, "glob": "*.txt", "debounce_ms": 500 })),
            context,
        )
        .unwrap();
    std::fs::write(root.join("a.txt"), "one").unwrap();
    std::fs::write(root.join("b.log"), "ignored").unwrap();
    std::fs::write(root.join("sub").join("c.txt"), "two").unwrap();

    let event = published.next().await;
    let kinds = event.payload["kinds"].clone();
    assert!(
        kinds.as_array().unwrap().contains(&json!("create")),
        "{kinds}"
    );
    let paths = [root.join("a.txt"), root.join("sub").join("c.txt")];
    assert_eq!(
        event,
        source_event(
            Topic::new("source/inbox/changed").unwrap(),
            "inbox",
            json!({ "paths": paths, "kinds": kinds }),
            Trust::Owner,
        )
    );
    tokio::time::timeout(PATIENCE, watch.stop())
        .await
        .expect("a prompt stop")
        .unwrap();
}

#[tokio::test]
async fn a_missing_folder_is_refused_at_start() {
    let folder = tempfile::tempdir().unwrap();
    let (context, _published) = context(None);
    let started = WatchSource.start(
        "inbox",
        options(json!({ "path": folder.path().join("absent") })),
        context,
    );
    assert!(started.is_err());
}
