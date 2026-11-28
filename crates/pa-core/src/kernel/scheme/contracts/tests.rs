//! Call contracts through a real session kernel, and the guards that hold
//! every kernel function to them.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use steel::parser::ast::{Define, ExprKind};
use steel::parser::parser::Parser;
use steel::parser::tokens::TokenType;

use crate::kernel::call_refusals::{register_call_refusal_handler, CallRefusal};
use crate::kernel::scheme::SchemeKernelProvisioner;
use crate::kernel::shared::{host_handler, HostRequestHandlers, HostRequestPayload};
use crate::tools::kernel_cell::{KernelExecuteOptions, KernelExecutor, KernelProvisioner};

/// The error each cell raises (`None` for a cell that ran).
async fn errors(kernel: &dyn KernelExecutor, cells: &[String]) -> Vec<Option<String>> {
    let mut errors = Vec::new();
    for cell in cells {
        let result = kernel
            .execute(
                cell,
                KernelExecuteOptions {
                    signal: None,
                    timeout_ms: None,
                    on_stream: None,
                    on_late_sent_agent_message: None,
                    tool_call_id: None,
                },
            )
            .await
            .unwrap();
        errors.push(result.error.map(|error| error.evalue));
    }
    errors
}

/// Every public function the kernel's product sources define at the top
/// level (`(define (name ...) ...)` at the start of a line in a `.scm` file
/// or in the Scheme text a `.rs` file embeds), parsed by Steel's own parser.
/// Test files hold doubles, not the product's surface.
fn kernel_definitions() -> BTreeMap<String, Define> {
    let mut definitions = BTreeMap::new();
    let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src/kernel/scheme")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let product = path
                .extension()
                .is_some_and(|extension| extension == "scm" || extension == "rs");
            if !product || name.ends_with("tests.rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let source = text
                .split_once("#[cfg(test)]\nmod tests {")
                .map_or(text.as_str(), |(product, _)| product);
            for (at, _) in source.match_indices("\n(define (") {
                // The form's text: up to the parenthesis that closes it,
                // strings and comments skipped.
                let mut depth = 0_u32;
                let mut letters = source[at + 1..].char_indices();
                let mut end = None;
                while let (Some((offset, letter)), None) = (letters.next(), end) {
                    match letter {
                        '(' | '[' => depth += 1,
                        ')' | ']' => {
                            depth -= 1;
                            if depth == 0 {
                                end = Some(at + 1 + offset + 1);
                            }
                        }
                        '"' => {
                            while let Some((_, inside)) = letters.next() {
                                match inside {
                                    '\\' => {
                                        letters.next();
                                    }
                                    '"' => break,
                                    _ => {}
                                }
                            }
                        }
                        ';' => {
                            letters.find(|(_, comment)| *comment == '\n');
                        }
                        '\\' => {
                            letters.next();
                        }
                        _ => {}
                    }
                }
                let form =
                    &source[at + 1..end.unwrap_or_else(|| panic!("unclosed form in {name}"))];
                let parsed =
                    Parser::parse(form).unwrap_or_else(|error| panic!("{name}: {form}: {error:?}"));
                let [ExprKind::Define(define)] = &parsed[..] else {
                    panic!("{name}: not one definition: {form}");
                };
                let function = define.name.atom_identifier().unwrap().resolve().to_string();
                if !function.starts_with('%') {
                    definitions.insert(function, (**define).clone());
                }
            }
        }
    }
    definitions
}

/// A definition's parameters as a call sees them: how many arguments it
/// requires, its optional arguments' names, each keyword with the variable
/// it binds, and whether a rest parameter takes what follows.
fn parameters(define: &Define) -> (usize, Vec<String>, Vec<(String, String)>, bool) {
    let ExprKind::LambdaFunction(function) = &define.body else {
        panic!("{} is not a function", define.name);
    };
    let (mut required, mut optional, mut keywords) = (0, Vec::new(), Vec::new());
    let mut declared = function.args.iter().peekable();
    while let Some(parameter) = declared.next() {
        if let Some(TokenType::Keyword(keyword)) =
            parameter.atom_syntax_object().map(|syntax| &syntax.ty)
        {
            let binding = match declared.next().unwrap() {
                ExprKind::List(default) => default.first().unwrap().to_string(),
                plain => plain.to_string(),
            };
            keywords.push((keyword.resolve().to_string(), binding));
        } else if let ExprKind::List(default) = parameter {
            optional.push(default.first().unwrap().to_string());
        } else if !(function.rest && declared.peek().is_none()) {
            required += 1;
        }
    }
    (required, optional, keywords, function.rest)
}

/// What Steel's own keyword binding let through: a misspelt option ran the
/// call with the default, a repeated one took the first, and the rest tools
/// dropped extra arguments. Each is refused with what to write instead, the
/// host hears of none of them, and each is counted by its class.
#[tokio::test(flavor = "multi_thread")]
async fn a_call_its_function_does_not_declare_is_refused_before_the_function_runs() {
    let received = Arc::new(Mutex::new(Vec::new()));
    let mut handlers = HostRequestHandlers::new();
    for request_type in [
        "rlm.find_models",
        "rlm.collect",
        "rlm.abort",
        "web.search",
        "agent_observe.recent",
    ] {
        let received = Arc::clone(&received);
        handlers.register(
            request_type,
            host_handler(move |payload: HostRequestPayload| {
                received.lock().unwrap().push(payload.data);
                async { Ok(json!({"models": [], "results": []})) }
            }),
        );
    }
    let (counted, mut counts) = tokio::sync::mpsc::unbounded_channel();
    register_call_refusal_handler(
        &mut handlers,
        Some(Arc::new(move |refusal| {
            let _ = counted.send(refusal);
        })),
    );
    let kernel = SchemeKernelProvisioner::new(handlers, "/")
        .ensure(None, None)
        .await
        .unwrap();
    let refused = [
        (
            r#"(rlm-find-models "claude" #:limt 1)"#,
            "rlm-find-models: unknown option #:limt. Did you mean #:limit? rlm-find-models accepts #:limit",
            CallRefusal::UnknownOption,
        ),
        (
            "(rlm-collect #:timeout 500)",
            "rlm-collect: unknown option #:timeout. Did you mean #:timeout-ms? rlm-collect accepts #:timeout-ms",
            CallRefusal::UnknownOption,
        ),
        (
            r#"(web-search "q" #:max-results 3)"#,
            "web-search: unknown option #:max-results. Did you mean #:num-results? web-search accepts #:provider #:num-results #:recency #:timeout",
            CallRefusal::UnknownOption,
        ),
        (
            r#"(rlm-abort "worker" #:force #t)"#,
            "rlm-abort: unknown option #:force. rlm-abort takes no options: (rlm-abort target)",
            CallRefusal::UnknownOption,
        ),
        (
            r#"(bash "true" #:timeout-ms 5000)"#,
            "bash: unknown option #:timeout-ms. bash takes no options as keywords: its options are one hash, (hash 'timeout-ms value), passed as [options] in (bash command [options])",
            CallRefusal::UnknownOption,
        ),
        (
            r#"(bash "true" (hash 'timout-ms 5000))"#,
            "bash: unknown option 'timout-ms. Did you mean 'timeout-ms? bash accepts 'timeout-ms 'wait-ms 'background 'persistent 'elevated",
            CallRefusal::UnknownOption,
        ),
        (
            "(fs-find (hash 'max-result 1))",
            "fs-find: unknown option 'max-result. Did you mean 'max-results? fs-find accepts 'path 'glob 'name 'kind 'ignore-case 'max-depth 'hidden 'gitignore 'max-results",
            CallRefusal::UnknownOption,
        ),
        (
            r#"(agent-roster "worker" #:recent #t #:limit 1 #:limit 2)"#,
            "agent-roster: option #:limit is given twice: pass it once",
            CallRefusal::RepeatedOption,
        ),
        (
            r#"(rlm-find-models "claude" #:limit)"#,
            "rlm-find-models: option #:limit has no value: write #:limit followed by its value",
            CallRefusal::OptionWithoutValue,
        ),
        (
            r#"(rlm-abort "worker" "other")"#,
            "rlm-abort: expected 1 argument before any options, got 2: (rlm-abort target)",
            CallRefusal::Arity,
        ),
        (
            r#"(rlm-collect "worker" "other" #:timeout-ms 5)"#,
            "rlm-collect: expected 0 to 1 argument before any options, got 2: (rlm-collect [targets] #:timeout-ms)",
            CallRefusal::Arity,
        ),
        (
            "(rlm-collect 'worker)",
            "rlm-collect: the symbol 'worker cannot be passed in a call that gives an optional argument (it would be dropped): pass a string: (rlm-collect [targets] #:timeout-ms)",
            CallRefusal::Arity,
        ),
        (
            r#"(bash "true" (hash) 5000)"#,
            "bash: expected 1 to 2 arguments before any options, got 3: (bash command [options])",
            CallRefusal::Arity,
        ),
    ];
    let cells: Vec<String> = refused
        .iter()
        .map(|(cell, _, _)| (*cell).to_string())
        .collect();
    let raised = errors(kernel.as_ref(), &cells).await;
    let mut classes = Vec::new();
    for _ in &refused {
        classes.push(
            tokio::time::timeout(Duration::from_secs(10), counts.recv())
                .await
                .expect("every refusal is counted"),
        );
    }
    // Counts are sent without being waited for: they arrive in any order.
    let mut expected: Vec<Option<CallRefusal>> =
        refused.iter().map(|(_, _, class)| Some(*class)).collect();
    classes.sort_by_key(|class| class.map(CallRefusal::class));
    expected.sort_by_key(|class| class.map(CallRefusal::class));
    assert_eq!(
        (raised, classes, received.lock().unwrap().clone()),
        (
            refused
                .iter()
                .map(|(_, reason, _)| Some(format!("Error: Generic: {reason}")))
                .collect(),
            expected,
            Vec::<Value>::new(),
        )
    );
}

/// Every function the kernel's sources define refuses a keyword it does not
/// declare, and each tool that takes an options hash refuses a key it does
/// not read: a function added outside [`super::load`], or a hash tool that
/// stops checking its keys, fails here. The exempt functions take a rest
/// list, which a contract cannot speak for.
#[tokio::test(flavor = "multi_thread")]
async fn every_kernel_function_refuses_an_option_it_does_not_declare() {
    let definitions = kernel_definitions();
    let mut variadic = Vec::new();
    let mut probes = Vec::new();
    for (name, define) in &definitions {
        let (required, optional, _, rest) = parameters(define);
        if rest {
            variadic.push(name.as_str());
            continue;
        }
        let arguments = " \"x\"".repeat(required);
        probes.push((
            format!("({name}{arguments} #:no-such-option 1)"),
            format!("Error: Generic: {name}: unknown option #:no-such-option."),
        ));
        if optional.iter().any(|optional| optional == "options") {
            probes.push((
                format!("({name}{arguments} (hash 'no-such-option 1))"),
                format!("Error: Generic: {name}: unknown option 'no-such-option."),
            ));
        }
    }
    let kernel = SchemeKernelProvisioner::new(HostRequestHandlers::new(), "/")
        .ensure(None, None)
        .await
        .unwrap();
    let cells: Vec<String> = probes.iter().map(|(cell, _)| cell.clone()).collect();
    let not_refused: Vec<(String, Option<String>)> = errors(kernel.as_ref(), &cells)
        .await
        .into_iter()
        .zip(probes)
        .filter(|(raised, (_, refusal))| {
            !raised
                .as_ref()
                .is_some_and(|raised| raised.starts_with(refusal))
        })
        .map(|(raised, (cell, _))| (cell, raised))
        .collect();
    assert_eq!(
        (not_refused, variadic),
        // Any number of image paths.
        (Vec::new(), vec!["attach-image"])
    );
}

/// The accepted options cannot drift from the ones read. A keyword function
/// accepts what its definition binds, so the guard is that every bound
/// keyword is used by the body. `bash` and its job functions read their
/// options from a hash in Rust: the names their table accepts are the names
/// the code reads.
#[test]
fn every_declared_option_is_read_by_its_function() {
    let unread: Vec<String> = kernel_definitions()
        .iter()
        .flat_map(|(name, define)| {
            let ExprKind::LambdaFunction(function) = &define.body else {
                panic!("{name} is not a function");
            };
            let body = function.body.to_string();
            let used: BTreeSet<&str> = body
                .split(|letter: char| letter.is_whitespace() || "()[]'".contains(letter))
                .collect();
            parameters(define)
                .2
                .into_iter()
                .filter(|(_, binding)| !used.contains(binding.as_str()))
                .map(|(keyword, _)| format!("{name} {keyword}"))
                .collect::<Vec<_>>()
        })
        .collect();
    let bash = include_str!("../native/bash.rs");
    let table = bash
        .split_once("let accepted: &[&str] = match operation {")
        .and_then(|(_, rest)| rest.split_once("};"))
        .map(|(table, _)| table)
        .expect("bash.rs declares its accepted options");
    let quoted_after = |text: &str, opening: &str| -> BTreeSet<String> {
        text.match_indices(opening)
            .filter_map(|(at, _)| text[at + opening.len()..].split('"').next())
            .map(str::to_string)
            .collect()
    };
    let declared: BTreeSet<String> = table
        .match_indices("&[")
        .flat_map(|(at, _)| {
            let list = table[at..].split(']').next().unwrap_or_default();
            quoted_after(list, "\"")
                .into_iter()
                .filter(|name| !name.trim_matches([',', ' ', '\n']).is_empty())
        })
        .collect();
    let mut read = quoted_after(bash, "fields.get(\"");
    read.extend(quoted_after(bash, "flag(\""));
    // The command is the function's argument, not an option.
    read.remove("command");
    assert_eq!((unread, read), (Vec::<String>::new(), declared));
}
