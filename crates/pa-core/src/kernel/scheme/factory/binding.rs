//! How one state's settle reaches the next state: reading a named JSON
//! output out of a child's final answer, rendering bound inputs into a
//! prompt template, and evaluating a transition guard over the captured
//! outputs.

use std::fmt::Write as _;

use serde_json::{Map, Value};

use super::python_text::repr_str;
use super::spec::field;

/// The contents of every fenced ```` ```json ```` block of an answer, in
/// order, surrounding whitespace removed.
fn fenced_json_blocks(answer: &str) -> Vec<&str> {
    let mut blocks = Vec::new();
    let mut rest = answer;
    while let Some(open) = rest.find("```json") {
        let body = rest[open + "```json".len()..].trim_start();
        let Some(close) = body.find("```") else {
            break;
        };
        blocks.push(body[..close].trim_end());
        rest = &body[close + 3..];
    }
    blocks
}

/// The bare JSON object that terminates the answer, if one does: the last
/// balanced top-level object span that only whitespace or punctuation
/// follows.
///
/// A brace opens a candidate only where it starts a string key, so prose
/// that mentions a block hides nothing; braces inside string literals never
/// count; an unterminated candidate owns the tail, so a truncated emit
/// never binds.
pub(super) fn terminal_json_object(answer: &str) -> Option<String> {
    let letters: Vec<char> = answer.chars().collect();
    let length = letters.len();
    let mut last: Option<(usize, usize)> = None;
    let mut index = 0;
    while index < length {
        if letters[index] != '{' {
            index += 1;
            continue;
        }
        let mut cursor = index + 1;
        while cursor < length && letters[cursor].is_whitespace() {
            cursor += 1;
        }
        if cursor == length || letters[cursor] != '"' {
            index += 1;
            continue;
        }
        let mut depth = 1;
        let mut in_string = false;
        let mut escaped = false;
        while cursor < length {
            let letter = letters[cursor];
            if in_string {
                if escaped {
                    escaped = false;
                } else if letter == '\\' {
                    escaped = true;
                } else if letter == '"' {
                    in_string = false;
                }
            } else if letter == '"' {
                in_string = true;
            } else if letter == '{' {
                depth += 1;
            } else if letter == '}' {
                depth -= 1;
                if depth == 0 {
                    last = Some((index, cursor + 1));
                    cursor += 1;
                    break;
                }
            }
            cursor += 1;
        }
        if depth != 0 {
            return None;
        }
        index = cursor;
    }
    let (start, end) = last?;
    if letters[end..]
        .iter()
        .any(|letter| letter.is_alphanumeric() || "\"'{}[]\\".contains(*letter))
    {
        return None;
    }
    Some(letters[start..end].iter().collect())
}

/// Extract one named JSON output from an upstream answer: the fenced
/// ```` ```json ```` block whose object carries the name (trailing blocks
/// first), then the answer's terminal bare object, then the whole answer.
/// Only an object that contains the port binds.
///
/// # Errors
///
/// The sentence a dependent's binding failure reports; a capture cut at the
/// binding cap (`truncated_at`) names the size.
pub(super) fn parse_json_output(
    answer: &str,
    output_name: &str,
    truncated_at: Option<usize>,
) -> Result<Value, String> {
    let terminal = terminal_json_object(answer);
    let candidates = fenced_json_blocks(answer)
        .into_iter()
        .rev()
        .chain(terminal.as_deref())
        .chain([answer.trim()]);
    for candidate in candidates {
        if let Ok(Value::Object(mut parsed)) = serde_json::from_str::<Value>(candidate) {
            if let Some(value) = parsed.remove(output_name) {
                return Ok(value);
            }
        }
    }
    let mut error = format!(
        "no JSON object containing output {} in the upstream answer",
        repr_str(output_name)
    );
    if let Some(truncated_at) = truncated_at {
        let _ = write!(
            error,
            "; the captured answer is truncated at {truncated_at} characters \
             - keep the fenced JSON block compact"
        );
    }
    Err(error)
}

/// Render bound input values into a prompt template: each `{input_name}`
/// placeholder is replaced in one pass (a value that looks like a
/// placeholder is never substituted again), and inputs without a
/// placeholder are appended in a trailing `## Inputs` section.
pub(super) fn render_prompt(template: &str, values: &[(String, String)]) -> String {
    if values.is_empty() {
        return template.to_string();
    }
    let mut used = vec![false; values.len()];
    let mut rendered = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        rendered.push_str(&rest[..open]);
        rest = &rest[open..];
        let placed = values
            .iter()
            .enumerate()
            .find_map(|(index, (name, value))| {
                rest[1..]
                    .strip_prefix(name.as_str())
                    .and_then(|after| after.strip_prefix('}'))
                    .map(|after| (index, value, after))
            });
        if let Some((index, value, after)) = placed {
            used[index] = true;
            rendered.push_str(value);
            rest = after;
        } else {
            rendered.push('{');
            rest = &rest[1..];
        }
    }
    rendered.push_str(rest);
    if used.iter().any(|placed| !placed) {
        rendered.push_str("\n\n## Inputs\n");
        for ((name, value), _) in values.iter().zip(&used).filter(|(_, placed)| !**placed) {
            let _ = writeln!(rendered, "- {name}: {value}");
        }
    }
    rendered
}

/// Numeric equality of two JSON numbers: integers exactly, anything else
/// as floats.
fn numbers_equal(left: &serde_json::Number, right: &serde_json::Number) -> bool {
    let whole = |number: &serde_json::Number| {
        number
            .as_i64()
            .map(i128::from)
            .or_else(|| number.as_u64().map(i128::from))
    };
    match (whole(left), whole(right)) {
        (Some(left), Some(right)) => left == right,
        (_, _) => left.as_f64() == right.as_f64(),
    }
}

/// JSON-strict equality for `eq`/`ne` guards: a boolean never equals a
/// number, numbers compare numerically (1 == 1.0), strings and nulls
/// compare within their own type, and containers never compare equal.
pub(super) fn json_equal(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Null, Value::Null) => true,
        (Value::Number(left), Value::Number(right)) => numbers_equal(left, right),
        (Value::String(left), Value::String(right)) => left == right,
        (_, _) => false,
    }
}

/// Python's `==` over JSON values, the membership test of a `contains`
/// guard: numbers compare numerically and a boolean equals its number
/// (`True == 1`), containers compare element by element.
fn python_equal(left: &Value, right: &Value) -> bool {
    let numeric = |value: &Value| match value {
        Value::Bool(flag) => Some(serde_json::Number::from(u8::from(*flag))),
        Value::Number(number) => Some(number.clone()),
        Value::Null | Value::String(_) | Value::Array(_) | Value::Object(_) => None,
    };
    match (left, right) {
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| python_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .is_some_and(|other| python_equal(value, other))
                })
        }
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Null, Value::Null) => true,
        (_, _) => match (numeric(left), numeric(right)) {
            (Some(left), Some(right)) => numbers_equal(&left, &right),
            (_, _) => false,
        },
    }
}

/// Evaluate one transition guard over a settle's captured outputs. A
/// missing or unparseable port fails every op except `exists` (which is
/// then false); an empty `contains` needle is false.
pub(super) fn guard_passes(when: &Map<String, Value>, outputs: &Map<String, Value>) -> bool {
    let port = field(when, "output").as_str().unwrap_or_default();
    let found: Option<&Value> = match field(when, "path").as_str().filter(|path| !path.is_empty()) {
        Some(path) => outputs
            .get(port)
            .filter(|value| value.is_object())
            .and_then(|root| {
                path.split('.')
                    .try_fold(root, |current, part| current.as_object()?.get(part))
            }),
        None => outputs.get(port),
    };
    let op = field(when, "op").as_str().unwrap_or_default();
    let Some(value) = found else {
        return false;
    };
    let bound = field(when, "value");
    match op {
        "exists" => true,
        "eq" => json_equal(value, bound),
        "ne" => !json_equal(value, bound),
        "gt" | "gte" | "lt" | "lte" => {
            let (Some(value), Some(bound)) = (
                value.as_number().and_then(serde_json::Number::as_f64),
                bound.as_number().and_then(serde_json::Number::as_f64),
            ) else {
                return false;
            };
            match op {
                "gt" => value > bound,
                "gte" => value >= bound,
                "lt" => value < bound,
                _ => value <= bound,
            }
        }
        "contains" => {
            let Some(needle) = bound.as_array().filter(|needle| !needle.is_empty()) else {
                return false;
            };
            match value {
                Value::Array(items) => needle
                    .iter()
                    .all(|wanted| items.iter().any(|item| python_equal(item, wanted))),
                Value::String(text) => needle
                    .iter()
                    .all(|wanted| wanted.as_str().is_some_and(|wanted| text.contains(wanted))),
                Value::Null | Value::Bool(_) | Value::Number(_) | Value::Object(_) => false,
            }
        }
        _ => false,
    }
}
