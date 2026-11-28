//! The unmapped-command guard: every `DaemonCommand` the TUI's own code builds has a wire2 form.
//!
//! The commands are found in the crate's source, not listed by hand: a struct literal
//! `DaemonCommand::<Variant> { ... rest: ... }` outside this layer and outside tests is a command
//! the TUI sends (a pattern never sets `rest`). Each is built (from its type, with placeholder
//! members) and must come out of [`super::outgoing`] carried; the compiler already holds
//! `outgoing`'s match to every variant without a wildcard, so a new variant cannot fall through
//! unclassified, and this holds a new send to a carried form.

use std::collections::BTreeSet;
use std::path::Path;

use pa_types::daemon::DaemonCommand;
use serde_json::{json, Map, Value};

use super::{outgoing, Outgoing};

/// The commands the TUI builds that wire2 deliberately does not carry, each with its reason.
const DELIBERATELY_UNMAPPED: [(&str, &str); 1] = [(
    "GetDirectWorkerTransport",
    "the old wire's direct worker link: `DaemonClient::upgrade_direct` asks for it only on the \
     old wire, a wire2 connection has one link and never upgrades",
)];

/// Commands whose members the placeholder builder cannot guess (a nested object's own required
/// members), as one example each.
fn example(variant: &str) -> Option<Value> {
    Some(match variant {
        "AnswerQuestion" => json!({ "activeSessionId": "s", "questionId": "q",
                                    "reply": { "status": "answered", "answer": ["yes"] } }),
        "MutateQueuedMessage" => json!({ "activeSessionId": "s", "lane": "followUp", "index": 0,
                                         "expectedText": "x", "mutation": { "type": "delete" } }),
        _ => return None,
    })
}

/// Every `.rs` file under `dir`, recursively.
fn sources(dir: &Path, found: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("the source dir reads") {
        let path = entry.expect("a dir entry").path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            found.push(path);
        }
    }
}

/// The variants the TUI's own code builds: struct literals that set `rest`, in the crate's source
/// outside this layer, its tests and the inline test modules.
fn sent_variants() -> BTreeSet<String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    let literal = regex::Regex::new(r"DaemonCommand::([A-Z][A-Za-z]+)\s*\{").expect("a pattern");
    let mut sent = BTreeSet::new();
    for file in files {
        let relative = file
            .strip_prefix(&root)
            .expect("under src")
            .to_string_lossy()
            .to_string();
        let name = file
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if relative.starts_with("wire2_compat")
            || name == "tests.rs"
            || name.ends_with("_tests.rs")
            || relative.contains("/tests/")
        {
            continue;
        }
        let text = std::fs::read_to_string(&file).expect("a source reads");
        // An inline test module is the file's tail.
        let product = text.split("#[cfg(test)]\nmod ").next().unwrap_or_default();
        for found in literal.captures_iter(product) {
            let Some(whole) = found.get(0) else { continue };
            // The literal's body: up to its closing brace.
            let mut depth = 0usize;
            let mut body = String::new();
            for character in product[whole.end() - 1..].chars() {
                match character {
                    '{' => depth += 1,
                    '}' => depth -= 1,
                    _ => {}
                }
                body.push(character);
                if depth == 0 {
                    break;
                }
            }
            if body.contains("rest:") {
                sent.insert(found[1].to_string());
            }
        }
    }
    sent
}

/// `MutateQueuedMessage` as `mutate_queued_message`.
fn snake(variant: &str) -> String {
    let mut out = String::new();
    for (index, character) in variant.chars().enumerate() {
        if character.is_ascii_uppercase() {
            if index > 0 {
                out.push('_');
            }
            out.push(character.to_ascii_lowercase());
        } else {
            out.push(character);
        }
    }
    out
}

/// `variant` with placeholder members: each missing member gets the first of a few values of
/// different types the command takes.
fn build(variant: &str) -> DaemonCommand {
    let mut members = match example(variant) {
        Some(Value::Object(members)) => members,
        _ => Map::new(),
    };
    members.insert("type".to_string(), Value::from(snake(variant)));
    let missing = regex::Regex::new(r"missing field `([A-Za-z]+)`").expect("a pattern");
    let first_variant =
        regex::Regex::new(r"expected (?:one of )?`([A-Za-z_]+)`").expect("a pattern");
    for _ in 0..16 {
        let error = match serde_json::from_value::<DaemonCommand>(Value::Object(members.clone())) {
            Ok(command) => return command,
            Err(error) => error.to_string(),
        };
        let Some(field) = missing.captures(&error).map(|found| found[1].to_string()) else {
            panic!("{variant}: give it an example (`example`): {error}");
        };
        let candidates = [json!("x"), json!(true), json!(1), json!({}), json!([])];
        let mut chosen = None;
        for candidate in candidates {
            let mut tried = members.clone();
            tried.insert(field.clone(), candidate.clone());
            match serde_json::from_value::<DaemonCommand>(Value::Object(tried)) {
                Ok(_) => chosen = Some(candidate),
                Err(error) => {
                    let error = error.to_string();
                    if let Some(name) = first_variant.captures(&error) {
                        chosen = Some(Value::from(name[1].to_string()));
                    } else if missing
                        .captures(&error)
                        .is_some_and(|found| found[1] != *field)
                    {
                        chosen = Some(candidate);
                    } else {
                        continue;
                    }
                }
            }
            break;
        }
        let Some(value) = chosen else {
            panic!("{variant}: give it an example (`example`), `{field}` takes none of the placeholders");
        };
        members.insert(field, value);
    }
    panic!("{variant}: give it an example (`example`)")
}

#[test]
fn every_command_the_tui_sends_has_a_wire2_form() {
    let sent = sent_variants();
    assert!(
        sent.contains("Prompt") && sent.contains("Attach") && sent.contains("Create"),
        "the source scan found the TUI's commands: {sent:?}"
    );
    let unmapped: Vec<(String, &str)> = sent
        .iter()
        .filter(|variant| {
            !DELIBERATELY_UNMAPPED
                .iter()
                .any(|(listed, _)| listed == variant)
        })
        .filter_map(|variant| match outgoing("g".to_string(), build(variant)) {
            Ok(Outgoing::Unmapped { reason, .. }) => Some((variant.clone(), reason)),
            Ok(_) => None,
            Err(error) => panic!("{variant}: {error:#}"),
        })
        .collect();
    let stale: Vec<&str> = DELIBERATELY_UNMAPPED
        .iter()
        .map(|(listed, _)| *listed)
        .filter(|listed| !sent.contains(*listed))
        .collect();
    assert_eq!(
        (unmapped, stale),
        (Vec::new(), Vec::new()),
        "a command the TUI sends with no wire2 form (map it in `outgoing`, or list it in \
         DELIBERATELY_UNMAPPED with its reason), or a listed one it no longer sends"
    );
}
