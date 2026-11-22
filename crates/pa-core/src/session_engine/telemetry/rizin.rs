//! `agent repl rizin` (v2): kernel `rz-*` calls by kind, commands that were interrupted, rizins
//! that died, and opens refused because rizin is not installed (counts only: never a target, a
//! path, a command, or output). The `rizin` capability's rule file names the event; session end
//! emits it through the generic capability counter.

use super::SessionCounters;
use crate::session_engine::rizin_usage::RizinFact;

impl SessionCounters {
    /// One kernel `rz-*` call.
    pub(crate) fn note_rizin(&self, fact: RizinFact) {
        self.note_capability(
            match fact {
                RizinFact::Open => "rizin_open_count",
                RizinFact::DebugOpen => "rizin_debug_open_count",
                RizinFact::Command => "rizin_command_count",
                RizinFact::JsonCommand => "rizin_json_command_count",
                RizinFact::Interrupted => "rizin_interrupted_count",
                RizinFact::Died => "rizin_died_count",
                RizinFact::NotInstalled => "rizin_not_installed_count",
            },
            1,
        );
    }
}
