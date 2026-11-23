//! Files and their contents: `cat`, `head`, `tail`, `wc`, `tee`, `touch`,
//! `mkdir`, `rmdir`, `rm`, `cp`, `mv`, `basename`, `dirname`, `realpath`.
//! Each implements the options named in its function and words its output
//! and errors as GNU coreutils does.

use std::collections::VecDeque;
use std::io::{BufRead, Read, Write};
use std::path::{Path, PathBuf};

use super::{each_line, quoted, reason, Arg, Failure, Invocation, Options, Outcome, View};

/// A count option's value: digits only.
fn count(value: &str) -> Result<u64, Failure> {
    value
        .parse()
        .map_err(|_| Failure::Unsupported(format!("the count {value}")))
}

/// The usage error of a utility called without what it needs.
fn missing(invocation: &mut Invocation<'_>, what: &str) -> Outcome {
    let name = invocation.name;
    writeln!(
        invocation.stderr,
        "{name}: missing {what}\nTry '{name} --help' for more information."
    )?;
    Ok(1)
}

/// `cat [-n] [FILE...]`
pub(super) fn cat(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let (mut number, mut files) = (false, Vec::new());
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('n') | Arg::Long("number") => number = true,
            Arg::Operand(file) => files.push(file),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    if files.is_empty() {
        files.push("-");
    }
    let Invocation {
        name,
        stdin,
        stdout,
        stderr,
        shell,
        ..
    } = invocation;
    let (mut status, mut line_number) = (0, 0u64);
    for file in files {
        shell.alive()?;
        let mut reader = match shell.open(&mut **stdin, file) {
            Ok(reader) => reader,
            Err(Failure::Io(error)) => {
                // `cat` writes as it reads: what it has copied comes first.
                stdout.flush()?;
                writeln!(stderr, "{name}: {file}: {}", reason(&error))?;
                status = 1;
                continue;
            }
            Err(other) => return Err(other),
        };
        if number {
            each_line(&mut reader, *shell, |line, terminated| {
                line_number += 1;
                write!(stdout, "{line_number:6}\t")?;
                stdout.write_all(line)?;
                if terminated {
                    stdout.write_all(b"\n")?;
                }
                Ok(true)
            })?;
        } else {
            let mut chunk = vec![0u8; 64 * 1024];
            loop {
                let read = reader.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                shell.alive()?;
                stdout.write_all(&chunk[..read])?;
            }
        }
    }
    Ok(status)
}

/// Which part of its input `head` or `tail` keeps.
#[derive(Clone, Copy)]
enum Part {
    /// `head -n N`, `tail -n +N` (from line N on).
    Lines(u64),
    /// `head -n -N` (all but the last N), `tail -n N` (the last N).
    LinesFromEnd(u64),
    /// `head -c N`, `tail -c +N`.
    Bytes(u64),
    /// `tail -c N`.
    BytesFromEnd(u64),
}

/// The options `head` and `tail` share: `-n N`, `-N`, `-c N`, `-q`, `-v`,
/// with `relative` deciding what a sign on the count means.
fn head_or_tail_options(
    args: &[String],
    relative: fn(bytes: bool, sign: Option<char>, count: u64) -> Result<Part, Failure>,
) -> Result<(Part, Option<bool>, Vec<&str>), Failure> {
    let mut options = Options::new(args);
    let (mut part, mut headers, mut files) = (relative(false, None, 10)?, None, Vec::new());
    let parse = |bytes: bool, value: &str| {
        let sign = value
            .chars()
            .next()
            .filter(|sign| matches!(sign, '+' | '-'));
        relative(
            bytes,
            sign,
            count(&value[sign.map_or(0, char::len_utf8)..])?,
        )
    };
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('n') | Arg::Long("lines") => part = parse(false, options.value()?)?,
            Arg::Short('c') | Arg::Long("bytes") => part = parse(true, options.value()?)?,
            Arg::Short('q') | Arg::Long("quiet" | "silent") => headers = Some(false),
            Arg::Short('v') | Arg::Long("verbose") => headers = Some(true),
            // `-5`: the historical spelling of `-n 5`.
            Arg::Short(digit) if digit.is_ascii_digit() => {
                let rest = options.attached().unwrap_or_default();
                part = relative(false, None, count(&format!("{digit}{rest}"))?)?;
            }
            Arg::Operand(file) => files.push(file),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    if files.is_empty() {
        files.push("-");
    }
    Ok((part, headers, files))
}

/// Run `head` or `tail` over its files, with the `==> name <==` headers.
fn head_or_tail(
    invocation: &mut Invocation<'_>,
    (part, headers, files): (Part, Option<bool>, Vec<&str>),
) -> Outcome {
    let Invocation {
        name,
        stdin,
        stdout,
        stderr,
        shell,
        ..
    } = invocation;
    let headers = headers.unwrap_or(files.len() > 1);
    let (mut status, mut first) = (0, true);
    for file in files {
        shell.alive()?;
        let mut reader = match shell.open(&mut **stdin, file) {
            Ok(reader) => reader,
            Err(Failure::Io(error)) => {
                writeln!(
                    stderr,
                    "{name}: cannot open {} for reading: {}",
                    quoted(file),
                    reason(&error)
                )?;
                status = 1;
                continue;
            }
            Err(other) => return Err(other),
        };
        if headers {
            let shown = if file == "-" { "standard input" } else { file };
            writeln!(stdout, "{}==> {shown} <==", if first { "" } else { "\n" })?;
        }
        first = false;
        match part {
            Part::Lines(lines) if *name == "head" => {
                let mut left = lines;
                if left > 0 {
                    each_line(&mut reader, *shell, |line, terminated| {
                        stdout.write_all(line)?;
                        if terminated {
                            stdout.write_all(b"\n")?;
                        }
                        left -= 1;
                        Ok(left > 0)
                    })?;
                }
            }
            Part::Lines(from) => {
                let mut number = 0u64;
                each_line(&mut reader, *shell, |line, terminated| {
                    number += 1;
                    if number >= from {
                        stdout.write_all(line)?;
                        if terminated {
                            stdout.write_all(b"\n")?;
                        }
                    }
                    Ok(true)
                })?;
            }
            Part::LinesFromEnd(lines) => {
                // `head`: every line once `lines` newer ones exist. `tail`:
                // what is left in the window at the end.
                let heading = *name == "head";
                let mut window: VecDeque<(Vec<u8>, bool)> = VecDeque::new();
                each_line(&mut reader, *shell, |line, terminated| {
                    window.push_back((line.to_vec(), terminated));
                    if window.len() as u64 > lines {
                        if let (Some((old, ended)), true) = (window.pop_front(), heading) {
                            stdout.write_all(&old)?;
                            if ended {
                                stdout.write_all(b"\n")?;
                            }
                        }
                    }
                    Ok(true)
                })?;
                if !heading {
                    for (line, terminated) in window {
                        stdout.write_all(&line)?;
                        if terminated {
                            stdout.write_all(b"\n")?;
                        }
                    }
                }
            }
            Part::Bytes(bytes) if *name == "head" => {
                std::io::copy(&mut reader.take(bytes), stdout)?;
            }
            Part::Bytes(from) => {
                std::io::copy(
                    &mut (&mut reader).take(from.saturating_sub(1)),
                    &mut std::io::sink(),
                )?;
                std::io::copy(&mut reader, stdout)?;
            }
            Part::BytesFromEnd(bytes) => {
                let mut all = Vec::new();
                reader.read_to_end(&mut all)?;
                let keep = usize::try_from(bytes).unwrap_or(usize::MAX).min(all.len());
                stdout.write_all(&all[all.len() - keep..])?;
            }
        }
    }
    Ok(status)
}

/// `head [-n [-]N | -N | -c N] [-q] [-v] [FILE...]`
pub(super) fn head(invocation: &mut Invocation<'_>) -> Outcome {
    let parsed = head_or_tail_options(invocation.args, |bytes, sign, count| match (bytes, sign) {
        (false, None | Some('+')) => Ok(Part::Lines(count)),
        (false, Some(_)) => Ok(Part::LinesFromEnd(count)),
        (true, None | Some('+')) => Ok(Part::Bytes(count)),
        (true, Some(_)) => Err(Failure::Unsupported("-c with a negative count".to_string())),
    })?;
    head_or_tail(invocation, parsed)
}

/// `tail [-n [+]N | -N | -c [+]N] [-q] [-v] [FILE...]`
pub(super) fn tail(invocation: &mut Invocation<'_>) -> Outcome {
    let parsed = head_or_tail_options(invocation.args, |bytes, sign, count| {
        Ok(match (bytes, sign) {
            (false, Some('+')) => Part::Lines(count),
            (false, _) => Part::LinesFromEnd(count),
            (true, Some('+')) => Part::Bytes(count),
            (true, _) => Part::BytesFromEnd(count),
        })
    })?;
    head_or_tail(invocation, parsed)
}

/// `wc [-l] [-w] [-c] [-m] [FILE...]`
pub(super) fn wc(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    // Lines, words, characters, bytes: the order they are printed in.
    let (mut shown, mut files) = ([false; 4], Vec::new());
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('l') | Arg::Long("lines") => shown[0] = true,
            Arg::Short('w') | Arg::Long("words") => shown[1] = true,
            Arg::Short('m') | Arg::Long("chars") => shown[2] = true,
            Arg::Short('c') | Arg::Long("bytes") => shown[3] = true,
            Arg::Operand(file) => files.push(file),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    if shown == [false; 4] {
        shown = [true, true, false, true];
    }
    let Invocation {
        name,
        stdin,
        stdout,
        stderr,
        shell,
        ..
    } = invocation;
    let named = !files.is_empty();
    if !named {
        files.push("-");
    }
    // GNU's column width: one count of one input is printed bare; otherwise
    // the digits of the files' total size, and at least 7 once an input's
    // size is unknown beforehand.
    let width = if shown.iter().filter(|shown| **shown).count() == 1 && files.len() == 1 {
        1
    } else {
        let mut total = 0u64;
        let mut least = 1;
        for file in &files {
            match std::fs::metadata(shell.path(file)) {
                Ok(metadata) if *file != "-" && metadata.is_file() => total += metadata.len(),
                Ok(_) | Err(_) => least = 7,
            }
        }
        total.to_string().len().max(least)
    };
    let (mut status, mut totals) = (0, [0u64; 4]);
    let inputs = files.len();
    for file in files {
        shell.alive()?;
        let mut reader = match shell.open(&mut **stdin, file) {
            Ok(reader) => reader,
            Err(Failure::Io(error)) => {
                writeln!(stderr, "{name}: {file}: {}", reason(&error))?;
                status = 1;
                continue;
            }
            Err(other) => return Err(other),
        };
        let (mut counts, mut in_word) = ([0u64; 4], false);
        loop {
            let chunk = reader.fill_buf()?;
            if chunk.is_empty() {
                break;
            }
            shell.alive()?;
            for byte in chunk {
                counts[0] += u64::from(*byte == b'\n');
                // Every byte but a UTF-8 continuation byte starts a character.
                counts[2] += u64::from(byte & 0xC0 != 0x80);
                let space = matches!(byte, b' ' | b'\n' | b'\t' | b'\r' | 0x0B | 0x0C);
                counts[1] += u64::from(!space && !in_word);
                in_word = !space;
            }
            counts[3] += chunk.len() as u64;
            let consumed = chunk.len();
            reader.consume(consumed);
        }
        let mut row: Vec<String> = Vec::new();
        for (index, count) in counts.iter().enumerate() {
            totals[index] += count;
            if shown[index] {
                row.push(format!("{count:>width$}"));
            }
        }
        let row = row.join(" ");
        if named {
            writeln!(stdout, "{row} {file}")?;
        } else {
            writeln!(stdout, "{row}")?;
        }
    }
    if inputs > 1 {
        let row: Vec<String> = (0..4)
            .filter(|index| shown[*index])
            .map(|index| format!("{:>width$}", totals[index]))
            .collect();
        writeln!(stdout, "{} total", row.join(" "))?;
    }
    Ok(status)
}

/// `tee [-a] [FILE...]`
pub(super) fn tee(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let (mut append, mut files) = (false, Vec::new());
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('a') | Arg::Long("append") => append = true,
            Arg::Operand(file) => files.push(file),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    let Invocation {
        name,
        stdin,
        stdout,
        stderr,
        shell,
        ..
    } = invocation;
    let (mut status, mut sinks) = (0, Vec::new());
    for file in files {
        let opened = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .append(append)
            .truncate(!append)
            .open(shell.path(file));
        match opened {
            Ok(sink) => sinks.push(sink),
            Err(error) => {
                writeln!(stderr, "{name}: {file}: {}", reason(&error))?;
                status = 1;
            }
        }
    }
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let read = stdin.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        shell.alive()?;
        stdout.write_all(&chunk[..read])?;
        // A reader that takes line by line sees each line as it is written.
        stdout.flush()?;
        for sink in &mut sinks {
            sink.write_all(&chunk[..read])?;
        }
    }
    Ok(status)
}

/// `touch [-c] FILE...`
pub(super) fn touch(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let (mut create, mut files) = (true, Vec::new());
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('c') | Arg::Long("no-create") => create = false,
            Arg::Operand(file) => files.push(file),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    if files.is_empty() {
        return missing(invocation, "file operand");
    }
    let mut status = 0;
    for file in files {
        let path = invocation.shell.path(file);
        let touched = if path.exists() {
            // A directory cannot be opened for writing: its times are set
            // through a read handle.
            std::fs::File::open(&path)
                .and_then(|handle| handle.set_modified(std::time::SystemTime::now()))
        } else if create {
            std::fs::File::create(&path).map(drop)
        } else {
            Ok(())
        };
        if let Err(error) = touched {
            let quoted = quoted(file);
            writeln!(
                invocation.stderr,
                "touch: cannot touch {quoted}: {}",
                reason(&error)
            )?;
            status = 1;
        }
    }
    Ok(status)
}

/// `mkdir [-p] DIRECTORY...`
pub(super) fn mkdir(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let (mut parents, mut directories) = (false, Vec::new());
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('p') | Arg::Long("parents") => parents = true,
            Arg::Operand(directory) => directories.push(directory),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    if directories.is_empty() {
        return missing(invocation, "operand");
    }
    let mut status = 0;
    for directory in directories {
        let path = invocation.shell.path(directory);
        let made = if parents {
            std::fs::create_dir_all(&path)
        } else {
            std::fs::create_dir(&path)
        };
        if let Err(error) = made {
            let quoted = quoted(directory);
            writeln!(
                invocation.stderr,
                "mkdir: cannot create directory {quoted}: {}",
                reason(&error)
            )?;
            status = 1;
        }
    }
    Ok(status)
}

/// `rmdir DIRECTORY...`
pub(super) fn rmdir(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut directories = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Operand(directory) => directories.push(directory),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    if directories.is_empty() {
        return missing(invocation, "operand");
    }
    let mut status = 0;
    for directory in directories {
        if let Err(error) = std::fs::remove_dir(invocation.shell.path(directory)) {
            let quoted = quoted(directory);
            writeln!(
                invocation.stderr,
                "rmdir: failed to remove {quoted}: {}",
                reason(&error)
            )?;
            status = 1;
        }
    }
    Ok(status)
}

/// `rm [-r] [-R] [-f] FILE...`
pub(super) fn rm(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let (mut recursive, mut force, mut files) = (false, false, Vec::new());
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('r' | 'R') | Arg::Long("recursive") => recursive = true,
            Arg::Short('f') | Arg::Long("force") => force = true,
            Arg::Operand(file) => files.push(file),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    if files.is_empty() {
        return if force {
            Ok(0)
        } else {
            missing(invocation, "operand")
        };
    }
    let mut status = 0;
    for file in files {
        invocation.shell.alive()?;
        let quoted = quoted(file);
        let trimmed = file.trim_end_matches('/');
        let last = trimmed.rsplit('/').next().unwrap_or(trimmed);
        if matches!(last, "." | "..") {
            writeln!(
                invocation.stderr,
                "rm: refusing to remove '.' or '..' directory: skipping {quoted}"
            )?;
            status = 1;
            continue;
        }
        let path = invocation.shell.path(file);
        if recursive && path.parent().is_none() {
            writeln!(
                invocation.stderr,
                "rm: it is dangerous to operate recursively on {quoted}"
            )?;
            status = 1;
            continue;
        }
        // A link is removed, never followed.
        let removed = match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() && recursive => std::fs::remove_dir_all(&path),
            Ok(metadata) if metadata.is_dir() => Err(std::io::Error::new(
                std::io::ErrorKind::IsADirectory,
                "Is a directory",
            )),
            Ok(_) => std::fs::remove_file(&path),
            Err(error) => Err(error),
        };
        match removed {
            Ok(()) => {}
            Err(error) if force && error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                writeln!(
                    invocation.stderr,
                    "rm: cannot remove {quoted}: {}",
                    reason(&error)
                )?;
                status = 1;
            }
        }
    }
    Ok(status)
}

/// Copy `from` to `to`: a file's bytes and permissions, a link as a link,
/// a directory with everything under it.
fn copy_tree(shell: View<'_>, from: &Path, to: &Path) -> Result<(), Failure> {
    shell.alive()?;
    let metadata = std::fs::symlink_metadata(from)?;
    if metadata.is_dir() {
        match std::fs::create_dir(to) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && to.is_dir() => {}
            Err(error) => return Err(error.into()),
        }
        let mut entries = std::fs::read_dir(from)?.collect::<Result<Vec<_>, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            copy_tree(shell, &entry.path(), &to.join(entry.file_name()))?;
        }
        // After its contents, so a read-only directory can still be filled.
        std::fs::set_permissions(to, metadata.permissions())?;
        return Ok(());
    }
    #[cfg(unix)]
    if metadata.file_type().is_symlink() {
        // An existing destination is replaced, as `cp` replaces a file.
        let _ = std::fs::remove_file(to);
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)?;
        return Ok(());
    }
    std::fs::copy(from, to)?;
    Ok(())
}

/// What `cp` and `mv` share: their sources, each with where it goes. `Err`
/// carries a usage error's message.
fn sources_and_targets(
    shell: View<'_>,
    name: &str,
    mut operands: Vec<&str>,
) -> Result<Vec<(String, PathBuf, String)>, String> {
    let Some(target) = operands.pop() else {
        return Err(format!(
            "{name}: missing file operand\nTry '{name} --help' for more information."
        ));
    };
    if operands.is_empty() {
        return Err(format!(
            "{name}: missing destination file operand after {}\nTry '{name} --help' for more information.",
            quoted(target)
        ));
    }
    let into_directory = shell.path(target).is_dir();
    if operands.len() > 1 && !into_directory {
        return Err(if shell.path(target).exists() {
            format!("{name}: target {} is not a directory", quoted(target))
        } else {
            format!(
                "{name}: target {}: No such file or directory",
                quoted(target)
            )
        });
    }
    Ok(operands
        .into_iter()
        .map(|source| {
            let shown = if into_directory {
                let last = Path::new(source.trim_end_matches('/'))
                    .file_name()
                    .map_or_else(
                        || source.to_string(),
                        |last| last.to_string_lossy().into_owned(),
                    );
                format!("{}/{last}", target.trim_end_matches('/'))
            } else {
                target.to_string()
            };
            (source.to_string(), shell.path(&shown), shown)
        })
        .collect())
}

/// `cp [-r] [-R] [-f] SOURCE... DESTINATION`
pub(super) fn cp(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let (mut recursive, mut operands) = (false, Vec::new());
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('r' | 'R') | Arg::Long("recursive") => recursive = true,
            Arg::Short('f') | Arg::Long("force") => {}
            Arg::Operand(operand) => operands.push(operand),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    let shell = invocation.shell;
    let pairs = match sources_and_targets(shell, "cp", operands) {
        Ok(pairs) => pairs,
        Err(usage) => {
            writeln!(invocation.stderr, "{usage}")?;
            return Ok(1);
        }
    };
    let mut status = 0;
    for (source, target, shown) in pairs {
        let from = shell.path(&source);
        let copied = match std::fs::metadata(&from) {
            Err(error) => Err(format!(
                "cp: cannot stat {}: {}",
                quoted(&source),
                reason(&error)
            )),
            Ok(metadata) if metadata.is_dir() && !recursive => Err(format!(
                "cp: -r not specified; omitting directory {}",
                quoted(&source)
            )),
            Ok(_) if std::fs::canonicalize(&from).ok() == std::fs::canonicalize(&target).ok() => {
                Err(format!(
                    "cp: {} and {} are the same file",
                    quoted(&source),
                    quoted(&shown)
                ))
            }
            Ok(_) => match copy_tree(shell, &from, &target) {
                Ok(()) => Ok(()),
                Err(Failure::Io(error)) => Err(format!(
                    "cp: cannot create regular file {}: {}",
                    quoted(&shown),
                    reason(&error)
                )),
                Err(other) => return Err(other),
            },
        };
        if let Err(message) = copied {
            writeln!(invocation.stderr, "{message}")?;
            status = 1;
        }
    }
    Ok(status)
}

/// `mv [-f] SOURCE... DESTINATION`
pub(super) fn mv(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut operands = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('f') | Arg::Long("force") => {}
            Arg::Operand(operand) => operands.push(operand),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    let shell = invocation.shell;
    let pairs = match sources_and_targets(shell, "mv", operands) {
        Ok(pairs) => pairs,
        Err(usage) => {
            writeln!(invocation.stderr, "{usage}")?;
            return Ok(1);
        }
    };
    let mut status = 0;
    for (source, target, shown) in pairs {
        let from = shell.path(&source);
        let moved = match std::fs::symlink_metadata(&from) {
            Err(error) => Err(format!(
                "mv: cannot stat {}: {}",
                quoted(&source),
                reason(&error)
            )),
            Ok(metadata) => match std::fs::rename(&from, &target) {
                Ok(()) => Ok(()),
                // Another filesystem: copied over, then removed.
                Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
                    let copied = copy_tree(shell, &from, &target).and_then(|()| {
                        if metadata.is_dir() {
                            Ok(std::fs::remove_dir_all(&from)?)
                        } else {
                            Ok(std::fs::remove_file(&from)?)
                        }
                    });
                    match copied {
                        Ok(()) => Ok(()),
                        Err(Failure::Io(error)) => Err(error),
                        Err(other) => return Err(other),
                    }
                }
                Err(error) => Err(error),
            }
            .map_err(|error| {
                format!(
                    "mv: cannot move {} to {}: {}",
                    quoted(&source),
                    quoted(&shown),
                    reason(&error)
                )
            }),
        };
        if let Err(message) = moved {
            writeln!(invocation.stderr, "{message}")?;
            status = 1;
        }
    }
    Ok(status)
}

/// A path's last component and what precedes it, as `basename` and
/// `dirname` cut them.
fn split_name(path: &str) -> (&str, &str) {
    let trimmed = path.trim_end_matches('/');
    if trimmed.is_empty() {
        // Only slashes, or nothing at all.
        let root = if path.is_empty() { "." } else { "/" };
        return (root, if path.is_empty() { "" } else { "/" });
    }
    match trimmed.rfind('/') {
        Some(at) => {
            let directory = trimmed[..at].trim_end_matches('/');
            (
                if directory.is_empty() { "/" } else { directory },
                &trimmed[at + 1..],
            )
        }
        None => (".", trimmed),
    }
}

/// `basename NAME [SUFFIX]`
pub(super) fn basename(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut operands = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Operand(operand) => operands.push(operand),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    match operands.as_slice() {
        [] => missing(invocation, "operand"),
        [name] => {
            writeln!(invocation.stdout, "{}", split_name(name).1)?;
            Ok(0)
        }
        [name, suffix] => {
            let base = split_name(name).1;
            let cut = base
                .strip_suffix(suffix)
                .filter(|cut| !cut.is_empty())
                .unwrap_or(base);
            writeln!(invocation.stdout, "{cut}")?;
            Ok(0)
        }
        [_, _, extra, ..] => {
            let quoted = quoted(extra);
            writeln!(
                invocation.stderr,
                "basename: extra operand {quoted}\nTry 'basename --help' for more information."
            )?;
            Ok(1)
        }
    }
}

/// `dirname NAME...`
pub(super) fn dirname(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut operands = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Operand(operand) => operands.push(operand),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    if operands.is_empty() {
        return missing(invocation, "operand");
    }
    for operand in operands {
        writeln!(invocation.stdout, "{}", split_name(operand).0)?;
    }
    Ok(0)
}

/// `realpath FILE...`: every component but the last must exist.
pub(super) fn realpath(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut operands = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Operand(operand) => operands.push(operand),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    if operands.is_empty() {
        return missing(invocation, "operand");
    }
    let mut status = 0;
    for operand in operands {
        let path = invocation.shell.path(operand);
        let resolved =
            std::fs::canonicalize(&path).or_else(|error| match (path.parent(), path.file_name()) {
                (Some(parent), Some(last)) if error.kind() == std::io::ErrorKind::NotFound => {
                    Ok(std::fs::canonicalize(parent)?.join(last))
                }
                (_, _) => Err(error),
            });
        match resolved {
            Ok(resolved) => {
                let text = resolved.to_string_lossy();
                // Windows spells a canonical path with its verbatim prefix.
                writeln!(
                    invocation.stdout,
                    "{}",
                    text.strip_prefix(r"\\?\").unwrap_or(&text)
                )?;
            }
            Err(error) => {
                writeln!(invocation.stderr, "realpath: {operand}: {}", reason(&error))?;
                status = 1;
            }
        }
    }
    Ok(status)
}

/// One clause of a symbolic mode: `[ugoa]*[+-=][rwxX]*`.
#[cfg(unix)]
struct Clause {
    /// The permission bits of the classes it names, as a mask.
    who: u32,
    /// Whether no class was named: the bits are then cut by the umask.
    implied: bool,
    operator: char,
    /// The permissions as bits of one class (4, 2, 1).
    bits: u32,
    /// `X`: execute only for a directory or a file some class may execute.
    conditional_execute: bool,
}

/// `chmod [-R] MODE FILE...`: an octal mode, or clauses `[ugoa]*[+-=][rwxX]*`
/// joined by commas.
#[cfg(unix)]
pub(super) fn chmod(invocation: &mut Invocation<'_>) -> Outcome {
    use std::os::unix::fs::PermissionsExt;
    // A mode may start with a dash (`-x`): only -R is an option here.
    let (mut recursive, mut operands) = (false, Vec::new());
    for arg in invocation.args {
        match arg.as_str() {
            "-R" | "--recursive" => recursive = true,
            long if long.starts_with("--") => {
                return Err(Failure::Unsupported(format!("option {long}")))
            }
            operand => operands.push(operand),
        }
    }
    if let [only] = operands.as_slice() {
        let what = format!("operand after {}", quoted(only));
        return missing(invocation, &what);
    }
    if operands.is_empty() {
        return missing(invocation, "operand");
    }
    let mode = operands.remove(0);
    let octal = u32::from_str_radix(mode, 8)
        .ok()
        .filter(|bits| *bits <= 0o7777);
    let mut clauses = Vec::new();
    if octal.is_none() {
        for text in mode.split(',') {
            let Some(at) = text.find(['+', '-', '=']) else {
                return Err(Failure::Unsupported(format!("the mode {mode}")));
            };
            let mut who = 0;
            for class in text[..at].chars() {
                who |= match class {
                    'u' => 0o700,
                    'g' => 0o070,
                    'o' => 0o007,
                    'a' => 0o777,
                    _ => return Err(Failure::Unsupported(format!("the mode {mode}"))),
                };
            }
            let (mut bits, mut conditional_execute) = (0, false);
            for permission in text[at + 1..].chars() {
                match permission {
                    'r' => bits |= 4,
                    'w' => bits |= 2,
                    'x' => bits |= 1,
                    'X' => conditional_execute = true,
                    _ => return Err(Failure::Unsupported(format!("the mode {mode}"))),
                }
            }
            clauses.push(Clause {
                implied: who == 0,
                who: if who == 0 { 0o777 } else { who },
                operator: text[at..].chars().next().unwrap_or('+'),
                bits,
                conditional_execute,
            });
        }
    }
    // A clause that names no class is cut by the process's umask, which
    // only Linux lets a process read without changing it.
    let umask = if clauses.iter().any(|clause| clause.implied) {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let read = status
            .lines()
            .find_map(|line| line.strip_prefix("Umask:"))
            .and_then(|value| u32::from_str_radix(value.trim(), 8).ok());
        match read {
            Some(umask) => umask,
            None => return Err(Failure::Unsupported(format!("the mode {mode}"))),
        }
    } else {
        0
    };
    let shell = invocation.shell;
    let mut status = 0;
    let mut pending: Vec<String> = operands
        .iter()
        .rev()
        .map(|file| (*file).to_string())
        .collect();
    while let Some(file) = pending.pop() {
        shell.alive()?;
        let path = shell.path(&file);
        let changed = std::fs::metadata(&path).and_then(|metadata| {
            let current = metadata.permissions().mode() & 0o7777;
            let mut wanted = octal.unwrap_or(current);
            for clause in &clauses {
                let mut bits = clause.bits;
                if clause.conditional_execute && (metadata.is_dir() || current & 0o111 != 0) {
                    bits |= 1;
                }
                // The class bits, in each class the clause names.
                let mut mask = (bits << 6 | bits << 3 | bits) & clause.who;
                if clause.implied {
                    mask &= !umask;
                }
                match clause.operator {
                    '+' => wanted |= mask,
                    '-' => wanted &= !mask,
                    _ => wanted = (wanted & !clause.who) | mask,
                }
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(wanted))?;
            Ok(metadata.is_dir())
        });
        match changed {
            Ok(true) if recursive => {
                let mut below: Vec<String> = std::fs::read_dir(&path)?
                    .filter_map(Result::ok)
                    // A link's target is not changed by a walk.
                    .filter(|entry| entry.file_type().is_ok_and(|kind| !kind.is_symlink()))
                    .map(|entry| {
                        format!(
                            "{}/{}",
                            file.trim_end_matches('/'),
                            entry.file_name().to_string_lossy()
                        )
                    })
                    .collect();
                below.sort_by(|left, right| right.cmp(left));
                pending.extend(below);
            }
            Ok(_) => {}
            Err(error) => {
                writeln!(
                    invocation.stderr,
                    "chmod: cannot access {}: {}",
                    quoted(&file),
                    reason(&error)
                )?;
                status = 1;
            }
        }
    }
    Ok(status)
}

/// `chmod MODE FILE...` where files have no permission bits: the files must
/// exist, and nothing changes.
#[cfg(not(unix))]
pub(super) fn chmod(invocation: &mut Invocation<'_>) -> Outcome {
    let files: Vec<&String> = invocation
        .args
        .iter()
        .filter(|arg| !matches!(arg.as_str(), "-R" | "--recursive"))
        .skip(1)
        .collect();
    if files.is_empty() {
        return missing(invocation, "operand");
    }
    let mut status = 0;
    for file in files {
        if let Err(error) = std::fs::metadata(invocation.shell.path(file)) {
            writeln!(
                invocation.stderr,
                "chmod: cannot access {}: {}",
                quoted(file),
                reason(&error)
            )?;
            status = 1;
        }
    }
    Ok(status)
}
