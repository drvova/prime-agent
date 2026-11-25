//! `(bash command (hash 'elevated #t))`: one command run with the machine's
//! highest rights (Administrator on Windows, root on Linux and macOS) once
//! the user has consented through the operating system's own prompt, its
//! output captured and its result shaped as any command's.
//!
//! Consent is never Prime Agent's to give. The model cannot elevate
//! silently: every elevated command passes a prompt the user answers on
//! their own screen (on Windows one prompt opens the session's elevated
//! helper for a while, below), and no password ever passes through the
//! model, the transcript or this process. A refused or impossible consent
//! is a result, not an error: exit code 126 and an output that starts with
//! [`DECLINED`] or [`UNAVAILABLE`] and says what the user can do instead.
//! Each elevated command is counted by outcome and route
//! (`elevation.fact`, [`crate::kernel::elevation_runs`]).
//!
//! An elevated command is a job like any other: it waits, yields, runs in
//! the background, is listed, tailed and killed by the same functions, and
//! a cell that is stopped while it waits (for its consent too) stops it.
//! It is never queued behind the session's job limit: a consent prompt
//! that appeared minutes after the call would be a surprise.
//!
//! A session that already runs as Administrator or root runs the command as
//! any other, with no consent step.
//!
//! # Linux and macOS
//!
//! There is no lasting root process. Each command runs through `sudo -A`
//! with an askpass program that asks for the password in a window (the
//! session's `SUDO_ASKPASS`; on Linux with a graphical session, an
//! `ssh-askpass` program or a script over `zenity`/`kdialog`; on macOS a
//! script over the system's `osascript` dialog), through `pkexec` on a Linux
//! desktop with no askpass program (its polkit agent asks), or, with no way
//! to ask at all (a headless SSH login), through `sudo -n`, which runs only
//! when `sudo` needs no password. The prompt names the command.
//!
//! Root runs a small `bash` script ([`WRAPPER`]) that marks the output once
//! it runs (so this side tells a consented command from a refused prompt),
//! starts the command in a process group of its own, and holds the command's
//! lifeline: its standard input is a pipe whose only writing end this
//! process keeps. A root process cannot be signalled by the user's own
//! rights, so the lifeline is how the command is stopped: when the job is
//! killed, its cell is stopped, the kernel ends, or this process dies, the
//! pipe closes, and the script ends the command's group (`TERM`, then
//! `KILL` two seconds later). The built-in shell lives in this process, so an
//! elevated command runs on the system's `bash` instead.
//!
//! # Windows
//!
//! See `bash_elevated/windows.rs`: the first elevated command of a session
//! starts an elevated helper through UAC, which runs the session's elevated
//! commands with their output piped back, until the session ends or the
//! helper has been idle for ten minutes.

#[cfg_attr(
    not(windows),
    expect(
        dead_code,
        reason = "off Windows there is no helper to talk to: only the tests use the wire"
    )
)]
#[path = "bash_elevated/protocol.rs"]
mod protocol;
#[cfg(all(test, target_os = "linux"))]
#[path = "bash_elevated/tests.rs"]
mod tests;
#[cfg(windows)]
#[path = "bash_elevated/windows.rs"]
mod windows;

use std::sync::{Arc, Mutex};

use anyhow::bail;
use serde_json::json;

use super::job::{self, lock, Job};
use super::Session;
use crate::kernel::elevation_runs::{ConsentRoute, ElevationOutcome};
use crate::kernel::shared::HostRequestPayload;

#[cfg(windows)]
pub(crate) use windows::HelperSlot;

/// How the result of a command whose consent was refused starts.
pub(super) const DECLINED: &str = "elevation declined:";
/// How the result of a command no consent could be asked for starts.
pub(super) const UNAVAILABLE: &str = "elevation unavailable:";
/// The exit code of an elevated command that did not run.
const NOT_RUN: i32 = 126;

/// In debug builds, a session variable naming a program that stands in for
/// `sudo` (taking its arguments), so tests need no `sudo`. It also turns off
/// the already-elevated shortcut and `pkexec`.
#[cfg(debug_assertions)]
pub(super) const RUNNER_VARIABLE: &str = "PRIME_AGENT_ELEVATION_RUNNER";
/// In debug builds, a session variable that turns off the already-elevated
/// shortcut, so a test machine that runs as Administrator (GitHub's Windows
/// runners, with UAC off) takes the consent route: on Windows the elevated
/// helper, which `runas` starts there without a prompt.
#[cfg(debug_assertions)]
pub(super) const ALWAYS_ASK_VARIABLE: &str = "PRIME_AGENT_ELEVATION_ALWAYS_ASK";

/// The elevated part of a job: how the command is stopped, which this
/// process's own rights cannot do.
pub(super) struct Elevated {
    /// Taken by the first stop.
    stop: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

impl Elevated {
    fn new(stop: Box<dyn FnOnce() + Send>) -> Self {
        Elevated {
            stop: Mutex::new(Some(stop)),
        }
    }

    /// End the elevated command and everything it started (Linux and macOS:
    /// close its lifeline; Windows: ask the helper). Safe to call again.
    pub(super) fn stop(&self) {
        if let Some(stop) = lock(&self.stop).take() {
            stop();
        }
    }

    #[cfg(unix)]
    fn was_stopped(&self) -> bool {
        lock(&self.stop).is_none()
    }
}

/// `name` as the session's commands see it: its own environment over the
/// process's.
fn session_var(session: &Session, name: &str) -> Option<String> {
    session
        .host
        .settings
        .env
        .iter()
        .rev()
        .find(|(variable, _)| variable == name)
        .map(|(_, value)| value.clone())
        .or_else(|| pa_types::session_scope::var(name).ok())
        .filter(|value| !value.is_empty())
}

/// Count one elevated command (adoption telemetry): nothing waits for it,
/// and a count the host refuses changes nothing here.
fn report(session: &Session, outcome: ElevationOutcome, route: ConsentRoute) {
    if let Some(handler) = session.host.handlers.get("elevation.fact") {
        let counted = handler(HostRequestPayload {
            data: json!({ "outcome": outcome.wire(), "route": route.wire() }),
        });
        session.runtime.spawn(async move {
            let _ = counted.await;
        });
    }
}

/// The account an elevated command acts as, as the user knows it.
fn account() -> &'static str {
    pa_types::platform::ElevationRoute::CURRENT.elevated_account()
}

/// A job for `command` that never ran: its result is `message`, at once.
///
/// # Errors
///
/// The kernel was shut down.
#[cfg(not(windows))]
fn refuse(
    session: &Arc<Session>,
    command: &str,
    outcome: ElevationOutcome,
    route: ConsentRoute,
    message: String,
) -> anyhow::Result<Arc<Job>> {
    let job = Arc::new(Job::new(
        session,
        command,
        super::engine::job_number(),
        uuid::Uuid::new_v4().simple().to_string(),
        /*lasting*/ None,
    ));
    if job::enlist(session, &job, /*may_queue*/ false).is_none() {
        bail!("the kernel was shut down");
    }
    report(session, outcome, route);
    job::settle(session, &job, NOT_RUN, Some(message));
    job::retire(session, &job);
    Ok(job)
}

/// Start `command` elevated, as a job of `session` that is never queued.
/// The consent prompt, if any, appears now; the job runs while it waits.
///
/// # Errors
///
/// No working directory, a pipe or a process that could not be started, or
/// a kernel already killed. Nothing the command would have done has
/// happened in any of these. A refused or impossible consent is the job's
/// result, not an error.
pub(super) fn spawn(session: &Arc<Session>, command: &str) -> anyhow::Result<Arc<Job>> {
    if !session.cwd.is_dir() {
        bail!(
            "the working directory does not exist: {}",
            session.cwd.display()
        );
    }
    #[cfg(debug_assertions)]
    let always_ask = [RUNNER_VARIABLE, ALWAYS_ASK_VARIABLE]
        .iter()
        .any(|name| session_var(session, name).is_some());
    #[cfg(not(debug_assertions))]
    let always_ask = false;
    if !always_ask
        && pa_types::platform::process_elevation() == pa_types::platform::Elevation::Elevated
    {
        report(
            session,
            ElevationOutcome::Approved,
            ConsentRoute::AlreadyElevated,
        );
        return job::spawn(session, command, /*on_finished*/ None);
    }
    #[cfg(windows)]
    return windows::spawn(session, command);
    #[cfg(unix)]
    return unix::spawn(session, command);
    #[cfg(not(any(unix, windows)))]
    return refuse(
        session,
        command,
        ElevationOutcome::Unavailable,
        ConsentRoute::SudoNonInteractive,
        format!("{UNAVAILABLE} this platform has no way to run a command elevated"),
    );
}

/// Run the elevated helper (`prime-agent --elevation-helper <pipe>
/// <session-pid> <token>`): Windows only, started by a session through UAC.
/// Returns the process's exit code.
#[must_use]
pub fn run_elevation_helper(args: &[String]) -> i32 {
    #[cfg(windows)]
    return windows::run_helper(args);
    #[cfg(not(windows))]
    {
        let _ = args;
        eprintln!("--elevation-helper runs only on Windows, started by a Prime Agent session");
        2
    }
}

#[cfg(unix)]
mod unix {
    use std::ffi::OsString;
    use std::fmt::Write as _;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{anyhow, Context};

    use super::super::job::{self, lock, GroupHold, Job};
    use super::super::Session;
    use super::{account, refuse, report, session_var, Elevated, DECLINED, NOT_RUN, UNAVAILABLE};
    use crate::kernel::elevation_runs::{ConsentRoute, ElevationOutcome};
    use crate::kernel::orphan_journal::record_orphan_process_state;
    use crate::platform;

    /// How long output is still read after the runner ends: a process that
    /// holds the inherited pipe must not hold the result open.
    const EXIT_PIPE_GRACE: Duration = Duration::from_millis(500);

    /// The script root runs: `$1` the working directory, `$2` the output
    /// mark, `$3` the command. Its standard input is the lifeline, moved to
    /// descriptor 3 so the command reads nothing from it and only the
    /// watcher holds it.
    pub(super) const WRAPPER: &str = r#"exec 3<&0 0</dev/null
cd -- "$1" || exit 126
printf '\036prime-agent-elevated:%s\037' "$2"
set -m
"$BASH" -c "$3" 3<&- &
command_pid=$!
set +m
end() {
  kill -TERM -- "-$command_pid" 2>/dev/null || kill -TERM "$command_pid" 2>/dev/null
}
{ while IFS= read -r _; do :; done
  end
  sleep 2
  kill -KILL -- "-$command_pid" 2>/dev/null || kill -KILL "$command_pid" 2>/dev/null
} <&3 3<&- &
watcher=$!
exec 3<&-
trap end TERM HUP INT
while :; do
  wait "$command_pid"
  status=$?
  kill -0 "$command_pid" 2>/dev/null || break
done
kill "$watcher" 2>/dev/null
exit "$status"
"#;

    /// Askpass programs that ask for a password in a window, by name on
    /// `PATH` and then where distributions install them.
    const ASKPASS_PROGRAMS: [&str; 4] = [
        "ssh-askpass",
        "ksshaskpass",
        "lxqt-openssh-askpass",
        "x11-ssh-askpass",
    ];
    const ASKPASS_PATHS: [&str; 5] = [
        "/usr/lib/ssh/ssh-askpass",
        "/usr/lib/openssh/gnome-ssh-askpass",
        "/usr/libexec/openssh/gnome-ssh-askpass",
        "/usr/libexec/openssh/ssh-askpass",
        "/usr/libexec/seahorse/ssh-askpass",
    ];

    /// How root is asked for.
    enum Way {
        /// `sudo -A` with this askpass program.
        Askpass(PathBuf),
        /// `pkexec`: the desktop's polkit agent asks.
        Pkexec,
        /// `sudo -n`: nothing can ask.
        NonInteractive,
    }

    impl Way {
        fn route(&self) -> ConsentRoute {
            match self {
                Way::Askpass(_) => ConsentRoute::SudoAskpass,
                Way::Pkexec => ConsentRoute::Pkexec,
                Way::NonInteractive => ConsentRoute::SudoNonInteractive,
            }
        }
    }

    fn executable(path: &Path) -> bool {
        path.metadata()
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }

    /// Write `body` as the askpass script `name` under the session's agent
    /// directory (the user's own, mode 0700), unless it is there already.
    fn askpass_script(session: &Session, name: &str, body: &str) -> Option<PathBuf> {
        let dir = session.host.settings.agent_dir.as_ref()?.join("elevation");
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() == Some(body) {
            return executable(&path).then_some(path);
        }
        // What is left of an ended session lifetime writes nothing.
        pa_types::session_scope::ensure_live().ok()?;
        std::fs::create_dir_all(&dir).ok()?;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).ok()?;
        // Written beside it and renamed over it: `sudo` never runs half a script.
        let staged = dir.join(format!("{name}.{}", uuid::Uuid::new_v4().simple()));
        std::fs::write(&staged, body).ok()?;
        let placed = std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o700))
            .and_then(|()| std::fs::rename(&staged, &path));
        if placed.is_err() {
            let _ = std::fs::remove_file(&staged);
            return None;
        }
        Some(path)
    }

    /// `text` as one single-quoted shell word.
    fn quoted(text: &Path) -> String {
        format!("'{}'", text.to_string_lossy().replace('\'', r"'\''"))
    }

    /// How this session asks for root, with the program that does, or why
    /// it cannot.
    fn way(session: &Session) -> Result<(PathBuf, Way), String> {
        let path = session_var(session, "PATH").unwrap_or_default();
        let find = |name: &str| {
            std::env::split_paths(&path)
                .map(|dir| dir.join(name))
                .find(|candidate| executable(candidate))
        };
        #[cfg(debug_assertions)]
        let injected = session_var(session, super::RUNNER_VARIABLE).map(PathBuf::from);
        #[cfg(not(debug_assertions))]
        let injected: Option<PathBuf> = None;
        let graphical = ["DISPLAY", "WAYLAND_DISPLAY"]
            .iter()
            .any(|name| session_var(session, name).is_some());
        let askpass = session_var(session, "SUDO_ASKPASS")
            .map(PathBuf::from)
            .filter(|program| executable(program))
            .or_else(|| {
                if cfg!(target_os = "macos") {
                    return askpass_script(
                        session,
                        "askpass-macos",
                        "#!/bin/sh\nexec /usr/bin/osascript -e 'on run argv' -e 'text returned of (display dialog (item 1 of argv) default answer \"\" with hidden answer with title \"Prime Agent\" with icon caution)' -e 'end run' \"$1\"\n",
                    );
                }
                if !graphical {
                    return None;
                }
                ASKPASS_PROGRAMS
                    .iter()
                    .find_map(|name| find(name))
                    .or_else(|| {
                        ASKPASS_PATHS
                            .iter()
                            .map(PathBuf::from)
                            .find(|candidate| executable(candidate))
                    })
                    .or_else(|| {
                        let zenity = find("zenity")?;
                        askpass_script(
                            session,
                            "askpass-zenity",
                            &format!(
                                "#!/bin/sh\nexec {} --entry --hide-text --title='Prime Agent' --text=\"$1\"\n",
                                quoted(&zenity)
                            ),
                        )
                    })
                    .or_else(|| {
                        let kdialog = find("kdialog")?;
                        askpass_script(
                            session,
                            "askpass-kdialog",
                            &format!(
                                "#!/bin/sh\nexec {} --title 'Prime Agent' --password \"$1\"\n",
                                quoted(&kdialog)
                            ),
                        )
                    })
            });
        let sudo = injected.clone().or_else(|| find("sudo"));
        if let (Some(sudo), Some(askpass)) = (&sudo, askpass) {
            return Ok((sudo.clone(), Way::Askpass(askpass)));
        }
        if cfg!(target_os = "linux") && graphical && injected.is_none() {
            if let Some(pkexec) = find("pkexec") {
                return Ok((pkexec, Way::Pkexec));
            }
        }
        sudo.map(|sudo| (sudo, Way::NonInteractive)).ok_or_else(|| {
            format!(
                "{UNAVAILABLE} neither sudo nor pkexec is installed, so the command did not run. Give the user the exact command to run as {} themselves.",
                account()
            )
        })
    }

    /// See [`super::spawn`].
    pub(super) fn spawn(session: &Arc<Session>, command: &str) -> anyhow::Result<Arc<Job>> {
        let (program, way) = match way(session) {
            Ok(found) => found,
            Err(why) => {
                return refuse(
                    session,
                    command,
                    ElevationOutcome::Unavailable,
                    ConsentRoute::SudoNonInteractive,
                    why,
                );
            }
        };
        let settings = &session.host.settings;
        let shell = platform::resolve_kernel_bash_shell(settings.shell.as_deref())
            .ok()
            .flatten()
            .unwrap_or_else(|| "/bin/bash".to_string());
        let source = match &settings.command_prefix {
            Some(prefix) => format!("{prefix}\n{command}"),
            None => command.to_string(),
        };
        let mark = uuid::Uuid::new_v4().simple().to_string();
        // Root's environment is the one `sudo` or `pkexec` makes, plus the
        // settings no command may do without (no pager, no prompt).
        let exports =
            job::child_env(&[])
                .into_iter()
                .fold(String::new(), |mut exports, (name, value)| {
                    // Writing to a string cannot fail.
                    let _ = writeln!(exports, "export {name}='{}'", value.replace('\'', r"'\''"));
                    exports
                });
        let shown: String = command
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(120)
            .collect();
        let shown = if shown.len() < command.trim_end().len() {
            format!("{shown} ...")
        } else {
            shown
        };
        let mut args: Vec<OsString> = match &way {
            Way::Askpass(_) => vec![
                "-A".into(),
                "-p".into(),
                format!(
                    "Prime Agent asks to run as root: {}. Password for %p: ",
                    shown.replace('%', "%%")
                )
                .into(),
                "--".into(),
            ],
            Way::NonInteractive => vec!["-n".into(), "--".into()],
            Way::Pkexec => Vec::new(),
        };
        args.extend([
            shell.into(),
            "-c".into(),
            format!("{exports}{WRAPPER}").into(),
            "prime-agent-elevated".into(),
            session.cwd.clone().into_os_string(),
            mark.clone().into(),
            source.into(),
        ]);
        let (lifeline, lifeline_end) =
            std::io::pipe().context("could not open the elevated command's lifeline")?;
        let (output, output_end) = std::io::pipe().context("could not open the output pipe")?;
        let mut process = pa_types::session_scope::tokio_command(&program);
        process
            .args(args)
            .current_dir(&session.cwd)
            .envs(job::child_env(&settings.env))
            .stdin(lifeline)
            .stdout(
                output_end
                    .try_clone()
                    .context("could not open the output pipe")?,
            )
            .stderr(output_end)
            .kill_on_drop(true);
        if let Way::Askpass(askpass) = &way {
            process.env("SUDO_ASKPASS", askpass);
        }
        platform::process::set_new_session(process.as_std_mut());
        let context = session.runtime.enter();
        let mut child = process
            .spawn()
            .with_context(|| format!("could not start {}", program.display()))?;
        drop(context);
        drop(process);
        let pid = child
            .id()
            .ok_or_else(|| anyhow!("{} ended before it had a pid", program.display()))?;
        record_orphan_process_state(pid as i32, true);
        let abandon = |child: &mut tokio::process::Child, reason: anyhow::Error| {
            // A runner that already died needs no kill.
            let _ = child.start_kill();
            record_orphan_process_state(pid as i32, false);
            reason
        };
        let mut job = Job::new(
            session,
            command,
            pid,
            uuid::Uuid::new_v4().simple().to_string(),
            /*lasting*/ None,
        );
        // Closing the lifeline is how root's command is stopped.
        job.elevated = Some(Elevated::new(Box::new(move || drop(lifeline_end))));
        // Its output stays out of the spill store: it carries root's mark.
        lock(&job.capture).copy = None;
        let job = Arc::new(job);
        // The output carries no fence: its end is the runner's.
        let fence = format!(
            "\x1eprime-agent-unfenced:{}\x1f",
            uuid::Uuid::new_v4().simple()
        );
        if let Err(error) = job::read_output(&job, output, fence.into_bytes()) {
            return Err(abandon(
                &mut child,
                anyhow!("could not start the output reader: {error}"),
            ));
        }
        if job::enlist(session, &job, /*may_queue*/ false).is_none() {
            return Err(abandon(&mut child, anyhow!("the kernel was shut down")));
        }
        session.runtime.spawn(supervise(
            Arc::clone(session),
            GroupHold(Arc::clone(&job)),
            child,
            mark,
            way,
        ));
        Ok(job)
    }

    /// Watch one elevated command to its end: tell a consented run from a
    /// refused prompt by the mark root prints, count it, and settle it.
    async fn supervise(
        session: Arc<Session>,
        hold: GroupHold,
        mut child: tokio::process::Child,
        mark: String,
        way: Way,
    ) {
        let job = Arc::clone(&hold.0);
        let status = child.wait().await;
        let code = status
            .as_ref()
            .ok()
            .and_then(platform::normalized_exit_code)
            .unwrap_or(-1);
        let closed = job.until(|job| lock(&job.capture).eof);
        let _ = tokio::time::timeout(EXIT_PIPE_GRACE, closed).await;
        lock(&job.capture).settle();
        // Only a persistent shell's text can fail to read.
        let text = job.text().unwrap_or_default();
        let marked = format!("\x1eprime-agent-elevated:{mark}\x1f");
        let stopped = job.elevated.as_ref().is_some_and(Elevated::was_stopped);
        let (output, exit_code, outcome) = match text.split_once(&marked) {
            // What came before the mark is the runner's own (a `sudo` lecture).
            Some((_, ran)) => (ran.to_string(), code, Some(ElevationOutcome::Approved)),
            None if stopped => (text, code, None),
            None => {
                let outcome = match (&way, code) {
                    (Way::Pkexec, 127) | (Way::NonInteractive, _) => ElevationOutcome::Unavailable,
                    (Way::Pkexec | Way::Askpass(_), _) => ElevationOutcome::Declined,
                };
                let why = match (outcome, &way) {
                    (ElevationOutcome::Unavailable, Way::Pkexec) => format!(
                        "{UNAVAILABLE} pkexec found no polkit agent to ask the user, so the command did not run. Give the user the exact command to run in their own terminal with sudo."
                    ),
                    (ElevationOutcome::Unavailable, _) => format!(
                        "{UNAVAILABLE} no password dialog can be shown here (no graphical session with an askpass program or a polkit agent) and sudo needs a password, so the command did not run. Give the user the exact command to run in their own terminal with sudo."
                    ),
                    (ElevationOutcome::Declined | ElevationOutcome::Approved, _) => format!(
                        "{DECLINED} the request to run this command as {} was refused or dismissed, so it did not run. Ask the user before trying again.",
                        account()
                    ),
                };
                let said = text.trim_end();
                let output = if said.is_empty() {
                    format!("{why}\n")
                } else {
                    format!("{why}\n{said}\n")
                };
                (output, NOT_RUN, Some(outcome))
            }
        };
        if let Some(outcome) = outcome {
            report(&session, outcome, way.route());
        }
        job::settle(&session, &job, exit_code, Some(output));
        let gone = job.destroy();
        job::retire(&session, &job);
        if gone {
            record_orphan_process_state(job.pid as i32, false);
        }
    }
}
