//! Python's text forms of JSON values, which the factory's wording and
//! prompts were defined in: `repr` (every validation sentence quotes values
//! with it), truthiness, and `json.dumps` with its default separators (the
//! form a bound input takes inside a rendered prompt).

use std::fmt::Write as _;

use serde_json::Value;

/// `repr(value)` for a JSON value: `None`, `True`, `'text'`, `[1, 'a']`,
/// `{'key': 1.0}`.
pub(super) fn repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number_text(number),
        Value::String(text) => repr_str(text),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(repr).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(fields) => {
            let inner: Vec<String> = fields
                .iter()
                .map(|(key, item)| format!("{}: {}", repr_str(key), repr(item)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

/// `repr(text)` for a string: single quotes unless the text holds one and
/// no double quote, with Python's escapes.
pub(super) fn repr_str(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for letter in text.chars() {
        match letter {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            found if found == quote => {
                out.push('\\');
                out.push(found);
            }
            control if (control as u32) < 0x20 || control as u32 == 0x7f => {
                let _ = write!(out, "\\x{:02x}", control as u32);
            }
            // Python escapes the C1 controls and the non-printable
            // separators the same way; everything printable stays.
            control if (0x80..0xa0).contains(&(control as u32)) || control as u32 == 0xad => {
                let _ = write!(out, "\\x{:02x}", control as u32);
            }
            plain => out.push(plain),
        }
    }
    out.push(quote);
    out
}

/// A list of fixed words as Python prints a list of strings.
pub(super) fn repr_words(words: &[&str]) -> String {
    let inner: Vec<String> = words.iter().map(|word| repr_str(word)).collect();
    format!("[{}]", inner.join(", "))
}

/// A JSON number as Python prints it: integers as they are, floats in
/// `float.__repr__` form (`1.0`, `1e+16`, `1.5e-05`).
fn number_text(number: &serde_json::Number) -> String {
    if number.is_i64() || number.is_u64() {
        return number.to_string();
    }
    let float = number.as_f64().unwrap_or(0.0);
    let magnitude = float.abs();
    if float == 0.0 || (1e-4..1e16).contains(&magnitude) {
        return format!("{float:?}");
    }
    let scientific = format!("{float:e}");
    match scientific.split_once('e') {
        Some((mantissa, exponent)) => {
            let (sign, digits) = match exponent.strip_prefix('-') {
                Some(digits) => ('-', digits),
                None => ('+', exponent),
            };
            format!("{mantissa}e{sign}{digits:0>2}")
        }
        None => scientific,
    }
}

/// Python's truthiness of a JSON value: null, false, zero, and empty
/// strings and containers are false.
pub(super) fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => number.as_f64().is_some_and(|float| float != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// `type(value).__name__` for a JSON value.
pub(super) fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_i64() || number.is_u64() => "int",
        Value::Number(_) => "float",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// `json.dumps(value)` with Python's defaults: `", "` and `": "`
/// separators and every non-ASCII character escaped.
pub(super) fn json_dumps(value: &Value) -> String {
    let mut out = String::new();
    dumps_into(value, &mut out);
    out
}

fn dumps_into(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number_text(number)),
        Value::String(text) => dumps_str(text, out),
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                dumps_into(item, out);
            }
            out.push(']');
        }
        Value::Object(fields) => {
            out.push('{');
            for (index, (key, item)) in fields.iter().enumerate() {
                if index > 0 {
                    out.push_str(", ");
                }
                dumps_str(key, out);
                out.push_str(": ");
                dumps_into(item, out);
            }
            out.push('}');
        }
    }
}

fn dumps_str(text: &str, out: &mut String) {
    out.push('"');
    for letter in text.chars() {
        match letter {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ascii if (' '..='~').contains(&ascii) => out.push(ascii),
            other => {
                let mut units = [0u16; 2];
                for unit in other.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{unit:04x}");
                }
            }
        }
    }
    out.push('"');
}
