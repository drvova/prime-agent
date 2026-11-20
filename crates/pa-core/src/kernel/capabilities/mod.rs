//! The kernel capability manifests: one TOML file per capability under
//! `prime-agent-runtime/capabilities/` (schema in that directory's README), embedded here. Each
//! records what the host keeps about one capability: its readiness-probe statements, closure
//! seed, bootstrap binding, prompt names, host requests, and standalone telemetry event. Beside each
//! manifest, its doc files hold the capability's section of the core prompt.
//!
//! The runtime-ready probe and the closure freeze's seed derive from the manifests, and the core
//! prompt layer splices in the doc files and the manifests' prompt builtins
//! ([`crate::prompts::layers::CORE_LAYER`]); the tests hold the hand-written surfaces (the
//! bootstrap bindings, the core prompt's remaining text, and the telemetry catalog) equal to them.

use std::collections::BTreeMap;

use anyhow::Context;

#[cfg(test)]
mod tests;

/// Embed one capability: its manifest under its name (the file stem), and its doc files, each under
/// its file stem (the core prompt slot it fills).
macro_rules! manifest {
    ($name:literal $(, docs = [$($doc:literal),+ $(,)?])?) => {
        EmbeddedCapability {
            name: $name,
            manifest: include_str!(concat!(
                "../../../../../prime-agent-runtime/capabilities/",
                $name,
                ".toml"
            )),
            docs: &[$($((
                $doc,
                include_str!(concat!(
                    "../../../../../prime-agent-runtime/capabilities/",
                    $doc,
                    ".md"
                )),
            )),+)?],
        }
    };
}

/// One capability as embedded: its manifest text and its doc files.
pub(crate) struct EmbeddedCapability {
    pub(crate) name: &'static str,
    manifest: &'static str,
    /// Each doc file's stem (`<name>` or `<name>.<part>`, the `{{capability:<stem>}}` slot of
    /// `prompts/layers/core.md` it fills) and text.
    pub(crate) docs: &'static [(&'static str, &'static str)],
}

/// Every capability, by name. A new capability adds its files and one line here; the consistency
/// tests fail when the directory holds a file this list does not embed.
pub(crate) const MANIFESTS: &[EmbeddedCapability] = &[
    manifest!("ask", docs = ["ask"]),
    manifest!("bash", docs = ["bash"]),
    manifest!("better", docs = ["better"]),
    manifest!("cells", docs = ["cells"]),
    manifest!("checkpoint", docs = ["checkpoint"]),
    manifest!(
        "core",
        docs = ["core", "core.end_turn", "core.progress_note"]
    ),
    manifest!("ctx", docs = ["ctx"]),
    manifest!("fs", docs = ["fs"]),
    manifest!("harness", docs = ["harness"]),
    manifest!("mcp", docs = ["mcp"]),
    manifest!("model", docs = ["model", "model.set_effort"]),
    manifest!("notify", docs = ["notify"]),
    manifest!("repl"),
    manifest!("resource_reload", docs = ["resource_reload"]),
    manifest!("schedule", docs = ["schedule"]),
    manifest!("session", docs = ["session"]),
    manifest!("settings", docs = ["settings"]),
    manifest!("todo", docs = ["todo"]),
    manifest!("usage", docs = ["usage"]),
    manifest!("web", docs = ["web"]),
    manifest!("worktree", docs = ["worktree"]),
];

/// One capability's manifest, as the README's schema table describes it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
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
pub(crate) struct TelemetryClaim {
    pub(crate) event: String,
    pub(crate) properties: Vec<String>,
}

/// Parse every embedded manifest, keyed by capability name.
pub(crate) fn capabilities() -> anyhow::Result<BTreeMap<&'static str, CapabilityManifest>> {
    MANIFESTS
        .iter()
        .map(|embedded| {
            toml::from_str(embedded.manifest)
                .map(|manifest| (embedded.name, manifest))
                .with_context(|| format!("capability manifest {}.toml", embedded.name))
        })
        .collect()
}
