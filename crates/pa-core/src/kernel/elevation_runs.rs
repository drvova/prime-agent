//! The `elevation.fact` host request: each `bash` command the kernel was
//! asked to run elevated (`'elevated #t`) reports how its consent step ended
//! and which route asked for it, so the session can count them (adoption
//! telemetry). Never the command, its output, or anything the user typed.
//! The kernel sends it without awaiting the reply.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::kernel::shared::{host_handler, HostRequestHandlers};

/// How an elevated command's consent step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ElevationOutcome {
    /// The user approved (or no consent was needed): the command ran
    /// elevated.
    Approved,
    /// The user dismissed or refused the prompt: the command did not run.
    Declined,
    /// No consent prompt could be shown here: the command did not run.
    Unavailable,
}

/// Which way the consent was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConsentRoute {
    /// Windows: UAC, for the session's elevated helper.
    Uac,
    /// Linux and macOS: `sudo -A`, the password asked by a graphical askpass
    /// program.
    SudoAskpass,
    /// Linux: `pkexec`, the password asked by the desktop's polkit agent.
    Pkexec,
    /// `sudo -n`: no dialog exists, so it runs only when `sudo` needs no
    /// password.
    SudoNonInteractive,
    /// The session already runs as Administrator or root: no consent step.
    AlreadyElevated,
}

impl ElevationOutcome {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            ElevationOutcome::Approved => "approved",
            ElevationOutcome::Declined => "declined",
            ElevationOutcome::Unavailable => "unavailable",
        }
    }
}

impl ConsentRoute {
    pub(crate) fn wire(self) -> &'static str {
        match self {
            ConsentRoute::Uac => "uac",
            ConsentRoute::SudoAskpass => "sudo_askpass",
            ConsentRoute::Pkexec => "pkexec",
            ConsentRoute::SudoNonInteractive => "sudo_noninteractive",
            ConsentRoute::AlreadyElevated => "already_elevated",
        }
    }
}

/// One elevated command's consent step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ElevationRun {
    pub(crate) outcome: ElevationOutcome,
    pub(crate) route: ConsentRoute,
}

/// Hears each reported command.
pub(crate) type ElevationObserver = Arc<dyn Fn(ElevationRun) + Send + Sync>;

/// Register `elevation.fact` (a session without telemetry counts nothing).
pub(crate) fn register_elevation_handler(
    handlers: &mut HostRequestHandlers,
    observer: Option<ElevationObserver>,
) {
    handlers.register(
        "elevation.fact",
        host_handler(move |payload| {
            let observer = observer.clone();
            Box::pin(async move {
                let field = |name: &str| payload.data.get(name).and_then(Value::as_str);
                let outcome = [
                    ElevationOutcome::Approved,
                    ElevationOutcome::Declined,
                    ElevationOutcome::Unavailable,
                ]
                .into_iter()
                .find(|outcome| field("outcome") == Some(outcome.wire()))
                .ok_or_else(|| anyhow::anyhow!("elevation.fact: unknown outcome"))?;
                let route = [
                    ConsentRoute::Uac,
                    ConsentRoute::SudoAskpass,
                    ConsentRoute::Pkexec,
                    ConsentRoute::SudoNonInteractive,
                    ConsentRoute::AlreadyElevated,
                ]
                .into_iter()
                .find(|route| field("route") == Some(route.wire()))
                .ok_or_else(|| anyhow::anyhow!("elevation.fact: unknown route"))?;
                if let Some(observer) = observer {
                    observer(ElevationRun { outcome, route });
                }
                Ok(json!({}))
            })
        }),
    );
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::kernel::shared::HostRequestPayload;

    /// Each reported command reaches the observer; an unknown outcome or
    /// route is refused.
    #[tokio::test]
    async fn elevated_commands_are_counted_and_malformed_ones_refused() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut handlers = HostRequestHandlers::new();
        register_elevation_handler(
            &mut handlers,
            Some({
                let seen = Arc::clone(&seen);
                Arc::new(move |run| seen.lock().unwrap().push(run))
            }),
        );
        let handler = handlers.get("elevation.fact").expect("registered");
        let mut replies = Vec::new();
        for data in [
            json!({"outcome": "declined", "route": "pkexec"}),
            json!({"outcome": "maybe", "route": "pkexec"}),
            json!({"outcome": "approved"}),
        ] {
            replies.push(handler(HostRequestPayload { data }).await.is_ok());
        }
        assert_eq!(
            (replies, seen.lock().unwrap().clone()),
            (
                vec![true, false, false],
                vec![ElevationRun {
                    outcome: ElevationOutcome::Declined,
                    route: ConsentRoute::Pkexec,
                }],
            )
        );
    }
}
