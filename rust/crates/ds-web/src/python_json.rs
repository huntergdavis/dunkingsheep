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
    pub fn from_str(value: &str) -> Self {
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

impl Value {
    /// Project only the two fields consumed by the existing resize route.
    /// Unknown keys (including keys containing lone surrogates) are ignored as
    /// they are by `route_post`; selected values must be representable by the
    /// route's serde value interface or projection returns an explicit error.
    pub fn resize_fields(&self) -> Result<serde_json::Value, ProjectionError> {
        let Self::Object(items) = self else {
            return Ok(serde_json::Value::Null);
        };
        let mut out = serde_json::Map::new();
        for name in ["cols", "rows"] {
            if let Some((_, value)) = items
                .iter()
                .find(|(key, _)| key.0 == name.encode_utf16().collect::<Vec<_>>())
            {
                out.insert(name.to_owned(), value.to_serde()?);
            }
        }
        Ok(serde_json::Value::Object(out))
    }

    fn to_serde(&self) -> Result<serde_json::Value, ProjectionError> {
        use serde_json::Value as J;
        Ok(match self {
            Self::Null => J::Null,
            Self::Bool(value) => J::Bool(*value),
            Self::Integer(token) => J::Number(
                token
                    .parse()
                    .map_err(|_| ProjectionError("integer outside route carrier"))?,
            ),
            Self::Float(value) => J::Number(
                ds_core::wire_json::float_value(*value)
                    .as_number()
                    .expect("wire JSON float adapter always returns a number")
                    .clone(),
            ),
            Self::String(value) => {
                J::String(value.to_rust_string().map_err(|_| {
                    ProjectionError("selected route string contains a lone surrogate")
                })?)
            }
            Self::Array(values) => J::Array(
                values
                    .iter()
                    .map(Self::to_serde)
                    .collect::<Result<_, _>>()?,
            ),
            Self::Object(items) => {
                let mut out = serde_json::Map::new();
                for (key, value) in items {
                    let key = key.to_rust_string().map_err(|_| {
                        ProjectionError("selected route object key contains a lone surrogate")
                    })?;
                    out.insert(key, value.to_serde()?);
                }
                J::Object(out)
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionError(pub &'static str);

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
            J::String(value) => Self::String(PythonString::from_str(&value)),
            J::Array(values) => Self::Array(values.into_iter().map(Self::from).collect()),
            J::Object(values) => Self::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (PythonString::from_str(&key), Self::from(value)))
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
    if bytes.len() % 2 != 0 {
        return Err(JsonError {
            offset: bytes.len() - 1,
        });
    }
    let units = bytes.chunks_exact(2).map(|p| {
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
    if bytes.len() % 4 != 0 {
        return Err(JsonError {
            offset: bytes.len() - bytes.len() % 4,
        });
    }
    let mut out = String::new();
    for (index, part) in bytes.chunks_exact(4).enumerate() {
        let n = if big_endian {
            u32::from_be_bytes(part.try_into().unwrap())
        } else {
            u32::from_le_bytes(part.try_into().unwrap())
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
                .unwrap_or_else(|_| {
                    if token.starts_with('-') {
                        token.to_owned()
                    } else {
                        token.to_owned()
                    }
                });
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
                    if indent.is_some() {
                        out.push(' ');
                    } else {
                        out.push(' ');
                    }
                    write_value(out, val, indent, depth + 1);
                }
                write_close(out, indent, depth);
            }
            out.push('}');
        }
    }
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
            value.resize_fields().unwrap(),
            serde_json::json!({"cols":80.0,"rows":24})
        );
        let surrogate_selected = loads(br#"{"cols":"\ud800","rows":24}"#).unwrap();
        assert!(surrogate_selected.resize_fields().is_err());
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
    fn exact_4096_byte_json_body_is_parsed_without_framing_changes() {
        let mut body = br#"{"cols":80,"rows":24}"#.to_vec();
        body.resize(4096, b' ');
        assert!(loads(&body).is_ok());
        body.push(b' ');
        assert!(loads(&body).is_ok());
        assert_eq!(
            response_bytes(&Value::String(PythonString::from_str("☃")), None),
            br#""\u2603""#
        );
        let nested = format!("{}0{}", "[".repeat(2047), "]".repeat(2047));
        assert_eq!(nested.len(), 4095);
        assert!(loads(nested.as_bytes()).is_ok());
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
