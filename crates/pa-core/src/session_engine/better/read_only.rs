//! The helper's read-only tools: the parent's tool definitions, byte for byte (so the helper's
//! requests share the session's cached prompt prefix), with host-side executors that can only
//! read.
//!
//! Why the host executes them instead of the session's kernel and shell: neither can be
//! restricted from the inside. A Python cell can `open(path, "w")`, and a shell line can
//! redirect, pipe into anything, or reach a program through a reading command's own flags
//! (`rg --pre`, `find -exec`, `git grep -O`, a repository-configured fsmonitor hook or diff
//! driver). So the helper never reaches either:
//!
//! - `python_repl` accepts only calls of `fs.read`/`fs.grep`/`fs.glob`/`fs.find` with literal
//!   arguments (`print(await fs.read("src/lib.rs", start=1, end=80))`, one per line). The host
//!   parses the cell (nothing evaluates it) and runs each call through the same in-process
//!   readers the kernel's `fs.*` host requests use. Anything else is refused with what is
//!   accepted.
//! - `bash` accepts only one read-only `git` command (`log`, `show`, `diff`, `status`,
//!   `ls-files`, `grep`, `rev-parse`), parsed without a shell: no pipes, redirects,
//!   substitutions, or variables, and no flag that opens a pager, writes a file, or starts a
//!   program. It runs with the autonomous gates' hardening overrides (no fsmonitor hook, no
//!   index write, no lazy fetch, no text conversion or external diff). The one residual those
//!   overrides document stays: a `.gitattributes` clean filter can run on a modified tracked
//!   file for `diff`/`status`.
//! - Every other tool (`edit`, and the internal helpers) is refused: this helper never writes.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use pa_agent::abort::AbortSignal;
use pa_agent::types::{AgentTool, AgentToolResult, AgentToolUpdateCallback};
use pa_agent::BoxFut;
use serde_json::{json, Map, Value};

/// Progress the helper reports while it reads (a short label per read).
pub type ReadProgress = Arc<dyn Fn(&str) + Send + Sync>;

/// The most text one tool result returns; the rest is cut with a note. Reads are for grounding a
/// rewrite, not for copying files, so a few screens per call is plenty.
const RESULT_MAX_CHARS: usize = 12_000;

/// How long one read-only git command may run.
const GIT_TIMEOUT_MS: u64 = 15_000;

/// The git subcommands the helper may run: each only reads the repository.
const GIT_READ_SUBCOMMANDS: [&str; 7] = [
    "log",
    "show",
    "diff",
    "status",
    "ls-files",
    "grep",
    "rev-parse",
];

/// Flag prefixes refused on any git subcommand: each writes a file (`--output`), opens a pager
/// program (`-O`, `--open-files-in-pager`), or re-enables a program the overrides turn off.
const GIT_REFUSED_FLAGS: [&str; 6] = [
    "--output",
    "-O",
    "--open-files-in-pager",
    "--ext-diff",
    "--textconv",
    "--exec",
];

pub(crate) const EDIT_REFUSED: &str = "Refused: the /better helper is read-only. It cannot edit, \
     write, or delete files. Read what you need, then reply with your <rewrite> block.";

const REPL_ACCEPTS: &str = "python_repl in this read-only helper accepts only fs reads with \
     literal arguments, one call per line, e.g. `print(await fs.read(\"src/lib.rs\", start=1, \
     end=80))`, `print(await fs.grep(\"fn parse\", \"src\", glob=\"*.rs\"))`, \
     `print(await fs.glob(\"**/*.toml\"))`, `print(await fs.find(\"Makefile\"))`.";

const BASH_ACCEPTS: &str = "bash in this read-only helper runs only one read-only git command \
     (`git log`, `show`, `diff`, `status`, `ls-files`, `grep`, `rev-parse`) without pipes, \
     redirects, or substitutions. Read and search files with python_repl's fs calls.";

/// The helper's read allowance for its current pass, shared by its tools.
#[derive(Debug, Default)]
pub(crate) struct ReadBudget {
    used: AtomicU32,
    limit: AtomicU32,
}

impl ReadBudget {
    pub(crate) fn new(limit: u32) -> Self {
        Self {
            used: AtomicU32::new(0),
            limit: AtomicU32::new(limit),
        }
    }

    /// Take one read; `false` once the pass's allowance is spent.
    fn take(&self) -> bool {
        let limit = self.limit.load(Ordering::SeqCst);
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |used| {
                (used < limit).then_some(used + 1)
            })
            .is_ok()
    }

    #[cfg(test)]
    pub(crate) fn used(&self) -> u32 {
        self.used.load(Ordering::SeqCst)
    }

    pub(crate) fn limit(&self) -> u32 {
        self.limit.load(Ordering::SeqCst)
    }
}

/// What every read-only executor shares: where reads resolve, the budget, and the progress tap.
#[derive(Clone)]
pub(crate) struct ReadContext {
    pub(crate) cwd: PathBuf,
    pub(crate) budget: Arc<ReadBudget>,
    pub(crate) progress: ReadProgress,
}

impl ReadContext {
    fn budget_spent(&self) -> String {
        format!(
            "The read budget for this pass is used up ({} reads). Reply now with your <depth>, \
             <unresolved>, and <rewrite> blocks; list under <unresolved> what you could not \
             locate yet.",
            self.budget.limit()
        )
    }
}

/// Which executor a wrapped tool runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Executor {
    FsCalls,
    Git,
    Refused,
}

/// One parent tool's definition with a read-only executor in place of its own.
struct ReadOnlyTool {
    definition: Arc<dyn AgentTool>,
    executor: Executor,
    context: ReadContext,
}

/// The parent's tools, each wrapped with its read-only executor (see the module docs).
pub(crate) fn read_only_tools(
    parent: &[Arc<dyn AgentTool>],
    context: &ReadContext,
) -> Vec<Arc<dyn AgentTool>> {
    parent
        .iter()
        .map(|tool| {
            let executor = match tool.name() {
                pa_types::ai::PYTHON_TOOL_NAME => Executor::FsCalls,
                "bash" => Executor::Git,
                _ => Executor::Refused,
            };
            Arc::new(ReadOnlyTool {
                definition: Arc::clone(tool),
                executor,
                context: context.clone(),
            }) as Arc<dyn AgentTool>
        })
        .collect()
}

impl AgentTool for ReadOnlyTool {
    fn name(&self) -> &str {
        self.definition.name()
    }

    fn description(&self) -> &str {
        self.definition.description()
    }

    fn parameters(&self) -> &Value {
        self.definition.parameters()
    }

    fn label(&self) -> &str {
        self.definition.label()
    }

    fn execute(
        self: Arc<Self>,
        _tool_call_id: String,
        params: Value,
        _signal: AbortSignal,
        _on_update: AgentToolUpdateCallback,
    ) -> BoxFut<'static, anyhow::Result<AgentToolResult>> {
        Box::pin(async move {
            let text = match self.executor {
                Executor::Refused => EDIT_REFUSED.to_string(),
                Executor::FsCalls => {
                    let code = params.get("code").and_then(Value::as_str).unwrap_or("");
                    run_repl_cell(code, &self.context).await
                }
                Executor::Git => {
                    let command = params.get("command").and_then(Value::as_str).unwrap_or("");
                    run_git_command(command, &self.context).await
                }
            };
            Ok(AgentToolResult::text(text))
        })
    }
}

/// Cut `text` to [`RESULT_MAX_CHARS`] with a note.
fn capped(text: String) -> String {
    match text.char_indices().nth(RESULT_MAX_CHARS) {
        Some((end, _)) => format!(
            "{}\n[output cut at {RESULT_MAX_CHARS} characters; read a narrower range]",
            &text[..end]
        ),
        None => text,
    }
}

// ---------------------------------------------------------------------------------------------
// python_repl: literal fs calls only

/// One literal value a call argument may hold.
#[derive(Debug, Clone, PartialEq)]
enum Literal {
    Str(String),
    Int(i64),
    Bool(bool),
    None,
    List(Vec<Literal>),
}

/// The four readers a cell may call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FsOp {
    Read,
    Grep,
    Glob,
    Find,
}

impl FsOp {
    fn from_name(name: &str) -> Option<Self> {
        match name {
            "read" => Some(Self::Read),
            "grep" => Some(Self::Grep),
            "glob" => Some(Self::Glob),
            "find" => Some(Self::Find),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Read => "fs.read",
            Self::Grep => "fs.grep",
            Self::Glob => "fs.glob",
            Self::Find => "fs.find",
        }
    }

    /// Positional parameter names, in order.
    fn positional(self) -> &'static [&'static str] {
        match self {
            Self::Read => &["path"],
            Self::Grep | Self::Glob => &["pattern", "path"],
            Self::Find => &["name", "path"],
        }
    }

    /// Every parameter the call takes (the kernel client's signature).
    fn parameters(self) -> &'static [&'static str] {
        match self {
            Self::Read => &["path", "start", "end"],
            Self::Grep => &[
                "pattern",
                "path",
                "glob",
                "type",
                "case",
                "literal",
                "word",
                "multiline",
                "context",
                "before",
                "after",
                "hidden",
                "gitignore",
                "max_results",
                "max_per_file",
                "max_columns",
            ],
            Self::Glob => &["pattern", "path", "hidden", "gitignore", "max_results"],
            Self::Find => &[
                "name",
                "path",
                "kind",
                "ignore_case",
                "max_depth",
                "hidden",
                "gitignore",
                "max_results",
            ],
        }
    }
}

/// One parsed `fs.<op>(...)` call: its arguments by parameter name.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct FsCall {
    pub(crate) op: FsOp,
    arguments: Vec<(&'static str, Literal)>,
}

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Name(String),
    Str(String),
    Int(i64),
    Punct(char),
    /// A statement break: a newline outside brackets, or `;`.
    Break,
}

/// Tokenize the cell's Python subset. Comments drop; newlines inside brackets join lines.
fn tokenize(code: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = code.chars().collect();
    let mut tokens = Vec::new();
    let mut depth = 0usize;
    let mut at = 0;
    while at < chars.len() {
        let c = chars[at];
        match c {
            '#' => {
                while at < chars.len() && chars[at] != '\n' {
                    at += 1;
                }
            }
            '\n' | ';' => {
                if depth == 0 {
                    tokens.push(Token::Break);
                }
                at += 1;
            }
            c if c.is_whitespace() => at += 1,
            '(' | '[' => {
                depth += 1;
                tokens.push(Token::Punct(c));
                at += 1;
            }
            ')' | ']' => {
                depth = depth.saturating_sub(1);
                tokens.push(Token::Punct(c));
                at += 1;
            }
            ',' | '=' | '.' => {
                tokens.push(Token::Punct(c));
                at += 1;
            }
            '\'' | '"' => {
                let (text, next) = string_literal(&chars, at, false)?;
                tokens.push(Token::Str(text));
                at = next;
            }
            'r' | 'R' if matches!(chars.get(at + 1), Some('\'' | '"')) => {
                let (text, next) = string_literal(&chars, at + 1, true)?;
                tokens.push(Token::Str(text));
                at = next;
            }
            c if c.is_ascii_digit() => {
                let start = at;
                while at < chars.len() && (chars[at].is_ascii_digit() || chars[at] == '_') {
                    at += 1;
                }
                let digits: String = chars[start..at].iter().filter(|c| **c != '_').collect();
                let value = digits
                    .parse::<i64>()
                    .map_err(|_| format!("the number {digits} is out of range"))?;
                tokens.push(Token::Int(value));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = at;
                while at < chars.len() && (chars[at].is_alphanumeric() || chars[at] == '_') {
                    at += 1;
                }
                tokens.push(Token::Name(chars[start..at].iter().collect()));
            }
            other => return Err(format!("`{other}` is not accepted here")),
        }
    }
    Ok(tokens)
}

/// One string literal starting at its opening quote: its value and the index after it.
fn string_literal(chars: &[char], open: usize, raw: bool) -> Result<(String, usize), String> {
    let quote = chars[open];
    let triple = chars.get(open + 1) == Some(&quote) && chars.get(open + 2) == Some(&quote);
    let mut at = open + if triple { 3 } else { 1 };
    let mut value = String::new();
    loop {
        let Some(&c) = chars.get(at) else {
            return Err("a string literal is not closed".to_string());
        };
        if c == quote {
            if !triple {
                return Ok((value, at + 1));
            }
            if chars.get(at + 1) == Some(&quote) && chars.get(at + 2) == Some(&quote) {
                return Ok((value, at + 3));
            }
        }
        if c == '\n' && !triple {
            return Err("a string literal is not closed".to_string());
        }
        if c == '\\' && !raw {
            let Some(&escaped) = chars.get(at + 1) else {
                return Err("a string literal is not closed".to_string());
            };
            match escaped {
                'n' => value.push('\n'),
                't' => value.push('\t'),
                'r' => value.push('\r'),
                '0' => value.push('\0'),
                '\\' | '\'' | '"' => value.push(escaped),
                '\n' => {}
                other => {
                    value.push('\\');
                    value.push(other);
                }
            }
            at += 2;
            continue;
        }
        value.push(c);
        at += 1;
    }
}

/// A cursor over one statement's tokens.
struct Cursor<'a> {
    tokens: &'a [Token],
    at: usize,
}

impl Cursor<'_> {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn next(&mut self) -> Option<&Token> {
        let token = self.tokens.get(self.at);
        self.at += 1;
        token
    }

    fn eat_name(&mut self, name: &str) -> bool {
        if matches!(self.peek(), Some(Token::Name(found)) if found == name) {
            self.at += 1;
            return true;
        }
        false
    }

    fn eat_punct(&mut self, punct: char) -> bool {
        if self.peek() == Some(&Token::Punct(punct)) {
            self.at += 1;
            return true;
        }
        false
    }

    fn expect_punct(&mut self, punct: char) -> Result<(), String> {
        if self.eat_punct(punct) {
            Ok(())
        } else {
            Err(format!("expected `{punct}`"))
        }
    }

    fn literal(&mut self) -> Result<Literal, String> {
        match self.next() {
            Some(Token::Str(text)) => {
                let mut text = text.clone();
                // Adjacent literals concatenate, as in Python.
                while let Some(Token::Str(more)) = self.peek() {
                    text.push_str(more);
                    self.at += 1;
                }
                Ok(Literal::Str(text))
            }
            Some(Token::Int(value)) => Ok(Literal::Int(*value)),
            Some(Token::Name(name)) => match name.as_str() {
                "True" => Ok(Literal::Bool(true)),
                "False" => Ok(Literal::Bool(false)),
                "None" => Ok(Literal::None),
                other => Err(format!(
                    "`{other}` is a name, and only literal arguments are accepted"
                )),
            },
            Some(Token::Punct('[')) => {
                let mut items = Vec::new();
                while !self.eat_punct(']') {
                    items.push(self.literal()?);
                    if !self.eat_punct(',') {
                        self.expect_punct(']')?;
                        break;
                    }
                }
                Ok(Literal::List(items))
            }
            _ => Err("expected a literal argument".to_string()),
        }
    }

    /// `[print(] [str(] [await] fs.<op>(<args>) [)] [)]`
    fn statement(&mut self) -> Result<FsCall, String> {
        let mut closers = 0;
        for wrapper in ["print", "str"] {
            if self.eat_name(wrapper) {
                self.expect_punct('(')?;
                closers += 1;
            }
        }
        self.eat_name("await");
        if !self.eat_name("fs") {
            return Err("each line must be one fs.read/grep/glob/find call".to_string());
        }
        self.expect_punct('.')?;
        let op = match self.next() {
            Some(Token::Name(name)) => FsOp::from_name(name)
                .ok_or_else(|| format!("fs.{name} is not a read; only read/grep/glob/find"))?,
            _ => return Err("expected fs.read/grep/glob/find".to_string()),
        };
        self.expect_punct('(')?;
        let mut arguments: Vec<(&'static str, Literal)> = Vec::new();
        let mut positional = op.positional().iter();
        while !self.eat_punct(')') {
            let keyword = match (self.tokens.get(self.at), self.tokens.get(self.at + 1)) {
                (Some(Token::Name(name)), Some(Token::Punct('='))) => {
                    let name = name.clone();
                    self.at += 2;
                    Some(name)
                }
                _ => None,
            };
            let value = self.literal()?;
            let name = match keyword {
                Some(keyword) => *op
                    .parameters()
                    .iter()
                    .find(|parameter| **parameter == keyword)
                    .ok_or_else(|| format!("{}() has no parameter `{keyword}`", op.name()))?,
                None => *positional
                    .next()
                    .ok_or_else(|| format!("{}() takes fewer positional arguments", op.name()))?,
            };
            if arguments.iter().any(|(existing, _)| *existing == name) {
                return Err(format!("{}() got `{name}` twice", op.name()));
            }
            arguments.push((name, value));
            if !self.eat_punct(',') {
                self.expect_punct(')')?;
                break;
            }
        }
        for _ in 0..closers {
            self.expect_punct(')')?;
        }
        if self.at != self.tokens.len() {
            return Err("each line must hold exactly one call".to_string());
        }
        Ok(FsCall { op, arguments })
    }
}

/// Parse a cell into its fs calls; the error says what is not accepted.
pub(crate) fn parse_repl_cell(code: &str) -> Result<Vec<FsCall>, String> {
    let tokens = tokenize(code)?;
    let mut calls = Vec::new();
    for statement in tokens.split(|token| *token == Token::Break) {
        if statement.is_empty() {
            continue;
        }
        calls.push(
            Cursor {
                tokens: statement,
                at: 0,
            }
            .statement()?,
        );
    }
    if calls.is_empty() {
        return Err("the cell holds no fs call".to_string());
    }
    Ok(calls)
}

impl FsCall {
    fn get(&self, name: &str) -> Option<&Literal> {
        self.arguments
            .iter()
            .find(|(parameter, _)| *parameter == name)
            .map(|(_, value)| value)
    }

    fn string(&self, name: &str) -> Result<Option<String>, String> {
        match self.get(name) {
            None | Some(Literal::None) => Ok(None),
            Some(Literal::Str(text)) => Ok(Some(text.clone())),
            Some(_) => Err(format!("{}: `{name}` must be a string", self.op.name())),
        }
    }

    fn strings(&self, name: &str) -> Result<Option<Vec<String>>, String> {
        match self.get(name) {
            None | Some(Literal::None) => Ok(None),
            Some(Literal::Str(text)) => Ok(Some(vec![text.clone()])),
            Some(Literal::List(items)) => items
                .iter()
                .map(|item| match item {
                    Literal::Str(text) => Ok(text.clone()),
                    _ => Err(format!("{}: `{name}` must hold strings", self.op.name())),
                })
                .collect::<Result<Vec<_>, _>>()
                .map(Some),
            Some(_) => Err(format!(
                "{}: `{name}` must be a string or a list",
                self.op.name()
            )),
        }
    }

    fn flag(&self, name: &str, default: bool) -> Result<bool, String> {
        match self.get(name) {
            None => Ok(default),
            Some(Literal::Bool(value)) => Ok(*value),
            Some(_) => Err(format!(
                "{}: `{name}` must be True or False",
                self.op.name()
            )),
        }
    }

    fn count(&self, name: &str) -> Result<Option<i64>, String> {
        match self.get(name) {
            None | Some(Literal::None) => Ok(None),
            Some(Literal::Int(value)) => Ok(Some(*value)),
            Some(_) => Err(format!("{}: `{name}` must be an integer", self.op.name())),
        }
    }

    /// A short label for progress (`fs.read src/lib.rs`).
    fn label(&self) -> String {
        let target = self
            .string("path")
            .ok()
            .flatten()
            .or_else(|| self.string("pattern").ok().flatten())
            .or_else(|| self.string("name").ok().flatten())
            .unwrap_or_else(|| ".".to_string());
        format!("{} {target}", self.op.name())
    }

    /// The host request payload the kernel client would send (`rlm/fs.py`).
    fn payload(&self, cwd: &Path) -> Result<Value, String> {
        let resolve = |path: &str| -> String { cwd.join(path).to_string_lossy().into_owned() };
        let path = self.string("path")?.unwrap_or_else(|| ".".to_string());
        let cwd_text = cwd.to_string_lossy().into_owned();
        let mut payload = Map::new();
        let mut set = |key: &str, value: Value| {
            payload.insert(key.to_string(), value);
        };
        match self.op {
            FsOp::Read => {
                let Some(path) = self.string("path")? else {
                    return Err("fs.read: `path` is required".to_string());
                };
                set("path", json!(resolve(&path)));
                set("cwd", json!(cwd_text));
                for key in ["start", "end"] {
                    if let Some(value) = self.count(key)? {
                        set(key, json!(value));
                    }
                }
            }
            FsOp::Grep => {
                let Some(pattern) = self.string("pattern")? else {
                    return Err("fs.grep: `pattern` is required".to_string());
                };
                let context = self.count("context")?.unwrap_or(0);
                set("pattern", json!(pattern));
                set("root", json!(resolve(&path)));
                set("cwd", json!(cwd_text));
                set(
                    "case",
                    json!(self
                        .string("case")?
                        .unwrap_or_else(|| "sensitive".to_string())),
                );
                for (key, wire) in [
                    ("literal", "literal"),
                    ("word", "word"),
                    ("multiline", "multiline"),
                    ("hidden", "hidden"),
                ] {
                    set(wire, json!(self.flag(key, false)?));
                }
                set("gitignore", json!(self.flag("gitignore", true)?));
                set("before", json!(self.count("before")?.unwrap_or(context)));
                set("after", json!(self.count("after")?.unwrap_or(context)));
                set(
                    "maxResults",
                    json!(self.count("max_results")?.unwrap_or(100)),
                );
                set(
                    "maxColumns",
                    json!(self.count("max_columns")?.unwrap_or(300)),
                );
                if let Some(globs) = self.strings("glob")? {
                    set("globs", json!(globs));
                }
                if let Some(types) = self.strings("type")? {
                    set("types", json!(types));
                }
                if let Some(max) = self.count("max_per_file")? {
                    set("maxPerFile", json!(max));
                }
            }
            FsOp::Glob => {
                let Some(pattern) = self.string("pattern")? else {
                    return Err("fs.glob: `pattern` is required".to_string());
                };
                set("pattern", json!(pattern));
                set("root", json!(resolve(&path)));
                set("cwd", json!(cwd_text));
                set("hidden", json!(self.flag("hidden", false)?));
                set("gitignore", json!(self.flag("gitignore", true)?));
                set(
                    "maxResults",
                    json!(self.count("max_results")?.unwrap_or(500)),
                );
            }
            FsOp::Find => {
                set("root", json!(resolve(&path)));
                set("cwd", json!(cwd_text));
                if let Some(name) = self.string("name")? {
                    set("name", json!(name));
                }
                if let Some(kind) = self.string("kind")? {
                    set("kind", json!(kind));
                }
                if let Some(depth) = self.count("max_depth")? {
                    set("maxDepth", json!(depth));
                }
                set("ignoreCase", json!(self.flag("ignore_case", false)?));
                set("hidden", json!(self.flag("hidden", false)?));
                set("gitignore", json!(self.flag("gitignore", true)?));
                set(
                    "maxResults",
                    json!(self.count("max_results")?.unwrap_or(500)),
                );
            }
        }
        Ok(Value::Object(payload))
    }
}

/// Run one parsed call through the in-process readers: its rendered text, or the error.
fn run_fs_call(call: &FsCall, cwd: &Path) -> String {
    let outcome = call
        .payload(cwd)
        .map_err(anyhow::Error::msg)
        .and_then(|payload| {
            let reply = match call.op {
                FsOp::Read => crate::fs_edit::read(&crate::fs_edit::parse_read(&payload)?)?.0,
                FsOp::Grep => {
                    let request = crate::fs_search::parse_grep(&payload)?;
                    crate::fs_search::grep(&request)?.to_json(request.max_results)
                }
                FsOp::Glob => {
                    let request = crate::fs_search::parse_glob(&payload)?;
                    crate::fs_search::glob(&request)?.to_json(request.max_results)
                }
                FsOp::Find => {
                    let request = crate::fs_search::parse_find(&payload)?;
                    crate::fs_search::find(&request)?.to_json(request.max_results)
                }
            };
            Ok(reply
                .get("text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string())
        });
    outcome.unwrap_or_else(|error| format!("{} failed: {error:#}", call.op.name()))
}

/// Run one `python_repl` cell: every accepted call in order, each against the budget.
async fn run_repl_cell(code: &str, context: &ReadContext) -> String {
    let calls = match parse_repl_cell(code) {
        Ok(calls) => calls,
        Err(reason) => return format!("Refused: {reason}. {REPL_ACCEPTS}"),
    };
    let mut sections = Vec::new();
    for call in calls {
        if !context.budget.take() {
            sections.push(context.budget_spent());
            break;
        }
        (context.progress)(&call.label());
        let cwd = context.cwd.clone();
        let text = tokio::task::spawn_blocking(move || run_fs_call(&call, &cwd))
            .await
            .unwrap_or_else(|error| format!("the read failed: {error}"));
        sections.push(text);
    }
    capped(sections.join("\n\n"))
}

// ---------------------------------------------------------------------------------------------
// bash: one read-only git command

/// Split a command line into words without a shell: single and double quotes group, a
/// backslash escapes the next character, and every shell operator is refused.
fn shell_words(command: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut in_word = false;
    let mut chars = command.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(inner) => word.push(inner),
                        None => return Err("a quote is not closed".to_string()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('$' | '`') => {
                            return Err("substitutions are not accepted".to_string());
                        }
                        Some('\\') => match chars.next() {
                            Some(escaped) => word.push(escaped),
                            None => return Err("a quote is not closed".to_string()),
                        },
                        Some(inner) => word.push(inner),
                        None => return Err("a quote is not closed".to_string()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(escaped) => word.push(escaped),
                    None => return Err("a trailing backslash is not accepted".to_string()),
                }
            }
            '|' | '&' | ';' | '<' | '>' | '$' | '`' | '(' | ')' | '{' | '}' | '\n' | '*' | '?'
            | '~' | '!' => {
                return Err(format!("`{c}` is a shell operator and is not accepted"));
            }
            c if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut word));
                    in_word = false;
                }
            }
            c => {
                in_word = true;
                word.push(c);
            }
        }
    }
    if in_word {
        words.push(word);
    }
    Ok(words)
}

/// The full argument list for one accepted read-only git command, or why it is refused.
pub(crate) fn git_arguments(command: &str) -> Result<Vec<String>, String> {
    let words = shell_words(command)?;
    let Some((program, rest)) = words.split_first() else {
        return Err("the command is empty".to_string());
    };
    if program != "git" {
        return Err(format!("`{program}` is not read-only git"));
    }
    let Some((subcommand, arguments)) = rest.split_first() else {
        return Err("name a git subcommand".to_string());
    };
    if !GIT_READ_SUBCOMMANDS.contains(&subcommand.as_str()) {
        return Err(format!(
            "`git {subcommand}` is not one of the read-only subcommands"
        ));
    }
    if let Some(flag) = arguments.iter().find(|argument| {
        GIT_REFUSED_FLAGS
            .iter()
            .any(|refused| argument.starts_with(refused))
    }) {
        return Err(format!("`{flag}` can write a file or start a program"));
    }
    let mut full: Vec<String> = crate::autonomous::SNAPSHOT_GIT_OVERRIDES
        .iter()
        .map(ToString::to_string)
        .collect();
    full.extend(
        ["--no-pager", "-c", "core.pager=cat"]
            .iter()
            .map(ToString::to_string),
    );
    full.push(subcommand.clone());
    let hardening: &[&str] = match subcommand.as_str() {
        "diff" | "show" | "log" => &["--no-ext-diff", "--no-textconv"],
        "grep" => &["--no-textconv"],
        "status" => &["--ignore-submodules=all"],
        _ => &[],
    };
    full.extend(hardening.iter().map(ToString::to_string));
    full.extend(arguments.iter().cloned());
    Ok(full)
}

/// Run one `bash` call: an accepted git command, against the budget.
async fn run_git_command(command: &str, context: &ReadContext) -> String {
    let arguments = match git_arguments(command) {
        Ok(arguments) => arguments,
        Err(reason) => return format!("Refused: {reason}. {BASH_ACCEPTS}"),
    };
    if !context.budget.take() {
        return context.budget_spent();
    }
    (context.progress)(command.trim());
    // The same transport lock-down as the gates' snapshot probes: no lazy fetch can start a
    // repository-chosen transport.
    let result = crate::autonomous::run_child_process(
        "git",
        &arguments,
        &context.cwd,
        GIT_TIMEOUT_MS,
        &[
            ("GIT_ALLOW_PROTOCOL", ""),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_PAGER", "cat"),
            ("GIT_OPTIONAL_LOCKS", "0"),
        ],
    )
    .await;
    let text = match result {
        Ok(result) if result.timed_out => {
            format!("git timed out after {GIT_TIMEOUT_MS} ms")
        }
        Ok(result) => {
            let mut text = result.stdout;
            if !result.stderr.is_empty() {
                text.push_str(&result.stderr);
            }
            if result.status != Some(0) {
                let _ = write!(text, "\n[exit {}]", result.status.unwrap_or(-1));
            }
            text
        }
        Err(error) => format!("git did not run: {error:#}"),
    };
    capped(text)
}

#[cfg(test)]
pub(super) mod tests;
