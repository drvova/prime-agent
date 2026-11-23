//! Reading a Scheme cell's printed value. The kernel prints a cell's last
//! value as data, and a kernel function's reply is a hash, so a cell that
//! ends in `(bash …)`, `(fs-read …)` or `(edit …)` settles with
//! `#<hashmap {` and one `'key: value,` row per field: strings quoted and
//! escaped onto one line, nested values indented further. The card reads
//! the fields it shows better than the dump does; anything that is not such
//! a print is left as it came.

/// The top-level fields of one printed hash, values as printed.
pub(crate) struct PrintedHash<'a> {
    fields: Vec<(&'a str, &'a str)>,
}

/// What a printed `bash` value says of its command.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BashOutcome {
    /// The command's exit code once it has one.
    pub(crate) exit_code: Option<i64>,
    /// The command was still running when the cell ended.
    pub(crate) running: bool,
    pub(crate) timed_out: bool,
}

impl<'a> PrintedHash<'a> {
    /// The fields of `text` when all of it is one printed hash.
    pub(crate) fn parse(text: &'a str) -> Option<PrintedHash<'a>> {
        let body = text
            .trim()
            .strip_prefix("#<hashmap {")?
            .strip_suffix("}>")?;
        let mut fields: Vec<(&str, &str)> = Vec::new();
        let mut offset = 0usize;
        for line in body.split_inclusive('\n') {
            let start = offset;
            offset += line.len();
            // A field's row is indented once; a deeper row continues the
            // value above it.
            let field = line.strip_prefix("    '").and_then(|row| {
                let (key, _) = row.split_once(": ")?;
                (!key.is_empty() && !key.contains(char::is_whitespace)).then_some(key)
            });
            match field {
                Some(key) => {
                    let value = start + "    '".len() + key.len() + ": ".len();
                    fields.push((key, &body[value..offset]));
                }
                None => {
                    if let Some((_, value)) = fields.last_mut() {
                        *value = &body[start - value.len()..offset];
                    }
                }
            }
        }
        for (_, value) in &mut fields {
            let trimmed = value.trim_end();
            *value = trimmed.strip_suffix(',').unwrap_or(trimmed);
        }
        Some(PrintedHash { fields })
    }

    /// One field's value as printed.
    pub(crate) fn raw(&self, key: &str) -> Option<&'a str> {
        self.fields
            .iter()
            .find(|(name, _)| *name == key)
            .map(|(_, value)| *value)
    }

    /// One string field's text, its escapes read.
    pub(crate) fn string(&self, key: &str) -> Option<String> {
        let quoted = self.raw(key)?.strip_prefix('"')?.strip_suffix('"')?;
        let mut text = String::with_capacity(quoted.len());
        let mut chars = quoted.chars();
        while let Some(next) = chars.next() {
            if next != '\\' {
                text.push(next);
                continue;
            }
            match chars.next()? {
                'n' => text.push('\n'),
                'r' => text.push('\r'),
                't' => text.push('\t'),
                '0' => text.push('\0'),
                'u' => {
                    let digits: String = chars
                        .by_ref()
                        .skip(1)
                        .take_while(|digit| *digit != '}')
                        .collect();
                    text.push(char::from_u32(u32::from_str_radix(&digits, 16).ok()?)?);
                }
                other => text.push(other),
            }
        }
        Some(text)
    }

    /// The command outcome of a printed `bash` value: a job handle, a
    /// finished command's result, or a snapshot of one still running.
    pub(crate) fn bash_outcome(&self) -> Option<BashOutcome> {
        self.raw("job")?;
        self.raw("pid")?;
        Some(BashOutcome {
            running: self.raw("running")? == "#true",
            exit_code: self.raw("exit-code").and_then(|code| code.parse().ok()),
            timed_out: self.raw("timed-out") == Some("#true"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASH: &str = "#<hashmap {\n    'running: #false,\n    'exit-code: 101,\n    'output: \"a \\\"b\\\"\\n\\tc \\u{1b}[0m\",\n    'tests: #<hashmap {\n        'failed: 2,\n        'output: \"inner\",\n    }>,\n    'timed-out: #false,\n    'job: \"j3\",\n    'pid: 4242,\n}>";

    /// A printed hash reads back by field: a string's escapes are undone, a
    /// nested hash stays inside its own field, and a value that is not one
    /// printed hash is not read at all.
    #[test]
    fn a_printed_hash_reads_back_by_field() {
        let hash = PrintedHash::parse(BASH).expect("a printed hash");
        assert_eq!(
            (
                hash.string("output"),
                hash.raw("exit-code"),
                hash.raw("tests"),
                hash.bash_outcome(),
                PrintedHash::parse("(1 2 3)").is_some(),
                PrintedHash::parse("#<hashmap {}>").map(|empty| empty.fields.len()),
            ),
            (
                Some("a \"b\"\n\tc \u{1b}[0m".to_string()),
                Some("101"),
                Some("#<hashmap {\n        'failed: 2,\n        'output: \"inner\",\n    }>"),
                Some(BashOutcome {
                    exit_code: Some(101),
                    running: false,
                    timed_out: false,
                }),
                false,
                Some(0),
            )
        );
    }
}
