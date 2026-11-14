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
use std::cell::Cell;
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
    #[cfg(test)]
    pub(super) before_fields: TimerPrecommitBarrier,
    #[cfg(test)]
    pub(super) before_hold_wait: TimerPrecommitBarrier,
    pub(super) cancelled: AtomicBool,
    pub(super) generation: Mutex<u64>,
    pub(super) changed: std::sync::Condvar,
}

impl TimerSignal {
    fn wake(&self) {
        let mut generation = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        *generation = generation.wrapping_add(1);
        self.changed.notify_all();
    }
}

pub(super) struct TimerWorker {
    pub(super) signal: Arc<TimerSignal>,
    pub(super) join: JoinHandle<()>,
}

pub(super) struct DunkTimerOwner {
    #[cfg(test)]
    pub(super) precommit: TimerPrecommitBarrier,
    #[cfg(test)]
    pub(super) before_fields: TimerPrecommitBarrier,
    #[cfg(test)]
    pub(super) before_hold_wait: TimerPrecommitBarrier,
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
            #[cfg(test)]
            before_fields: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            before_hold_wait: Arc::new(Mutex::new(None)),
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

    pub(super) fn has_live_worker(&self, handle: &crate::flock::dunks::DunkerHandle) -> bool {
        self.workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&Self::key(handle))
            .is_some_and(|worker| !worker.join.is_finished())
    }

    pub(super) fn start(&self, handle: crate::flock::dunks::DunkerHandle) -> Result<(), DunkError> {
        let key = Self::key(&handle);
        let thread_name = format!(
            "dunk-timer-{}",
            crate::flock::dunks::DunkList::timer_fields(&handle).id
        );
        let mut workers = loop {
            let mut workers = self.workers.lock().unwrap_or_else(|e| e.into_inner());
            if self.stopped.load(Ordering::Acquire) {
                return Err(DunkError::Refused("dunk timer runtime is stopped".into()));
            }
            if let Some(worker) = workers.get(&key) {
                if !worker.join.is_finished() {
                    worker.signal.wake();
                    return Ok(());
                }
            }
            // Remove finished registrations under the map lock, then join them after
            // releasing it so a worker exit can never participate in lock inversion.
            let finished_keys: Vec<_> = workers
                .iter()
                .filter_map(|(key, worker)| worker.join.is_finished().then_some(*key))
                .collect();
            let finished: Vec<_> = finished_keys
                .into_iter()
                .filter_map(|key| workers.remove(&key))
                .collect();
            if finished.is_empty() {
                break workers;
            }
            drop(workers);
            for worker in finished {
                let _ = worker.join.join();
            }
        };
        let signal = Arc::new(TimerSignal {
            #[cfg(test)]
            precommit: Arc::clone(&self.precommit),
            #[cfg(test)]
            before_fields: Arc::clone(&self.before_fields),
            #[cfg(test)]
            before_hold_wait: Arc::clone(&self.before_hold_wait),
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
        let log = crate::daemon_log::ThreadLog::capture();
        let join = thread::Builder::new().name(thread_name).spawn(move || {
            log.adopt();
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
            worker.signal.wake();
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
        if let Err(error) = self.stop_all_checked() {
            eprintln!("dunk timer shutdown failed: {error}");
        }
    }

    pub(super) fn stop_all_checked(&self) -> Result<(), DunkError> {
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
        let mut failures = Vec::new();
        for (key, worker) in workers {
            if worker.join.join().is_err() {
                failures.push(format!("timer worker {key} panicked"));
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(crate::error::refusal(failures.join("; ")))
        }
    }
}

impl Drop for DunkTimerOwner {
    fn drop(&mut self) {
        self.stop_all();
    }
}

#[cfg(test)]
fn cross_timer_barrier(barrier: &TimerPrecommitBarrier) {
    let barrier = barrier.lock().unwrap().take();
    if let Some((entered, release)) = barrier {
        entered.send(()).unwrap();
        release.recv_timeout(Duration::from_secs(10)).unwrap();
    }
}

fn timer_wait(signal: &TimerSignal, observed: &Cell<u64>, seconds: f64) -> bool {
    #[cfg(test)]
    if seconds == 60.0 {
        cross_timer_barrier(&signal.before_hold_wait);
    }
    let generation = signal.generation.lock().unwrap_or_else(|e| e.into_inner());
    if signal.cancelled.load(Ordering::Acquire) {
        return true;
    }
    // Retain wakes delivered while the worker was doing I/O or accounting, before this wait.
    if *generation != observed.get() {
        observed.set(*generation);
        return true;
    }
    let before = observed.get();
    let duration = Duration::from_secs_f64(seconds.max(0.0));
    let (generation, _) = signal
        .changed
        .wait_timeout_while(generation, duration, |current| {
            !signal.cancelled.load(Ordering::Acquire) && *current == before
        })
        .unwrap_or_else(|e| e.into_inner());
    observed.set(*generation);
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

/// t4681: one journal line for every thing a scheduled dunk does, so a delivery can be read back.
fn log_dunk(handle: &crate::flock::dunks::DunkerHandle, pane: &str, what: &str) {
    let id = crate::flock::dunks::DunkList::timer_fields(handle).id;
    eprintln!("dunk {id} -> {pane}: {what}");
}

/// t4681: the composer refusals that mean "a person's text is in the box". The dunk is then
/// stored as an inbox row; every other refusal stays an ordinary hold.
fn human_draft(blocker: Option<&str>) -> bool {
    matches!(blocker, Some("human_draft"))
}

/// t4681: store the dunk as an inbox row instead of typing it, log what happened and advance the
/// schedule. `Some(done)` when it was stored or is already waiting; `None` when the store failed
/// and the caller should hold.
fn store_instead_of_typing(
    flock: &Flock,
    inbox: &InboxRuntimeView<'_>,
    handle: &crate::flock::dunks::DunkerHandle,
    pane: &str,
    why: &str,
) -> Option<bool> {
    use crate::flock::dunks::DunkStored;
    match flock.store_scheduled_dunk(handle, inbox) {
        Ok(DunkStored::Stored { message_id, notice }) => {
            log_dunk(
                handle,
                pane,
                &format!(
                    "stored in the inbox as {message_id}, nothing typed ({why}); notice {notice}"
                ),
            );
            record_timer_metric(flock, handle, "dunk.stored", "ok", Some(why.to_owned()));
        }
        Ok(DunkStored::Waiting { message_id, notice }) => {
            log_dunk(
                handle,
                pane,
                &format!(
                    "not stored again, {message_id} is still unread, nothing typed ({why}); re-wake notice {notice}"
                ),
            );
            record_timer_metric(
                flock,
                handle,
                "dunk.stored",
                "waiting",
                Some(why.to_owned()),
            );
        }
        Err(error) => {
            log_dunk(
                handle,
                pane,
                &format!("could not be stored in the inbox ({error}); held, nothing typed ({why})"),
            );
            return None;
        }
    }
    Some(crate::flock::dunks::DunkList::timer_stored(handle))
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
        mailbox: crate::flock::dunks::DunkList::handle_row(handle)
            .get("target_label")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
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
    let observed_wake = Cell::new(0);
    let mut panel_seen = None;
    let mut panel_target = None;
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
        #[cfg(test)]
        cross_timer_barrier(&signal.before_fields);
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
            timer_wait(&signal, &observed_wake, remaining.min(1.0));
            continue;
        }
        let Some(pane) = fields.target_pane_id.as_deref() else {
            break;
        };
        if panel_target.as_deref() != Some(pane) {
            panel_target = Some(pane.to_owned());
            panel_seen = None;
            panel_since = now;
            gate_held = false;
        }
        // The box is read here on every tick: once it is seen empty the person's draft is
        // gone, and the pane's notices go back to behaving as on any other lane.
        if crate::flock::sends::box_is_empty(&env, pane) {
            flock.set_dunk_draft(pane, false);
        }
        // An owner lane is never typed into, so none of the gates below applies to it: not the
        // idle gate (a busy pane can still receive mail), not the typing gate (an ambiguous
        // composer, or a harness mid-turn, would hold the dunk for as long as the pane never
        // reads clear) and not the consumption check (there is no typed text of ours to wait
        // for). The dunk is stored as mail on every due tick; that is what keeps supervision of
        // the owner seat from going quiet (t4681, found live). Only the WAKE keeps its gates:
        // the one-line notice is typed by the outbox when the pane is idle and its box empty.
        if flock.is_owner_lane(pane) {
            match store_instead_of_typing(&flock, &inbox, &handle, pane, "owner lane") {
                Some(done) => {
                    save_timer_state(&flock, &handle, store.as_ref());
                    if done {
                        flock.dunks.remove_handle(&handle);
                        save_timer_state(&flock, &handle, store.as_ref());
                        break;
                    }
                    timer_wait(&signal, &observed_wake, 1.0);
                }
                None => {
                    crate::flock::dunks::DunkList::timer_hold(&handle, "inbox store failed");
                    save_timer_state(&flock, &handle, store.as_ref());
                    timer_wait(&signal, &observed_wake, 60.0);
                }
            }
            continue;
        }
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
                    crate::flock::dunks::DunkList::set_timer_status(
                        &handle,
                        &format!(
                            "Busy ({})",
                            info["agent_status"].as_str().unwrap_or("unknown")
                        ),
                    );
                    timer_wait(&signal, &observed_wake, 1.0);
                    continue;
                }
                _ => {}
            }
        }
        let settle_interrupted = Cell::new(false);
        if fields.hold_while_typing
            && !crate::flock::sends::clear_to_send(&env, pane, &|seconds| {
                let interrupted = timer_wait(&signal, &observed_wake, seconds);
                settle_interrupted.set(interrupted);
                interrupted
            })
        {
            if signal.cancelled.load(Ordering::Acquire) {
                break;
            }
            if settle_interrupted.get() {
                continue; // An edit/wake restarts the gates; it is not a typing hold.
            }
            if !gate_held {
                gate_held = true;
                crate::flock::dunks::DunkList::timer_hold(&handle, "the box would not clear");
                log_dunk(
                    &handle,
                    pane,
                    "held before typing (the pane is not clear to send)",
                );
                save_timer_state(&flock, &handle, store.as_ref());
            }
            crate::flock::dunks::DunkList::set_timer_status(&handle, "Held (typing)");
            timer_wait(&signal, &observed_wake, 60.0);
            // Returning through the idle gate is essential: clearing the box may submit a turn.
            continue;
        }
        gate_held = false;
        if fields.skip_if_unconsumed && fields.send_count > 0 {
            if let Some(text) = fields
                .last_sent_text
                .as_deref()
                .filter(|text| !text.is_empty())
            {
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
                        timer_wait(&signal, &observed_wake, 1.0);
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
                            timer_wait(&signal, &observed_wake, 1.0);
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
                (signal.cancelled.load(Ordering::Acquire)
                    || !flock.dunks.is_running(&handle)
                    || crate::flock::dunks::DunkList::timer_fields(&handle).target_pane_id
                        != fields.target_pane_id)
                    .then(|| "scheduled dunk cancelled before attempt".to_owned())
            });
        if signal.cancelled.load(Ordering::Acquire) || !flock.dunks.is_running(&handle) {
            break;
        }
        if crate::flock::dunks::DunkList::timer_fields(&handle).target_pane_id
            != fields.target_pane_id
        {
            continue;
        }
        if sent {
            flock.set_dunk_draft(pane, false);
            log_dunk(
                &handle,
                pane,
                &if ok.unwrap_or(false) {
                    "typed and submitted".to_owned()
                } else {
                    format!("typed but the submit failed ({detail})")
                },
            );
            let done =
                crate::flock::dunks::DunkList::timer_after_send(&handle, true, ok.unwrap_or(false));
            save_timer_state(&flock, &handle, store.as_ref());
            if done {
                flock.dunks.remove_handle(&handle);
                save_timer_state(&flock, &handle, store.as_ref());
                break;
            }
            timer_wait(&signal, &observed_wake, 1.0);
        } else if human_draft(blocker.as_deref()) {
            // A person's text is in the box: nothing was typed. The dunk waits in the inbox, and
            // its notice must not flush that draft either.
            if flock.set_dunk_draft(pane, true) {
                log_dunk(
                    &handle,
                    pane,
                    &format!(
                        "a person's draft is in the input box; notices to this pane wait for an empty box for up to {} minutes",
                        (crate::flock::dunks::DUNK_DRAFT_HOLD_S / 60.0) as i64
                    ),
                );
            }
            match store_instead_of_typing(
                &flock,
                &inbox,
                &handle,
                pane,
                "the input box holds a draft",
            ) {
                Some(done) => {
                    save_timer_state(&flock, &handle, store.as_ref());
                    if done {
                        flock.dunks.remove_handle(&handle);
                        save_timer_state(&flock, &handle, store.as_ref());
                        break;
                    }
                    timer_wait(&signal, &observed_wake, 1.0);
                }
                None => {
                    crate::flock::dunks::DunkList::timer_hold(&handle, "inbox store failed");
                    save_timer_state(&flock, &handle, store.as_ref());
                    timer_wait(&signal, &observed_wake, 60.0);
                }
            }
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
                log_dunk(
                    &handle,
                    pane,
                    "recovered our own text stuck in the input box",
                );
                panel_seen = None;
                panel_since = now;
                continue;
            }
            log_dunk(&handle, pane, &format!("held, nothing typed ({detail})"));
            crate::flock::dunks::DunkList::timer_hold(
                &handle,
                blocker.as_deref().unwrap_or(&detail),
            );
            record_timer_metric(&flock, &handle, "dunk.held", "held", Some(detail));
            save_timer_state(&flock, &handle, store.as_ref());
            timer_wait(&signal, &observed_wake, 60.0);
        }
    }
}
