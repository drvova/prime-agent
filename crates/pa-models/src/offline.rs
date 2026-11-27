//! Offline mode (`PI_OFFLINE`): no network at all, serve cache/bundled/compiled.

/// Whether the catalog subsystem must avoid every network request.
#[must_use]
pub fn is_catalog_offline() -> bool {
    offline_flag().is_some_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
}

fn offline_flag() -> Option<String> {
    pa_types::session_scope::var("PI_OFFLINE")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pa_types::session_scope::{self, SessionScope};
    use std::collections::{BTreeMap, BTreeSet};

    /// `is_catalog_offline` inside a session scope where `PI_OFFLINE` is `raw`
    /// (`None` hides it), so the process environment stays untouched.
    fn offline_with(raw: Option<&str>) -> bool {
        let env: BTreeMap<String, String> = raw
            .map(|raw| ("PI_OFFLINE".to_string(), raw.to_string()))
            .into_iter()
            .collect();
        let hidden: BTreeSet<String> = raw
            .is_none()
            .then(|| "PI_OFFLINE".to_string())
            .into_iter()
            .collect();
        let _scope = session_scope::enter(Some(SessionScope::new(
            "offline-test".to_string(),
            std::path::PathBuf::new(),
            env,
            hidden,
        )));
        is_catalog_offline()
    }

    #[test]
    fn parses_offline_values() {
        let answers =
            ["1", "true", "YES", " Yes ", "0", "no", "maybe"].map(|raw| offline_with(Some(raw)));
        assert_eq!(answers, [true, true, true, true, false, false, false]);
        assert!(!offline_with(None));
    }
}
