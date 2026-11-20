//! Mechanical verification of a rewrite: every path, symbol, and command it cites (each in
//! `backticks`, as the helper is told to write them) is checked against the repository, without
//! a model and without running anything.
//!
//! - A path (it has a `/`, or a file extension) must exist under the read root, or, as a bare
//!   file name, somewhere in the repository.
//! - A command (more than one word) must start with a program on `PATH` or a file in the
//!   repository; nothing is executed.
//! - A symbol (an identifier, `a::b`, `a.b()`) must occur as a whole word in a repository file.
//!
//! Anything else in backticks (a literal value, a glob, prose) is not a reference and is not
//! checked.

use std::path::Path;

use serde_json::json;

/// One reference a rewrite cites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reference {
    Path(String),
    Command(String),
    Symbol(String),
}

impl Reference {
    /// The reference as cited.
    pub(crate) fn text(&self) -> &str {
        match self {
            Self::Path(text) | Self::Command(text) | Self::Symbol(text) => text,
        }
    }
}

/// The backticked spans of `text`, classified, first citation of each only.
pub(crate) fn cited_references(text: &str) -> Vec<Reference> {
    let mut references: Vec<Reference> = Vec::new();
    for (index, span) in text.split('`').enumerate() {
        // Odd pieces sit between a pair of backticks.
        if index % 2 == 0 {
            continue;
        }
        if let Some(reference) = classify(span.trim()) {
            if !references.contains(&reference) {
                references.push(reference);
            }
        }
    }
    references
}

fn classify(span: &str) -> Option<Reference> {
    if span.is_empty() || span.contains(['*', '?', '{', '<', '"', '\'', '=']) {
        return None;
    }
    if span.split_whitespace().count() > 1 {
        return Some(Reference::Command(span.to_string()));
    }
    // `src/parser.rs:42` cites a line of the file.
    let path = span.split(':').next().unwrap_or(span);
    let has_extension = Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            !extension.is_empty() && extension.chars().all(|c| c.is_ascii_alphanumeric())
        });
    let looks_like_call = path.ends_with("()");
    if (path.contains('/') || has_extension) && !looks_like_call && !span.contains("::") {
        return Some(Reference::Path(path.to_string()));
    }
    let symbol = span.trim_end_matches("()");
    let last = symbol.rsplit(['.', ':']).next().unwrap_or(symbol);
    let is_identifier = last.len() > 1
        && last.chars().all(|c| c.is_alphanumeric() || c == '_')
        && !last.chars().all(|c| c.is_ascii_digit());
    // A plain lowercase word with no `_` (`true`, `cargo`) is prose, not a name to find.
    let named = symbol.contains(['.', ':', '_']) || last.chars().any(char::is_uppercase);
    (is_identifier && named).then(|| Reference::Symbol(symbol.to_string()))
}

/// Whether `reference` resolves from `root` (blocking: call off the async threads).
pub(crate) fn resolves(reference: &Reference, root: &Path) -> bool {
    match reference {
        Reference::Path(path) => {
            let path = path.trim_start_matches("./");
            root.join(path).exists() || (!path.contains('/') && file_named(path, root))
        }
        Reference::Command(command) => {
            let program = command.split_whitespace().next().unwrap_or_default();
            program_on_path(program) || root.join(program).is_file()
        }
        Reference::Symbol(symbol) => {
            let last = symbol.rsplit(['.', ':']).next().unwrap_or(symbol);
            word_in_repository(last, root)
        }
    }
}

fn file_named(name: &str, root: &Path) -> bool {
    let payload = json!({
        "pattern": format!("**/{name}"),
        "root": root,
        "cwd": root,
        "maxResults": 1,
    });
    crate::fs_search::parse_glob(&payload)
        .and_then(|request| crate::fs_search::glob(&request))
        .is_ok_and(|listing| !listing.entries.is_empty())
}

fn word_in_repository(word: &str, root: &Path) -> bool {
    let payload = json!({
        "pattern": word,
        "root": root,
        "cwd": root,
        "literal": true,
        "word": true,
        "maxResults": 1,
    });
    crate::fs_search::parse_grep(&payload)
        .and_then(|request| crate::fs_search::grep(&request))
        .is_ok_and(|report| !report.matches.is_empty())
}

fn program_on_path(program: &str) -> bool {
    if program.contains(['/', '\\']) {
        return false;
    }
    let Some(paths) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&paths).any(|dir| {
        let candidate = dir.join(program);
        candidate.is_file()
            || (cfg!(windows)
                && ["exe", "cmd", "bat"]
                    .iter()
                    .any(|extension| candidate.with_extension(extension).is_file()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backticked_spans_classify_into_paths_commands_and_symbols() {
        assert_eq!(
            cited_references(
                "Fix `parse_expr` in `src/parser.rs:42` (see `Parser::new` and `parser.rs`); run \
                 `cargo test -p parser`, not `true`, `42`, `*.rs`, or `parse_expr` again."
            ),
            vec![
                Reference::Symbol("parse_expr".to_string()),
                Reference::Path("src/parser.rs".to_string()),
                Reference::Symbol("Parser::new".to_string()),
                Reference::Path("parser.rs".to_string()),
                Reference::Command("cargo test -p parser".to_string()),
            ]
        );
    }

    #[test]
    fn references_resolve_only_when_the_repository_has_them() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/parser.rs"), "pub fn parse_expr() {}\n").unwrap();
        let checked: Vec<(String, bool)> = [
            Reference::Path("src/parser.rs".to_string()),
            Reference::Path("parser.rs".to_string()),
            Reference::Path("src/lexer.rs".to_string()),
            Reference::Symbol("parse_expr".to_string()),
            Reference::Symbol("Parser::parse_statement".to_string()),
            Reference::Command("sh -c true".to_string()),
            Reference::Command("no-such-program-xyz --flag".to_string()),
        ]
        .iter()
        .map(|reference| {
            (
                reference.text().to_string(),
                resolves(reference, dir.path()),
            )
        })
        .collect();
        assert_eq!(
            checked,
            [
                ("src/parser.rs", true),
                ("parser.rs", true),
                ("src/lexer.rs", false),
                ("parse_expr", true),
                ("Parser::parse_statement", false),
                ("sh -c true", cfg!(unix)),
                ("no-such-program-xyz --flag", false),
            ]
            .map(|(text, resolved)| (text.to_string(), resolved))
        );
    }
}
