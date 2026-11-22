//! Resolve config values: `!command` (successful results are cached),
//! env var, or literal.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Result;

static COMMAND_RESULT_CACHE: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);

/// Resolve a config value: `!command` executes and caches successful results;
/// otherwise the environment wins over the literal string (set-but-empty means missing).
pub fn resolve_config_value(config: &str) -> Option<String> {
    if let Some(command) = config.strip_prefix('!') {
        return execute_command(config, command);
    }
    resolve_env_or_literal(config)
}

/// Never-cached variant used when a command key was marked stale.
pub fn resolve_config_value_uncached(config: &str) -> Option<String> {
    if let Some(command) = config.strip_prefix('!') {
        return run_command(command).ok().flatten();
    }
    resolve_env_or_literal(config)
}

/// Unset env var falls back to the literal string; set-but-empty is a missing
/// credential (never the variable name).
fn resolve_env_or_literal(config: &str) -> Option<String> {
    match pa_types::session_scope::var(config) {
        Ok(value) if !value.is_empty() => Some(value),
        Ok(_) => None,
        Err(_) => Some(config.to_string()),
    }
}

fn execute_command(cache_key: &str, command: &str) -> Option<String> {
    // A command runs in its session's environment and directory, so its
    // result is that session's: sessions hosted in one process never share one.
    let cache_key = &match pa_types::session_scope::current() {
        Some(scope) => format!("{}\u{0}{cache_key}", scope.id()),
        None => cache_key.to_string(),
    };
    let cached = || {
        COMMAND_RESULT_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .and_then(|cache| cache.get(cache_key).cloned())
    };
    if let Some(cached) = cached() {
        return Some(cached);
    }
    // Run outside the lock: the cache is the whole process's, and a command
    // that hangs must not hold up every other session's lookups. Two callers
    // racing on one key both run it; the first result stays.
    let value = run_command(command).ok().flatten()?;
    // A command that produced no value is not a resolution: a transient failure
    // must be retried instead of pinning the failure for the process lifetime (TS #2497).
    Some(
        COMMAND_RESULT_CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert_with(HashMap::new)
            .entry(cache_key.to_string())
            .or_insert(value)
            .clone(),
    )
}

fn run_command(command: &str) -> Result<Option<String>> {
    let output = hidden_spawn(command)?;
    let Some(output) = output else {
        return Ok(None);
    };
    if !output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Ok((!value.is_empty()).then_some(value))
}

/// Hidden command execution for `!command` config values: Unix runs the default
/// shell; Windows tries the configured shell first and falls back to `ComSpec`.
#[cfg(unix)]
fn hidden_spawn(command: &str) -> Result<Option<std::process::Output>> {
    use std::process::Stdio;
    let output = pa_types::session_scope::std_command("bash")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    Ok(Some(output))
}

#[cfg(windows)]
fn hidden_spawn(command: &str) -> Result<Option<std::process::Output>> {
    use std::process::Stdio;
    if let Ok(config) = crate::platform::get_shell_config(None) {
        match pa_types::session_scope::std_command(&config.shell)
            .args(&config.args)
            .arg(command)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
        {
            Ok(output) => return Ok(Some(output)),
            // ENOENT: the configured shell is missing; other spawn
            // errors are `executed` with no value.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Ok(None),
        }
    }
    let comspec = pa_types::session_scope::var("ComSpec").unwrap_or_else(|_| "cmd.exe".to_string());
    let output = pa_types::session_scope::std_command(comspec)
        .args(["/d", "/s", "/c"])
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()?;
    Ok(Some(output))
}

#[cfg(not(any(unix, windows)))]
fn hidden_spawn(_command: &str) -> Result<Option<std::process::Output>> {
    anyhow::bail!("config value command execution is not implemented on this platform")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_or_literal_semantics() {
        let key = "PA_TEST_DEFINITELY_UNSET_VAR";
        assert_eq!(resolve_env_or_literal(key), Some(key.to_string()));
    }

    #[test]
    fn command_resolution_and_cache() {
        let value = resolve_config_value("!echo resolved-value");
        assert_eq!(value.as_deref(), Some("resolved-value"));
        assert_eq!(
            resolve_config_value("!echo resolved-value").as_deref(),
            Some("resolved-value")
        );

        // A failing command is retried on every lookup (TS #2497: a transient failure must
        // not disable the credential for the process lifetime).
        let counter = std::env::temp_dir().join(format!("pa-resolve-retry-{}", std::process::id()));
        let _ = std::fs::remove_file(&counter);
        let command = format!("echo x >> {} ; exit 1", counter.display());
        for _ in 0..3 {
            assert_eq!(resolve_config_value(&format!("!{command}")), None);
        }
        let runs = std::fs::read_to_string(&counter).map_or(0, |text| text.lines().count());
        let _ = std::fs::remove_file(&counter);
        assert_eq!(runs, 3, "failed commands are re-run on each lookup");
    }

    /// The command cache belongs to the whole process. A command that hangs
    /// holds up no other lookup (the cache's lock is not held while a
    /// command runs), and a session hosted beside others gets the result of
    /// the command run in its own environment, never another session's.
    #[cfg(unix)]
    #[test]
    fn a_hanging_command_holds_up_no_other_lookup_and_results_stay_with_their_session() {
        let dir = tempfile::tempdir().unwrap();
        let gate = dir.path().join("gate");
        nix::unistd::mkfifo(&gate, nix::sys::stat::Mode::S_IRWXU).unwrap();
        let hanging = format!("!cat {}", gate.display());
        let blocked = std::thread::spawn(move || resolve_config_value(&hanging));
        // The command is running once its `cat` holds the pipe's read end:
        // only then does opening the write end without blocking succeed.
        let mut release = loop {
            use std::os::unix::fs::OpenOptionsExt as _;
            match std::fs::OpenOptions::new()
                .write(true)
                .custom_flags(nix::libc::O_NONBLOCK)
                .open(&gate)
            {
                Ok(pipe) => break pipe,
                Err(_) => std::thread::yield_now(),
            }
        };
        let in_session = |name: &'static str| {
            let scope = pa_types::session_scope::SessionScope::new(
                name.to_string(),
                std::env::temp_dir(),
                std::collections::BTreeMap::from([(
                    "PA_COMMAND_CACHE_TEST".to_string(),
                    format!("of-{name}"),
                )]),
                std::collections::BTreeSet::new(),
            );
            std::thread::spawn(move || {
                let _scope = pa_types::session_scope::enter(Some(scope));
                resolve_config_value("!echo $PA_COMMAND_CACHE_TEST")
            })
            .join()
            .unwrap()
        };
        let while_blocked = (in_session("first"), in_session("second"));
        std::io::Write::write_all(&mut release, b"released\n").unwrap();
        drop(release);
        assert_eq!(
            (while_blocked, blocked.join().unwrap()),
            (
                (Some("of-first".to_string()), Some("of-second".to_string())),
                Some("released".to_string())
            )
        );
    }
}
