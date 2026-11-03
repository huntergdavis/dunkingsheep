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
    pub(crate) fn new(flock: Arc<Flock>, runtime: Handle, factory: Arc<Factory>) -> Self {
        let pool = Arc::new_cyclic(|pool| {
            OutboxWorkerPool::new(Arc::new(Runner {
                runtime,
                factory,
                pool: pool.clone(),
            }))
        });
        Self { flock, pool }
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
