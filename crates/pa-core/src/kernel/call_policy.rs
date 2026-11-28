//! The call policy: what the embedder says about each call a cell makes
//! before it runs.
//!
//! A session's kernel asks its [`CallPolicy`], when it has one, before every
//! host request reaches its handler and before every side-effecting native
//! runs. The host requests are asked in one place: the session's handlers
//! are wrapped as the kernel is built (`HostRequestHandlers::gated`), so a request a
//! cell sends through `(%host-request ...)` and one a native sends itself
//! (a file tool's `fs.read`, a command's `bash.completed`) are asked alike,
//! and nothing reaches a handler around the policy. The natives are asked at
//! the entry of each dispatcher a cell's functions reach them through
//! (`%native-call`, `%worktree-call`, `%hypotheses-call`, `%harness-call`,
//! `%factory-request-json`, `%rz-call`, `%mcp-call`): the check is in the
//! native itself, so it holds in whichever engine calls it (the engine
//! Steel's compiler runs macro bodies in has none of them).
//!
//! A refusal is the call's ordinary error: "refused for an untrusted turn:
//! <call>". A kernel without a policy asks nothing and behaves as before.

use std::sync::Arc;

use serde_json::Value;

/// Decides, before it runs, whether a call a session's cell makes may run.
///
/// The kernel consults it before every host request reaches its handler and
/// at the entry of every side-effecting native ([`CallKind`]). `admit` runs
/// on the cell's thread with the cell waiting, so it must answer at once
/// from state it already holds: it never blocks, awaits, or does I/O beyond
/// resolving a path it is shown. It is consulted for the kernel's own
/// reports too (a finished command's `bash.completed`, a cell's
/// `cell.completed`), which may arrive after the turn that started their
/// work, so a policy that refuses by the running turn admits those.
pub trait CallPolicy: Send + Sync {
    /// `Ok` lets the call run; `Err` refuses it, and the cell sees the
    /// refusal as the call's error.
    ///
    /// # Errors
    ///
    /// The call is refused.
    fn admit(&self, call: CallKind<'_>) -> Result<(), CallRefused>;
}

/// One call a policy is asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallKind<'a> {
    /// A host request of `kind` (`fs.read`, `rlm.run`), with the payload its
    /// handler would get.
    HostRequest { kind: &'a str, payload: &'a Value },
    /// A native of `family`, called as `name` (`bash`, `worktree-create`).
    Native { family: NativeFamily, name: &'a str },
}

/// The side-effecting natives, by what they reach. Every native operation a
/// cell can call through a dispatcher is in exactly one family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NativeFamily {
    /// `bash` and its job functions (`bash-wait`, `bash-kill`, ...).
    Bash,
    /// `fs-read`, `fs-grep`, `fs-find`: each sends its `fs.*`
    /// host request, which the policy is asked about with its paths.
    FileRead,
    /// `fs-symbol`: the project's symbol index.
    SymbolIndex,
    /// `fs-write`, `fs-edit`: they write files (an `'old`/`'new` edit
    /// without a host request).
    FileWrite,
    /// `worktree-*`: git worktrees of the project.
    Worktree,
    /// `rlm-hypotheses`: parallel approaches in worktrees.
    Hypotheses,
    /// `harness-*`: the harness store.
    Harness,
    /// `factory-*`: factory machines and their runs.
    Factory,
    /// `rz-*`: rizin sessions.
    Rizin,
    /// `mcp-*`: MCP servers' tools.
    Mcp,
}

impl NativeFamily {
    /// The family of a `%native-call` operation.
    pub(crate) fn of_native_call(operation: &str) -> NativeFamily {
        match operation {
            "fs-read" | "fs-grep" | "fs-find" => NativeFamily::FileRead,
            "fs-symbol" => NativeFamily::SymbolIndex,
            command if command.starts_with("bash") => NativeFamily::Bash,
            // `fs-write`, `fs-edit`, and an operation no tool has.
            _ => NativeFamily::FileWrite,
        }
    }
}

/// A call a [`CallPolicy`] refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallRefused {
    /// The refused call: a host request's kind or a native's name.
    pub call: String,
}

impl std::fmt::Display for CallRefused {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "refused for an untrusted turn: {}", self.call)
    }
}

impl std::error::Error for CallRefused {}

/// A kernel's hold on its policy: none admits every call.
#[derive(Clone, Default)]
pub(crate) struct Admission(pub(crate) Option<Arc<dyn CallPolicy>>);

impl Admission {
    /// Asks the policy about `call`.
    ///
    /// # Errors
    ///
    /// The policy refused it.
    pub(crate) fn admit(&self, call: CallKind<'_>) -> anyhow::Result<()> {
        match &self.0 {
            Some(policy) => policy.admit(call).map_err(anyhow::Error::new),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
