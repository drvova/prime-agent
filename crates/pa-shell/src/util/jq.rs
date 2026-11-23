//! `jq`: run a jq filter over JSON, with the `jaq` interpreter.
//!
//! Options: `-r -j -c -n -s -e -S -R -M -f`, `--tab`, `--indent N`,
//! `--arg NAME VALUE`, `--argjson NAME JSON`, `--slurpfile NAME FILE`,
//! `--rawfile NAME FILE`, `--args`, `--jsonargs`, `--unbuffered`, and their
//! long names. Anything else (`--stream`, `--seq`, `-a`, `-C`, `-L`, ...)
//! is the program's.
//!
//! The interpreter is not jq, and where the two differ this command sides
//! with jq where it can: values are laid out the way jq 1.7 lays them out,
//! and the vendored `jaq-json` prints numbers, assigns through a missing
//! parent and deletes keys as jq does; several input files are one stream, and
//! `-s` makes one array of all of them; `env` and `$ENV` are the shell's
//! exported variables; `debug` and `stderr` write to the command's standard
//! error; `halt` ends the command, not the process. What stays jaq's is
//! listed in the crate's README.
//!
//! The interpreter bounds nothing itself: it recurses on the thread's stack
//! with the nesting of the JSON it parses, compares and prints and with the
//! depth of a filter's own recursion, and a stack overflow ends the process.
//! So before anything runs: JSON nested deeper than [`DATA_DEPTH`] is
//! refused wherever it is parsed (the input, `fromjson`, `--argjson`,
//! `--slurpfile`), and a filter that is longer than [`FILTER_BYTES`], nests
//! deeper than [`FILTER_DEPTH`], defines a function that calls itself, or
//! does not compile here goes to the `jq` program. A filter can still build
//! a deeper value one level at a time, so it runs on a thread of its own
//! with a [`STACK_BYTES`] stack, which holds millions of levels. Nothing
//! here bounds the memory a filter takes. A stopped command ends at once,
//! but a filter that computes without reading or printing runs on, on its
//! thread, to its own end.

use std::cell::RefCell;
use std::io::{BufRead, Read, Write};
use std::panic::AssertUnwindSafe;
use std::sync::mpsc::{channel, RecvTimeoutError};
use std::time::Duration;

use jaq_core::box_iter::box_once;
use jaq_core::data::HasLut;
use jaq_core::load::parse::{BinaryOp, Pattern, Term};
use jaq_core::load::{self, lex::StrPart, Arena, File, Loader};
use jaq_core::native::{bome, run, v, Filter};
use jaq_core::path::Part;
use jaq_core::{Compiler, Ctx, DataT, Lut, RunPtr, Vars};
use jaq_json::{Map, Val};
use jaq_std::input::{HasInputs, Inputs, RcIter};

use super::{reason, Failure, Invocation, Outcome};

/// The deepest nesting of arrays and objects in JSON this command parses:
/// jq's own limit.
const DATA_DEPTH: usize = 10_000;
/// The longest filter run here. The interpreter compiles and runs a chain
/// of operators (`.a | .b | ...`) by recursing once per operator.
const FILTER_BYTES: usize = 16 * 1024;
/// The deepest nesting of brackets in a filter run here.
const FILTER_DEPTH: usize = 256;
/// Functions of jq 1.7 that the interpreter lacks, as jq defines them.
const JQ_DEFINITIONS: &str = r#"
def IN(s): any(s == .; .);
def IN(src; s): any(src == s; .);
def INDEX(stream; idx_expr): reduce stream as $row ({}; .[$row | idx_expr | tostring] |= $row);
def INDEX(idx_expr): INDEX(.[]; idx_expr);
def tostream: path(def r: (.[]? | r), .; r) as $p | getpath($p) | reduce path(.[]?) as $q ([$p, .]; [$p + $q]);
def @csv: map(
    if type == "string" then "\"" + gsub("\""; "\"\"") + "\""
    elif type == "number" or type == "boolean" then tojson
    elif . == null then ""
    else error("\(type) is not valid in a csv row") end
  ) | join(",");
def @tsv: map(
    if type == "string" then gsub("\\\\"; "\\\\") | gsub("\t"; "\\t") | gsub("\n"; "\\n") | gsub("\r"; "\\r")
    elif type == "number" or type == "boolean" then tojson
    elif . == null then ""
    else error("\(type) is not valid in a tsv row") end
  ) | join("\t");
"#;
/// The stack of the thread a filter runs on. It is address space: only the
/// pages a filter touches become memory. Dropping a value recurses to its
/// depth: measured in a debug build, arrays nested two million deep are
/// built and dropped within it, and objects nested five million deep are
/// not.
const STACK_BYTES: usize = 1 << 30;
/// How often the command looks for a stop while its filter runs.
const STOP_POLL: Duration = Duration::from_millis(50);

/// Where in JSON text a [`Nesting`] is.
#[derive(Clone, Copy, Default)]
enum Within {
    #[default]
    Structure,
    Text,
    /// After a backslash in a string.
    Escape,
    /// After a `#`, which the interpreter's reader takes as a comment.
    Comment,
}

/// How many arrays and objects are open in JSON text read so far, lexed as
/// the interpreter's reader lexes it.
#[derive(Default)]
struct Nesting {
    depth: usize,
    within: Within,
}

impl Nesting {
    /// Take in the next bytes of the text; whether it still nests no deeper
    /// than [`DATA_DEPTH`].
    fn admits(&mut self, json: &[u8]) -> bool {
        for byte in json {
            self.within = match (self.within, byte) {
                (Within::Structure, b'"') | (Within::Escape, _) => Within::Text,
                (Within::Structure, b'#') => Within::Comment,
                (Within::Structure, b'[' | b'{') => {
                    self.depth += 1;
                    if self.depth > DATA_DEPTH {
                        return false;
                    }
                    Within::Structure
                }
                (Within::Structure, b']' | b'}') => {
                    self.depth = self.depth.saturating_sub(1);
                    Within::Structure
                }
                (Within::Text, b'\\') => Within::Escape,
                (Within::Text, b'"') | (Within::Comment, b'\n') => Within::Structure,
                (within @ (Within::Structure | Within::Text | Within::Comment), _) => within,
            };
        }
        true
    }
}

/// A reader of JSON text that fails once the text nests too deep, before
/// the interpreter's reader is given the bytes that do.
struct Shallow<R> {
    reader: R,
    nesting: Nesting,
}

impl<R: Read> Read for Shallow<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.reader.read(buffer)?;
        if self.nesting.admits(&buffer[..read]) {
            Ok(read)
        } else {
            Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("JSON nested deeper than {DATA_DEPTH} levels"),
            ))
        }
    }
}

/// How values are printed.
#[derive(Clone, Default)]
struct Layout {
    /// What one level of nesting is indented by; `None` prints on one line.
    indent: Option<String>,
    sort_keys: bool,
}

/// Write `value` as jq 1.7 lays it out.
fn write_json(
    out: &mut dyn Write,
    value: &Val,
    layout: &Layout,
    level: usize,
) -> std::io::Result<()> {
    let text = |out: &mut dyn Write, bytes: &[u8]| -> std::io::Result<()> {
        out.write_all(b"\"")?;
        for character in String::from_utf8_lossy(bytes).chars() {
            match character {
                '"' => out.write_all(b"\\\"")?,
                '\\' => out.write_all(b"\\\\")?,
                '\n' => out.write_all(b"\\n")?,
                '\t' => out.write_all(b"\\t")?,
                '\r' => out.write_all(b"\\r")?,
                '\u{8}' => out.write_all(b"\\b")?,
                '\u{c}' => out.write_all(b"\\f")?,
                control if u32::from(control) < 0x20 || control == '\u{7f}' => {
                    write!(out, "\\u{:04x}", u32::from(control))?;
                }
                other => write!(out, "{other}")?,
            }
        }
        out.write_all(b"\"")
    };
    // What separates the members of an array or object at `level`.
    let line = |out: &mut dyn Write, level: usize| -> std::io::Result<()> {
        match &layout.indent {
            Some(indent) => write!(out, "\n{}", indent.repeat(level)),
            None => Ok(()),
        }
    };
    match value {
        Val::Null => out.write_all(b"null"),
        Val::Bool(truth) => write!(out, "{truth}"),
        // Floats as jq prints them: the vendored `jaq-json` does that.
        Val::Num(number) => write!(out, "{number}"),
        Val::TStr(bytes) | Val::BStr(bytes) => text(out, bytes),
        Val::Arr(items) if items.is_empty() => out.write_all(b"[]"),
        Val::Arr(items) => {
            out.write_all(b"[")?;
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.write_all(b",")?;
                }
                line(out, level + 1)?;
                write_json(out, item, layout, level + 1)?;
            }
            line(out, level)?;
            out.write_all(b"]")
        }
        Val::Obj(members) if members.is_empty() => out.write_all(b"{}"),
        Val::Obj(members) => {
            let mut members: Vec<(&Val, &Val)> = members.iter().collect();
            if layout.sort_keys {
                members.sort_by(|left, right| left.0.cmp(right.0));
            }
            out.write_all(b"{")?;
            for (index, (key, member)) in members.into_iter().enumerate() {
                if index > 0 {
                    out.write_all(b",")?;
                }
                line(out, level + 1)?;
                match key {
                    Val::TStr(bytes) | Val::BStr(bytes) => text(out, bytes)?,
                    // The interpreter allows any value as a key; JSON
                    // does not, so such a key is printed as its JSON text.
                    Val::Null | Val::Bool(_) | Val::Num(_) | Val::Arr(_) | Val::Obj(_) => {
                        let mut json = Vec::new();
                        write_json(&mut json, key, &Layout::default(), 0)?;
                        text(out, &json)?;
                    }
                }
                out.write_all(if layout.indent.is_some() { b": " } else { b":" })?;
                write_json(out, member, layout, level + 1)?;
            }
            line(out, level)?;
            out.write_all(b"}")
        }
    }
}

/// Whether `term` calls one of the definitions it is inside of (`open`): a
/// function that calls itself, directly or through one it defines. Such a
/// filter recurses on the stack as deep as its data sends it.
fn calls_itself<'s>(term: &Term<&'s str>, open: &mut Vec<&'s str>) -> bool {
    let any = |terms: &mut dyn Iterator<Item = &Term<&'s str>>, open: &mut Vec<&'s str>| {
        terms.fold(false, |found, term| found | calls_itself(term, open))
    };
    match term {
        Term::Id | Term::Recurse | Term::Num(_) | Term::Break(_) | Term::Var(_) => false,
        Term::Str(_, parts) => any(
            &mut parts.iter().filter_map(|part| match part {
                StrPart::Term(term) => Some(term),
                StrPart::Str(_) | StrPart::Char(_) => None,
            }),
            open,
        ),
        Term::Arr(inner) => any(&mut inner.iter().map(AsRef::as_ref), open),
        Term::Obj(members) => any(
            &mut members
                .iter()
                .flat_map(|(key, member)| std::iter::once(key).chain(member)),
            open,
        ),
        Term::Neg(inner) | Term::Label(_, inner) => any(&mut std::iter::once(inner.as_ref()), open),
        Term::BinOp(left, operator, right) => {
            let bound = match operator {
                BinaryOp::Pipe(Some(pattern)) => pattern_calls_itself(pattern, open),
                BinaryOp::Pipe(None)
                | BinaryOp::Comma
                | BinaryOp::Alt
                | BinaryOp::Or
                | BinaryOp::And
                | BinaryOp::Math(_)
                | BinaryOp::Cmp(_)
                | BinaryOp::Assign
                | BinaryOp::Update
                | BinaryOp::UpdateMath(_)
                | BinaryOp::UpdateAlt => false,
            };
            bound | any(&mut [left.as_ref(), right.as_ref()].into_iter(), open)
        }
        Term::Fold(_, source, pattern, steps) => {
            pattern_calls_itself(pattern, open)
                | any(&mut std::iter::once(source.as_ref()).chain(steps), open)
        }
        Term::TryCatch(tried, caught) => any(
            &mut std::iter::once(tried.as_ref()).chain(caught.iter().map(AsRef::as_ref)),
            open,
        ),
        Term::IfThenElse(branches, otherwise) => any(
            &mut branches
                .iter()
                .flat_map(|(condition, then)| [condition, then])
                .chain(otherwise.iter().map(AsRef::as_ref)),
            open,
        ),
        Term::Def(definitions, body) => {
            let mut found = false;
            for definition in definitions {
                open.push(definition.name);
                found |= calls_itself(&definition.body, open);
                open.pop();
            }
            found | calls_itself(body, open)
        }
        Term::Call(name, arguments) => open.contains(name) | any(&mut arguments.iter(), open),
        Term::Path(head, path) => any(
            &mut std::iter::once(head.as_ref()).chain(path.0.iter().flat_map(
                |(part, _)| match part {
                    Part::Index(index) => vec![index],
                    Part::Range(from, upto) => from.iter().chain(upto).collect(),
                },
            )),
            open,
        ),
    }
}

/// [`calls_itself`] for the computed keys of a destructuring pattern.
fn pattern_calls_itself<'s>(pattern: &Pattern<&'s str>, open: &mut Vec<&'s str>) -> bool {
    match pattern {
        Pattern::Var(_) => false,
        Pattern::Arr(items) => items.iter().fold(false, |found, item| {
            found | pattern_calls_itself(item, open)
        }),
        Pattern::Obj(members) => members.iter().fold(false, |found, (key, member)| {
            found | calls_itself(key, open) | pattern_calls_itself(member, open)
        }),
    }
}

/// The kind of data a filter runs on here.
struct Kind;

impl DataT for Kind {
    type V<'a> = Val;
    type Data<'a> = &'a Data<'a>;
}

/// What a running filter reaches besides its input.
struct Data<'a> {
    lut: &'a Lut<Kind>,
    /// The values `input` and `inputs` read, which are also the filter's.
    inputs: Inputs<'a, Val>,
    /// The command's standard error, for `debug`, `stderr` and failures.
    stderr: &'a RefCell<Box<dyn Write + Send>>,
    /// The shell's exported variables, as `env` and `$ENV`.
    env: &'a Val,
    /// The name of the file the last input value came from, or null.
    filename: &'a RefCell<Val>,
}

impl<'a> HasLut<'a, Kind> for &'a Data<'a> {
    fn lut(&self) -> &'a Lut<Kind> {
        self.lut
    }
}

impl<'a> HasInputs<'a, Val> for &'a Data<'a> {
    fn inputs(&self) -> Inputs<'a, Val> {
        self.inputs
    }
}

/// How positional arguments after the filter are taken.
#[derive(Clone, Copy)]
enum Positional {
    Files,
    /// After `--args`: strings in `$ARGS.positional`.
    Strings,
    /// After `--jsonargs`: JSON texts in `$ARGS.positional`.
    Json,
}

/// What a named-argument option binds its name to.
#[derive(Clone, Copy)]
enum Named {
    /// `--arg`: the text.
    Text,
    /// `--argjson`: the JSON value.
    Json,
    /// `--slurpfile`: an array of the JSON values in the file.
    JsonFile,
    /// `--rawfile`: the file's contents as a string.
    TextFile,
}

pub(crate) fn jq(invocation: &mut Invocation<'_>) -> Outcome {
    let Invocation {
        name: _,
        args,
        stdin: _,
        stdout: _,
        stderr,
        shell,
    } = invocation;
    let shell = *shell;
    // A usage error: said, and exit code 2.
    let mut misuse = |message: &str| -> Outcome {
        writeln!(stderr, "jq: error: {message}")?;
        Ok(2)
    };

    let (mut raw, mut join, mut null_input, mut slurp, mut exit_status, mut raw_input) =
        (false, false, false, false, false, false);
    let mut from_file = false;
    let mut layout = Layout {
        indent: Some("  ".to_string()),
        sort_keys: false,
    };
    let mut positional = Positional::Files;
    let mut named: Vec<(Named, &str, &str)> = Vec::new();
    let mut filter: Option<&str> = None;
    let (mut files, mut strings, mut json_texts) = (Vec::new(), Vec::new(), Vec::new());
    let mut options_ended = false;
    let mut words = args.iter().map(String::as_str);
    while let Some(word) = words.next() {
        let flags: Vec<&str> = match word.strip_prefix("--") {
            _ if options_ended || word == "-" || !word.starts_with('-') => {
                match (filter, positional) {
                    (None, _) => filter = Some(word),
                    (Some(_), Positional::Files) => files.push(word.to_string()),
                    (Some(_), Positional::Strings) => strings.push(word.to_string()),
                    (Some(_), Positional::Json) => json_texts.push(word.to_string()),
                }
                continue;
            }
            Some("") => {
                options_ended = true;
                continue;
            }
            Some(long) => vec![long],
            // A cluster of short options, each under its long name.
            None => {
                let mut flags = Vec::new();
                for short in word[1..].chars() {
                    flags.push(match short {
                        'r' => "raw-output",
                        'j' => "join-output",
                        'c' => "compact-output",
                        'n' => "null-input",
                        's' => "slurp",
                        'e' => "exit-status",
                        'S' => "sort-keys",
                        'R' => "raw-input",
                        'M' => "monochrome-output",
                        'f' => "from-file",
                        _ => return Err(Failure::Unsupported(format!("option -{short}"))),
                    });
                }
                flags
            }
        };
        for flag in flags {
            match flag {
                "raw-output" => raw = true,
                "join-output" => (raw, join) = (true, true),
                "compact-output" => layout.indent = None,
                "null-input" => null_input = true,
                "slurp" => slurp = true,
                "exit-status" => exit_status = true,
                "sort-keys" => layout.sort_keys = true,
                "raw-input" => raw_input = true,
                "from-file" => from_file = true,
                "tab" => layout.indent = Some("\t".to_string()),
                // Colours are never written, and each value is passed on
                // as it is produced.
                "monochrome-output" | "unbuffered" => {}
                "indent" => match words.next().map(str::parse::<usize>) {
                    Some(Ok(0)) => layout.indent = None,
                    Some(Ok(width @ 1..=7)) => layout.indent = Some(" ".repeat(width)),
                    Some(Ok(_)) => return misuse("Cannot indent more than 7 characters"),
                    Some(Err(_)) | None => return misuse("--indent takes a number"),
                },
                "arg" | "argjson" | "slurpfile" | "rawfile" => {
                    let (Some(name), Some(value)) = (words.next(), words.next()) else {
                        return misuse(&format!("--{flag} takes two parameters"));
                    };
                    let kind = match flag {
                        "arg" => Named::Text,
                        "argjson" => Named::Json,
                        "slurpfile" => Named::JsonFile,
                        _ => Named::TextFile,
                    };
                    named.push((kind, name, value));
                }
                "args" => positional = Positional::Strings,
                "jsonargs" => positional = Positional::Json,
                _ => return Err(Failure::Unsupported(format!("option --{flag}"))),
            }
        }
    }

    // Everything a reader gives, with a stop noticed between its chunks.
    let read_all = |reader: &mut dyn BufRead| -> Result<Vec<u8>, Failure> {
        let mut bytes = Vec::new();
        loop {
            shell.alive()?;
            let chunk = reader.fill_buf()?;
            if chunk.is_empty() {
                return Ok(bytes);
            }
            bytes.extend_from_slice(chunk);
            let read = chunk.len();
            reader.consume(read);
        }
    };
    // A file named by an option, or why it cannot be read.
    let read_file = |path: &str| -> Result<Result<Vec<u8>, String>, Failure> {
        match shell.file(path).and_then(|mut file| read_all(&mut *file)) {
            Ok(bytes) => Ok(Ok(bytes)),
            Err(Failure::Io(error)) => {
                Ok(Err(format!("Could not open {path}: {}", reason(&error))))
            }
            Err(other) => Err(other),
        }
    };

    // As jq away from a terminal: without a filter the input is printed.
    let filter = filter.unwrap_or(".");
    let filter = if from_file {
        match read_file(filter)? {
            Ok(text) => String::from_utf8_lossy(&text).into_owned(),
            Err(message) => return misuse(&message),
        }
    } else {
        filter.to_string()
    };
    if filter.len() > FILTER_BYTES {
        return Err(Failure::Unsupported(format!(
            "a filter longer than {FILTER_BYTES} bytes"
        )));
    }
    // Quoted or not: a string holds interpolated filters of its own.
    let nested = filter
        .bytes()
        .fold((0usize, 0usize), |(depth, deepest), byte| match byte {
            b'(' | b'[' | b'{' => (depth + 1, deepest.max(depth + 1)),
            b')' | b']' | b'}' => (depth.saturating_sub(1), deepest),
            _ => (depth, deepest),
        })
        .1;
    if nested > FILTER_DEPTH {
        return Err(Failure::Unsupported(format!(
            "a filter that nests brackets more than {FILTER_DEPTH} deep"
        )));
    }
    if load::parse(&filter, |parser| parser.term())
        .is_some_and(|term: Term<&str>| calls_itself(&term, &mut Vec::new()))
    {
        return Err(Failure::Unsupported(
            "a filter that defines a recursive function".to_string(),
        ));
    }
    let mut arguments: Vec<(Named, String, Vec<u8>)> = Vec::new();
    for (kind, name, value) in named {
        let bytes = match kind {
            Named::Text | Named::Json => value.as_bytes().to_vec(),
            Named::JsonFile | Named::TextFile => match read_file(value)? {
                Ok(bytes) => bytes,
                Err(message) => return misuse(&message),
            },
        };
        arguments.push((kind, name.to_string(), bytes));
    }
    // The input: the files, or the command's standard input. Opened here,
    // read on the filter's thread: a filter that goes to the program leaves
    // the input unread for the program.
    let mut sources: Vec<(Option<String>, Box<dyn BufRead + Send>)> = Vec::new();
    let mut unopened = false;
    let standard_input = || -> Box<dyn BufRead + Send> {
        match (shell.descriptor)(0) {
            Some(input) => Box::new(std::io::BufReader::new(input)),
            None => Box::new(std::io::empty()),
        }
    };
    if files.is_empty() {
        sources.push((None, standard_input()));
    }
    for path in files {
        if path == "-" {
            sources.push((None, standard_input()));
            continue;
        }
        match shell.file(&path) {
            Ok(file) => sources.push((Some(path), file)),
            Err(Failure::Io(error)) => {
                writeln!(
                    stderr,
                    "jq: error: Could not open {path}: {}",
                    reason(&error)
                )?;
                unopened = true;
            }
            Err(other) => return Err(other),
        }
    }
    let (Some(output), Some(diagnostics)) = ((shell.output)(1), (shell.output)(2)) else {
        return Err(Failure::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Bad file descriptor",
        )));
    };
    let environment = (shell.environment)();
    let has_program = (shell.program)("jq").is_some();
    let stop = shell.stop.clone();

    // The filter runs on a thread of its own, for its stack, with the
    // command's streams; this one waits for it, and for a stop.
    let (finished, finishing) = channel();
    let worker = std::thread::Builder::new()
        .name("pa-shell-jq".to_string())
        .stack_size(STACK_BYTES)
        .spawn(move || {
            let outcome =
                pa_types::session_scope::catching_panics(AssertUnwindSafe(|| -> Outcome {
                    let stderr = RefCell::new(diagnostics);
                    let mut stdout = std::io::BufWriter::with_capacity(64 * 1024, output);
                    // A fault-injection seam for the tests: a filter that panics.
                    #[cfg(test)]
                    assert!(filter != "pa-test-panic", "a filter panicked");
                    let misuse = |message: String| -> Outcome {
                        writeln!(stderr.borrow_mut(), "jq: error: {message}")?;
                        Ok(2)
                    };
                    // One JSON text as its value, or why it is none.
                    let parse_one = |json: &[u8]| -> Result<Val, String> {
                        if !Nesting::default().admits(json) {
                            return Err(format!("JSON nested deeper than {DATA_DEPTH} levels"));
                        }
                        jaq_json::read::parse_single(json).map_err(|error| error.to_string())
                    };

                    // The variables every filter may name, after the named
                    // arguments.
                    let mut bound: Vec<(String, Val)> = Vec::new();
                    for (kind, name, bytes) in arguments {
                        let bind = match kind {
                            Named::Text | Named::TextFile => Ok(Val::utf8_str(bytes)),
                            Named::Json => parse_one(&bytes).map_err(|error| {
                                format!("Invalid JSON text passed to --argjson: {error}")
                            }),
                            Named::JsonFile if !Nesting::default().admits(&bytes) => Err(format!(
                                "--slurpfile {name}: JSON nested deeper than {DATA_DEPTH} levels"
                            )),
                            Named::JsonFile => jaq_json::read::parse_many(&bytes)
                                .collect::<Result<Val, _>>()
                                .map_err(|error| format!("--slurpfile {name}: {error}")),
                        };
                        match bind {
                            Ok(bind) => bound.push((format!("${name}"), bind)),
                            Err(message) => return misuse(message),
                        }
                    }
                    let mut positionals: Vec<Val> = strings.into_iter().map(Val::from).collect();
                    for json in json_texts {
                        match parse_one(json.as_bytes()) {
                            Ok(value) => positionals.push(value),
                            Err(error) => {
                                return misuse(format!(
                                    "Invalid JSON text passed to --jsonargs: {error}"
                                ));
                            }
                        }
                    }
                    let key = |name: &str| Val::from(name.to_string());
                    let environment = Val::obj(
                        environment
                            .into_iter()
                            .map(|(name, value)| (Val::from(name), Val::from(value)))
                            .collect(),
                    );
                    let given_arguments = Val::obj(Map::from_iter([
                        (key("positional"), positionals.into_iter().collect()),
                        (
                            key("named"),
                            Val::obj(
                                bound
                                    .iter()
                                    .map(|(name, value)| (key(&name[1..]), value.clone()))
                                    .collect(),
                            ),
                        ),
                    ]));
                    bound.push(("$ARGS".to_string(), given_arguments));
                    bound.push(("$ENV".to_string(), environment.clone()));

                    // The interpreter's functions, with the ones that would
                    // reach the process (its environment, its standard error,
                    // its time zone) or parse JSON of any depth replaced by
                    // ones that act on this command.
                    let own: [Filter<RunPtr<Kind>>; 6] = [
                        ("env", v(0), |cv| box_once(Ok(cv.0.data().env.clone()))),
                        ("input_filename", v(0), |cv| {
                            box_once(Ok(cv.0.data().filename.borrow().clone()))
                        }),
                        ("debug_empty", v(0), |cv| {
                            let mut line = b"[\"DEBUG:\",".to_vec();
                            // Writing to a vector cannot fail, and a closed
                            // standard error has no reader left to tell.
                            let _ = write_json(&mut line, &cv.1, &Layout::default(), 0);
                            line.extend_from_slice(b"]\n");
                            let _ = cv.0.data().stderr.borrow_mut().write_all(&line);
                            Box::new(std::iter::empty())
                        }),
                        ("stderr_empty", v(0), |cv| {
                            let mut text = Vec::new();
                            match &cv.1 {
                                Val::TStr(bytes) | Val::BStr(bytes) => {
                                    text.extend_from_slice(bytes);
                                }
                                Val::Null
                                | Val::Bool(_)
                                | Val::Num(_)
                                | Val::Arr(_)
                                | Val::Obj(_) => {
                                    // As above: neither write has anyone to
                                    // report to.
                                    let _ = write_json(&mut text, &cv.1, &Layout::default(), 0);
                                    text.push(b'\n');
                                }
                            }
                            let _ = cv.0.data().stderr.borrow_mut().write_all(&text);
                            Box::new(std::iter::empty())
                        }),
                        ("tojson", v(0), |cv| {
                            let mut json = Vec::new();
                            bome(
                                write_json(&mut json, &cv.1, &Layout::default(), 0)
                                    .map(|()| Val::utf8_str(json))
                                    .map_err(jaq_json::Error::str),
                            )
                        }),
                        ("fromjson", v(0), |cv| {
                            bome(cv.1.try_as_utf8_bytes_owned().and_then(|json| {
                                if !Nesting::default().admits(&json) {
                                    return Err(jaq_json::Error::str(format_args!(
                                        "cannot parse JSON nested deeper than {DATA_DEPTH} levels"
                                    )));
                                }
                                jaq_json::read::parse_single(&json).map_err(|error| {
                                    jaq_json::Error::str(format_args!(
                                        "cannot parse {} as JSON: {error}",
                                        cv.1
                                    ))
                                })
                            }))
                        }),
                    ];
                    let functions = jaq_core::funs::<Kind>()
                        .chain(jaq_std::funs::<Kind>())
                        .chain(jaq_json::funs::<Kind>())
                        .filter(|(name, _, _)| {
                            !own.iter().any(|(mine, _, _)| mine == name)
                            // The process's time zone, not the shell's.
                            && !["localtime", "strflocaltime"].contains(name)
                        })
                        .chain(
                            jaq_std::input::funs::<Kind>()
                                .into_vec()
                                .into_iter()
                                .map(run::<Kind>),
                        )
                        .chain(own.clone().into_iter().map(run::<Kind>));

                    let arena = Arena::default();
                    let loader = Loader::new(
                        jaq_core::defs()
                            .chain(jaq_std::defs())
                            .chain(jaq_json::defs())
                            .chain(
                                load::parse(JQ_DEFINITIONS, |parser| parser.defs())
                                    .unwrap_or_default(),
                            ),
                    );
                    // Why the filter cannot run here, one line per reason.
                    let mut faults: Vec<String> = Vec::new();
                    let near = |found: &str| {
                        if found.is_empty() {
                            "at the end of the filter".to_string()
                        } else {
                            format!("at {:?}", found.chars().take(20).collect::<String>())
                        }
                    };
                    let compiled = match loader.load(
                        &arena,
                        File {
                            path: (),
                            code: filter.as_str(),
                        },
                    ) {
                        Ok(modules) => Compiler::default()
                            .with_funs(functions)
                            .with_global_vars(bound.iter().map(|(name, _)| name.as_str()))
                            .compile(modules)
                            .map_err(|errors| {
                                for (found, undefined) in
                                    errors.into_iter().flat_map(|(_, errors)| errors)
                                {
                                    faults.push(match undefined {
                                        jaq_core::compile::Undefined::Filter(arity) => {
                                            format!("{found}/{arity} is not defined")
                                        }
                                        // The enum is open to new kinds of names.
                                        other => {
                                            format!("{} {found} is not defined", other.as_str())
                                        }
                                    });
                                }
                            }),
                        Err(errors) => {
                            for (_, error) in errors {
                                match error {
                                    load::Error::Io(errors) => {
                                        faults.extend(errors.into_iter().map(|(path, error)| {
                                            format!("could not load {path}: {error}")
                                        }));
                                    }
                                    load::Error::Lex(errors) => {
                                        faults.extend(errors.into_iter().map(
                                            |(expected, found)| {
                                                format!(
                                                    "syntax error: expected {} {}",
                                                    expected.as_str(),
                                                    near(found)
                                                )
                                            },
                                        ));
                                    }
                                    load::Error::Parse(errors) => {
                                        faults.extend(errors.into_iter().map(
                                            |(expected, found)| {
                                                format!(
                                                    "syntax error: expected {} {}",
                                                    expected.as_str(),
                                                    near(found)
                                                )
                                            },
                                        ));
                                    }
                                }
                            }
                            Err(())
                        }
                    };
                    let Ok(compiled) = compiled else {
                        // jq itself knows whether the filter is wrong or only
                        // beyond this interpreter, and says so in its own words.
                        if has_program {
                            return Err(Failure::Unsupported("this filter".to_string()));
                        }
                        let mut stderr = stderr.borrow_mut();
                        for fault in &faults {
                            writeln!(stderr, "jq: error: {fault}")?;
                        }
                        writeln!(stderr, "jq: {} compile error(s)", faults.len().max(1))?;
                        return Ok(3);
                    };

                    let filename = RefCell::new(Val::Null);
                    let mut failed = false;
                    let mut values: Box<dyn Iterator<Item = Result<Val, String>> + '_> =
                        if slurp && raw_input {
                            let mut text = Vec::new();
                            let mut read = Ok(());
                            for (_, mut reader) in sources {
                                read = read.and(reader.read_to_end(&mut text).map(|_| ()));
                            }
                            Box::new(std::iter::once(
                                read.map(|()| Val::utf8_str(text))
                                    .map_err(|error| reason(&error)),
                            ))
                        } else {
                            Box::new(
                        sources
                            .into_iter()
                            .flat_map(
                                |(name, reader)| -> Box<dyn Iterator<Item = Result<Val, String>>> {
                                    let name = name.map_or(Val::Null, Val::from);
                                    let filename = &filename;
                                    let named = move |item: std::io::Result<Val>| {
                                        filename.replace(name.clone());
                                        item.map_err(|error| reason(&error))
                                    };
                                    if raw_input {
                                        Box::new(
                                            reader
                                                .split(b'\n')
                                                .map(|line| line.map(Val::utf8_str))
                                                .map(named),
                                        )
                                    } else {
                                        Box::new(
                                            jaq_json::read::read_many(std::io::BufReader::new(
                                                Shallow {
                                                    reader,
                                                    nesting: Nesting::default(),
                                                },
                                            ))
                                            .map(named),
                                        )
                                    }
                                },
                            )
                            // Nothing follows what could not be parsed, or
                            // a stop.
                            .take_while(|item| {
                                let go_on = !failed && !stop.requested();
                                failed = item.is_err();
                                go_on
                            }),
                    )
                        };
                    if slurp && !raw_input {
                        let all = values.by_ref().collect::<Result<Val, String>>();
                        values = Box::new(std::iter::once(all));
                    }
                    let inputs = RcIter::new(values);
                    let nothing = RcIter::new(std::iter::once(Ok(Val::Null)));
                    let data = Data {
                        lut: &compiled.lut,
                        inputs: &inputs,
                        stderr: &stderr,
                        env: &environment,
                        filename: &filename,
                    };
                    let context = Ctx::<Kind>::new(
                        &data,
                        Vars::new(bound.into_iter().map(|(_, value)| value)),
                    );

                    // What the command exits with besides `-e`'s verdict on the
                    // last value: 5 when input could not be parsed or the filter
                    // failed on the last input, or what `halt` gave.
                    let mut failure = None;
                    let mut last: Option<bool> = None;
                    let mains: Inputs<'_, Val> = if null_input { &nothing } else { &inputs };
                    'inputs: for input in mains {
                        let input = match input {
                            Ok(input) => input,
                            Err(message) => {
                                stdout.flush()?;
                                writeln!(stderr.borrow_mut(), "jq: parse error: {message}")?;
                                failure = Some(5);
                                break;
                            }
                        };
                        failure = None;
                        for output in compiled.id.run::<Kind>((context.clone(), input)) {
                            if stop.requested() {
                                return Err(Failure::Stopped);
                            }
                            match output.map_err(jaq_core::Exn::get_err) {
                                Ok(value) => {
                                    last = Some(!matches!(value, Val::Null | Val::Bool(false)));
                                    match &value {
                                        Val::TStr(bytes) | Val::BStr(bytes) if raw => {
                                            stdout.write_all(bytes)?;
                                        }
                                        Val::Null
                                        | Val::Bool(_)
                                        | Val::Num(_)
                                        | Val::TStr(_)
                                        | Val::BStr(_)
                                        | Val::Arr(_)
                                        | Val::Obj(_) => {
                                            write_json(&mut stdout, &value, &layout, 0)?;
                                        }
                                    }
                                    if !join {
                                        stdout.write_all(b"\n")?;
                                    }
                                }
                                // The filter failed on this input; the next one
                                // is still run.
                                Err(Ok(error)) => {
                                    stdout.flush()?;
                                    let mut stderr = stderr.borrow_mut();
                                    match error.into_val() {
                                        Val::TStr(text) | Val::BStr(text) => {
                                            let text = String::from_utf8_lossy(&text);
                                            writeln!(stderr, "jq: error: {text}")?;
                                        }
                                        thrown @ (Val::Null
                                        | Val::Bool(_)
                                        | Val::Num(_)
                                        | Val::Arr(_)
                                        | Val::Obj(_)) => {
                                            stderr.write_all(b"jq: error (not a string): ")?;
                                            write_json(
                                                &mut **stderr,
                                                &thrown,
                                                &Layout::default(),
                                                0,
                                            )?;
                                            stderr.write_all(b"\n")?;
                                        }
                                    }
                                    failure = Some(5);
                                    break;
                                }
                                // `halt`: the command ends here with the code
                                // it gave.
                                Err(Err(halt)) => {
                                    failure = Some(halt.get_halt().unwrap_or(5));
                                    break 'inputs;
                                }
                            }
                        }
                        if stop.requested() {
                            return Err(Failure::Stopped);
                        }
                    }
                    stdout.flush()?;
                    Ok(match (failure, exit_status, last) {
                        (Some(code), _, _) => code,
                        (None, false, _) | (None, true, Some(true)) => 0,
                        (None, true, Some(false)) => 1,
                        (None, true, None) => 4,
                    })
                }));
            // Unheard when the command was stopped and has gone.
            let _ = finished.send(outcome);
        });
    if worker.is_err() {
        return Err(Failure::Unsupported(
            "running a filter (this system gave no thread a stack of 1 GiB)".to_string(),
        ));
    }
    let outcome = loop {
        match finishing.recv_timeout(STOP_POLL) {
            Ok(outcome) => break outcome,
            // A stop is noticed here: the command ends, and a filter that
            // computes without reading or printing runs on, on its thread,
            // to its own end.
            Err(RecvTimeoutError::Timeout) => shell.alive()?,
            // The thread ended without a word, which only a panic outside
            // the filter does.
            Err(RecvTimeoutError::Disconnected) => {
                return Err(Failure::Io(std::io::Error::other(
                    "the filter's thread ended without a result",
                )));
            }
        }
    };
    match outcome {
        // A missing input file is exit code 2, as jq's.
        Ok(Ok(_)) if unopened => Ok(2),
        Ok(outcome) => outcome,
        // The filter's panic is this command's.
        Err(panic) => std::panic::resume_unwind(panic),
    }
}
