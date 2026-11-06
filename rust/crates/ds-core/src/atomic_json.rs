//! Atomic JSON writes with mode 0600 - replacing the Python's 15 call sites.
//!
//! Ported from the `mkstemp` + `os.replace` + `chmod(0o600)` dance copied through
//! 12 modules (RUST_PORT.md section 6). The two properties that matter and are
//! tested here: a reader never sees a half-written file, and the file is never
//! group/world readable, because these files hold tokens.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::error::DunkError;

/// Preserve whether an atomic state write failed because of an OS operation or
/// because the payload/path could not be represented. A few Python-compatible
/// mutation paths intentionally ignore only the former.
#[derive(Debug)]
pub(crate) enum ClassifiedWriteError {
    Io(DunkError),
    Other(DunkError),
}

impl ClassifiedWriteError {
    pub(crate) fn into_dunk_error(self) -> DunkError {
        match self {
            Self::Io(error) | Self::Other(error) => error,
        }
    }
}

/// Read JSON, or the default when the file does not exist.
///
/// A missing file is the normal first-run state (the Python's `except OSError:
/// return {}`); malformed JSON is a named refusal, because silently replacing a
/// corrupt registry would throw away what a human could still fix.
pub fn read_or_default<T: serde::de::DeserializeOwned + Default>(
    path: &Path,
) -> Result<T, DunkError> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_err(|error| {
            DunkError::Refused(format!("{} is not valid JSON: {error}", path.display()))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(error) => Err(DunkError::Refused(format!(
            "read {}: {error}",
            path.display()
        ))),
    }
}

/// Write JSON atomically: a temp file in the same directory, fsync, rename, 0600.
///
/// The temp file must be in the same directory as the target: `rename` is only
/// atomic within a filesystem, and `/tmp` is not the config directory.
pub fn write<T: serde::Serialize>(path: &Path, value: &T) -> Result<(), DunkError> {
    write_classified(path, value).map_err(ClassifiedWriteError::into_dunk_error)
}

pub(crate) fn write_classified<T: serde::Serialize>(
    path: &Path,
    value: &T,
) -> Result<(), ClassifiedWriteError> {
    // State files are written with sorted keys (the Python's `sort_keys=True`). serde_json now
    // keeps insertion order (`preserve_order`), so sort explicitly instead of leaning on a
    // BTreeMap: the file bytes and the order a reader sees stay what they were.
    let json =
        serde_json::to_value(value).map_err(|error| ClassifiedWriteError::Other(error.into()))?;
    let text = serde_json::to_string_pretty(&sorted_keys(json))
        .map_err(|error| ClassifiedWriteError::Other(error.into()))?
        + "\n";
    write_text_atomically(path, &text)
}

/// The whole-Flock state file (`dunks.json`), written the way the Python daemon writes it:
/// `json.dump(payload, handle, indent=2)`. Key order is insertion order (no sorting), non-ASCII
/// is escaped, floats use Python's `repr`, and there is no trailing newline, so a file written by
/// either implementation reads back byte-identical from the other.
pub(crate) fn write_python_state_classified(
    path: &Path,
    value: &serde_json::Value,
) -> Result<(), ClassifiedWriteError> {
    write_text_atomically(path, &python_json_indent2(value))
}

/// Temp file in the same directory, created 0600, fsynced, then renamed over `path`.
fn write_text_atomically(path: &Path, text: &str) -> Result<(), ClassifiedWriteError> {
    let directory = path.parent().ok_or_else(|| {
        ClassifiedWriteError::Other(DunkError::Refused(format!(
            "{} has no parent directory",
            path.display()
        )))
    })?;
    fs::create_dir_all(directory).map_err(|error| ClassifiedWriteError::Io(error.into()))?;
    let temporary = temporary_path(path);
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600) // created 0600, before any content
            .open(&temporary)
            .map_err(|error| {
                ClassifiedWriteError::Io(DunkError::Refused(format!(
                    "create {}: {error}",
                    temporary.display()
                )))
            })?;
        file.write_all(text.as_bytes())
            .map_err(|error| ClassifiedWriteError::Io(error.into()))?;
        file.sync_all()
            .map_err(|error| ClassifiedWriteError::Io(error.into()))?; // data before rename
    }
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .map_err(|error| ClassifiedWriteError::Io(error.into()))?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        ClassifiedWriteError::Io(DunkError::Refused(format!(
            "rename {}: {error}",
            path.display()
        )))
    })?;
    Ok(())
}

/// Every object's keys in sorted order, at every depth.
pub fn sorted_keys(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(String, Value)> = map.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, sorted_keys(v)))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.into_iter().map(sorted_keys).collect()),
        other => other,
    }
}

/// Python's `json.dumps(value, indent=2)`, the text `json.dump(payload, handle, indent=2)`
/// writes: two-space indent, `,` then a newline between items, `": "` after keys, `ensure_ascii`
/// escapes, Python float `repr`, and empty containers as `[]` and `{}`. No trailing newline.
pub fn python_json_indent2(value: &serde_json::Value) -> String {
    let mut out = String::new();
    write_python_value(&mut out, value, 0);
    out
}

fn write_python_value(out: &mut String, value: &serde_json::Value, depth: usize) {
    use serde_json::Value;
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(flag) => out.push_str(if *flag { "true" } else { "false" }),
        Value::Number(number) => out.push_str(&python_number(number)),
        Value::String(text) => out.push_str(&python_string(text)),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                push_item_break(out, index, depth + 1);
                write_python_value(out, item, depth + 1);
            }
            push_close(out, depth, ']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (index, (key, item)) in map.iter().enumerate() {
                push_item_break(out, index, depth + 1);
                out.push_str(&python_string(key));
                out.push_str(": ");
                write_python_value(out, item, depth + 1);
            }
            push_close(out, depth, '}');
        }
    }
}

fn push_item_break(out: &mut String, index: usize, depth: usize) {
    if index > 0 {
        out.push(',');
    }
    out.push('\n');
    push_indent(out, depth);
}

fn push_close(out: &mut String, depth: usize, close: char) {
    out.push('\n');
    push_indent(out, depth);
    out.push(close);
}

fn push_indent(out: &mut String, depth: usize) {
    out.extend(std::iter::repeat_n(' ', depth * 2));
}

/// A JSON number as Python reads and prints it. A token with a fraction or an exponent is a
/// Python float, so it is re-rendered with `repr`; an integer token is written as it is.
fn python_number(number: &serde_json::Number) -> String {
    let token = number.to_string();
    if token.contains(['.', 'e', 'E']) {
        match token.parse::<f64>() {
            Ok(value) => python_float_repr(value),
            Err(_) => token,
        }
    } else {
        token
    }
}

/// Python's `repr(float)`: the shortest digits that round-trip, in fixed notation when the
/// decimal exponent is in [-4, 16), otherwise `d[.ddd]e±XX` with at least two exponent digits.
pub fn python_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 { "Infinity" } else { "-Infinity" }.into();
    }
    if value == 0.0 {
        return if value.is_sign_negative() {
            "-0.0"
        } else {
            "0.0"
        }
        .into();
    }
    // Rust's `{:e}` prints the same shortest round-trip digits that Python's repr uses.
    let scientific = format!("{value:e}");
    let (mantissa, exponent) = scientific
        .split_once('e')
        .unwrap_or((scientific.as_str(), "0"));
    let exponent: i32 = exponent.parse().unwrap_or(0);
    let (sign, unsigned) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa),
    };
    let digits: String = unsigned.chars().filter(|c| *c != '.').collect();
    let mut out = String::from(sign);
    if (-4..16).contains(&exponent) {
        let point = exponent + 1;
        if point <= 0 {
            out.push_str("0.");
            out.extend(std::iter::repeat_n('0', point.unsigned_abs() as usize));
            out.push_str(&digits);
        } else if point as usize >= digits.len() {
            out.push_str(&digits);
            out.extend(std::iter::repeat_n('0', point as usize - digits.len()));
            out.push_str(".0");
        } else {
            let (whole, fraction) = digits.split_at(point as usize);
            out.push_str(whole);
            out.push('.');
            out.push_str(fraction);
        }
    } else {
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exponent < 0 { '-' } else { '+' });
        let magnitude = exponent.unsigned_abs();
        if magnitude < 10 {
            out.push('0');
        }
        out.push_str(&magnitude.to_string());
    }
    out
}

/// A Python `json` string literal with `ensure_ascii`: `"`, `\` and the control characters use
/// their short escapes or `\u00XX`; every other character outside printable ASCII becomes
/// `\uXXXX` in lowercase hex, as a UTF-16 surrogate pair above U+FFFF.
fn python_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='~' => out.push(ch),
            _ => {
                let mut units = [0u16; 2];
                for unit in ch.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
    out
}

fn temporary_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    path.with_file_name(format!(".{name}.{pid}.{nanos}.tmp"))
}

/// The mode a written file must carry: 0600, checked by the acceptance test.
pub fn mode(path: &Path) -> Result<u32, DunkError> {
    Ok(fs::metadata(path)?.permissions().mode() & 0o777)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn a_written_file_is_0600_and_round_trips() {
        let dir = tempdir();
        let path = dir.path().join("roles.json");
        write(&path, &json!({"default": "anonymous", "lanes": {}})).unwrap();
        assert_eq!(
            0o600,
            mode(&path).unwrap(),
            "credentials file must not be group/world readable"
        );
        let back: serde_json::Value = read_or_default(&path).unwrap();
        assert_eq!(json!({"default": "anonymous", "lanes": {}}), back);
    }

    #[test]
    fn a_missing_file_reads_as_the_default() {
        let dir = tempdir();
        let back: serde_json::Value = read_or_default(&dir.path().join("nope.json")).unwrap();
        assert_eq!(serde_json::Value::Null, back);
    }

    #[test]
    fn malformed_json_is_a_named_refusal_not_a_silent_default() {
        let dir = tempdir();
        let path = dir.path().join("broken.json");
        fs::write(&path, "{not json").unwrap();
        let error = read_or_default::<serde_json::Value>(&path).unwrap_err();
        assert!(error.to_string().contains("not valid JSON"), "{error}");
        assert_eq!(2, error.exit_code());
    }

    #[test]
    fn a_reader_never_sees_a_partial_file() {
        // The temp file is in the same directory (atomic rename), so a concurrent
        // reader either sees the whole old file or the whole new one.
        let dir = tempdir();
        let path = dir.path().join("dunks.json");
        write(&path, &json!({"n": 1})).unwrap();
        let reader_path = path.clone();
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let reader = std::thread::spawn(move || {
            let mut seen = 0;
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                let value: serde_json::Value =
                    serde_json::from_str(&fs::read_to_string(&reader_path).unwrap()).unwrap();
                assert!(
                    value["n"].is_number(),
                    "a reader saw a partial file: {value}"
                );
                seen += 1;
            }
            seen
        });
        for n in 2..40 {
            write(&path, &json!({"n": n})).unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(reader.join().unwrap() > 0);
    }

    #[test]
    fn no_temp_files_are_left_behind() {
        let dir = tempdir();
        let path = dir.path().join("state.json");
        write(&path, &json!({"ok": true})).unwrap();
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?}");
    }

    // Expected values were produced by CPython 3 with `json.dumps(v, indent=2)` and `repr`.
    #[test]
    fn python_floats_print_like_python_repr() {
        let cases: &[(f64, &str)] = &[
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (1.0, "1.0"),
            (15.0, "15.0"),
            (2.5, "2.5"),
            (0.001, "0.001"),
            (0.0001, "0.0001"),
            (0.00001, "1e-05"),
            (1e-7, "1e-07"),
            (1e15, "1000000000000000.0"),
            (1e16, "1e+16"),
            (1.5e16, "1.5e+16"),
            (1.5e300, "1.5e+300"),
            (-2.5e-5, "-2.5e-05"),
            (1_793_000_000.5, "1793000000.5"),
            (1_793_000_000.123_456_7, "1793000000.1234567"),
            (4_102_444_800.25, "4102444800.25"),
            (1.2345678901234568e17, "1.2345678901234568e+17"),
            (0.1, "0.1"),
            (1.0 / 3.0, "0.3333333333333333"),
            (5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
        ];
        for (value, expected) in cases {
            assert_eq!(*expected, python_float_repr(*value), "repr of {value:e}");
        }
    }

    #[test]
    fn python_strings_escape_like_ensure_ascii() {
        assert_eq!(
            r#""eé—😀\u0001\u007f\t\"\\/""#,
            python_string("e\u{e9}\u{2014}\u{1F600}\u{1}\u{7f}\t\"\\/")
        );
    }

    #[test]
    fn indent_two_layout_matches_json_dump_without_a_trailing_newline() {
        let value = json!({"a": [], "b": {}, "c": [1, {"x": null}]});
        assert_eq!(
            "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    {\n      \"x\": null\n    }\n  ]\n}",
            python_json_indent2(&value)
        );
    }

    #[test]
    fn state_file_keeps_insertion_order_and_python_bytes() {
        let dir = tempdir();
        let path = dir.path().join("dunks.json");
        let value = json!({
            "version": 1,
            "next_id": 7,
            "dunkers": [{"id": "d1", "text": "tab\there", "interval_minutes": 15.0}],
            "decided": {"m30": {"status": "delivered"}, "m4": {"status": "failed"}},
        });
        write_python_state_classified(&path, &value).unwrap();
        let bytes = fs::read(&path).unwrap();
        assert_eq!(python_json_indent2(&value).as_bytes(), bytes.as_slice());
        assert!(
            !bytes.ends_with(b"\n"),
            "json.dump writes no trailing newline"
        );
        assert_eq!(0o600, mode(&path).unwrap());
        let text = String::from_utf8(bytes).unwrap();
        assert!(
            text.find("\"version\"") < text.find("\"dunkers\""),
            "{text}"
        );
        assert!(
            text.find("\"dunkers\"") < text.find("\"decided\""),
            "{text}"
        );
        assert!(
            text.find("\"m30\"") < text.find("\"m4\""),
            "not sorted: {text}"
        );
        assert!(text.contains("\"tab\\there\""), "{text}");
        assert!(text.contains("\"interval_minutes\": 15.0"), "{text}");
    }
}
