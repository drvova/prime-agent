//! The helper's `eval` gate. A cell is read as data and never evaluated: it is accepted only
//! when every top-level form is one call of a file reader with literal arguments, and each
//! accepted call then runs through the kernel's own readers ([`FileReads`]).
//!
//! Accepted forms, one or more per cell:
//!
//! - `(fs-read "path" [options])`, `(fs-grep "pattern" [options])`, `(fs-find [options])`,
//!   `(fs-symbol "name" [options])`, where `options` is
//!   `(hash 'key value ...)` and each value is a string, an integer, `#t`/`#f`, or a `(list ...)`
//!   of those;
//! - one of those inside `(displayln ...)`, `(display ...)`, or `(hash-ref ... 'text)`, the ways a
//!   session prints a reader's result.
//!
//! Everything else is refused with the reason: a definition, a variable, a call of any other
//! function (so no `bash`, `fs-edit`, `fs-write`, or Steel builtin), quasiquotation, and any `#`
//! syntax but the booleans.

use serde_json::{Map, Value};

use super::{capped, ReadContext};
use crate::kernel::scheme::FileReads;

#[cfg(test)]
mod tests;

pub(super) const EVAL_ACCEPTS: &str = "eval in this read-only helper accepts only file reads \
     with literal arguments, one call per form, e.g. `(displayln (hash-ref (fs-read \
     \"src/lib.rs\" (hash 'start 1 'end 80)) 'text))`, `(fs-grep \"fn parse\" (hash 'path \
     \"src\" 'glob \"*.rs\"))`, `(fs-find (hash 'glob \"**/*.toml\"))`, `(fs-find (hash \
     'name \"Makefile\"))`, `(fs-symbol \"parse\")`.";

/// One piece of a cell, as read.
#[derive(Debug, Clone, PartialEq)]
enum Datum {
    List(Vec<Datum>),
    Str(String),
    Int(i64),
    Bool(bool),
    Symbol(String),
    Quoted(Box<Datum>),
}

/// The readers a cell may call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsOp {
    Read,
    Grep,
    Find,
    Symbol,
}

impl FsOp {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "fs-read" => Some(Self::Read),
            "fs-grep" => Some(Self::Grep),
            "fs-find" => Some(Self::Find),
            "fs-symbol" => Some(Self::Symbol),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Read => "fs-read",
            Self::Grep => "fs-grep",
            Self::Find => "fs-find",
            Self::Symbol => "fs-symbol",
        }
    }

    /// The option the call's leading argument fills, when it takes one.
    fn positional(self) -> Option<&'static str> {
        match self {
            Self::Read => Some("path"),
            Self::Grep => Some("pattern"),
            Self::Find => None,
            Self::Symbol => Some("name"),
        }
    }
}

/// One accepted reader call: the options its Scheme function would hand the kernel.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FsCall {
    pub(crate) op: FsOp,
    options: Map<String, Value>,
}

/// Reads a cell's text into data.
struct Reader<'a> {
    rest: std::iter::Peekable<std::str::Chars<'a>>,
}

impl Reader<'_> {
    /// Skip blanks and `;` comments; `false` at the end of the cell.
    fn at_datum(&mut self) -> bool {
        loop {
            match self.rest.peek() {
                None => return false,
                Some(';') => {
                    for c in self.rest.by_ref() {
                        if c == '\n' {
                            break;
                        }
                    }
                }
                Some(c) if c.is_whitespace() => {
                    self.rest.next();
                }
                Some(_) => return true,
            }
        }
    }

    fn datum(&mut self) -> Result<Datum, String> {
        if !self.at_datum() {
            return Err("a form is not closed".to_string());
        }
        match self.rest.next() {
            Some(open @ ('(' | '[')) => {
                let close = if open == '(' { ')' } else { ']' };
                let mut items = Vec::new();
                loop {
                    if !self.at_datum() {
                        return Err("a form is not closed".to_string());
                    }
                    if self.rest.peek() == Some(&close) {
                        self.rest.next();
                        return Ok(Datum::List(items));
                    }
                    items.push(self.datum()?);
                }
            }
            Some(close @ (')' | ']')) => Err(format!("`{close}` closes nothing")),
            Some('\'') => Ok(Datum::Quoted(Box::new(self.datum()?))),
            Some('"') => {
                let mut text = String::new();
                loop {
                    match self.rest.next() {
                        None => return Err("a string is not closed".to_string()),
                        Some('"') => return Ok(Datum::Str(text)),
                        Some('\\') => match self.rest.next() {
                            Some(escaped @ ('\\' | '"')) => text.push(escaped),
                            Some('n') => text.push('\n'),
                            Some('t') => text.push('\t'),
                            Some(other) => {
                                return Err(format!(
                                    "the string escape `\\{other}` is not accepted"
                                ));
                            }
                            None => return Err("a string is not closed".to_string()),
                        },
                        Some(c) => text.push(c),
                    }
                }
            }
            Some(mark @ ('`' | ',')) => Err(format!("`{mark}` is not accepted here")),
            Some(first) => {
                let mut word = String::from(first);
                while let Some(c) = self.rest.peek().copied() {
                    if c.is_whitespace() || "()[]\"';`,".contains(c) {
                        break;
                    }
                    word.push(c);
                    self.rest.next();
                }
                match word.as_str() {
                    "#t" | "#true" => Ok(Datum::Bool(true)),
                    "#f" | "#false" => Ok(Datum::Bool(false)),
                    word if word.starts_with('#') => Err(format!("`{word}` is not accepted here")),
                    word => Ok(word
                        .parse::<i64>()
                        .map_or_else(|_| Datum::Symbol(word.to_string()), Datum::Int)),
                }
            }
            None => Err("a form is not closed".to_string()),
        }
    }
}

/// A literal argument as the JSON the kernel's readers take.
fn literal(datum: &Datum) -> Result<Value, String> {
    match datum {
        Datum::Str(text) => Ok(Value::from(text.clone())),
        Datum::Int(number) => Ok(Value::from(*number)),
        Datum::Bool(flag) => Ok(Value::from(*flag)),
        Datum::List(items) => match items.split_first() {
            Some((Datum::Symbol(head), values)) if head == "list" => values
                .iter()
                .map(literal)
                .collect::<Result<Vec<_>, _>>()
                .map(Value::from),
            _ => Err("only literal arguments are accepted, and a list is `(list ...)`".to_string()),
        },
        Datum::Quoted(quoted) => match quoted.as_ref() {
            Datum::List(items) if items.is_empty() => Ok(Value::Array(Vec::new())),
            _ => Err("a quoted value is accepted only as an option's name".to_string()),
        },
        Datum::Symbol(name) => Err(format!(
            "`{name}` is a name, and only literal arguments are accepted"
        )),
    }
}

/// One top-level form as the reader call it is, or why it is refused.
fn call(form: &Datum) -> Result<FsCall, String> {
    const SHAPE: &str = "each form must be one fs-read/fs-grep/fs-find/fs-symbol call";
    let Datum::List(items) = form else {
        return Err(SHAPE.to_string());
    };
    let Some((Datum::Symbol(head), arguments)) = items.split_first() else {
        return Err(SHAPE.to_string());
    };
    let Some(op) = FsOp::from_name(head) else {
        return match (head.as_str(), arguments) {
            ("display" | "displayln", [printed]) => call(printed),
            ("hash-ref", [result, Datum::Quoted(key)]) if matches!(**key, Datum::Symbol(_)) => {
                call(result)
            }
            (name, _) if name.starts_with("fs-") => Err(format!(
                "{name} is not a read; only fs-read, fs-grep, fs-find, fs-symbol"
            )),
            _ => Err(SHAPE.to_string()),
        };
    };
    let (leading, rest) = match (op.positional(), arguments.split_first()) {
        (Some(option), Some((first, rest))) => (Some((option, literal(first)?)), rest),
        (Some(option), None) => return Err(format!("{}: `{option}` is required", op.name())),
        (None, _) => (None, arguments),
    };
    let mut options = Map::new();
    match rest {
        [] => {}
        [Datum::List(hash)] if hash.first() == Some(&Datum::Symbol("hash".to_string())) => {
            let (pairs, unpaired) = hash[1..].as_chunks::<2>();
            if !unpaired.is_empty() {
                return Err(format!("{}: an option has no value", op.name()));
            }
            for [name, value] in pairs {
                let name = match name {
                    Datum::Quoted(name) => match name.as_ref() {
                        Datum::Symbol(name) => name.clone(),
                        _ => {
                            return Err(format!(
                                "{}: an option's name is a quoted symbol",
                                op.name()
                            ))
                        }
                    },
                    _ => {
                        return Err(format!(
                            "{}: an option's name is a quoted symbol",
                            op.name()
                        ))
                    }
                };
                options.insert(name, literal(value)?);
            }
        }
        _ => {
            return Err(format!(
                "{}: options are one `(hash 'name value ...)`",
                op.name()
            ));
        }
    }
    if let Some((option, value)) = leading {
        options.insert(option.to_string(), value);
    }
    Ok(FsCall { op, options })
}

/// The reader calls of one cell, in order, or why the cell is refused.
pub(crate) fn parse_eval_cell(code: &str) -> Result<Vec<FsCall>, String> {
    let mut reader = Reader {
        rest: code.chars().peekable(),
    };
    let mut calls = Vec::new();
    while reader.at_datum() {
        calls.push(call(&reader.datum()?)?);
    }
    if calls.is_empty() {
        return Err("the cell holds no fs call".to_string());
    }
    Ok(calls)
}

impl FsCall {
    /// A short label for progress (`fs-read src/lib.rs`).
    fn label(&self) -> String {
        let target = ["path", "pattern", "name"]
            .iter()
            .find_map(|option| self.options.get(*option).and_then(Value::as_str))
            .unwrap_or(".");
        format!("{} {target}", self.op.name())
    }
}

/// Run one `eval` cell: every accepted call in order, each against the budget.
pub(super) async fn run_eval_cell(code: &str, context: &ReadContext) -> String {
    let calls = match parse_eval_cell(code) {
        Ok(calls) => calls,
        Err(reason) => return format!("Refused: {reason}. {EVAL_ACCEPTS}"),
    };
    let reads = std::sync::Arc::new(FileReads::new(
        context.cwd.clone(),
        tokio::runtime::Handle::current(),
    ));
    let mut sections = Vec::new();
    for call in calls {
        if !context.budget.take() {
            sections.push(context.budget_spent());
            break;
        }
        (context.progress)(&call.label());
        let reads = std::sync::Arc::clone(&reads);
        // The readers block on their replies.
        let text = tokio::task::spawn_blocking(move || {
            let name = call.op.name();
            match reads.run(name, &Value::Object(call.options)) {
                Ok(reply) => reply["text"].as_str().unwrap_or_default().to_string(),
                Err(error) => format!("{name} failed: {error:#}"),
            }
        })
        .await
        .unwrap_or_else(|error| format!("the read failed: {error}"));
        sections.push(text);
    }
    capped(sections.join("\n\n"))
}
