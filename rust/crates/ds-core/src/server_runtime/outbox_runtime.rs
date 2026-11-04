//! Ownership and worker-local services for the real outbox.
use super::{inbox_data::InboxRuntimeView, RequestRuntimeAdapter, TerminalCallAdapter};
use crate::error::DunkError;
use crate::flock::loops_stuck::StalePanelReporter;
use crate::flock::panes::PaneInfoEnv;
use crate::flock::sends::{
    outbox_worker, pane_generation, Message, OutboxWorkerPool, OutboxWorkerRunner,
    SendPreparationEnv, SnapshotStore,
};
use crate::flock::Flock;
use crate::server::RequestCtx;
use crate::terminal::Terminal;
use std::collections::HashMap;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

type Factory = dyn Fn() -> Box<dyn Terminal> + Send + Sync + 'static;

pub(super) struct FlockSnapshot;
impl SnapshotStore for FlockSnapshot {
    fn save(&self, flock: &Flock) -> Result<(), DunkError> {
        flock.save()
    }
}

#[allow(dead_code)]
pub(crate) struct OutboxRuntime {
    pub(super) flock: Arc<Flock>,
    pub(super) pool: Arc<OutboxWorkerPool>,
    dunk_timers: DunkTimerOwner,
}
impl OutboxRuntime {
    #[allow(dead_code)]
    pub(crate) fn new(
        flock: Arc<Flock>,
        runtime: Handle,
        factory: Arc<Factory>,
    ) -> Result<Self, DunkError> {
        Self::new_with_reporter(flock, runtime, factory, StalePanelReporter::new())
    }

    pub(crate) fn new_with_reporter(
        flock: Arc<Flock>,
        runtime: Handle,
        factory: Arc<Factory>,
        reporter: StalePanelReporter,
    ) -> Result<Self, DunkError> {
        if flock.stores().inbox.is_none() {
            return Err(DunkError::Refused("inbox store is not attached".into()));
        }
        let runner_runtime = runtime.clone();
        let runner_factory = Arc::clone(&factory);
        let pool = Arc::new_cyclic(|pool| {
            OutboxWorkerPool::new(Arc::new(Runner {
                runtime: runner_runtime,
                factory: runner_factory,
                pool: pool.clone(),
                reporter: reporter.clone(),
            }))
        });
        let timers = DunkTimerOwner::new(
            Arc::downgrade(&flock),
            runtime,
            factory,
            Arc::downgrade(&pool),
            reporter,
        );
        for handle in flock.dunks.running_handles() {
            timers.start(handle)?;
        }
        Ok(Self {
            flock,
            pool,
            dunk_timers: timers,
        })
    }

    pub(crate) fn start_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let handle = self.flock.start_dunk(id)?;
        if let Err(error) = self.dunk_timers.start(handle) {
            let _ = self.flock.stop_dunk(id);
            return Err(error);
        }
        self.flock.dunks.get(id)
    }

    pub(crate) fn stop_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let handle = self.flock.stop_dunk(id)?;
        self.dunk_timers.stop(&handle);
        self.flock.dunks.get(id)
    }

    pub(crate) fn toggle_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let (handle, started) = self.flock.toggle_dunk(id)?;
        if started {
            if let Err(error) = self.dunk_timers.start(Arc::clone(&handle)) {
                let _ = self.flock.stop_dunk(id);
                return Err(error);
            }
        } else {
            self.dunk_timers.stop(&handle);
        }
        self.flock.dunks.get(id)
    }

    pub(crate) fn remove_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let handle = self.flock.dunks.handle(id)?;
        self.dunk_timers.stop(&handle);
        self.flock.dunks.remove(id)
    }

    pub(crate) fn interval_changed(&self, id: &str) -> Result<(), DunkError> {
        let handle = self.flock.dunks.handle(id)?;
        self.dunk_timers.wake(&handle);
        Ok(())
    }

    pub(crate) fn add_dunk(
        &self,
        pane: Option<&serde_json::Map<String, serde_json::Value>>,
        args: &crate::flock::dunks::AddArgs,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let row = self.flock.dunks.add(pane, args)?;
        if args.start && pane.is_some() {
            let id = row
                .get("id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| DunkError::Refused("created dunk has no id".into()))?;
            let handle = self.flock.dunks.handle(id)?;
            if let Err(error) = self.dunk_timers.start(handle) {
                let _ = self.flock.dunks.remove(id);
                return Err(error);
            }
        }
        self.flock.save()?;
        Ok(row)
    }

    pub(crate) fn update_dunk(
        &self,
        id: &str,
        pane: Option<&serde_json::Map<String, serde_json::Value>>,
        args: &crate::flock::dunks::UpdateArgs,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let row = self.flock.dunks.update(id, pane, args)?;
        self.interval_changed(id)?;
        self.flock.save()?;
        Ok(row)
    }

    pub(crate) fn stop_all_dunks(
        &self,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, DunkError> {
        let rows = self.flock.dunks.list();
        for row in rows {
            if row.get("running").and_then(serde_json::Value::as_bool) == Some(true) {
                if let Some(id) = row.get("id").and_then(serde_json::Value::as_str) {
                    let handle = self.flock.dunks.handle(id)?;
                    self.dunk_timers.stop(&handle);
                }
            }
        }
        let result = self.flock.dunks.stop_all();
        self.flock.save()?;
        Ok(result)
    }

    pub(crate) fn halt_dunks_aimed_at(&self, panes: &[String]) -> Result<Vec<String>, DunkError> {
        let stopped = self.flock.dunks.halt_aimed_at(panes);
        for id in &stopped {
            if let Ok(handle) = self.flock.dunks.handle(id) {
                self.dunk_timers.stop(&handle);
            }
        }
        if !stopped.is_empty() {
            self.flock.save()?;
        }
        Ok(stopped)
    }

    pub(crate) fn stop_timer_workers(&self) {
        self.dunk_timers.stop_all();
    }

    pub(crate) fn stop_outbox_workers(&self) {
        self.pool.stop_outbox_workers(&self.flock);
    }

    #[allow(dead_code)]
    pub(crate) fn stop_and_join(&self) {
        self.stop_timer_workers();
        self.stop_outbox_workers();
    }
}
impl Drop for OutboxRuntime {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}

struct TimerSignal {
    cancelled: AtomicBool,
    generation: Mutex<u64>,
    changed: std::sync::Condvar,
}

struct TimerWorker {
    signal: Arc<TimerSignal>,
    join: JoinHandle<()>,
}

struct DunkTimerOwner {
    stopped: AtomicBool,
    flock: Weak<Flock>,
    runtime: Handle,
    factory: Arc<Factory>,
    pool: Weak<OutboxWorkerPool>,
    reporter: StalePanelReporter,
    workers: Mutex<HashMap<usize, TimerWorker>>,
}

impl DunkTimerOwner {
    fn new(
        flock: Weak<Flock>,
        runtime: Handle,
        factory: Arc<Factory>,
        pool: Weak<OutboxWorkerPool>,
        reporter: StalePanelReporter,
    ) -> Self {
        Self {
            stopped: AtomicBool::new(false),
            flock,
            runtime,
            factory,
            pool,
            reporter,
            workers: Mutex::new(HashMap::new()),
        }
    }

    fn key(handle: &crate::flock::dunks::DunkerHandle) -> usize {
        Arc::as_ptr(handle) as usize
    }

    fn start(&self, handle: crate::flock::dunks::DunkerHandle) -> Result<(), DunkError> {
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
                run_dunk_timer(
                    flock,
                    runtime,
                    factory,
                    handle,
                    thread_signal,
                    pool,
                    reporter,
                )
            });
        let join = join
            .map_err(|error| DunkError::Refused(format!("could not start dunk timer: {error}")))?;
        workers.insert(key, TimerWorker { signal, join });
        Ok(())
    }

    fn wake(&self, handle: &crate::flock::dunks::DunkerHandle) {
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

    fn stop(&self, handle: &crate::flock::dunks::DunkerHandle) {
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

    fn stop_all(&self) {
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
                    save_timer_state(&flock, &handle, store.as_ref());
                    timer_wait(&signal, 1.0);
                    continue;
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
        if fields.skip_if_unconsumed && fields.awaiting_consumption && fields.send_count > 0 {
            if let Some(text) = fields.last_sent_text.as_deref() {
                match crate::flock::sends::box_holds(&env, pane, text) {
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

struct Runner {
    runtime: Handle,
    factory: Arc<Factory>,
    pool: Weak<OutboxWorkerPool>,
    reporter: StalePanelReporter,
}
impl OutboxWorkerRunner for Runner {
    fn run(&self, flock: Arc<Flock>, pane_id: &str, stopped: &AtomicBool) {
        let Some(pool) = self.pool.upgrade() else {
            return;
        };
        let terminal = Mutex::new((self.factory)());
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, &self.runtime, &terminal);
        let Ok(inbox) = InboxRuntimeView::new(adapter, Arc::clone(&flock), pool) else {
            eprintln!("outbox worker: inbox services unavailable");
            return;
        };
        let mut send = TerminalCallAdapter {
            flock: &flock,
            terminal: &terminal,
            runtime: &self.runtime,
        };
        let mut reporter = self.reporter.clone();
        let wait = |seconds: f64| {
            let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));
            while !stopped.load(Ordering::Acquire) {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return false;
                }
                std::thread::sleep(remaining.min(Duration::from_millis(100)));
            }
            true
        };
        outbox_worker(
            &flock,
            &mut send,
            &inbox,
            pane_id,
            stopped,
            &mut reporter,
            &wait,
            || None,
        );
    }
}
impl SendPreparationEnv for TerminalCallAdapter<'_> {
    fn send_text(&mut self, pane: &str, text: &str) -> (bool, String) {
        match self.with_terminal_mut(|terminal| terminal.send_text(pane, text)) {
            Ok(()) => (true, "typed".into()),
            Err(error) => (false, error.to_string()),
        }
    }
    fn attempt_generation(&self, pane: &str) -> Option<String> {
        pane_generation(self, pane)
    }
    fn persist_typed_attempt(
        &mut self,
        message: Option<&mut Message>,
        pane: &str,
        text: &str,
        press: bool,
        generation: Option<&str>,
    ) -> bool {
        let Some(message) = message else { return true };
        if message.attempt_generation.as_deref() != generation {
            return false;
        }
        let pane_record = PaneInfoEnv::get_pane(self, pane).unwrap_or(serde_json::Value::Null);
        self.flock
            .persist_typed_attempt(message, pane, text, press, &pane_record)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flock::dunks::{AddArgs, UpdateArgs};
    use crate::terminal::fakes::FakeTerminal;

    #[test]
    fn typed_attempt_without_frame_succeeds_but_snapshot_failure_refuses() {
        let dir = tempfile::tempdir().unwrap();
        // Persistence is enabled but no status state is attached, so the real save fails.
        let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), true);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let mut env = TerminalCallAdapter {
            flock: &flock,
            terminal: &terminal,
            runtime: runtime.handle(),
        };
        assert!(env.persist_typed_attempt(None, "w1:p1", "hello", true, None));
        let mut message = Message::new("m1", "w1:p1", "lane", "hello", "test");
        message.attempt_generation = Some("captured".into());
        assert!(!env.persist_typed_attempt(
            Some(&mut message),
            "w1:p1",
            "hello",
            true,
            Some("captured")
        ));
        assert_eq!(message.status, "queued");
    }

    #[test]
    fn runtime_shutdown_is_idempotent_and_releases_pool() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        flock.attach_stores(crate::flock::panes::Stores {
            inbox: Some(Arc::new(
                crate::inbox_store::Store::open(
                    dir.path().join("inbox.db"),
                    dir.path().join("blobs"),
                )
                .unwrap(),
            )),
            ..Default::default()
        });
        let owner = OutboxRuntime::new(
            flock,
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
        )
        .unwrap();
        let weak = Arc::downgrade(&owner.pool);
        owner.stop_and_join();
        owner.stop_and_join();
        drop(owner);
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn real_worker_drains_fifo_and_joins() {
        use crate::flock::panes::Stores;
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        let store = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        flock.attach_stores(Stores {
            inbox: Some(store),
            ..Stores::default()
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let created = Arc::new(Mutex::new(Vec::new()));
        let factory_threads = Arc::clone(&created);
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(move || {
                factory_threads
                    .lock()
                    .unwrap()
                    .push(std::thread::current().id());
                Box::new(FakeTerminal::new())
            }),
        )
        .unwrap();
        let mut first = Message::new("m1", "w1:p1", "lane", "first", "test");
        let mut second = Message::new("m2", "w1:p1", "lane", "second", "test");
        for message in [&mut first, &mut second] {
            message.hold_while_typing = false;
            message.press_enter = false;
        }
        flock
            .enqueue(first.clone(), owner.pool.as_ref(), &FlockSnapshot)
            .unwrap();
        flock
            .enqueue(second.clone(), owner.pool.as_ref(), &FlockSnapshot)
            .unwrap();
        assert!(
            first.done.wait_timeout(Duration::from_secs(5)),
            "first message did not complete"
        );
        assert!(
            second.done.wait_timeout(Duration::from_secs(5)),
            "second message did not complete"
        );
        owner.stop_and_join();
        let history = flock.message_history.lock().unwrap();
        let completed: Vec<_> = history
            .iter()
            .map(|message| (message.id.as_str(), message.status.as_str()))
            .collect();
        assert_eq!(completed, [("m1", "sent"), ("m2", "sent")]);
        let threads = created.lock().unwrap();
        assert!(!threads.is_empty());
        assert!(threads.iter().all(|id| *id != std::thread::current().id()));
    }
    #[test]
    fn blocked_pane_does_not_block_another_worker() {
        use crate::flock::panes::Stores;
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        let store = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        flock.attach_stores(Stores {
            inbox: Some(store),
            ..Stores::default()
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
        )
        .unwrap();
        let mut blocked = Message::new("m1", "w1:p1", "one", "first", "test");
        let mut independent = Message::new("m2", "w2:p2", "two", "second", "test");
        for message in [&mut blocked, &mut independent] {
            message.hold_while_typing = false;
            message.press_enter = false;
        }
        let lock = flock.send_lock_for("w1:p1");
        let guard = lock.lock().unwrap();
        flock
            .enqueue(blocked.clone(), owner.pool.as_ref(), &FlockSnapshot)
            .unwrap();
        flock
            .enqueue(independent.clone(), owner.pool.as_ref(), &FlockSnapshot)
            .unwrap();
        let progressed = independent.done.wait_timeout(Duration::from_secs(5));
        let still_blocked = !blocked.done.is_set();
        drop(guard);
        assert!(progressed, "independent pane failed to progress");
        assert!(still_blocked, "blocked pane unexpectedly completed");
        assert!(blocked.done.wait_timeout(Duration::from_secs(5)));
        owner.stop_and_join();
    }
    #[test]
    fn shutdown_joins_waiting_worker_without_dropping_message() {
        use crate::flock::panes::Stores;
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        let store = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        flock.attach_stores(Stores {
            inbox: Some(store),
            ..Stores::default()
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(move || {
                let mut fake = FakeTerminal::new();
                fake.screens
                    .insert("w1:p1".into(), "❯ a human draft".into());
                let _ = started_tx.send(());
                Box::new(fake)
            }),
        )
        .unwrap();
        let message = Message::new("m1", "w1:p1", "lane", "message", "test");
        flock
            .enqueue(message.clone(), owner.pool.as_ref(), &FlockSnapshot)
            .unwrap();
        started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let start = Instant::now();
        owner.stop_and_join();
        assert!(start.elapsed() < Duration::from_secs(3));
        assert!(!message.done.is_set());
        assert_eq!(
            crate::flock::sends::outbox_lock(&flock.outbox)["w1:p1"][0].id,
            "m1"
        );
    }

    #[test]
    fn scheduled_timer_sends_through_dunk_action_and_removes_at_max_sends() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        flock.attach_stores(crate::flock::panes::Stores {
            inbox: Some(Arc::new(
                crate::inbox_store::Store::open(
                    dir.path().join("inbox.db"),
                    dir.path().join("blobs"),
                )
                .unwrap(),
            )),
            ..Default::default()
        });
        let pane = serde_json::json!({
            "pane_id": "w1:p1", "tab_label": "1", "workspace_label": "work",
            "agent": "codex", "cwd": "/tmp"
        });
        let args = AddArgs {
            text: Some("scheduled {count}".into()),
            interval: Some("0.001".into()),
            start: true,
            max_sends: Some("1".into()),
            hold_while_typing: Some("false".into()),
            ..AddArgs::default()
        };
        flock.dunks.add(pane.as_object(), &args).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
        )
        .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline && flock.dunks.get("d1").is_ok() {
            thread::sleep(Duration::from_millis(20));
        }
        assert!(
            flock.dunks.get("d1").is_err(),
            "scheduled max_sends removes its row"
        );
        owner.stop_and_join();
    }

    #[test]
    fn interval_update_wakes_the_captured_timer_and_stop_joins_it() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        flock.attach_stores(crate::flock::panes::Stores {
            inbox: Some(Arc::new(
                crate::inbox_store::Store::open(
                    dir.path().join("inbox.db"),
                    dir.path().join("blobs"),
                )
                .unwrap(),
            )),
            ..Default::default()
        });
        let pane = serde_json::json!({"pane_id": "w1:p1"});
        let args = AddArgs {
            interval: Some("10".into()),
            start: true,
            hold_while_typing: Some("false".into()),
            ..AddArgs::default()
        };
        flock.dunks.add(pane.as_object(), &args).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
        )
        .unwrap();
        let before =
            crate::flock::dunks::DunkList::timer_fields(&flock.dunks.handle("d1").unwrap())
                .next_send_at
                .unwrap();
        owner
            .update_dunk(
                "d1",
                None,
                &UpdateArgs {
                    interval: Some("0.001".into()),
                    ..UpdateArgs::default()
                },
            )
            .unwrap();
        let after = crate::flock::dunks::DunkList::timer_fields(&flock.dunks.handle("d1").unwrap())
            .next_send_at
            .unwrap();
        assert!(after < before);
        owner.stop_dunk("d1").unwrap();
        assert!(owner.dunk_timers.workers.lock().unwrap().is_empty());
        owner.stop_and_join();
        let error = owner.start_dunk("d1").unwrap_err();
        assert!(error.to_string().contains("dunk timer runtime is stopped"));
        assert!(owner.dunk_timers.workers.lock().unwrap().is_empty());
    }

    #[test]
    fn scheduled_idle_gate_holds_until_policy_changes_then_delivers() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        flock.attach_stores(crate::flock::panes::Stores {
            inbox: Some(Arc::new(
                crate::inbox_store::Store::open(
                    dir.path().join("inbox.db"),
                    dir.path().join("blobs"),
                )
                .unwrap(),
            )),
            ..Default::default()
        });
        let pane = serde_json::json!({"pane_id": "w1:p1"});
        flock
            .dunks
            .add(
                pane.as_object(),
                &AddArgs {
                    text: Some("idle gate delivery".into()),
                    interval: Some("0.001".into()),
                    start: true,
                    only_when: Some("idle".into()),
                    max_sends: Some("1".into()),
                    hold_while_typing: Some("false".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(|| {
                let mut fake = FakeTerminal::new();
                fake.statuses.insert("w1:p1".into(), "working".into());
                Box::new(fake)
            }),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let row = flock
                .dunks
                .get("d1")
                .expect("busy timer must retain its row");
            assert_eq!(row["send_count"], serde_json::json!(0));
            if row["status"] == "Busy" {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timer never observed busy status"
            );
            thread::sleep(Duration::from_millis(10));
        }
        owner
            .update_dunk(
                "d1",
                None,
                &UpdateArgs {
                    only_when: Some(None),
                    ..Default::default()
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while flock.dunks.get("d1").is_ok() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert!(
            flock.dunks.get("d1").is_err(),
            "released timer must actually deliver and reach max_sends"
        );
        owner.stop_and_join();
    }
}
