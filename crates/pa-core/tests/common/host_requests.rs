//! The host-request kinds pa-core registers, found by scanning its sources:
//! shared by the guards that hold a list of them to the registrations (the
//! prompt guards here; pa-runtime's trust classes include this file by path).

use std::collections::BTreeSet;
use std::path::Path;

/// pa-core's directory, wherever the including crate is.
const PA_CORE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../pa-core");

/// A host-request type literal: lowercase dotted name (`rlm.collect`).
fn is_request_type_literal(literal: &str) -> bool {
    !literal.is_empty()
        && literal
            .chars()
            .next()
            .is_some_and(|ch| ch.is_ascii_lowercase())
        && literal
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '.')
        && literal.contains('.')
}

/// Scan the pa-core sources for every host-request type the session engine
/// registers: `handlers.register("<type>", ...)` literals,
/// `for request_type in [ "<type>", ... ]` loop tables, and the
/// `pub const <NAME>_REQUEST_TYPE: &str = "<type>"` constants handlers register under.
pub fn registered_host_requests() -> BTreeSet<String> {
    let src_root = Path::new(PA_CORE_DIR).join("src");
    let mut found = BTreeSet::new();
    let mut stack = vec![src_root];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
                continue;
            }
            // A test module's registrations are test doubles (a handler that
            // never answers, a gate a test holds), not the product's surface.
            if path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("tests.rs"))
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read source");
            // Direct registrations: the request type is the FIRST
            // argument of each `.register(` — a literal right at the
            // call's head counts; anything after a non-literal first
            // argument is not the request type.
            for remainder in source.split(".register(").skip(1) {
                let rest = remainder.trim_start();
                if rest.starts_with('"') {
                    if let Some(literal) = first_string_literal(rest) {
                        if is_request_type_literal(&literal) {
                            found.insert(literal);
                        }
                    }
                }
            }
            // Request-type constants: `pub const <NAME>_REQUEST_TYPE: &str = "a.b"`.
            found.extend(
                source
                    .split("_REQUEST_TYPE: &str = ")
                    .skip(1)
                    .filter_map(first_string_literal)
                    .filter(|literal| is_request_type_literal(literal)),
            );
            // Loop tables: `for request_type in [ "a.b", ... ]`.
            for remainder in source.split("for request_type in").skip(1) {
                let Some(open) = remainder.find('[') else {
                    continue;
                };
                let Some(close) = remainder[open..].find(']') else {
                    continue;
                };
                for literal in all_string_literals(&remainder[open..open + close]) {
                    if is_request_type_literal(&literal) {
                        found.insert(literal);
                    }
                }
            }
        }
    }
    found
}

fn first_string_literal(text: &str) -> Option<String> {
    let open = text.find('"')?;
    let tail = &text[open + 1..];
    let close = tail.find('"')?;
    Some(tail[..close].to_string())
}

fn all_string_literals(text: &str) -> Vec<String> {
    let mut literals = Vec::new();
    let mut rest = text;
    while let Some(open) = rest.find('"') {
        let tail = &rest[open + 1..];
        match tail.find('"') {
            Some(close) => {
                literals.push(tail[..close].to_string());
                rest = &tail[close + 1..];
            }
            None => break,
        }
    }
    literals
}
