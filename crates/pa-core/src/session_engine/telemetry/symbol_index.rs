//! `agent symbol index` (v2): `fs.symbol` queries the kernel answered from
//! the project's symbol index (counts only: never a name or a path). The `fs` capability's rule
//! file names the event; session end emits it through the generic capability counter.

use super::SessionCounters;
use crate::symbol_index::SymbolFact;

impl SessionCounters {
    /// One answered `fs.symbol` query: the query, whether it was answered before the first full
    /// pass, and the definitions and references it returned.
    pub(crate) fn note_symbol_query(&self, fact: SymbolFact) {
        for (property, count) in [
            ("symbol_query_count", 1),
            ("symbol_query_partial_count", u64::from(fact.partial)),
            ("symbol_definition_count", fact.definitions as u64),
            ("symbol_reference_count", fact.references as u64),
        ] {
            self.note_capability(property, count);
        }
    }
}
