//! `--elevated`: run Prime Agent with the machine's highest rights, as a choice the user makes
//! at launch and sees for as long as the window is open.
//!
//! Every session of a daemon runs in its process, so rights are a daemon's, never one
//! session's: an elevated Prime Agent talks to a daemon of its own (on Windows the elevated pipe,
//! `pa_daemon::platform::default_daemon_socket_path`; on Unix root's own socket directory), and
//! the user's normal daemon and its sessions are never elevated. On Windows, a launch that is not
//! elevated yet starts Prime Agent again through UAC, in a window of its own, and waits for it.
//! On Linux and macOS Prime Agent never raises its own rights: `--elevated` runs only as root.

use std::path::{Path, PathBuf};

use pa_types::platform::{Elevation, ElevationRoute};

/// Whose home a root process's `HOME` names: under `sudo` without `-H` it can
/// still be the invoking user's, and root would then write root-owned files
/// into that user's agent directory, which their own Prime Agent can no longer
/// change.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RootHome {
    /// Root's own, or not started through `sudo`.
    Own,
    /// `SUDO_USER` is set and `HOME` (this path) belongs to a uid other than
    /// root.
    Callers { user: String, home: PathBuf },
}

impl RootHome {
    /// The home of a process started by `sudo_user` (`SUDO_USER`) with `HOME`
    /// `home`, where `owner_of` reads a path's owning uid.
    pub(crate) fn of(
        sudo_user: Option<&str>,
        home: Option<&Path>,
        owner_of: impl Fn(&Path) -> Option<u32>,
    ) -> Self {
        match (sudo_user.filter(|user| !user.is_empty()), home) {
            (Some(user), Some(home)) if owner_of(home).is_some_and(|uid| uid != 0) => {
                RootHome::Callers {
                    user: user.to_string(),
                    home: home.to_path_buf(),
                }
            }
            (Some(_) | None, Some(_) | None) => RootHome::Own,
        }
    }
}

/// What an `--elevated` launch does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ElevatedLaunch {
    /// Run here: the flag was not given, or this process is already elevated.
    Proceed,
    /// Refuse, with the message that says how to start elevated instead.
    Refuse(String),
    /// Start Prime Agent again through UAC, with the same arguments, and wait for it.
    RelaunchThroughUac,
}

impl ElevatedLaunch {
    /// The launch for `requested` (whether `--elevated` was given), in a process of
    /// `elevation` on `route`'s platform, running the interactive mode or not, whose `HOME`
    /// is `home`'s.
    pub(crate) fn of(
        requested: bool,
        elevation: Elevation,
        route: ElevationRoute,
        interactive: bool,
        home: &RootHome,
    ) -> Self {
        let command = crate::config::COMMAND_NAME;
        match (requested, elevation, route) {
            (false, _, _) | (true, Elevation::Elevated, ElevationRoute::Uac) => {
                ElevatedLaunch::Proceed
            }
            (true, Elevation::Elevated, ElevationRoute::MacosSudo | ElevationRoute::UnixSudo) => {
                match home {
                    RootHome::Own => ElevatedLaunch::Proceed,
                    RootHome::Callers { user, home } => ElevatedLaunch::Refuse(format!(
                        "--elevated runs as root here, but HOME is still {user}'s ({}): root \
                         would write root-owned files into {user}'s Prime Agent data. Start it \
                         with `sudo -H {command} --elevated`, which gives root its own home.",
                        home.display()
                    )),
                }
            }
            (true, Elevation::Standard, ElevationRoute::Uac) if interactive => {
                ElevatedLaunch::RelaunchThroughUac
            }
            (true, Elevation::Standard, ElevationRoute::Uac) => ElevatedLaunch::Refuse(format!(
                "--elevated opens the interactive mode in a new Administrator window, so it \
                 cannot run a --print or --mode run: start that run from a terminal opened \
                 with \"Run as administrator\" instead, or run `{command} --elevated` alone."
            )),
            (true, Elevation::Standard, ElevationRoute::MacosSudo | ElevationRoute::UnixSudo) => {
                ElevatedLaunch::Refuse(format!(
                    "--elevated runs Prime Agent as root, and this process is not root. Prime \
                     Agent never raises its own rights on Linux or macOS: start it with \
                     `sudo -H {command} --elevated` (root keeps its own sessions, settings and \
                     sign-ins, under root's home), or run `{command}` as you are, and the agent \
                     asks you for each step that needs root."
                ))
            }
        }
    }
}

/// Carry out `--elevated` for this process: `None` when the run goes on here, the elevated
/// run's exit code when it ran in a window of its own.
///
/// # Errors
///
/// The refusal when this platform or mode cannot start elevated, a declined consent prompt, or
/// a launch the system refused.
pub(crate) fn launch(
    requested: bool,
    args: &[String],
    interactive: bool,
) -> Result<Option<i32>, String> {
    // Read only for an `--elevated` launch: a `sudo` that kept the caller's HOME.
    let home = if requested {
        RootHome::of(
            std::env::var("SUDO_USER").ok().as_deref(),
            std::env::var_os("HOME").as_deref().map(Path::new),
            |path| {
                #[cfg(unix)]
                return std::fs::metadata(path)
                    .ok()
                    .map(|metadata| std::os::unix::fs::MetadataExt::uid(&metadata));
                #[cfg(not(unix))]
                return {
                    let _ = path;
                    None
                };
            },
        )
    } else {
        RootHome::Own
    };
    match ElevatedLaunch::of(
        requested,
        pa_types::platform::process_elevation(),
        ElevationRoute::CURRENT,
        interactive,
        &home,
    ) {
        ElevatedLaunch::Proceed => Ok(None),
        ElevatedLaunch::Refuse(message) => Err(message),
        ElevatedLaunch::RelaunchThroughUac => {
            let program = std::env::current_exe().map_err(|error| {
                format!("cannot find this program to start it elevated: {error}")
            })?;
            let directory = std::env::current_dir()
                .map_err(|error| format!("cannot read the working directory: {error}"))?;
            eprintln!(
                "Prime Agent opens as Administrator in a new window once you approve the Windows \
                 prompt; this terminal waits until it closes."
            );
            match pa_types::platform::elevation::run_elevated(
                program.as_os_str(),
                args,
                directory.as_os_str(),
            ) {
                // The exit code is a DWORD; Windows reports NTSTATUS failures above i32::MAX.
                Ok(pa_types::platform::elevation::ElevatedRun::Exited(code)) => {
                    Ok(Some(i32::from_ne_bytes(code.to_ne_bytes())))
                }
                Ok(pa_types::platform::elevation::ElevatedRun::Declined) => Err(
                    "the Windows administrator prompt was declined: Prime Agent did not start \
                     elevated"
                        .to_string(),
                ),
                Err(error) => Err(format!("Prime Agent could not start elevated: {error}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Linux and macOS never raise Prime Agent's rights: a non-root `--elevated` is refused with
    /// the way to start as root, root runs, and Windows relaunches an interactive run through UAC.
    #[test]
    fn elevated_launches_refuse_on_unix_unless_root() {
        let command = crate::config::COMMAND_NAME;
        let unix_refusal = ElevatedLaunch::Refuse(format!(
            "--elevated runs Prime Agent as root, and this process is not root. Prime Agent never \
             raises its own rights on Linux or macOS: start it with `sudo -H {command} \
             --elevated` (root keeps its own sessions, settings and sign-ins, under root's \
             home), or run `{command}` as you are, and the agent asks you for each step that \
             needs root."
        ));
        let own = RootHome::Own;
        assert_eq!(
            [
                ElevatedLaunch::of(
                    true,
                    Elevation::Standard,
                    ElevationRoute::UnixSudo,
                    true,
                    &own
                ),
                ElevatedLaunch::of(
                    true,
                    Elevation::Standard,
                    ElevationRoute::MacosSudo,
                    true,
                    &own
                ),
                ElevatedLaunch::of(
                    true,
                    Elevation::Elevated,
                    ElevationRoute::UnixSudo,
                    true,
                    &own
                ),
                ElevatedLaunch::of(
                    false,
                    Elevation::Standard,
                    ElevationRoute::UnixSudo,
                    true,
                    &own
                ),
                ElevatedLaunch::of(true, Elevation::Standard, ElevationRoute::Uac, true, &own),
                ElevatedLaunch::of(true, Elevation::Elevated, ElevationRoute::Uac, false, &own),
            ],
            [
                unix_refusal.clone(),
                unix_refusal,
                ElevatedLaunch::Proceed,
                ElevatedLaunch::Proceed,
                ElevatedLaunch::RelaunchThroughUac,
                ElevatedLaunch::Proceed,
            ]
        );
    }

    /// Root under `sudo` without `-H` keeps the caller's HOME: an `--elevated` launch is refused
    /// with the way to give root its own home. Root's own HOME, and no `sudo`, run.
    #[test]
    fn root_with_the_callers_home_is_refused() {
        let command = crate::config::COMMAND_NAME;
        let owners = |path: &Path| match path.to_str() {
            Some("/home/me") => Some(1000),
            Some("/root") => Some(0),
            Some(_) | None => None,
        };
        let callers = RootHome::of(Some("me"), Some(Path::new("/home/me")), owners);
        assert_eq!(
            [
                callers.clone(),
                RootHome::of(Some("me"), Some(Path::new("/root")), owners),
                RootHome::of(None, Some(Path::new("/home/me")), owners),
                RootHome::of(Some(""), Some(Path::new("/home/me")), owners),
            ],
            [
                RootHome::Callers {
                    user: "me".to_string(),
                    home: PathBuf::from("/home/me"),
                },
                RootHome::Own,
                RootHome::Own,
                RootHome::Own,
            ]
        );
        assert_eq!(
            ElevatedLaunch::of(
                true,
                Elevation::Elevated,
                ElevationRoute::UnixSudo,
                true,
                &callers
            ),
            ElevatedLaunch::Refuse(format!(
                "--elevated runs as root here, but HOME is still me's (/home/me): root would \
                 write root-owned files into me's Prime Agent data. Start it with `sudo -H \
                 {command} --elevated`, which gives root its own home."
            ))
        );
    }
}
