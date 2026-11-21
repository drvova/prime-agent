//! The artefacts derived from the capability manifests, and consistency between them and the
//! hand-written surfaces they describe. What the model is told is guarded in
//! `tests/prompt_guards.rs` and `prompts/layers.rs`.

use std::collections::BTreeMap;
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
    // Error enrichment reads the cell result host-side (no kernel capability).
    "agent error enrichment",
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

/// Every manifest in this directory is embedded.
#[test]
fn every_manifest_is_embedded() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/kernel/capabilities");
    let on_disk = sorted(
        std::fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "toml")
            })
            .map(|path| {
                path.file_stem()
                    .expect("file stem")
                    .to_string_lossy()
                    .into_owned()
            }),
    );
    assert_eq!(on_disk, sorted(MANIFESTS.iter().map(|(name, _)| *name)));
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

/// The bootstrap binds exactly the manifests' bound modules (`import rlm.<module> as <name>`).
/// `ask` and `notify` are bound as functions under private module aliases, so they carry no
/// `bound_module`.
#[test]
fn the_bootstrap_binds_the_manifests_modules() {
    let code = crate::kernel::bootstrap::build_rlm_bootstrap_code(&[], false);
    let bound = sorted(
        code.lines()
            .filter_map(|line| line.trim().strip_prefix("import rlm."))
            .filter_map(|line| line.split_once(" as ").map(|(_, name)| name))
            .filter(|name| !name.starts_with('_')),
    );
    assert_eq!(
        bound,
        sorted(
            manifests()
                .into_iter()
                .filter_map(|manifest| manifest.bound_module)
        )
    );
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
