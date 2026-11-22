//! `agent repl frida` (v2): kernel `frida-*` calls by kind, and calls refused because the build
//! links no Frida (counts only: never a target, a pid, a device, a script, or a path). The
//! `frida` capability's rule file names the event; session end emits it through the generic
//! capability counter.

use super::SessionCounters;
use crate::session_engine::frida_usage::FridaFact;

impl SessionCounters {
    /// One kernel `frida-*` call.
    pub(crate) fn note_frida(&self, fact: FridaFact) {
        self.note_capability(
            match fact {
                FridaFact::Devices => "frida_devices_count",
                FridaFact::Ps => "frida_ps_count",
                FridaFact::Kill => "frida_kill_count",
                FridaFact::Attach => "frida_attach_count",
                FridaFact::Spawn => "frida_spawn_count",
                FridaFact::ScriptLoad => "frida_script_load_count",
                FridaFact::Trace => "frida_trace_count",
                FridaFact::Compile => "frida_compile_count",
                FridaFact::Unavailable => "frida_unavailable_count",
            },
            1,
        );
    }
}
