//! The API reference against a live kernel: what it documents is defined there, and the kernel
//! serves it. `tests/prompt_guards.rs` holds the reference equal to the kernel's sources.

use std::path::Path;
use std::sync::{Arc, Mutex};

use super::{api_help, documented_calls, splice_core_layer, API_DOCS, CORE_TEMPLATE};
use crate::kernel::scheme::{SchemeKernelOptions, SchemeKernelProvisioner};
use crate::kernel::shared::ReferenceLookupFact;
use crate::tools::kernel_cell::{ExecuteStatus, KernelExecuteOptions, KernelProvisioner};

const NO_OPTIONS: KernelExecuteOptions<'static> = KernelExecuteOptions {
    signal: None,
    timeout_ms: None,
    on_stream: None,
    on_late_sent_agent_message: None,
    tool_call_id: None,
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

/// `(api-search query)` ranks the reference: a function's name puts its entry first, a
/// description of what a function does finds it, a query no passage shares a word with says so
/// and lists the topics, and a limit below 1 is refused. The session's observer hears each search
/// that ran and whether it found a passage.
#[tokio::test(flavor = "multi_thread")]
async fn api_search_ranks_the_reference_by_the_querys_words() {
    let dir = tempfile::tempdir().unwrap();
    let heard = Arc::new(Mutex::new(Vec::new()));
    let observer = Arc::clone(&heard);
    let kernel = SchemeKernelProvisioner::with_options(SchemeKernelOptions {
        cwd: dir.path().to_path_buf(),
        on_reference_lookup: Some(Arc::new(move |fact| observer.lock().unwrap().push(fact))),
        ..SchemeKernelOptions::default()
    })
    .ensure(None, None)
    .await
    .unwrap();
    let mut printed = Vec::new();
    for cell in [
        "(api-search \"fs-read\" #:limit 1)",
        "(api-search \"kill a background job\")",
        "(api-search \"xyzzy\")",
        "(api-search \"fs-read\" #:limit 0)",
    ] {
        let result = kernel.execute(cell, NO_OPTIONS).await.unwrap();
        printed.push((result.status, result.stdout));
    }
    let described = printed[1].1.contains(api_help("bash-kill").as_str());
    printed[1].1 = String::new();
    printed[3].1 = String::new();
    let heard = heard.lock().unwrap().clone();
    assert_eq!(
        (printed, described, heard),
        (
            vec![
                (
                    ExecuteStatus::Ok,
                    format!(
                        "api-search: 1 passage of the reference for \"fs-read\", best first. \
                         (api-help \"<function>\") prints an entry, (api-help \"<topic>\") a \
                         topic.\n\n[fs]\n{}",
                        api_help("fs-read")
                    )
                ),
                (ExecuteStatus::Ok, String::new()),
                (
                    ExecuteStatus::Ok,
                    format!(
                        "api-search: nothing in the reference matches \"xyzzy\".\n{}",
                        api_help("")
                    )
                ),
                (ExecuteStatus::Error, String::new()),
            ],
            true,
            vec![
                ReferenceLookupFact::SearchFound,
                ReferenceLookupFact::SearchFound,
                ReferenceLookupFact::SearchMissed,
            ]
        )
    );
}
