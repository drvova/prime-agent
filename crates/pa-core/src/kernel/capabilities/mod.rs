//! The kernel capability manifests: one TOML file per capability of the kernel API, beside this
//! module (schema in this directory's README), embedded here. Each records what the host keeps
//! about one capability: the host requests it issues as plumbing, and its standalone telemetry
//! events. The tests hold the telemetry catalog equal to them. What the model is told about the
//! kernel lives with the prompt: [`crate::prompts::layers::API_DOCS`].

use std::collections::BTreeMap;

use anyhow::Context;

#[cfg(test)]
mod tests;

/// Embed one capability's manifest under its name (the file stem).
macro_rules! manifest {
    ($name:literal) => {
        ($name, include_str!(concat!($name, ".toml")))
    };
}

/// Every capability's manifest text, by name. A new capability adds its file and one line here;
/// the consistency tests fail when the directory holds a file this list does not embed.
pub(crate) const MANIFESTS: &[(&str, &str)] = &[
    manifest!("ask"),
    manifest!("bash"),
    manifest!("better"),
    manifest!("cells"),
    manifest!("checkpoint"),
    manifest!("core"),
    manifest!("ctx"),
    manifest!("decision"),
    manifest!("frida"),
    manifest!("fs"),
    manifest!("hypothesis"),
    manifest!("mcp"),
    manifest!("model"),
    manifest!("notify"),
    manifest!("resource_reload"),
    manifest!("schedule"),
    manifest!("session"),
    manifest!("settings"),
    manifest!("todo"),
    manifest!("usage"),
    manifest!("web"),
    manifest!("worktree"),
];

/// One capability's manifest, as the README's schema table describes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct CapabilityManifest {
    #[serde(default)]
    pub(crate) internal_host_requests: Vec<String>,
    #[serde(default)]
    pub(crate) telemetry: Vec<TelemetryClaim>,
}

/// The count properties of one standalone session-end telemetry event a capability owns.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
