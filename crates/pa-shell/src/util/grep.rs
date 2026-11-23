//! `grep`: lines that match a pattern, in files, directories, or the
//! command's input.
//!
//! Options: `-E -F -G -i -y -v -n -c -l -L -r -R -w -x -o -q -s -h -H -a -I`,
//! `-e PATTERN`, `-f FILE`, `-m N`, `-A N`, `-B N`, `-C N`, `-N`,
//! `--include=GLOB`, `--exclude=GLOB`, `--exclude-dir=GLOB`,
//! `--color=never|auto`. A directory's entries are searched in name order
//! (GNU grep takes them in the order the directory lists them).

use std::collections::VecDeque;
use std::io::Write;
use std::path::Path;

use super::pattern::{compile, Syntax};
use super::{each_line, reason, Arg, Failure, Invocation, Options, Outcome, View};

/// What a file's matches are printed as.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Lines,
    Count,
    /// `-l`: the names of files with a match.
    Names,
    /// `-L`: the names of files without one.
    NamesWithout,
    Quiet,
}

struct Search {
    matcher: regex::bytes::Regex,
    mode: Mode,
    invert: bool,
    line_numbers: bool,
    only_matching: bool,
    /// How many selected lines end a file's search.
    limit: Option<u64>,
    before: usize,
    after: usize,
    /// `-a`: a file with a NUL byte is searched as text all the same.
    text: bool,
    /// `-I`: such a file is skipped.
    skip_binary: bool,
    with_names: bool,
}

/// Search one input. Returns whether a line was selected.
fn search(
    search: &Search,
    shell: View<'_>,
    reader: &mut dyn std::io::BufRead,
    shown: &str,
    stdout: &mut dyn Write,
    stderr: &mut dyn Write,
    printed_before: &mut bool,
) -> Result<bool, Failure> {
    let binary = !search.text && reader.fill_buf()?.contains(&0);
    if binary && search.skip_binary {
        return Ok(false);
    }
    let prefix = |separator: char, number: u64| match (search.with_names, search.line_numbers) {
        (true, true) => format!("{shown}{separator}{number}{separator}"),
        (true, false) => format!("{shown}{separator}"),
        (false, true) => format!("{number}{separator}"),
        (false, false) => String::new(),
    };
    let (mut selected, mut number) = (0u64, 0u64);
    let mut held: VecDeque<(u64, Vec<u8>)> = VecDeque::new();
    // The last line printed, and how many lines after a match are still due.
    let (mut last_printed, mut trailing) = (0u64, 0usize);
    let mut first_group = true;
    let mut binary_match = false;
    each_line(reader, shell, |line, _| {
        number += 1;
        let matched = search.matcher.is_match(line) != search.invert;
        if !matched {
            if search.mode == Mode::Lines && trailing > 0 {
                trailing -= 1;
                stdout.write_all(prefix('-', number).as_bytes())?;
                stdout.write_all(line)?;
                stdout.write_all(b"\n")?;
                last_printed = number;
            } else if search.before > 0 {
                held.push_back((number, line.to_vec()));
                if held.len() > search.before {
                    held.pop_front();
                }
            }
            return Ok(true);
        }
        selected += 1;
        match search.mode {
            Mode::Quiet | Mode::Names => return Ok(false),
            Mode::NamesWithout | Mode::Count => {}
            Mode::Lines if binary => {
                binary_match = true;
                return Ok(false);
            }
            Mode::Lines => {
                let context = search.before > 0 || search.after > 0;
                let first_held = held.front().map_or(number, |(held, _)| *held);
                let apart =
                    std::mem::replace(&mut first_group, false) || first_held > last_printed + 1;
                if context && *printed_before && apart {
                    stdout.write_all(b"--\n")?;
                }
                for (held_number, held_line) in held.drain(..) {
                    stdout.write_all(prefix('-', held_number).as_bytes())?;
                    stdout.write_all(&held_line)?;
                    stdout.write_all(b"\n")?;
                }
                if search.only_matching {
                    for found in search.matcher.find_iter(line) {
                        if !search.invert && !found.as_bytes().is_empty() {
                            stdout.write_all(prefix(':', number).as_bytes())?;
                            stdout.write_all(found.as_bytes())?;
                            stdout.write_all(b"\n")?;
                        }
                    }
                } else {
                    stdout.write_all(prefix(':', number).as_bytes())?;
                    stdout.write_all(line)?;
                    stdout.write_all(b"\n")?;
                }
                last_printed = number;
                *printed_before = true;
                trailing = search.after;
            }
        }
        Ok(search.limit.is_none_or(|limit| selected < limit))
    })?;
    if binary_match {
        stdout.flush()?;
        writeln!(stderr, "grep: {shown}: binary file matches")?;
    }
    match search.mode {
        Mode::Count if search.with_names => writeln!(stdout, "{shown}:{selected}")?,
        Mode::Count => writeln!(stdout, "{selected}")?,
        Mode::Names if selected > 0 => writeln!(stdout, "{shown}")?,
        Mode::NamesWithout if selected == 0 => writeln!(stdout, "{shown}")?,
        Mode::Names | Mode::NamesWithout | Mode::Lines | Mode::Quiet => {}
    }
    Ok(selected > 0)
}

/// Collect the files under `directory` in name order, links not followed.
fn walk(
    shell: View<'_>,
    directory: &Path,
    shown: &str,
    keep_file: &dyn Fn(&str) -> bool,
    keep_directory: &dyn Fn(&str) -> bool,
    files: &mut Vec<String>,
) -> Result<(), Failure> {
    shell.alive()?;
    let mut entries = std::fs::read_dir(directory)?.collect::<Result<Vec<_>, _>>()?;
    entries.sort_by_key(std::fs::DirEntry::file_name);
    for entry in entries {
        let name = entry.file_name().to_string_lossy().into_owned();
        let below = if shown.is_empty() {
            name.clone()
        } else {
            format!("{}/{name}", shown.trim_end_matches('/'))
        };
        let kind = entry.file_type()?;
        if kind.is_dir() {
            if keep_directory(&name) {
                walk(
                    shell,
                    &entry.path(),
                    &below,
                    keep_file,
                    keep_directory,
                    files,
                )?;
            }
        } else if kind.is_file() && keep_file(&name) {
            files.push(below);
        }
    }
    Ok(())
}

pub(super) fn grep(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut syntax = Syntax::Basic;
    let (mut ignore_case, mut invert, mut line_numbers, mut only_matching) =
        (false, false, false, false);
    let (mut word, mut whole_line, mut recursive, mut silent) = (false, false, false, false);
    let (mut text, mut skip_binary) = (false, false);
    let mut mode = Mode::Lines;
    let mut names: Option<bool> = None;
    let (mut limit, mut before, mut after) = (None, 0usize, 0usize);
    let mut patterns: Option<Vec<String>> = None;
    let mut pattern_files = Vec::new();
    let (mut include, mut exclude, mut exclude_dir) = (Vec::new(), Vec::new(), Vec::new());
    let mut operands = Vec::new();
    let number = |value: &str| {
        value
            .parse::<usize>()
            .map_err(|_| Failure::Unsupported(format!("the count {value}")))
    };
    let glob = |pattern: &str| {
        globset::Glob::new(pattern)
            .map(|glob| glob.compile_matcher())
            .map_err(|_| Failure::Unsupported(format!("the glob {pattern}")))
    };
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('E') | Arg::Long("extended-regexp") => syntax = Syntax::Extended,
            Arg::Short('F') | Arg::Long("fixed-strings") => syntax = Syntax::Fixed,
            Arg::Short('G') | Arg::Long("basic-regexp") => syntax = Syntax::Basic,
            Arg::Short('i' | 'y') | Arg::Long("ignore-case") => ignore_case = true,
            Arg::Short('v') | Arg::Long("invert-match") => invert = true,
            Arg::Short('n') | Arg::Long("line-number") => line_numbers = true,
            Arg::Short('c') | Arg::Long("count") => mode = Mode::Count,
            Arg::Short('l') | Arg::Long("files-with-matches") => mode = Mode::Names,
            Arg::Short('L') | Arg::Long("files-without-match") => mode = Mode::NamesWithout,
            Arg::Short('q') | Arg::Long("quiet" | "silent") => mode = Mode::Quiet,
            Arg::Short('r' | 'R') | Arg::Long("recursive" | "dereference-recursive") => {
                recursive = true;
            }
            Arg::Short('w') | Arg::Long("word-regexp") => word = true,
            Arg::Short('x') | Arg::Long("line-regexp") => whole_line = true,
            Arg::Short('o') | Arg::Long("only-matching") => only_matching = true,
            Arg::Short('s') | Arg::Long("no-messages") => silent = true,
            Arg::Short('h') | Arg::Long("no-filename") => names = Some(false),
            Arg::Short('H') | Arg::Long("with-filename") => names = Some(true),
            Arg::Short('a') | Arg::Long("text") => text = true,
            Arg::Short('I') => skip_binary = true,
            Arg::Short('e') | Arg::Long("regexp") => patterns
                .get_or_insert_with(Vec::new)
                .extend(options.value()?.split('\n').map(str::to_string)),
            Arg::Short('f') | Arg::Long("file") => pattern_files.push(options.value()?),
            Arg::Short('m') | Arg::Long("max-count") => {
                limit = Some(number(options.value()?)? as u64);
            }
            Arg::Short('A') | Arg::Long("after-context") => after = number(options.value()?)?,
            Arg::Short('B') | Arg::Long("before-context") => before = number(options.value()?)?,
            Arg::Short('C') | Arg::Long("context") => {
                before = number(options.value()?)?;
                after = before;
            }
            Arg::Short(digit) if digit.is_ascii_digit() => {
                let rest = options.attached().unwrap_or_default();
                before = number(&format!("{digit}{rest}"))?;
                after = before;
            }
            Arg::Long("include") => include.push(glob(options.value()?)?),
            Arg::Long("exclude") => exclude.push(glob(options.value()?)?),
            Arg::Long("exclude-dir") => exclude_dir.push(glob(options.value()?)?),
            Arg::Long("color" | "colour") => {
                if !matches!(options.value()?, "never" | "auto") {
                    return Err(Failure::Unsupported("--color=always".to_string()));
                }
            }
            Arg::Operand(operand) => operands.push(operand),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    let Invocation {
        stdin,
        stdout,
        stderr,
        shell,
        ..
    } = invocation;
    let shell = *shell;
    for file in pattern_files {
        let mut reader = shell.open(&mut **stdin, file)?;
        let list = patterns.get_or_insert_with(Vec::new);
        each_line(&mut reader, shell, |line, _| {
            list.push(String::from_utf8_lossy(line).into_owned());
            Ok(true)
        })?;
    }
    let patterns = match patterns {
        Some(patterns) => patterns,
        None if operands.is_empty() => {
            writeln!(
                stderr,
                "Usage: grep [OPTION]... PATTERNS [FILE]...\nTry 'grep --help' for more information."
            )?;
            return Ok(2);
        }
        None => operands.remove(0).split('\n').map(str::to_string).collect(),
    };
    let matcher = compile(&patterns, syntax, ignore_case, |pattern| {
        if whole_line {
            format!("^(?:{pattern})$")
        } else if word {
            format!(r"\b{{start-half}}(?:{pattern})\b{{end-half}}")
        } else {
            pattern.to_string()
        }
    })?;
    // What is searched, by the name it is shown under.
    let mut files: Vec<String> = Vec::new();
    let mut status_error = false;
    let keep_file = |name: &str| {
        (include.is_empty() || include.iter().any(|glob| glob.is_match(name)))
            && !exclude.iter().any(|glob| glob.is_match(name))
    };
    let keep_directory = |name: &str| !exclude_dir.iter().any(|glob| glob.is_match(name));
    if operands.is_empty() {
        if recursive {
            walk(
                shell,
                &shell.path("."),
                "",
                &keep_file,
                &keep_directory,
                &mut files,
            )?;
        } else {
            files.push("-".to_string());
        }
    }
    for operand in &operands {
        let path = shell.path(operand);
        if *operand != "-" && path.is_dir() {
            if recursive {
                walk(
                    shell,
                    &path,
                    operand,
                    &keep_file,
                    &keep_directory,
                    &mut files,
                )?;
            } else {
                if !silent {
                    writeln!(stderr, "grep: {operand}: Is a directory")?;
                }
                status_error = true;
            }
        } else {
            files.push((*operand).to_string());
        }
    }
    let searching = Search {
        matcher,
        mode,
        invert,
        line_numbers,
        only_matching,
        limit,
        before,
        after,
        text,
        skip_binary,
        with_names: names.unwrap_or(recursive || operands.len() > 1),
    };
    let (mut any, mut printed_before) = (false, false);
    for file in files {
        shell.alive()?;
        let mut reader = match shell.open(&mut **stdin, &file) {
            Ok(reader) => reader,
            Err(Failure::Io(error)) => {
                if !silent {
                    stdout.flush()?;
                    writeln!(stderr, "grep: {file}: {}", reason(&error))?;
                }
                status_error = true;
                continue;
            }
            Err(other) => return Err(other),
        };
        let shown = if file == "-" {
            "(standard input)"
        } else {
            &file
        };
        if search(
            &searching,
            shell,
            &mut reader,
            shown,
            stdout,
            stderr,
            &mut printed_before,
        )? {
            any = true;
            if mode == Mode::Quiet {
                return Ok(0);
            }
        }
    }
    Ok(if status_error { 2 } else { i32::from(!any) })
}
