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
    let directory = path
        .parent()
        .ok_or_else(|| DunkError::Refused(format!("{} has no parent directory", path.display())))?;
    fs::create_dir_all(directory)?;
    let text = serde_json::to_string_pretty(value)? + "\n";
    let temporary = temporary_path(path);
    {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600) // created 0600, before any content
            .open(&temporary)
            .map_err(|error| {
                DunkError::Refused(format!("create {}: {error}", temporary.display()))
            })?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?; // the data is on disk before the rename
    }
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(&temporary, path).map_err(|error| {
        let _ = fs::remove_file(&temporary);
        DunkError::Refused(format!("rename {}: {error}", path.display()))
    })?;
    Ok(())
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
}
