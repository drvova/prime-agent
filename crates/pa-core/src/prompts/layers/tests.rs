//! The API reference against a live kernel: what it documents is defined there, and the kernel
//! serves it. `tests/prompt_guards.rs` holds the reference equal to the kernel's sources.

use std::path::Path;

use super::{api_help, documented_calls, splice_core_layer, API_DOCS, CORE_TEMPLATE};
use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
use crate::tools::ipython::{IpythonKernelProvisioner, KernelExecuteOptions};

const NO_OPTIONS: KernelExecuteOptions<'static> = KernelExecuteOptions {
    signal: None,
    timeout_ms: None,
    on_stream: None,
    on_late_sent_agent_message: None,
};

/// Every doc file in `prompts/capabilities/` is embedded, and every topic has its slot in
/// `core.md`.
#[test]
fn every_doc_file_is_embedded_and_placed_in_the_core_layer() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/prompts/capabilities");
    let mut on_disk: Vec<String> = std::fs::read_dir(&dir)
        .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        .map(|entry| entry.expect("directory entry").path())
        .map(|path| {
            path.file_stem()
                .expect("file stem")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    on_disk.sort();
    let mut embedded: Vec<&str> = API_DOCS.iter().map(|(name, _)| *name).collect();
    embedded.sort_unstable();
    assert_eq!(
        (
            on_disk,
            splice_core_layer(CORE_TEMPLATE).map_err(|error| format!("{error:#}"))
        ),
        (
            embedded.into_iter().map(str::to_string).collect(),
            Ok(super::CORE_LAYER.clone())
        )
    );
}

/// The reference names nothing the kernel does not define: each documented name is bound to a
/// function in a session's engine.
#[tokio::test(flavor = "multi_thread")]
async fn every_documented_function_is_defined_in_a_session_kernel() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        cwd: dir.path().to_path_buf(),
        ..SchemeKernelOptions::default()
    })
    .ensure(None, None)
    .await
    .unwrap();
    let mut undefined = Vec::new();
    for call in API_DOCS.iter().flat_map(|(_, doc)| documented_calls(doc)) {
        let name = call.split(' ').next().unwrap_or(call);
        let result = kernel
            .execute(&format!("(function? {name})"), NO_OPTIONS)
            .await
            .unwrap();
        if result.result.as_deref() != Some("#true") {
            undefined.push(name);
        }
    }
    assert_eq!(undefined, Vec::<&str>::new());
}

/// `(api-help)` prints the topics, a topic's name its doc file, a function's name (a string or a
/// symbol) that function's entry with its nested bullets, and an unknown name says so.
#[tokio::test(flavor = "multi_thread")]
async fn api_help_prints_the_topics_a_topic_and_one_function() {
    let dir = tempfile::tempdir().unwrap();
    let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        cwd: dir.path().to_path_buf(),
        ..SchemeKernelOptions::default()
    })
    .ensure(None, None)
    .await
    .unwrap();
    let mut printed = Vec::new();
    for cell in [
        "(api-help)",
        "(api-help \"cells\")",
        "(api-help 'cell-id)",
        "(api-help \"bash\")",
        "(api-help \"nope\")",
    ] {
        printed.push(kernel.execute(cell, NO_OPTIONS).await.unwrap().stdout);
    }
    let doc = |topic: &str| {
        API_DOCS
            .iter()
            .find(|(name, _)| *name == topic)
            .map(|(_, doc)| *doc)
            .unwrap()
    };
    // `bash`'s entry runs from its bullet through the bullets nested under it.
    let bash = doc("shell");
    let bash_entry =
        &bash[bash.find("- `(bash command").unwrap()..bash.find("- `(bash-wait").unwrap()];
    assert_eq!(
        printed,
        vec![
            api_help(""),
            doc("cells").to_string(),
            "- `(cell-id)`: the id of the running cell\n".to_string(),
            bash_entry.to_string(),
            format!(
                "api-help: no topic or function is named \"nope\".\n{}",
                api_help("")
            ),
        ]
    );
}
