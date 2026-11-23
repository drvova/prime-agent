//! `sed [-n] [-E|-r] [-i[SUFFIX]] [-s] [-e SCRIPT]... [-f FILE]... [FILE...]`.
//!
//! Commands: `s` (flags `g`, `p`, `i`/`I`, a number), `y`, `d`, `D`, `p`,
//! `P`, `q`, `Q`, `=`, `a`, `i`, `c`, `n`, `N`, `h`, `H`, `g`, `G`, `x`,
//! `{...}`, with line, `$`, `/regex/`, `first~step` and range addresses and
//! `!`. Branches and labels, files read or written by the script, and
//! `\U`-style case conversion are not implemented: such a script goes to
//! the program.

use std::io::{BufRead, Write};

use super::pattern::{translate, Syntax};
use super::{reason, Arg, Failure, Invocation, Options, Outcome};

enum Address {
    Line(u64),
    Last,
    Pattern(regex::bytes::Regex),
    /// `first~step`
    Step(u64, u64),
}

enum RangeEnd {
    Address(Address),
    /// `addr,+N`
    Lines(u64),
}

/// A piece of an `s` replacement.
enum Piece {
    Text(Vec<u8>),
    /// `&` is group 0.
    Group(usize),
}

enum Command {
    Substitute {
        regex: regex::bytes::Regex,
        replacement: Vec<Piece>,
        global: bool,
        /// Replace from this match on (1 is the first).
        nth: usize,
        print: bool,
    },
    Translate(Box<[u8; 256]>),
    Delete,
    DeleteFirstLine,
    Print,
    PrintFirstLine,
    Quit {
        print: bool,
        code: i32,
    },
    LineNumber,
    Append(Vec<u8>),
    Insert(Vec<u8>),
    Change(Vec<u8>),
    Next,
    NextAppend,
    Hold,
    HoldAppend,
    Get,
    GetAppend,
    Exchange,
    /// `{`: the index of its `}`.
    Block(usize),
    BlockEnd,
}

/// Where an instruction with a range stands.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Range {
    Outside,
    /// Inside, up to this line when the range's end is a line number.
    Inside(Option<u64>),
}

struct Instruction {
    start: Option<Address>,
    end: Option<RangeEnd>,
    negate: bool,
    command: Command,
    range: Range,
}

/// The script's text, read a character at a time.
struct Script {
    chars: Vec<char>,
    at: usize,
    syntax: Syntax,
}

impl Script {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let next = self.peek();
        self.at += usize::from(next.is_some());
        next
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.at += 1;
        }
    }

    fn number(&mut self) -> Option<u64> {
        let start = self.at;
        while self.peek().is_some_and(|digit| digit.is_ascii_digit()) {
            self.at += 1;
        }
        self.chars[start..self.at]
            .iter()
            .collect::<String>()
            .parse()
            .ok()
    }

    /// The text up to the next unescaped `delimiter`, escapes kept, except
    /// that an escaped delimiter is the delimiter itself.
    fn delimited(&mut self, delimiter: char) -> Result<String, Failure> {
        let mut text = String::new();
        loop {
            match self.bump() {
                None => return Err(Failure::Unsupported("an unterminated command".to_string())),
                Some(found) if found == delimiter => return Ok(text),
                Some('\\') => match self.bump() {
                    Some(escaped) if escaped == delimiter => text.push(escaped),
                    Some(escaped) => {
                        text.push('\\');
                        text.push(escaped);
                    }
                    None => text.push('\\'),
                },
                Some(other) => text.push(other),
            }
        }
    }

    fn regex(&mut self, delimiter: char) -> Result<regex::bytes::Regex, Failure> {
        let text = self.delimited(delimiter)?;
        if text.is_empty() {
            return Err(Failure::Unsupported(
                "an empty regular expression".to_string(),
            ));
        }
        let mut ignore_case = false;
        // After an address or before an `s` replacement the flags differ:
        // only an address takes `I` here.
        if self.peek() == Some('I') {
            self.at += 1;
            ignore_case = true;
        }
        self.compile(&text, ignore_case)
    }

    fn compile(&self, text: &str, ignore_case: bool) -> Result<regex::bytes::Regex, Failure> {
        // GNU sed reads `\t` as a tab; the shared syntax does not.
        let text = text
            .replace("\\\\", "\u{0}")
            .replace("\\t", "\t")
            .replace('\u{0}', "\\\\");
        regex::bytes::RegexBuilder::new(&translate(&text, self.syntax)?)
            .case_insensitive(ignore_case)
            .build()
            .map_err(|error| {
                Failure::Unsupported(format!("the expression ({error})").replace('\n', " "))
            })
    }

    fn address(&mut self) -> Result<Option<Address>, Failure> {
        Ok(match self.peek() {
            Some('$') => {
                self.at += 1;
                Some(Address::Last)
            }
            Some('/') => {
                self.at += 1;
                Some(Address::Pattern(self.regex('/')?))
            }
            Some('\\') => {
                self.at += 1;
                let delimiter = self
                    .bump()
                    .ok_or_else(|| Failure::Unsupported("an unterminated address".to_string()))?;
                Some(Address::Pattern(self.regex(delimiter)?))
            }
            Some(digit) if digit.is_ascii_digit() => {
                let first = self.number().unwrap_or(0);
                if self.peek() == Some('~') {
                    self.at += 1;
                    Some(Address::Step(first, self.number().unwrap_or(0)))
                } else {
                    Some(Address::Line(first))
                }
            }
            Some(_) | None => None,
        })
    }

    /// The text of `a`, `i` or `c`: the rest of the line, continued over
    /// lines that end in a backslash.
    fn text(&mut self) -> Vec<u8> {
        self.skip_blanks();
        if self.peek() == Some('\\') {
            self.at += 1;
            if self.peek() == Some('\n') {
                self.at += 1;
            } else {
                self.skip_blanks();
            }
        }
        let mut text = String::new();
        loop {
            match self.bump() {
                None | Some('\n') => break,
                Some('\\') => {
                    if let Some(escaped) = self.bump() {
                        text.push(escaped);
                    }
                }
                Some(other) => text.push(other),
            }
        }
        text.push('\n');
        text.into_bytes()
    }

    fn parse(&mut self) -> Result<Vec<Instruction>, Failure> {
        let mut instructions: Vec<Instruction> = Vec::new();
        let mut open: Vec<usize> = Vec::new();
        loop {
            while matches!(self.peek(), Some(' ' | '\t' | '\n' | ';')) {
                self.at += 1;
            }
            let Some(_) = self.peek() else {
                break;
            };
            let start = self.address()?;
            let mut end = None;
            if start.is_some() && self.peek() == Some(',') {
                self.at += 1;
                end = Some(if self.peek() == Some('+') {
                    self.at += 1;
                    RangeEnd::Lines(self.number().unwrap_or(0))
                } else {
                    RangeEnd::Address(self.address()?.ok_or_else(|| {
                        Failure::Unsupported("a range without its end".to_string())
                    })?)
                });
            }
            self.skip_blanks();
            let mut negate = false;
            while self.peek() == Some('!') {
                self.at += 1;
                negate = true;
                self.skip_blanks();
            }
            let Some(name) = self.bump() else {
                return Err(Failure::Unsupported(
                    "an address without a command".to_string(),
                ));
            };
            let command = match name {
                '#' => {
                    while !matches!(self.peek(), None | Some('\n')) {
                        self.at += 1;
                    }
                    continue;
                }
                '{' => {
                    open.push(instructions.len());
                    Command::Block(0)
                }
                '}' => {
                    let Some(opened) = open.pop() else {
                        return Err(Failure::Unsupported("an unmatched }".to_string()));
                    };
                    instructions[opened].command = Command::Block(instructions.len());
                    Command::BlockEnd
                }
                's' => {
                    let delimiter = self
                        .bump()
                        .ok_or_else(|| Failure::Unsupported("an unterminated s".to_string()))?;
                    let pattern = self.delimited(delimiter)?;
                    let raw: Vec<char> = self.delimited(delimiter)?.chars().collect();
                    let mut replacement = Vec::new();
                    let mut text = String::new();
                    let mut at = 0;
                    while at < raw.len() {
                        let current = raw[at];
                        at += 1;
                        let group = match current {
                            '&' => Some(0),
                            '\\' => {
                                let escaped = raw.get(at).copied();
                                at += 1;
                                match escaped {
                                    Some(digit @ '0'..='9') => {
                                        digit.to_digit(10).map(|digit| digit as usize)
                                    }
                                    Some('n') => {
                                        text.push('\n');
                                        None
                                    }
                                    Some('t') => {
                                        text.push('\t');
                                        None
                                    }
                                    Some('U' | 'L' | 'E' | 'u' | 'l') => {
                                        return Err(Failure::Unsupported(
                                            "case conversion in a replacement".to_string(),
                                        ))
                                    }
                                    Some(other) => {
                                        text.push(other);
                                        None
                                    }
                                    None => None,
                                }
                            }
                            other => {
                                text.push(other);
                                None
                            }
                        };
                        if let Some(group) = group {
                            replacement.push(Piece::Text(std::mem::take(&mut text).into_bytes()));
                            replacement.push(Piece::Group(group));
                        }
                    }
                    replacement.push(Piece::Text(text.into_bytes()));
                    let (mut global, mut print, mut ignore_case, mut nth) =
                        (false, false, false, 1);
                    loop {
                        match self.peek() {
                            Some('g') => global = true,
                            Some('p') => print = true,
                            Some('i' | 'I') => ignore_case = true,
                            Some(digit) if digit.is_ascii_digit() => {
                                nth = self.number().unwrap_or(1).max(1) as usize;
                                continue;
                            }
                            Some('w' | 'e' | 'm' | 'M') => {
                                return Err(Failure::Unsupported("an s flag".to_string()))
                            }
                            Some(_) | None => break,
                        }
                        self.at += 1;
                    }
                    if pattern.is_empty() {
                        return Err(Failure::Unsupported(
                            "an empty regular expression".to_string(),
                        ));
                    }
                    Command::Substitute {
                        regex: self.compile(&pattern, ignore_case)?,
                        replacement,
                        global,
                        nth,
                        print,
                    }
                }
                'y' => {
                    let delimiter = self
                        .bump()
                        .ok_or_else(|| Failure::Unsupported("an unterminated y".to_string()))?;
                    let unescape = |text: String| text.replace("\\n", "\n").replace("\\\\", "\\");
                    let from = unescape(self.delimited(delimiter)?);
                    let to = unescape(self.delimited(delimiter)?);
                    if from.len() != to.len() || !from.is_ascii() || !to.is_ascii() {
                        return Err(Failure::Unsupported("this y command".to_string()));
                    }
                    let mut table: Box<[u8; 256]> =
                        Box::new(std::array::from_fn(|byte| u8::try_from(byte).unwrap_or(0)));
                    for (source, target) in from.bytes().zip(to.bytes()) {
                        table[usize::from(source)] = target;
                    }
                    Command::Translate(table)
                }
                'a' => Command::Append(self.text()),
                'i' => Command::Insert(self.text()),
                'c' => Command::Change(self.text()),
                'q' | 'Q' => {
                    self.skip_blanks();
                    Command::Quit {
                        print: name == 'q',
                        code: self
                            .number()
                            .and_then(|code| i32::try_from(code).ok())
                            .unwrap_or(0),
                    }
                }
                'd' => Command::Delete,
                'D' => Command::DeleteFirstLine,
                'p' => Command::Print,
                'P' => Command::PrintFirstLine,
                '=' => Command::LineNumber,
                'n' => Command::Next,
                'N' => Command::NextAppend,
                'h' => Command::Hold,
                'H' => Command::HoldAppend,
                'g' => Command::Get,
                'G' => Command::GetAppend,
                'x' => Command::Exchange,
                other => return Err(Failure::Unsupported(format!("the sed command {other}"))),
            };
            instructions.push(Instruction {
                start,
                end,
                negate,
                command,
                range: Range::Outside,
            });
        }
        if open.is_empty() {
            Ok(instructions)
        } else {
            Err(Failure::Unsupported("an unmatched {".to_string()))
        }
    }
}

/// The input lines of one run, with one line of lookahead for `$`.
struct Input<'a> {
    readers: Vec<Box<dyn BufRead + 'a>>,
    ahead: Option<(Vec<u8>, bool)>,
    number: u64,
}

impl Input<'_> {
    fn read(&mut self) -> std::io::Result<Option<(Vec<u8>, bool)>> {
        while let Some(reader) = self.readers.first_mut() {
            let mut line = Vec::new();
            if reader.read_until(b'\n', &mut line)? > 0 {
                let terminated = line.last() == Some(&b'\n');
                if terminated {
                    line.pop();
                }
                return Ok(Some((line, terminated)));
            }
            self.readers.remove(0);
        }
        Ok(None)
    }

    /// The next line, and whether it had a newline.
    fn next(&mut self) -> std::io::Result<Option<(Vec<u8>, bool)>> {
        let line = match self.ahead.take() {
            Some(line) => Some(line),
            None => self.read()?,
        };
        if line.is_some() {
            self.number += 1;
            self.ahead = self.read()?;
        }
        Ok(line)
    }

    fn at_last(&self) -> bool {
        self.ahead.is_none()
    }
}

/// Run `instructions` over `input`. Returns the exit code.
fn run(
    instructions: &mut [Instruction],
    quiet: bool,
    input: &mut Input<'_>,
    alive: &dyn Fn() -> Result<(), Failure>,
    out: &mut dyn Write,
) -> Result<i32, Failure> {
    let mut hold: Vec<u8> = Vec::new();
    'cycle: loop {
        let Some((mut space, mut terminated)) = input.next()? else {
            return Ok(0);
        };
        // A `D` restarts the script on what is left of the pattern space.
        'script: loop {
            alive()?;
            let mut appended: Vec<u8> = Vec::new();
            let emit =
                |out: &mut dyn Write, text: &[u8], terminated: bool| -> std::io::Result<()> {
                    out.write_all(text)?;
                    if terminated {
                        out.write_all(b"\n")?;
                    }
                    Ok(())
                };
            let mut at = 0;
            let mut print_at_end = !quiet;
            while at < instructions.len() {
                let line = input.number;
                let last = input.at_last();
                let instruction = &mut instructions[at];
                let on = |address: &Address, space: &[u8]| match address {
                    Address::Line(wanted) => *wanted == line,
                    Address::Last => last,
                    Address::Pattern(regex) => regex.is_match(space),
                    Address::Step(first, step) => {
                        line >= *first
                            && (*step == 0 && line == *first
                                || *step > 0 && (line - first).is_multiple_of(*step))
                    }
                };
                let selected = match (&instruction.start, &instruction.end) {
                    (None, _) => true,
                    (Some(start), None) => on(start, &space),
                    (Some(start), Some(end)) => match instruction.range {
                        Range::Inside(until) => {
                            let ends = match (until, end) {
                                (Some(until), _) => line >= until,
                                (None, RangeEnd::Address(address)) => on(address, &space),
                                (None, RangeEnd::Lines(_)) => true,
                            };
                            if ends || last {
                                instruction.range = Range::Outside;
                            }
                            true
                        }
                        Range::Outside if on(start, &space) => {
                            // An end at or before this line is this line alone.
                            let until = match end {
                                RangeEnd::Address(Address::Line(until)) => Some(*until),
                                RangeEnd::Lines(lines) => Some(line + lines),
                                RangeEnd::Address(
                                    Address::Last | Address::Pattern(_) | Address::Step(_, _),
                                ) => None,
                            };
                            if until.is_none_or(|until| until > line) && !last {
                                instruction.range = Range::Inside(until);
                            }
                            true
                        }
                        Range::Outside => false,
                    },
                };
                if selected == instruction.negate {
                    at = if let Command::Block(end) = instruction.command {
                        end + 1
                    } else {
                        at + 1
                    };
                    continue;
                }
                at += 1;
                match &instruction.command {
                    Command::Block(_) | Command::BlockEnd => {}
                    Command::Substitute {
                        regex,
                        replacement,
                        global,
                        nth,
                        print,
                    } => {
                        let mut result = Vec::with_capacity(space.len());
                        let (mut copied, mut seen, mut replaced) = (0, 0usize, false);
                        for captures in regex.captures_iter(&space) {
                            seen += 1;
                            if seen < *nth {
                                continue;
                            }
                            let Some(whole) = captures.get(0) else {
                                continue;
                            };
                            result.extend_from_slice(&space[copied..whole.start()]);
                            for piece in replacement {
                                match piece {
                                    Piece::Text(text) => result.extend_from_slice(text),
                                    Piece::Group(group) => {
                                        if let Some(found) = captures.get(*group) {
                                            result.extend_from_slice(found.as_bytes());
                                        }
                                    }
                                }
                            }
                            copied = whole.end();
                            replaced = true;
                            if !global {
                                break;
                            }
                        }
                        if replaced {
                            result.extend_from_slice(&space[copied..]);
                            space = result;
                            if *print {
                                emit(out, &space, true)?;
                            }
                        }
                    }
                    Command::Translate(table) => {
                        for byte in &mut space {
                            *byte = table[usize::from(*byte)];
                        }
                    }
                    Command::Delete => {
                        out.write_all(&appended)?;
                        continue 'cycle;
                    }
                    Command::DeleteFirstLine => {
                        out.write_all(&appended)?;
                        match space.iter().position(|byte| *byte == b'\n') {
                            Some(newline) => {
                                space.drain(..=newline);
                                continue 'script;
                            }
                            None => continue 'cycle,
                        }
                    }
                    Command::Print => emit(out, &space, true)?,
                    Command::PrintFirstLine => {
                        let end = space
                            .iter()
                            .position(|byte| *byte == b'\n')
                            .unwrap_or(space.len());
                        emit(out, &space[..end], true)?;
                    }
                    Command::Quit { print, code } => {
                        if *print && print_at_end {
                            emit(out, &space, terminated)?;
                        }
                        out.write_all(&appended)?;
                        return Ok(*code);
                    }
                    Command::LineNumber => writeln!(out, "{line}")?,
                    Command::Append(text) => appended.extend_from_slice(text),
                    Command::Insert(text) => out.write_all(text)?,
                    Command::Change(text) => {
                        // A range is replaced once, at its end.
                        if instruction.range == Range::Outside {
                            out.write_all(text)?;
                        }
                        out.write_all(&appended)?;
                        continue 'cycle;
                    }
                    Command::Next => {
                        if print_at_end {
                            emit(out, &space, terminated)?;
                        }
                        out.write_all(&std::mem::take(&mut appended))?;
                        match input.next()? {
                            Some((next, ended)) => (space, terminated) = (next, ended),
                            // GNU sed ends here without printing again.
                            None => return Ok(0),
                        }
                    }
                    Command::NextAppend => match input.next()? {
                        Some((next, ended)) => {
                            space.push(b'\n');
                            space.extend_from_slice(&next);
                            terminated = ended;
                        }
                        // GNU sed prints the pattern space and ends.
                        None => break,
                    },
                    Command::Hold => hold.clone_from(&space),
                    Command::HoldAppend => {
                        hold.push(b'\n');
                        hold.extend_from_slice(&space);
                    }
                    Command::Get => space.clone_from(&hold),
                    Command::GetAppend => {
                        space.push(b'\n');
                        space.extend_from_slice(&hold);
                    }
                    Command::Exchange => std::mem::swap(&mut space, &mut hold),
                }
                print_at_end = !quiet;
            }
            if print_at_end {
                emit(out, &space, terminated)?;
            }
            out.write_all(&appended)?;
            continue 'cycle;
        }
    }
}

pub(super) fn sed(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let (mut quiet, mut syntax) = (false, Syntax::Basic);
    let mut in_place: Option<&str> = None;
    let mut scripts: Vec<String> = Vec::new();
    let mut script_files = Vec::new();
    let mut operands = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('n') | Arg::Long("quiet" | "silent") => quiet = true,
            Arg::Short('E' | 'r') | Arg::Long("regexp-extended") => syntax = Syntax::Extended,
            Arg::Short('s') | Arg::Long("separate") => {}
            Arg::Short('i') | Arg::Long("in-place") => {
                in_place = Some(options.attached().unwrap_or(""));
            }
            Arg::Short('e') | Arg::Long("expression") => scripts.push(options.value()?.to_string()),
            Arg::Short('f') | Arg::Long("file") => script_files.push(options.value()?),
            Arg::Operand(operand) => operands.push(operand),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    let shell = invocation.shell;
    for file in script_files {
        scripts.push(std::fs::read_to_string(shell.path(file))?);
    }
    if scripts.is_empty() {
        if operands.is_empty() {
            writeln!(
                invocation.stderr,
                "Usage: sed [OPTION]... {{script-only-if-no-other-script}} [input-file]..."
            )?;
            return Ok(1);
        }
        scripts.push(operands.remove(0).to_string());
    }
    let source = scripts.join("\n");
    if source.starts_with("#n\n") {
        quiet = true;
    }
    let mut instructions = Script {
        chars: source.chars().collect(),
        at: 0,
        syntax,
    }
    .parse()?;
    let alive = || shell.alive();
    let Invocation {
        stdin,
        stdout,
        stderr,
        ..
    } = invocation;
    if let Some(suffix) = in_place {
        let mut status = 0;
        for file in operands {
            let path = shell.path(file);
            let original = match shell.open(&mut **stdin, file) {
                Ok(reader) => reader,
                Err(Failure::Io(error)) => {
                    writeln!(stderr, "sed: can't read {file}: {}", reason(&error))?;
                    status = 2;
                    continue;
                }
                Err(other) => return Err(other),
            };
            let mut edited = Vec::new();
            for instruction in &mut instructions {
                instruction.range = Range::Outside;
            }
            let code = run(
                &mut instructions,
                quiet,
                &mut Input {
                    readers: vec![original],
                    ahead: None,
                    number: 0,
                },
                &alive,
                &mut edited,
            )?;
            if !suffix.is_empty() {
                std::fs::copy(&path, shell.path(&format!("{file}{suffix}")))?;
            }
            // Written in place, so the file keeps its permissions and links.
            std::fs::write(&path, edited)?;
            if code != 0 {
                return Ok(code);
            }
        }
        return Ok(status);
    }
    if operands.is_empty() {
        operands.push("-");
    }
    let mut status = 0;
    let mut readers: Vec<Box<dyn BufRead + '_>> = Vec::new();
    let mut standard_input = Some(&mut **stdin);
    for file in operands {
        let opened = if file == "-" {
            standard_input
                .take()
                .map(|stdin| Box::new(stdin) as Box<dyn BufRead + '_>)
                .ok_or_else(|| Failure::Unsupported("standard input named twice".to_string()))
        } else {
            shell.file(file)
        };
        match opened {
            Ok(reader) => readers.push(reader),
            Err(Failure::Io(error)) => {
                writeln!(stderr, "sed: can't read {file}: {}", reason(&error))?;
                status = 2;
            }
            Err(other) => return Err(other),
        }
    }
    let code = run(
        &mut instructions,
        quiet,
        &mut Input {
            readers,
            ahead: None,
            number: 0,
        },
        &alive,
        stdout,
    )?;
    Ok(if code == 0 { status } else { code })
}
