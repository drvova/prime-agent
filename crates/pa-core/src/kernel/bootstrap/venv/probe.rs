//! The runtime-ready probe concern: the two-layer memo (in-process map + the cross-process on-disk
//! verdict) that skips re-probing a venv nobody damaged.

use std::sync::LazyLock;

use super::{
    collect_python_files, Digest, HashMap, KernelPythonSkill, Mutex, Path, PathBuf, Stdio,
    TreeIdentityCache, DEFAULT_RLM_EXTRA_PACKAGES,
};
use crate::kernel::capabilities::capabilities;

fn python_imports(python: &str, module_name: &str) -> bool {
    run_quiet(python, &["-c", &format!("import {module_name}")])
}

fn run_quiet(command: &str, args: &[&str]) -> bool {
    let mut child = std::process::Command::new(command);
    child
        // Never let the repository cwd shadow the installed runtime during a
        // readiness probe.
        .arg("-P")
        .arg("-s")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    child
        .env_clear()
        .envs(crate::kernel::python_env::inherited_python_env());
    // Hidden window on Windows (TS `spawnHidden`).
    crate::platform::process::set_no_window(&mut child);
    if cfg!(windows) {
        child.env("PYTHONUTF8", "1");
    }
    matches!(child.status(), Ok(status) if status.success())
}

#[cfg(all(test, unix))]
mod safe_path_tests {
    use super::python_imports;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn readiness_probe_does_not_import_a_module_from_the_project_cwd() {
        let Some(python) = ["python3.11", "python3"].into_iter().find(|python| {
            std::process::Command::new(python)
                .args(["-P", "-c", "pass"])
                .status()
                .is_ok_and(|status| status.success())
        }) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("repository-module-ran");
        std::fs::write(
            dir.path().join("rlm.py"),
            format!("open({marker:?}, 'w').close()\n"),
        )
        .unwrap();
        let interpreter = dir.path().join("project-python");
        let project = dir.path();
        std::fs::write(
            &interpreter,
            format!("#!/bin/sh\ncd {project:?}\nexec {python:?} \"$@\"\n"),
        )
        .unwrap();
        std::fs::set_permissions(&interpreter, std::fs::Permissions::from_mode(0o755)).unwrap();

        let _ = python_imports(interpreter.to_str().unwrap(), "rlm");
        assert!(
            !marker.exists(),
            "repository module ran during readiness probe"
        );
    }
}

/// The runtime-ready assertion from the TS product: a current prime-agent-runtime with the
/// callable RLM surface, harness CRUD, bash handles, and protocol version 3. One `python -c`
/// program derived from the kernel capability manifests ([`crate::kernel::capabilities`]): the
/// `core` manifest's statements first (they import `inspect` and `rlm`, which every other manifest
/// may use), then every other manifest's by capability name, each in its file order, joined with
/// `; `. Built once, on the first kernel start that probes.
pub(crate) static RUNTIME_READY_CHECK: LazyLock<String> = LazyLock::new(|| {
    let mut manifests =
        capabilities().expect("the embedded capability manifests parse (their tests pin them)");
    let core = manifests
        .remove("core")
        .expect("the core capability manifest is embedded");
    std::iter::once(core)
        .chain(manifests.into_values())
        .flat_map(|manifest| manifest.probe)
        .collect::<Vec<_>>()
        .join("; ")
});

pub(crate) fn has_prime_agent_runtime(python: &str) -> bool {
    run_quiet(python, &["-c", RUNTIME_READY_CHECK.as_str()])
}

pub(crate) fn missing_rlm_extra_import_labels(python: &str) -> Vec<&'static str> {
    DEFAULT_RLM_EXTRA_PACKAGES
        .iter()
        .filter(|(_, import, _)| !python_imports(python, import))
        .map(|(_, _, label)| *label)
        .collect()
}

pub(crate) fn missing_python_skill_import_labels(
    python: &str,
    python_skills: &[KernelPythonSkill],
) -> Vec<String> {
    python_skills
        .iter()
        .filter(|skill| !python_imports(python, &skill.import_name))
        .map(|skill| format!("{} ({})", skill.name, skill.import_name))
        .collect()
}

/// Process-global memo of a successful runtime-ready probe, tiered above the cross-process on-disk
/// memo ([`super::super::disk_memo`]): the probe is a full interpreter start. Memoized on success
/// only: the key carries every input the probe observes; a failed kernel start drops both layers
/// ([`invalidate_runtime_probe_cache`]).
static RUNTIME_PROBE_MEMO: Mutex<Option<HashMap<String, PathBuf>>> = Mutex::new(None);

pub(super) fn lock_probe_memo() -> std::sync::MutexGuard<'static, Option<HashMap<String, PathBuf>>>
{
    RUNTIME_PROBE_MEMO
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Identity of the runtime as installed in the venv: the interpreter binary's stat plus a content
/// hash of the installed `rlm` and `dill` package trees. Out-of-band damage changes this identity,
/// so a memoized probe result can never mask a mutated install; an unchanged tree is served from
/// the venv's stat-keyed [`TreeIdentityCache`] without re-reading its bytes.
pub(super) fn installed_runtime_identity(python: &Path, venv: &Path) -> String {
    let mut hasher = sha2::Sha256::new();
    match std::fs::metadata(python) {
        Ok(meta) => {
            let modified = meta
                .modified()
                .map_or_else(|_| "no-mtime".to_string(), |time| format!("{time:?}"));
            hasher.update(format!("py:{}:{}:{modified}", python.display(), meta.len()).as_bytes());
        }
        Err(error) => hasher.update(format!("py-error:{}:{error}", python.display()).as_bytes()),
    }
    let cache = TreeIdentityCache::in_venv(venv);
    for package in ["rlm", "dill"] {
        let tree_hash = installed_package_dir(venv, package).map(|dir| {
            let mut files = Vec::new();
            collect_python_files(&dir, &mut files)?;
            files.sort();
            cache.content_hash(&dir, &files)
        });
        match tree_hash {
            Some(Ok(hash)) => hasher.update(format!("{package}:{hash}").as_bytes()),
            Some(Err(error)) => hasher.update(format!("{package}-error:{error}").as_bytes()),
            None => hasher.update(format!("{package}-missing").as_bytes()),
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

/// The installed `rlm` package under the venv's site-packages: the Windows layout
/// `<venv>/Lib/site-packages/rlm` (no python-version layer) or the Unix layout
/// `<venv>/lib/python*/site-packages/rlm`.
#[cfg(test)]
pub(super) fn installed_rlm_dir(venv: &Path) -> Option<PathBuf> {
    installed_package_dir(venv, "rlm")
}

pub(super) fn installed_package_dir(venv: &Path, package: &str) -> Option<PathBuf> {
    let lib = venv.join("lib");
    let windows_layout = lib.join("site-packages").join(package);
    if windows_layout.is_dir() {
        return Some(windows_layout);
    }
    let entries = std::fs::read_dir(&lib).ok()?;
    for entry in entries.flatten() {
        if !entry.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        if !entry.file_name().to_string_lossy().starts_with("python") {
            continue;
        }
        let installed = entry.path().join("site-packages").join(package);
        if installed.is_dir() {
            return Some(installed);
        }
    }
    None
}

/// The memo key: every input the runtime-ready probe observes.
pub(super) fn runtime_probe_key(
    python: &str,
    runtime_identity: &str,
    version_raw: &str,
    installed_identity: &str,
) -> String {
    format!(
        "{python}\u{0}{runtime_identity}\u{0}{installed_identity}\u{0}sha256:{:x}",
        sha2::Sha256::digest(version_raw.as_bytes())
    )
}

/// The runtime-ready check, memoized on success across two layers: the process-global map first,
/// then the on-disk cross-process memo. `version_raw` is the raw `.bootstrap-version` text the
/// caller already read. The key is recomputed fresh on every call so a hit skips only the two
/// interpreter probes. A caller-owned `PRIME_AGENT_KERNEL_PYTHON` override never reaches this.
pub(super) fn has_prime_agent_runtime_memoized(
    python: &str,
    runtime_identity: &str,
    version_raw: &str,
    installed_identity: &str,
    venv: &Path,
) -> bool {
    let key = runtime_probe_key(python, runtime_identity, version_raw, installed_identity);
    let memo_path = super::super::disk_memo::disk_memo_path(venv);
    if lock_probe_memo()
        .as_ref()
        .is_some_and(|memo| memo.contains_key(&key))
    {
        return true;
    }
    if super::super::disk_memo::disk_memo_hit(&memo_path, &key) {
        let mut memo = lock_probe_memo();
        let entries = memo.get_or_insert_with(HashMap::new);
        if entries.len() >= 16 {
            entries.clear();
        }
        entries.insert(key, memo_path);
        return true;
    }
    if !has_prime_agent_runtime(python) || !python_imports(python, "dill") {
        return false;
    }
    let mut memo = lock_probe_memo();
    let entries = memo.get_or_insert_with(HashMap::new);
    if entries.len() >= 16 {
        entries.clear();
    }
    super::super::disk_memo::disk_memo_write(&memo_path, &key);
    entries.insert(key, memo_path);
    true
}

/// Drop every memoized runtime-ready result, both layers: the in-process map dies with this call.
/// The next kernel start re-runs the probe.
pub fn invalidate_runtime_probe_cache() {
    let tracked: Vec<PathBuf> = lock_probe_memo()
        .take()
        .map(|memo| memo.values().cloned().collect())
        .unwrap_or_default();
    for path in tracked {
        super::super::disk_memo::disk_memo_invalidate(&path);
    }
}

/// Drop only the in-process memo layer, leaving the on-disk layer intact:
/// the fresh-process simulation the disk-memo oracles use. Unix only.
#[cfg(all(test, unix))]
pub(crate) fn clear_in_process_probe_memo_for_tests() {
    *lock_probe_memo() = None;
}
