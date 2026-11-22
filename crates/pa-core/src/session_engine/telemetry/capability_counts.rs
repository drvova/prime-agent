//! The generic capability counter: every kernel capability's standalone adoption counts, keyed by
//! the catalog property they report as. The capability rule files in pa-telemetry
//! (`catalog/capabilities/`) name each event and its properties; session end emits every rule
//! with a non-zero count, its counted properties in rule order and then its derived ones. A new
//! capability counts through [`super::SessionCounters::note_capability`] under the property names
//! its rule file declares, and touches no counter struct here.

use std::collections::BTreeMap;

use pa_telemetry::EventRule;
use serde_json::Value;

/// The spilled bytes (`agent context paging`): kept as a number here, reported only as
/// `spill_bytes_bucket`.
pub(super) const SPILL_BYTES: &str = "spill_bytes";

/// A rule property computed from a raw session value rather than counted: its name, the raw
/// value it reads, and how that value reads as the property.
struct DerivedProperty {
    property: &'static str,
    source: &'static str,
    derive: fn(u64) -> &'static str,
}

/// Every derived capability property; each emits after its event's counted properties.
const DERIVED: &[DerivedProperty] = &[DerivedProperty {
    property: "spill_bytes_bucket",
    source: SPILL_BYTES,
    derive: spill_bytes_bucket,
}];

/// The spilled bytes as a size bucket (never the size itself).
fn spill_bytes_bucket(bytes: u64) -> &'static str {
    const MB: u64 = 1024 * 1024;
    match bytes {
        0 => "none",
        bytes if bytes < MB => "lt_1mb",
        bytes if bytes < 16 * MB => "lt_16mb",
        bytes if bytes < 64 * MB => "lt_64mb",
        bytes if bytes < 256 * MB => "lt_256mb",
        _ => "ge_256mb",
    }
}

/// One session's capability adoption counts, by catalog property (and the raw values the
/// derived properties read).
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

    /// What `property` has counted so far (zero when nothing counted into it).
    pub(super) fn count(&self, property: &str) -> u64 {
        self.0.get(property).copied().unwrap_or(0)
    }

    /// `rule`'s properties (its implied `session_id` aside) with their values: the counted ones in
    /// rule order, then the derived ones; `None` when the session never used the capability.
    pub(super) fn event_properties(&self, rule: &EventRule) -> Option<Vec<(&'static str, Value)>> {
        let mut active = false;
        let mut counted = Vec::new();
        let mut derived = Vec::new();
        for (name, _) in rule
            .properties
            .iter()
            .filter(|(name, _)| *name != "session_id")
        {
            if let Some(rule) = DERIVED.iter().find(|derived| derived.property == *name) {
                let raw = self.count(rule.source);
                active |= raw > 0;
                derived.push((*name, Value::from((rule.derive)(raw))));
            } else {
                let count = self.count(name);
                active |= count > 0;
                counted.push((*name, Value::from(count)));
            }
        }
        counted.extend(derived);
        active.then_some(counted)
    }
}
