//! `json.dumps` and `repr()` the way CPython prints them.
//!
//! The Python server writes `json.dumps(payload)` (", " and ": " separators, non-ASCII
//! escaped as `\uXXXX`) and `json.dumps(result, indent=2)` for the text a tool returns, and
//! formats names with `{name!r}` in its error texts. Clients and tests match on those bytes,
//! so this reproduces them. One known difference: `serde_json` keeps object keys sorted
//! (`preserve_order` would pull in `indexmap`, which RUST_PORT.md section 3 does not list), so
//! key ORDER inside an object can differ from the Python's insertion order; the JSON is equal.

use serde_json::Value;

/// `json.dumps(value)` (`indent` None) or `json.dumps(value, indent=n)`.
pub fn dumps(value: &Value, indent: Option<usize>) -> String {
    let mut out = String::new();
    write_value(&mut out, value, indent, 0);
    out
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&number.to_string()),
        Value::String(text) => write_string(out, text),
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Array(items) => {
            out.push('[');
            for (position, item) in items.iter().enumerate() {
                separator(out, position, indent, depth + 1);
                write_value(out, item, indent, depth + 1);
            }
            close(out, ']', indent, depth);
        }
        Value::Object(map) => {
            out.push('{');
            for (position, (key, item)) in map.iter().enumerate() {
                separator(out, position, indent, depth + 1);
                write_string(out, key);
                out.push_str(": ");
                write_value(out, item, indent, depth + 1);
            }
            close(out, '}', indent, depth);
        }
    }
}

fn separator(out: &mut String, position: usize, indent: Option<usize>, depth: usize) {
    match indent {
        Some(width) => {
            if position > 0 {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&" ".repeat(width * depth));
        }
        None if position > 0 => out.push_str(", "),
        None => {}
    }
}

fn close(out: &mut String, bracket: char, indent: Option<usize>, depth: usize) {
    if let Some(width) = indent {
        out.push('\n');
        out.push_str(&" ".repeat(width * depth));
    }
    out.push(bracket);
}

/// `json.dumps` with the default `ensure_ascii=True`.
fn write_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='\u{7e}' | '\u{7f}' => out.push(character),
            other => {
                let mut units = [0u16; 2];
                for unit in other.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

/// `repr()` of what a JSON value becomes in Python (`{name!r}` in the error texts).
pub fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(number) => number.to_string(),
        Value::String(text) => str_repr(text),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", parts.join(", "))
        }
        Value::Object(map) => {
            let parts: Vec<String> = map
                .iter()
                .map(|(key, item)| format!("{}: {}", str_repr(key), py_repr(item)))
                .collect();
            format!("{{{}}}", parts.join(", "))
        }
    }
}

/// `repr(str)`: single quotes unless the text holds a `'` and no `"`.
fn str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::new();
    out.push(quote);
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other == quote => {
                out.push('\\');
                out.push(other);
            }
            other if (other as u32) < 0x20 || other as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", other as u32));
            }
            other => out.push(other),
        }
    }
    out.push(quote);
    out
}

/// The Python type name of a JSON value, for the `AttributeError` texts the Python surfaces.
pub fn py_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Number(number) if number.is_f64() => "float",
        Value::Number(_) => "int",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
    }
}

/// Python truthiness of a JSON value (`params or {}`).
pub fn py_falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(number) => number.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}
