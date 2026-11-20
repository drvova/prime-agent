//! The generic capability counter: every kernel capability's standalone adoption counts, keyed by
//! the catalog property they report as. The capability rule files in pa-telemetry
//! (`catalog/capabilities/`) name each event and its properties; session end emits every rule
//! with a non-zero count, all its properties in rule order. A new capability counts through
//! [`super::SessionCounters::note_capability`] (or `note_capability_max`) under the property names
//! its rule file declares, and touches no counter struct here.

use std::collections::BTreeMap;

use pa_telemetry::EventRule;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct CapabilityCounts(BTreeMap<&'static str, u64>);

impl CapabilityCounts {
    /// Add `count` to `property`.
    pub(super) fn add(&mut self, property: &'static str, count: u64) {
        *self.0.entry(property).or_default() += count;
    }

    /// Raise `property` to `value` when `value` is larger (the slowest-call properties).
    pub(super) fn raise(&mut self, property: &'static str, value: u64) {
        let slot = self.0.entry(property).or_default();
        *slot = (*slot).max(value);
    }

    /// `rule`'s properties (its implied `session_id` aside) with their counts, in rule order, when
    /// any of them counted; `None` when the session never used the capability.
    pub(super) fn event_counts(&self, rule: &EventRule) -> Option<Vec<(&'static str, u64)>> {
        let counts: Vec<(&'static str, u64)> = rule
            .properties
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| *name != "session_id")
            .map(|name| (name, self.0.get(name).copied().unwrap_or(0)))
            .collect();
        counts.iter().any(|(_, count)| *count > 0).then_some(counts)
    }
}
