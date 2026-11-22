//! Limits on what one cell can cost the process that hosts its session.
//!
//! A process that hosts many sessions dies whole when one thread runs off
//! the end of its stack or the allocator is refused memory: neither is a
//! panic, so neither can be contained after the fact. Steel's parser,
//! compiler, JSON conversion, hashing and destructors recurse on the native
//! stack once per level of nesting, and several builtins allocate whatever a
//! number tells them to. What this module does about it:
//!
//! - **A known, large stack.** A cell runs on a thread of its own
//!   ([`cell_stack_bytes`], 1 GiB where the system grants it). Measured on
//!   that thread, a level of nesting costs about 12 KiB in the compiler,
//!   3 KiB in a builtin that calls back into Scheme (Steel refuses those at
//!   131,072 levels: 400 MiB), 1.4 KiB in the JSON conversion, 740 bytes in
//!   the reader, and about 120 bytes when a nested value is hashed or
//!   dropped. A runtime thread's 2 MiB ends at 172 nested calls in source.
//! - **Depth is refused first** wherever text or data enters something that
//!   recurses and the kernel can see it: a cell's source and every string
//!   given to `eval-string` or `read` ([`SOURCE_DEPTH`]); a value given to
//!   `eval` (the same limit); a value that crosses to the host as JSON
//!   ([`DATA_DEPTH`], which is also the deepest JSON the host can send back).
//! - **Size is refused first** for the builtins that allocate by a count
//!   ([`COUNT_LIMIT`]), and for a cell's source ([`SOURCE_BYTES`]).
//!
//! What stays unbounded is stated in [`guards`]' table and the tests beside
//! it: a value nested millions deep can still be built one level at a time,
//! and hashing or dropping it recurses to its depth (the 1 GiB stack holds
//! about 8 million levels, which is over 500 MiB of values); and nothing
//! here bounds the total memory a cell's values take.

use std::sync::{Arc, OnceLock};

use anyhow::{anyhow, bail};
use serde_json::Value;
use steel::rerrs::{ErrorKind, SteelErr};
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;

/// The deepest nesting of brackets and quotes in source text handed to
/// Steel's reader and compiler. The compiler's 12 KiB a level makes this
/// 12 MiB of stack, a fifth of the smallest stack a cell is given.
pub(super) const SOURCE_DEPTH: usize = 1_000;

/// The longest source one cell may be. Steel compiles a cell in one
/// uninterruptible step whose time grows faster than its input.
pub(super) const SOURCE_BYTES: usize = 1 << 20;

/// The deepest nesting of a value that crosses to the host as JSON: the
/// depth `serde_json` reads back, so anything deeper could never return.
pub(super) const DATA_DEPTH: usize = 128;

/// The largest count a builtin that allocates by its argument is given
/// (`make-string`, `make-vector`, `range`, `expt`): 16,777,216 elements,
/// at most a few hundred megabytes, where an unchecked count is a request
/// the allocator answers by ending the process.
pub(super) const COUNT_LIMIT: isize = 1 << 24;

/// The stacks tried for a cell's thread, largest first. They are address
/// space: only the pages a cell touches become memory, and the thread's end
/// returns them.
const STACKS: [usize; 5] = [1 << 30, 1 << 29, 1 << 28, 1 << 27, 1 << 26];

/// The stack a cell's thread gets: the largest of [`STACKS`] the system
/// grants a thread, found once.
pub(super) fn cell_stack_bytes() -> usize {
    static GRANTED: OnceLock<usize> = OnceLock::new();
    *GRANTED.get_or_init(|| {
        STACKS
            .into_iter()
            .find(|bytes| {
                std::thread::Builder::new()
                    .name("scheme-stack-probe".to_string())
                    .stack_size(*bytes)
                    .spawn(|| ())
                    .is_ok_and(|probe| probe.join().is_ok())
            })
            // No thread at all: the cell's own spawn reports it.
            .unwrap_or(STACKS[STACKS.len() - 1])
    })
}

/// Why `source` may not be read or compiled, when it may not: it is nested
/// deeper than [`SOURCE_DEPTH`]. A quote counts as the level it adds.
pub(super) fn source_refusal(source: &str) -> Option<String> {
    // What each open bracket added: itself and the quotes before it.
    let mut open: Vec<usize> = Vec::new();
    let (mut depth, mut quotes) = (0_usize, 0_usize);
    let mut letters = source.chars().peekable();
    while let Some(letter) = letters.next() {
        match letter {
            '(' | '[' | '{' => {
                open.push(1 + quotes);
                depth += 1 + quotes;
                quotes = 0;
            }
            ')' | ']' | '}' => depth -= open.pop().unwrap_or(0),
            '\'' | '`' | ',' => quotes += 1,
            '"' => {
                quotes = 0;
                while let Some(inside) = letters.next() {
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
                letters.by_ref().find(|inside| *inside == '\n');
            }
            '#' if letters.peek() == Some(&'\\') => {
                // A character literal: `#\(` opens nothing.
                quotes = 0;
                letters.next();
                letters.next();
            }
            '#' if letters.peek() == Some(&'|') => {
                let mut last = ' ';
                letters.by_ref().find(|inside| {
                    let ends = last == '|' && *inside == '#';
                    last = *inside;
                    ends
                });
            }
            space if space.is_whitespace() => {}
            _ => quotes = 0,
        }
        if depth + quotes > SOURCE_DEPTH {
            return Some(format!(
                "the code is nested deeper than {SOURCE_DEPTH} levels, which the kernel does not \
                 compile: build a deeply nested value with a loop instead of writing it out"
            ));
        }
    }
    None
}

/// Whether `value` nests lists, vectors, pairs, hashes or sets more than
/// `levels` deep. It recurses at most `levels` times itself.
fn nests_deeper_than(value: &SteelVal, levels: usize) -> bool {
    let inside = |mut items: &mut dyn Iterator<Item = &SteelVal>| {
        levels == 0 || Iterator::any(&mut items, |item| nests_deeper_than(item, levels - 1))
    };
    match value {
        SteelVal::ListV(items) => inside(&mut items.iter()),
        SteelVal::VectorV(items) => inside(&mut items.iter()),
        SteelVal::HashSetV(items) => inside(&mut items.iter()),
        SteelVal::HashMapV(fields) => {
            inside(&mut fields.iter().flat_map(|(key, value)| [key, value]))
        }
        SteelVal::Pair(pair) => inside(&mut [pair.car_ref(), pair.cdr_ref()].into_iter()),
        // Nothing below is walked by the JSON conversion or by `eval`'s
        // reading of a value as code: each is a leaf or refused there.
        SteelVal::Closure(_)
        | SteelVal::BoolV(_)
        | SteelVal::NumV(_)
        | SteelVal::IntV(_)
        | SteelVal::Rational(_)
        | SteelVal::CharV(_)
        | SteelVal::Void
        | SteelVal::StringV(_)
        | SteelVal::FuncV(_)
        | SteelVal::SymbolV(_)
        | SteelVal::Custom(_)
        | SteelVal::CustomStruct(_)
        | SteelVal::PortV(_)
        | SteelVal::IterV(_)
        | SteelVal::ReducerV(_)
        | SteelVal::FutureFunc(_)
        | SteelVal::FutureV(_)
        | SteelVal::StreamV(_)
        | SteelVal::BoxedFunction(_)
        | SteelVal::ContinuationFunction(_)
        | SteelVal::MutFunc(_)
        | SteelVal::BuiltIn(_)
        | SteelVal::MutableVector(_)
        | SteelVal::BoxedIterator(_)
        | SteelVal::SyntaxObject(_)
        | SteelVal::Boxed(_)
        | SteelVal::HeapAllocated(_)
        | SteelVal::Reference(_)
        | SteelVal::BigNum(_)
        | SteelVal::BigRational(_)
        | SteelVal::Complex(_)
        | SteelVal::ByteVector(_) => false,
    }
}

/// `value` as the JSON it crosses to the host as.
///
/// # Errors
///
/// It nests deeper than [`DATA_DEPTH`], or holds what JSON cannot.
pub(super) fn plain_data(value: SteelVal) -> Result<Value, SteelErr> {
    Check::Data.run(std::slice::from_ref(&value))?;
    Value::try_from(value)
}

/// What a guarded builtin's arguments are checked for before it runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Check {
    /// Values that become JSON: at most [`DATA_DEPTH`] deep.
    Data,
    /// Values read as code: at most [`SOURCE_DEPTH`] deep.
    Code,
    /// Strings read as source: at most [`SOURCE_DEPTH`] deep.
    Source,
    /// A count to allocate by: every integer at most [`COUNT_LIMIT`].
    Count,
}

impl Check {
    fn run(self, arguments: &[SteelVal]) -> Result<(), SteelErr> {
        let refusal = match self {
            Check::Data => arguments
                .iter()
                .any(|value| nests_deeper_than(value, DATA_DEPTH))
                .then(|| {
                    format!(
                        "the value is nested deeper than {DATA_DEPTH} levels, which JSON between \
                         the kernel and its host does not carry"
                    )
                }),
            Check::Code => arguments
                .iter()
                .any(|value| nests_deeper_than(value, SOURCE_DEPTH))
                .then(|| {
                    format!(
                        "the code is nested deeper than {SOURCE_DEPTH} levels, which the kernel \
                         does not compile"
                    )
                }),
            Check::Source => arguments.iter().find_map(|value| match value {
                SteelVal::StringV(source) => source_refusal(source.as_str()),
                _ => None,
            }),
            Check::Count => arguments
                .iter()
                .any(|value| match value {
                    SteelVal::IntV(count) => *count > COUNT_LIMIT,
                    SteelVal::NumV(count) => *count > COUNT_LIMIT as f64,
                    SteelVal::BigNum(_) => true,
                    _ => false,
                })
                .then(|| {
                    format!(
                        "the count is over {COUNT_LIMIT}, the most elements the kernel builds in \
                         one call: build it in pieces, or keep the data in a file"
                    )
                }),
        };
        refusal.map_or(Ok(()), |reason| {
            Err(SteelErr::new(ErrorKind::Generic, reason))
        })
    }
}

/// The Steel builtins that recurse or allocate by what they are given, and
/// what each is checked for. Everything else a cell can reach either does
/// neither, or is listed as unbounded in the module docs.
pub(super) const GUARDED: [(&str, Check); 13] = [
    ("value->jsexpr-string", Check::Data),
    ("eval", Check::Code),
    ("eval!", Check::Code),
    ("expand!", Check::Code),
    ("eval-string", Check::Source),
    ("reader-push-string", Check::Source),
    ("make-string", Check::Count),
    ("make-vector", Check::Count),
    ("make-bytes", Check::Count),
    ("make-bytevector", Check::Count),
    ("range", Check::Count),
    ("expt", Check::Count),
    ("arithmetic-shift", Check::Count),
];

/// The replacement of each of [`GUARDED`] that `engine` has: the builtin
/// behind its check. A builtin that needs the running machine (`eval`) is
/// wrapped in Scheme, any other in Rust.
///
/// # Errors
///
/// A builtin is of a kind no wrapper is written for.
pub(super) fn guards(engine: &mut Engine) -> anyhow::Result<Vec<(String, SteelVal)>> {
    // `(%guard check wrapped)`: a function that checks its arguments, then
    // calls `wrapped` with them.
    let in_scheme = engine
        .run("(lambda (check wrapped) (lambda arguments (check arguments) (apply wrapped arguments)))")
        .map_err(|error| anyhow!("the guard for Steel's builtins failed to load: {error}"))?
        .pop()
        .ok_or_else(|| anyhow!("the guard for Steel's builtins is not a function"))?;
    let mut guarded = Vec::new();
    for (name, check) in GUARDED {
        let Ok(builtin) = engine.extract_value(name).or_else(|_| {
            // Not a global: one of Steel's own modules holds it.
            engine.extract_value(&format!("#%prim.{name}"))
        }) else {
            match STEEL_PRIVATE.iter().find_map(|module| {
                let module = engine.builtin_modules().get(module)?;
                module.contains(name).then(|| module.get(name.to_string()))
            }) {
                Some(builtin) => guarded.push((name.to_string(), natively(name, check, builtin)?)),
                // This Steel has no such builtin.
                None => {}
            }
            continue;
        };
        let wrapper = match builtin {
            SteelVal::BuiltIn(_) => engine
                .call_function_with_args(
                    in_scheme.clone(),
                    vec![
                        SteelVal::anonymous_boxed_function(Arc::new(
                            move |arguments: &[SteelVal]| match arguments {
                                [SteelVal::ListV(arguments)] => {
                                    let arguments: Vec<SteelVal> =
                                        arguments.iter().cloned().collect();
                                    check.run(&arguments).map(|()| SteelVal::Void)
                                }
                                _ => Err(SteelErr::new(
                                    ErrorKind::ArityMismatch,
                                    "a guard takes its builtin's argument list".to_string(),
                                )),
                            },
                        )),
                        builtin,
                    ],
                )
                .map_err(|error| anyhow!("the guard for {name} could not be made: {error}"))?,
            other => natively(name, check, other)?,
        };
        guarded.push((name.to_string(), wrapper));
    }
    Ok(guarded)
}

/// Steel's modules that hold builtins no global names.
const STEEL_PRIVATE: [&str; 1] = ["#%private/steel/reader"];

/// `builtin` behind `check`, for a builtin that is a plain Rust function.
fn natively(name: &str, check: Check, builtin: SteelVal) -> anyhow::Result<SteelVal> {
    type Plain = Arc<dyn Fn(&[SteelVal]) -> Result<SteelVal, SteelErr> + Send + Sync>;
    let call: Plain = match builtin {
        SteelVal::FuncV(function) => Arc::new(function),
        SteelVal::BoxedFunction(boxed) => Arc::clone(&boxed.function),
        SteelVal::MutFunc(function) => {
            Arc::new(move |arguments: &[SteelVal]| function(&mut arguments.to_vec()))
        }
        other => bail!("the builtin {name} is {other:?}, which the kernel has no guard for"),
    };
    Ok(SteelVal::anonymous_boxed_function(Arc::new(
        move |arguments: &[SteelVal]| {
            check.run(arguments)?;
            call(arguments)
        },
    )))
}

#[cfg(test)]
mod tests;
