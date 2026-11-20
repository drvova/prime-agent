use std::sync::{Arc, Mutex};

use serde_json::json;

use super::{register_test_summary_handler, TestRunFact};
use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
use crate::kernel::spill::{content_id, SpillLookup, SpillStore};

/// A summarized run's whole output and each failure's section get handles
/// that read back their text, and the run is counted.
#[tokio::test]
async fn a_summarized_run_gets_a_handle_per_output_and_failure() {
    let session = tempfile::tempdir().expect("session dir");
    let store = SpillStore::for_session_dir(session.path());
    let facts = Arc::new(Mutex::new(Vec::new()));
    let mut handlers = HostRequestHandlers::new();
    register_test_summary_handler(
        &mut handlers,
        Some(&store),
        Some({
            let facts = Arc::clone(&facts);
            Arc::new(move |fact| facts.lock().unwrap().push(fact))
        }),
    );
    let output = "running 2 tests\ntest a ... FAILED\ntest b ... ok\n";
    let failure = "---- a stdout ----\npanicked at src/lib.rs:3:5:\nboom";
    let handler = handlers.get("tests.summary").expect("registered");
    let reply = handler(HostRequestPayload {
        data: json!({"runner": "cargo", "failed": 1, "output": output, "failures": [failure]}),
    })
    .await
    .expect("reply");
    assert_eq!(
        (reply, facts.lock().unwrap().clone()),
        (
            json!({
                "output": format!("out:{}", content_id(output)),
                "failures": [format!("out:{}", content_id(failure))],
            }),
            vec![TestRunFact {
                failed: 1,
                failures: 1,
                stored: true,
            }],
        )
    );
    match store.lookup(&content_id(failure)).await {
        SpillLookup::Stored(stored) => assert_eq!(stored.text, failure),
        other => panic!("the failure's handle reads its section, got {other:?}"),
    }
}
