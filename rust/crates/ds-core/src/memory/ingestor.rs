//! `memory::ingestor` (T51, slice 5): the single ingest writer, ported from
//! `dunk_memory.MemoryIngestor`.
//!
//! One worker thread claims a queued source, parses it through `sources::ingest_path`, and
//! records the outcome with `ingest::finish`. Queries never touch this thread. The worker
//! opens its OWN writer connection (WAL, `BEGIN IMMEDIATE`, 5 s busy timeout) so a slow ingest
//! never holds a reader. Behaviour kept from the Python: `start` recovers expired leases and
//! requeues sources left partial by a stop BEFORE the loop begins and never fails on recovery;
//! a stop is recorded as skipped (not failed) and resumes on the next start; a failure is
//! recorded with the Python's back-off and the worker stays alive.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};

use super::ingest::{claim_next, enqueue, finish, recover_leases, requeue_partial, Outcome};
use super::jsonl::IngestStats;
use super::sources::ingest_path;
use crate::db::open_writer;
use crate::error::DunkError;

/// `DUNKINGSHEEP_MEMORY_THROTTLE_S`'s default.
pub const DEFAULT_THROTTLE_S: f64 = 0.1;
/// The Python claims with `claim_next()`'s default 120 s lease.
const LEASE_S: f64 = 120.0;

#[derive(Default)]
struct Shared {
    last_error: Option<String>,
    last_result: Option<Value>,
}

/// The state `status()` reports.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    pub running: bool,
    pub paused: bool,
    pub throttle_s: f64,
    pub last_error: Option<String>,
    pub last_result: Option<Value>,
}

pub struct Ingestor {
    path: PathBuf,
    poll: Duration,
    throttle: Duration,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    shared: Arc<Mutex<Shared>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

/// What `finish` stores as the source's stats (the Python's result dict).
fn stats_json(stats: &IngestStats) -> Value {
    let mut value =
        json!({"sessions": stats.sessions, "chunks": stats.chunks, "harness": stats.harness});
    if stats.stopped {
        value["stopped"] = json!(true);
    }
    value
}

impl Ingestor {
    pub fn new(path: impl AsRef<Path>, poll: Duration, throttle: Option<Duration>) -> Self {
        let throttle = throttle.unwrap_or_else(|| {
            let seconds = std::env::var("DUNKINGSHEEP_MEMORY_THROTTLE_S")
                .ok()
                .and_then(|text| text.parse::<f64>().ok())
                .unwrap_or(DEFAULT_THROTTLE_S);
            Duration::from_secs_f64(seconds.max(0.0))
        });
        Ingestor {
            path: path.as_ref().to_path_buf(),
            poll,
            throttle,
            stop: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            wake: Arc::new((Mutex::new(()), Condvar::new())),
            shared: Arc::new(Mutex::new(Shared::default())),
            thread: Mutex::new(None),
        }
    }

    fn running(&self) -> bool {
        self.thread
            .lock()
            .expect("thread lock")
            .as_ref()
            .is_some_and(|handle| !handle.is_finished())
    }

    /// `start`: recover, then run. A second start while running is a no-op.
    pub fn start(&self) -> Result<(), DunkError> {
        if self.running() {
            return Ok(());
        }
        // Recovery must not fail the start (Python swallows it): a locked or unreadable db is
        // retried by the loop's own claims.
        if let Ok(mut connection) = open_writer(&self.path) {
            let _ = recover_leases(&mut connection);
            let _ = requeue_partial(&mut connection);
        }
        self.stop.store(false, Ordering::SeqCst);
        let (path, poll, throttle) = (self.path.clone(), self.poll, self.throttle);
        let (stop, paused, wake, shared) = (
            Arc::clone(&self.stop),
            Arc::clone(&self.paused),
            Arc::clone(&self.wake),
            Arc::clone(&self.shared),
        );
        let handle = std::thread::Builder::new()
            .name("memory-ingest".to_string())
            .spawn(move || run_loop(&path, poll, throttle, &stop, &paused, &wake, &shared))
            .map_err(|error| DunkError::Refused(format!("memory ingest thread: {error}")))?;
        *self.thread.lock().expect("thread lock") = Some(handle);
        Ok(())
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.wake.1.notify_all();
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    /// `stop`: signal, wake the loop, join (the Python waits up to 5 s).
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.1.notify_all();
        if let Some(handle) = self.thread.lock().expect("thread lock").take() {
            let _ = handle.join();
        }
    }

    /// `enqueue`: a short writer connection, like the Python's store call.
    pub fn enqueue(
        &self,
        path: &str,
        harness: Option<&str>,
        project: Option<&str>,
    ) -> Result<(i64, i64), DunkError> {
        let mut connection = open_writer(&self.path)?;
        let queued = enqueue(&mut connection, path, harness, project)?;
        self.wake.1.notify_all();
        Ok(queued)
    }

    pub fn status(&self) -> Status {
        let shared = self.shared.lock().expect("shared lock");
        Status {
            running: self.running(),
            paused: self.paused(),
            throttle_s: self.throttle.as_secs_f64(),
            last_error: shared.last_error.clone(),
            last_result: shared.last_result.clone(),
        }
    }
}

impl Drop for Ingestor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run_loop(
    path: &Path,
    poll: Duration,
    throttle: Duration,
    stop: &AtomicBool,
    paused: &AtomicBool,
    wake: &(Mutex<()>, Condvar),
    shared: &Mutex<Shared>,
) {
    let mut connection = match open_writer(path) {
        Ok(connection) => connection,
        Err(error) => {
            shared.lock().expect("shared lock").last_error = Some(format!("open: {error}"));
            return;
        }
    };
    let record = |error: Option<String>, result: Option<Value>| {
        let mut state = shared.lock().expect("shared lock");
        if error.is_some() || result.is_some() {
            state.last_error = error;
            if result.is_some() {
                state.last_result = result;
            }
        }
    };
    loop {
        // `while not self._stop.wait(poll_s)`: sleep one poll, or until stop/enqueue wakes us.
        {
            let guard = wake.0.lock().expect("wake lock");
            let _ = wake.1.wait_timeout(guard, poll).expect("wake wait");
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if paused.load(Ordering::SeqCst) {
            continue;
        }
        let lease = match claim_next(&mut connection, LEASE_S) {
            Ok(Some(lease)) => lease,
            Ok(None) => continue,
            Err(error) => {
                record(Some(format!("claim: {error}")), None);
                continue;
            }
        };
        let should_stop = || stop.load(Ordering::SeqCst);
        match ingest_path(
            &mut connection,
            &lease.path,
            Some(&lease.harness),
            lease.project.as_deref(),
            &should_stop,
            throttle,
        ) {
            Ok(stats) => {
                let value = stats_json(&stats);
                let outcome = if stats.stopped {
                    Outcome::Skipped("stopped by operator")
                } else {
                    Outcome::Ok
                };
                if let Err(error) = finish(&mut connection, &lease, outcome, &value) {
                    record(Some(error.to_string()), None);
                } else {
                    record(None, Some(value));
                    // the Python clears last_error on a good run
                    shared.lock().expect("shared lock").last_error = None;
                }
            }
            Err(error) => {
                let text = error.to_string();
                let _ = finish(&mut connection, &lease, Outcome::Failed(&text), &json!({}));
                record(Some(text), None);
            }
        }
    }
}

#[cfg(test)]
#[path = "ingestor_tests.rs"]
mod tests;
