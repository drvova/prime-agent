//! Frida in the kernel venv: the pinned `frida` and `frida-tools` wheels the REPL's `frida`
//! module drives, installed after the venv is ready rather than inside its build.
//!
//! They are default packages of the kernel venv, installed by uv like the others, but they are
//! large (about 55 MB to download, 166 MB on disk on 64-bit x86 Linux), and Frida publishes
//! wheels for only some platforms. Inside the venv build every first run and every runtime
//! upgrade would wait on the download, and a platform without a wheel would fail the whole
//! build. So:
//!
//! - Kernel starts never wait on it ([`KernelFrida::InstallInBackground`]): a detached process
//!   runs the install and records its progress in the venv's `.frida-install`, which the REPL's
//!   `frida` module reads to answer "still installing" or why frida is unavailable. The child
//!   runs in its own session (unix) or detached process group (Windows), so it outlives a
//!   short-lived host, and it records the outcome itself.
//! - The install-time bootstrap (`--prime-agent-bootstrap`) installs before it returns
//!   ([`KernelFrida::InstallBeforeReturn`]): the user is already waiting on the installer.
//! - A platform without a wheel records `unavailable` and spawns nothing; the runtime-ready probe
//!   never imports frida, so the REPL stays ready there.
//! - An install that already landed these pins spawns nothing; a failed one is retried by the
//!   next kernel start. A fresh lock beside the record (a sibling process installing) means
//!   nothing to do.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;

/// The pinned packages (the REPL's `rlm.frida` and the runtime's dev group hold the same pins).
const FRIDA_PACKAGES: [&str; 2] = ["frida==17.27.2", "frida-tools==14.13.2"];

/// The install record in the venv, read by the REPL's `rlm.frida`.
const FRIDA_STATE_FILE: &str = ".frida-install";

/// How the frida install relates to [`super::super::ensure_kernel_python`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum KernelFrida {
    /// Every kernel start, prewarm and session: install on a detached process nothing awaits.
    #[default]
    InstallInBackground,
    /// The install-time bootstrap: install before returning.
    InstallBeforeReturn,
}

/// argv: record path, uv, the packages as JSON. Low priority first (uv inherits it), as the
/// bytecode compile runs; records `installing`, runs uv (binary wheels only: frida's sdist would
/// fetch and build a devkit), then `installed` or `failed` with uv's last error text.
const INSTALL_SCRIPT: &str = r#"import json, os, subprocess, sys, time
record, uv, packages = sys.argv[1], sys.argv[2], json.loads(sys.argv[3])
if hasattr(os, "nice"):
    os.nice(19)
elif sys.platform == "win32":
    import ctypes
    _k32 = ctypes.windll.kernel32
    _k32.SetPriorityClass(_k32.GetCurrentProcess(), 0x4000)
def write(state):
    with open(record + ".tmp", "w", encoding="utf-8") as handle:
        json.dump({**state, "packages": packages}, handle)
    os.replace(record + ".tmp", record)
lock = record + ".lock"
try:
    os.close(os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600))
except FileExistsError:
    if time.time() - os.stat(lock).st_mtime < 1800:
        sys.exit(0)
    os.unlink(lock)
    os.close(os.open(lock, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600))
try:
    write({"state": "installing", "started": time.time()})
    done = subprocess.run(
        [uv, "--no-config", "pip", "install", "--python", sys.executable, "--only-binary", "frida",
         "--compile-bytecode", *packages],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
        text=True, errors="replace",
    )
    if done.returncode == 0:
        write({"state": "installed"})
    else:
        lines = [line.strip() for line in done.stderr.splitlines() if line.strip()]
        detail = (": " + " ".join(lines[-3:])[-400:]) if lines else ""
        write({"state": "failed", "reason": f"uv exited {done.returncode}{detail}"})
finally:
    try:
        os.unlink(lock)
    except OSError:
        pass
"#;

/// Venvs this process already launched a background install for: every kernel start re-enters
/// the bootstrap, and one install per venv is enough.
static LAUNCHED: Mutex<Option<HashSet<PathBuf>>> = Mutex::new(None);

/// Why Frida publishes no wheel for this platform, or `None` when it does (the pinned release
/// ships manylinux `x86`/`x86_64`/`armv7`/`aarch64`, macOS `x86_64`/`arm64`, and Windows
/// `x86`/`x64`/`arm64`).
fn missing_wheel(os: &str, arch: &str, musl: bool) -> Option<String> {
    let published = match os {
        "linux" => !musl && matches!(arch, "x86" | "x86_64" | "arm" | "aarch64"),
        "macos" => matches!(arch, "x86_64" | "aarch64"),
        "windows" => matches!(arch, "x86" | "x86_64" | "aarch64"),
        _ => false,
    };
    let libc = if musl { "-musl" } else { "" };
    (!published).then(|| format!("no frida wheel for {os}{libc}-{arch}"))
}

fn write_record(record: &Path, value: &serde_json::Value) {
    if let Err(error) = std::fs::write(record, value.to_string()) {
        tracing::warn!(%error, path = %record.display(), "frida install record not written");
    }
}

/// Install the pinned frida packages into the ready venv unless its record already covers them.
/// `uv` finds the bootstrap's uv, only when an install runs (its lookup error is recorded as the
/// failure).
#[tracing::instrument(skip_all, fields(venv = %venv.display(), mode = ?mode))]
pub(crate) async fn install_frida(
    python: &Path,
    venv: &Path,
    mode: KernelFrida,
    uv: impl FnOnce() -> anyhow::Result<String>,
) {
    let record = venv.join(FRIDA_STATE_FILE);
    let recorded: Option<serde_json::Value> = std::fs::read_to_string(&record)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    if let Some(reason) = missing_wheel(
        std::env::consts::OS,
        std::env::consts::ARCH,
        cfg!(target_env = "musl"),
    ) {
        if recorded.is_none() {
            write_record(
                &record,
                &serde_json::json!({"state": "unavailable", "reason": reason}),
            );
        }
        return;
    }
    if recorded.is_some_and(|recorded| {
        recorded["state"] == "installed"
            && recorded["packages"] == serde_json::json!(FRIDA_PACKAGES)
    }) {
        return;
    }
    let uv = match uv() {
        Ok(uv) => uv,
        Err(error) => {
            write_record(
                &record,
                &serde_json::json!({
                    "state": "failed",
                    "reason": format!("{error:#}"),
                    "packages": FRIDA_PACKAGES,
                }),
            );
            return;
        }
    };
    let mut command = std::process::Command::new(python);
    command
        .args(["-P", "-s", "-c", INSTALL_SCRIPT])
        .arg(&record)
        .arg(uv)
        .arg(serde_json::json!(FRIDA_PACKAGES).to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let mut env = crate::kernel::python_env::inherited_python_env();
    crate::kernel::python_env::sanitize_uv_path(&mut env, None);
    command.env_clear().envs(env);
    if cfg!(windows) {
        command.env("PYTHONUTF8", "1");
    }
    match mode {
        KernelFrida::InstallInBackground => {
            let first = LAUNCHED
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get_or_insert_with(HashSet::new)
                .insert(venv.to_path_buf());
            if !first {
                return;
            }
            #[cfg(unix)]
            crate::platform::process::set_new_session(&mut command);
            #[cfg(not(unix))]
            crate::platform::process::set_new_process_group(&mut command);
            // A plain thread, not the async runtime's blocking pool: a runtime shutdown must
            // never wait for the download.
            let spawned = std::thread::Builder::new()
                .name("kernel-frida-install".to_string())
                .spawn(move || run_install(&mut command));
            if let Err(error) = spawned {
                tracing::warn!(%error, "kernel frida install thread did not start");
            }
        }
        KernelFrida::InstallBeforeReturn => {
            crate::platform::process::set_no_window(&mut command);
            let joined = tokio::task::spawn_blocking(move || run_install(&mut command)).await;
            if let Err(error) = joined {
                tracing::warn!(%error, "kernel frida install task failed");
            }
        }
    }
}

/// Spawn the install and reap it; the script records its own outcome.
fn run_install(command: &mut std::process::Command) {
    match command.status() {
        Ok(status) if status.success() => {}
        Ok(status) => tracing::warn!(code = ?status.code(), "kernel frida install script failed"),
        Err(error) => tracing::warn!(%error, "kernel frida install did not start"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_platforms_with_a_published_wheel_install() {
        assert_eq!(
            [
                ("linux", "x86_64", false),
                ("linux", "aarch64", false),
                ("linux", "arm", false),
                ("linux", "x86_64", true),
                ("linux", "riscv64", false),
                ("macos", "aarch64", false),
                ("windows", "aarch64", false),
                ("freebsd", "x86_64", false),
            ]
            .map(|(os, arch, musl)| missing_wheel(os, arch, musl)),
            [
                None,
                None,
                None,
                Some("no frida wheel for linux-musl-x86_64".to_string()),
                Some("no frida wheel for linux-riscv64".to_string()),
                None,
                None,
                Some("no frida wheel for freebsd-x86_64".to_string()),
            ]
        );
    }

    /// The pins here, in the REPL's `rlm.frida`, and in the runtime's dev group stay one set.
    #[test]
    fn the_pins_match_the_runtime() {
        let runtime = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prime-agent-runtime");
        let module = std::fs::read_to_string(runtime.join("src/rlm/frida.py")).unwrap();
        let constant = |name: &str| {
            module
                .lines()
                .find_map(|line| line.strip_prefix(&format!("{name} = \"")))
                .and_then(|rest| rest.strip_suffix('"'))
                .map(str::to_string)
        };
        let pyproject = std::fs::read_to_string(runtime.join("pyproject.toml")).unwrap();
        let pyproject: toml::Value = toml::from_str(&pyproject).unwrap();
        let dev_pins: Vec<&str> = pyproject["dependency-groups"]["dev"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(toml::Value::as_str)
            .filter(|spec| spec.starts_with("frida"))
            .collect();
        assert_eq!(
            (
                vec![
                    format!("frida=={}", constant("FRIDA_VERSION").unwrap_or_default()),
                    format!(
                        "frida-tools=={}",
                        constant("FRIDA_TOOLS_VERSION").unwrap_or_default()
                    ),
                ],
                dev_pins
            ),
            (
                FRIDA_PACKAGES.map(str::to_string).to_vec(),
                FRIDA_PACKAGES.to_vec()
            )
        );
    }

    #[cfg(unix)]
    mod install {
        use super::*;
        use std::os::unix::fs::PermissionsExt;

        fn python() -> Option<&'static str> {
            ["python3.11", "python3"].into_iter().find(|python| {
                std::process::Command::new(python)
                    .args(["-P", "-c", "pass"])
                    .status()
                    .is_ok_and(|status| status.success())
            })
        }

        /// A venv dir and a fake uv that records its argv (minus the interpreter path) and runs
        /// `body`.
        fn fake(body: &str) -> (tempfile::TempDir, PathBuf) {
            let dir = tempfile::tempdir().unwrap();
            let uv = dir.path().join("uv");
            std::fs::write(
                &uv,
                format!(
                    "#!/bin/sh\nfor a in \"$@\"; do echo \"$a\"; done > {:?}\n{body}\n",
                    dir.path().join("argv")
                ),
            )
            .unwrap();
            std::fs::set_permissions(&uv, std::fs::Permissions::from_mode(0o755)).unwrap();
            (dir, uv)
        }

        fn record(dir: &Path) -> serde_json::Value {
            let mut value: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(dir.join(FRIDA_STATE_FILE)).unwrap())
                    .unwrap();
            value.as_object_mut().unwrap().remove("started");
            value
        }

        fn argv(dir: &Path) -> Vec<String> {
            std::fs::read_to_string(dir.join("argv"))
                .map(|text| text.lines().map(str::to_string).collect())
                .unwrap_or_default()
        }

        #[tokio::test]
        async fn the_install_records_its_outcome_and_a_landed_install_is_not_repeated() {
            let (Some(python), None) = (
                python(),
                missing_wheel(
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    cfg!(target_env = "musl"),
                ),
            ) else {
                eprintln!("no python, or no frida wheel for this host; skipping");
                return;
            };
            let (dir, uv) = fake("exit 0");
            let run = || {
                install_frida(
                    Path::new(python),
                    dir.path(),
                    KernelFrida::InstallBeforeReturn,
                    || Ok(uv.to_string_lossy().into_owned()),
                )
            };
            run().await;
            let first = (record(dir.path()), argv(dir.path()));
            std::fs::remove_file(dir.path().join("argv")).unwrap();
            run().await;
            let interpreter = first.1.get(4).cloned().unwrap_or_default();
            assert_eq!(
                (first, argv(dir.path())),
                (
                    (
                        serde_json::json!({"state": "installed", "packages": FRIDA_PACKAGES}),
                        [
                            "--no-config",
                            "pip",
                            "install",
                            "--python",
                            interpreter.as_str(),
                            "--only-binary",
                            "frida",
                            "--compile-bytecode",
                            FRIDA_PACKAGES[0],
                            FRIDA_PACKAGES[1],
                        ]
                        .map(str::to_string)
                        .to_vec()
                    ),
                    Vec::<String>::new()
                )
            );
        }

        #[tokio::test]
        async fn a_failed_install_records_uvs_error_and_a_missing_uv_records_why() {
            let (Some(python), None) = (
                python(),
                missing_wheel(
                    std::env::consts::OS,
                    std::env::consts::ARCH,
                    cfg!(target_env = "musl"),
                ),
            ) else {
                eprintln!("no python, or no frida wheel for this host; skipping");
                return;
            };
            let (failing, uv) = fake("echo 'error: network unreachable' >&2\nexit 2");
            install_frida(
                Path::new(python),
                failing.path(),
                KernelFrida::InstallBeforeReturn,
                || Ok(uv.to_string_lossy().into_owned()),
            )
            .await;
            let no_uv = tempfile::tempdir().unwrap();
            install_frida(
                Path::new(python),
                no_uv.path(),
                KernelFrida::InstallBeforeReturn,
                || Err(anyhow::anyhow!("uv is required")),
            )
            .await;
            assert_eq!(
                (record(failing.path()), record(no_uv.path())),
                (
                    serde_json::json!({
                        "state": "failed",
                        "reason": "uv exited 2: error: network unreachable",
                        "packages": FRIDA_PACKAGES,
                    }),
                    serde_json::json!({
                        "state": "failed",
                        "reason": "uv is required",
                        "packages": FRIDA_PACKAGES,
                    }),
                )
            );
        }
    }
}
