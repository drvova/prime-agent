//! The local Chrome/Chromium a rendered crawl drives.
//!
//! The browser is found on the session's `PATH` or in a platform's usual
//! install places (`CHROME_PATH` in the session's environment names one
//! outright), and started here rather than by spider, so it obeys the rules
//! every program a session starts obeys: its own session and process group
//! on Unix (its own kill-on-close job on Windows), listed in the orphan
//! journal while it runs, and killed with its whole group when the crawl
//! ends for any reason. The crawl owns the [`Browser`]; dropping the crawl's
//! future (a stopped cell, a deadline, a session's end) drops it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{anyhow, bail, Context};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::kernel::orphan_journal::{note_orphan_process_state, OrphanState};

/// How long a started browser has to print its `DevTools` endpoint.
const STARTUP_WAIT: Duration = Duration::from_secs(20);

/// The session variable naming the browser to use.
pub(crate) const BROWSER_PATH_VAR: &str = "CHROME_PATH";

/// What the error for a missing browser says to do.
pub(crate) const NO_BROWSER: &str = "web-crawl #:render #t needs Chrome or Chromium and none was found on PATH or in its usual install places; install one (Debian/Ubuntu: `sudo apt install chromium`, macOS: `brew install --cask chromium`, Windows: `winget install Google.Chrome`) or set CHROME_PATH to its executable";

/// Program names a Chromium-family browser goes by on `PATH`.
const NAMES: [&str; 7] = [
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "chrome",
    "microsoft-edge",
    "msedge",
];

/// The browser to start: `CHROME_PATH`, else the first Chromium-family
/// program on the session's `PATH`, else one in its platform's install
/// places.
pub(crate) fn find() -> Option<PathBuf> {
    if let Some(named) = pa_types::session_scope::var_os(BROWSER_PATH_VAR) {
        let named = PathBuf::from(named);
        return crate::platform::is_executable(&named).then_some(named);
    }
    let on_path = pa_types::session_scope::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .unwrap_or_default();
    let suffixes: &[&str] = if cfg!(windows) { &[".exe"] } else { &[""] };
    on_path
        .iter()
        .flat_map(|dir| {
            NAMES.iter().flat_map(move |name| {
                suffixes
                    .iter()
                    .map(move |suffix| dir.join(format!("{name}{suffix}")))
            })
        })
        .chain(install_places())
        .find(|candidate| crate::platform::is_executable(candidate))
}

/// Where the platform's installers put a Chromium-family browser.
fn install_places() -> Vec<PathBuf> {
    #[cfg(target_os = "macos")]
    return [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
    #[cfg(windows)]
    return ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"]
        .into_iter()
        .filter_map(pa_types::session_scope::var_os)
        .flat_map(|root| {
            let root = PathBuf::from(root);
            [
                root.join(r"Google\Chrome\Application\chrome.exe"),
                root.join(r"Chromium\Application\chrome.exe"),
                root.join(r"Microsoft\Edge\Application\msedge.exe"),
            ]
        })
        .collect();
    #[cfg(all(unix, not(target_os = "macos")))]
    return [
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/snap/bin/chromium",
        "/opt/google/chrome/chrome",
    ]
    .into_iter()
    .map(PathBuf::from)
    .collect();
}

/// A started headless browser. Dropping it kills the browser and every
/// process it started, and removes its profile directory.
pub(crate) struct Browser {
    child: tokio::process::Child,
    pid: Option<u32>,
    profile: PathBuf,
    /// The `DevTools` websocket spider connects to.
    pub(crate) endpoint: String,
    #[cfg(windows)]
    job: crate::kernel::scheme::JobObject,
}

impl Browser {
    /// Start `program` headless on a fresh profile, with `extra_args` after
    /// the fixed ones, and wait for its `DevTools` endpoint.
    ///
    /// # Errors
    ///
    /// The program could not be started or contained, or printed no
    /// endpoint within [`STARTUP_WAIT`] (the error carries what it printed).
    pub(crate) async fn start(program: &Path, extra_args: &[String]) -> anyhow::Result<Browser> {
        static NEXT_PROFILE: AtomicU64 = AtomicU64::new(0);
        let profile = std::env::temp_dir().join(format!(
            "prime-agent-browser-{}-{}",
            std::process::id(),
            NEXT_PROFILE.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&profile)
            .with_context(|| format!("creating the browser profile {}", profile.display()))?;
        let mut command = pa_types::session_scope::std_command(program);
        command
            .args([
                "--headless=new",
                "--remote-debugging-address=127.0.0.1",
                "--remote-debugging-port=0",
                "--no-first-run",
                "--no-default-browser-check",
                "--disable-background-networking",
                "--disable-component-update",
                "--disable-default-apps",
                "--disable-extensions",
                "--disable-sync",
                "--disable-gpu",
                "--mute-audio",
                "--password-store=basic",
                "--use-mock-keychain",
            ])
            .arg(format!("--user-data-dir={}", profile.display()))
            .args(extra_args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped());
        // Chrome refuses to start its sandbox as root.
        #[cfg(unix)]
        if nix::unistd::geteuid().is_root() {
            command.arg("--no-sandbox");
        }
        command.arg("about:blank");
        #[cfg(unix)]
        crate::platform::process::set_new_session(&mut command);
        #[cfg(not(unix))]
        crate::platform::set_new_process_group(&mut command);
        let mut command = tokio::process::Command::from(command);
        command.kill_on_drop(true);
        #[cfg(windows)]
        let job = crate::kernel::scheme::JobObject::create()
            .ok_or_else(|| anyhow!("Windows job containment for the browser is unavailable"))?;
        let child = command
            .spawn()
            .with_context(|| format!("starting {}", program.display()))?;
        let pid = child.id();
        // From here the guard owns the process: every early return kills it.
        let mut browser = Browser {
            child,
            pid,
            profile,
            endpoint: String::new(),
            #[cfg(windows)]
            job,
        };
        #[cfg(windows)]
        if !browser
            .child
            .raw_handle()
            .is_some_and(|process| browser.job.assign(process))
        {
            bail!("Windows job containment for the browser could not be established");
        }
        if let Some(pid) = pid {
            note_orphan_process_state(pid as i32, OrphanState::Active);
        }
        let stderr = browser
            .child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("the browser's error stream was not opened"))?;
        let mut lines = BufReader::new(stderr).lines();
        let mut printed = Vec::new();
        let found = tokio::time::timeout(STARTUP_WAIT, async {
            while let Some(line) = lines.next_line().await? {
                if let Some(at) = line.find("ws://") {
                    return Ok::<_, std::io::Error>(Some(line[at..].trim().to_string()));
                }
                if printed.len() < 20 {
                    printed.push(line);
                }
            }
            Ok(None)
        })
        .await;
        match found {
            Ok(Ok(Some(endpoint))) => browser.endpoint = endpoint,
            Ok(Ok(None)) => bail!(
                "{} ended before it was ready: {}",
                program.display(),
                printed.join(" | ")
            ),
            Ok(Err(error)) => {
                return Err(error).context(format!("reading {}'s output", program.display()))
            }
            Err(_) => bail!(
                "{} printed no DevTools endpoint within {}s: {}",
                program.display(),
                STARTUP_WAIT.as_secs(),
                printed.join(" | ")
            ),
        }
        // The rest of its output is read and dropped, so a full pipe never
        // stalls the browser; the task ends with the browser.
        tokio::spawn(async move { while let Ok(Some(_)) = lines.next_line().await {} });
        Ok(browser)
    }
}

impl Drop for Browser {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.pid {
            // The browser leads its own group: this reaches its renderers.
            let _ = crate::platform::process::signal_process_group(
                pid as i32,
                crate::platform::Signal::Kill,
            );
        }
        #[cfg(windows)]
        self.job.terminate();
        // Whatever the group kill missed (or a platform without groups).
        let _ = self.child.start_kill();
        if let Some(pid) = self.pid {
            note_orphan_process_state(pid as i32, OrphanState::Ended);
        }
        // Best effort: a renderer still closing may hold a file briefly.
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}
