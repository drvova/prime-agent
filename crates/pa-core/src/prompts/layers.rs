//! Static system-prompt layers assembled into the cache-stable prefix. Layers must never
//! contain session-specific values (the cache-safety guard test pins the boundary).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use anyhow::{bail, ensure, Context};

use crate::kernel::capabilities::{capabilities, MANIFESTS};

/// The core layer's hand-written text, with a slot for each capability doc file and one for the
/// kernel-implemented tool list (see [`splice_core_layer`]).
pub(crate) const CORE_TEMPLATE: &str = include_str!("layers/core.md");

/// The core layer: [`CORE_TEMPLATE`] with its slots filled from the kernel capability manifests
/// and doc files under `prime-agent-runtime/capabilities/`. Built once, by the first system-prompt
/// assembly (a session start or `prime-agent prompt`); the embedded files cannot change after
/// that, and the capability tests pin that they splice.
pub static CORE_LAYER: LazyLock<String> = LazyLock::new(|| {
    splice_core_layer(CORE_TEMPLATE)
        .expect("the embedded capability docs splice into core.md (their tests pin them)")
});
pub const USAGE_LAYER: &str = include_str!("layers/usage.md");
pub const OPINIONATED_LAYER: &str = include_str!("layers/opinionated.md");

/// Fill `template`'s slots: each `{{capability:<stem>}}` becomes the doc file `<stem>.md` without
/// its final newline, and `{{prompt_builtins:<capability>,...}}` becomes those capabilities'
/// `prompt_builtins`, in that order, as a backticked list (`` `a`, `b`, and `c` ``). Fails on an
/// unknown or repeated slot, a doc file no slot places, or a builtin list that does not name every
/// capability with prompt builtins exactly once.
pub(crate) fn splice_core_layer(template: &str) -> anyhow::Result<String> {
    let manifests = capabilities()?;
    let mut docs: BTreeMap<&str, &str> = MANIFESTS
        .iter()
        .flat_map(|capability| capability.docs.iter().copied())
        .collect();
    let mut spliced =
        String::with_capacity(template.len() + docs.values().map(|doc| doc.len()).sum::<usize>());
    let mut rest = template;
    while let Some((text, after)) = rest.split_once("{{") {
        spliced.push_str(text);
        let (slot, after) = after
            .split_once("}}")
            .with_context(|| format!("unclosed core.md slot after {text:?}"))?;
        match slot.split_once(':') {
            Some(("capability", stem)) => {
                let doc = docs.remove(stem).with_context(|| {
                    format!("core.md slot {slot:?}: no such doc file, or already placed")
                })?;
                spliced.push_str(doc.strip_suffix('\n').unwrap_or(doc));
            }
            Some(("prompt_builtins", names)) => {
                let names: Vec<&str> = names.split(',').collect();
                let mut listed = names.clone();
                listed.sort_unstable();
                let expected: Vec<&str> = manifests
                    .iter()
                    .filter(|(_, manifest)| !manifest.prompt_builtins.is_empty())
                    .map(|(name, _)| *name)
                    .collect();
                ensure!(
                    listed == expected,
                    "core.md slot {slot:?} must list each capability with prompt builtins \
                     exactly once: {expected:?}"
                );
                let builtins: Vec<String> = names
                    .iter()
                    .flat_map(|name| &manifests[name].prompt_builtins)
                    .map(|builtin| format!("`{builtin}`"))
                    .collect();
                let Some((last, init)) = builtins.split_last() else {
                    bail!("core.md slot {slot:?} lists no builtins");
                };
                spliced.push_str(&init.join(", "));
                if !init.is_empty() {
                    spliced.push_str(", and ");
                }
                spliced.push_str(last);
            }
            Some(_) | None => bail!("unknown core.md slot {slot:?}"),
        }
        rest = after;
    }
    spliced.push_str(rest);
    ensure!(
        docs.is_empty(),
        "capability doc files no core.md slot places: {:?}",
        docs.keys().collect::<Vec<_>>()
    );
    Ok(spliced)
}

/// Source file of one layer (breakdown provenance).
#[must_use]
pub fn layer_source(name: &str) -> Option<&'static str> {
    match name {
        "core" => Some("prompts/layers/core.md"),
        "usage" => Some("prompts/layers/usage.md"),
        "opinionated" => Some("prompts/layers/opinionated.md"),
        "model-prompts" => Some("prompts/layers/model_prompts.toml"),
        _ => None,
    }
}
