//! The cadence and state-file readers from `dunk_watchdog_deadman.py`.
//!
//! These helpers are independent of the watchdog runner and are tested with temporary files.

use serde_json::{json, Value};
use std::fs;
use std::io;
use std::path::Path;
use std::str::Utf8Error;

use crate::error::DunkError;

/// The recorded cadence may widen the unit cadence by at most six times.
pub const CLAMP_FACTOR: f64 = 6.0;

/// `(cadence_minutes, problem)` from the heartbeat body.
///
/// An absent or legacy empty heartbeat has no recorded cadence and no problem. Invalid data
/// uses the unit cadence and returns a problem for the caller to report.
pub fn recorded_cadence(path: impl AsRef<Path>) -> (Option<Value>, Option<String>) {
    let path = path.as_ref();
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return (None, None),
        Err(error) => {
            return (
                None,
                Some(format!(
                    "unreadable ({}: {})",
                    io_error_type(&error),
                    python_io_error(path, &error)
                )),
            )
        }
    };
    let decoded = match std::str::from_utf8(&bytes) {
        Ok(text) => text,
        Err(error) => {
            return (
                None,
                Some(format!(
                    "unreadable (UnicodeDecodeError: {})",
                    unicode_decode_error(&bytes, error)
                )),
            )
        }
    };
    let text = decoded.chars().take(4096).collect::<String>();
    if text.trim().is_empty() {
        return (None, None);
    }
    let data: Value = match serde_json::from_str(&text) {
        Ok(data) => data,
        Err(error) => return (None, Some(format!("not JSON ({error})"))),
    };
    let value = data.get("cadence_minutes");
    let valid = value
        .and_then(Value::as_f64)
        .is_some_and(|number| number > 0.0 && number.is_finite());
    if !valid {
        return (
            None,
            Some(format!(
                "no positive cadence_minutes (got {})",
                value.map(python_repr).unwrap_or_else(|| "None".into())
            )),
        );
    }
    (value.cloned(), None)
}

/// `max(unit, recorded)`, clamped to `CLAMP_FACTOR * unit`.
pub fn effective_cadence(unit: &Value, recorded: Option<&Value>) -> (Value, bool) {
    let Some(recorded) = recorded else {
        return (unit.clone(), false);
    };
    let unit_number = unit.as_f64().unwrap_or(0.0);
    let recorded_number = recorded.as_f64().unwrap_or(0.0);
    if recorded_number > CLAMP_FACTOR * unit_number {
        (scale_unit(unit), true)
    } else if recorded_number > unit_number {
        (recorded.clone(), false)
    } else {
        (unit.clone(), false)
    }
}

/// `_load`: missing, invalid UTF-8, or invalid JSON state is an empty object; other I/O errors
/// remain errors just as `open()` errors other than `FileNotFoundError` do in Python.
pub fn load(path: impl AsRef<Path>) -> Result<Value, DunkError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::InvalidData
            ) =>
        {
            return Ok(json!({}));
        }
        Err(error) => return Err(error.into()),
    };
    Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({})))
}

fn scale_unit(unit: &Value) -> Value {
    match unit {
        Value::Number(number) if number.is_i64() => {
            number.as_i64().and_then(|n| n.checked_mul(6)).map_or_else(
                || json!(unit.as_f64().unwrap_or(0.0) * CLAMP_FACTOR),
                |n| json!(n),
            )
        }
        Value::Number(number) if number.is_u64() => {
            number.as_u64().and_then(|n| n.checked_mul(6)).map_or_else(
                || json!(unit.as_f64().unwrap_or(0.0) * CLAMP_FACTOR),
                |n| json!(n),
            )
        }
        _ => json!(unit.as_f64().unwrap_or(0.0) * CLAMP_FACTOR),
    }
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(text) => format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'")),
        other => other.to_string(),
    }
}

fn io_error_type(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::NotFound => "FileNotFoundError",
        io::ErrorKind::PermissionDenied => "PermissionError",
        io::ErrorKind::IsADirectory => "IsADirectoryError",
        _ => "OSError",
    }
}

fn python_io_error(path: &Path, error: &io::Error) -> String {
    let code = error.raw_os_error().unwrap_or(5);
    let description = match error.kind() {
        io::ErrorKind::NotFound => "No such file or directory",
        io::ErrorKind::PermissionDenied => "Permission denied",
        io::ErrorKind::IsADirectory => "Is a directory",
        _ => return error.to_string(),
    };
    format!("[Errno {code}] {description}: '{}'", path.display())
}

fn unicode_decode_error(bytes: &[u8], error: Utf8Error) -> String {
    let position = error.valid_up_to();
    let byte = bytes.get(position).copied().unwrap_or_default();
    let reason = if error.error_len().is_none() {
        "unexpected end of data"
    } else if byte & 0xc0 == 0x80 || byte >= 0xf5 {
        "invalid start byte"
    } else {
        "invalid continuation byte"
    };
    format!("'utf-8' codec can't decode byte 0x{byte:02x} in position {position}: {reason}")
}

#[cfg(test)]
mod tests {
    use super::{effective_cadence, load, recorded_cadence};
    use serde_json::{json, Value};
    use std::fs;

    fn heartbeat(directory: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = directory.path().join("state/heartbeat.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn watchdog_deadman_recorded_cadence_accepts_present_finite_positive_number() {
        let directory = tempfile::tempdir().unwrap();
        let path = heartbeat(&directory, r#"{"schema":1,"cadence_minutes":15}"#);
        assert_eq!((Some(json!(15)), None), recorded_cadence(path));
    }

    #[test]
    fn watchdog_deadman_recorded_cadence_treats_missing_and_legacy_empty_files_as_unrecorded() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("missing");
        assert_eq!((None, None), recorded_cadence(missing));
        let empty = heartbeat(&directory, " \n");
        assert_eq!((None, None), recorded_cadence(empty));
    }

    #[test]
    fn watchdog_deadman_recorded_cadence_rejects_non_objects_and_invalid_values() {
        let directory = tempfile::tempdir().unwrap();
        let state_dir = directory.path().join("state");
        fs::create_dir_all(&state_dir).unwrap();
        for (index, body) in [
            "garbage",
            "[1]",
            r#"{"cadence_minutes":0}"#,
            r#"{"cadence_minutes":-1}"#,
            r#"{"cadence_minutes":true}"#,
            r#"{"cadence_minutes":"15"}"#,
        ]
        .into_iter()
        .enumerate()
        {
            let path = state_dir.join(format!("heartbeat-{index}.json"));
            fs::write(&path, body).unwrap();
            let (value, problem) = recorded_cadence(path);
            assert!(value.is_none(), "{body}");
            assert!(problem.is_some(), "{body}");
        }
    }

    #[test]
    fn watchdog_deadman_effective_cadence_widens_without_narrowing_and_clamps_at_six_times_unit() {
        let unit = json!(10);
        assert_eq!((json!(10), false), effective_cadence(&unit, None));
        assert_eq!(
            (json!(15), false),
            effective_cadence(&unit, Some(&json!(15)))
        );
        assert_eq!(
            (json!(10), false),
            effective_cadence(&unit, Some(&json!(5)))
        );
        assert_eq!(
            (json!(60), true),
            effective_cadence(&unit, Some(&json!(1440)))
        );
    }

    #[test]
    fn watchdog_deadman_load_returns_empty_object_for_missing_invalid_json_and_invalid_utf8_state()
    {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state/deadman.json");
        assert_eq!(json!({}), load(&path).unwrap());
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "not json").unwrap();
        assert_eq!(json!({}), load(&path).unwrap());
        fs::write(&path, [0xffu8]).unwrap();
        assert_eq!(json!({}), load(path).unwrap());
    }

    #[test]
    fn watchdog_deadman_load_preserves_valid_json_value() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::write(&path, r#"{"last_alerted":123}"#).unwrap();
        assert_eq!(json!({"last_alerted":123}), load(path).unwrap());
        fs::write(directory.path().join("array.json"), "[1,2]").unwrap();
        assert_eq!(
            json!([1, 2]),
            load(directory.path().join("array.json")).unwrap()
        );
    }

    #[test]
    fn watchdog_deadman_recorded_cadence_truncates_heartbeat_content_to_4096_characters() {
        let directory = tempfile::tempdir().unwrap();
        let body = format!("{}{}", " ".repeat(4095), r#"{"cadence_minutes":15}"#);
        let path = heartbeat(&directory, &body);
        let (cadence, issue) = recorded_cadence(path);
        assert_eq!(None, cadence);
        assert!(issue.is_some());
    }

    #[test]
    fn watchdog_deadman_load_preserves_non_object_json_like_python() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state.json");
        fs::write(&path, "true").unwrap();
        let loaded: Value = load(path).unwrap();
        assert_eq!(json!(true), loaded);
    }
}
