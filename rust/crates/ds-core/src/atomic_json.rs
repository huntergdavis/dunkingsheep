//! Atomic JSON writes with mode 0600 (T02).
//!
//! The Python writes 15 files this way: a temp file in the same directory, `os.replace`,
//! and mode 0600. The reason is in the tree's history - a half-written `roles.json` made
//! every CLI call fail (2026-10-28), and a world-readable `telegram.json` would leak the
//! bot token. Both properties are part of the contract, so both are tested.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::error::DunkError;

/// Write `value` as pretty JSON to `path`, atomically, mode 0600.
///
/// "Atomically" is `rename` within the same directory: a reader either sees the old file
/// or the new one, never a truncated one. The temp name carries the pid so two writers
/// cannot collide on it.
pub fn write_json(path: &Path, value: &serde_json::Value) -> Result<(), DunkError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent)?;
    let temp = temp_path(path);
    let text = serde_json::to_string_pretty(value)? + "\n";
    {
        let mut handle = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temp)?;
        handle.write_all(text.as_bytes())?;
        handle.sync_all()?;
    }
    fs::rename(&temp, path)?;
    Ok(())
}

/// Read JSON, mapping a missing file to `Ok(None)` and a malformed one to an error.
///
/// The distinction matters: the fleet's callers treat "no file yet" as "seed it" and
/// "unreadable file" as a refusal to start (the watchdog refuses rather than guessing).
pub fn read_json(path: &Path) -> Result<Option<serde_json::Value>, DunkError> {
    match fs::read_to_string(path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(DunkError::Store(format!(
            "cannot read {}: {error}",
            path.display()
        ))),
    }
}

/// The mode of a file, or `None` when it does not exist.
pub fn mode_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode() & 0o777)
}

fn temp_path(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.{}.tmp", std::process::id()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmpdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!("ds-core-atomic-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn a_written_file_is_mode_0600() {
        let dir = tmpdir();
        let path = dir.join("roles.json");
        write_json(&path, &json!({"lanes": {}})).unwrap();
        assert_eq!(
            Some(0o600),
            mode_of(&path),
            "a registry holds tokens; 0600 is the contract"
        );
    }

    #[test]
    fn a_reader_never_sees_a_partial_file() {
        // The property is the rename, not the temp file: after a write the target is
        // complete, and the temp file is gone.
        let dir = tmpdir();
        let path = dir.join("state.json");
        write_json(&path, &json!({"n": 1})).unwrap();
        write_json(&path, &json!({"n": 2})).unwrap();
        let value = read_json(&path).unwrap().unwrap();
        assert_eq!(json!({"n": 2}), value);
        assert!(
            !temp_path(&path).exists(),
            "the temp file must not survive the rename"
        );
    }

    #[test]
    fn a_missing_file_is_none_and_a_broken_one_is_an_error() {
        let dir = tmpdir();
        let missing = dir.join("absent.json");
        assert_eq!(None, read_json(&missing).unwrap());
        let broken = dir.join("broken.json");
        fs::write(&broken, "{not json").unwrap();
        assert!(
            read_json(&broken).is_err(),
            "unreadable is a refusal, not an empty file"
        );
    }
}
