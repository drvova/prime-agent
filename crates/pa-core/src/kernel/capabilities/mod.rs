//! The kernel capability manifests: one TOML file per capability under
//! `prime-agent-runtime/capabilities/` (schema in that directory's README), embedded here. Each
//! records what the host keeps about one capability: its readiness-probe statements, closure
//! seed, bootstrap binding, prompt names, host requests, and standalone telemetry event.
//!
//! The runtime-ready probe and the closure freeze's seed derive from the manifests; the tests hold
//! the hand-written surfaces (the bootstrap bindings, the core prompt, and the telemetry catalog)
//! equal to them.

use std::collections::BTreeMap;

use anyhow::Context;

#[cfg(test)]
mod tests;

/// Embed one manifest under its capability name (the file stem).
macro_rules! manifest {
    ($name:literal) => {
        (
            $name,
            include_str!(concat!(
                "../../../../../prime-agent-runtime/capabilities/",
                $name,
                ".toml"
            )),
        )
    };
}

/// Every manifest, by capability name. A new capability adds its file and one line here; the
/// consistency test fails when the directory holds a file this list does not embed.
const MANIFESTS: &[(&str, &str)] = &[
    manifest!("ask"),
    manifest!("bash"),
    manifest!("cells"),
    manifest!("checkpoint"),
    manifest!("core"),
    manifest!("fs"),
    manifest!("harness"),
    manifest!("mcp"),
    manifest!("model"),
    manifest!("notify"),
    manifest!("repl"),
    manifest!("resource_reload"),
    manifest!("schedule"),
    manifest!("session"),
    manifest!("settings"),
    manifest!("todo"),
    manifest!("usage"),
    manifest!("web"),
];

/// One capability's manifest, as the README's schema table describes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "the shipped build derives only the probe; the tests read every other key"
    )
)]
pub(crate) struct CapabilityManifest {
    pub(crate) module: String,
    pub(crate) closure_seed: bool,
    #[serde(default)]
    pub(crate) closure_transitive: Vec<String>,
    #[serde(default)]
    pub(crate) bound_module: Option<String>,
    #[serde(default)]
    pub(crate) prompt_builtins: Vec<String>,
    #[serde(default)]
    pub(crate) documented: Vec<String>,
    #[serde(default)]
    pub(crate) internal_host_requests: Vec<String>,
    pub(crate) probe: Vec<String>,
    #[serde(default)]
    pub(crate) host_requests: BTreeMap<String, String>,
    #[serde(default)]
    pub(crate) telemetry: Vec<TelemetryClaim>,
}

/// The count properties of one standalone session-end telemetry event a capability owns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "read only by the telemetry catalog test")
)]
pub(crate) struct TelemetryClaim {
    pub(crate) event: String,
    pub(crate) properties: Vec<String>,
}

/// Parse every embedded manifest, keyed by capability name.
pub(crate) fn capabilities() -> anyhow::Result<BTreeMap<&'static str, CapabilityManifest>> {
    MANIFESTS
        .iter()
        .map(|(name, text)| {
            toml::from_str(text)
                .map(|manifest| (*name, manifest))
                .with_context(|| format!("capability manifest {name}.toml"))
        })
        .collect()
}
