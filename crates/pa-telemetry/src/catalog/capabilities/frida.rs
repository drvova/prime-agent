//! The `frida` capability.

use super::super::{count, required, uuid, EventRule};

/// Kernel `frida-*` (Frida dynamic instrumentation) adoption for one session (v2), emitted at
/// session end beside `agent session ended` (which sits near the backend's 128-key cap) only when
/// the session used it: device and process listings, kills, attaches and spawns, scripts loaded,
/// traces, compiler runs, and calls refused because the build links no Frida. Counts only: never
/// a target, a pid, a device, a script, or a path.
pub(in crate::catalog) const AGENT_REPL_FRIDA: EventRule = EventRule {
    name: "agent repl frida",
    since: 2,
    properties: &[
        ("session_id", required(uuid())),
        ("frida_devices_count", required(count())),
        ("frida_ps_count", required(count())),
        ("frida_kill_count", required(count())),
        ("frida_attach_count", required(count())),
        ("frida_spawn_count", required(count())),
        ("frida_script_load_count", required(count())),
        ("frida_trace_count", required(count())),
        ("frida_compile_count", required(count())),
        ("frida_unavailable_count", required(count())),
    ],
};
