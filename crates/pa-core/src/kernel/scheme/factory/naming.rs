//! The sibling names a run gives its spawned children. The supervisor
//! requires unique names among one parent session's children and caps them
//! at 64 characters, and a parent session outlives its runs, so every label
//! is scoped to its run and disambiguated per instance and attempt.

use std::fmt::Write as _;

use sha2::{Digest, Sha256};

use super::spec::SUBAGENT_NAME_MAX_LENGTH;

/// The first 16 hex characters of a text's SHA-256.
fn digest16(text: &str) -> String {
    let mut hex = String::with_capacity(16);
    for byte in Sha256::digest(text.as_bytes()).iter().take(8) {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

/// The generated label of one spawned instance: `sw-<state>-<run6>` plus
/// `-i<n>` for an instance index and `-a<n>` for a retry. A state id longer
/// than 20 characters is cut and disambiguated with a digest of the full
/// id, so two states sharing a prefix never collide.
pub(super) fn child_name(
    run_id: &str,
    state_id: &str,
    instance_index: i64,
    attempt: u64,
) -> String {
    let token = if state_id.chars().count() <= 20 {
        state_id.to_string()
    } else {
        let head: String = state_id.chars().take(20).collect();
        format!("{head}-{}", digest16(state_id))
    };
    let run: String = run_id.chars().take(6).collect();
    let mut name = format!("sw-{token}-{run}");
    if instance_index >= 0 {
        let _ = write!(name, "-i{instance_index}");
    }
    if attempt > 1 {
        let _ = write!(name, "-a{attempt}");
    }
    name
}

/// The label of one spawned instance: the state's configured inline
/// subagent name prefixed with the run id (`<run6>-name`), else the
/// generated label. Later instances and retries carry the generated
/// label's `-i<n>`/`-a<n>` suffixes; a label that would pass the host's
/// cap shrinks its base to a digest-suffixed prefix of the full name.
pub(super) fn spawn_label(
    configured: Option<&str>,
    run_id: &str,
    state_id: &str,
    instance_index: i64,
    attempt: u64,
) -> String {
    let Some(configured) = configured else {
        return child_name(run_id, state_id, instance_index, attempt);
    };
    let mut suffix = String::new();
    if instance_index > 0 {
        let _ = write!(suffix, "-i{instance_index}");
    }
    if attempt > 1 {
        let _ = write!(suffix, "-a{attempt}");
    }
    let run: String = run_id.chars().take(6).collect();
    let mut base = format!("{run}-{configured}");
    if base.chars().count() + suffix.len() > SUBAGENT_NAME_MAX_LENGTH {
        let digest = digest16(&base);
        let room = SUBAGENT_NAME_MAX_LENGTH.saturating_sub(suffix.len() + digest.len() + 1);
        let head: String = base.chars().take(room).collect();
        base = format!("{head}-{digest}");
    }
    base + &suffix
}

/// Whether `candidate` is a spawn label a state configured as `base` can
/// produce: `base-i<n>` (n >= 1) and `base-a<n>` (n >= 2), in any chain.
pub(super) fn suffixed_spawn_form(base: &str, candidate: &str) -> bool {
    let Some(remainder) = candidate
        .strip_prefix(base)
        .and_then(|rest| rest.strip_prefix('-'))
    else {
        return false;
    };
    remainder.split('-').all(|part| {
        let mut letters = part.chars();
        let (Some(kind), digits) = (letters.next(), letters.as_str()) else {
            return false;
        };
        if digits.is_empty() || !digits.chars().all(|digit| digit.is_ascii_digit()) {
            return false;
        }
        // The never-generated `-i0` and `-a1` do not count.
        let number = digits.trim_start_matches('0');
        match kind {
            'i' => !number.is_empty(),
            'a' => !number.is_empty() && number != "1",
            _ => false,
        }
    })
}
