//! The artefacts derived from the capability manifests and doc files, and consistency between them
//! and the hand-written surfaces they describe. The host-request half lives in
//! `tests/prompt_guards.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{capabilities, CapabilityManifest, MANIFESTS};

/// Session-scoped catalog events that are not a kernel capability's standalone adoption event:
/// the session lifecycle, goal verification (`goal` is an executable skill), and agent messaging
/// and waiting (the `agent-message` skill).
const NON_CAPABILITY_SESSION_EVENTS: &[&str] = &[
    "agent started",
    "agent run completed",
    "agent session ended",
    "session archived",
    "agent goal verification",
    "agent messages",
    "agent message waits",
    // The provider layer's Responses transport counts (no kernel capability).
    "agent responses transport",
    // Provider request admission is engine plumbing, not a kernel capability.
    "agent provider admission",
    // The scraped engines' limits are web search plumbing beside the capability's own counts.
    "agent web search engines",
    // Session-boot admission paces worker spawns and kernel starts (no kernel capability).
    "agent session boot admission",
    // The turn-start provider connection re-warm is provider plumbing, not a kernel capability.
    "agent connection rewarm",
    // The opt-in quota fallback is request-auth plumbing, not a kernel capability.
    "agent quota fallback",
    // The worker's background endpoint discovery (no kernel capability).
    "agent endpoint discovery",
    // The session's recorded calls by billing mode (no kernel capability).
    "agent billing modes",
];

fn manifests() -> Vec<CapabilityManifest> {
    capabilities()
        .expect("every embedded manifest parses")
        .into_values()
        .collect()
}

fn sorted(items: impl IntoIterator<Item = impl Into<String>>) -> Vec<String> {
    let mut items: Vec<String> = items.into_iter().map(Into::into).collect();
    items.sort();
    items
}

/// The capability directory's files, by extension, as file stems.
fn files_on_disk(extension: &str) -> Vec<String> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prime-agent-runtime/capabilities");
    sorted(
        std::fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == extension))
            .map(|path| {
                path.file_stem()
                    .expect("file stem")
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|stem| stem != "README"),
    )
}

/// Every manifest and doc file in the directory is embedded, and each doc file under the manifest
/// it is named for (`<name>.md` or `<name>.<part>.md`).
#[test]
fn every_manifest_and_doc_file_is_embedded() {
    let misfiled: Vec<&str> = MANIFESTS
        .iter()
        .flat_map(|capability| {
            capability
                .docs
                .iter()
                .map(|(stem, _)| *stem)
                .filter(|stem| stem.split('.').next() != Some(capability.name))
        })
        .collect();
    assert_eq!(
        (files_on_disk("toml"), files_on_disk("md"), misfiled),
        (
            sorted(MANIFESTS.iter().map(|capability| capability.name)),
            sorted(
                MANIFESTS
                    .iter()
                    .flat_map(|capability| capability.docs.iter().map(|(stem, _)| *stem))
            ),
            Vec::new()
        )
    );
}

/// Every slot of `core.md` resolves and every doc file fills one.
#[test]
fn the_core_layer_splices() {
    if let Err(error) =
        crate::prompts::layers::splice_core_layer(crate::prompts::layers::CORE_TEMPLATE)
    {
        panic!("{error:#}");
    }
}

/// The derived runtime-ready probe passes against this checkout's runtime source, and so does the
/// core manifest's probe followed by each other manifest's alone: a manifest's statements need only
/// core's imports and their own, so the derived order never puts a use before its import. Skipped
/// without a Python that takes `-P` (3.11+), as the probe's own safe-path test is.
#[test]
fn the_derived_probe_passes_against_the_runtime_source() {
    let Some(python) = ["python3.11", "python3"].into_iter().find(|python| {
        std::process::Command::new(python)
            .args(["-P", "-c", "pass"])
            .status()
            .is_ok_and(|status| status.success())
    }) else {
        eprintln!("no Python 3.11+ found; skipping the derived-probe run");
        return;
    };
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prime-agent-runtime/src");
    let passes = |program: &str| {
        std::process::Command::new(python)
            .args(["-P", "-s", "-c", program])
            .env("PYTHONPATH", &source)
            .status()
            .is_ok_and(|status| status.success())
    };
    let manifests = capabilities().expect("every embedded manifest parses");
    let core = &manifests["core"].probe;
    let failing: Vec<&str> = manifests
        .iter()
        .filter(|(name, manifest)| {
            **name != "core"
                && !passes(
                    &[core.as_slice(), manifest.probe.as_slice()]
                        .concat()
                        .join("; "),
                )
        })
        .map(|(name, _)| *name)
        .collect();
    assert_eq!(
        (
            passes(&crate::kernel::bootstrap::venv::RUNTIME_READY_CHECK),
            failing
        ),
        (true, Vec::new())
    );
}

/// The bootstrap binds exactly the manifests' bound modules (`import rlm.<module> as <name>`), and
/// each is one its manifest lists among the core prompt's kernel-implemented tools (`<name>.*`), so
/// a new kernel API ships bound and documented together. `ask` and `notify` are bound as functions
/// under private module aliases, so they carry no `bound_module`.
#[test]
fn the_bootstrap_binds_the_manifests_modules() {
    let code = crate::kernel::bootstrap::build_rlm_bootstrap_code(&[], false);
    let bound = sorted(
        code.lines()
            .filter_map(|line| line.trim().strip_prefix("import rlm."))
            .filter_map(|line| line.split_once(" as ").map(|(_, name)| name))
            .filter(|name| !name.starts_with('_')),
    );
    let manifests = manifests();
    let unlisted: Vec<&str> = manifests
        .iter()
        .filter_map(|manifest| {
            let name = manifest.bound_module.as_deref()?;
            (!manifest.prompt_builtins.contains(&format!("{name}.*"))).then_some(name)
        })
        .collect();
    assert_eq!(
        (bound, unlisted),
        (
            sorted(
                manifests
                    .iter()
                    .filter_map(|manifest| manifest.bound_module.clone())
            ),
            Vec::new()
        )
    );
}

/// The bullet tokens of `text` (`- `name(...)`...`) under one of the manifests' builtin
/// namespaces (`rlm`, `schedule`, `ask`, ...).
fn namespaced_bullets<'a>(text: &'a str, manifests: &[CapabilityManifest]) -> BTreeSet<&'a str> {
    let namespaces: BTreeSet<&str> = manifests
        .iter()
        .flat_map(|manifest| &manifest.prompt_builtins)
        .map(|name| name.trim_end_matches(".*"))
        .collect();
    text.lines()
        .filter_map(|line| line.trim_start().strip_prefix("- `"))
        .filter_map(|rest| rest.split_once('`').map(|(token, _)| token))
        .filter_map(|token| token.split('(').next())
        .map(str::trim)
        .filter(|token| {
            token.starts_with(|ch: char| ch.is_ascii_lowercase())
                && token.chars().all(|ch| {
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '.'
                })
        })
        .filter(|token| namespaces.contains(token.split('.').next().unwrap_or_default()))
        .collect()
}

/// The assembled core layer's bullet tokens under the builtin namespaces are exactly the
/// manifests' documented names, each claimed once; each manifest's host requests surface as names
/// that same manifest documents.
#[test]
fn the_core_prompt_names_the_manifests_tools() {
    let manifests = manifests();
    let undocumented_requests: Vec<&str> = manifests
        .iter()
        .flat_map(|manifest| {
            manifest
                .host_requests
                .values()
                .filter(|token| !manifest.documented.contains(token))
                .map(String::as_str)
        })
        .collect();

    assert_eq!(
        (
            sorted(namespaced_bullets(
                crate::prompts::layers::CORE_LAYER.as_str(),
                &manifests
            )),
            undocumented_requests
        ),
        (
            sorted(
                manifests
                    .iter()
                    .flat_map(|manifest| manifest.documented.iter().map(String::as_str))
            ),
            Vec::new()
        )
    );
}

/// Each capability's documented names are bulleted in its own doc files, so a capability with
/// documented names has a doc file and its section documents nothing it does not own.
#[test]
fn each_capability_documents_its_own_names() {
    let parsed = capabilities().expect("every embedded manifest parses");
    let manifests: Vec<CapabilityManifest> = parsed.values().cloned().collect();
    let mismatched: Vec<&str> = MANIFESTS
        .iter()
        .filter(|capability| {
            let docs: String = capability.docs.iter().map(|(_, text)| *text).collect();
            namespaced_bullets(&docs, &manifests)
                != parsed[capability.name]
                    .documented
                    .iter()
                    .map(String::as_str)
                    .collect()
        })
        .map(|capability| capability.name)
        .collect();
    assert_eq!(mismatched, Vec::<&str>::new());
}

/// Every standalone session-scoped adoption event in the telemetry catalog is claimed by the
/// manifests, and its properties are exactly `session_id` plus the claimed counts.
#[test]
fn the_telemetry_catalog_holds_the_manifests_events() {
    let catalogued: BTreeMap<&str, Vec<String>> = pa_telemetry::catalog()
        .into_iter()
        .filter(|rule| {
            rule.properties
                .iter()
                .any(|(name, _)| *name == "session_id")
                && !NON_CAPABILITY_SESSION_EVENTS.contains(&rule.name)
        })
        .map(|rule| {
            (
                rule.name,
                sorted(rule.properties.iter().map(|(name, _)| *name)),
            )
        })
        .collect();

    let mut claimed: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let manifests = manifests();
    for claim in manifests.iter().flat_map(|manifest| &manifest.telemetry) {
        claimed
            .entry(claim.event.as_str())
            .or_insert_with(|| vec!["session_id".to_string()])
            .extend(claim.properties.iter().cloned());
    }
    for properties in claimed.values_mut() {
        properties.sort();
    }
    assert_eq!(catalogued, claimed);
}
