//! An exclusive lock a second task cannot take (T02), from `dunk_superusers._locked`.
//!
//! The Python holds an `fcntl.flock` on a `<file>.lock` beside the data it protects, so a
//! second writer blocks instead of interleaving. The port uses `create_new` on the lock
//! file - a boring, dependency-free exclusion that fails closed - and NAMES its limit:
//! unlike `flock`, a crashed holder leaves the file behind, so the caller must be able to
//! recognise a stale lock. `Lock::is_stale` is that check, and the timeout is the caller's.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::DunkError;

/// A held lock file. Dropping it releases the lock.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    /// Take the lock for `target`, waiting up to `timeout`.
    ///
    /// The path is `<target>.lock`, the same name the Python uses, so a mixed fleet does
    /// not create two different locks for one file.
    pub fn acquire(target: &Path, timeout: Duration) -> Result<Lock, DunkError> {
        let path = lock_path(target);
        let deadline = SystemTime::now() + timeout;
        loop {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut handle) => {
                    let _ = writeln!(handle, "{}", std::process::id());
                    return Ok(Lock { path });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if SystemTime::now() >= deadline {
                        return Err(DunkError::Refused(format!(
                            "lock busy: {} is held by another writer",
                            path.display()
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    return Err(DunkError::Store(format!(
                        "cannot create {}: {error}",
                        path.display()
                    )))
                }
            }
        }
    }

    /// True when the lock file is older than `age` - a crashed holder, not a live one.
    ///
    /// The caller decides what to do; this only reports the observation, because a stale
    /// lock that is silently broken is how two writers end up in one file.
    pub fn is_stale(target: &Path, age: Duration) -> bool {
        let path = lock_path(target);
        match fs::metadata(&path).and_then(|meta| meta.modified()) {
            Ok(modified) => SystemTime::now()
                .duration_since(modified)
                .map(|elapsed| elapsed > age)
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// The lock file this guard holds.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// `<target>.lock`, the name the Python's `_locked` uses.
pub fn lock_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    target.with_file_name(format!("{name}.lock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("ds-core-lock-{tag}-{}", std::process::id()));
        fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn a_held_lock_blocks_a_second_taker() {
        let dir = tmpdir("held");
        let target = dir.join("roles.json");
        let held = Lock::acquire(&target, Duration::from_millis(10)).unwrap();
        let second = Lock::acquire(&target, Duration::from_millis(60));
        let error = second.expect_err("a second writer must not get the lock");
        assert_eq!(
            Some("lock busy"),
            error.reason().map(|r| r.split(':').next().unwrap_or(r))
        );
        assert!(error.to_string().contains("held by another writer"));
        drop(held);
        // Released on drop, so the next taker succeeds.
        Lock::acquire(&target, Duration::from_millis(10)).unwrap();
    }

    #[test]
    fn the_lock_file_is_the_name_the_python_uses() {
        assert_eq!(
            PathBuf::from("/x/roles.json.lock"),
            lock_path(Path::new("/x/roles.json"))
        );
    }

    #[test]
    fn a_fresh_lock_is_not_stale_and_a_missing_one_is_not_either() {
        let dir = tmpdir("stale");
        let target = dir.join("state.json");
        let _held = Lock::acquire(&target, Duration::from_millis(10)).unwrap();
        assert!(!Lock::is_stale(&target, Duration::from_secs(60)));
        assert!(!Lock::is_stale(
            &dir.join("never-locked.json"),
            Duration::from_secs(60)
        ));
    }
}
