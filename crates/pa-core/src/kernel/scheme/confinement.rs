//! Builds the engines sessions run on with Steel's own ways out of the
//! kernel closed.
//!
//! Steel's builtins include a file system, ports over files, a process
//! launcher, sockets, a git client, environment variables, file loaders and
//! an engine constructor. They act on the agent process itself: relative
//! paths resolve against its directory, not the session's, and nothing they
//! do passes the session's file tools, its `bash`, its observers, or its
//! cancellation. A session's cells reach the outside only through the
//! kernel's own functions, so every engine built here has them closed:
//!
//! - It starts from Steel's sandboxed engine, which has no file system
//!   module, no sockets, and ports that refuse to open a file.
//! - Every function of the modules of [`CLOSED_MODULES`], and each of
//!   [`CLOSED_FUNCTIONS`] in the modules that otherwise stay, is replaced in
//!   its module by a function that raises, so neither `require-builtin` nor
//!   a module lookup finds the real one.
//! - Every global those names were bound to, plain and under Steel's
//!   `#%prim.` prefix, is rebound to that function too.
//! - `(require "file.scm")` finds no file: [`NoSourceFiles`] answers for
//!   every module name Steel does not embed.
//!
//! What stays open: `display` and the string ports its capture uses, `eval`
//! (it compiles in the same closed engine), threads, time, and the pure
//! library. A cell can still block on the process's standard input through
//! `(read-line)`: closing the port accessors does not change the default
//! input port.

use std::collections::{BTreeMap, BTreeSet};

use steel::compiler::modules::SourceModuleResolver;
use steel::rerrs::{ErrorKind, SteelErr};
use steel::rvals::SteelVal;
use steel::steel_vm::builtin::BuiltInModule;
use steel::steel_vm::engine::Engine;

/// Builtin modules no session engine keeps: processes, git, the file
/// system, sockets, HTTP, polling, and foreign libraries.
const CLOSED_MODULES: [&str; 7] = [
    "steel/process",
    "steel/git",
    "steel/filesystem",
    "steel/tcp",
    "steel/http",
    "steel/polling",
    "steel/ffi",
];

/// Functions closed inside modules that otherwise stay: engines without
/// these limits, loading and running files, the process's environment, and
/// its standard streams.
const CLOSED_FUNCTIONS: [&str; 21] = [
    // A thread a cell starts would run outside the cell's cancellation and
    // outlive the cell; in a process that hosts several sessions it would
    // also run outside the session's scope.
    "spawn-native-thread",
    "Engine::new",
    "Engine::clone",
    "Engine::add-module",
    "Engine::modules->list",
    "Engine::raise_error",
    "run!",
    "load",
    "load-expanded",
    "load-from-module!",
    "env-var",
    "maybe-get-env-var",
    "set-env-var!",
    "stdin",
    "stdout",
    "read-to-string",
    "open-input-file",
    "open-output-file",
    "#%get-dylib",
    "#%require-dylib",
    "current-directory",
];

/// Every builtin module name a Steel engine may hold. A module container
/// cannot be listed, so the ones to filter are named here.
const STEEL_MODULES: [&str; 39] = [
    "steel/base",
    "steel/bytevectors",
    "steel/constants",
    "steel/contracts",
    "steel/core/option",
    "steel/core/result",
    "steel/core/types",
    "steel/equality",
    "steel/ffi",
    "steel/filesystem",
    "steel/git",
    "steel/hash",
    "steel/hashes",
    "steel/http",
    "steel/identity",
    "steel/immutable-vectors",
    "steel/io",
    "steel/json",
    "steel/lists",
    "steel/meta",
    "steel/numbers",
    "steel/ord",
    "steel/polling",
    "steel/ports",
    "steel/process",
    "steel/random",
    "steel/sets",
    "steel/streams",
    "steel/strings",
    "steel/symbols",
    "steel/syntax",
    "steel/tcp",
    "steel/threads",
    "steel/time",
    "steel/transducers",
    "steel/vectors",
    "#%private/steel/mvector",
    "#%private/steel/primitives",
    "#%private/steel/reader",
];

/// Claims every module name Steel does not embed and has no source for it,
/// so a `require` of a file path fails instead of opening the file.
struct NoSourceFiles;

impl SourceModuleResolver for NoSourceFiles {
    fn resolve(&self, _key: &str) -> Option<String> {
        None
    }

    fn exists(&self, _key: &str) -> bool {
        true
    }
}

/// A new engine for a session, closed as the module docs describe and with
/// the builtins of [`super::limits`] guarded.
///
/// # Errors
///
/// A guarded builtin is not what the guard was written for: the engine is
/// not handed out with a hole in it.
pub(super) fn engine() -> anyhow::Result<Engine> {
    let mut engine = Engine::new_sandboxed();
    let mut closed: BTreeSet<String> = CLOSED_FUNCTIONS.iter().map(ToString::to_string).collect();
    for name in CLOSED_MODULES {
        if let Some(module) = engine.builtin_modules().get(name) {
            closed.extend(module.names());
        }
    }
    let mut replaced: BTreeMap<String, SteelVal> = closed
        .into_iter()
        .map(|name| (name, SteelVal::FuncV(refusal)))
        .collect();
    replaced.extend(super::limits::guards(&mut engine)?);
    // Each replaced name is bound to its replacement everywhere a cell could
    // find the builtin: in every module that has it, and in every global.
    for name in STEEL_MODULES {
        let Some(module) = engine.builtin_modules().get(name) else {
            continue;
        };
        if module
            .names()
            .iter()
            .any(|bound| replaced.contains_key(bound))
        {
            // A lookup of a name its module lacks panics inside Steel, so a
            // closed name stays bound, to the refusal.
            let mut kept = BuiltInModule::new(name);
            for (bound, value) in module.inner_map().iter() {
                let value = replaced.get(bound.as_ref()).unwrap_or(value).clone();
                kept.register_value(bound, value);
            }
            engine.register_module(kept);
        }
    }
    let globals: Vec<String> = engine
        .globals()
        .iter()
        .map(|name| name.resolve().to_string())
        .collect();
    for global in globals {
        if let Some(value) = builtin_names(&global)
            .into_iter()
            .find_map(|name| replaced.get(name))
        {
            // In place: Steel's own Scheme code, compiled before this, reads
            // the global by its slot, and a registration under the same name
            // would give the name a new one.
            if engine.update_value(&global, value.clone()).is_none() {
                anyhow::bail!("the Steel global {global} could not be rebound");
            }
        }
    }
    engine.register_source_module_resolver(NoSourceFiles);
    Ok(engine)
}

/// What a closed builtin is bound to.
pub(super) fn refusal(_arguments: &[SteelVal]) -> Result<SteelVal, SteelErr> {
    Err(SteelErr::new(
        ErrorKind::Generic,
        "this Steel builtin is closed in a session's kernel: read and write files with \
         fs-read, fs-write and fs-edit, find them with fs-glob, fs-find and fs-grep, and run \
         commands with bash"
            .to_string(),
    ))
}

/// The builtin a global is bound to, by the global's name: the name itself,
/// under Steel's `#%prim.` prefix, or inside one of Steel's own Scheme
/// modules (`##mm19__%#__reader.new-reader`), which bind the builtins they
/// require under an alias.
pub(super) fn builtin_names(global: &str) -> Vec<&str> {
    let unprefixed = global.strip_prefix("#%prim.").unwrap_or(global);
    let unmangled = unprefixed
        .split_once("__%#__")
        .map_or(unprefixed, |(_, name)| name);
    let mut names = vec![global, unprefixed, unmangled];
    names.extend(unmangled.split_once('.').map(|(_, name)| name));
    names
}

#[cfg(test)]
mod tests;
