//! POSIX regular expressions, as `grep` and `sed` take them, rewritten for
//! the `regex` crate.
//!
//! Two things POSIX has and the crate does not: back-references, which are
//! answered as unsupported so the program takes over, and leftmost-longest
//! alternation (`a|ab` on `ab` matches `a` here), which only shows where a
//! match's extent is printed or replaced.

use super::Failure;

/// Which syntax a pattern is written in.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Syntax {
    /// Basic: `\(`, `\{`, `\|`, `\+` and `\?` are the operators.
    Basic,
    /// Extended: the bare characters are.
    Extended,
    /// No operators at all.
    Fixed,
}

/// `pattern` in the `regex` crate's syntax.
pub(crate) fn translate(pattern: &str, syntax: Syntax) -> Result<String, Failure> {
    if syntax == Syntax::Fixed {
        return Ok(regex::escape(pattern));
    }
    let basic = syntax == Syntax::Basic;
    let chars: Vec<char> = pattern.chars().collect();
    let mut out = String::with_capacity(pattern.len() + 8);
    // Whether the next character starts an expression: there a basic `*` and
    // a basic `^` mean themselves.
    let mut at_start = true;
    let mut at = 0;
    while at < chars.len() {
        let current = chars[at];
        at += 1;
        let starts = std::mem::replace(&mut at_start, false);
        match current {
            '\\' => {
                let Some(&escaped) = chars.get(at) else {
                    return Err(Failure::Unsupported("a trailing backslash".to_string()));
                };
                at += 1;
                match escaped {
                    '1'..='9' => return Err(Failure::Unsupported("a back-reference".to_string())),
                    '(' | '|' if basic => {
                        out.push(escaped);
                        at_start = true;
                    }
                    ')' | '{' | '}' | '+' | '?' if basic => out.push(escaped),
                    '<' => out.push_str(r"\b{start}"),
                    '>' => out.push_str(r"\b{end}"),
                    'w' | 'W' | 's' | 'S' | 'b' | 'B' => {
                        out.push('\\');
                        out.push(escaped);
                    }
                    'n' => out.push('\n'),
                    other => out.push_str(&regex::escape(&other.to_string())),
                }
            }
            '[' => {
                out.push('[');
                if chars.get(at) == Some(&'^') {
                    out.push('^');
                    at += 1;
                }
                // A bracket first in the list is a member, not the end.
                if chars.get(at) == Some(&']') {
                    out.push_str(r"\]");
                    at += 1;
                }
                loop {
                    let Some(&member) = chars.get(at) else {
                        return Err(Failure::Unsupported(
                            "an unterminated bracket expression".to_string(),
                        ));
                    };
                    at += 1;
                    match member {
                        ']' => break,
                        '[' if chars.get(at) == Some(&':') => {
                            let Some(end) =
                                chars[at..].windows(2).position(|pair| pair == [':', ']'])
                            else {
                                return Err(Failure::Unsupported(
                                    "an unterminated character class".to_string(),
                                ));
                            };
                            out.push('[');
                            out.extend(&chars[at..at + end + 2]);
                            at += end + 2;
                        }
                        '[' if matches!(chars.get(at), Some('.' | '=')) => {
                            return Err(Failure::Unsupported(
                                "a collating symbol or equivalence class".to_string(),
                            ))
                        }
                        '\\' | '[' | '&' | '~' => {
                            out.push('\\');
                            out.push(member);
                        }
                        other => out.push(other),
                    }
                }
                out.push(']');
            }
            '*' if basic && starts => out.push_str(r"\*"),
            '^' if basic && !starts => out.push_str(r"\^"),
            '^' => {
                out.push('^');
                at_start = true;
            }
            '$' if basic => {
                // An anchor only where the expression ends.
                let ends = matches!(
                    (chars.get(at), chars.get(at + 1)),
                    (None, _) | (Some('\\'), Some(')' | '|'))
                );
                out.push_str(if ends { "$" } else { r"\$" });
            }
            '(' | ')' | '{' | '}' | '|' | '+' | '?' if basic => {
                out.push('\\');
                out.push(current);
            }
            '(' | '|' => {
                out.push(current);
                at_start = true;
            }
            '{' => {
                // An interval only when it reads as one; a brace otherwise.
                let close = chars[at..].iter().position(|member| *member == '}');
                let body: String =
                    close.map_or_else(String::new, |close| chars[at..at + close].iter().collect());
                let (low, high) = body.split_once(',').unwrap_or((&body, &body));
                let interval = close.is_some()
                    && !body.is_empty()
                    && low.chars().all(|digit| digit.is_ascii_digit())
                    && high.chars().all(|digit| digit.is_ascii_digit())
                    && (!low.is_empty() || !high.is_empty());
                if interval {
                    out.push('{');
                    if low.is_empty() {
                        out.push('0');
                    }
                    out.push_str(&body);
                    out.push('}');
                    at += body.chars().count() + 1;
                } else {
                    out.push_str(r"\{");
                }
            }
            '}' => out.push_str(r"\}"),
            other => out.push(other),
        }
    }
    Ok(out)
}

/// Compile `patterns` (any of them may match) for matching lines of bytes.
pub(crate) fn compile(
    patterns: &[String],
    syntax: Syntax,
    ignore_case: bool,
    wrap: impl Fn(&str) -> String,
) -> Result<regex::bytes::Regex, Failure> {
    let alternatives = patterns
        .iter()
        .map(|pattern| Ok(format!("(?:{})", translate(pattern, syntax)?)))
        .collect::<Result<Vec<String>, Failure>>()?;
    regex::bytes::RegexBuilder::new(&wrap(&alternatives.join("|")))
        .case_insensitive(ignore_case)
        .build()
        .map_err(|error| {
            // What the crate rejects the program may still take.
            Failure::Unsupported(format!("the pattern ({error})").replace('\n', " "))
        })
}
