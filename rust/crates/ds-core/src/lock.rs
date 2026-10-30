//! File locks: the Python's `fcntl.flock`, with the same shared/exclusive rule.
//!
//! Ported from `dunk_superusers._locked` / `locked_allowlist` (a lock file next to
//! the allowlist). Two properties are load-bearing and tested: a held lock blocks
//! another owner, and dropping the guard releases it - a lock that outlives its
//! guard deadlocks the next writer, which is how a fleet lane stops making progress.

use std::fs::{File, OpenOptions};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::Path;

// `flock` (the free function) is deprecated in nix 0.29 in favour of the `Flock`
// type, but the type owns the File and drops the lock with it, which is the same
// guarantee this module's `Guard` provides. Using the function keeps the lock tied
// to the file descriptor `Guard` already owns, so the deprecation is accepted and
// silenced here rather than restructuring for a cosmetic warning.
#[allow(deprecated)]
use nix::fcntl::{flock, FlockArg};

use crate::error::DunkError;

/// Whether a lock is held for reading (shared) or writing (exclusive).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Shared,
    Exclusive,
}

/// An acquired lock; releasing happens on drop, on every path including `?`.
#[derive(Debug)]
pub struct Guard {
    file: File,
    mode: Mode,
}

impl Guard {
    /// Try once, without waiting: `false` means someone else holds it.
    ///
    /// This is the Python's `fcntl.flock(..., LOCK_EX | LOCK_NB)` + the
    /// `BlockingIOError` branch that raises "already running".
    #[allow(deprecated)] // see the note on the import: the free function keeps the lock on our fd
    pub fn try_acquire(path: &Path, mode: Mode) -> Result<Option<Guard>, DunkError> {
        let file = open(path)?;
        let arg = match mode {
            Mode::Shared => FlockArg::LockSharedNonblock,
            Mode::Exclusive => FlockArg::LockExclusiveNonblock,
        };
        match flock(file.as_raw_fd(), arg) {
            Ok(()) => Ok(Some(Guard { file, mode })),
            Err(nix::errno::Errno::EWOULDBLOCK) => Ok(None),
            Err(error) => Err(DunkError::Refused(format!(
                "lock {}: {error}",
                path.display()
            ))),
        }
    }

    /// Wait for the lock.
    #[allow(deprecated)]
    pub fn acquire(path: &Path, mode: Mode) -> Result<Guard, DunkError> {
        let file = open(path)?;
        let arg = match mode {
            Mode::Shared => FlockArg::LockShared,
            Mode::Exclusive => FlockArg::LockExclusive,
        };
        flock(file.as_raw_fd(), arg)
            .map_err(|error| DunkError::Refused(format!("lock {}: {error}", path.display())))?;
        Ok(Guard { file, mode })
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }
}

impl Drop for Guard {
    #[allow(deprecated)]
    fn drop(&mut self) {
        // A failed unlock is not actionable here, and panicking in Drop would
        // abort the process; the kernel releases the lock when the file closes.
        let _ = flock(self.file.as_raw_fd(), FlockArg::Unlock);
    }
}

fn open(path: &Path) -> Result<File, DunkError> {
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600) // a lock file is not a public artefact
        .open(path)
        .map_err(|error| DunkError::Refused(format!("open {}: {error}", path.display())))
}

/// The Python's `with _locked(path): ...` shape.
pub fn with_exclusive<T, F>(path: &Path, body: F) -> Result<T, DunkError>
where
    F: FnOnce() -> Result<T, DunkError>,
{
    let _guard = Guard::acquire(path, Mode::Exclusive)?;
    body()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    #[test]
    fn a_held_exclusive_lock_blocks_a_second_owner() {
        let dir = tempdir();
        let path = dir.path().join("superusers.lock");
        let first = Guard::try_acquire(&path, Mode::Exclusive).unwrap();
        assert!(first.is_some(), "the first owner gets the lock");
        assert!(
            Guard::try_acquire(&path, Mode::Exclusive)
                .unwrap()
                .is_none(),
            "a second exclusive owner must be refused while the first holds it"
        );
        drop(first);
        assert!(
            Guard::try_acquire(&path, Mode::Exclusive)
                .unwrap()
                .is_some(),
            "the lock is free once the guard is dropped"
        );
    }

    #[test]
    fn two_threads_never_hold_it_at_once() {
        let dir = tempdir();
        let path = dir.path().join("counter.lock");
        let inside = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..4 {
            let path = path.clone();
            let inside = inside.clone();
            let peak = peak.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..10 {
                    let _guard = Guard::acquire(&path, Mode::Exclusive).unwrap();
                    let now = inside.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    std::thread::sleep(Duration::from_micros(200));
                    inside.fetch_sub(1, Ordering::SeqCst);
                }
            }));
        }
        for handle in handles {
            handle.join().unwrap();
        }
        assert_eq!(
            1,
            peak.load(Ordering::SeqCst),
            "two owners held the lock at once"
        );
    }

    #[test]
    fn a_shared_lock_still_blocks_an_exclusive_one() {
        let dir = tempdir();
        let path = dir.path().join("roles.lock");
        let shared = Guard::try_acquire(&path, Mode::Shared).unwrap();
        assert!(shared.is_some());
        assert!(
            Guard::try_acquire(&path, Mode::Shared).unwrap().is_some(),
            "two readers may hold a shared lock"
        );
        assert!(
            Guard::try_acquire(&path, Mode::Exclusive)
                .unwrap()
                .is_none(),
            "a writer must not enter while a reader holds it"
        );
    }

    #[test]
    fn the_lock_file_is_0600() {
        let dir = tempdir();
        let path = dir.path().join("owner.lock");
        let guard = Guard::try_acquire(&path, Mode::Exclusive).unwrap().unwrap();
        assert_eq!(0o600, crate::atomic_json::mode(&path).unwrap());
        drop(guard);
    }

    #[test]
    fn with_exclusive_runs_the_body_and_propagates_its_error() {
        let dir = tempdir();
        let path = dir.path().join("allow.lock");
        assert_eq!(7, with_exclusive(&path, || Ok(7)).unwrap());
        let error: DunkError = with_exclusive(&path, || -> Result<(), DunkError> {
            Err(crate::error::refusal("nope"))
        })
        .unwrap_err();
        assert_eq!("nope", error.to_string());
        // and the lock is free again after an error
        assert!(Guard::try_acquire(&path, Mode::Exclusive)
            .unwrap()
            .is_some());
    }
}
