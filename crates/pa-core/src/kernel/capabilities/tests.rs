//! Consistency between the capability manifests and the hand-maintained lists they describe.
//! The host-request mapping half lives beside its mapping in `tests/prompt_guards.rs`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use super::{capabilities, CapabilityManifest, MANIFESTS};

/// The test source that holds the closure freeze's seed list and its expected-closure chain.
const VENV_TESTS: &str = include_str!("../bootstrap/venv/tests.rs");

/// Session-scoped catalog events that are not a kernel capability's standalone adoption event:
/// the session lifecycle, and goal verification (`goal` is an executable skill).
const NON_CAPABILITY_SESSION_EVENTS: &[&str] = &[
    "agent started",
    "agent run completed",
    "agent session ended",
    "session archived",
    "agent goal verification",
];

fn manifests() -> Vec<CapabilityManifest> {
    capabilities()
        .expect("every embedded manifest parses")
        .into_values()
        .collect()
}

fn sorted(items: impl IntoIterator<Item = impl Into<String>>) -> Vec<String> {
    let mut items: Vec<String> = items.into_iter().map(Into::into).collect();
    items.sort();
    items
}

#[test]
fn every_manifest_file_is_embedded() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../prime-agent-runtime/capabilities");
    let on_disk = sorted(
        std::fs::read_dir(&dir)
            .unwrap_or_else(|error| panic!("read {dir:?}: {error}"))
            .map(|entry| entry.expect("directory entry").path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .map(|path| {
                path.file_stem()
                    .expect("file stem")
                    .to_string_lossy()
                    .into_owned()
            }),
    );
    assert_eq!(on_disk, sorted(MANIFESTS.iter().map(|(name, _)| *name)));
}

/// The hand-written runtime-ready probe this PR replaces, kept only to prove the derivation equal.
const PRE_DERIVATION_RUNTIME_READY_CHECK: &str = "import inspect; import rlm; from rlm import McpIntegration; import rlm.mcp as mcp; from rlm.harness import HarnessEntry; _harness_methods = ['create_memory', 'update_memory', 'delete_memory', 'create_skill', 'update_skill', 'delete_skill', 'create_subagent', 'update_subagent', 'delete_subagent', 'create_prompt_note', 'update_prompt_note', 'delete_prompt_note', 'record_refinement']; _mcp_discovery_methods = ['list_plugins', 'search_plugins', 'list_connections', 'search_tools', 'describe_tool']; assert callable(mcp.list_tools); assert callable(mcp.call_tool); assert callable(mcp.login); assert callable(rlm.rlm.session.search); assert all(callable(getattr(mcp, _m, None)) for _m in _mcp_discovery_methods), \"rlm.mcp is missing MCP discovery methods (list_plugins, search_plugins, list_connections, search_tools, describe_tool); the kernel venv needs a current prime-agent-runtime\"; assert callable(rlm.spawn); assert hasattr(rlm, 'rlm'); assert callable(rlm.rlm.spawn); assert inspect.signature(rlm.spawn).parameters['name'].default is inspect.Parameter.empty; assert not hasattr(rlm, 'run'); assert not hasattr(rlm.rlm, 'run'); assert callable(rlm.end_turn); assert callable(rlm.rlm.end_turn); assert callable(rlm.host_request); assert callable(rlm.find_models); assert callable(rlm.rlm.find_models); assert callable(rlm.create_session); assert callable(rlm.rlm.create_session); assert callable(rlm.progress_note); assert callable(rlm.rlm.progress_note); assert callable(rlm.rename); assert callable(rlm.rlm.rename); assert callable(rlm.abort); assert callable(rlm.rlm.abort); assert hasattr(rlm, 'harness'); assert hasattr(rlm, 'get_harness_state'); assert hasattr(rlm.rlm, 'harness'); assert hasattr(rlm.rlm, 'get_harness_state'); assert all(callable(getattr(_harness, _method, None)) for _harness in (rlm.harness, rlm.rlm.harness) for _method in _harness_methods); assert 'reference' in HarnessEntry.__dataclass_fields__; assert 'scope' in HarnessEntry.__dataclass_fields__; assert 'reference' in inspect.signature(rlm.harness.create_skill).parameters; assert 'reference' in inspect.signature(rlm.harness.update_skill).parameters; assert 'global_' in inspect.signature(rlm.harness.create_memory).parameters; assert 'global_' in inspect.signature(rlm.get_harness_state).parameters; assert not hasattr(rlm, 'background'); assert not hasattr(rlm.rlm, 'background'); from rlm.bash import BashHandle, BashResult; assert callable(rlm.bash); assert list(inspect.signature(rlm.bash).parameters) == ['command', 'persistent']; assert callable(rlm.persistent_shells); assert callable(rlm._set_completion_notices_wired); assert all(callable(getattr(BashHandle, _m, None)) for _m in ('tail', 'output', 'poll', 'kill')); assert {'exit_code', 'output', 'duration'} <= set(BashResult.__dataclass_fields__); import rlm.repl as _repl; assert callable(_repl.main); assert callable(_repl.emit); assert callable(_repl.host_request); assert callable(_repl.is_active); assert _repl.PROTOCOL_VERSION == 3; assert callable(rlm.emit); assert not hasattr(rlm, 'HOST_COMM_TARGET'); assert not hasattr(mcp, 'install_shutdown_hook'); import rlm.web as _web; assert callable(_web.search); assert callable(_web.read); import rlm.fs as _fs; assert all(callable(getattr(_fs, _m, None)) for _m in ('grep', 'glob', 'find', 'read', 'edit')); assert issubclass(_fs.StaleAnchorsError, RuntimeError); import rlm.schedule as _schedule; assert all(callable(getattr(_schedule, _m, None)) for _m in ('add', 'list', 'cancel')); import rlm.checkpoint as _checkpoint; assert all(callable(getattr(_checkpoint, _m, None)) for _m in ('create', 'list', 'restore')); import rlm.todo as _todo; assert all(callable(getattr(_todo, _m, None)) for _m in ('get', 'set', 'update')); import rlm.ask as _ask; assert callable(_ask.ask); import rlm.notify as _notify; assert callable(_notify.notify); import rlm.model as _model; assert callable(_model.set_effort); assert callable(_model.info); import rlm.settings as _settings; assert callable(_settings.get); import rlm.resource_reload as _reload; assert callable(_reload.reload); assert callable(rlm.rlm.reload); import rlm.usage as _usage; assert callable(_usage.get); assert callable(_repl.host_notify); import rlm.cells as _cells; assert all(callable(getattr(_cells, _m, None)) for _m in ('get', 'handles'))";

/// A probe program's `; `-joined statements (split outside string literals), sorted.
fn sorted_statements(program: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = program.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(_), '\\') => {
                current.push(ch);
                current.extend(chars.next());
            }
            (Some(open), _) => {
                current.push(ch);
                if ch == open {
                    quote = None;
                }
            }
            (None, ';') if chars.peek() == Some(&' ') => {
                chars.next();
                statements.push(std::mem::take(&mut current));
            }
            (None, '"' | '\'') => {
                quote = Some(ch);
                current.push(ch);
            }
            (None, _) => current.push(ch),
        }
    }
    statements.push(current);
    sorted(statements)
}

/// The derived runtime-ready probe holds exactly the hand-written probe's statements.
#[test]
fn the_derived_probe_holds_the_hand_written_probes_statements() {
    assert_eq!(
        sorted_statements(&crate::kernel::bootstrap::venv::RUNTIME_READY_CHECK),
        sorted_statements(PRE_DERIVATION_RUNTIME_READY_CHECK)
    );
}

/// The live closure freeze seeds from the manifests' seed modules, and its expected-closure chain
/// (which pins the `rlm.` submodules, not the package root) adds their transitive modules.
#[test]
fn the_closure_freeze_seeds_from_the_manifests() {
    let seed_line = VENV_TESTS
        .lines()
        .find_map(|line| line.strip_prefix("seed = ["))
        .expect("the closure freeze's seed list");
    let seed = sorted(
        seed_line
            .trim_end_matches(']')
            .split(", ")
            .map(|module| module.trim_matches('"')),
    );
    let chain: BTreeSet<&str> = VENV_TESTS
        .lines()
        .filter_map(|line| {
            line.trim()
                .trim_start_matches("&& ")
                .strip_prefix("report.closure.contains(&\"")
        })
        .filter_map(|rest| rest.split_once('"').map(|(module, _)| module))
        .collect();

    let manifests = manifests();
    let manifest_seed = sorted(
        manifests
            .iter()
            .filter(|manifest| manifest.closure_seed)
            .map(|manifest| manifest.module.as_str()),
    );
    let manifest_chain: BTreeSet<&str> = manifests
        .iter()
        .filter(|manifest| manifest.closure_seed)
        .map(|manifest| manifest.module.as_str())
        .chain(
            manifests
                .iter()
                .flat_map(|manifest| manifest.closure_transitive.iter().map(String::as_str)),
        )
        .filter(|module| module.starts_with("rlm."))
        .collect();
    assert_eq!((seed, chain), (manifest_seed, manifest_chain));
}

/// The bootstrap binds exactly the manifests' bound modules (`import rlm.<module> as <name>`).
#[test]
fn the_bootstrap_binds_the_manifests_modules() {
    let code = crate::kernel::bootstrap::build_rlm_bootstrap_code(&[], false);
    let bound = sorted(
        code.lines()
            .filter_map(|line| line.trim().strip_prefix("import rlm."))
            .filter_map(|line| line.split_once(" as ").map(|(_, name)| name))
            .filter(|name| !name.starts_with('_')),
    );
    assert_eq!(
        bound,
        sorted(
            manifests()
                .into_iter()
                .filter_map(|manifest| manifest.bound_module)
        )
    );
}

/// The core prompt's kernel-implemented list is the manifests' prompt builtins, and its bullet
/// tokens under those namespaces are exactly the manifests' documented names; each manifest's
/// host requests surface as names that same manifest documents.
#[test]
fn the_core_prompt_names_the_manifests_tools() {
    let core_layer = crate::prompts::layers::CORE_LAYER;
    let (_, built_in) = core_layer
        .split_once("All programmatic tools described above except for ")
        .expect("the core prompt's kernel-implemented list");
    let (built_in, _) = built_in
        .split_once(" are implemented")
        .expect("the end of the kernel-implemented list");
    let listed = sorted(
        built_in
            .split(", ")
            .map(|name| name.trim_start_matches("and ").trim_matches('`')),
    );

    let manifests = manifests();
    let builtins = sorted(
        manifests
            .iter()
            .flat_map(|manifest| manifest.prompt_builtins.iter().map(String::as_str)),
    );
    let namespaces: BTreeSet<&str> = builtins
        .iter()
        .map(|name| name.trim_end_matches(".*"))
        .collect();
    let bullets: BTreeSet<&str> = core_layer
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("- `"))
        .filter_map(|rest| rest.split_once('`').map(|(token, _)| token))
        .filter_map(|token| token.split('(').next())
        .map(str::trim)
        .filter(|token| {
            token.starts_with(|ch: char| ch.is_ascii_lowercase())
                && token.chars().all(|ch| {
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '_' || ch == '.'
                })
        })
        .filter(|token| namespaces.contains(token.split('.').next().unwrap_or_default()))
        .collect();
    let undocumented_requests: Vec<&str> = manifests
        .iter()
        .flat_map(|manifest| {
            manifest
                .host_requests
                .values()
                .filter(|token| !manifest.documented.contains(token))
                .map(String::as_str)
        })
        .collect();

    assert_eq!(
        (listed, sorted(bullets), undocumented_requests),
        (
            builtins,
            sorted(
                manifests
                    .iter()
                    .flat_map(|manifest| manifest.documented.iter().map(String::as_str))
            ),
            Vec::new()
        )
    );
}

/// Every standalone session-scoped adoption event in the telemetry catalog is claimed by the
/// manifests, and its properties are exactly `session_id` plus the claimed counts.
#[test]
fn the_telemetry_catalog_holds_the_manifests_events() {
    let catalogued: BTreeMap<&str, Vec<String>> = pa_telemetry::catalog()
        .into_iter()
        .filter(|rule| {
            rule.properties
                .iter()
                .any(|(name, _)| *name == "session_id")
                && !NON_CAPABILITY_SESSION_EVENTS.contains(&rule.name)
        })
        .map(|rule| {
            (
                rule.name,
                sorted(rule.properties.iter().map(|(name, _)| *name)),
            )
        })
        .collect();

    let mut claimed: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let manifests = manifests();
    for claim in manifests.iter().flat_map(|manifest| &manifest.telemetry) {
        claimed
            .entry(claim.event.as_str())
            .or_insert_with(|| vec!["session_id".to_string()])
            .extend(claim.properties.iter().cloned());
    }
    for properties in claimed.values_mut() {
        properties.sort();
    }
    assert_eq!(catalogued, claimed);
}
