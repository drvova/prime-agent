//! Deprecated kernel names: a function renamed or folded into another keeps
//! its old name as an alias for one release.
//!
//! [`ALIASES`] (`deprecated/aliases.txt`) is the one list of them: each old
//! name with the call that replaces it. An alias is an ordinary definition in
//! its topic's Scheme source, loaded through `contracts::load` with the old
//! parameter list, whose body first calls `%deprecated` with its old name and then
//! the new function. The first call of each alias in a kernel prints one line
//! to the cell's output naming the replacement; later calls print nothing.
//! The reference, the system prompt and `api-help` never name an alias, and
//! `tests/prompt_guards.rs` holds the list to the sources.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::anyhow;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::register_fn::RegisterFn;

/// The deprecated names and their replacements, one `old-name replacement`
/// per line; `#` starts a comment line.
const ALIASES: &str = include_str!("deprecated/aliases.txt");

/// `(%deprecated name)`: print the notice for `name` the first time.
const SCHEME: &str = r#"
(define (%deprecated name)
  (let ([notice (%deprecation-notice name)])
    (when (> (string-length notice) 0) (displayln notice))))
"#;

/// The replacement [`ALIASES`] names for `name`.
fn replacement(name: &str) -> Option<&'static str> {
    ALIASES
        .lines()
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.split_once(' ').filter(|(old, _)| *old == name))
        .map(|(_, new)| new)
}

/// Define `%deprecated` in a session's `engine`. Each engine tells each name
/// once.
///
/// # Errors
///
/// The Scheme definition failed to load.
pub(super) fn register(engine: &mut Engine) -> anyhow::Result<()> {
    let told: Arc<Mutex<BTreeSet<String>>> = Arc::default();
    engine.register_fn("%deprecation-notice", move |name: String| -> String {
        let first = told
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(name.clone());
        match (first, replacement(&name)) {
            (false, _) => String::new(),
            (true, Some(new)) => {
                format!("{name} is deprecated and goes away in the next release: use {new}")
            }
            // The guard in `tests/prompt_guards.rs` lists every caller.
            (true, None) => format!("{name} is deprecated and goes away in the next release"),
        }
    });
    super::contracts::load(engine, SCHEME)
        .map_err(|error| anyhow!("the deprecated-name notice failed to load: {error}"))
}

#[cfg(test)]
mod tests;
