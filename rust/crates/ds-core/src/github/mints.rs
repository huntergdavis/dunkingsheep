//! Source-faithful mint attribution (`dunk_github.py:516-611`, t1715/t1717).
//! The legacy ledger has no lock: replace is atomic for readers, but concurrent read/modify/
//! write calls can lose updates or collide on the fixed `.tmp`. This port preserves that
//! limit; it adds neither locking, fsync, nor a different failure policy.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;
use serde_json::{Map, Value};

use super::GitHubError;

const DEFAULT_REPOS: [&str; 2] = ["huntergdavis/dunkingsheep", "huntergdavis/poolrad-decomp"];
const AUTHOR_MINT_FILE: &str = "github-author-mints.json";
const WINDOW: f64 = 1800.0;
const KEEP: f64 = 86400.0;

/// Repositories configured for review; malformed/missing/empty input uses the source defaults.
pub fn repos_config(config_dir: &Path) -> Vec<String> {
    if let Some(doc) = Json::read(&config_dir.join("github-repos.json"), "") {
        let repos = if doc.value.is_object() {
            doc.value.get("repos")
        } else {
            Some(&doc.value)
        };
        if let Some(repos) = repos
            .and_then(Value::as_array)
            .filter(|items| !items.is_empty())
        {
            return repos.iter().map(|value| doc.py_str(value)).collect();
        }
    }
    DEFAULT_REPOS.iter().map(|repo| (*repo).into()).collect()
}

pub fn author_mint_path(path: Option<&Path>, config_dir: &Path) -> PathBuf {
    path.filter(|path| !path.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| config_dir.join(AUTHOR_MINT_FILE))
}

/// Return None for blank panes or filesystem failures. Source type/coercion errors remain
/// errors; the Python gh_token caller catches these independently of token minting.
pub fn record_author_mint(
    pane: &str,
    at: Option<f64>,
    path: Option<&Path>,
    config_dir: &Path,
) -> Result<Option<PathBuf>, GitHubError> {
    let pane = pane.trim();
    if pane.is_empty() {
        return Ok(None);
    }
    let path = author_mint_path(path, config_dir);
    let now = at.unwrap_or_else(crate::clock::now_s);
    let mut doc = Json::read(&path, pane).unwrap_or_else(|| Json::empty(pane));
    let mut panes = doc
        .value
        .get("panes")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    let prior = panes.get(pane).cloned().unwrap_or(Value::Null);
    let mut stamps = Vec::new();
    for value in doc.iterable(&prior)? {
        if doc.is_number(&value) {
            let stamp = doc
                .float(&value)
                .map_err(|error| GitHubError(error.message().into()))?;
            if now - stamp <= KEEP {
                stamps.push(doc.number(stamp));
            }
        }
    }
    let rounded = format!("{now:.3}")
        .parse::<f64>()
        .map_err(|error| GitHubError(error.to_string()))?;
    stamps.push(doc.number(rounded));
    let stamps = stamps.split_off(stamps.len().saturating_sub(50));
    panes.insert(pane.into(), Value::Array(stamps));
    let mut payload = Map::new();
    payload.insert("panes".into(), Value::Object(panes));
    payload.insert("updated_at".into(), doc.number(rounded));
    let text = doc.dump(&Value::Object(payload));
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let write = || -> std::io::Result<()> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&temporary)?;
            file.write_all(text.as_bytes())?;
        }
        fs::rename(&temporary, &path)
    };
    if write().is_err() {
        return Ok(None);
    }
    Ok(Some(path))
}

pub fn author_from_mints(
    since: Option<f64>,
    path: Option<&Path>,
    now: Option<f64>,
    config_dir: &Path,
) -> Result<Option<String>, GitHubError> {
    let now = now.unwrap_or_else(crate::clock::now_s);
    let since = since.unwrap_or(now - WINDOW);
    let Some(doc) = Json::read(&author_mint_path(path, config_dir), "") else {
        return Ok(None);
    };
    let Some(panes) = doc.value.get("panes").and_then(Value::as_object) else {
        return Ok(None);
    };
    let mut recent = BTreeSet::new();
    for (pane, stamps) in panes {
        for value in doc.iterable(stamps)? {
            match doc.float(&value) {
                Ok(stamp) if since < stamp && stamp <= now => {
                    recent.insert(pane.clone());
                    break;
                }
                Err(FloatError::Overflow) => {
                    return Err(GitHubError(FloatError::Overflow.message().into()))
                }
                _ => {}
            }
        }
    }
    Ok((recent.len() == 1)
        .then(|| recent.into_iter().next())
        .flatten())
}

#[derive(Debug)]
pub(super) enum FloatError {
    Invalid,
    Overflow,
}
impl FloatError {
    fn message(&self) -> &'static str {
        match self {
            Self::Invalid => "float() argument must be a string or a real number",
            Self::Overflow => "int too large to convert to float",
        }
    }
}

/// Preserve Python JSON's numeric types, arbitrary integers and NaN/Infinity while using
/// serde for structure. Collision-free markers never escape into the persisted ledger.
struct Json {
    value: Value,
    numbers: BTreeMap<String, String>,
    prefix: String,
}
impl Json {
    fn empty(reserved: &str) -> Self {
        let mut prefix = "__ds_mint_number__".to_string();
        while reserved.contains(&prefix) {
            prefix.push('_');
        }
        Self {
            value: Value::Null,
            numbers: BTreeMap::new(),
            prefix,
        }
    }
    fn read(path: &Path, reserved: &str) -> Option<Self> {
        let text = fs::read_to_string(path).ok()?;
        let bytes = text.as_bytes();
        let mut strings = Vec::new();
        let mut index = 0;
        while index < bytes.len() {
            if bytes[index] == b'"' {
                let end = string_end(bytes, index)?;
                strings.push(serde_json::from_str::<String>(&text[index..end]).ok()?);
                index = end;
            } else {
                index += 1;
            }
        }
        let mut doc = Self::empty(reserved);
        while text.contains(&doc.prefix) || strings.iter().any(|value| value.contains(&doc.prefix))
        {
            doc.prefix.push('_');
        }
        static NUMBER: OnceLock<Regex> = OnceLock::new();
        let number = NUMBER.get_or_init(|| {
            Regex::new(r"^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?$")
                .expect("static JSON number regex")
        });
        let mut rewritten = String::new();
        index = 0;
        let mut copied = 0;
        while index < bytes.len() {
            if bytes[index] == b'"' {
                index = string_end(bytes, index)?;
                continue;
            }
            if !bytes[index].is_ascii() {
                return None;
            }
            let rest = &text[index..];
            let constant = ["-Infinity", "Infinity", "NaN"]
                .into_iter()
                .find(|token| rest.starts_with(token));
            let end = if let Some(token) = constant {
                index + token.len()
            } else if bytes[index] == b'-' || bytes[index].is_ascii_digit() {
                let mut end = index + 1;
                while end < bytes.len()
                    && (bytes[end].is_ascii_digit() || b".eE+-".contains(&bytes[end]))
                {
                    end += 1;
                }
                if !number.is_match(&text[index..end]) {
                    return None;
                }
                end
            } else {
                index += 1;
                continue;
            };
            rewritten.push_str(&text[copied..index]);
            let marker = doc.raw_number(&text[index..end]);
            rewritten.push_str(&serde_json::to_string(&marker).ok()?);
            index = end;
            copied = end;
        }
        rewritten.push_str(&text[copied..]);
        doc.value = serde_json::from_str(&rewritten).ok()?;
        Some(doc)
    }
    fn raw_number(&mut self, raw: &str) -> String {
        let marker = format!("{}{}", self.prefix, self.numbers.len());
        self.numbers.insert(marker.clone(), raw.into());
        marker
    }
    fn number(&mut self, value: f64) -> Value {
        let raw = if value.is_nan() {
            "NaN".into()
        } else if value == f64::INFINITY {
            "Infinity".into()
        } else if value == f64::NEG_INFINITY {
            "-Infinity".into()
        } else {
            crate::usage::py_float_repr(value)
        };
        Value::String(self.raw_number(&raw))
    }
    fn raw(&self, value: &Value) -> Option<&str> {
        value
            .as_str()
            .and_then(|text| self.numbers.get(text))
            .map(String::as_str)
    }
    fn is_number(&self, value: &Value) -> bool {
        self.raw(value).is_some() || value.is_number() || value.is_boolean()
    }
    fn float(&self, value: &Value) -> Result<f64, FloatError> {
        if let Some(raw) = self.raw(value) {
            let result = parse_float(raw)?;
            if integer(raw) && !result.is_finite() {
                return Err(FloatError::Overflow);
            }
            return Ok(result);
        }
        match value {
            Value::Bool(value) => Ok(if *value { 1.0 } else { 0.0 }),
            Value::String(text) => parse_float(text),
            Value::Number(number) => number.as_f64().ok_or(FloatError::Overflow),
            _ => Err(FloatError::Invalid),
        }
    }
    fn truthy(&self, value: &Value) -> bool {
        if self.raw(value).is_some() {
            return self.float(value).map(|value| value != 0.0).unwrap_or(true);
        }
        match value {
            Value::Null => false,
            Value::Bool(value) => *value,
            Value::String(value) => !value.is_empty(),
            Value::Array(value) => !value.is_empty(),
            Value::Object(value) => !value.is_empty(),
            Value::Number(value) => value.as_f64() != Some(0.0),
        }
    }
    fn iterable(&self, value: &Value) -> Result<Vec<Value>, GitHubError> {
        if !self.truthy(value) {
            return Ok(Vec::new());
        }
        if let Some(raw) = self.raw(value) {
            return Err(GitHubError(format!(
                "'{}' object is not iterable",
                if integer(raw) { "int" } else { "float" }
            )));
        }
        match value {
            Value::Array(values) => Ok(values.clone()),
            Value::Object(values) => Ok(values.keys().cloned().map(Value::String).collect()),
            Value::String(text) => Ok(text
                .chars()
                .map(|value| Value::String(value.to_string()))
                .collect()),
            Value::Bool(_) => Err(GitHubError("'bool' object is not iterable".into())),
            _ => Err(GitHubError("number object is not iterable".into())),
        }
    }
    fn py_str(&self, value: &Value) -> String {
        if let Some(raw) = self.raw(value) {
            return number_text(raw, false);
        }
        match value {
            Value::String(text) => text.clone(),
            _ => self.py_repr(value),
        }
    }
    fn py_repr(&self, value: &Value) -> String {
        if let Some(raw) = self.raw(value) {
            return number_text(raw, false);
        }
        match value {
            Value::String(text) => repr_string(text),
            Value::Null => "None".into(),
            Value::Bool(value) => if *value { "True" } else { "False" }.into(),
            Value::Array(values) => format!(
                "[{}]",
                values
                    .iter()
                    .map(|value| self.py_repr(value))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Object(values) => format!(
                "{{{}}}",
                values
                    .iter()
                    .map(|(key, value)| format!("{}: {}", repr_string(key), self.py_repr(value)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Number(number) => number.to_string(),
        }
    }
    fn dump(&self, value: &Value) -> String {
        if let Some(raw) = self.raw(value) {
            return number_text(raw, true);
        }
        match value {
            Value::String(text) => json_string(text),
            Value::Null => "null".into(),
            Value::Bool(value) => value.to_string(),
            Value::Array(values) => format!(
                "[{}]",
                values
                    .iter()
                    .map(|value| self.dump(value))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Object(values) => format!(
                "{{{}}}",
                values
                    .iter()
                    .map(|(key, value)| format!("{}: {}", json_string(key), self.dump(value)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            Value::Number(number) => number.to_string(),
        }
    }
}
fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut index = start + 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index += 2,
            b'"' => return Some(index + 1),
            _ => index += 1,
        }
    }
    None
}
fn integer(raw: &str) -> bool {
    !raw.contains(['.', 'e', 'E']) && !matches!(raw, "NaN" | "Infinity" | "-Infinity")
}
pub(super) fn parse_float(raw: &str) -> Result<f64, FloatError> {
    let converted: String = raw
        .trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii() {
                return ch;
            }
            static DIGIT: OnceLock<Regex> = OnceLock::new();
            let digit = DIGIT
                .get_or_init(|| Regex::new(r"^\p{Nd}$").expect("static decimal category regex"));
            if !digit.is_match(&ch.to_string()) {
                return ch;
            }
            let mut start = ch as u32;
            while start > 0
                && char::from_u32(start - 1).is_some_and(|value| digit.is_match(&value.to_string()))
            {
                start -= 1;
            }
            char::from(b'0' + ((ch as u32 - start) % 10) as u8)
        })
        .collect();
    let raw = converted.as_str();
    if raw.as_bytes().iter().enumerate().any(|(i, b)| {
        *b == b'_'
            && (i == 0
                || i + 1 == raw.len()
                || !raw.as_bytes()[i - 1].is_ascii_digit()
                || !raw.as_bytes()[i + 1].is_ascii_digit())
    }) {
        return Err(FloatError::Invalid);
    }
    let raw = raw.replace('_', "");
    match raw.to_ascii_lowercase().as_str() {
        "nan" | "+nan" | "-nan" => Ok(f64::NAN),
        "inf" | "+inf" | "infinity" | "+infinity" => Ok(f64::INFINITY),
        "-inf" | "-infinity" => Ok(f64::NEG_INFINITY),
        _ => raw.parse().map_err(|_| FloatError::Invalid),
    }
}
fn number_text(raw: &str, json: bool) -> String {
    if integer(raw) {
        return if raw == "-0" { "0".into() } else { raw.into() };
    }
    let value = parse_float(raw).expect("validated numeric token");
    if json && !value.is_finite() {
        return if value.is_nan() {
            "NaN"
        } else if value.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        }
        .into();
    }
    crate::usage::py_float_repr(value)
}
fn json_string(text: &str) -> String {
    let mut result = String::from("\"");
    for ch in text.chars() {
        match ch {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\u{8}' => result.push_str("\\b"),
            '\u{c}' => result.push_str("\\f"),
            ch if !('\u{20}'..'\u{7f}').contains(&ch) => {
                for unit in ch.encode_utf16(&mut [0; 2]) {
                    result.push_str(&format!("\\u{unit:04x}"));
                }
            }
            ch => result.push(ch),
        }
    }
    result.push('"');
    result
}
fn repr_string(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut result = quote.to_string();
    static NONPRINT: OnceLock<Regex> = OnceLock::new();
    let nonprint = NONPRINT
        .get_or_init(|| Regex::new(r"^[\p{C}\p{Z}]$").expect("static Unicode category regex"));
    for ch in text.chars() {
        match ch {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            ch if ch == quote => {
                result.push('\\');
                result.push(ch);
            }
            ch if ch != ' ' && nonprint.is_match(&ch.to_string()) => {
                let n = ch as u32;
                result.push_str(&if n <= 255 {
                    format!("\\x{n:02x}")
                } else if n <= 65535 {
                    format!("\\u{n:04x}")
                } else {
                    format!("\\U{n:08x}")
                });
            }
            ch => result.push(ch),
        }
    }
    result.push(quote);
    result
}

#[cfg(test)]
mod tests;
