//! `agent symbol index` (v2): `fs.symbol` queries the kernel answered from
//! the project's symbol index (counts only: never a name or a path).

use super::{SessionCounters, SessionTelemetry, Value};
use crate::symbol_index::SymbolFact;

/// Queries, those answered before the first full pass, and the definitions
/// and references they returned.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct SymbolIndexCounts {
    queries: u64,
    partial: u64,
    definitions: u64,
    references: u64,
}

impl SessionCounters {
    /// One answered `fs.symbol` query.
    pub(crate) fn note_symbol_query(&self, fact: SymbolFact) {
        self.with(|values| {
            let counts = &mut values.symbol_index;
            counts.queries += 1;
            counts.partial += u64::from(fact.partial);
            counts.definitions += fact.definitions as u64;
            counts.references += fact.references as u64;
        });
    }
}

impl SessionTelemetry {
    /// Track `agent symbol index`, unless the session asked no `fs.symbol` query.
    pub(super) fn track_symbol_index(&self) {
        let mut counts = SymbolIndexCounts::default();
        self.counters.with(|values| counts = values.symbol_index);
        if counts == SymbolIndexCounts::default() {
            return;
        }
        let mut properties = self.session_properties();
        for (key, value) in [
            ("symbol_query_count", counts.queries),
            ("symbol_query_partial_count", counts.partial),
            ("symbol_definition_count", counts.definitions),
            ("symbol_reference_count", counts.references),
        ] {
            properties.set(key, Value::from(value));
        }
        self.client.track("agent symbol index", properties);
    }
}
