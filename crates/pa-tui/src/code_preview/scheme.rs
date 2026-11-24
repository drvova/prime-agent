//! Reading a Scheme kernel cell's source for display: a lexer that is total
//! over arbitrary text (an unbalanced paren or an unterminated string is
//! ordinary input while a cell's code streams in), and the cell's outline,
//! the kernel call its title names.

/// One lexical class of Scheme source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SchemeToken {
    /// `(`, `[` or `{`.
    Open,
    /// `)`, `]` or `}`.
    Close,
    /// A string literal, closed or running to the end of the text.
    String,
    /// A `;` line comment or a `#| ... |#` block comment.
    Comment,
    Number,
    /// `#t`, `#f`, `#true`, `#false` and `#\x` characters.
    Literal,
    /// A `#:name` keyword argument.
    Keyword,
    /// `'`, `` ` ``, `,` and `,@`.
    Quote,
    Symbol,
    Space,
}

/// Split `code` into tokens: every byte belongs to exactly one, in order.
pub(crate) fn scheme_tokens(code: &str) -> Vec<(SchemeToken, &str)> {
    let delimiter =
        |c: char| c.is_whitespace() || matches!(c, '(' | ')' | '[' | ']' | '{' | '}' | '"' | ';');
    let mut tokens = Vec::new();
    let mut rest = code;
    while let Some(first) = rest.chars().next() {
        let mut chars = rest.char_indices().skip(1);
        let (token, end) = match first {
            c if c.is_whitespace() => (
                SchemeToken::Space,
                chars.find(|(_, c)| !c.is_whitespace()).map(|(at, _)| at),
            ),
            '(' | '[' | '{' => (SchemeToken::Open, Some(1)),
            ')' | ']' | '}' => (SchemeToken::Close, Some(1)),
            '\'' | '`' => (SchemeToken::Quote, Some(1)),
            ',' => (
                SchemeToken::Quote,
                Some(if rest.starts_with(",@") { 2 } else { 1 }),
            ),
            ';' => (
                SchemeToken::Comment,
                chars.find(|(_, c)| *c == '\n').map(|(at, _)| at),
            ),
            '"' => {
                let mut escaped = false;
                let close = chars.find(|(_, c)| {
                    let closes = *c == '"' && !escaped;
                    escaped = *c == '\\' && !escaped;
                    closes
                });
                (SchemeToken::String, close.map(|(at, _)| at + 1))
            }
            '#' if rest.starts_with("#|") => {
                // Block comments nest.
                let bytes = rest.as_bytes();
                let (mut depth, mut at) = (1usize, 2usize);
                while depth > 0 && at < bytes.len() {
                    match (bytes[at], bytes.get(at + 1)) {
                        (b'#', Some(b'|')) => (depth, at) = (depth + 1, at + 2),
                        (b'|', Some(b'#')) => (depth, at) = (depth - 1, at + 2),
                        _ => at += 1,
                    }
                }
                (SchemeToken::Comment, (depth == 0).then_some(at))
            }
            '#' if rest.starts_with("#\\") => {
                // The character itself, then a name's remaining letters (`#\space`).
                let end = chars
                    .skip(2)
                    .find(|(_, c)| !c.is_alphanumeric())
                    .map(|(at, _)| at);
                (SchemeToken::Literal, end)
            }
            _ => {
                let end = chars.find(|(_, c)| delimiter(*c)).map(|(at, _)| at);
                let atom = &rest[..end.unwrap_or(rest.len())];
                let numeric = atom.bytes().any(|b| b.is_ascii_digit())
                    && atom.bytes().all(|b| {
                        b.is_ascii_digit() || matches!(b, b'+' | b'-' | b'.' | b'e' | b'E' | b'/')
                    });
                let token = if matches!(atom, "#t" | "#f" | "#true" | "#false") {
                    SchemeToken::Literal
                } else if atom.starts_with("#:") {
                    SchemeToken::Keyword
                } else if numeric {
                    SchemeToken::Number
                } else {
                    SchemeToken::Symbol
                };
                (token, end)
            }
        };
        let (text, tail) = rest.split_at(end.unwrap_or(rest.len()));
        tokens.push((token, text));
        rest = tail;
    }
    tokens
}

/// A string token's text with Scheme's escapes read; an unterminated literal
/// gives what it has so far.
fn string_literal(token: &str) -> String {
    let mut text = String::with_capacity(token.len());
    let mut chars = token.chars().skip(1);
    while let Some(next) = chars.next() {
        match next {
            '"' => break,
            '\\' => text.push(match chars.next() {
                Some('n') => '\n',
                Some('t') => '\t',
                Some(other) => other,
                None => break,
            }),
            other => text.push(other),
        }
    }
    text
}

/// Whether `head` names one of the kernel's own functions: the calls a cell
/// reaches the system through. The families are the kernel API's
/// (`crates/pa-core/src/prompts/capabilities/`); a name this list misses
/// only costs its cell the named title.
pub(crate) fn is_kernel_function(head: &str) -> bool {
    const FAMILIES: [&str; 20] = [
        "bash-",
        "fs-",
        "rlm-",
        "agent-message-",
        "agent-observe-",
        "web-",
        "mcp-",
        "worktree-",
        "factory-",
        "rz-",
        "harness-",
        "goal-",
        "refine-",
        "compact-",
        "ctx-",
        "cell-",
        "checkpoint-",
        "schedule-",
        "todo-",
        "better-",
    ];
    matches!(
        head,
        "bash" | "edit" | "ask" | "better" | "notify" | "api-help" | "attach-image"
    ) || FAMILIES.iter().any(|family| head.starts_with(family))
}

/// The `(bash …)` calls of a Scheme cell, read from its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemeBashCalls {
    /// The first call's command when it is a string literal, else empty.
    pub first: String,
    /// How many forms call `bash`.
    pub count: usize,
    /// How many of the cell's lines the top-level forms holding them span.
    pub lines: usize,
    /// Whether any of them asks to run elevated (`'elevated #t`).
    pub elevated: bool,
}

/// The title of a Scheme cell: the kernel call it names, or the cell's
/// first form when it calls none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemePreview {
    /// The cell is mostly `bash` calls and reads as a shell command.
    pub bash: bool,
    /// The named kernel function; empty for a `bash` title and for a cell
    /// that calls none.
    pub head: String,
    /// The call's main argument, the command of a `bash` title, or the
    /// cell's first form.
    pub text: String,
    /// How many further calls of the same kind the cell makes.
    pub more: usize,
}

/// What a Scheme cell does, read from its source alone: never from what the
/// cell went on to run, so a title made from it reads the same before,
/// during and after the cell. A title that waited for the result changed
/// under the reader.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemeOutline {
    /// The first kernel call: its function and its main argument.
    pub call: Option<(String, String)>,
    /// How many kernel calls the cell makes.
    pub calls: usize,
    /// The cell's `bash` calls, when it makes any.
    pub bash: Option<SchemeBashCalls>,
    /// The first form on one line (the first line of a cell without forms).
    pub first_form: String,
    /// The cell's lines that are not blank.
    pub lines: usize,
}

/// A cell renders as bash when the forms holding its `bash` calls span at
/// least `1/BASH_DOMINANCE_DIVISOR` of its non-blank lines.
const BASH_DOMINANCE_DIVISOR: usize = 2;

impl SchemeOutline {
    /// The cell's `bash` calls when they make up most of it.
    #[must_use]
    pub fn dominant_bash(&self) -> Option<&SchemeBashCalls> {
        self.bash
            .as_ref()
            .filter(|bash| bash.lines * BASH_DOMINANCE_DIVISOR >= self.lines)
    }

    /// The cell's title.
    #[must_use]
    pub fn preview(&self) -> SchemePreview {
        if let Some(bash) = self.dominant_bash() {
            // A command that is not a literal previews as the cell's first form.
            let command = super::preview_bash_command(&bash.first).text;
            return SchemePreview {
                bash: true,
                head: String::new(),
                text: if command.is_empty() {
                    self.first_form.clone()
                } else {
                    command
                },
                more: bash.count - 1,
            };
        }
        match &self.call {
            Some((head, argument)) => SchemePreview {
                bash: false,
                head: head.clone(),
                text: argument.clone(),
                more: self.calls - 1,
            },
            None => SchemePreview {
                bash: false,
                head: String::new(),
                text: self.first_form.clone(),
                more: 0,
            },
        }
    }
}

/// Read a Scheme cell's outline from its source.
#[must_use]
pub fn scheme_outline(code: &str) -> SchemeOutline {
    let one_line = |text: &str| text.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut outline = SchemeOutline {
        call: None,
        calls: 0,
        bash: None,
        first_form: String::new(),
        lines: code.lines().filter(|line| !line.trim().is_empty()).count(),
    };
    let tokens = scheme_tokens(code);
    let significant =
        |token: SchemeToken| !matches!(token, SchemeToken::Space | SchemeToken::Comment);
    let mut depth = 0usize;
    // The token after an opening paren is the form's head.
    let mut head_next = false;
    // The open top-level form: its first byte and line, and whether it calls bash.
    let mut form: Option<(usize, usize, bool)> = None;
    let (mut offset, mut line) = (0usize, 0usize);
    for (index, (token, text)) in tokens.iter().enumerate() {
        let at = offset;
        offset += text.len();
        let is_head = significant(*token) && std::mem::take(&mut head_next);
        match token {
            SchemeToken::Open => {
                if depth == 0 {
                    form = Some((at, line, false));
                }
                depth += 1;
                head_next = true;
            }
            SchemeToken::Close => depth = depth.saturating_sub(1),
            SchemeToken::Symbol if is_head && is_kernel_function(text) => {
                // The call's main argument is the datum after its head: a
                // string's text, an atom, or a whole nested form. A keyword
                // starts the options, so a call that opens with one has none.
                let mut rest = tokens[index + 1..]
                    .iter()
                    .scan(offset, |end, (kind, text)| {
                        let start = *end;
                        *end += text.len();
                        Some((*kind, *text, start))
                    })
                    .filter(|(kind, _, _)| significant(*kind))
                    .peekable();
                let first = rest.peek().copied();
                let (mut end, mut nesting) = (offset, 0usize);
                for (kind, text, begins) in rest {
                    end = begins + text.len();
                    match kind {
                        SchemeToken::Open => nesting += 1,
                        SchemeToken::Close => nesting = nesting.saturating_sub(1),
                        // The quoted datum follows.
                        SchemeToken::Quote => continue,
                        SchemeToken::String
                        | SchemeToken::Number
                        | SchemeToken::Literal
                        | SchemeToken::Keyword
                        | SchemeToken::Symbol
                        | SchemeToken::Space
                        | SchemeToken::Comment => {}
                    }
                    if nesting == 0 {
                        break;
                    }
                }
                let command = match first {
                    Some((SchemeToken::String, literal, _)) => Some(string_literal(literal)),
                    Some(_) | None => None,
                };
                outline.calls += 1;
                if *text == "bash" {
                    if let Some(open) = form.as_mut() {
                        open.2 = true;
                    }
                    // `'elevated #t` anywhere in the call's own parentheses.
                    let (mut nesting, mut last) = (1usize, ["", ""]);
                    let mut elevated = false;
                    for (kind, word) in tokens[index + 1..]
                        .iter()
                        .filter(|(kind, _)| significant(*kind))
                    {
                        match kind {
                            SchemeToken::Open => nesting += 1,
                            SchemeToken::Close => nesting -= 1,
                            SchemeToken::String
                            | SchemeToken::Number
                            | SchemeToken::Literal
                            | SchemeToken::Keyword
                            | SchemeToken::Symbol
                            | SchemeToken::Quote
                            | SchemeToken::Space
                            | SchemeToken::Comment => {}
                        }
                        if nesting == 0 {
                            break;
                        }
                        elevated |= last == ["'", "elevated"] && matches!(*word, "#t" | "#true");
                        last = [last[1], word];
                    }
                    match outline.bash.as_mut() {
                        Some(bash) => {
                            bash.count += 1;
                            bash.elevated |= elevated;
                        }
                        None => {
                            outline.bash = Some(SchemeBashCalls {
                                first: command.clone().unwrap_or_default(),
                                count: 1,
                                lines: 0,
                                elevated,
                            });
                        }
                    }
                }
                if outline.call.is_none() {
                    let argument = match (command, first) {
                        (Some(literal), _) => one_line(&literal),
                        (None, Some((SchemeToken::Keyword | SchemeToken::Close, _, _)) | None) => {
                            String::new()
                        }
                        (None, Some((_, _, start))) => one_line(&code[start..end]),
                    };
                    outline.call = Some(((*text).to_string(), argument));
                }
            }
            SchemeToken::String
            | SchemeToken::Symbol
            | SchemeToken::Number
            | SchemeToken::Literal
            | SchemeToken::Keyword
            | SchemeToken::Quote
            | SchemeToken::Space
            | SchemeToken::Comment => {}
        }
        line += text.matches('\n').count();
        // A top-level form ends at its closing paren, or with the text while
        // the cell's code is still arriving.
        if depth == 0 || index + 1 == tokens.len() {
            if let Some((start, first_line, has_bash)) = form.take() {
                if outline.first_form.is_empty() {
                    outline.first_form = one_line(&code[start..offset]);
                }
                if let (true, Some(bash)) = (has_bash, outline.bash.as_mut()) {
                    bash.lines += line - first_line + 1;
                }
            }
        }
    }
    if outline.first_form.is_empty() {
        // No form at all: the first line with code on it, else the first line.
        let mut lines = code.lines().map(str::trim).filter(|line| !line.is_empty());
        let coded = lines.clone().find(|line| !line.starts_with(';'));
        outline.first_form = coded
            .or_else(|| lines.next())
            .unwrap_or_default()
            .to_string();
    }
    outline
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn preview(code: &str) -> (bool, String, String, usize) {
        let SchemePreview {
            bash,
            head,
            text,
            more,
        } = scheme_outline(code).preview();
        (bash, head, text, more)
    }

    /// A cell is titled by the kernel call it makes and that call's main
    /// argument, wherever the call sits: behind a comment, inside a
    /// `define` or a `displayln`. Its first line said `;; step 1` or
    /// `(define hits` instead.
    #[test]
    fn a_cell_is_titled_by_its_kernel_call() {
        let titled =
            |head: &str, text: &str, more| (false, head.to_string(), text.to_string(), more);
        let shell = |text: &str, more| (true, String::new(), text.to_string(), more);
        assert_eq!(
            [
                preview(";; step 1\n(fs-read \"src/a.rs\" (hash 'start 1))"),
                preview("(define hits (fs-grep \"TODO\" opts))\n(length hits)\n(fs-glob \"*.rs\")\n(car hits)"),
                preview("(rlm-collect (list a\n  b) #:timeout-ms 5)"),
                preview("(todo-get #:all #t)"),
                preview("(displayln (hash-ref (bash \"git status\") 'output))"),
                preview("(bash \"make\")\n(bash \"make test\")\n(+ 1 2)"),
                preview("(define r (bash cmd))\n(define n 1)\n(define m 2)"),
                preview("(define (square x)\n  (* x x))\n(square 4)"),
                preview("; nothing yet"),
                preview(""),
            ],
            [
                titled("fs-read", "src/a.rs", 0),
                titled("fs-grep", "TODO", 1),
                titled("rlm-collect", "(list a b)", 0),
                titled("todo-get", "", 0),
                shell("git status", 0),
                shell("make", 1),
                titled("bash", "cmd", 0),
                titled("", "(define (square x) (* x x))", 0),
                titled("", "; nothing yet", 0),
                titled("", "", 0),
            ]
        );
    }

    proptest! {
        /// The lexer is total: any text splits into tokens that are exactly
        /// that text, and the outline reads it without a panic, while the
        /// cell's code is still arriving as much as when it is whole.
        #[test]
        fn any_text_lexes_losslessly(code in "[()\\[\\]\"';#|\\\\ \n\ta-z0-9:.,`+\\-数é\u{301}🙂]{0,80}") {
            let tokens = scheme_tokens(&code);
            prop_assert!(tokens.iter().all(|(_, text)| !text.is_empty()));
            prop_assert_eq!(tokens.iter().map(|(_, text)| *text).collect::<String>(), code.clone());
            for (end, _) in code.char_indices() {
                prop_assert!(scheme_outline(&code[..end]).lines <= code.len());
            }
        }
    }
}
