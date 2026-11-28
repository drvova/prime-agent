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
//! - The engine Steel's compiler keeps for macros, where the body of a
//!   `defmacro`, of a `syntax-case` transformer and of `begin-for-syntax`
//!   runs, is closed the same way ([`close`] runs on both), prints to its
//!   cell's ports, and is interrupted with the session's ([`controllers`]).
//!   Steel's own macros (`struct`, parameters) need none of what is closed.
//!
//! - Steel builds every engine from one image, and the image's modules,
//!   documentation and memo tables, will executor and its flag are one
//!   object in all of them. Each engine gets its own, and the builtins that
//!   write into a module, force a collection or return the process's
//!   arguments are closed: nothing a cell sets in Steel's own state is read
//!   in another session.
//!
//! What stays open: `display` and the string ports its capture uses (the
//! builtins that print past every port, and the process's standard output
//! and error ports, are closed), `eval`
//! (it compiles in the same closed engine), time, and the pure library. The
//! builtins that wait where no interrupt reaches are closed too, and
//! `time/sleep-ms` is replaced by one that ends with its cell
//! ([`super::limits::sleep`]). Each cell reads from an empty input port, not
//! the process's standard input. `will-register` stays: no will ever runs
//! (`will-execute` and the thread that would call it are closed), and what
//! is registered is held by the engine's own executor and goes with it.
//! `confinement/open_globals.txt` lists every
//! name a cell can reach, and the test beside it fails on one it does not
//! list.

use std::collections::{BTreeMap, BTreeSet};

use steel::compiler::modules::SourceModuleResolver;
use steel::rerrs::{ErrorKind, SteelErr};
use steel::rvals::{IntoSteelVal as _, SteelVal};
use steel::steel_vm::builtin::BuiltInModule;
use steel::steel_vm::cache::WeakMemoizationTable;
use steel::steel_vm::engine::Engine;
use steel::steel_vm::ThreadStateController;
use steel::LambdaMetadataTable;

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
const CLOSED_FUNCTIONS: [&str; 52] = [
    // Steel's debugger stop, and the value (de)serialization its threads
    // use: a cell has no use for either, and the second reads bytes a cell
    // made up as Steel's own structures.
    "breakpoint!",
    "serialize-value",
    "deserialize-value",
    // Each of these waits in native code where no interrupt reaches, for
    // something only another thread could deliver, and a cell has no other
    // thread: a channel's next message, a lock it already holds, a will, a
    // future. A cell that called one could only be abandoned.
    "channel/recv",
    "receivers-select",
    "lock-acquire!",
    "will-execute",
    "thread-join!",
    "thread-suspend",
    "block-on",
    "local-executor/block-on",
    "futures-join-all",
    "join!",
    "poll!",
    // The process's own standard streams: a cell's input port is an empty
    // one, and its output and error ports are its own. In a daemon these
    // are the daemon's log, shared by every session; in the headless modes
    // standard output is the protocol stream.
    "#%default-input-port",
    "#%default-output-port",
    "#%default-error-port",
    // Each of these prints with the process's `print!`, past every port:
    // Steel's port-less display, its documentation and markdown printers,
    // and its debugging dumps.
    "stdout-simple-displayln",
    "%string->render-markdown",
    "#%lookup-doc",
    "#%native-fn-ptr-doc",
    "inspect",
    "debug-globals",
    "dump-profiler",
    "#%debug-syntax->exprkind",
    // Builds a native library with cargo.
    "#%build-dylib",
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
    // The arguments the agent process was started with.
    "command-line",
    // These act on what every engine of the process shares. A module is
    // the same object in each engine Steel builds, so a name added to one
    // was read in every session. A collection is the engine's own business:
    // asked for, it doubles the heap's free list about every tenth call.
    "#%module-add",
    "#%module-add-doc",
    "#%gc-collect",
    // Steel never finished it: the first value kept for a function is a
    // `todo!()`, which would take the session down.
    "%memo-table-set!",
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
/// the builtins of [`super::limits`] guarded, and so is the engine its
/// macro bodies run in.
///
/// # Errors
///
/// A guarded builtin is not what the guard was written for, or Steel built
/// the engine without one for its macros: the engine is not handed out with
/// a hole in it.
pub(super) fn engine() -> anyhow::Result<Engine> {
    let mut engine = Engine::new_sandboxed();
    close(&mut engine)?;
    engine.register_source_module_resolver(NoSourceFiles);
    engine
        .with_macro_engine(|macros| {
            close(macros)?;
            // There `displayln` was the builtin that prints past every port,
            // closed above with it: a macro body's `displayln` writes to the
            // current port, as a cell's does.
            macros
                .run("(set! displayln simple-displayln)")
                .map_err(|error| anyhow::anyhow!("the macro engine's displayln: {error}"))?;
            Ok::<(), anyhow::Error>(())
        })
        .ok_or_else(|| anyhow::anyhow!("the Steel engine has no engine for its macros"))??;
    Ok(engine)
}

/// What interrupts `engine`: its own controller, and that of the engine its
/// macro bodies run in. A body that loops is on the second, which an
/// interrupt of the first never reaches.
pub(super) fn controllers(engine: &mut Engine) -> Vec<ThreadStateController> {
    let own = engine.get_thread_state_controller();
    std::iter::once(own)
        .chain(engine.with_macro_engine(|macros| macros.get_thread_state_controller()))
        .collect()
}

/// Close `engine`: one of the two Steel engines a session's code runs in.
fn close(engine: &mut Engine) -> anyhow::Result<()> {
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
    replaced.extend(super::limits::guards(engine)?);
    replaced.insert(
        "time/sleep-ms".to_string(),
        SteelVal::FuncV(super::limits::sleep),
    );
    // Steel builds every engine from one image, so what its library made
    // as the image was built is one object in all of them. Each engine gets
    // its own of the ones a cell can change: the two tables here, and below
    // the modules, the will executor, and the box that says it runs.
    for (name, own) in [
        (
            "#%function-ptr-table",
            LambdaMetadataTable::new().into_steelval(),
        ),
        ("%memo-table", WeakMemoizationTable::new().into_steelval()),
    ] {
        let own = own.map_err(|error| anyhow::anyhow!("the engine's own {name}: {error}"))?;
        replaced.insert(name.to_string(), own);
    }
    // Each replaced name is bound to its replacement everywhere a cell could
    // find the builtin: in every module that has it, and in every global.
    // A global may hold the builtin under another name (Steel's macro engine
    // has `(define displayln stdout-simple-displayln)`), so the builtins are
    // found by what they are too.
    let mut aliased: BTreeMap<usize, SteelVal> = BTreeMap::new();
    for (global, own) in [
        ("#%will-executor-running", "(box #f)"),
        ("#%global-will-executor", "(#%prim.make-will-executor)"),
    ] {
        if let Some(shared) = engine
            .extract_value(global)
            .ok()
            .as_ref()
            .and_then(identity)
        {
            let own = engine
                .run(own)
                .map_err(|error| anyhow::anyhow!("the engine's own {global}: {error}"))?
                .pop()
                .ok_or_else(|| anyhow::anyhow!("the engine's own {global} is no value"))?;
            aliased.insert(shared, own);
        }
    }
    for name in STEEL_MODULES {
        let Some(module) = engine.builtin_modules().get(name) else {
            continue;
        };
        // Every module is rebuilt, replaced names or not: the image's module
        // is the same object in every engine. A lookup of a name its module
        // lacks panics inside Steel, so a closed name stays bound, to the
        // refusal.
        let mut kept = BuiltInModule::new(name);
        for (bound, value) in module.inner_map().iter() {
            let replacement = replaced.get(bound.as_ref());
            if let (Some(replacement), Some(builtin)) = (replacement, identity(value)) {
                aliased.insert(builtin, replacement.clone());
            }
            kept.register_value(bound, replacement.unwrap_or(value).clone());
        }
        engine.register_module(kept);
    }
    let globals: Vec<String> = engine
        .globals()
        .iter()
        .map(|name| name.resolve().to_string())
        .collect();
    for global in globals {
        let named = builtin_names(&global)
            .into_iter()
            .find_map(|name| replaced.get(name));
        let bound = engine.extract_value(&global).ok();
        if let Some(value) = named.or_else(|| aliased.get(&bound.as_ref().and_then(identity)?)) {
            // In place: Steel's own Scheme code, compiled before this, reads
            // the global by its slot, and a registration under the same name
            // would give the name a new one.
            if engine.update_value(&global, value.clone()).is_none() {
                anyhow::bail!("the Steel global {global} could not be rebound");
            }
        }
    }
    // A new engine's current ports are the process's standard streams. Every
    // cell installs its own, but nothing run on the engine before or between
    // cells (a skill's source as it loads) may hold the process's either.
    engine
        .run("(current-output-port (open-output-string)) (current-error-port (open-output-string))")
        .map_err(|error| anyhow::anyhow!("the engine's ports could not be replaced: {error}"))?;
    Ok(())
}

/// What tells one of Steel's own functions, native objects or boxes from
/// another, whatever name it is bound to: where its code or its allocation
/// is. Nothing else is rebound by what it is.
fn identity(value: &SteelVal) -> Option<usize> {
    match value {
        SteelVal::FuncV(function) => Some(*function as usize),
        SteelVal::MutFunc(function) => Some(*function as usize),
        SteelVal::BuiltIn(function) => Some(*function as usize),
        SteelVal::BoxedFunction(function) => Some(function.as_ptr() as usize),
        SteelVal::Custom(_) | SteelVal::HeapAllocated(_) => value.as_ptr_usize(),
        SteelVal::Closure(_)
        | SteelVal::BoolV(_)
        | SteelVal::NumV(_)
        | SteelVal::IntV(_)
        | SteelVal::Rational(_)
        | SteelVal::CharV(_)
        | SteelVal::Void
        | SteelVal::StringV(_)
        | SteelVal::SymbolV(_)
        | SteelVal::CustomStruct(_)
        | SteelVal::PortV(_)
        | SteelVal::IterV(_)
        | SteelVal::ReducerV(_)
        | SteelVal::FutureFunc(_)
        | SteelVal::FutureV(_)
        | SteelVal::StreamV(_)
        | SteelVal::ContinuationFunction(_)
        | SteelVal::BoxedIterator(_)
        | SteelVal::SyntaxObject(_)
        | SteelVal::Boxed(_)
        | SteelVal::Reference(_)
        | SteelVal::BigNum(_)
        | SteelVal::BigRational(_)
        | SteelVal::Complex(_)
        | SteelVal::ByteVector(_)
        | SteelVal::ListV(_)
        | SteelVal::VectorV(_)
        | SteelVal::HashSetV(_)
        | SteelVal::HashMapV(_)
        | SteelVal::Pair(_)
        | SteelVal::MutableVector(_) => None,
    }
}

/// What a closed builtin is bound to.
pub(super) fn refusal(_arguments: &[SteelVal]) -> Result<SteelVal, SteelErr> {
    Err(SteelErr::new(
        ErrorKind::Generic,
        "this Steel builtin is closed in a session's kernel: read and write files with \
         fs-read, fs-write and fs-edit, find them with fs-find and fs-grep, and run \
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
