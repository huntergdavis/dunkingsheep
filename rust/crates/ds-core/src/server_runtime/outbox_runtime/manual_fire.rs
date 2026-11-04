//! Tracked manual-fire workers. A request captures one DunkerHandle before this owner starts
//! any countdown; worker-local terminal and inbox adapters are created only on the worker.

use super::{Factory, FlockSnapshot};
use crate::error::DunkError;
use crate::flock::dunks::{DunkList, DunkerHandle};
use crate::flock::sends::{OutboxWorkerPool, SnapshotStore};
use crate::flock::Flock;
use crate::server::RequestCtx;
use crate::server_runtime::{
    inbox_data::InboxRuntimeView, RequestRuntimeAdapter, TerminalCallAdapter,
};
use crate::terminal::Terminal;
use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Condvar, Mutex, Weak,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::runtime::Handle;

struct FireSignal {
    cancelled: AtomicBool,
    lock: Mutex<()>,
    changed: Condvar,
}

impl FireSignal {
    fn new() -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            lock: Mutex::new(()),
            changed: Condvar::new(),
        }
    }

    fn cancel(&self) {
        let _guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        self.cancelled.store(true, Ordering::Release);
        self.changed.notify_all();
    }

    /// Returns true when shutdown cancelled this countdown step.
    fn wait_one_second(&self) -> bool {
        let guard = self.lock.lock().unwrap_or_else(|error| error.into_inner());
        if self.cancelled.load(Ordering::Acquire) {
            return true;
        }
        let (_guard, _) = self
            .changed
            .wait_timeout_while(guard, Duration::from_secs(1), |_| {
                !self.cancelled.load(Ordering::Acquire)
            })
            .unwrap_or_else(|error| error.into_inner());
        self.cancelled.load(Ordering::Acquire)
    }
}

struct FireWorker {
    signal: Arc<FireSignal>,
    join: JoinHandle<()>,
}

#[derive(Default)]
struct WorkerState {
    stopped: bool,
    workers: HashMap<u64, FireWorker>,
}

/// Process-owned manual-fire threads. Workers hold weak Flock/pool references, so the owner can
/// cancel countdowns and join workers before their services are torn down without an Arc cycle.
pub(super) struct ManualFireOwner {
    flock: Weak<Flock>,
    runtime: Handle,
    factory: Arc<Factory>,
    pool: Weak<OutboxWorkerPool>,
    next_id: AtomicU64,
    state: Mutex<WorkerState>,
    completed: Arc<Mutex<VecDeque<(String, String)>>>,
    #[cfg(test)]
    countdown_gate: Arc<Mutex<Option<Arc<CountdownGate>>>>,
    #[cfg(test)]
    submit_gate: Arc<Mutex<Option<Arc<SubmitGate>>>>,
    #[cfg(test)]
    fail_next_spawn: AtomicBool,
}

#[cfg(test)]
pub(crate) struct CountdownGate {
    entered: std::sync::mpsc::Sender<i64>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
impl CountdownGate {
    pub(crate) fn channel() -> (
        Arc<Self>,
        std::sync::mpsc::Receiver<i64>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                entered: entered_tx,
                release: Mutex::new(release_rx),
            }),
            entered_rx,
            release_tx,
        )
    }

    fn step(&self, remaining: i64) {
        self.entered.send(remaining).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
    }
}

#[cfg(test)]
pub(crate) struct SubmitGate {
    entered: std::sync::mpsc::Sender<()>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
}

#[cfg(test)]
impl SubmitGate {
    pub(crate) fn channel() -> (
        Arc<Self>,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<()>,
    ) {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        (
            Arc::new(Self {
                entered: entered_tx,
                release: Mutex::new(release_rx),
            }),
            entered_rx,
            release_tx,
        )
    }

    fn wait(&self) {
        self.entered.send(()).unwrap();
        self.release
            .lock()
            .unwrap()
            .recv_timeout(Duration::from_secs(10))
            .unwrap();
    }
}

impl ManualFireOwner {
    pub(super) fn new(
        flock: Weak<Flock>,
        runtime: Handle,
        factory: Arc<Factory>,
        pool: Weak<OutboxWorkerPool>,
    ) -> Self {
        Self {
            flock,
            runtime,
            factory,
            pool,
            next_id: AtomicU64::new(1),
            state: Mutex::new(WorkerState::default()),
            completed: Arc::new(Mutex::new(VecDeque::new())),
            #[cfg(test)]
            countdown_gate: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            submit_gate: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            fail_next_spawn: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub(crate) fn set_countdown_gate(&self, gate: Option<Arc<CountdownGate>>) {
        *self
            .countdown_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = gate;
    }

    #[cfg(test)]
    pub(crate) fn set_submit_gate(&self, gate: Option<Arc<SubmitGate>>) {
        *self
            .submit_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = gate;
    }

    #[cfg(test)]
    pub(crate) fn fail_next_spawn(&self) {
        self.fail_next_spawn.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(crate) fn is_stopped(&self) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .stopped
    }

    #[cfg(test)]
    pub(crate) fn completed(&self) -> Vec<(String, String)> {
        self.completed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .cloned()
            .collect()
    }

    pub(super) fn fire(
        &self,
        id: &str,
        countdown_s: i64,
        wait: bool,
    ) -> Result<serde_json::Value, DunkError> {
        self.join_finished();
        let flock = self
            .flock
            .upgrade()
            .ok_or_else(|| DunkError::Refused("manual dunk fire runtime is stopped".into()))?;
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        if state.stopped {
            return Err(DunkError::Refused(
                "manual dunk fire runtime is shutting down".into(),
            ));
        }

        // Match Python's ordering: find and validate the target before any countdown begins,
        // retaining the exact row lifetime even if remove/add reuses the same visible id.
        let handle = flock.capture_fire(id)?;
        let dunk_id = DunkList::timer_fields(&handle).id;
        #[cfg(test)]
        if self.fail_next_spawn.swap(false, Ordering::AcqRel) {
            let error = "injected thread spawn failure";
            DunkList::finish_fire(&handle, false, error);
            let _ = FlockSnapshot.save(&flock);
            return Err(DunkError::Refused(format!(
                "could not start manual dunk fire: {error}"
            )));
        }
        let signal = Arc::new(FireSignal::new());
        let worker_signal = Arc::clone(&signal);
        let worker_flock = Arc::downgrade(&flock);
        let runtime = self.runtime.clone();
        let factory = Arc::clone(&self.factory);
        let pool = self.pool.clone();
        let worker_handle = Arc::clone(&handle);
        let worker_dunk_id = dunk_id.clone();
        let completed = Arc::clone(&self.completed);
        #[cfg(test)]
        let countdown_gate = self
            .countdown_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        #[cfg(test)]
        let submit_gate = self
            .submit_gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let worker = thread::Builder::new()
            .name(format!("manual-dunk-fire-{dunk_id}"))
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_fire(WorkerFire {
                        flock: worker_flock.clone(),
                        runtime,
                        factory,
                        pool,
                        handle: worker_handle.clone(),
                        signal: worker_signal,
                        countdown_s,
                        #[cfg(test)]
                        countdown_gate,
                        #[cfg(test)]
                        submit_gate,
                    })
                }))
                .unwrap_or_else(|_| {
                    Err(DunkError::Refused(
                        "manual dunk fire worker panicked".into(),
                    ))
                });
                if let Err(error) = &result {
                    DunkList::finish_fire(&worker_handle, false, &error.to_string());
                    if let Some(flock) = worker_flock.upgrade() {
                        let _ = FlockSnapshot.save(&flock);
                    }
                }
                let outcome = match &result {
                    Ok(value) => value.to_string(),
                    Err(error) => format!("error: {error}"),
                };
                let mut completed = completed.lock().unwrap_or_else(|error| error.into_inner());
                if completed.len() == 128 {
                    completed.pop_front();
                }
                completed.push_back((worker_dunk_id, outcome));
                drop(completed);
                let _ = result_tx.send(result);
            });

        let join = match worker {
            Ok(join) => join,
            Err(error) => {
                DunkList::finish_fire(&handle, false, &error.to_string());
                let _ = FlockSnapshot.save(&flock);
                return Err(DunkError::Refused(format!(
                    "could not start manual dunk fire: {error}"
                )));
            }
        };
        let worker_id = self.next_id.fetch_add(1, Ordering::Relaxed);
        state.workers.insert(worker_id, FireWorker { signal, join });
        drop(state);

        if wait {
            return result_rx.recv().map_err(|_| {
                DunkError::Refused("manual dunk fire worker ended without a result".into())
            })?;
        }
        Ok(serde_json::json!({
            "id": dunk_id,
            "ok": serde_json::Value::Null,
            "message": "test send scheduled",
        }))
    }

    fn join_finished(&self) {
        let finished = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let ids = state
                .workers
                .iter()
                .filter_map(|(id, worker)| worker.join.is_finished().then_some(*id))
                .collect::<Vec<_>>();
            ids.into_iter()
                .filter_map(|id| state.workers.remove(&id))
                .collect::<Vec<_>>()
        };
        for worker in finished {
            let _ = worker.join.join();
        }
    }

    /// Shutdown cancels workers still in countdown. A worker past the countdown may finish its
    /// real send or durable enqueue; shutdown joins it before pool/store/runtime teardown. Normal
    /// stop/remove never calls this method and therefore never cancels a fire.
    pub(super) fn stop_and_join(&self) {
        let workers = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.stopped = true;
            std::mem::take(&mut state.workers)
        };
        for worker in workers.values() {
            worker.signal.cancel();
        }
        for (_, worker) in workers {
            let _ = worker.join.join();
        }
    }
}

impl Drop for ManualFireOwner {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

struct TrustedWorker<'a>(&'a Flock);

impl TrustedWorker<'_> {
    fn new(flock: &Flock) -> TrustedWorker<'_> {
        flock.clear_caller();
        flock.clear_socket_peer();
        TrustedWorker(flock)
    }
}

impl Drop for TrustedWorker<'_> {
    fn drop(&mut self) {
        self.0.clear_caller();
        self.0.clear_socket_peer();
    }
}

struct WorkerFire {
    flock: Weak<Flock>,
    runtime: Handle,
    factory: Arc<Factory>,
    pool: Weak<OutboxWorkerPool>,
    handle: DunkerHandle,
    signal: Arc<FireSignal>,
    countdown_s: i64,
    #[cfg(test)]
    countdown_gate: Option<Arc<CountdownGate>>,
    #[cfg(test)]
    submit_gate: Option<Arc<SubmitGate>>,
}

fn run_fire(worker: WorkerFire) -> Result<serde_json::Value, DunkError> {
    let WorkerFire {
        flock: weak_flock,
        runtime,
        factory,
        pool: weak_pool,
        handle,
        signal,
        countdown_s,
        #[cfg(test)]
        countdown_gate,
        #[cfg(test)]
        submit_gate,
    } = worker;
    for remaining in (1..=countdown_s.max(0)).rev() {
        if signal.cancelled.load(Ordering::Acquire) {
            return Err(DunkError::Refused(
                "manual dunk fire cancelled during runtime shutdown".into(),
            ));
        }
        DunkList::set_fire_status(&handle, &format!("Test in {remaining}..."));
        #[cfg(test)]
        if let Some(gate) = &countdown_gate {
            gate.step(remaining);
            if signal.cancelled.load(Ordering::Acquire) {
                return Err(DunkError::Refused(
                    "manual dunk fire cancelled during runtime shutdown".into(),
                ));
            }
            continue;
        }
        if signal.wait_one_second() {
            return Err(DunkError::Refused(
                "manual dunk fire cancelled during runtime shutdown".into(),
            ));
        }
    }
    if signal.cancelled.load(Ordering::Acquire) {
        return Err(DunkError::Refused(
            "manual dunk fire cancelled during runtime shutdown".into(),
        ));
    }
    let flock = weak_flock
        .upgrade()
        .ok_or_else(|| DunkError::Refused("manual dunk fire runtime is stopped".into()))?;
    let _trusted = TrustedWorker::new(&flock);
    let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(factory());
    let pool = weak_pool
        .upgrade()
        .ok_or_else(|| DunkError::Refused("manual dunk fire outbox is stopped".into()))?;
    let request = RequestCtx::default();
    let adapter = RequestRuntimeAdapter::new(&flock, &request, &runtime, &terminal);
    // Constructing the canonical view validates the inbox store/outbox/Flock ownership before
    // any send or queue side effect. The terminal remains owned by this worker for its lifetime.
    let _inbox = InboxRuntimeView::new(adapter, Arc::clone(&flock), Arc::clone(&pool))?;
    let mut env = TerminalCallAdapter {
        flock: &flock,
        terminal: &terminal,
        runtime: &runtime,
    };
    #[cfg(test)]
    if let Some(gate) = submit_gate {
        gate.wait();
    }
    flock.fire_captured(
        &handle,
        &mut env,
        pool.as_ref(),
        Arc::new(FlockSnapshot),
        || None,
    )
}
