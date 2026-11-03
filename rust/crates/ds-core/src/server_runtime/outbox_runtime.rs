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
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex, Weak,
};
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
}
impl OutboxRuntime {
    #[allow(dead_code)]
    pub(crate) fn new(
        flock: Arc<Flock>,
        runtime: Handle,
        factory: Arc<Factory>,
    ) -> Result<Self, DunkError> {
        if flock.stores().inbox.is_none() {
            return Err(DunkError::Refused("inbox store is not attached".into()));
        }
        let pool = Arc::new_cyclic(|pool| {
            OutboxWorkerPool::new(Arc::new(Runner {
                runtime,
                factory,
                pool: pool.clone(),
            }))
        });
        Ok(Self { flock, pool })
    }
    #[allow(dead_code)]
    pub(crate) fn stop_and_join(&self) {
        self.pool.stop_outbox_workers(&self.flock);
    }
}
impl Drop for OutboxRuntime {
    fn drop(&mut self) {
        self.stop_and_join();
    }
}
struct Runner {
    runtime: Handle,
    factory: Arc<Factory>,
    pool: Weak<OutboxWorkerPool>,
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
        let mut reporter = StalePanelReporter::new();
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
}
