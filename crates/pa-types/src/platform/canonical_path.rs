//! Canonical paths in the form other programs accept.
//!
//! On Windows `std::fs::canonicalize` answers with a verbatim path
//! (`\\?\C:\Users\…`, `\\?\UNC\server\share\…`). Rust's own file APIs take it,
//! but git, most other programs and every person reading it do not: git
//! fails `worktree add` on one, and a path shown to the model or the user
//! carries the prefix. Every canonical path in the workspace comes from
//! [`canonicalize`], which drops the prefix whenever the plain spelling names
//! the same file; clippy's `disallowed-methods` (`clippy.toml`) keeps the
//! standard library's form out.

use std::io;
use std::path::{Path, PathBuf};

/// The longest path the plain (non-verbatim) Windows form reaches without the
/// long-path opt-in (`MAX_PATH` less its terminator).
const PLAIN_PATH_LIMIT: usize = 259;

/// Device names Windows reserves in every directory, with any extension.
const RESERVED_NAMES: [&str; 22] = [
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// `std::fs::canonicalize`, answering a verbatim Windows path in its plain
/// spelling when that spelling names the same file. Elsewhere the result is
/// the standard library's unchanged.
///
/// # Errors
///
/// The standard library's: the path does not exist or a component cannot be
/// resolved.
pub fn canonicalize(path: impl AsRef<Path>) -> io::Result<PathBuf> {
    // The one sanctioned call: every other canonicalization goes through here.
    #[allow(clippy::disallowed_methods)]
    let canonical = std::fs::canonicalize(path)?;
    Ok(match canonical.to_str().and_then(plain_spelling) {
        Some(plain) => PathBuf::from(plain),
        None => canonical,
    })
}

/// The plain spelling of a verbatim drive (`\\?\C:\…`) or UNC
/// (`\\?\UNC\server\share\…`) path, or `None` when `text` is not one or only
/// the verbatim form reaches it: a component Windows would reinterpret (a
/// reserved device name, a trailing dot or space, `.` or `..`), a forward
/// slash (a verbatim path's literal character), or a length past `MAX_PATH`.
fn plain_spelling(text: &str) -> Option<String> {
    let (plain, rest) = if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        (format!(r"\\{rest}"), rest)
    } else {
        let rest = text.strip_prefix(r"\\?\")?;
        let bytes = rest.as_bytes();
        let drive = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        if !drive || bytes.get(2).is_some_and(|byte| *byte != b'\\') {
            return None;
        }
        (rest.to_owned(), &rest[2..])
    };
    if plain.len() > PLAIN_PATH_LIMIT || rest.contains('/') {
        return None;
    }
    let reinterpreted = rest
        .split('\\')
        .filter(|part| !part.is_empty())
        .any(|part| {
            let stem = part.split('.').next().unwrap_or(part);
            part == "."
                || part == ".."
                || part.ends_with('.')
                || part.ends_with(' ')
                || RESERVED_NAMES
                    .iter()
                    .any(|name| name.eq_ignore_ascii_case(stem.trim_end()))
        });
    (!reinterpreted).then_some(plain)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbatim_paths_take_their_plain_spelling_only_when_it_names_the_same_file() {
        let long = format!(r"\\?\C:\{}", "a".repeat(PLAIN_PATH_LIMIT));
        let cases = [
            r"\\?\C:\Users\me\repo",
            r"\\?\C:\",
            r"\\?\C:",
            r"\\?\UNC\server\share\dir",
            r"\\?\C:\dir\nul.txt",
            r"\\?\C:\dir\trailing.",
            r"\\?\C:\dir\space ",
            r"\\?\C:\dir\a/b",
            r"\\?\GLOBALROOT\Device\x",
            r"\\?\Volume{0}\x",
            &long,
            r"C:\already\plain",
            "/usr/lib",
        ];
        let spelled: Vec<Option<String>> = cases.iter().map(|case| plain_spelling(case)).collect();
        assert_eq!(
            spelled,
            [
                Some(r"C:\Users\me\repo".to_owned()),
                Some(r"C:\".to_owned()),
                Some("C:".to_owned()),
                Some(r"\\server\share\dir".to_owned()),
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            ]
        );
    }

    #[test]
    fn a_canonical_directory_has_no_verbatim_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let canonical = canonicalize(dir.path()).unwrap();
        assert!(
            !canonical.to_string_lossy().starts_with(r"\\?\") && canonical.is_absolute(),
            "{}",
            canonical.display()
        );
    }
}
