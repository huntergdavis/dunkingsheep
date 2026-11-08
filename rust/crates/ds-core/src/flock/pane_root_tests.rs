//! t3368 tests: the caller-binding root pid is read fail closed. It keeps the 2 s cache, has no
//! grace window, and an error is never cached.

use std::cell::Cell;

use super::Flock;
use crate::error::DunkError;

fn flock() -> (tempfile::TempDir, Flock) {
    let dir = tempfile::tempdir().unwrap();
    let flock = Flock::new(
        dir.path().join("launches.json"),
        dir.path().join("lanes.json"),
        false,
    );
    (dir, flock)
}

#[test]
fn a_found_root_pid_is_served_from_the_two_second_cache() {
    let (_dir, flock) = flock();
    let reads = Cell::new(0);
    let read = || -> Result<Option<Option<i64>>, DunkError> {
        reads.set(reads.get() + 1);
        Ok(Some(Some(4321)))
    };
    assert_eq!(
        flock.pane_root_pid("%1", 100.0, read).unwrap(),
        Some(Some(4321))
    );
    assert_eq!(
        flock.pane_root_pid("%1", 101.9, read).unwrap(),
        Some(Some(4321))
    );
    assert_eq!(reads.get(), 1, "a found pid is cached for 2 s");
    assert_eq!(
        flock.pane_root_pid("%1", 102.1, read).unwrap(),
        Some(Some(4321))
    );
    assert_eq!(reads.get(), 2, "the cache expires after 2 s");
}

#[test]
fn a_listing_that_misses_the_pane_refuses_with_no_grace_window() {
    let (_dir, flock) = flock();
    let found = || -> Result<Option<Option<i64>>, DunkError> { Ok(Some(Some(4321))) };
    assert_eq!(
        flock.pane_root_pid("%1", 100.0, found).unwrap(),
        Some(Some(4321))
    );
    // Well past the 2 s cache the listing no longer shows the pane. The earlier sighting must not
    // stand in for it: the role-pane 180 s fail-open grace does not apply to the binding read.
    let missing = || -> Result<Option<Option<i64>>, DunkError> { Ok(None) };
    assert_eq!(flock.pane_root_pid("%1", 150.0, missing).unwrap(), None);
}

#[test]
fn a_read_that_fails_is_an_error_and_is_not_cached() {
    let (_dir, flock) = flock();
    let failing = || -> Result<Option<Option<i64>>, DunkError> {
        Err(DunkError::Refused("tmux refused".into()))
    };
    assert!(flock.pane_root_pid("%1", 100.0, failing).is_err());
    let found = || -> Result<Option<Option<i64>>, DunkError> { Ok(Some(Some(4321))) };
    assert_eq!(
        flock.pane_root_pid("%1", 100.5, found).unwrap(),
        Some(Some(4321)),
        "the failure was not cached, so the next read answers"
    );
}
