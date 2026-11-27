//! Prompt guard tests: cache safety (static layers never contain dynamic content; the cached prefix
//! is byte-identical across sessions) and tool surface (the API reference the prompt is spliced
//! from documents exactly the functions the Scheme kernel's sources define, with their call forms;
//! a kernel change without a reference update fails here).

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::LazyLock;

use pa_core::prompts::layers::{
    documented_calls, API_DOCS, CORE_LAYER, OPINIONATED_LAYER, USAGE_LAYER,
};
use pa_core::prompts::model_prompts::MODEL_PROMPTS_TOML;
use pa_core::prompts::system_prompt::{
    system_prompt_breakdown, BuildSystemPromptOptions, HostAccess, SegmentKind,
};
use pa_core::skills::load_skills_from_dir;

#[path = "common/host_requests.rs"]
mod host_requests;
use host_requests::registered_host_requests;

/// The workspace bundled skills directory (source-checkout layout):
/// pa-core lives at `<root>/crates/pa-core`.
fn bundled_skills_dir() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root")
        .join("skills")
}

fn sorted_bundled_skills() -> Vec<pa_core::skills::Skill> {
    let skills_dir = bundled_skills_dir();
    let mut loaded = load_skills_from_dir(&skills_dir, "package");
    loaded
        .skills
        .sort_by(|left, right| left.name.cmp(&right.name));
    loaded.skills
}

// Cache safety

/// Strings that only dynamic segments generate. If one of these ever appears in a static layer
/// file, the cached prefix has started leaking per-session content.
const DYNAMIC_ONLY_MARKERS: &[&str] = &[
    "Working directory:",
    "Conversation log:",
    "Recursive agent depth:",
    "Current date:",
    "<available_skills>",
    "Enabled generic MCP servers:",
    "# Project Context",
    "You are a child agent spawned by",
    "Machine: the user's own computer",
];

#[test]
fn static_layers_contain_no_dynamic_content() {
    for (name, text) in [
        ("core", CORE_LAYER.as_str()),
        ("usage", USAGE_LAYER),
        ("opinionated", OPINIONATED_LAYER),
        ("model-prompts", MODEL_PROMPTS_TOML),
    ] {
        for marker in DYNAMIC_ONLY_MARKERS {
            assert!(
                !text.contains(marker),
                "static layer {name} contains dynamic-only marker {marker:?}: \
                 dynamic content must stay in the tail, not the cacheable prefix"
            );
        }
    }
}

#[test]
fn cached_prefix_is_stable_across_sessions() {
    let mut options = BuildSystemPromptOptions {
        cwd: "/first/cwd".to_string(),
        messages_path: Some("/first/session.jsonl".to_string()),
        model_prompt_extras: Some("model guidance."),
        skills: sorted_bundled_skills(),
        selected_tools: Some(vec![pa_types::ai::SCHEME_TOOL_NAME]),
        host: Some(HostAccess {
            route: pa_types::platform::ElevationRoute::Uac,
            elevation: pa_types::platform::Elevation::Standard,
        }),
        ..Default::default()
    };
    let first = system_prompt_breakdown(&options);

    options.cwd = "/completely/different".into();
    options.messages_path = None;
    options.rlm_depth = Some(3);
    options.rlm_parent_agent = Some("orchestrator");
    options.generic_mcp_servers = vec!["slack".into(), "github".into()];
    options.context_files = vec![("AGENTS.md".into(), "Never commit main.".into())];
    options.append_system_prompt = Some("And one more thing.".into());
    options.project_card = Some("# Project card\n\n- Test: `cargo test`".into());
    options.host = Some(HostAccess {
        route: pa_types::platform::ElevationRoute::UnixSudo,
        elevation: pa_types::platform::Elevation::Elevated,
    });
    let second = system_prompt_breakdown(&options);

    assert_eq!(first.cached_prefix_len, second.cached_prefix_len);
    assert_eq!(
        &first.assembled[..first.cached_prefix_len],
        &second.assembled[..second.cached_prefix_len]
    );
    assert_eq!(
        &first.assembled[..first.cached_prefix_len],
        &format!(
            "{}\n\n{}\n\n{}\n\nmodel guidance.",
            CORE_LAYER.trim(),
            USAGE_LAYER.trim(),
            OPINIONATED_LAYER.trim()
        )
    );

    let first_tail = &first.assembled[first.cached_prefix_len..];
    assert!(first_tail.contains("Current date: "));
    assert!(first_tail.contains("Working directory: /first/cwd"));
    assert!(first_tail.contains("Recursive agent depth: 0 (root)"));
    assert!(first_tail.contains("<available_skills>"));
    let second_tail = &second.assembled[second.cached_prefix_len..];
    assert!(second_tail.contains("/completely/different"));
    assert!(second_tail.contains("# Project card\n\n- Test: `cargo test`"));
    assert!(second_tail.contains("Enabled generic MCP servers: `github`, `slack`."));
    assert!(second_tail.contains("Recursive agent depth: 3 (not root)"));
    assert!(second_tail.contains("This session runs as root"));
    assert!(second_tail.contains("You are a child agent spawned by orchestrator."));
    assert!(second_tail.contains("# Project Context"));
    assert!(second_tail.ends_with("And one more thing."));

    for segment in &first.segments {
        let inside = first
            .assembled
            .find(&segment.text)
            .is_some_and(|at| at < first.cached_prefix_len);
        assert_eq!(
            inside,
            segment.kind == SegmentKind::Static,
            "segment {} on the wrong side of the cache boundary",
            segment.name
        );
    }
}

// Tool surface

/// The Scheme kernel's product sources (`src/kernel/scheme`): every `.scm` file, and every `.rs`
/// file up to its inline test module. Test files hold doubles, not the product's surface.
static KERNEL_SOURCES: LazyLock<Vec<String>> = LazyLock::new(|| {
    let mut sources = Vec::new();
    let mut stack = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src/kernel/scheme")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
        {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let extension = path.extension().and_then(|extension| extension.to_str());
            let scheme = extension == Some("scm");
            let test_file = path
                .file_name()
                .is_some_and(|name| name.to_string_lossy().ends_with("tests.rs"));
            if !scheme && (extension != Some("rs") || test_file) {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("read source");
            sources.push(match text.split_once("#[cfg(test)]\nmod tests {") {
                Some((product, _)) if !scheme => product.to_string(),
                Some(_) | None => text,
            });
        }
    }
    sources
});

/// Every model-facing function the kernel's sources define (`(define (name ...) ...)` at the
/// start of a line, names starting with `%` being internal), mapped to its call form as the
/// reference writes it: required arguments, `[optional]` ones, then each `#:keyword`. `None` for a
/// function that takes a rest argument, whose call form the definition does not spell out (the
/// tools taking an options hash).
fn kernel_functions() -> BTreeMap<String, Option<String>> {
    let mut functions = BTreeMap::new();
    for source in KERNEL_SOURCES.iter() {
        for (at, opening) in source.match_indices("(define (") {
            let line_start = source[..at].rfind('\n').map_or(0, |newline| newline + 1);
            if !source[line_start..at].trim().trim_matches('"').is_empty() {
                continue;
            }
            // The parameter list's top-level tokens: a bracketed default is one token.
            let mut tokens: Vec<String> = vec![String::new()];
            let mut depth = 1_u32;
            for letter in source[at + opening.len()..].chars() {
                match letter {
                    '(' | '[' => depth += 1,
                    ')' | ']' => depth -= 1,
                    _ => {}
                }
                if depth == 0 {
                    break;
                }
                if depth == 1 && letter.is_whitespace() {
                    tokens.push(String::new());
                } else if let Some(token) = tokens.last_mut() {
                    token.push(letter);
                }
            }
            tokens.retain(|token| !token.is_empty());
            let Some((name, parameters)) = tokens.split_first() else {
                continue;
            };
            if name.starts_with('%') {
                continue;
            }
            let mut form = Some(vec![name.clone()]);
            let mut parameters = parameters.iter();
            while let (Some(parameter), Some(written)) = (parameters.next(), form.as_mut()) {
                if parameter == "." {
                    form = None;
                } else if parameter.starts_with("#:") {
                    // The keyword's binding and default follow it.
                    parameters.next();
                    written.push(parameter.clone());
                } else if let Some(optional) = parameter.strip_prefix('[') {
                    let name = optional.split_whitespace().next().unwrap_or(optional);
                    written.push(format!("[{name}]"));
                } else {
                    written.push(parameter.clone());
                }
            }
            functions.insert(name.clone(), form.map(|written| written.join(" ")));
        }
    }
    functions
}

/// The API reference (`prompts/capabilities/`, spliced into the core layer and served by
/// `(api-help)`) documents exactly the functions the kernel defines, each once and with the call
/// form its definition has: a function added, removed, or given another argument without a
/// reference update fails here. No topic is named like a function, so `(api-help name)` is
/// unambiguous.
#[test]
fn the_reference_documents_exactly_the_kernels_functions() {
    let calls: Vec<&str> = API_DOCS
        .iter()
        .flat_map(|(_, doc)| documented_calls(doc))
        .collect();
    let documented: BTreeMap<String, String> = calls
        .iter()
        .map(|call| {
            (
                call.split(' ').next().unwrap_or(call).to_string(),
                (*call).to_string(),
            )
        })
        .collect();
    let defined: BTreeMap<String, String> = kernel_functions()
        .into_iter()
        .map(|(name, form)| {
            let form = form
                .or_else(|| documented.get(&name).cloned())
                .unwrap_or_else(|| format!("{name} (not documented)"));
            (name, form)
        })
        .collect();
    let topics_named_like_functions: Vec<&str> = API_DOCS
        .iter()
        .map(|(topic, _)| *topic)
        .filter(|topic| defined.contains_key(*topic))
        .collect();
    assert_eq!(
        (documented.len(), &documented, topics_named_like_functions),
        (calls.len(), &defined, Vec::new())
    );
}

/// Each entry of the reference names only keywords a call can pass: the ones its own function
/// declares, or those of another function the entry names (`` `rlm-spawn`'s `#:model` ``, or a
/// call form of it). A keyword the kernel refuses cannot be taught.
#[test]
fn the_reference_names_no_keyword_its_function_refuses() {
    let functions = kernel_functions();
    let declares = |name: &str, keyword: &str| {
        functions
            .get(name)
            .and_then(Option::as_deref)
            .is_some_and(|form| form.split(' ').any(|word| word == keyword))
    };
    let mut refused = Vec::new();
    for (_, doc) in API_DOCS {
        let mut lines = doc.lines().peekable();
        while let Some(bullet) = lines.next() {
            let Some(call) = documented_calls(bullet).next() else {
                continue;
            };
            let name = call.split(' ').next().unwrap_or(call);
            let mut entry = bullet.to_string();
            while let Some(nested) = lines.next_if(|line| line.starts_with("  ")) {
                entry.push_str(nested);
            }
            for (at, _) in entry.match_indices("#:") {
                let keyword = entry[at..]
                    .split(|letter: char| {
                        !(letter.is_ascii_alphanumeric() || "#:-".contains(letter))
                    })
                    .next()
                    .unwrap_or_default();
                let taught = declares(name, keyword)
                    || functions.keys().any(|other| {
                        declares(other, keyword)
                            && (entry.contains(&format!("`{other}`"))
                                || entry.contains(&format!("({other} ")))
                    });
                if !taught {
                    refused.push(format!("{name}: {keyword}"));
                }
            }
        }
    }
    assert_eq!(refused, Vec::<String>::new());
}

/// The capability manifests' `internal_host_requests`: requests the kernel issues as host
/// plumbing, never as a model-facing function.
static MANIFEST_INTERNAL_HOST_REQUESTS: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
    /// The manifest key this file reads (the schema is in the manifests' README).
    #[derive(serde::Deserialize)]
    struct ManifestHostSurface {
        #[serde(default)]
        internal_host_requests: Vec<String>,
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/kernel/capabilities");
    let mut internal = BTreeSet::new();
    for entry in
        std::fs::read_dir(&dir).unwrap_or_else(|error| panic!("read {}: {error}", dir.display()))
    {
        let path = entry.expect("directory entry").path();
        if path.extension().is_none_or(|ext| ext != "toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("manifest text");
        let manifest: ManifestHostSurface = toml::from_str(&text)
            .unwrap_or_else(|error| panic!("unparseable manifest {}: {error}", path.display()));
        internal.extend(manifest.internal_host_requests);
    }
    internal
});

/// Registered host-request types no capability manifest declares that are host-internal plumbing,
/// not model-facing programmatic tools.
const NON_MANIFEST_INTERNAL_HOST_REQUESTS: &[&str] = &[
    // The factory module reports run milestones; the model never calls it.
    "factory.progress",
    // pa-core's gate for `model.set_effort` hands the level to the embedding's switch.
    "model.apply_effort",
];

/// Registered model-facing host requests the Scheme kernel does not issue, each with the reason
/// the model loses nothing by it. Anything else the kernel does not issue is a capability the
/// model cannot reach, and fails the guard.
const HOST_REQUESTS_THE_SCHEME_KERNEL_DOES_NOT_ISSUE: &[&str] = &[
    // A tombstone: its handler only answers that the request was removed in favour of
    // `agent_observe.list`, which `agent-observe-list` issues.
    "agent_message.list_agents",
];

/// Every host request the session registers for the model is one the Scheme kernel issues (its
/// type is a literal in the kernel's sources, behind a function the reference documents), is
/// declared host plumbing, or is listed above with its reason; and that list names nothing the
/// kernel does issue.
#[test]
fn every_model_facing_host_request_is_issued_by_the_scheme_kernel() {
    let issued = |request: &str| {
        let literal = format!("\"{request}\"");
        KERNEL_SOURCES
            .iter()
            .any(|source| source.contains(&literal))
    };
    let unreachable: Vec<String> = registered_host_requests()
        .into_iter()
        .filter(|request| {
            !MANIFEST_INTERNAL_HOST_REQUESTS.contains(request)
                && !NON_MANIFEST_INTERNAL_HOST_REQUESTS.contains(&request.as_str())
                && !HOST_REQUESTS_THE_SCHEME_KERNEL_DOES_NOT_ISSUE.contains(&request.as_str())
                && !issued(request)
        })
        .collect();
    let stale: Vec<&str> = HOST_REQUESTS_THE_SCHEME_KERNEL_DOES_NOT_ISSUE
        .iter()
        .copied()
        .filter(|request| issued(request))
        .collect();
    assert_eq!((unreachable, stale), (Vec::new(), Vec::new()));
}

/// Every host request a capability manifest declares as plumbing is one pa-core registers a
/// handler for.
#[test]
fn capability_manifests_declare_registered_host_requests() {
    let registered = registered_host_requests();
    let unregistered: Vec<&String> = MANIFEST_INTERNAL_HOST_REQUESTS
        .iter()
        .filter(|request| !registered.contains(*request))
        .collect();
    assert_eq!(unregistered, Vec::<&String>::new());
}

// Packaged-set parity (TS packages/coding-agent/skills)

/// The packaged skill set at TS tip `f62dae4d0` (`packages/coding-agent/skills/`): 12 skills — the
/// generic `mcp` doc skill in, the per-service linear/notion pair out (TS removed theirs when the
/// generic MCP surface landed).
const TS_PACKAGED_SKILL_SET: &[&str] = &[
    "agent-message",
    "agent-observe",
    "attach-image",
    "compact",
    "edit",
    "goal",
    "mcp",
    "prime-intellect",
    "refine",
    "rlm-heartbeat",
    "skill-creator",
    "websearch",
];

/// The bundled skills directory matches the TS packaged set name-for-name: the generic `mcp` skill
/// is present and markdown-only, and the retired per-service pair (linear/notion) is gone.
#[test]
fn bundled_skills_match_the_ts_packaged_set() {
    let skills = sorted_bundled_skills();
    let names: Vec<&str> = skills.iter().map(|skill| skill.name.as_str()).collect();
    let mut expected = TS_PACKAGED_SKILL_SET.to_vec();
    expected.sort_unstable();
    assert_eq!(
        names, expected,
        "the packaged skill set must match the TS packaged set"
    );
    assert!(
        !names.contains(&"linear") && !names.contains(&"notion"),
        "the per-service MCP skills retired with the generic mcp skill"
    );
    let mcp_skill = skills
        .iter()
        .find(|skill| skill.name == "mcp")
        .expect("the generic mcp skill is packaged");
    assert_eq!(
        mcp_skill.kind,
        pa_core::skills::SkillKind::Markdown,
        "the mcp skill is documentation for the kernel's mcp functions"
    );
    assert_eq!(
        mcp_skill.description,
        "Use external MCP services generically from the Scheme kernel - search the supported-service catalog, inspect the user's connections, discover live tool schemas, and call tools on any connection (Notion, Linear, Slack, and the rest of the catalog) without per-service packages."
    );
}

/// The bounded-wait contract renders wherever the kernel does: every session kind, daemon
/// or daemonless, registers the bash completion notice the snapshot relies on.
#[test]
fn bounded_wait_contract_renders_in_daemon_and_daemonless_sessions() {
    let mut options = BuildSystemPromptOptions {
        cwd: "/w".to_string(),
        messages_path: Some("/log.jsonl".to_string()),
        skills: sorted_bundled_skills(),
        selected_tools: Some(vec![pa_types::ai::SCHEME_TOOL_NAME]),
        ..Default::default()
    };
    for daemonless in [false, true] {
        options.daemonless = daemonless;
        let prompt = system_prompt_breakdown(&options).assembled;
        assert!(
            prompt.contains("`running` is `#t`")
                && prompt.contains("`exit-code` is `#f`")
                && prompt.contains("`(bash cmd (hash 'wait-ms 120000))`"),
            "daemonless={daemonless}: the prompt must carry the bounded-wait result contract: {prompt}"
        );
    }
}

/// The generic mcp skill loads through the normal markdown discovery and renders in the
/// `<available_skills>` inventory exactly like any other markdown skill: the `[skill name
/// location]`-style XML the TS `formatSkillsForPrompt` emits.
#[test]
fn generic_mcp_skill_renders_in_the_prompt_inventory() {
    let mut options = BuildSystemPromptOptions {
        cwd: "/w".to_string(),
        messages_path: Some("/log.jsonl".to_string()),
        skills: sorted_bundled_skills(),
        selected_tools: Some(vec![pa_types::ai::SCHEME_TOOL_NAME]),
        ..Default::default()
    };
    let breakdown = system_prompt_breakdown(&options);
    let inventory = breakdown
        .segments
        .iter()
        .find(|segment| {
            matches!(segment.kind, SegmentKind::Dynamic)
                && segment.text.contains("<available_skills>")
        })
        .map(|segment| segment.text.clone())
        .expect("the skills inventory segment renders");
    assert!(inventory.contains("<name>mcp</name>"));
    assert!(inventory.contains("<type>markdown</type>"));
    options.generic_mcp_servers = vec!["notion".into()];
    let second = system_prompt_breakdown(&options);
    assert!(second.assembled.contains("(mcp-list-tools \"notion\")"));
}

// Turn discipline

/// Since #3580 every session kind wakes on bash completion: daemonless runs
/// get a queued `[bash-done]` turn, so the static usage layer must not carry
/// the pre-#3580 rule to await commands before ending the turn — the
/// no-polling rule right below it already ends turns on the done
/// notification, and an unscoped await rule would contradict that wake.
#[test]
fn usage_layer_ends_turns_on_the_done_notification_not_an_await_rule() {
    assert!(
        !USAGE_LAYER.contains("await commands before ending the turn"),
        "every session kind has completion notifications: the await-before-turn-end rule is stale"
    );
    assert!(
        USAGE_LAYER.contains("end the turn and collect the result on the done notification"),
        "the no-polling rule keeps the collect-on-notice turn-end contract"
    );
}
