//! Lossless JSON boundary for the Python HTTP facade.
//!
//! `serde_json::Value` cannot represent Python strings containing lone UTF-16
//! surrogates and its object representation is not the Python insertion-ordered
//! mapping used by `json.loads`. Keep the request representation here until a
//! route explicitly projects the fields it consumes.

use std::fmt::Write as _;

// The HTTP facade accepts at most 4096 request bytes, so 2048 nested
// one-byte containers cover every body it can receive without inheriting
// serde_json's unrelated default depth limit.
const MAX_JSON_DEPTH: usize = 2048;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Integer(String),
    Float(f64),
    String(PythonString),
    Array(Vec<Value>),
    Object(Vec<(PythonString, Value)>),
}

/// Python string code units. UTF-16 permits isolated surrogates and keeps
/// supplementary characters as their corresponding pair for ensure_ascii.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PythonString(Vec<u16>);

impl PythonString {
    pub fn from_text(value: &str) -> Self {
        Self(value.encode_utf16().collect())
    }

    pub fn code_units(&self) -> &[u16] {
        &self.0
    }

    pub fn to_rust_string(&self) -> Result<String, SurrogateError> {
        char::decode_utf16(self.0.iter().copied())
            .collect::<Result<String, _>>()
            .map_err(|error| SurrogateError(error.unpaired_surrogate()))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurrogateError(pub u16);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JsonError {
    pub offset: usize,
}

/// Parse a framed request body as Python's `json.loads(bytes)` does for the
/// supported JSON encodings. HTTP framing and short-read validation belong to
/// the caller and are intentionally outside this module.
pub fn loads(body: &[u8]) -> Result<Value, JsonError> {
    let text = decode_json_bytes(body)?;
    let mut parser = Parser { text: &text, at: 0 };
    let value = parser.value(0)?;
    parser.space();
    if parser.at != text.len() {
        return Err(parser.error());
    }
    Ok(value)
}

/// Python `json.dumps`-compatible output. `indent=None` uses the default
/// compact separators (which include a space after commas and colons).
pub fn dumps(value: &Value, indent: Option<usize>) -> String {
    let mut out = String::new();
    write_value(&mut out, value, indent, 0);
    out
}

/// Response bytes after Python-compatible string encoding. Callers must derive
/// `Content-Length` from this returned vector, after UTF-8 encoding.
pub fn response_bytes(value: &Value, indent: Option<usize>) -> Vec<u8> {
    dumps(value, indent).into_bytes()
}

/// Decimal value of a Unicode `Nd` code point, using the Unicode 16 digit
/// blocks available to the paired CPython 3.14 runtime.
pub fn decimal_digit(character: char) -> Option<u8> {
    const ZEROES: [u32; 76] = [
        0x0030, 0x0660, 0x06f0, 0x07c0, 0x0966, 0x09e6, 0x0a66, 0x0ae6, 0x0b66, 0x0be6, 0x0c66,
        0x0ce6, 0x0d66, 0x0de6, 0x0e50, 0x0ed0, 0x0f20, 0x1040, 0x1090, 0x17e0, 0x1810, 0x1946,
        0x19d0, 0x1a80, 0x1a90, 0x1b50, 0x1bb0, 0x1c40, 0x1c50, 0xa620, 0xa8d0, 0xa900, 0xa9d0,
        0xa9f0, 0xaa50, 0xabf0, 0xff10, 0x104a0, 0x10d30, 0x10d40, 0x11066, 0x110f0, 0x11136,
        0x111d0, 0x112f0, 0x11450, 0x114d0, 0x11650, 0x116c0, 0x116d0, 0x116da, 0x11730, 0x118e0,
        0x11950, 0x11bf0, 0x11c50, 0x11d50, 0x11da0, 0x11f50, 0x16130, 0x16a60, 0x16ac0, 0x16b50,
        0x16d70, 0x1ccf0, 0x1d7ce, 0x1d7d8, 0x1d7e2, 0x1d7ec, 0x1d7f6, 0x1e140, 0x1e2f0, 0x1e4f0,
        0x1e5f1, 0x1e950, 0x1fbf0,
    ];
    let codepoint = character as u32;
    ZEROES.iter().find_map(|zero| {
        (codepoint >= *zero && codepoint < *zero + 10).then_some((codepoint - *zero) as u8)
    })
}

impl Value {
    /// Build the carrier used to reach the existing route/backend validation.
    /// The original values stay in this AST and are used for Python-compatible
    /// error echoes. Values which serde cannot represent are mapped only to a
    /// value with the same Python `int(...)` failure class; they are never used
    /// for serialization or echoed to the caller.
    pub fn resize_fields(&self) -> serde_json::Value {
        let Self::Object(items) = self else {
            return serde_json::Value::Null;
        };
        let mut out = serde_json::Map::new();
        for name in ["cols", "rows"] {
            if let Some((_, value)) = items
                .iter()
                .find(|(key, _)| key.0 == name.encode_utf16().collect::<Vec<_>>())
            {
                out.insert(name.to_owned(), value.to_resize_carrier());
            }
        }
        serde_json::Value::Object(out)
    }

    pub fn resize_value(&self, name: &str) -> Option<&Value> {
        let Self::Object(items) = self else {
            return None;
        };
        items
            .iter()
            .find(|(key, _)| key.0 == name.encode_utf16().collect::<Vec<_>>())
            .map(|(_, value)| value)
    }

    fn to_resize_carrier(&self) -> serde_json::Value {
        use serde_json::Value as J;
        match self {
            Self::Null => J::Null,
            Self::Bool(value) => J::Bool(*value),
            Self::Integer(token) => {
                let fallback = if token.starts_with('-') {
                    serde_json::Number::from(i64::MIN)
                } else {
                    serde_json::Number::from(i64::MAX)
                };
                J::Number(token.parse::<serde_json::Number>().unwrap_or(fallback))
            }
            Self::Float(value) if value.is_finite() => serde_json::Number::from_f64(*value)
                .map(J::Number)
                .unwrap_or(J::Null),
            Self::Float(_) => J::Null,
            Self::String(value) => J::String(value.to_rust_string().unwrap_or_default()),
            // `int(list)` and `int(dict)` fail regardless of their contents.
            Self::Array(_) => J::Array(Vec::new()),
            Self::Object(_) => J::Object(serde_json::Map::new()),
        }
    }
}

impl From<serde_json::Value> for Value {
    fn from(value: serde_json::Value) -> Self {
        use serde_json::Value as J;
        match value {
            J::Null => Self::Null,
            J::Bool(value) => Self::Bool(value),
            J::Number(number) => {
                let token = number.to_string();
                match token.as_str() {
                    "NaN" => Self::Float(f64::NAN),
                    "Infinity" => Self::Float(f64::INFINITY),
                    "-Infinity" => Self::Float(f64::NEG_INFINITY),
                    _ if token.contains(['.', 'e', 'E']) => {
                        Self::Float(token.parse::<f64>().unwrap_or_else(|_| {
                            if token.starts_with('-') {
                                f64::NEG_INFINITY
                            } else {
                                f64::INFINITY
                            }
                        }))
                    }
                    _ => Self::Integer(token),
                }
            }
            J::String(value) => Self::String(PythonString::from_text(&value)),
            J::Array(values) => Self::Array(values.into_iter().map(Self::from).collect()),
            J::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (PythonString::from_text(&key), Self::from(value)))
                    .collect(),
            ),
        }
    }
}

fn decode_json_bytes(bytes: &[u8]) -> Result<String, JsonError> {
    if bytes.starts_with(&[0xef, 0xbb, 0xbf]) {
        return String::from_utf8(bytes[3..].to_vec()).map_err(|e| JsonError {
            offset: e.utf8_error().valid_up_to(),
        });
    }
    if bytes.starts_with(&[0x00, 0x00, 0xfe, 0xff]) {
        return decode_utf32(&bytes[4..], true);
    }
    if bytes.starts_with(&[0xff, 0xfe, 0x00, 0x00]) {
        return decode_utf32(&bytes[4..], false);
    }
    if bytes.starts_with(&[0xfe, 0xff]) {
        return decode_utf16(&bytes[2..], true);
    }
    if bytes.starts_with(&[0xff, 0xfe]) {
        return decode_utf16(&bytes[2..], false);
    }
    if bytes.len() >= 4 {
        if bytes[0] == 0 && bytes[1] == 0 {
            return decode_utf32(bytes, true);
        }
        if bytes[1] == 0 && bytes[2] == 0 {
            return decode_utf32(bytes, false);
        }
        if bytes[0] == 0 {
            return decode_utf16(bytes, true);
        }
        if bytes[1] == 0 {
            return decode_utf16(bytes, false);
        }
    }
    String::from_utf8(bytes.to_vec()).map_err(|e| JsonError {
        offset: e.utf8_error().valid_up_to(),
    })
}

fn decode_utf16(bytes: &[u8], big_endian: bool) -> Result<String, JsonError> {
    if !bytes.len().is_multiple_of(2) {
        return Err(JsonError {
            offset: bytes.len() - 1,
        });
    }
    let (pairs, _) = bytes.as_chunks::<2>();
    let units = pairs.iter().map(|p| {
        if big_endian {
            u16::from_be_bytes([p[0], p[1]])
        } else {
            u16::from_le_bytes([p[0], p[1]])
        }
    });
    char::decode_utf16(units)
        .collect::<Result<_, _>>()
        .map_err(|e| JsonError {
            offset: e.unpaired_surrogate() as usize,
        })
}

fn decode_utf32(bytes: &[u8], big_endian: bool) -> Result<String, JsonError> {
    if !bytes.len().is_multiple_of(4) {
        let remainder = bytes.len() & 3;
        return Err(JsonError {
            offset: bytes.len() - remainder,
        });
    }
    let mut out = String::new();
    let (parts, _) = bytes.as_chunks::<4>();
    for (index, part) in parts.iter().enumerate() {
        let n = if big_endian {
            u32::from_be_bytes(*part)
        } else {
            u32::from_le_bytes(*part)
        };
        out.push(char::from_u32(n).ok_or(JsonError { offset: index * 4 })?);
    }
    Ok(out)
}

struct Parser<'a> {
    text: &'a str,
    at: usize,
}
impl Parser<'_> {
    fn error(&self) -> JsonError {
        JsonError { offset: self.at }
    }
    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }
    fn space(&mut self) {
        while self
            .peek()
            .is_some_and(|b| matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        {
            self.at += 1;
        }
    }
    fn take(&mut self, expected: u8) -> Result<(), JsonError> {
        self.space();
        if self.peek() != Some(expected) {
            return Err(self.error());
        }
        self.at += 1;
        Ok(())
    }
    fn value(&mut self, depth: usize) -> Result<Value, JsonError> {
        self.space();
        match self.peek().ok_or_else(|| self.error())? {
            b'"' => self.string().map(Value::String),
            b'[' | b'{' => {
                if depth >= MAX_JSON_DEPTH {
                    return Err(self.error());
                }
                let object = self.peek() == Some(b'{');
                self.at += 1;
                let end = if object { b'}' } else { b']' };
                self.space();
                if self.peek() == Some(end) {
                    self.at += 1;
                    return Ok(if object {
                        Value::Object(Vec::new())
                    } else {
                        Value::Array(Vec::new())
                    });
                }
                if object {
                    let mut items: Vec<(PythonString, Value)> = Vec::new();
                    loop {
                        let key = self.string()?;
                        self.take(b':')?;
                        let value = self.value(depth + 1)?;
                        if let Some((_, old)) =
                            items.iter_mut().find(|(existing, _)| *existing == key)
                        {
                            *old = value;
                        } else {
                            items.push((key, value));
                        }
                        self.space();
                        if self.peek() == Some(end) {
                            self.at += 1;
                            break;
                        }
                        self.take(b',')?;
                    }
                    Ok(Value::Object(items))
                } else {
                    let mut items = Vec::new();
                    loop {
                        items.push(self.value(depth + 1)?);
                        self.space();
                        if self.peek() == Some(end) {
                            self.at += 1;
                            break;
                        }
                        self.take(b',')?;
                    }
                    Ok(Value::Array(items))
                }
            }
            b't' => {
                self.literal(b"true")?;
                Ok(Value::Bool(true))
            }
            b'f' => {
                self.literal(b"false")?;
                Ok(Value::Bool(false))
            }
            b'n' => {
                self.literal(b"null")?;
                Ok(Value::Null)
            }
            b'N' => {
                self.literal(b"NaN")?;
                Ok(Value::Float(f64::NAN))
            }
            b'I' => {
                self.literal(b"Infinity")?;
                Ok(Value::Float(f64::INFINITY))
            }
            b'-' if self.text[self.at..].starts_with("-Infinity") => {
                self.literal(b"-Infinity")?;
                Ok(Value::Float(f64::NEG_INFINITY))
            }
            b'-' | b'0'..=b'9' => self.number(),
            _ => Err(self.error()),
        }
    }
    fn literal(&mut self, token: &[u8]) -> Result<(), JsonError> {
        if self.text.as_bytes().get(self.at..self.at + token.len()) == Some(token) {
            self.at += token.len();
            Ok(())
        } else {
            Err(self.error())
        }
    }
    fn string(&mut self) -> Result<PythonString, JsonError> {
        self.space();
        self.take(b'"')?;
        let mut units = Vec::new();
        loop {
            let byte = self.peek().ok_or_else(|| self.error())?;
            match byte {
                b'"' => {
                    self.at += 1;
                    return Ok(PythonString(units));
                }
                b'\\' => {
                    self.at += 1;
                    match self.peek().ok_or_else(|| self.error())? {
                        b'"' | b'\\' | b'/' => {
                            units.push(self.peek().unwrap() as u16);
                            self.at += 1;
                        }
                        b'b' => {
                            units.push(8);
                            self.at += 1;
                        }
                        b'f' => {
                            units.push(12);
                            self.at += 1;
                        }
                        b'n' => {
                            units.push(10);
                            self.at += 1;
                        }
                        b'r' => {
                            units.push(13);
                            self.at += 1;
                        }
                        b't' => {
                            units.push(9);
                            self.at += 1;
                        }
                        b'u' => {
                            self.at += 1;
                            units.push(self.hex4()?);
                        }
                        _ => return Err(self.error()),
                    }
                }
                0..=0x1f => return Err(self.error()),
                _ => {
                    let ch = self.text[self.at..]
                        .chars()
                        .next()
                        .ok_or_else(|| self.error())?;
                    self.at += ch.len_utf8();
                    let mut pair = [0u16; 2];
                    units.extend_from_slice(ch.encode_utf16(&mut pair));
                }
            }
        }
    }
    fn hex4(&mut self) -> Result<u16, JsonError> {
        let end = self.at.checked_add(4).ok_or_else(|| self.error())?;
        let token = self
            .text
            .as_bytes()
            .get(self.at..end)
            .ok_or_else(|| self.error())?;
        let mut n = 0u16;
        for &b in token {
            n = (n << 4)
                | match b {
                    b'0'..=b'9' => (b - b'0') as u16,
                    b'a'..=b'f' => (b - b'a' + 10) as u16,
                    b'A'..=b'F' => (b - b'A' + 10) as u16,
                    _ => return Err(self.error()),
                };
        }
        self.at = end;
        Ok(n)
    }
    fn number(&mut self) -> Result<Value, JsonError> {
        let start = self.at;
        if self.peek() == Some(b'-') {
            self.at += 1;
        }
        match self.peek() {
            Some(b'0') => self.at += 1,
            Some(b'1'..=b'9') => {
                self.at += 1;
                while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                    self.at += 1;
                }
            }
            _ => return Err(self.error()),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.at += 1;
            let before = self.at;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.at += 1;
            }
            if self.at == before {
                return Err(self.error());
            }
        }
        if self.peek().is_some_and(|b| matches!(b, b'e' | b'E')) {
            is_float = true;
            self.at += 1;
            if self.peek().is_some_and(|b| matches!(b, b'+' | b'-')) {
                self.at += 1;
            }
            let before = self.at;
            while self.peek().is_some_and(|b| b.is_ascii_digit()) {
                self.at += 1;
            }
            if self.at == before {
                return Err(self.error());
            }
        }
        let token = &self.text[start..self.at];
        if is_float {
            Ok(Value::Float(token.parse().unwrap_or_else(|_| {
                if token.starts_with('-') {
                    f64::NEG_INFINITY
                } else {
                    f64::INFINITY
                }
            })))
        } else {
            let normalized = token
                .parse::<i128>()
                .map(|n| n.to_string())
                .unwrap_or_else(|_| token.to_owned());
            Ok(Value::Integer(normalized))
        }
    }
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>, depth: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(v) => out.push_str(if *v { "true" } else { "false" }),
        Value::Integer(v) => out.push_str(v),
        Value::Float(v) => {
            if v.is_nan() {
                out.push_str("NaN");
            } else if *v == f64::INFINITY {
                out.push_str("Infinity");
            } else if *v == f64::NEG_INFINITY {
                out.push_str("-Infinity");
            } else {
                out.push_str(&ds_core::usage::py_float_repr(*v));
            }
        }
        Value::String(s) => write_string(out, s),
        Value::Array(items) => write_sequence(out, items.iter(), indent, depth, '[', ']'),
        Value::Object(items) => {
            out.push('{');
            if !items.is_empty() {
                for (i, (key, val)) in items.iter().enumerate() {
                    write_item_prefix(out, indent, depth + 1, i == 0);
                    write_string(out, key);
                    out.push(':');
                    out.push(' ');
                    write_value(out, val, indent, depth + 1);
                }
                write_close(out, indent, depth);
            }
            out.push('}');
        }
    }
}

fn write_sequence<'a>(
    out: &mut String,
    values: impl Iterator<Item = &'a Value>,
    indent: Option<usize>,
    depth: usize,
    open: char,
    close: char,
) {
    let values: Vec<_> = values.collect();
    out.push(open);
    for (i, value) in values.iter().enumerate() {
        write_item_prefix(out, indent, depth + 1, i == 0);
        write_value(out, value, indent, depth + 1);
    }
    if !values.is_empty() {
        write_close(out, indent, depth);
    }
    out.push(close);
}
fn write_item_prefix(out: &mut String, indent: Option<usize>, depth: usize, first: bool) {
    if !first {
        out.push(',');
        if indent.is_none() {
            out.push(' ');
        }
    }
    if let Some(width) = indent {
        out.push('\n');
        for _ in 0..width.saturating_mul(depth) {
            out.push(' ');
        }
    }
}
fn write_close(out: &mut String, indent: Option<usize>, depth: usize) {
    if let Some(width) = indent {
        out.push('\n');
        for _ in 0..width.saturating_mul(depth) {
            out.push(' ');
        }
    }
}
fn write_string(out: &mut String, value: &PythonString) {
    out.push('"');
    for &unit in &value.0 {
        match unit {
            8 => out.push_str("\\b"),
            9 => out.push_str("\\t"),
            10 => out.push_str("\\n"),
            12 => out.push_str("\\f"),
            13 => out.push_str("\\r"),
            0x20..=0x7e if unit != b'"' as u16 && unit != b'\\' as u16 => {
                out.push(unit as u8 as char)
            }
            0x20..=0x7e => {
                out.push('\\');
                out.push(unit as u8 as char);
            }
            _ => {
                let _ = write!(out, "\\u{unit:04x}");
            }
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    const PYTHON_INDENTED: &str = "{\n  \"z\": \"snowman \\u2603\",\n  \"bad\\ud800\": \"\\udfff\",\n  \"pair\": \"\\ud83d\\ude00\",\n  \"nan\": NaN,\n  \"pos\": Infinity,\n  \"neg\": -Infinity,\n  \"int\": 123456789012345678901234567890,\n  \"flt\": 1.0,\n  \"exp\": 1e+20\n}";

    #[test]
    fn actual_python_json_oracle_preserves_surrogates_order_numbers_and_indent() {
        // Oracle produced with CPython json.loads + json.dumps(..., indent=2).
        let raw = br#"{"z":"snowman \u2603","bad\ud800":"\udfff","pair":"\ud83d\ude00","nan":NaN,"pos":Infinity,"neg":-Infinity,"int":123456789012345678901234567890,"flt":1.0,"exp":1e+20}"#;
        let parsed = loads(raw).unwrap();
        assert_eq!(dumps(&parsed, Some(2)), PYTHON_INDENTED);
        let compact = dumps(&parsed, None);
        assert_eq!(dumps(&loads(compact.as_bytes()).unwrap(), None), compact);
        let Value::Object(fields) = parsed else {
            panic!("expected object")
        };
        assert_eq!(
            fields
                .iter()
                .map(|(key, _)| key
                    .to_rust_string()
                    .unwrap_or_else(|_| "<surrogate>".into()))
                .collect::<Vec<_>>(),
            [
                "z",
                "<surrogate>",
                "pair",
                "nan",
                "pos",
                "neg",
                "int",
                "flt",
                "exp"
            ]
        );
    }

    #[test]
    fn malformed_json_and_python_byte_encodings_are_explicit() {
        for raw in [b"{".as_slice(), b"[1,]", b"01", b"\"\\x\"", b"true false"] {
            assert!(loads(raw).is_err(), "accepted {raw:?}");
        }
        assert_eq!(
            loads(&[0xff, 0xfe, b'{', 0, b'}', 0]).unwrap(),
            Value::Object(vec![])
        );
        assert_eq!(
            loads(&[0, 0, 0, b'[', 0, 0, 0, b']']).unwrap(),
            Value::Array(vec![])
        );
    }

    #[test]
    fn resize_projection_ignores_unknown_lone_surrogate_fields() {
        let value = loads(br#"{"\ud800":"ignored","cols":80.0,"rows":24}"#).unwrap();
        assert_eq!(
            value.resize_fields(),
            serde_json::json!({"cols":80.0,"rows":24})
        );
        let surrogate_selected = loads(br#"{"cols":"\ud800","rows":24}"#).unwrap();
        assert_eq!(
            surrogate_selected.resize_fields(),
            serde_json::json!({"cols":"","rows":24})
        );
        assert_eq!(
            dumps(
                &Value::Object(vec![(
                    PythonString::from_text("cols"),
                    surrogate_selected.resize_value("cols").unwrap().clone(),
                )]),
                None,
            ),
            r#"{"cols": "\ud800"}"#
        );
    }

    #[test]
    fn escaped_quoted_keys_match_python_compact_encoding() {
        let value = loads(br#"{"quote\"key":"value\nline","slash\\key":"x"}"#).unwrap();
        assert_eq!(
            dumps(&value, None),
            r#"{"quote\"key": "value\nline", "slash\\key": "x"}"#
        );
    }

    #[test]
    fn unicode_decimal_digit_table_matches_python_decimal_categories() {
        assert_eq!(decimal_digit('١'), Some(1));
        assert_eq!(decimal_digit('𝟠'), Some(8));
        assert_eq!(decimal_digit('²'), None);
    }

    #[test]
    fn exact_4096_byte_json_body_is_parsed_without_framing_changes() {
        let mut body = br#"{"cols":80,"rows":24}"#.to_vec();
        body.resize(4096, b' ');
        assert!(loads(&body).is_ok());
        body.push(b' ');
        assert!(loads(&body).is_ok());
        assert_eq!(
            response_bytes(&Value::String(PythonString::from_text("☃")), None),
            br#""\u2603""#
        );
        let nested = format!("{}0{}", "[".repeat(2047), "]".repeat(2047));
        assert_eq!(nested.len(), 4095);
        assert!(loads(nested.as_bytes()).is_ok());
    }
}
