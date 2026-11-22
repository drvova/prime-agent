//! The seam between the session's Frida bookkeeping and Frida itself: the
//! registry talks to these traits, the linked frida-core implements them
//! (`core.rs`, behind the `frida` cargo feature), and the tests implement
//! them with a recording fake.

use std::sync::Arc;

use super::cancel::Cancel;

/// A Frida call's result; the error is Frida's own message, unchanged.
pub(super) type Outcome<T> = Result<T, String>;

/// Receives one script message: Frida's JSON text and its binary payload.
/// Called on Frida's own thread, never on a session's: it must not block,
/// panic, or read per-session state.
pub(super) type MessageSink = Box<dyn Fn(&str, Option<&[u8]>) + Send + Sync>;

/// Receives the reason a session detached on its own (`process-terminated`,
/// `connection-terminated`, ...). Called on Frida's own thread, under the
/// same rules as a [`MessageSink`].
pub(super) type DetachSink = Box<dyn Fn(&str) + Send + Sync>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DeviceInfo {
    pub(super) id: String,
    pub(super) name: String,
    /// `local`, `remote` or `usb`.
    pub(super) kind: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProcessInfo {
    /// 0 for an installed application that is not running.
    pub(super) pid: u32,
    pub(super) name: String,
    /// An application's identifier; `None` for a plain process.
    pub(super) identifier: Option<String>,
}

/// How to reach a frida-server (`frida -H`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(super) struct RemoteOptions {
    /// Path of the PEM certificate the server must present.
    pub(super) certificate: Option<String>,
    pub(super) origin: Option<String>,
    pub(super) token: Option<String>,
    pub(super) keepalive_interval: Option<i32>,
}

/// A script's JavaScript runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Runtime {
    Default,
    Qjs,
    V8,
}

/// What a script is created from.
#[cfg_attr(
    not(feature = "frida"),
    allow(
        dead_code,
        reason = "read by the linked frida-core, which a build without the feature leaves out"
    )
)]
pub(super) struct ScriptSpec<'a> {
    pub(super) source: &'a str,
    pub(super) name: Option<&'a str>,
    pub(super) runtime: Runtime,
}

/// One session's connection to Frida: its devices, the processes on them,
/// and the sessions it opens.
///
/// Every method blocks its caller until Frida answers and must return
/// promptly once `cancel` is cancelled, with an error or with the result it
/// already had. A `device` is `None` for the local system, `"usb"`,
/// `"remote"`, or a device id. Implementations are called from several
/// threads at once and never from Frida's own callback thread.
pub(super) trait Backend: Send + Sync {
    fn devices(&self, cancel: &Cancel) -> Outcome<Vec<DeviceInfo>>;

    /// Connect a frida-server at `address` (`host[:port]`) and return the
    /// device it becomes.
    fn add_remote(
        &self,
        address: &str,
        options: &RemoteOptions,
        cancel: &Cancel,
    ) -> Outcome<DeviceInfo>;

    fn processes(&self, device: Option<&str>, cancel: &Cancel) -> Outcome<Vec<ProcessInfo>>;

    /// The device's installed applications, each with its identifier.
    fn applications(&self, device: Option<&str>, cancel: &Cancel) -> Outcome<Vec<ProcessInfo>>;

    /// The pid of the one process called `name`, by Frida's own name match.
    fn find_process(&self, device: Option<&str>, name: &str, cancel: &Cancel) -> Outcome<u32>;

    /// Start `program` suspended with `args` (not counting the program
    /// itself) and return its pid.
    fn spawn(
        &self,
        device: Option<&str>,
        program: &str,
        args: &[String],
        cancel: &Cancel,
    ) -> Outcome<u32>;

    /// Let a spawned process run.
    fn resume(&self, device: Option<&str>, pid: u32, cancel: &Cancel) -> Outcome<()>;

    fn kill(&self, device: Option<&str>, pid: u32, cancel: &Cancel) -> Outcome<()>;

    /// Attach to `pid`. `on_detached` is told once if the session ends on
    /// its own; it is never called after [`BackendSession::detach`] returned.
    fn attach(
        &self,
        device: Option<&str>,
        pid: u32,
        on_detached: DetachSink,
        cancel: &Cancel,
    ) -> Outcome<Arc<dyn BackendSession>>;

    /// Bundle the script project at `entrypoint` (Frida's compiler) and
    /// return the bundle's text.
    fn compile(
        &self,
        entrypoint: &str,
        project_root: Option<&str>,
        cancel: &Cancel,
    ) -> Outcome<String>;

    /// Give up the connection: every device and session of it is gone
    /// afterwards. Called once, when the owning session's kernel ends.
    fn close(&self, cancel: &Cancel);
}

/// A session on one process. Blocking and cancellation are as for
/// [`Backend`].
pub(super) trait BackendSession: Send + Sync {
    /// Create a script, not yet loaded, whose messages go to `sink`. The
    /// sink is not called after [`BackendScript::unload`] returned or the
    /// session detached.
    fn create_script(
        &self,
        spec: &ScriptSpec<'_>,
        sink: MessageSink,
        cancel: &Cancel,
    ) -> Outcome<Arc<dyn BackendScript>>;

    /// Detach from the process, which unloads the session's scripts. A
    /// session that is already detached succeeds.
    fn detach(&self, cancel: &Cancel) -> Outcome<()>;
}

/// A script of a session. Blocking and cancellation are as for [`Backend`].
pub(super) trait BackendScript: Send + Sync {
    /// Run the script's top level in the process.
    fn load(&self, cancel: &Cancel) -> Outcome<()>;

    fn unload(&self, cancel: &Cancel) -> Outcome<()>;

    /// Queue a JSON message (and binary payload) for the script's `recv()`.
    /// Returns at once: delivery is Frida's.
    fn post(&self, json: &str, data: Option<&[u8]>) -> Outcome<()>;
}
