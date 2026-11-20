//! `agent repl frida` (v2): kernel `frida.*` calls by kind, and calls refused while frida was
//! unavailable or still installing (counts only: never a target, a pid, a script, or a path).

use super::{SessionCounters, SessionTelemetry, Value};
use crate::session_engine::frida_usage::FridaFact;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct FridaCounts {
    devices: u64,
    ps: u64,
    kill: u64,
    attach: u64,
    spawn: u64,
    script_load: u64,
    trace: u64,
    discover: u64,
    tool: u64,
    graft: u64,
    unavailable: u64,
    installing: u64,
}

impl SessionCounters {
    /// One kernel `frida.*` call.
    pub(crate) fn note_frida(&self, fact: FridaFact) {
        self.with(|values| {
            let counts = &mut values.frida;
            *match fact {
                FridaFact::Devices => &mut counts.devices,
                FridaFact::Ps => &mut counts.ps,
                FridaFact::Kill => &mut counts.kill,
                FridaFact::Attach => &mut counts.attach,
                FridaFact::Spawn => &mut counts.spawn,
                FridaFact::ScriptLoad => &mut counts.script_load,
                FridaFact::Trace => &mut counts.trace,
                FridaFact::Discover => &mut counts.discover,
                FridaFact::Tool => &mut counts.tool,
                FridaFact::Graft => &mut counts.graft,
                FridaFact::Unavailable => &mut counts.unavailable,
                FridaFact::Installing => &mut counts.installing,
            } += 1;
        });
    }
}

impl SessionTelemetry {
    /// Track `agent repl frida`, unless the session never called `frida.*`.
    pub(super) fn track_frida(&self) {
        let mut counts = FridaCounts::default();
        self.counters.with(|values| counts = values.frida);
        if counts == FridaCounts::default() {
            return;
        }
        let mut properties = self.session_properties();
        for (key, value) in [
            ("frida_devices_count", counts.devices),
            ("frida_ps_count", counts.ps),
            ("frida_kill_count", counts.kill),
            ("frida_attach_count", counts.attach),
            ("frida_spawn_count", counts.spawn),
            ("frida_script_load_count", counts.script_load),
            ("frida_trace_count", counts.trace),
            ("frida_discover_count", counts.discover),
            ("frida_tool_count", counts.tool),
            ("frida_graft_count", counts.graft),
            ("frida_unavailable_count", counts.unavailable),
            ("frida_installing_count", counts.installing),
        ] {
            properties.set(key, Value::from(value));
        }
        self.client.track("agent repl frida", properties);
    }
}
