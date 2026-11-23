//! Lines of text: `sort`, `uniq`, `cut`, `tr`, `seq`, `tac`, `yes`. Each
//! implements the options named in its function, as GNU coreutils does;
//! text is bytes throughout.

use std::cmp::Ordering;
use std::io::{Read, Write};

use super::{each_line, reason, Arg, Failure, Invocation, Options, Outcome};

/// How one `sort` key (or the whole line) is compared.
#[derive(Clone, Copy, Default)]
struct KeyFlags {
    numeric: bool,
    reverse: bool,
    fold_case: bool,
    skip_blanks: bool,
}

/// `-k FIRST[,LAST]`: the fields a key spans, counted from 1.
struct Key {
    first: usize,
    last: Option<usize>,
    flags: Option<KeyFlags>,
}

/// The number a line starts with, as `sort -n` reads it.
fn leading_number(text: &[u8]) -> f64 {
    let text = String::from_utf8_lossy(text);
    let text = text.trim_start_matches([' ', '\t']);
    let mut end = 0;
    let mut seen_point = false;
    for (index, character) in text.char_indices() {
        let accepted = character.is_ascii_digit()
            || (character == '-' && index == 0)
            || (character == '.' && !std::mem::replace(&mut seen_point, true));
        if !accepted {
            break;
        }
        end = index + character.len_utf8();
    }
    text[..end].parse().unwrap_or(0.0)
}

/// `sort [-r] [-n] [-u] [-f] [-b] [-s] [-t SEP] [-k FIRST[,LAST][bfnr]]... [-o FILE] [FILE...]`
pub(super) fn sort(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut global = KeyFlags::default();
    let (mut unique, mut stable) = (false, false);
    let mut separator: Option<u8> = None;
    let mut keys: Vec<Key> = Vec::new();
    let mut output = None;
    let mut files = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('r') | Arg::Long("reverse") => global.reverse = true,
            Arg::Short('n') | Arg::Long("numeric-sort") => global.numeric = true,
            Arg::Short('f') | Arg::Long("ignore-case") => global.fold_case = true,
            Arg::Short('b') | Arg::Long("ignore-leading-blanks") => global.skip_blanks = true,
            Arg::Short('u') | Arg::Long("unique") => unique = true,
            Arg::Short('s') | Arg::Long("stable") => stable = true,
            Arg::Short('o') | Arg::Long("output") => output = Some(options.value()?),
            Arg::Short('t') | Arg::Long("field-separator") => match options.value()?.as_bytes() {
                [byte] => separator = Some(*byte),
                _ => return Err(Failure::Unsupported("a multi-byte -t".to_string())),
            },
            Arg::Short('k') | Arg::Long("key") => {
                let spec = options.value()?;
                let position = |text: &str| -> Result<(usize, Option<KeyFlags>), Failure> {
                    let digits = text.trim_end_matches(|flag: char| flag.is_ascii_alphabetic());
                    let field = digits
                        .parse::<usize>()
                        .ok()
                        .filter(|field| *field > 0)
                        .ok_or_else(|| Failure::Unsupported(format!("the key {spec}")))?;
                    let mut flags = None;
                    for flag in text[digits.len()..].chars() {
                        let flags = flags.get_or_insert_with(KeyFlags::default);
                        match flag {
                            'n' => flags.numeric = true,
                            'r' => flags.reverse = true,
                            'f' => flags.fold_case = true,
                            'b' => flags.skip_blanks = true,
                            other => {
                                return Err(Failure::Unsupported(format!("the key flag {other}")))
                            }
                        }
                    }
                    Ok((field, flags))
                };
                let (first, last) = match spec.split_once(',') {
                    Some((first, last)) => (position(first)?, Some(position(last)?)),
                    None => (position(spec)?, None),
                };
                keys.push(Key {
                    first: first.0,
                    last: last.as_ref().map(|last| last.0),
                    flags: first.1.or(last.and_then(|last| last.1)),
                });
            }
            Arg::Operand(file) => files.push(file),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    invocation.shell.sorts_by_bytes("sort")?;
    if files.is_empty() {
        files.push("-");
    }
    let Invocation {
        stdin,
        stdout,
        stderr,
        shell,
        ..
    } = invocation;
    let shell = *shell;
    let mut lines: Vec<Vec<u8>> = Vec::new();
    for file in files {
        let mut reader = match shell.open(&mut **stdin, file) {
            Ok(reader) => reader,
            Err(Failure::Io(error)) => {
                writeln!(stderr, "sort: cannot read: {file}: {}", reason(&error))?;
                return Ok(2);
            }
            Err(other) => return Err(other),
        };
        each_line(&mut reader, shell, |line, _| {
            lines.push(line.to_vec());
            Ok(true)
        })?;
    }
    // The bytes of fields `first..=last` of `line`.
    let span = |line: &'_ [u8], first: usize, last: Option<usize>| -> (usize, usize) {
        let mut starts = vec![0usize];
        match separator {
            Some(separator) => {
                for (index, byte) in line.iter().enumerate() {
                    if *byte == separator {
                        starts.push(index + 1);
                    }
                }
            }
            // A field starts where blanks follow a non-blank, and includes them.
            None => {
                for index in 1..line.len() {
                    let blank = |byte: u8| matches!(byte, b' ' | b'\t');
                    if blank(line[index]) && !blank(line[index - 1]) {
                        starts.push(index);
                    }
                }
            }
        }
        let begin = starts.get(first - 1).copied().unwrap_or(line.len());
        let end = match last {
            Some(last) => starts
                .get(last)
                .map_or(line.len(), |next| next - usize::from(separator.is_some())),
            None => line.len(),
        };
        (begin, end.max(begin))
    };
    let compare_text = |left: &[u8], right: &[u8], flags: KeyFlags| -> Ordering {
        let trim = |text: &'_ [u8]| -> Vec<u8> {
            let skipped = if flags.skip_blanks {
                text.iter()
                    .position(|byte| !matches!(byte, b' ' | b'\t'))
                    .unwrap_or(text.len())
            } else {
                0
            };
            if flags.fold_case {
                text[skipped..].to_ascii_uppercase()
            } else {
                text[skipped..].to_vec()
            }
        };
        let ordering = if flags.numeric {
            leading_number(left)
                .partial_cmp(&leading_number(right))
                .unwrap_or(Ordering::Equal)
        } else {
            trim(left).cmp(&trim(right))
        };
        if flags.reverse {
            ordering.reverse()
        } else {
            ordering
        }
    };
    let by_keys = |left: &Vec<u8>, right: &Vec<u8>| -> Ordering {
        if keys.is_empty() {
            return compare_text(left, right, global);
        }
        keys.iter()
            .map(|key| {
                let (left_begin, left_end) = span(left, key.first, key.last);
                let (right_begin, right_end) = span(right, key.first, key.last);
                compare_text(
                    &left[left_begin..left_end],
                    &right[right_begin..right_end],
                    key.flags.unwrap_or(global),
                )
            })
            .find(|ordering| *ordering != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    };
    shell.alive()?;
    // Lines that compare equal fall back to their bytes, unless told to keep
    // their order.
    let last_resort =
        !stable && !unique && (!keys.is_empty() || global.numeric || global.fold_case);
    lines.sort_by(|left, right| {
        by_keys(left, right).then_with(|| {
            if !last_resort {
                Ordering::Equal
            } else if global.reverse {
                right.cmp(left)
            } else {
                left.cmp(right)
            }
        })
    });
    if unique {
        lines.dedup_by(|later, earlier| by_keys(earlier, later) == Ordering::Equal);
    }
    let mut sink: Box<dyn Write + '_> = match output {
        Some(file) => Box::new(std::io::BufWriter::new(std::fs::File::create(
            shell.path(file),
        )?)),
        None => Box::new(stdout),
    };
    for line in lines {
        sink.write_all(&line)?;
        sink.write_all(b"\n")?;
    }
    sink.flush()?;
    Ok(0)
}

/// `uniq [-c] [-d] [-u] [-i] [INPUT]`
pub(super) fn uniq(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let (mut counted, mut repeated, mut singles, mut ignore_case) = (false, false, false, false);
    let mut input = "-";
    let mut operands = 0;
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('c') | Arg::Long("count") => counted = true,
            Arg::Short('d') | Arg::Long("repeated") => repeated = true,
            Arg::Short('u') | Arg::Long("unique") => singles = true,
            Arg::Short('i') | Arg::Long("ignore-case") => ignore_case = true,
            Arg::Operand(file) if operands == 0 => {
                input = file;
                operands += 1;
            }
            Arg::Operand(_) | Arg::Short(_) | Arg::Long(_) => {
                return Err(Options::unsupported(&arg))
            }
        }
    }
    let Invocation {
        stdin,
        stdout,
        shell,
        ..
    } = invocation;
    let shell = *shell;
    let mut reader = shell.open(&mut **stdin, input)?;
    let mut emit = |line: &[u8], times: u64| -> Result<(), Failure> {
        if (repeated && times < 2) || (singles && times > 1) {
            return Ok(());
        }
        if counted {
            write!(stdout, "{times:7} ")?;
        }
        stdout.write_all(line)?;
        stdout.write_all(b"\n")?;
        Ok(())
    };
    let mut current: Option<(Vec<u8>, u64)> = None;
    each_line(&mut reader, shell, |line, _| {
        let same = current.as_ref().is_some_and(|(held, _)| {
            if ignore_case {
                held.eq_ignore_ascii_case(line)
            } else {
                held == line
            }
        });
        if same {
            if let Some((_, times)) = current.as_mut() {
                *times += 1;
            }
        } else if let Some((held, times)) = current.replace((line.to_vec(), 1)) {
            emit(&held, times)?;
        }
        Ok(true)
    })?;
    if let Some((held, times)) = current {
        emit(&held, times)?;
    }
    Ok(0)
}

/// `cut (-f LIST [-d DELIMITER] [-s] | -c LIST | -b LIST) [--output-delimiter=TEXT] [FILE...]`
pub(super) fn cut(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut delimiter = b'\t';
    let mut output_delimiter: Option<&str> = None;
    let (mut fields, mut characters): (Option<&str>, Option<&str>) = (None, None);
    let mut only_delimited = false;
    let mut files = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('d') | Arg::Long("delimiter") => match options.value()?.as_bytes() {
                [byte] => delimiter = *byte,
                _ => return Err(Failure::Unsupported("a multi-byte delimiter".to_string())),
            },
            Arg::Short('f') | Arg::Long("fields") => fields = Some(options.value()?),
            Arg::Short('c' | 'b') | Arg::Long("characters" | "bytes") => {
                characters = Some(options.value()?);
            }
            Arg::Short('s') | Arg::Long("only-delimited") => only_delimited = true,
            Arg::Long("output-delimiter") => output_delimiter = Some(options.value()?),
            Arg::Operand(file) => files.push(file),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    let Some(list) = fields.or(characters) else {
        writeln!(
            invocation.stderr,
            "cut: you must specify a list of bytes, characters, or fields\nTry 'cut --help' for more information."
        )?;
        return Ok(1);
    };
    // The list as inclusive ranges, counted from 1.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    for part in list.split(',') {
        let bound = |text: &str, open: usize| {
            if text.is_empty() {
                Ok(open)
            } else {
                text.parse::<usize>()
                    .ok()
                    .filter(|bound| *bound > 0)
                    .ok_or_else(|| Failure::Unsupported(format!("the list {list}")))
            }
        };
        ranges.push(if let Some((low, high)) = part.split_once('-') {
            (bound(low, 1)?, bound(high, usize::MAX)?)
        } else {
            let only = bound(part, 0)?;
            (only, only)
        });
    }
    let selected = |position: usize| {
        ranges
            .iter()
            .any(|(low, high)| (*low..=*high).contains(&position))
    };
    if files.is_empty() {
        files.push("-");
    }
    let Invocation {
        stdin,
        stdout,
        stderr,
        shell,
        ..
    } = invocation;
    let shell = *shell;
    let joiner = output_delimiter.map_or_else(|| vec![delimiter], |text| text.as_bytes().to_vec());
    let mut status = 0;
    for file in files {
        let mut reader = match shell.open(&mut **stdin, file) {
            Ok(reader) => reader,
            Err(Failure::Io(error)) => {
                writeln!(stderr, "cut: {file}: {}", reason(&error))?;
                status = 1;
                continue;
            }
            Err(other) => return Err(other),
        };
        each_line(&mut reader, shell, |line, _| {
            if fields.is_some() {
                if !line.contains(&delimiter) {
                    if !only_delimited {
                        stdout.write_all(line)?;
                        stdout.write_all(b"\n")?;
                    }
                    return Ok(true);
                }
                let mut first = true;
                for (index, field) in line.split(|byte| *byte == delimiter).enumerate() {
                    if selected(index + 1) {
                        if !std::mem::replace(&mut first, false) {
                            stdout.write_all(&joiner)?;
                        }
                        stdout.write_all(field)?;
                    }
                }
            } else {
                // Selected runs are joined by the output delimiter, if one
                // was given.
                let mut previous: Option<usize> = None;
                for (index, byte) in line.iter().enumerate() {
                    if selected(index + 1) {
                        if output_delimiter.is_some()
                            && previous.is_some_and(|previous| previous + 1 != index)
                        {
                            stdout.write_all(&joiner)?;
                        }
                        stdout.write_all(&[*byte])?;
                        previous = Some(index);
                    }
                }
            }
            stdout.write_all(b"\n")?;
            Ok(true)
        })?;
    }
    Ok(status)
}

/// The bytes a `tr` set names: escapes, ranges and character classes.
fn tr_set(text: &str) -> Result<Vec<u8>, Failure> {
    let bytes = text.as_bytes();
    let mut set = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let mut byte = bytes[at];
        at += 1;
        if byte == b'[' && bytes.get(at) == Some(&b':') {
            let Some(end) = text[at..].find(":]") else {
                return Err(Failure::Unsupported(format!("the set {text}")));
            };
            let class: fn(u8) -> bool = match &text[at + 1..at + end] {
                "alpha" => |byte| byte.is_ascii_alphabetic(),
                "digit" => |byte| byte.is_ascii_digit(),
                "alnum" => |byte| byte.is_ascii_alphanumeric(),
                "upper" => |byte| byte.is_ascii_uppercase(),
                "lower" => |byte| byte.is_ascii_lowercase(),
                "space" => |byte| matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 0x0B | 0x0C),
                "blank" => |byte| matches!(byte, b' ' | b'\t'),
                "punct" => |byte| byte.is_ascii_punctuation(),
                "xdigit" => |byte| byte.is_ascii_hexdigit(),
                "cntrl" => |byte| byte.is_ascii_control(),
                "print" => |byte| byte == b' ' || byte.is_ascii_graphic(),
                "graph" => |byte| byte.is_ascii_graphic(),
                other => return Err(Failure::Unsupported(format!("the class [:{other}:]"))),
            };
            set.extend((0..=255u8).filter(|byte| class(*byte)));
            at += end + 2;
            continue;
        }
        if byte == b'[' && matches!(bytes.get(at), Some(b'=')) {
            return Err(Failure::Unsupported(format!("the set {text}")));
        }
        if byte == b'\\' {
            let Some(&escaped) = bytes.get(at) else {
                set.push(b'\\');
                break;
            };
            at += 1;
            byte = match escaped {
                b'n' => b'\n',
                b't' => b'\t',
                b'r' => b'\r',
                b'a' => 0x07,
                b'b' => 0x08,
                b'f' => 0x0C,
                b'v' => 0x0B,
                b'0'..=b'7' => {
                    let mut value = u32::from(escaped - b'0');
                    let mut digits = 1;
                    while digits < 3 {
                        match bytes.get(at) {
                            Some(digit @ b'0'..=b'7') => {
                                value = value * 8 + u32::from(digit - b'0');
                                at += 1;
                                digits += 1;
                            }
                            Some(_) | None => break,
                        }
                    }
                    u8::try_from(value)
                        .map_err(|_| Failure::Unsupported(format!("the set {text}")))?
                }
                other => other,
            };
        }
        // `a-z`: a range, unless the dash is the set's last byte.
        if bytes.get(at) == Some(&b'-') && at + 1 < bytes.len() && bytes[at + 1] != b'\\' {
            let high = bytes[at + 1];
            if high < byte {
                return Err(Failure::Unsupported(format!(
                    "the reversed range in {text}"
                )));
            }
            set.extend(byte..=high);
            at += 2;
        } else {
            set.push(byte);
        }
    }
    Ok(set)
}

/// `tr [-d] [-s] [-c] SET1 [SET2]`
pub(super) fn tr(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let (mut delete, mut squeeze, mut complement) = (false, false, false);
    let mut sets = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Short('d') | Arg::Long("delete") => delete = true,
            Arg::Short('s') | Arg::Long("squeeze-repeats") => squeeze = true,
            Arg::Short('c' | 'C') | Arg::Long("complement") => complement = true,
            Arg::Operand(set) => sets.push(set),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    let wanted = if delete == squeeze { 2 } else { 1 };
    let (first, second) = match sets.as_slice() {
        [first] if wanted == 1 || (squeeze && !delete) => (*first, None),
        [first, second] if wanted == 2 || (squeeze && !delete) => (*first, Some(*second)),
        [] => {
            writeln!(
                invocation.stderr,
                "tr: missing operand\nTry 'tr --help' for more information."
            )?;
            return Ok(1);
        }
        [only] => {
            writeln!(
                invocation.stderr,
                "tr: missing operand after {}\nTwo strings must be given when translating.\nTry 'tr --help' for more information.",
                super::quoted(only)
            )?;
            return Ok(1);
        }
        [_, extra, ..] => {
            writeln!(
                invocation.stderr,
                "tr: extra operand {}\nTry 'tr --help' for more information.",
                super::quoted(extra)
            )?;
            return Ok(1);
        }
    };
    let mut from = tr_set(first)?;
    if complement {
        from = (0..=255u8).filter(|byte| !from.contains(byte)).collect();
    }
    let to = second.map(tr_set).transpose()?;
    // What each byte becomes (`None`: deleted), and which bytes squeeze.
    let mut table: [Option<u8>; 256] = std::array::from_fn(|byte| u8::try_from(byte).ok());
    let mut squeezed = [false; 256];
    match (&to, delete) {
        (_, true) => {
            for byte in &from {
                table[usize::from(*byte)] = None;
            }
            for byte in to.iter().flatten() {
                squeezed[usize::from(*byte)] = squeeze;
            }
        }
        (Some(to), false) => {
            let Some(&last) = to.last() else {
                writeln!(
                    invocation.stderr,
                    "tr: when not truncating set1, string2 must be non-empty"
                )?;
                return Ok(1);
            };
            for (index, byte) in from.iter().enumerate() {
                // The second set is extended with its last byte.
                table[usize::from(*byte)] = Some(to.get(index).copied().unwrap_or(last));
            }
            for byte in to {
                squeezed[usize::from(*byte)] = squeeze;
            }
        }
        (None, false) => {
            for byte in &from {
                squeezed[usize::from(*byte)] = squeeze;
            }
        }
    }
    let Invocation {
        stdin,
        stdout,
        shell,
        ..
    } = invocation;
    let mut chunk = vec![0u8; 64 * 1024];
    let mut previous: Option<u8> = None;
    loop {
        let read = stdin.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        shell.alive()?;
        let mut out = Vec::with_capacity(read);
        for byte in &chunk[..read] {
            if let Some(mapped) = table[usize::from(*byte)] {
                if !(squeezed[usize::from(mapped)] && previous == Some(mapped)) {
                    out.push(mapped);
                }
                previous = Some(mapped);
            }
        }
        stdout.write_all(&out)?;
    }
    Ok(0)
}

/// `seq [-s SEPARATOR] [-w] [FIRST [INCREMENT]] LAST`, whole numbers only.
pub(super) fn seq(invocation: &mut Invocation<'_>) -> Outcome {
    let mut separator = "\n";
    let mut equal_width = false;
    let mut numbers: Vec<i64> = Vec::new();
    let mut args = invocation.args.iter().map(String::as_str);
    while let Some(arg) = args.next() {
        match arg {
            "-w" | "--equal-width" => equal_width = true,
            "-s" | "--separator" => {
                separator = args
                    .next()
                    .ok_or_else(|| Failure::Unsupported("-s without its value".to_string()))?;
            }
            attached if attached.starts_with("-s") && attached.len() > 2 => {
                separator = &attached[2..];
            }
            // A negative number is an operand, not an option.
            number => numbers.push(
                number
                    .parse()
                    .map_err(|_| Failure::Unsupported(format!("the operand {number}")))?,
            ),
        }
    }
    let (first, step, last) = match numbers.as_slice() {
        [last] => (1, 1, *last),
        [first, last] => (*first, 1, *last),
        [first, step, last] => (*first, *step, *last),
        [] | [_, _, _, _, ..] => {
            writeln!(
                invocation.stderr,
                "seq: missing or extra operand\nTry 'seq --help' for more information."
            )?;
            return Ok(1);
        }
    };
    if step == 0 {
        return Err(Failure::Unsupported("a zero increment".to_string()));
    }
    let width = if equal_width {
        first.to_string().len().max(last.to_string().len())
    } else {
        0
    };
    let (mut current, mut written) = (first, false);
    while (step > 0 && current <= last) || (step < 0 && current >= last) {
        if written {
            invocation.stdout.write_all(separator.as_bytes())?;
        }
        write!(invocation.stdout, "{current:0width$}")?;
        written = true;
        if current % 1024 == 0 {
            invocation.shell.alive()?;
        }
        let Some(next) = current.checked_add(step) else {
            break;
        };
        current = next;
    }
    if written {
        invocation.stdout.write_all(b"\n")?;
    }
    Ok(0)
}

/// `tac [FILE...]`
pub(super) fn tac(invocation: &mut Invocation<'_>) -> Outcome {
    let mut options = Options::new(invocation.args);
    let mut files = Vec::new();
    while let Some(arg) = options.next() {
        match arg {
            Arg::Operand(file) => files.push(file),
            Arg::Short(_) | Arg::Long(_) => return Err(Options::unsupported(&arg)),
        }
    }
    if files.is_empty() {
        files.push("-");
    }
    let Invocation {
        stdin,
        stdout,
        stderr,
        shell,
        ..
    } = invocation;
    let shell = *shell;
    let mut status = 0;
    for file in files {
        let mut reader = match shell.open(&mut **stdin, file) {
            Ok(reader) => reader,
            Err(Failure::Io(error)) => {
                writeln!(
                    stderr,
                    "tac: failed to open '{file}' for reading: {}",
                    reason(&error)
                )?;
                status = 1;
                continue;
            }
            Err(other) => return Err(other),
        };
        // Records keep their own newline: a last line without one is
        // written first, joined to what follows it.
        let mut records: Vec<Vec<u8>> = Vec::new();
        each_line(&mut reader, shell, |line, terminated| {
            let mut record = line.to_vec();
            if terminated {
                record.push(b'\n');
            }
            records.push(record);
            Ok(true)
        })?;
        for record in records.iter().rev() {
            stdout.write_all(record)?;
        }
    }
    Ok(status)
}

/// `yes [TEXT...]`
pub(super) fn yes(invocation: &mut Invocation<'_>) -> Outcome {
    let line = if invocation.args.is_empty() {
        "y\n".to_string()
    } else {
        format!("{}\n", invocation.args.join(" "))
    };
    let block = line.repeat((8192 / line.len()).max(1));
    loop {
        invocation.shell.alive()?;
        invocation.stdout.write_all(block.as_bytes())?;
        // Unbuffered here, so a reader that leaves ends this at once.
        invocation.stdout.flush()?;
    }
}
