//! Static system-prompt layers assembled into the cache-stable prefix. Layers must never
//! contain session-specific values (the cache-safety guard test pins the boundary).
//!
//! The Scheme kernel's API reference lives beside the layers, one doc file per topic under
//! `capabilities/` ([`API_DOCS`]). The core layer carries each function's call form, spliced from
//! those files, and a paragraph per topic; a function's full entry (result shape, options,
//! defaults) is read on demand in the kernel with `(api-help ...)`, which serves the same files
//! ([`api_help`]). A doc file documents a function with a top-level bullet that opens with its
//! call form in backticks: ``- `(fs-read path [options])`: ...``.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use anyhow::{bail, ensure, Context};

/// The core layer's hand-written text, with one slot per API topic (see [`splice_core_layer`]).
pub(crate) const CORE_TEMPLATE: &str = include_str!("layers/core.md");

/// Embed one API topic's doc file under its name (the file stem).
macro_rules! topic {
    ($name:literal) => {
        ($name, include_str!(concat!("capabilities/", $name, ".md")))
    };
}

/// The kernel API reference: each topic's name and doc file, in the order `(api-help)` lists
/// them. A new topic adds its file and one line here; the tests fail when the directory holds a
/// file this list does not embed, and when `core.md` has no slot for a topic.
pub const API_DOCS: &[(&str, &str)] = &[
    topic!("scheme"),
    topic!("shell"),
    topic!("fs"),
    topic!("cells"),
    topic!("subagents"),
    topic!("messaging"),
    topic!("transcript"),
    topic!("harness"),
    topic!("goal"),
    topic!("schedule"),
    topic!("worktree"),
    topic!("web"),
    topic!("mcp"),
    topic!("frida"),
    topic!("factory"),
    topic!("user"),
    topic!("session"),
];

/// The core layer: [`CORE_TEMPLATE`] with its slots filled from [`API_DOCS`]. Built once, by the
/// first system-prompt assembly (a session start or `prime-agent prompt`); the embedded files
/// cannot change after that, and the tests pin that they splice.
pub static CORE_LAYER: LazyLock<String> = LazyLock::new(|| {
    splice_core_layer(CORE_TEMPLATE)
        .expect("the embedded API docs splice into core.md (their tests pin them)")
});
pub const USAGE_LAYER: &str = include_str!("layers/usage.md");
pub const OPINIONATED_LAYER: &str = include_str!("layers/opinionated.md");

/// The call forms a doc file documents, without their outer parentheses (`fs-read path
/// [options]`): the backticked span that opens each top-level bullet. The function's name is the
/// form's first word.
pub fn documented_calls(doc: &str) -> impl Iterator<Item = &str> {
    doc.lines()
        .filter_map(|line| line.strip_prefix("- `("))
        .filter_map(|rest| rest.split_once(")`"))
        .map(|(call, _)| call)
}

/// Fill `template`'s slots: `{{calls:<topic>}}` becomes that topic's call forms as a backticked
/// list (`` `(a x)`, `(b)` ``), and `{{names:<topic>}}` its function names alone (`` `a`, `b` ``).
/// Fails on an unknown slot, a topic that documents no function, and a topic placed twice or not
/// at all, so the prompt names every function the reference documents exactly once.
pub(crate) fn splice_core_layer(template: &str) -> anyhow::Result<String> {
    let mut topics: BTreeMap<&str, &str> = API_DOCS.iter().copied().collect();
    let mut spliced = String::with_capacity(template.len() * 2);
    let mut rest = template;
    while let Some((text, after)) = rest.split_once("{{") {
        spliced.push_str(text);
        let (slot, after) = after
            .split_once("}}")
            .with_context(|| format!("unclosed core.md slot after {text:?}"))?;
        let Some((kind @ ("calls" | "names"), topic)) = slot.split_once(':') else {
            bail!("unknown core.md slot {slot:?}");
        };
        let doc = topics
            .remove(topic)
            .with_context(|| format!("core.md slot {slot:?}: no such topic, or already placed"))?;
        let listed: Vec<String> = documented_calls(doc)
            .map(|call| {
                if kind == "calls" {
                    format!("`({call})`")
                } else {
                    format!("`{}`", call.split(' ').next().unwrap_or(call))
                }
            })
            .collect();
        ensure!(
            !listed.is_empty(),
            "core.md slot {slot:?}: the topic documents no function"
        );
        spliced.push_str(&listed.join(", "));
        rest = after;
    }
    spliced.push_str(rest);
    ensure!(
        topics.is_empty(),
        "API topics no core.md slot places: {:?}",
        topics.keys().collect::<Vec<_>>()
    );
    Ok(spliced)
}

/// What `(api-help topic)` prints: a topic's doc file, one function's entry (its bullet and the
/// bullets nested under it), or, for an empty or unknown `topic`, the list of topics.
pub(crate) fn api_help(topic: &str) -> String {
    if let Some((_, doc)) = API_DOCS.iter().find(|(name, _)| *name == topic) {
        return (*doc).to_string();
    }
    let entry = API_DOCS.iter().find_map(|(_, doc)| {
        let mut lines = doc.lines().skip_while(|line| {
            documented_calls(line)
                .next()
                .and_then(|call| call.split(' ').next())
                != Some(topic)
        });
        let bullet = lines.next()?;
        Some(
            std::iter::once(bullet)
                .chain(lines.take_while(|line| line.starts_with("  ")))
                .collect::<Vec<_>>()
                .join("\n"),
        )
    });
    if let Some(entry) = entry {
        return format!("{entry}\n");
    }
    let unknown = if topic.is_empty() {
        String::new()
    } else {
        format!("api-help: no topic or function is named {topic:?}.\n")
    };
    let topics: Vec<String> = API_DOCS
        .iter()
        .map(|(_, doc)| {
            let heading = doc.lines().next().unwrap_or_default();
            format!("- {}\n", heading.trim_start_matches("# "))
        })
        .collect();
    format!(
        "{unknown}Topics of the kernel API reference. (api-help \"<topic>\") prints one, \
         (api-help \"<function>\") one function's entry:\n{}",
        topics.concat()
    )
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

#[cfg(test)]
mod tests;
