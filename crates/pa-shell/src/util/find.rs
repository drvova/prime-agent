//! `find [PATH...] [EXPRESSION]`.
//!
//! Tests: `-name`, `-iname`, `-path`, `-ipath`, `-type`, `-newer`, `-mtime`,
//! `-mmin`, `-size`, `-empty`, `-true`, `-false`. Actions: `-print`,
//! `-print0`, `-delete`, `-prune`. Operators: `!`, `-not`, `-a`, `-and`,
//! `-o`, `-or`, parentheses. Options: `-maxdepth`, `-mindepth`, `-depth`.
//! Links are not followed, and a directory's entries are visited in name
//! order (GNU find takes them in the order the directory lists them).
//! Anything that runs a command (`-exec`) is the program's.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use super::{quoted, reason, Failure, Invocation, Outcome, View};

#[derive(Clone, Copy)]
enum Compare {
    Less,
    Equal,
    More,
}

impl Compare {
    /// Split `+N`, `-N` or `N`.
    fn parse(text: &str) -> Result<(Self, u64), Failure> {
        let (compare, digits) = match text.as_bytes().first() {
            Some(b'+') => (Compare::More, &text[1..]),
            Some(b'-') => (Compare::Less, &text[1..]),
            Some(_) | None => (Compare::Equal, text),
        };
        digits
            .parse()
            .map(|number| (compare, number))
            .map_err(|_| Failure::Unsupported(format!("the number {text}")))
    }

    fn holds(self, actual: u64, wanted: u64) -> bool {
        match self {
            Compare::Less => actual < wanted,
            Compare::Equal => actual == wanted,
            Compare::More => actual > wanted,
        }
    }
}

enum Expression {
    And(Box<Expression>, Box<Expression>),
    Or(Box<Expression>, Box<Expression>),
    Not(Box<Expression>),
    Constant(bool),
    /// `-name` (on the last component) or `-path` (on the whole path).
    Glob {
        matcher: globset::GlobMatcher,
        whole_path: bool,
    },
    Type(char),
    Newer(SystemTime),
    /// Age in units of `seconds` (a day or a minute).
    Age {
        compare: Compare,
        wanted: u64,
        seconds: u64,
    },
    /// Size in units of `unit` bytes, rounded up.
    Size {
        compare: Compare,
        wanted: u64,
        unit: u64,
    },
    Empty,
    Print {
        terminator: u8,
    },
    Delete,
    Prune,
}

struct Search<'a> {
    shell: View<'a>,
    stdout: &'a mut dyn Write,
    stderr: &'a mut dyn Write,
    min_depth: usize,
    max_depth: usize,
    /// A directory's contents before the directory itself.
    depth_first: bool,
    now: SystemTime,
    status: i32,
}

/// The expression parser: `-o` binds loosest, then (implied) `-a`, then `!`.
struct Parser<'a> {
    words: &'a [String],
    at: usize,
    shell: View<'a>,
    has_action: bool,
    min_depth: usize,
    max_depth: usize,
    depth_first: bool,
}

impl<'a> Parser<'a> {
    fn value(&mut self, option: &str) -> Result<&'a str, Failure> {
        let value = self
            .words
            .get(self.at)
            .ok_or_else(|| Failure::Unsupported(format!("{option} without its value")))?;
        self.at += 1;
        Ok(value)
    }

    fn or(&mut self) -> Result<Expression, Failure> {
        let mut left = self.and()?;
        while matches!(
            self.words.get(self.at).map(String::as_str),
            Some("-o" | "-or")
        ) {
            self.at += 1;
            left = Expression::Or(Box::new(left), Box::new(self.and()?));
        }
        Ok(left)
    }

    fn and(&mut self) -> Result<Expression, Failure> {
        let mut left = self.primary()?;
        loop {
            match self.words.get(self.at).map(String::as_str) {
                None | Some("-o" | "-or" | ")") => return Ok(left),
                Some("-a" | "-and") => self.at += 1,
                Some(_) => {}
            }
            left = Expression::And(Box::new(left), Box::new(self.primary()?));
        }
    }

    fn primary(&mut self) -> Result<Expression, Failure> {
        let Some(word) = self.words.get(self.at).map(String::as_str) else {
            return Err(Failure::Unsupported("an incomplete expression".to_string()));
        };
        self.at += 1;
        let glob = |pattern: &str, ignore_case: bool| {
            globset::GlobBuilder::new(pattern)
                .case_insensitive(ignore_case)
                .backslash_escape(true)
                .build()
                .map(|glob| glob.compile_matcher())
                .map_err(|_| Failure::Unsupported(format!("the pattern {pattern}")))
        };
        Ok(match word {
            "!" | "-not" => Expression::Not(Box::new(self.primary()?)),
            "(" => {
                let inner = self.or()?;
                if self.words.get(self.at).map(String::as_str) != Some(")") {
                    return Err(Failure::Unsupported("an unclosed parenthesis".to_string()));
                }
                self.at += 1;
                inner
            }
            "-true" => Expression::Constant(true),
            "-false" => Expression::Constant(false),
            "-name" | "-iname" => Expression::Glob {
                matcher: glob(self.value(word)?, word == "-iname")?,
                whole_path: false,
            },
            "-path" | "-ipath" | "-wholename" => Expression::Glob {
                matcher: glob(self.value(word)?, word == "-ipath")?,
                whole_path: true,
            },
            "-type" => match self.value(word)? {
                "f" => Expression::Type('f'),
                "d" => Expression::Type('d'),
                "l" => Expression::Type('l'),
                other => return Err(Failure::Unsupported(format!("-type {other}"))),
            },
            "-newer" => {
                let reference = self.value(word)?;
                let modified = std::fs::metadata(self.shell.path(reference))
                    .and_then(|metadata| metadata.modified())
                    .map_err(|_| Failure::Unsupported(format!("-newer {reference}")))?;
                Expression::Newer(modified)
            }
            "-mtime" | "-mmin" => {
                let (compare, wanted) = Compare::parse(self.value(word)?)?;
                Expression::Age {
                    compare,
                    wanted,
                    seconds: if word == "-mtime" { 86_400 } else { 60 },
                }
            }
            "-size" => {
                let text = self.value(word)?;
                let (number, unit) = match text.as_bytes().last() {
                    Some(b'c') => (&text[..text.len() - 1], 1),
                    Some(b'k') => (&text[..text.len() - 1], 1024),
                    Some(b'M') => (&text[..text.len() - 1], 1024 * 1024),
                    Some(b'G') => (&text[..text.len() - 1], 1024 * 1024 * 1024),
                    Some(b'b') => (&text[..text.len() - 1], 512),
                    Some(_) | None => (text, 512),
                };
                let (compare, wanted) = Compare::parse(number)?;
                Expression::Size {
                    compare,
                    wanted,
                    unit,
                }
            }
            "-empty" => Expression::Empty,
            "-print" => {
                self.has_action = true;
                Expression::Print { terminator: b'\n' }
            }
            "-print0" => {
                self.has_action = true;
                Expression::Print { terminator: 0 }
            }
            "-delete" => {
                self.has_action = true;
                self.depth_first = true;
                Expression::Delete
            }
            "-prune" => Expression::Prune,
            "-maxdepth" | "-mindepth" => {
                let depth = self
                    .value(word)?
                    .parse()
                    .map_err(|_| Failure::Unsupported(format!("the depth after {word}")))?;
                if word == "-maxdepth" {
                    self.max_depth = depth;
                } else {
                    self.min_depth = depth;
                }
                Expression::Constant(true)
            }
            "-depth" => {
                self.depth_first = true;
                Expression::Constant(true)
            }
            other => return Err(Failure::Unsupported(format!("the expression {other}"))),
        })
    }
}

impl Search<'_> {
    /// Whether `expression` holds for the file at `path`, shown as `shown`.
    /// `prune` is set when the file's directory must not be entered.
    fn holds(
        &mut self,
        expression: &Expression,
        path: &Path,
        shown: &str,
        metadata: &std::fs::Metadata,
        prune: &mut bool,
    ) -> Result<bool, Failure> {
        Ok(match expression {
            Expression::And(left, right) => {
                self.holds(left, path, shown, metadata, prune)?
                    && self.holds(right, path, shown, metadata, prune)?
            }
            Expression::Or(left, right) => {
                self.holds(left, path, shown, metadata, prune)?
                    || self.holds(right, path, shown, metadata, prune)?
            }
            Expression::Not(inner) => !self.holds(inner, path, shown, metadata, prune)?,
            Expression::Constant(value) => *value,
            Expression::Glob {
                matcher,
                whole_path,
            } => {
                if *whole_path {
                    matcher.is_match(shown)
                } else {
                    // A starting point such as `dir/` is matched by its own name.
                    let trimmed = shown.trim_end_matches('/');
                    matcher.is_match(if trimmed.is_empty() {
                        "/"
                    } else {
                        trimmed.rsplit('/').next().unwrap_or(trimmed)
                    })
                }
            }
            Expression::Type('d') => metadata.is_dir(),
            Expression::Type('l') => metadata.file_type().is_symlink(),
            Expression::Type(_) => metadata.is_file(),
            Expression::Newer(reference) => metadata
                .modified()
                .is_ok_and(|modified| modified > *reference),
            Expression::Age {
                compare,
                wanted,
                seconds,
            } => {
                let age = metadata
                    .modified()
                    .ok()
                    .and_then(|modified| self.now.duration_since(modified).ok())
                    .unwrap_or(Duration::ZERO);
                compare.holds(age.as_secs() / seconds, *wanted)
            }
            Expression::Size {
                compare,
                wanted,
                unit,
            } => compare.holds(metadata.len().div_ceil(*unit), *wanted),
            Expression::Empty => {
                if metadata.is_dir() {
                    std::fs::read_dir(path).is_ok_and(|mut entries| entries.next().is_none())
                } else {
                    metadata.is_file() && metadata.len() == 0
                }
            }
            Expression::Print { terminator } => {
                self.stdout.write_all(shown.as_bytes())?;
                self.stdout.write_all(&[*terminator])?;
                true
            }
            Expression::Delete => {
                let removed = if metadata.is_dir() {
                    std::fs::remove_dir(path)
                } else {
                    std::fs::remove_file(path)
                };
                match removed {
                    Ok(()) => true,
                    Err(error) => {
                        writeln!(
                            self.stderr,
                            "find: cannot delete {}: {}",
                            quoted(shown),
                            reason(&error)
                        )?;
                        self.status = 1;
                        false
                    }
                }
            }
            Expression::Prune => {
                *prune = true;
                true
            }
        })
    }

    /// Evaluate `expression` for `path` and everything under it.
    fn visit(
        &mut self,
        expression: &Expression,
        path: &Path,
        shown: &str,
        depth: usize,
    ) -> Result<(), Failure> {
        self.shell.alive()?;
        let listed = std::fs::symlink_metadata(path).and_then(|metadata| {
            let entries = if metadata.is_dir() && depth < self.max_depth {
                let mut entries = std::fs::read_dir(path)?.collect::<std::io::Result<Vec<_>>>()?;
                entries.sort_by_key(std::fs::DirEntry::file_name);
                entries
            } else {
                Vec::new()
            };
            Ok((metadata, entries))
        });
        let (metadata, entries) = match listed {
            Ok(listed) => listed,
            Err(error) => {
                self.stdout.flush()?;
                writeln!(self.stderr, "find: {}: {}", quoted(shown), reason(&error))?;
                self.status = 1;
                return Ok(());
            }
        };
        let mut prune = false;
        if !self.depth_first && depth >= self.min_depth {
            self.holds(expression, path, shown, &metadata, &mut prune)?;
        }
        if !prune {
            for entry in entries {
                let name = entry.file_name().to_string_lossy().into_owned();
                let below = format!("{}/{name}", shown.strip_suffix('/').unwrap_or(shown));
                self.visit(expression, &entry.path(), &below, depth + 1)?;
            }
        }
        if self.depth_first && depth >= self.min_depth {
            self.holds(expression, path, shown, &metadata, &mut prune)?;
        }
        Ok(())
    }
}

pub(super) fn find(invocation: &mut Invocation<'_>) -> Outcome {
    let words = invocation.args;
    let paths_end = words
        .iter()
        .position(|word| word.starts_with('-') || matches!(word.as_str(), "(" | "!"))
        .unwrap_or(words.len());
    let shell = invocation.shell;
    let mut parser = Parser {
        words: &words[paths_end..],
        at: 0,
        shell,
        has_action: false,
        min_depth: 0,
        max_depth: usize::MAX,
        depth_first: false,
    };
    let mut expression = if parser.words.is_empty() {
        Expression::Constant(true)
    } else {
        parser.or()?
    };
    if parser.at < parser.words.len() {
        return Err(Failure::Unsupported(format!(
            "the expression {}",
            parser.words[parser.at]
        )));
    }
    if !parser.has_action {
        expression = Expression::And(
            Box::new(expression),
            Box::new(Expression::Print { terminator: b'\n' }),
        );
    }
    let mut search = Search {
        shell,
        stdout: &mut invocation.stdout,
        stderr: &mut invocation.stderr,
        min_depth: parser.min_depth,
        max_depth: parser.max_depth,
        depth_first: parser.depth_first,
        now: SystemTime::now(),
        status: 0,
    };
    let default_path = [".".to_string()];
    let paths = if paths_end == 0 {
        &default_path[..]
    } else {
        &words[..paths_end]
    };
    for start in paths {
        search.visit(&expression, &shell.path(start), start, 0)?;
    }
    Ok(search.status)
}
