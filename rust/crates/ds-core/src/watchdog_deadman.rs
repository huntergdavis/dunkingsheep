//! The cadence and state-file readers from `dunk_watchdog_deadman.py`.
//!
//! These helpers are independent of the watchdog runner and are tested with temporary files.

use serde_json::{json, Value};
use std::fs;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};
use std::str::Utf8Error;
use std::time::UNIX_EPOCH;

use crate::error::DunkError;

/// The recorded cadence may widen the unit cadence by at most six times.
pub const CLAMP_FACTOR: f64 = 6.0;

/// Return the daemon-path configuration problem, or `None` when the socket path is usable.
pub fn daemon_path_problem(
    daemon_path: Option<&str>,
    expected_dir: Option<&Path>,
) -> Option<String> {
    let Some(daemon_path) = daemon_path.filter(|path| !path.trim().is_empty()) else {
        return Some("daemon path is empty".into());
    };
    let parent = absolute_path(Path::new(daemon_path))
        .parent()
        .unwrap_or_else(|| Path::new("/"))
        .to_path_buf();
    if !parent.is_dir() {
        return Some(format!(
            "daemon path parent {} does not exist or is not a directory",
            parent.display()
        ));
    }
    let expected_dir_display = expected_dir
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "None".to_string());
    let expected_dir = expected_dir.filter(|path| !path.as_os_str().is_empty());
    if expected_dir.and_then(|expected| fs::canonicalize(expected).ok())
        != fs::canonicalize(&parent).ok()
    {
        return Some(format!(
            "daemon path parent {} is not the expected directory {}",
            parent.display(),
            expected_dir_display
        ));
    }
    None
}

/// Classify transport errors without treating ambiguous failures as proof the daemon is down.
/// `error_filename` carries the optional filename exposed by Python's `OSError.filename`.
pub fn classify_daemon_error(
    error: &io::Error,
    error_filename: Option<&Path>,
    daemon_path: Option<&str>,
    expected_dir: Option<&Path>,
) -> &'static str {
    match error.kind() {
        io::ErrorKind::ConnectionRefused => "daemon_down",
        io::ErrorKind::NotFound | io::ErrorKind::NotADirectory => {
            if daemon_path_problem(daemon_path, expected_dir).is_some() {
                return "config_bad";
            }
            if let (Some(named), Some(daemon_path)) = (error_filename, daemon_path) {
                if absolute_path(named) != absolute_path(Path::new(daemon_path)) {
                    return "config_bad";
                }
            }
            if error.kind() == io::ErrorKind::NotADirectory {
                "config_bad"
            } else {
                "daemon_down"
            }
        }
        _ => "other",
    }
}

/// Seconds since the heartbeat timestamp, or `None` when it does not exist.
pub fn heartbeat_age(path: impl AsRef<Path>, now: f64) -> io::Result<Option<f64>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let modified = metadata.modified()?;
    let modified_s = match modified.duration_since(UNIX_EPOCH) {
        Ok(duration) => duration.as_secs_f64(),
        Err(error) => -error.duration().as_secs_f64(),
    };
    Ok(Some(now - modified_s))
}

fn absolute_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("/"))
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                normalized.pop();
            }
            _ => normalized.push(component.as_os_str()),
        }
    }
    normalized
}

/// `(cadence_minutes, problem)` from the heartbeat body.
///
/// An absent or legacy empty heartbeat has no recorded cadence and no problem. Invalid data
/// uses the unit cadence and returns a problem for the caller to report.
pub fn recorded_cadence(path: impl AsRef<Path>) -> (Option<Value>, Option<String>) {
    let path = path.as_ref();
    let bytes = match read_text_prefix(path, 4096) {
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
    let text = match std::str::from_utf8(&bytes) {
        Ok(text) => text.to_string(),
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

/// `_save`: write sorted Python JSON to the PID-specific sibling, then replace the state.
/// Failures leave the old destination (and any written temporary file) as Python does.
pub fn save(path: impl AsRef<Path>, data: &Value) -> io::Result<()> {
    let path = path.as_ref();
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)?;
    let mut temporary = path.as_os_str().to_os_string();
    temporary.push(format!(".tmp.{}", std::process::id()));
    // dumps_sorted supplies Python separators, float spelling, recursive sorting and
    // Unicode escaping; Python also escapes DEL, which that shared helper leaves raw.
    let text = crate::usage::dumps_sorted(data).replace('\u{7f}', "\\u007f");
    fs::write(Path::new(&temporary), text)?;
    fs::rename(Path::new(&temporary), path)
}

/// `_read_error`: decode the whole file, strip Python whitespace, then take 2000 characters.
/// Only a missing file or a directory is absence; other read/decode errors propagate.
pub fn read_error(path: impl AsRef<Path>) -> io::Result<Option<String>> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::IsADirectory
            ) =>
        {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    // Python text files use universal newlines, and str.strip additionally includes
    // the four ASCII information separators that Rust's is_whitespace excludes.
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let stripped =
        text.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c));
    Ok((!stripped.is_empty()).then(|| stripped.chars().take(2000).collect()))
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
        Value::String(text) => python_string_repr(text),
        other => other.to_string(),
    }
}

fn python_string_repr(text: &str) -> String {
    use std::fmt::Write as _;

    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut repr = String::with_capacity(text.len() + 2);
    repr.push(quote);
    for character in text.chars() {
        match character {
            '\\' => repr.push_str("\\\\"),
            '\n' => repr.push_str("\\n"),
            '\r' => repr.push_str("\\r"),
            '\t' => repr.push_str("\\t"),
            character if character == quote => {
                repr.push('\\');
                repr.push(character);
            }
            character if character.is_control() => {
                let codepoint = character as u32;
                if codepoint <= 0xff {
                    write!(repr, "\\x{codepoint:02x}").expect("writing to String");
                } else if codepoint <= 0xffff {
                    write!(repr, "\\u{codepoint:04x}").expect("writing to String");
                } else {
                    write!(repr, "\\U{codepoint:08x}").expect("writing to String");
                }
            }
            character => repr.push(character),
        }
    }
    repr.push(quote);
    repr
}

/// Read at most `max_chars` UTF-8 characters, like Python's text `read(size)`, without
/// allocating in proportion to the entire heartbeat file. `BufReader` may read ahead by its
/// fixed buffer size, but the retained text is capped at four bytes per requested character.
fn read_text_prefix(path: &Path, max_chars: usize) -> io::Result<Vec<u8>> {
    read_text_prefix_from(fs::File::open(path)?, max_chars)
}

fn read_text_prefix_from(reader: impl Read, max_chars: usize) -> io::Result<Vec<u8>> {
    let mut reader = BufReader::new(reader);
    let mut bytes = Vec::with_capacity(max_chars.min(4096).saturating_mul(4));
    for _ in 0..max_chars {
        let start = bytes.len();
        let mut lead = [0u8; 1];
        if reader.read(&mut lead)? == 0 {
            break;
        }
        bytes.push(lead[0]);
        let width = match lead[0] {
            0x00..=0x7f => 1,
            0xc2..=0xdf => 2,
            0xe0..=0xef => 3,
            0xf0..=0xf4 => 4,
            _ => return Ok(bytes),
        };
        for _ in 1..width {
            let mut continuation = [0u8; 1];
            if reader.read(&mut continuation)? == 0 {
                return Ok(bytes);
            }
            bytes.push(continuation[0]);
        }
        if std::str::from_utf8(&bytes[start..]).is_err() {
            return Ok(bytes);
        }
    }
    Ok(bytes)
}

fn io_error_type(error: &io::Error) -> &'static str {
    match error.kind() {
        io::ErrorKind::NotFound => "FileNotFoundError",
        io::ErrorKind::NotADirectory => "NotADirectoryError",
        io::ErrorKind::PermissionDenied => "PermissionError",
        io::ErrorKind::IsADirectory => "IsADirectoryError",
        _ => "OSError",
    }
}

fn python_io_error(path: &Path, error: &io::Error) -> String {
    let code = error.raw_os_error().unwrap_or(5);
    let description = match error.kind() {
        io::ErrorKind::NotFound => "No such file or directory",
        io::ErrorKind::NotADirectory => "Not a directory",
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
    } else if (0x80..=0xc1).contains(&byte) || byte >= 0xf5 {
        "invalid start byte"
    } else {
        "invalid continuation byte"
    };
    format!("'utf-8' codec can't decode byte 0x{byte:02x} in position {position}: {reason}")
}

#[cfg(test)]
mod tests {
    use super::{
        classify_daemon_error, daemon_path_problem, effective_cadence, heartbeat_age, load,
        read_error, read_text_prefix_from, recorded_cadence, save,
    };
    use serde_json::{json, Value};
    use std::fs;
    use std::io::{self, Cursor, Read};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct CountingReader {
        inner: Cursor<Vec<u8>>,
        bytes_read: Arc<AtomicUsize>,
    }

    impl Read for CountingReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            let read = self.inner.read(buffer)?;
            self.bytes_read.fetch_add(read, Ordering::Relaxed);
            Ok(read)
        }
    }

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
    fn watchdog_deadman_recorded_cadence_formats_not_a_directory_like_python() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("not-a-directory");
        fs::write(&file, "file").unwrap();
        let path = file.join("heartbeat.json");
        let error = fs::read(&path).unwrap_err();
        let errno = error.raw_os_error().unwrap_or(5);

        assert_eq!(
            (
                None,
                Some(format!(
                    "unreadable (NotADirectoryError: [Errno {errno}] Not a directory: '{}')",
                    path.display()
                ))
            ),
            recorded_cadence(path)
        );
    }

    #[test]
    fn watchdog_deadman_recorded_cadence_formats_c0_as_invalid_start_byte_like_python() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("state/heartbeat.json");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, [0xc0, 0xaf]).unwrap();

        assert_eq!(
            (
                None,
                Some("unreadable (UnicodeDecodeError: 'utf-8' codec can't decode byte 0xc0 in position 0: invalid start byte)".into())
            ),
            recorded_cadence(path)
        );
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
    fn watchdog_deadman_recorded_cadence_uses_python_repr_for_quotes_and_newlines() {
        let directory = tempfile::tempdir().unwrap();
        let path = heartbeat(&directory, r#"{"cadence_minutes":"bad'line\nsecond"}"#);
        let (cadence, problem) = recorded_cadence(path);
        assert_eq!(None, cadence);
        assert_eq!(
            Some(r#"no positive cadence_minutes (got "bad'line\nsecond")"#.into()),
            problem
        );
    }

    #[test]
    fn watchdog_deadman_recorded_cadence_reads_only_a_bounded_character_prefix() {
        let bytes_read = Arc::new(AtomicUsize::new(0));
        let mut input = "🧵".repeat(4096).into_bytes();
        input.extend(vec![b' '; 2_000_000]);
        let reader = CountingReader {
            inner: Cursor::new(input),
            bytes_read: Arc::clone(&bytes_read),
        };

        let prefix = read_text_prefix_from(reader, 4096).unwrap();

        assert_eq!(16_384, prefix.len());
        assert_eq!(4096, std::str::from_utf8(&prefix).unwrap().chars().count());
        assert!(bytes_read.load(Ordering::Relaxed) <= 16_384);
    }

    #[test]
    fn watchdog_deadman_daemon_path_problem_classifies_empty_missing_and_unexpected_parents() {
        let directory = tempfile::tempdir().unwrap();
        assert_eq!(
            Some("daemon path is empty".into()),
            daemon_path_problem(Some("  "), Some(directory.path()))
        );
        assert_eq!(
            Some(format!(
                "daemon path parent {} does not exist or is not a directory",
                directory.path().join("gone").display()
            )),
            daemon_path_problem(
                Some(directory.path().join("gone/socket").to_str().unwrap()),
                Some(directory.path())
            )
        );
        let other = directory.path().join("other");
        fs::create_dir(&other).unwrap();
        assert_eq!(
            Some(format!(
                "daemon path parent {} is not the expected directory {}",
                other.display(),
                directory.path().display()
            )),
            daemon_path_problem(
                Some(other.join("socket").to_str().unwrap()),
                Some(directory.path())
            )
        );
        assert_eq!(
            None,
            daemon_path_problem(
                Some(directory.path().join("socket").to_str().unwrap()),
                Some(directory.path())
            )
        );
    }

    #[test]
    fn watchdog_deadman_daemon_path_problem_renders_missing_expected_dir_as_none() {
        let directory = tempfile::tempdir().unwrap();
        let daemon_path = directory.path().join("socket");
        assert_eq!(
            Some(format!(
                "daemon path parent {} is not the expected directory None",
                directory.path().display()
            )),
            daemon_path_problem(Some(daemon_path.to_str().unwrap()), None)
        );
    }

    #[test]
    fn watchdog_deadman_classifies_daemon_errors_without_directing_ambiguous_failures() {
        let directory = tempfile::tempdir().unwrap();
        let daemon_path = directory.path().join("socket");
        let daemon_text = daemon_path.to_str().unwrap();
        let expected = directory.path();
        let classify = |kind, filename: Option<&std::path::Path>| {
            classify_daemon_error(
                &io::Error::from(kind),
                filename,
                Some(daemon_text),
                Some(expected),
            )
        };

        assert_eq!(
            "daemon_down",
            classify(io::ErrorKind::ConnectionRefused, None)
        );
        assert_eq!("daemon_down", classify(io::ErrorKind::NotFound, None));
        assert_eq!(
            "config_bad",
            classify(
                io::ErrorKind::NotFound,
                Some(&directory.path().join("other-socket"))
            )
        );
        assert_eq!(
            "config_bad",
            classify(io::ErrorKind::NotADirectory, Some(&daemon_path))
        );
        assert_eq!("other", classify(io::ErrorKind::TimedOut, None));
    }

    #[test]
    fn watchdog_deadman_heartbeat_age_distinguishes_fresh_aged_and_missing_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("heartbeat");
        fs::write(&path, "").unwrap();
        let modified = fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        assert_eq!(Some(30.0), heartbeat_age(&path, modified + 30.0).unwrap());
        assert_eq!(
            Some(1801.0),
            heartbeat_age(&path, modified + 30.0 * 60.0 + 1.0).unwrap()
        );
        assert_eq!(
            None,
            heartbeat_age(directory.path().join("missing"), modified).unwrap()
        );
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

    fn python_oracle(script: &str, path: &std::path::Path) -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let output = std::process::Command::new("python3")
            .args(["-c", script])
            .arg(path)
            .current_dir(root)
            .output()
            .expect("Python oracle must execute");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn state_temporary(path: &std::path::Path) -> std::path::PathBuf {
        let mut name = path.as_os_str().to_os_string();
        name.push(format!(".tmp.{}", std::process::id()));
        name.into()
    }

    #[test]
    fn watchdog_deadman_save_matches_python_sorted_json_and_replaces_inode() {
        use std::io::Read as _;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/state.json");
        let data = json!({"z": ["é🧵\u{7f}", 1.0, 1e-7, 1e20, -0.0], "a": {"z": true, "a": null}});
        save(&path, &data).unwrap();
        let expected = python_oracle(
            "import json, sys, dunk_watchdog_deadman as d; p=sys.argv[1]; data=json.load(open(p)); d._save(p+'.oracle', data); print(open(p+'.oracle').read(), end='')",
            &path,
        );
        assert_eq!(expected, fs::read_to_string(&path).unwrap());
        assert!(expected.starts_with("{\"a\": {\"a\": null, \"z\": true}, \"z\": ["));
        assert!(expected.contains(r"\u00e9\ud83e\uddf5\u007f"));
        assert!(!state_temporary(&path).exists());
        let mut old_reader = fs::File::open(&path).unwrap();
        save(&path, &json!({"new": 3})).unwrap();
        let mut old_text = String::new();
        old_reader.read_to_string(&mut old_text).unwrap();
        assert_eq!(
            expected, old_text,
            "replacement must not truncate old inode"
        );
        assert_eq!("{\"new\": 3}", fs::read_to_string(&path).unwrap());
    }

    #[test]
    fn watchdog_deadman_save_propagates_parent_write_and_replace_failures() {
        let directory = tempfile::tempdir().unwrap();
        let parent_file = directory.path().join("file");
        fs::write(&parent_file, "old").unwrap();
        assert!(save(parent_file.join("state"), &json!({})).is_err());
        let state = directory.path().join("state");
        fs::write(&state, "old").unwrap();
        let temporary = state_temporary(&state);
        fs::create_dir(&temporary).unwrap();
        assert_eq!(
            io::ErrorKind::IsADirectory,
            save(&state, &json!({})).unwrap_err().kind()
        );
        assert_eq!("old", fs::read_to_string(&state).unwrap());
        fs::remove_dir(&temporary).unwrap();
        fs::remove_file(&state).unwrap();
        fs::create_dir(&state).unwrap();
        assert!(save(&state, &json!({"new": true})).is_err());
        assert!(state.is_dir());
        assert_eq!("{\"new\": true}", fs::read_to_string(&temporary).unwrap());
        let oracle = python_oracle(
            "import os, sys, dunk_watchdog_deadman as d; p=sys.argv[1];\ntry: d._save(p, {'new': True})\nexcept IsADirectoryError: print(open(f'{p}.tmp.{os.getpid()}').read(), end='')",
            &state,
        );
        assert_eq!(oracle, fs::read_to_string(temporary).unwrap());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let locked = directory.path().join("locked");
            fs::create_dir(&locked).unwrap();
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
            let denied = fs::write(locked.join("probe"), "probe").is_err();
            let result = save(locked.join("state"), &json!({}));
            fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
            // A privileged test runner may bypass mode bits; ordinary CI must deny this.
            if denied {
                assert_eq!(io::ErrorKind::PermissionDenied, result.unwrap_err().kind());
                assert!(!locked.join("state").exists());
            }
        }
    }

    #[test]
    fn watchdog_deadman_read_error_matches_python_raw_unicode_whitespace_and_limit() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("error");
        for raw in [
            "CONFIG: pagethreshold: unknown key\n".to_string(),
            "\u{1c}\u{1f}\u{a0}\t\r\n".to_string(),
            " \rfirst\r\nsecond\rthird\r ".to_string(),
            format!("{}{} trailing  ", " \n".repeat(2100), "🧵é".repeat(1100)),
            "\u{200b}content\u{200b}".to_string(),
            String::new(),
        ] {
            fs::write(&path, raw).unwrap();
            let expected: Option<String> = serde_json::from_str(&python_oracle(
                "import json, sys, dunk_watchdog_deadman as d; print(json.dumps(d._read_error(sys.argv[1])))",
                &path,
            )).unwrap();
            assert_eq!(expected, read_error(&path).unwrap());
        }
        fs::write(&path, "🧵".repeat(2001)).unwrap();
        assert_eq!(2000, read_error(&path).unwrap().unwrap().chars().count());
    }

    #[test]
    fn watchdog_deadman_read_error_only_swallows_missing_or_directory() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("error");
        assert_eq!(None, read_error(&path).unwrap());
        assert_eq!(None, read_error(directory.path()).unwrap());
        fs::write(&path, "file").unwrap();
        assert_eq!(
            io::ErrorKind::NotADirectory,
            read_error(path.join("child")).unwrap_err().kind()
        );
        let mut invalid = vec![b'a'; 3000];
        invalid.push(0xff);
        fs::write(&path, invalid).unwrap();
        assert_eq!(
            io::ErrorKind::InvalidData,
            read_error(&path).unwrap_err().kind()
        );
        assert_eq!("UnicodeDecodeError\n", python_oracle(
            "import sys, dunk_watchdog_deadman as d;\ntry: d._read_error(sys.argv[1])\nexcept UnicodeDecodeError: print('UnicodeDecodeError')",
            &path,
        ));
        #[cfg(unix)]
        {
            use std::os::unix::fs::{symlink, PermissionsExt};
            fs::set_permissions(&path, fs::Permissions::from_mode(0)).unwrap();
            // Root can still read mode-000 files; exercise the kernel denial when available.
            if let Err(error) = fs::read(&path) {
                assert_eq!(io::ErrorKind::PermissionDenied, error.kind());
                assert_eq!(
                    io::ErrorKind::PermissionDenied,
                    read_error(&path).unwrap_err().kind()
                );
            }
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let loop_path = directory.path().join("loop");
            symlink(&loop_path, &loop_path).unwrap();
            assert_eq!(
                fs::read(&loop_path).unwrap_err().raw_os_error(),
                read_error(&loop_path).unwrap_err().raw_os_error()
            );
        }
    }
}
