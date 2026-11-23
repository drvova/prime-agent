//! The Scheme interpreter's home is named at process start, never created
//! there: a distribution build decides whether this is its first run by
//! whether its agent directory exists yet.

use std::process::Command;

/// `--version` on a fresh home leaves the home empty. A directory made under
/// the agent directory before the first-run import would make that import
/// skip itself.
#[test]
fn naming_the_scheme_home_creates_nothing_in_a_fresh_home() {
    let home = tempfile::tempdir().expect("home");
    let output = Command::new(env!("CARGO_BIN_EXE_prime-agent"))
        .arg("--version")
        .env_remove("PRIME_AGENT_CODING_AGENT_DIR")
        .env_remove("STEEL_HOME")
        .env_remove("XDG_DATA_HOME")
        .env("HOME", home.path())
        .env("USERPROFILE", home.path())
        .output()
        .expect("run prime-agent");
    let left: Vec<_> = walk(home.path());
    assert_eq!(
        (output.status.success(), left),
        (true, Vec::<std::path::PathBuf>::new()),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Every path under `root`, relative to it.
fn walk(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("entry").path();
            found.push(path.strip_prefix(root).expect("under root").to_path_buf());
            if path.is_dir() {
                pending.push(path);
            }
        }
    }
    found.sort();
    found
}
