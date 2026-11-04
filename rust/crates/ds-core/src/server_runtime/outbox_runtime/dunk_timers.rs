//! Per-Dunker lifetime ownership for scheduled delivery; uses the canonical outbox services.
use super::{Factory, FlockSnapshot};
use crate::error::DunkError;
use crate::flock::loops_stuck::StalePanelReporter;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::sends::{pane_generation, OutboxWorkerPool, SnapshotStore};
use crate::flock::Flock;
use crate::server::RequestCtx;
use crate::server_runtime::{
    inbox_data::InboxRuntimeView, RequestRuntimeAdapter, TerminalCallAdapter,
};
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tokio::runtime::Handle;

#[cfg(test)]
pub(super) type TimerPrecommitBarrier =
    Arc<Mutex<Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>>>;

pub(super) struct TimerSignal {
    #[cfg(test)]
    pub(super) precommit: TimerPrecommitBarrier,
    pub(super) cancelled: AtomicBool,
    pub(super) generation: Mutex<u64>,
    pub(super) changed: std::sync::Condvar,
}

pub(super) struct TimerWorker {
    pub(super) signal: Arc<TimerSignal>,
    pub(super) join: JoinHandle<()>,
}

pub(super) struct DunkTimerOwner {
    #[cfg(test)]
    pub(super) precommit: TimerPrecommitBarrier,
    pub(super) stopped: AtomicBool,
    pub(super) flock: Weak<Flock>,
    pub(super) runtime: Handle,
    pub(super) factory: Arc<Factory>,
    pub(super) pool: Weak<OutboxWorkerPool>,
    pub(super) reporter: StalePanelReporter,
    pub(super) workers: Mutex<HashMap<usize, TimerWorker>>,
}

impl DunkTimerOwner {
    pub(super) fn new(
        flock: Weak<Flock>,
        runtime: Handle,
        factory: Arc<Factory>,
        pool: Weak<OutboxWorkerPool>,
        reporter: StalePanelReporter,
    ) -> Self {
        Self {
            #[cfg(test)]
            precommit: Arc::new(Mutex::new(None)),
            stopped: AtomicBool::new(false),
            flock,
            runtime,
            factory,
            pool,
            reporter,
            workers: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn key(handle: &crate::flock::dunks::DunkerHandle) -> usize {
        Arc::as_ptr(handle) as usize
    }

    pub(super) fn start(&self, handle: crate::flock::dunks::DunkerHandle) -> Result<(), DunkError> {
        let key = Self::key(&handle);
        let mut workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
        if self.stopped.load(Ordering::Acquire) {
            return Err(DunkError::Refused("dunk timer runtime is stopped".into()));
        }
        // A timer can exit after removal or an error before a later start observes it.
        // Reap completed registrations so they cannot suppress a replacement worker.
        let finished: Vec<_> = workers
            .iter()
            .filter_map(|(key, worker)| worker.join.is_finished().then_some(*key))
            .collect();
        for key in finished {
            if let Some(worker) = workers.remove(&key) {
                let _ = worker.join.join();
            }
        }
        if let Some(worker) = workers.get(&key) {
            worker.signal.changed.notify_all();
            return Ok(());
        }
        let signal = Arc::new(TimerSignal {
            #[cfg(test)]
            precommit: Arc::clone(&self.precommit),
            cancelled: AtomicBool::new(false),
            generation: Mutex::new(0),
            changed: std::sync::Condvar::new(),
        });
        let thread_signal = Arc::clone(&signal);
        let flock = self.flock.clone();
        let runtime = self.runtime.clone();
        let factory = Arc::clone(&self.factory);
        let pool = self.pool.clone();
        let reporter = self.reporter.clone();
        let join = thread::Builder::new()
            .name(format!(
                "dunk-timer-{}",
                crate::flock::dunks::DunkList::timer_fields(&handle).id
            ))
            .spawn(move || {
                let failed_handle = Arc::clone(&handle);
                let failed_flock = flock.clone();
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_dunk_timer(
                        flock,
                        runtime,
                        factory,
                        handle,
                        thread_signal,
                        pool,
                        reporter,
                    )
                }));
                if result.is_err() {
                    crate::flock::dunks::DunkList::set_timer_error(
                        &failed_handle,
                        "scheduled dunk worker panicked",
                    );
                    if let Some(flock) = failed_flock.upgrade() {
                        save_timer_state(&flock, &failed_handle, &FlockSnapshot);
                    }
                }
            });
        let join = join
            .map_err(|error| DunkError::Refused(format!("could not start dunk timer: {error}")))?;
        workers.insert(key, TimerWorker { signal, join });
        Ok(())
    }

    pub(super) fn wake(&self, handle: &crate::flock::dunks::DunkerHandle) {
        if let Some(worker) = self
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&Self::key(handle))
        {
            let mut generation = worker
                .signal
                .generation
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *generation = generation.wrapping_add(1);
            worker.signal.changed.notify_all();
        }
    }

    pub(super) fn stop(&self, handle: &crate::flock::dunks::DunkerHandle) {
        let worker = self
            .workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&Self::key(handle));
        if let Some(worker) = worker {
            let guard = worker
                .signal
                .generation
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            worker.signal.cancelled.store(true, Ordering::Release);
            worker.signal.changed.notify_all();
            drop(guard);
            let _ = worker.join.join();
        }
    }

    pub(super) fn stop_all(&self) {
        let workers = {
            let mut workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
            self.stopped.store(true, Ordering::Release);
            std::mem::take(&mut *workers)
        };
        for worker in workers.values() {
            let guard = worker
                .signal
                .generation
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            worker.signal.cancelled.store(true, Ordering::Release);
            worker.signal.changed.notify_all();
            drop(guard);
        }
        for (_, worker) in workers {
            let _ = worker.join.join();
        }
    }
}

impl Drop for DunkTimerOwner {
    fn drop(&mut self) {
        self.stop_all();
    }
}

fn timer_wait(signal: &TimerSignal, seconds: f64) -> bool {
    let generation = signal.generation.lock().unwrap_or_else(|e| e.into_inner());
    if signal.cancelled.load(Ordering::Acquire) {
        return true;
    }
    let before = *generation;
    let duration = Duration::from_secs_f64(seconds.max(0.0));
    let (generation, _) = signal
        .changed
        .wait_timeout(generation, duration)
        .unwrap_or_else(|e| e.into_inner());
    signal.cancelled.load(Ordering::Acquire) || *generation != before
}

fn save_timer_state(
    flock: &Flock,
    handle: &crate::flock::dunks::DunkerHandle,
    store: &dyn SnapshotStore,
) {
    if let Err(error) = store.save(flock) {
        crate::flock::dunks::DunkList::set_timer_error(handle, &error.to_string());
        eprintln!("failed to save scheduled dunk state: {error}");
    }
}

fn record_timer_metric(
    flock: &Flock,
    handle: &crate::flock::dunks::DunkerHandle,
    kind: &str,
    outcome: &str,
    reason: Option<String>,
) {
    let Some(metrics) = flock.stores().metrics else {
        return;
    };
    let fields = crate::flock::dunks::DunkList::timer_fields(handle);
    metrics.record(&crate::metrics::Event {
        at: 0.0,
        kind: kind.to_owned(),
        pane_id: fields.target_pane_id,
        mailbox: None,
        sender: None,
        source: Some(fields.id),
        outcome: Some(outcome.to_owned()),
        reason,
        duration_ms: None,
        attempts: None,
        bytes: None,
    });
}

fn run_dunk_timer(
    weak_flock: Weak<Flock>,
    runtime: Handle,
    factory: Arc<Factory>,
    handle: crate::flock::dunks::DunkerHandle,
    signal: Arc<TimerSignal>,
    pool: Weak<OutboxWorkerPool>,
    mut reporter: StalePanelReporter,
) {
    let Some(flock) = weak_flock.upgrade() else {
        return;
    };
    let terminal = Mutex::new(factory());
    let Some(pool) = pool.upgrade() else { return };
    let request = RequestCtx::default();
    let adapter = RequestRuntimeAdapter::new(&flock, &request, &runtime, &terminal);
    let Ok(inbox) = InboxRuntimeView::new(adapter, Arc::clone(&flock), pool) else {
        return;
    };
    let mut panel_seen = None;
    let mut panel_since = crate::clock::now_s();
    let mut gate_held = false;
    let mut env = TerminalCallAdapter {
        flock: &flock,
        terminal: &terminal,
        runtime: &runtime,
    };
    let store: Arc<dyn SnapshotStore> = Arc::new(FlockSnapshot);
    loop {
        if signal.cancelled.load(Ordering::Acquire) || !flock.dunks.is_running(&handle) {
            break;
        }
        let fields = crate::flock::dunks::DunkList::timer_fields(&handle);
        let now = crate::clock::now_s();
        let due = fields.next_send_at.unwrap_or_else(|| {
            let deadline = now + (fields.interval_minutes * 60.0).max(1.0);
            crate::flock::dunks::DunkList::timer_set_deadline(&handle, deadline);
            deadline
        });
        if fields.next_send_at.is_none() {
            crate::flock::dunks::DunkList::set_timer_status(&handle, "Scheduled");
        }
        if due > now {
            let remaining = due - now;
            let seconds = remaining.ceil() as i64;
            let mins = seconds / 60;
            let secs = seconds % 60;
            crate::flock::dunks::DunkList::set_timer_status(
                &handle,
                &format!("Next: {mins:02}:{secs:02}"),
            );
            timer_wait(&signal, remaining.min(1.0));
            continue;
        }
        let Some(pane) = fields.target_pane_id.as_deref() else {
            break;
        };
        if fields.only_when_idle {
            match PaneInfoEnv::get_pane(&env, pane) {
                None => {
                    crate::flock::dunks::DunkList::timer_target_gone(&handle);
                }
                Some(info)
                    if info
                        .get("agent_status")
                        .and_then(serde_json::Value::as_str)
                        .is_some_and(|status| crate::push::BUSY_STATUSES.contains(&status)) =>
                {
                    crate::flock::dunks::DunkList::set_timer_status(&handle, "Busy");
                    timer_wait(&signal, 1.0);
                    continue;
                }
                _ => {}
            }
        }
        if fields.hold_while_typing
            && !crate::flock::sends::clear_to_send(&env, pane, &|seconds| {
                timer_wait(&signal, seconds)
            })
        {
            if signal.cancelled.load(Ordering::Acquire) {
                break;
            }
            if !gate_held {
                gate_held = true;
                crate::flock::dunks::DunkList::timer_hold(&handle, "the box would not clear");
                save_timer_state(&flock, &handle, store.as_ref());
            }
            crate::flock::dunks::DunkList::set_timer_status(&handle, "Held (typing)");
            timer_wait(&signal, 60.0);
            // Returning through the idle gate is essential: clearing the box may submit a turn.
            continue;
        }
        gate_held = false;
        if fields.skip_if_unconsumed && fields.send_count > 0 {
            if let Some(text) = fields.last_sent_text.as_deref() {
                let screen = crate::flock::sends::SendEnv::read_pane(&env, pane, 40, false);
                match crate::flock::dunks::input_pending(screen.as_deref(), text) {
                    Some(false) => crate::flock::dunks::DunkList::timer_consumed(&handle),
                    Some(true) => {
                        if signal.cancelled.load(Ordering::Acquire) {
                            break;
                        }
                        crate::flock::dunks::DunkList::timer_skip(&handle, "unconsumed");
                        record_timer_metric(
                            &flock,
                            &handle,
                            "dunk.skipped",
                            "held",
                            Some("unconsumed".into()),
                        );
                        save_timer_state(&flock, &handle, store.as_ref());
                        timer_wait(&signal, 1.0);
                        continue;
                    }
                    None => {
                        if crate::flock::dunks::DunkList::timer_unknown_skip(&handle) {
                            crate::flock::dunks::DunkList::timer_skip(&handle, "unrecognised");
                            record_timer_metric(
                                &flock,
                                &handle,
                                "dunk.skipped",
                                "held",
                                Some("unrecognised".into()),
                            );
                            save_timer_state(&flock, &handle, store.as_ref());
                            timer_wait(&signal, 1.0);
                            continue;
                        }
                    }
                }
            }
        }
        if signal.cancelled.load(Ordering::Acquire) {
            break;
        }
        crate::flock::dunks::DunkList::set_timer_status(&handle, "Sending...");
        #[cfg(test)]
        {
            let barrier = signal.precommit.lock().unwrap().take();
            if let Some((entered, release)) = barrier {
                entered.send(()).unwrap();
                release.recv_timeout(Duration::from_secs(10)).unwrap();
            }
        }
        let (sent, ok, detail, blocker) =
            flock.scheduled_dunk_send(&handle, &mut env, Arc::clone(&store), || {
                (signal.cancelled.load(Ordering::Acquire) || !flock.dunks.is_running(&handle))
                    .then(|| "scheduled dunk cancelled before attempt".to_owned())
            });
        if signal.cancelled.load(Ordering::Acquire) || !flock.dunks.is_running(&handle) {
            break;
        }
        if sent {
            let done =
                crate::flock::dunks::DunkList::timer_after_send(&handle, true, ok.unwrap_or(false));
            save_timer_state(&flock, &handle, store.as_ref());
            if done {
                flock.dunks.remove_handle(&handle);
                save_timer_state(&flock, &handle, store.as_ref());
                break;
            }
            timer_wait(&signal, 1.0);
        } else {
            let now = crate::clock::now_s();
            let signature = flock.dunk_hold_signature(&env, pane);
            if panel_seen.as_ref() != Some(&signature) {
                panel_seen = Some(signature.clone());
                panel_since = now;
            } else if signature.0 == "modal" {
                let generation = pane_generation(&env, pane);
                reporter.note(
                    &flock,
                    &inbox,
                    pane,
                    "modal",
                    generation.as_deref(),
                    Some(&signature.1),
                    panel_since,
                );
            } else if signature.0 == "box"
                && flock.unwedge(
                    &mut env,
                    pane,
                    &signature.1,
                    Some(panel_since),
                    None,
                    now,
                    None,
                    true,
                )
            {
                panel_seen = None;
                panel_since = now;
                continue;
            }
            crate::flock::dunks::DunkList::timer_hold(
                &handle,
                blocker.as_deref().unwrap_or(&detail),
            );
            record_timer_metric(&flock, &handle, "dunk.held", "held", Some(detail));
            save_timer_state(&flock, &handle, store.as_ref());
            timer_wait(&signal, 60.0);
        }
    }
}
