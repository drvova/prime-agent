//! Call contracts: every kernel function refuses a call it does not declare,
//! before the function runs, with an error the model repairs from in one step.
//!
//! Steel binds `#:keyword` arguments by counting them: a call may pass any
//! keyword as long as it passes no more than the function declares, so a
//! misspelt option (`#:timout-ms`) binds nothing, the declared one keeps its
//! default, and the call runs as if the option were absent. Steel also takes
//! the first of a repeated keyword, and answers a keyword with no value with
//! an arity error that names neither.
//!
//! [`load`] is how the kernel's Scheme sources (and skills' sources) reach an
//! engine: it runs the source, then reads the same text with Steel's parser
//! and puts each public `(define (name ...) ...)` behind a check built from
//! its own parameter list. The accepted keywords are therefore the ones the
//! definition binds; no second list exists to drift from it. A refused call
//! reaches neither the function's defaults nor its body:
//!
//! - an option the function does not declare (with the nearest declared one
//!   when the name is close), the same option twice, an option with no value;
//! - too few or too many arguments before the options;
//! - a symbol where an optional argument is given: Steel's binding of
//!   optional arguments passes over every symbol, so `(rlm-collect 'worker)`
//!   collected every child.
//!
//! Exempt: a function with a rest parameter (`attach-image`, `api-help`,
//! and any skill function that reads its own rest list) takes whatever
//! follows its declared arguments; only its required arguments are counted.
//! Names starting with `%` are the kernel's own plumbing and are not wrapped,
//! and a function a cell defines for itself keeps Steel's own binding.
//!
//! The tools that take their options as a hash (`bash`, the `fs-*` tools)
//! check its keys in Rust against the table they read them from, and an MCP
//! tool's arguments are checked against its schema; both word their refusal
//! through [`undeclared`], and all three are counted through [`Refusals`].

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use serde_json::json;
use steel::parser::ast::ExprKind;
use steel::parser::parser::Parser;
use steel::parser::tokens::TokenType;
use steel::rerrs::{ErrorKind, SteelErr};
use steel::rvals::SteelVal;
use steel::steel_vm::engine::Engine;

use crate::kernel::call_refusals::CallRefusal;
use crate::kernel::shared::{HostHandlerFn, HostRequestHandlers, HostRequestPayload};

#[cfg(test)]
mod tests;

/// `(%contract function keywords required optional variadic usage inner)`:
/// `inner` behind its contract.
const WRAPPER: &str = "
(define (%contract function keywords required optional variadic usage inner)
  (lambda arguments
    (%contract-check function keywords required optional variadic usage arguments)
    (apply inner arguments)))
";

/// How a refused name is written back to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Spelling {
    /// A keyword argument, `#:name` (the name carries its `#:`).
    Keyword,
    /// A key of an options hash, `'name`.
    HashKey,
    /// An argument of an MCP tool, `"name"`.
    ToolArgument,
}

/// A call refused for a name its function does not declare: the sentence,
/// without the function's name in front.
#[derive(Debug)]
pub(super) struct Undeclared(String);

impl std::fmt::Display for Undeclared {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Undeclared {}

/// The refusal of `given`, which `function` does not declare: what it
/// accepts, and the accepted name nearest to `given` when one is close.
pub(super) fn undeclared(
    function: &str,
    spelling: Spelling,
    given: &str,
    accepted: &[&str],
) -> Undeclared {
    let (noun, written): (&str, fn(&str) -> String) = match spelling {
        Spelling::Keyword => ("option", str::to_string),
        Spelling::HashKey => ("option", |name| format!("'{name}")),
        Spelling::ToolArgument => ("argument", |name| format!("{name:?}")),
    };
    // Edit distance to each accepted name; the nearest is offered when it is
    // within a third of the longer name (and always within two edits).
    let nearest = accepted
        .iter()
        .map(|candidate| {
            let (from, to) = (given.as_bytes(), candidate.as_bytes());
            let mut row: Vec<usize> = (0..=to.len()).collect();
            for (at, letter) in from.iter().enumerate() {
                let mut diagonal = row[0];
                row[0] = at + 1;
                for (column, other) in to.iter().enumerate() {
                    let substituted = diagonal + usize::from(letter != other);
                    diagonal = row[column + 1];
                    row[column + 1] = substituted.min(diagonal + 1).min(row[column] + 1);
                }
            }
            (row[to.len()], *candidate)
        })
        .filter(|(distance, candidate)| *distance <= (given.len().max(candidate.len()) / 3).max(2))
        .min();
    let mut sentence = format!("unknown {noun} {}.", written(given));
    if let Some((_, candidate)) = nearest {
        let _ = write!(sentence, " Did you mean {}?", written(candidate));
    }
    if accepted.is_empty() {
        let _ = write!(sentence, " {function} takes no {noun}s");
    } else {
        let names: Vec<String> = accepted.iter().map(|name| written(name)).collect();
        let _ = write!(sentence, " {function} accepts {}", names.join(" "));
    }
    Undeclared(sentence)
}

/// Where a session's refused calls are counted: its `call.refused` host
/// request, sent without waiting for the reply.
#[derive(Clone)]
pub(super) struct Refusals {
    handler: Option<HostHandlerFn>,
    runtime: tokio::runtime::Handle,
}

impl Refusals {
    /// Count one refused call. Adoption telemetry: nothing waits for it, and
    /// a count the host refuses changes nothing here.
    pub(super) fn count(&self, refusal: CallRefusal) {
        if let Some(handler) = &self.handler {
            let counted = handler(HostRequestPayload {
                data: json!({ "type": "call.refused", "class": refusal.class() }),
            });
            self.runtime.spawn(async move {
                let _ = counted.await;
            });
        }
    }
}

/// Define the contract check in a session's `engine`, before any source is
/// [`load`]ed into it; refused calls are counted through `handlers`. Must be
/// called from within a tokio runtime.
///
/// # Errors
///
/// The Scheme definition failed to load.
pub(super) fn register(
    engine: &mut Engine,
    handlers: &HostRequestHandlers,
) -> anyhow::Result<Refusals> {
    let refusals = Refusals {
        handler: handlers.get("call.refused").cloned(),
        runtime: tokio::runtime::Handle::current(),
    };
    let counted = refusals.clone();
    engine.register_value(
        "%contract-check",
        SteelVal::anonymous_boxed_function(Arc::new(move |contract: &[SteelVal]| {
            let [SteelVal::StringV(function), SteelVal::ListV(keywords), SteelVal::IntV(required), SteelVal::IntV(optional), SteelVal::BoolV(variadic), SteelVal::StringV(usage), SteelVal::ListV(arguments)] =
                contract
            else {
                return Err(SteelErr::new(
                    ErrorKind::ArityMismatch,
                    "%contract-check takes a contract and an argument list".to_string(),
                ));
            };
            let (function, usage) = (function.as_str(), usage.as_str());
            let (mut positional, mut symbol) = (0_isize, None);
            let mut seen: Vec<&str> = Vec::new();
            let mut refusal = None;
            let mut given = arguments.iter();
            while let (Some(argument), None) = (given.next(), &refusal) {
                let named = if let SteelVal::SymbolV(name) = argument {
                    Some(name.as_str())
                } else {
                    None
                };
                // A rest parameter takes whatever follows.
                let Some(option) = named.filter(|name| !*variadic && name.starts_with("#:"))
                else {
                    symbol = symbol.or(named);
                    positional += 1;
                    continue;
                };
                let value = given.next();
                let declared = keywords.iter().any(
                    |keyword| matches!(keyword, SteelVal::SymbolV(keyword) if keyword.as_str() == option),
                );
                refusal = if !declared {
                    let accepted: Vec<&str> = keywords
                        .iter()
                        .filter_map(|keyword| {
                            if let SteelVal::SymbolV(keyword) = keyword {
                                Some(keyword.as_str())
                            } else {
                                None
                            }
                        })
                        .collect();
                    let mut sentence =
                        undeclared(function, Spelling::Keyword, option, &accepted).0;
                    if accepted.is_empty() {
                        // The tools that take a hash say how to pass this one.
                        if usage.contains("[options]") {
                            let _ = write!(
                                sentence,
                                " as keywords: its options are one hash, (hash '{} value), \
                                 passed as [options] in {usage}",
                                option.trim_start_matches("#:")
                            );
                        } else {
                            let _ = write!(sentence, ": {usage}");
                        }
                    }
                    Some((CallRefusal::UnknownOption, sentence))
                } else if seen.contains(&option) {
                    Some((
                        CallRefusal::RepeatedOption,
                        format!("option {option} is given twice: pass it once"),
                    ))
                } else if value.is_none() {
                    Some((
                        CallRefusal::OptionWithoutValue,
                        format!("option {option} has no value: write {option} followed by its value"),
                    ))
                } else {
                    seen.push(option);
                    None
                };
            }
            let most = required + optional;
            if refusal.is_none() && (positional < *required || (!*variadic && positional > most)) {
                let (expected, last) = if *variadic {
                    (format!("at least {required}"), *required)
                } else if *optional == 0 {
                    (format!("{required}"), most)
                } else {
                    (format!("{required} to {most}"), most)
                };
                refusal = Some((
                    CallRefusal::Arity,
                    format!(
                        "expected {expected} argument{} before any options, got {positional}: {usage}",
                        if last == 1 { "" } else { "s" }
                    ),
                ));
            }
            // Steel's binding of optional arguments passes over every
            // symbol, so the argument after one would silently get its
            // default.
            if let (None, Some(symbol), true) =
                (&refusal, symbol, !*variadic && positional > *required)
            {
                refusal = Some((
                    CallRefusal::Arity,
                    format!(
                        "the symbol '{symbol} cannot be passed in a call that gives an optional \
                         argument (it would be dropped): pass a string: {usage}"
                    ),
                ));
            }
            match refusal {
                None => Ok(SteelVal::Void),
                Some((class, sentence)) => {
                    counted.count(class);
                    Err(SteelErr::new(
                        ErrorKind::Generic,
                        format!("{function}: {sentence}"),
                    ))
                }
            }
        })),
    );
    engine
        .run(WRAPPER)
        .map_err(|error| anyhow::anyhow!("the Scheme call contracts failed to load: {error}"))?;
    Ok(refusals)
}

/// What one function's definition declares.
struct Contract {
    function: String,
    /// Each with its `#:`.
    keywords: Vec<String>,
    required: isize,
    optional: isize,
    variadic: bool,
    /// The call form: `(name required [optional] #:keyword . rest)`.
    usage: String,
}

/// The most sources whose contracts are kept: the kernel's own, and room for
/// the skills' as they are edited.
const SOURCES_KEPT: usize = 512;

/// The contracts of the sources read so far, by their text: the kernel's own
/// sources are parsed once in the process, not once per session.
static DECLARED: LazyLock<Mutex<HashMap<String, Arc<Vec<Contract>>>>> =
    LazyLock::new(Mutex::default);

/// Run `source` in `engine`, then put each public function it defines at the
/// top level (`(define (name ...) ...)`, names not starting with `%`) behind
/// the contract its parameter list declares.
///
/// # Errors
///
/// The source failed to run.
pub(super) fn load(engine: &mut Engine, source: &str) -> Result<(), SteelErr> {
    engine.run(source.to_string())?;
    let known = DECLARED
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(source)
        .cloned();
    let contracts = if let Some(known) = known {
        known
    } else {
        // The last definition of a name is the one the engine holds.
        let mut contracts: BTreeMap<String, Contract> = BTreeMap::new();
        for form in Parser::parse(source)? {
            let ExprKind::Define(define) = form else {
                continue;
            };
            let (Some(name), ExprKind::LambdaFunction(function)) =
                (define.name.atom_identifier(), &define.body)
            else {
                continue;
            };
            let name = name.resolve();
            if name.starts_with('%') {
                continue;
            }
            let mut contract = Contract {
                function: name.to_string(),
                keywords: Vec::new(),
                required: 0,
                optional: 0,
                variadic: false,
                usage: format!("({name}"),
            };
            let mut parameters = function.args.iter().peekable();
            while let Some(parameter) = parameters.next() {
                if let Some(TokenType::Keyword(keyword)) =
                    parameter.atom_syntax_object().map(|syntax| &syntax.ty)
                {
                    // The keyword's binding and default follow it.
                    parameters.next();
                    contract.keywords.push(keyword.resolve().to_string());
                    let _ = write!(contract.usage, " {keyword}");
                } else if function.rest && parameters.peek().is_none() {
                    contract.variadic = true;
                    let _ = write!(contract.usage, " . {parameter}");
                } else if let ExprKind::List(default) = parameter {
                    contract.optional += 1;
                    let _ = match default.first() {
                        Some(binding) => write!(contract.usage, " [{binding}]"),
                        None => write!(contract.usage, " []"),
                    };
                } else {
                    contract.required += 1;
                    let _ = write!(contract.usage, " {parameter}");
                }
            }
            contract.usage.push(')');
            contracts.insert(name.to_string(), contract);
        }
        let contracts = Arc::new(contracts.into_values().collect::<Vec<_>>());
        let mut declared = DECLARED.lock().unwrap_or_else(PoisonError::into_inner);
        if declared.len() < SOURCES_KEPT {
            declared.insert(source.to_string(), Arc::clone(&contracts));
        }
        contracts
    };
    // Each function's global is rebound in place to the function behind its
    // check: nothing is compiled, and every caller reaches the check.
    for contract in contracts.iter() {
        let wrapper = engine.extract_value("%contract")?;
        let checked = engine.call_function_with_args(
            wrapper,
            vec![
                SteelVal::StringV(contract.function.as_str().into()),
                SteelVal::ListV(
                    contract
                        .keywords
                        .iter()
                        .map(|keyword| SteelVal::SymbolV(keyword.as_str().into()))
                        .collect(),
                ),
                SteelVal::IntV(contract.required),
                SteelVal::IntV(contract.optional),
                SteelVal::BoolV(contract.variadic),
                SteelVal::StringV(contract.usage.as_str().into()),
                engine.extract_value(&contract.function)?,
            ],
        )?;
        engine.update_value(&contract.function, checked);
    }
    Ok(())
}
