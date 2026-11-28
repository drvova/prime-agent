//! Transcript store guard: session transcripts are read and written only
//! through `pa_core::session::store` (the `TranscriptStore` boundary), so a
//! second store implementation sees every transcript operation.
//!
//! The scan covers the production sources of pa-core, pa-daemon and pa-cli
//! (test files and inline test modules are skipped; tests build fixtures with
//! raw files on purpose) and fails on two kinds of raw file I/O outside the
//! store module:
//!
//! - any raw I/O in a module whose subject is transcripts (the session
//!   modules and the daemon's session store), except the lines allowed below
//!   and the session leases' own ownership records;
//! - raw I/O anywhere whose argument names a transcript (a `session_file`,
//!   `session_path`, `transcript`, `.jsonl` path, ...).
//!
//! An allowed line needs a reason. What stays outside on purpose: pa-tui's
//! replay tool (no pa-core dependency), pa-perf's corpus builder (it makes
//! fixtures), and pa-core's legacy-install history import (it copies another
//! install's agent tree, chats included, before any store opens).

use std::path::{Path, PathBuf};

/// Raw file I/O spellings. A transcript handle's own `Read`/`Seek` calls are
/// not raw I/O: they go through the store.
const RAW_IO: &[&str] = &[
    "File::open",
    "File::create",
    "File::options",
    "OpenOptions",
    "fs::read_to_string",
    "fs::read(",
    "fs::write",
    "fs::copy",
    "fs::rename",
    "fs::remove_file",
    "fs::read_dir",
    "fs::metadata",
    "fs::symlink_metadata",
    "fs::hard_link",
    "replace_file",
    "rename_onto",
    "AtomicFile",
    "tokio::fs::",
    // A transcript need not be a file (the SQLite store keeps it in its
    // database): whether one is there is the store's `exists`/`stat`.
    ".exists()",
    ".is_file()",
    ".try_exists()",
];

/// Words that name a transcript in an I/O call's argument.
const TRANSCRIPT_WORDS: &[&str] = &[
    "session_file",
    "session_path",
    "sessionFile",
    "transcript",
    "jsonl",
    "child_file",
    "parent_file",
    "store.path",
];

/// Modules whose subject is transcripts: every raw I/O line in them is a
/// violation unless allowed.
const TRANSCRIPT_MODULES: &[&str] = &[
    "pa-core/src/session/",
    "pa-core/src/session_engine/session_search.rs",
    "pa-daemon/src/session_store.rs",
    "pa-daemon/src/session_store/",
    "pa-daemon/src/session_scan.rs",
    "pa-daemon/src/session_catalog.rs",
    "pa-daemon/src/saved_session_commands.rs",
    "pa-daemon/src/session_navigation.rs",
    "pa-daemon/src/session_tree.rs",
    "pa-daemon/src/branch_navigation.rs",
    "pa-daemon/src/stop_cleanup.rs",
    "pa-daemon/src/revived_turn.rs",
    "pa-daemon/src/session_usage.rs",
    "pa-daemon/src/hold_refusal.rs",
    "pa-daemon/src/goal_state_persist.rs",
    "pa-daemon/src/context_tree_children.rs",
    "pa-daemon/src/usage_report/history.rs",
    "pa-daemon/src/rlm_children/lifecycle.rs",
];

/// The store itself.
const STORE_MODULE: &str = "pa-core/src/session/store/";

/// Session modules whose own files are not transcripts: the session leases'
/// ownership records under `<agent dir>/session-leases`, and the workspace
/// checkpoints' shadow repository, indexes and record logs under
/// `<agent dir>/checkpoints`. A line of theirs that names a transcript is
/// still checked.
const OWNERSHIP_RECORD_MODULES: &[&str] = &[
    "pa-core/src/session/lease.rs",
    "pa-core/src/session/checkpoints.rs",
];

/// Allowed raw I/O outside the store: (file, a substring of the line, reason).
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "pa-core/src/session/window_cache.rs",
        "std::fs::read(cache_path(path))",
        "the window snapshot sidecar (a file-store cache, certified against the store's generation)",
    ),
    (
        "pa-core/src/session/window_cache.rs",
        "pa_types::platform::AtomicFile::create(",
        "the window snapshot sidecar write",
    ),
    (
        "pa-core/src/session/catalog/info_sidecar.rs",
        "std::fs::read(sidecar_path(path))",
        "the session-info scan sidecar (a file-store cache, certified against the store's generation)",
    ),
    (
        "pa-core/src/session/catalog/info_sidecar.rs",
        "pa_types::platform::AtomicFile::create(",
        "the session-info scan sidecar write",
    ),
    (
        "pa-core/src/session/branch_export.rs",
        "if std::fs::metadata(file_path).is_ok_and(|metadata| !metadata.is_file()) {",
        "the `/export <file>.jsonl` output is the file the user named, not a stored transcript \
         (its callers read the branch through the store and hand it in)",
    ),
    (
        "pa-core/src/session/branch_export.rs",
        "std::fs::write(file_path, body)",
        "the `/export` output file the user named (a non-regular target such as a FIFO)",
    ),
    (
        "pa-core/src/session/branch_export.rs",
        "pa_types::platform::replace_file(",
        "the `/export` output file the user named, replaced whole",
    ),
    (
        "pa-daemon/src/context_tree_children.rs",
        "let Ok(entries) = std::fs::read_dir(dir) else {",
        "`child_session_dirs` lists the `sub-*` child-session directories by their own mtimes \
         (directories, not transcripts; their transcripts are listed through the store)",
    ),
];

fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates dir")
        .to_path_buf()
}

fn is_test_file(relative: &str) -> bool {
    let name = relative.rsplit('/').next().unwrap_or(relative);
    relative.split('/').any(|part| part == "tests")
        || name == "tests.rs"
        || name == "test.rs"
        || name.ends_with("_tests.rs")
        || name.ends_with("_test.rs")
        || name == "test_support.rs"
        || name == "test_host.rs"
}

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|error| panic!("{}: {error}", dir.display()));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

/// The production lines of a source file: everything before its inline
/// `#[cfg(test)] mod name {` block, without comment lines.
fn production_lines(source: &str) -> Vec<(usize, &str)> {
    let lines: Vec<&str> = source.lines().collect();
    let mut kept = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed == "#[cfg(test)]"
            && lines.get(index + 1).is_some_and(|next| {
                let next = next.trim();
                next.split_whitespace()
                    .find(|word| !word.starts_with("pub"))
                    == Some("mod")
                    && next.ends_with('{')
            })
        {
            break;
        }
        if trimmed.starts_with("//") {
            continue;
        }
        kept.push((index + 1, *line));
    }
    kept
}

/// Where the line's first raw I/O spelling starts: a token preceded by an
/// identifier character (`SessionFile::open`) is another name.
fn raw_io_in(line: &str) -> Option<usize> {
    RAW_IO
        .iter()
        .flat_map(|token| line.match_indices(token).map(|(at, _)| at))
        .filter(|&at| {
            !line[..at]
                .chars()
                .next_back()
                .is_some_and(|before| before.is_alphanumeric() || before == '_')
        })
        .min()
}

#[test]
fn transcripts_are_read_and_written_only_through_the_store() {
    let crates = crates_dir();
    let mut violations = Vec::new();
    let mut allowed_used = vec![false; ALLOWED.len()];
    for krate in ["pa-core", "pa-daemon", "pa-cli"] {
        let mut files = Vec::new();
        rust_files(&crates.join(krate).join("src"), &mut files);
        files.sort();
        for file in files {
            let relative = file
                .strip_prefix(&crates)
                .expect("under crates")
                .to_string_lossy()
                .replace('\\', "/");
            if relative.starts_with(STORE_MODULE) || is_test_file(&relative) {
                continue;
            }
            let source = std::fs::read_to_string(&file).expect("read source");
            let transcript_module = TRANSCRIPT_MODULES
                .iter()
                .any(|module| relative.starts_with(module))
                && !OWNERSHIP_RECORD_MODULES.contains(&relative.as_str());
            for (number, line) in production_lines(&source) {
                let Some(at) = raw_io_in(line) else {
                    continue;
                };
                let names_transcript = TRANSCRIPT_WORDS
                    .iter()
                    .any(|word| line[at..].contains(word));
                if !transcript_module && !names_transcript {
                    continue;
                }
                if let Some(index) = ALLOWED
                    .iter()
                    .position(|(path, text, _)| *path == relative && line.contains(text))
                {
                    allowed_used[index] = true;
                    continue;
                }
                violations.push(format!("{relative}:{number}: {}", line.trim()));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "raw transcript I/O outside `pa_core::session::store` (route it through \
         `store()`, or allow the line with a reason):\n{}",
        violations.join("\n")
    );
    let stale: Vec<_> = ALLOWED
        .iter()
        .zip(&allowed_used)
        .filter(|(_, used)| !**used)
        .map(|((path, text, _), _)| format!("{path}: {text}"))
        .collect();
    assert!(
        stale.is_empty(),
        "allowed lines that no longer exist (drop them):\n{}",
        stale.join("\n")
    );
}
