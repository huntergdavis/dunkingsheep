//! `memory::ingestor` (T51, slice 5): the single ingest writer, ported from
//! `dunk_memory.MemoryIngestor`.
//!
//! One parser thread streams source files and sends only individual SQL mutations to
//! the shared SyncWriter. Startup recovery, enqueue and maintenance use that same actor.
//! The legacy stop joins; the service also exposes a bounded stop retaining a busy handle.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};

#[cfg(test)]
use super::ingest::enqueue;
use super::ingest::{claim_next, finish, recover_leases, requeue_partial, Outcome};
use super::jsonl::IngestStats;
use super::service::{MemoryWriter, WriterSink};
use super::sources::ingest_path;
#[cfg(test)]
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
    writer: Arc<MemoryWriter>,
    poll: Duration,
    throttle: Duration,
    stop: Arc<AtomicBool>,
    paused: Arc<AtomicBool>,
    wake: Arc<(Mutex<()>, Condvar)>,
    shared: Arc<Mutex<Shared>>,
    thread: Mutex<Option<JoinHandle<()>>>,
    done: Arc<(Mutex<bool>, Condvar)>,
    parse_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
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
            writer: Arc::new(MemoryWriter::new(path.as_ref())),
            poll,
            throttle,
            stop: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            wake: Arc::new((Mutex::new(()), Condvar::new())),
            shared: Arc::new(Mutex::new(Shared::default())),
            thread: Mutex::new(None),
            done: Arc::new((Mutex::new(true), Condvar::new())),
            parse_hook: Mutex::new(None),
        }
    }

    pub(crate) fn with_writer(
        writer: Arc<MemoryWriter>,
        poll: Duration,
        throttle: Duration,
    ) -> Self {
        let mut ingestor = Self::new(writer.path(), poll, Some(throttle));
        ingestor.writer = writer;
        ingestor
    }

    #[cfg(test)]
    pub(crate) fn set_parse_hook(&self, hook: Arc<dyn Fn() + Send + Sync>) {
        *self.parse_hook.lock().unwrap() = Some(hook);
    }
    #[cfg(test)]
    pub(crate) fn worker_id(&self) -> Option<std::thread::ThreadId> {
        self.thread
            .lock()
            .unwrap()
            .as_ref()
            .map(|handle| handle.thread().id())
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
        let mut slot = self.thread.lock().expect("thread lock");
        if slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
            return Ok(());
        }
        if let Some(handle) = slot.take() {
            let _ = handle.join();
        }

        // Recovery must not fail the start (Python swallows it): a locked or unreadable db is
        // retried by the loop's own claims.
        if let Err(error) = self
            .writer
            .call(recover_leases)
            .and_then(|_| self.writer.call(requeue_partial))
        {
            self.shared.lock().expect("shared lock").last_error =
                Some(format!("recovery: {error}"));
        }
        self.stop.store(false, Ordering::SeqCst);
        let (writer, poll, throttle) = (Arc::clone(&self.writer), self.poll, self.throttle);
        let (stop, paused, wake, shared) = (
            Arc::clone(&self.stop),
            Arc::clone(&self.paused),
            Arc::clone(&self.wake),
            Arc::clone(&self.shared),
        );
        let parse_hook = self.parse_hook.lock().expect("parse hook lock").clone();
        let done = Arc::clone(&self.done);
        *done.0.lock().expect("done lock") = false;
        let handle = std::thread::Builder::new()
            .name("memory-ingest".to_string())
            .spawn(move || {
                struct Completed(Arc<(Mutex<bool>, Condvar)>);
                impl Drop for Completed {
                    fn drop(&mut self) {
                        *self.0 .0.lock().expect("done lock") = true;
                        self.0 .1.notify_all();
                    }
                }
                let _completed = Completed(done);
                run_loop(
                    writer,
                    (poll, throttle),
                    &stop,
                    &paused,
                    &wake,
                    &shared,
                    parse_hook,
                );
            })
            .map_err(|error| DunkError::Refused(format!("memory ingest thread: {error}")))?;
        *slot = Some(handle);
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
        {
            let _wake = self.wake.0.lock().expect("wake lock");
            self.stop.store(true, Ordering::SeqCst);
            self.wake.1.notify_all();
        }
        if let Some(handle) = self.thread.lock().expect("thread lock").take() {
            let _ = handle.join();
        }
    }

    /// A bounded Python-style stop retains the handle and writer when still busy.
    pub fn stop_with_timeout(&self, timeout: Duration) -> bool {
        let mut slot = self.thread.lock().expect("thread lock");
        {
            let _wake = self.wake.0.lock().expect("wake lock");
            self.stop.store(true, Ordering::SeqCst);
            self.wake.1.notify_all();
        }
        if slot.is_none() {
            return true;
        }
        let done = self.done.0.lock().expect("done lock");
        let (done, _) = self
            .done
            .1
            .wait_timeout_while(done, timeout, |done| !*done)
            .expect("done wait");
        if !*done {
            return false;
        }
        drop(done);
        if let Some(handle) = slot.take() {
            let _ = handle.join();
        }
        true
    }

    /// `enqueue`: prepare the source outside the actor, then serialize its SQL mutation.
    pub fn enqueue(
        &self,
        path: &str,
        harness: Option<&str>,
        project: Option<&str>,
    ) -> Result<(i64, i64), DunkError> {
        let queued = self.writer.enqueue(path, harness, project)?;
        self.wake.1.notify_all();
        Ok(queued)
    }

    pub fn status(&self) -> Status {
        let running = self.running();
        let shared = self.shared.lock().expect("shared lock");
        Status {
            running,
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
    writer: Arc<MemoryWriter>,
    timing: (Duration, Duration),
    stop: &AtomicBool,
    paused: &AtomicBool,
    wake: &(Mutex<()>, Condvar),
    shared: &Mutex<Shared>,
    parse_hook: Option<Arc<dyn Fn() + Send + Sync>>,
) {
    let (poll, throttle) = timing;
    let mut sink = WriterSink(Arc::clone(&writer));
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
            if !stop.load(Ordering::SeqCst) {
                let _ = wake.1.wait_timeout(guard, poll).expect("wake wait");
            }
        }
        if stop.load(Ordering::SeqCst) {
            return;
        }
        if paused.load(Ordering::SeqCst) {
            continue;
        }
        let lease = match writer.call(|connection| claim_next(connection, LEASE_S)) {
            Ok(Some(lease)) => lease,
            Ok(None) => continue,
            Err(error) => {
                record(Some(format!("claim: {error}")), None);
                continue;
            }
        };
        if let Some(hook) = &parse_hook {
            hook();
        }
        let should_stop = || stop.load(Ordering::SeqCst);
        match ingest_path(
            &mut sink,
            &lease.path,
            Some(&lease.harness),
            lease.project.as_deref(),
            &should_stop,
            throttle,
        ) {
            Ok(stats) => {
                let value = stats_json(&stats);
                let lease = lease.clone();
                let result_value = value.clone();
                let stopped = stats.stopped;
                if let Err(error) = writer.call(move |connection| {
                    finish(
                        connection,
                        &lease,
                        if stopped {
                            Outcome::Skipped("stopped by operator")
                        } else {
                            Outcome::Ok
                        },
                        &result_value,
                    )
                }) {
                    record(Some(error.to_string()), None);
                } else {
                    record(None, Some(value));
                    // the Python clears last_error on a good run
                    shared.lock().expect("shared lock").last_error = None;
                }
            }
            Err(error) => {
                let text = error.to_string();
                let failure = text.clone();
                let _ = writer.call(move |connection| {
                    finish(connection, &lease, Outcome::Failed(&failure), &json!({}))
                });
                record(Some(text), None);
            }
        }
    }
}

#[cfg(test)]
#[path = "ingestor_tests.rs"]
mod tests;
