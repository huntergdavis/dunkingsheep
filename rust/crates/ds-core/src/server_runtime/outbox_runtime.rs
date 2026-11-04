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
#[cfg(test)]
use std::thread;
use std::time::{Duration, Instant};
use tokio::runtime::Handle;

mod dunk_timers;
use dunk_timers::DunkTimerOwner;
mod manual_fire;
use manual_fire::ManualFireOwner;
#[cfg(test)]
pub(crate) use manual_fire::{CountdownGate, SubmitGate};

type Factory = dyn Fn() -> Box<dyn Terminal> + Send + Sync + 'static;

pub(super) struct FlockSnapshot;
impl SnapshotStore for FlockSnapshot {
    fn save(&self, flock: &Flock) -> Result<(), DunkError> {
        flock.save()
    }

    fn save_classified(
        &self,
        flock: &Flock,
    ) -> Result<(), crate::flock::sends::SnapshotSaveFailure> {
        flock.save_classified()
    }
}

#[allow(dead_code)]
pub(crate) struct OutboxRuntime {
    pub(super) flock: Arc<Flock>,
    pub(super) pool: Arc<OutboxWorkerPool>,
    dunk_timers: DunkTimerOwner,
    manual_fire: ManualFireOwner,
    // Serializes row and worker-lifetime transitions; timer threads never acquire it.
    dunk_lifecycle: Mutex<()>,
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
            runtime.clone(),
            Arc::clone(&factory),
            Arc::downgrade(&pool),
            reporter,
        );
        let manual_fire = ManualFireOwner::new(
            Arc::downgrade(&flock),
            runtime,
            factory,
            Arc::downgrade(&pool),
        );
        for handle in flock.dunks.running_handles() {
            timers.start(handle)?;
        }
        Ok(Self {
            flock,
            pool,
            dunk_timers: timers,
            manual_fire,
            dunk_lifecycle: Mutex::new(()),
        })
    }

    pub(crate) fn start_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let _lifecycle = self
            .dunk_lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (handle, should_start_worker) = self
            .flock
            .start_dunk(id, |handle| self.dunk_timers.has_live_worker(handle))?;
        if should_start_worker {
            if let Err(error) = self.dunk_timers.start(Arc::clone(&handle)) {
                crate::flock::dunks::DunkList::stop_handle(&handle, "Stopped");
                return Err(error);
            }
        }
        Ok(crate::flock::dunks::DunkList::handle_row(&handle))
    }

    pub(crate) fn stop_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let _lifecycle = self
            .dunk_lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = self.flock.stop_dunk(id)?;
        self.dunk_timers.stop(&handle);
        Ok(crate::flock::dunks::DunkList::handle_row(&handle))
    }

    pub(crate) fn toggle_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let _lifecycle = self
            .dunk_lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (handle, started) = self.flock.toggle_dunk(id)?;
        if started {
            if let Err(error) = self.dunk_timers.start(Arc::clone(&handle)) {
                crate::flock::dunks::DunkList::stop_handle(&handle, "Stopped");
                return Err(error);
            }
        } else {
            self.dunk_timers.stop(&handle);
        }
        Ok(crate::flock::dunks::DunkList::handle_row(&handle))
    }

    pub(crate) fn remove_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let _lifecycle = self
            .dunk_lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = self.flock.dunks.handle(id)?;
        self.dunk_timers.stop(&handle);
        self.flock.dunks.remove_handle(&handle);
        Ok(crate::flock::dunks::DunkList::handle_row(&handle))
    }

    #[cfg(test)]
    pub(crate) fn dunk_timer_generation(&self, id: &str) -> Result<Option<u64>, DunkError> {
        let handle = self.flock.dunks.handle(id)?;
        let workers = self
            .dunk_timers
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Ok(workers.get(&DunkTimerOwner::key(&handle)).map(|worker| {
            *worker
                .signal
                .generation
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        }))
    }

    #[cfg(test)]
    pub(crate) fn dunk_timer_worker_count(&self) -> usize {
        self.dunk_timers
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    #[cfg(test)]
    pub(crate) fn stop_dunk_timer_worker_only(&self, id: &str) -> Result<(), DunkError> {
        let _lifecycle = self
            .dunk_lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = self.flock.dunks.handle(id)?;
        self.dunk_timers.stop(&handle);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn install_finished_dunk_timer_worker(&self, id: &str) -> Result<(), DunkError> {
        let _lifecycle = self
            .dunk_lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let handle = self.flock.dunks.handle(id)?;
        self.dunk_timers.stop(&handle);
        let signal = Arc::new(dunk_timers::TimerSignal {
            precommit: Arc::clone(&self.dunk_timers.precommit),
            before_fields: Arc::clone(&self.dunk_timers.before_fields),
            before_hold_wait: Arc::clone(&self.dunk_timers.before_hold_wait),
            cancelled: AtomicBool::new(false),
            generation: Mutex::new(0),
            changed: std::sync::Condvar::new(),
        });
        let join = thread::spawn(|| {});
        let deadline = Instant::now() + Duration::from_secs(5);
        while !join.is_finished() {
            assert!(
                Instant::now() < deadline,
                "finished worker fixture did not exit"
            );
            thread::yield_now();
        }
        self.dunk_timers
            .workers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                DunkTimerOwner::key(&handle),
                dunk_timers::TimerWorker { signal, join },
            );
        Ok(())
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
        let row = {
            let _lifecycle = self
                .dunk_lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let row = self.flock.dunks.add(pane, args)?;
            if args.start && pane.is_some() {
                let id = row
                    .get("id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| DunkError::Refused("created dunk has no id".into()))?;
                let handle = self.flock.dunks.handle(id)?;
                if let Err(error) = self.dunk_timers.start(Arc::clone(&handle)) {
                    crate::flock::dunks::DunkList::set_timer_error(&handle, &error.to_string());
                    return Err(error);
                }
            }
            row
        };
        self.flock.save()?;
        Ok(row)
    }

    pub(crate) fn update_dunk(
        &self,
        id: &str,
        pane: Option<&serde_json::Map<String, serde_json::Value>>,
        args: &crate::flock::dunks::UpdateArgs,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let (row, changed) = self.flock.dunks.update_with_change(id, pane, args)?;
        if changed {
            self.interval_changed(id)?;
            self.flock.save()?;
        }
        Ok(row)
    }

    pub(crate) fn stop_all_dunks(
        &self,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, DunkError> {
        let result = {
            let _lifecycle = self
                .dunk_lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = self.flock.dunks.halt_handles(None);
            for handle in &handles {
                self.dunk_timers.stop(handle);
            }
            self.flock.dunks.list()
        };
        match self.flock.save_classified() {
            Ok(()) | Err(crate::flock::sends::SnapshotSaveFailure::Io(_)) => {}
            Err(crate::flock::sends::SnapshotSaveFailure::Other(error)) => return Err(error),
        }
        Ok(result)
    }

    pub(crate) fn fire_dunk(
        &self,
        id: &str,
        countdown_s: i64,
        wait: bool,
    ) -> Result<serde_json::Value, DunkError> {
        self.manual_fire.fire(id, countdown_s, wait)
    }

    #[cfg(test)]
    pub(crate) fn set_manual_fire_test_gates(
        &self,
        countdown: Option<Arc<manual_fire::CountdownGate>>,
        submit: Option<Arc<manual_fire::SubmitGate>>,
    ) {
        self.manual_fire.set_countdown_gate(countdown);
        self.manual_fire.set_submit_gate(submit);
    }

    #[cfg(test)]
    pub(crate) fn fail_next_manual_fire_spawn(&self) {
        self.manual_fire.fail_next_spawn();
    }

    #[cfg(test)]
    pub(crate) fn manual_fire_is_stopped(&self) -> bool {
        self.manual_fire.is_stopped()
    }

    #[cfg(test)]
    pub(crate) fn completed_manual_fires(&self) -> Vec<(String, String)> {
        self.manual_fire.completed()
    }

    pub(crate) fn halt_dunks_aimed_at(&self, panes: &[String]) -> Result<Vec<String>, DunkError> {
        let stopped = {
            let _lifecycle = self
                .dunk_lifecycle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let handles = self.flock.dunks.halt_handles(Some(panes));
            let mut stopped = Vec::new();
            for handle in &handles {
                self.dunk_timers.stop(handle);
                stopped.push(crate::flock::dunks::DunkList::timer_fields(handle).id);
            }
            stopped
        };
        if !stopped.is_empty() {
            self.flock.save()?;
        }
        Ok(stopped)
    }

    pub(crate) fn stop_timer_workers(&self) {
        let _lifecycle = self
            .dunk_lifecycle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.dunk_timers.stop_all();
    }

    pub(crate) fn stop_manual_fire_workers(&self) {
        self.manual_fire.stop_and_join();
    }

    pub(crate) fn stop_outbox_workers(&self) {
        self.pool.stop_outbox_workers(&self.flock);
    }

    #[allow(dead_code)]
    pub(crate) fn stop_and_join(&self) {
        self.stop_manual_fire_workers();
        self.stop_timer_workers();
        self.stop_outbox_workers();
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

    // Worker-local facade with externally controlled state and recorded terminal I/O.
    #[derive(Debug)]
    struct SharedTerminal(Arc<Mutex<FakeTerminal>>, usize);
    use crate::terminal::{AgentSpec, Pane, Resize, Tab, TerminalError, Workspace};
    use std::collections::BTreeMap;
    impl Terminal for SharedTerminal {
        fn version(&self) -> String {
            self.0.lock().unwrap().version()
        }
        fn is_available(&self) -> bool {
            self.0.lock().unwrap().is_available()
        }
        fn server_error_hint(&self) -> String {
            self.0.lock().unwrap().server_error_hint()
        }
        fn list_panes(&self) -> Vec<Pane> {
            self.0.lock().unwrap().list_panes()
        }
        fn get_pane(&self, pane_id: &str) -> Option<Pane> {
            self.0.lock().unwrap().get_pane(pane_id)
        }
        fn list_tabs(&self) -> Vec<Tab> {
            self.0.lock().unwrap().list_tabs()
        }
        fn list_workspaces(&self) -> Vec<Workspace> {
            self.0.lock().unwrap().list_workspaces()
        }
        fn list_panes_grouped(&self) -> BTreeMap<String, Vec<Pane>> {
            self.0.lock().unwrap().list_panes_grouped()
        }
        fn agent_child_pids(&self, pane_pid: i64) -> Result<Vec<i64>, TerminalError> {
            self.0.lock().unwrap().agent_child_pids(pane_pid)
        }
        fn create_workspace(
            &mut self,
            label: Option<&str>,
            cwd: Option<&str>,
            focus: bool,
        ) -> Result<Workspace, TerminalError> {
            self.0.lock().unwrap().create_workspace(label, cwd, focus)
        }
        fn create_tab(
            &mut self,
            workspace_id: Option<&str>,
            label: Option<&str>,
            cwd: Option<&str>,
            focus: bool,
        ) -> Result<Tab, TerminalError> {
            self.0
                .lock()
                .unwrap()
                .create_tab(workspace_id, label, cwd, focus)
        }
        fn split_pane(
            &mut self,
            pane_id: &str,
            direction: &str,
            cwd: Option<&str>,
        ) -> Result<Pane, TerminalError> {
            self.0.lock().unwrap().split_pane(pane_id, direction, cwd)
        }
        fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError> {
            self.0.lock().unwrap().start_agent(spec)
        }
        fn rename(
            &mut self,
            kind: &str,
            target_id: &str,
            label: &str,
        ) -> Result<(), TerminalError> {
            self.0.lock().unwrap().rename(kind, target_id, label)
        }
        fn close(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
            self.0.lock().unwrap().close(kind, target_id)
        }
        fn focus(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
            self.0.lock().unwrap().focus(kind, target_id)
        }
        fn run_command(&mut self, pane_id: &str, command: &str) -> Result<(), TerminalError> {
            self.0.lock().unwrap().run_command(pane_id, command)
        }
        fn send_text(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
            self.0.lock().unwrap().send_text(pane_id, text)
        }
        fn send_text_and_enter(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
            self.0.lock().unwrap().send_text_and_enter(pane_id, text)
        }
        fn send_keys(&mut self, pane_id: &str, keys: &[&str]) -> Result<(), TerminalError> {
            let mut fake = self.0.lock().unwrap();
            fake.send_keys(pane_id, keys)?;
            if self.1 > 0 && fake.keys_sent.len() >= self.1 {
                fake.screens.insert(
                    pane_id.into(),
                    recovery_frame("\x1b[2mAsk Codex to do anything\x1b[0m"),
                );
            }
            Ok(())
        }
        fn read_pane(
            &self,
            pane_id: &str,
            lines: i64,
            ansi: bool,
        ) -> Result<Option<String>, TerminalError> {
            self.0.lock().unwrap().read_pane(pane_id, lines, ansi)
        }
        fn agent_status(&self, pane_id: &str) -> Option<String> {
            self.0.lock().unwrap().agent_status(pane_id)
        }
        fn resize(&mut self, pane_id: &str, cols: i64, rows: i64) -> Result<Resize, TerminalError> {
            self.0.lock().unwrap().resize(pane_id, cols, rows)
        }
    }

    fn recovery_frame(body: &str) -> String {
        format!(
            "synthetic transcript\n{}› {body}\n  model · /tmp/synthetic-fixture · task\n",
            "\n".repeat(17)
        )
    }

    #[test]
    fn real_timer_stale_modal_reports_once_without_submitting_dialog() {
        // Isolate environment-based reporter sinks in a child test process. The owner command
        // cannot execute, and all notification storage and terminal I/O stay in this fixture.
        if std::env::var_os("DS_TIMER_MODAL_FIXTURE").is_none() {
            let dir = tempfile::tempdir().unwrap();
            let output = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "server_runtime::outbox_runtime::tests::real_timer_stale_modal_reports_once_without_submitting_dialog", "--nocapture"])
                .env("DS_TIMER_MODAL_FIXTURE", "1")
                .env("DUNKINGSHEEP_DIR", dir.path())
                .env("DUNKINGSHEEP_VPCTL", dir.path().join("absent-owner-command"))
                .env("DUNKINGSHEEP_VP_PANE", "fixture-missing-vp")
                .output().unwrap();
            assert!(
                output.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        let metrics =
            Arc::new(crate::metrics::MetricsStore::open(dir.path().join("metrics.db")).unwrap());
        flock.attach_stores(crate::flock::panes::Stores {
            inbox: Some(Arc::new(
                crate::inbox_store::Store::open(
                    dir.path().join("inbox.db"),
                    dir.path().join("blobs"),
                )
                .unwrap(),
            )),
            metrics: Some(Arc::clone(&metrics)),
            ..Default::default()
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let terminal = Arc::new(Mutex::new(FakeTerminal::new()));
        {
            let mut fake = terminal.lock().unwrap();
            fake.panes[0].pane_pid = Some(std::process::id() as i64);
            fake.screens.insert(
                "w1:p1".into(),
                "Press Enter to confirm or Esc to cancel".into(),
            );
        }
        let worker_terminal = Arc::clone(&terminal);
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(move || Box::new(SharedTerminal(Arc::clone(&worker_terminal), 0))),
        )
        .unwrap();
        let pane = serde_json::json!({"pane_id": "w1:p1"});
        owner
            .add_dunk(
                pane.as_object(),
                &AddArgs {
                    start: true,
                    interval: Some("0.001".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(45);
        let mut first_hold = None;
        loop {
            let row = flock.dunks.get("d1").unwrap();
            assert_eq!(serde_json::json!(0), row["send_count"]);
            if row["hold_count"].as_i64().unwrap() > 0 {
                let since = first_hold.get_or_insert_with(Instant::now);
                if since.elapsed().as_secs_f64() > crate::flock::loops_stuck::MODAL_WAIT_S + 1.0 {
                    owner.interval_changed("d1").unwrap();
                    thread::sleep(Duration::from_secs(1));
                }
            }
            let reports = metrics
                .events_between(0.0, crate::clock::now_s() + 1.0)
                .unwrap()
                .into_iter()
                .filter(|event| event.kind == "panel.blocking")
                .count();
            if reports > 0 {
                assert_eq!(1, reports);
                break;
            }
            assert!(Instant::now() < deadline, "stable modal was not reported");
            thread::sleep(Duration::from_millis(20));
        }
        owner.interval_changed("d1").unwrap();
        thread::sleep(Duration::from_secs(1));
        owner.stop_and_join();
        let reports = metrics
            .events_between(0.0, crate::clock::now_s() + 1.0)
            .unwrap()
            .into_iter()
            .filter(|event| event.kind == "panel.blocking")
            .count();
        assert_eq!(1, reports);
        let fake = terminal.lock().unwrap();
        assert!(fake.keys_sent.is_empty());
        assert!(fake.sends.is_empty());
        assert!(fake.submits.is_empty());
    }

    #[test]
    fn real_timer_recovers_owned_stranded_text_before_scheduled_delivery() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            true,
        ));
        flock
            .attach_status_state(crate::flock::status_runtime::StatusState::at_start(
                "test",
                dir.path().join("state.json"),
                std::env::current_exe().unwrap(),
                crate::config::Env::default(),
            ))
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
        {
            let mut config = flock.stuck_config.lock().unwrap();
            config.threshold_s = 0.01;
            config.observation_interval_s = 0.0;
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let terminal = Arc::new(Mutex::new(FakeTerminal::new()));
        {
            let mut fake = terminal.lock().unwrap();
            fake.panes[0].pane_pid = Some(std::process::id() as i64);
            fake.statuses.insert("w1:p1".into(), "idle".into());
            fake.screens
                .insert("w1:p1".into(), recovery_frame("stuck body"));
        }
        let worker_terminal = Arc::clone(&terminal);
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(move || Box::new(SharedTerminal(Arc::clone(&worker_terminal), 3))),
        )
        .unwrap();
        let adapter_terminal: Mutex<Box<dyn Terminal>> =
            Mutex::new(Box::new(SharedTerminal(Arc::clone(&terminal), 0)));
        let env = TerminalCallAdapter {
            flock: &flock,
            terminal: &adapter_terminal,
            runtime: runtime.handle(),
        };
        let mut message = Message::new("stranded", "w1:p1", "writer", "stuck body", "send_text");
        message.created_at = 1.0;
        message.status = "failed".into();
        message.attempt_generation = pane_generation(&env, "w1:p1");
        assert!(message.attempt_generation.is_some());
        flock
            .sent_text_by_pane
            .lock()
            .unwrap()
            .insert("w1:p1".into(), vec!["stuck body".into()]);
        flock.note_stuck_candidate(&message);
        let (held_tx, held_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *owner.dunk_timers.before_hold_wait.lock().unwrap() = Some((held_tx, release_rx));
        let pane = serde_json::json!({"pane_id": "w1:p1"});
        owner
            .add_dunk(
                pane.as_object(),
                &AddArgs {
                    start: true,
                    interval: Some("0.001".into()),
                    text: Some("next scheduled".into()),
                    max_sends: Some("1".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        held_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Force the wake to happen after hold accounting but BEFORE the condvar wait.
        owner.interval_changed("d1").unwrap();
        release_tx.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut observed_holds = 1;
        loop {
            let recovered = flock
                .stuck_candidates
                .lock()
                .unwrap()
                .get("w1:p1")
                .is_some_and(|entry| entry.recovery_outcome.as_deref() == Some("consumed"));
            if recovered {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timer did not recover owned composer: {:?}",
                flock.dunks.get("d1")
            );
            let holds = flock.dunks.get("d1").unwrap()["hold_count"]
                .as_i64()
                .unwrap();
            if holds > observed_holds {
                observed_holds = holds;
                owner.interval_changed("d1").unwrap();
            }
            thread::sleep(Duration::from_millis(10));
        }
        owner.stop_and_join();
        assert!(terminal.lock().unwrap().keys_sent.len() >= 3);
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(dir.path().join("state.json")).unwrap()).unwrap();
        assert!(persisted.is_object());
    }

    #[test]
    fn scheduled_worker_snapshot_failure_never_counts_a_successful_send() {
        let dir = tempfile::tempdir().unwrap();
        // Enabled persistence without a status-state path makes the real snapshot fail.
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            true,
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
                    start: true,
                    interval: Some("0.001".into()),
                    hold_while_typing: Some("false".into()),
                    max_sends: Some("1".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let terminal = Arc::new(Mutex::new(FakeTerminal::new()));
        let worker_terminal = Arc::clone(&terminal);
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(move || Box::new(SharedTerminal(Arc::clone(&worker_terminal), 0))),
        )
        .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let row = flock
                .dunks
                .get("d1")
                .expect("failed attempt must not reach max_sends");
            assert_eq!(serde_json::json!(0), row["send_count"]);
            if row["status"] == "Error" && row["last_error"].is_string() {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "snapshot failure was not surfaced"
            );
            thread::sleep(Duration::from_millis(10));
        }
        owner.stop_dunk("d1").unwrap();
        assert!(owner.dunk_timers.workers.lock().unwrap().is_empty());
        owner.stop_and_join();
        let recorded = terminal.lock().unwrap();
        assert!(recorded.submits.is_empty());
        assert!(
            recorded.keys_sent.is_empty(),
            "snapshot failure must precede Enter"
        );
    }

    #[test]
    fn real_timer_idle_hold_idle_recheck_then_delivery() {
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
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let terminal = Arc::new(Mutex::new(FakeTerminal::new()));
        terminal
            .lock()
            .unwrap()
            .statuses
            .insert("w1:p1".into(), "working".into());
        let worker_terminal = Arc::clone(&terminal);
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(move || Box::new(SharedTerminal(Arc::clone(&worker_terminal), 0))),
        )
        .unwrap();
        let pane = serde_json::json!({"pane_id": "w1:p1"});
        owner
            .add_dunk(
                pane.as_object(),
                &AddArgs {
                    start: true,
                    interval: Some("0.001".into()),
                    only_when: Some("idle".into()),
                    max_sends: Some("1".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let wait_status = |status: &str| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let row = flock
                    .dunks
                    .get("d1")
                    .expect("must not send before gates clear");
                assert_eq!(serde_json::json!(0), row["send_count"]);
                if row["status"] == status {
                    break;
                }
                assert!(Instant::now() < deadline, "did not reach {status}: {row:?}");
                thread::sleep(Duration::from_millis(10));
            }
        };
        wait_status("Busy (working)");
        {
            let mut fake = terminal.lock().unwrap();
            fake.statuses.insert("w1:p1".into(), "ready".into());
            fake.screens.insert(
                "w1:p1".into(),
                "Press Enter to confirm or Esc to cancel".into(),
            );
        }
        owner.interval_changed("d1").unwrap();
        wait_status("Held (typing)");
        {
            let mut fake = terminal.lock().unwrap();
            fake.screens.clear();
            fake.statuses.insert("w1:p1".into(), "working".into());
        }
        owner.interval_changed("d1").unwrap();
        wait_status("Busy (working)");
        assert_eq!(
            serde_json::json!(1),
            flock.dunks.get("d1").unwrap()["hold_count"]
        );
        assert!(terminal.lock().unwrap().keys_sent.is_empty());
        terminal
            .lock()
            .unwrap()
            .statuses
            .insert("w1:p1".into(), "ready".into());
        owner.interval_changed("d1").unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while flock.dunks.get("d1").is_ok() {
            assert!(
                Instant::now() < deadline,
                "cleared timer did not deliver/remove"
            );
            thread::sleep(Duration::from_millis(10));
        }
        owner.stop_and_join();
        let fake = terminal.lock().unwrap();
        assert!(!fake.sends.is_empty() || !fake.submits.is_empty());
        assert!(!fake.keys_sent.is_empty() || !fake.submits.is_empty());
    }

    #[test]
    fn held_timer_wake_rechecks_idle_before_another_attempt() {
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
                fake.screens.insert(
                    "w1:p1".into(),
                    "Press Enter to confirm or Esc to cancel".into(),
                );
                Box::new(fake)
            }),
        )
        .unwrap();
        let pane = serde_json::json!({"pane_id": "w1:p1"});
        owner
            .add_dunk(
                pane.as_object(),
                &AddArgs {
                    start: true,
                    interval: Some("0.001".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let row = flock.dunks.get("d1").unwrap();
            if row["hold_count"].as_i64().unwrap() > 0 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "timer did not enter guarded hold"
            );
            thread::sleep(Duration::from_millis(10));
        }
        owner
            .update_dunk(
                "d1",
                None,
                &UpdateArgs {
                    only_when: Some(Some("idle".into())),
                    ..Default::default()
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let row = flock.dunks.get("d1").unwrap();
            assert_eq!(serde_json::json!(0), row["send_count"]);
            if row["status"] == "Busy (working)" {
                assert_eq!(serde_json::json!(1), row["hold_count"]);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "held timer did not recheck idle after wake"
            );
            thread::sleep(Duration::from_millis(10));
        }
        owner.stop_dunk("d1").unwrap();
        assert!(owner.dunk_timers.workers.lock().unwrap().is_empty());
        owner.stop_and_join();
    }

    #[test]
    fn timer_factory_failure_is_observable_and_worker_remains_joinable() {
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
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(|| panic!("injected terminal factory failure")),
        )
        .unwrap();
        let pane = serde_json::json!({"pane_id": "w1:p1"});
        owner
            .add_dunk(
                pane.as_object(),
                &AddArgs {
                    start: true,
                    ..Default::default()
                },
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let row = flock.dunks.get("d1").unwrap();
            if row["status"] == "Error" {
                assert_eq!(
                    serde_json::json!("scheduled dunk worker panicked"),
                    row["last_error"]
                );
                assert_eq!(serde_json::json!(0), row["send_count"]);
                break;
            }
            assert!(
                Instant::now() < deadline,
                "factory failure was not surfaced"
            );
            thread::sleep(Duration::from_millis(10));
        }
        owner.stop_dunk("d1").unwrap();
        assert!(owner.dunk_timers.workers.lock().unwrap().is_empty());
        owner.stop_and_join();
    }

    #[test]
    fn stop_all_after_live_check_cannot_restore_stopped_deadline_or_status() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            true,
        ));
        let state_path = dir.path().join("state.json");
        flock
            .attach_status_state(crate::flock::status_runtime::StatusState::at_start(
                "test",
                &state_path,
                std::env::current_exe().unwrap(),
                crate::config::Env::default(),
            ))
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
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let owner = Arc::new(
            OutboxRuntime::new(
                Arc::clone(&flock),
                runtime.handle().clone(),
                Arc::new(|| Box::new(FakeTerminal::new())),
            )
            .unwrap(),
        );
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *owner.dunk_timers.before_fields.lock().unwrap() = Some((entered_tx, release_rx));
        let pane = serde_json::json!({"pane_id": "w1:p1"});
        owner
            .add_dunk(
                pane.as_object(),
                &AddArgs {
                    start: true,
                    interval: Some("10".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let handle = flock.dunks.handle("d1").unwrap();
        let signal = Arc::clone(
            &owner.dunk_timers.workers.lock().unwrap()[&DunkTimerOwner::key(&handle)].signal,
        );
        let stopping = Arc::clone(&owner);
        let join = thread::spawn(move || stopping.stop_all_dunks().unwrap());
        let deadline = Instant::now() + Duration::from_secs(5);
        while !signal.cancelled.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert!(
            !join.is_finished(),
            "stop returned before captured worker joined"
        );
        release_tx.send(()).unwrap();
        let rows = join.join().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["running"], false);
        assert!(
            rows[0]["next_send_at"].is_null(),
            "late worker restored deadline: {:?}",
            rows[0]
        );
        assert_eq!(rows[0]["status"], "Stopped");
        assert!(owner.dunk_timers.workers.lock().unwrap().is_empty());
        let persisted: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&state_path).unwrap()).unwrap();
        assert_eq!(persisted["dunkers"][0]["running"], false);
        assert!(persisted["dunkers"][0]["next_send_at"].is_null());
        owner.start_dunk("d1").unwrap();
        owner.stop_dunk("d1").unwrap();
        assert!(flock.dunks.get("d1").unwrap()["next_send_at"].is_null());
        owner.stop_and_join();
    }

    #[test]
    fn stop_all_joins_multiple_real_timer_workers_and_allows_explicit_restart() {
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
        for _ in 0..2 {
            flock
                .dunks
                .add(
                    pane.as_object(),
                    &AddArgs {
                        start: true,
                        interval: Some("10".into()),
                        ..Default::default()
                    },
                )
                .unwrap();
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let (created_tx, created_rx) = std::sync::mpsc::channel();
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(move || {
                created_tx.send(thread::current().id()).unwrap();
                Box::new(FakeTerminal::new())
            }),
        )
        .unwrap();
        let first = created_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let second = created_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_ne!(first, second);
        assert_ne!(first, thread::current().id());
        let stopped = owner.halt_dunks_aimed_at(&["w1:p1".to_owned()]).unwrap();
        assert_eq!(vec!["d1", "d2"], stopped);
        assert!(owner.dunk_timers.workers.lock().unwrap().is_empty());
        for id in ["d1", "d2"] {
            let row = flock.dunks.get(id).unwrap();
            assert_eq!(serde_json::json!("Target closed"), row["status"]);
            assert!(row["next_send_at"].is_null());
            owner.start_dunk(id).unwrap();
            created_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        }
        let rows = owner.stop_all_dunks().unwrap();
        assert_eq!(2, rows.len());
        assert!(rows
            .iter()
            .all(|row| row["running"] == false && row["next_send_at"].is_null()));
        assert!(owner.dunk_timers.workers.lock().unwrap().is_empty());
        owner.start_dunk("d1").unwrap();
        created_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        owner.stop_and_join();
        assert!(owner.dunk_timers.workers.lock().unwrap().is_empty());
        assert_eq!(
            serde_json::json!(true),
            flock.dunks.get("d1").unwrap()["running"],
            "runtime shutdown preserves restart intent"
        );
    }

    #[test]
    fn real_timer_backpressure_holds_full_and_unreadable_boxes_even_when_busy() {
        for unreadable in [false, true] {
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
            flock._load_dunks(
                serde_json::json!({"dunkers": [{
                    "id": "d1", "running": true, "next_send_at": crate::clock::now_s() + 0.2,
                    "interval_minutes": 10, "target_pane_id": "w1:p1", "text": "next",
                    "send_count": 1, "last_sent_text": "previous", "awaiting_consumption": true,
                    "skip_if_unconsumed": true, "hold_while_typing": false
                }]})
                .as_object()
                .unwrap(),
            );
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let owner = OutboxRuntime::new(
                Arc::clone(&flock),
                runtime.handle().clone(),
                Arc::new(move || {
                    let mut fake = FakeTerminal::new();
                    fake.statuses.insert("w1:p1".into(), "working".into());
                    fake.screens.insert("w1:p1".into(), "❯ previous".into());
                    if unreadable {
                        fake.unreadable.insert("w1:p1".into());
                    }
                    Box::new(fake)
                }),
            )
            .unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let row = flock.dunks.get("d1").unwrap();
                assert_eq!(
                    serde_json::json!(1),
                    row["send_count"],
                    "busy must not imply consumption"
                );
                if row["total_skips"].as_i64().unwrap() > 0 {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "real timer did not record backpressure"
                );
                thread::sleep(Duration::from_millis(10));
            }
            owner.stop_dunk("d1").unwrap();
            let row = flock.dunks.get("d1").unwrap();
            assert_eq!(serde_json::json!(1), row["total_skips"]);
            assert_eq!(serde_json::json!(1), row["send_count"]);
            assert_eq!(
                serde_json::json!(if unreadable { 1 } else { 0 }),
                row["unknown_skips"]
            );
            owner.stop_and_join();
        }
    }

    #[test]
    fn real_timer_cancel_before_commit_joins_without_touching_same_id_replacement() {
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
                    interval: Some("0.001".into()),
                    text: Some("old lifetime".into()),
                    hold_while_typing: Some("false".into()),
                    ..Default::default()
                },
            )
            .unwrap();
        let old = flock.dunks.handle("d1").unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let owner = Arc::new(
            OutboxRuntime::new(
                Arc::clone(&flock),
                runtime.handle().clone(),
                Arc::new(|| Box::new(FakeTerminal::new())),
            )
            .unwrap(),
        );
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        *owner.dunk_timers.precommit.lock().unwrap() = Some((entered_tx, release_rx));
        owner.start_dunk("d1").unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let signal = Arc::clone(
            &owner.dunk_timers.workers.lock().unwrap()[&DunkTimerOwner::key(&old)].signal,
        );
        let stopping = Arc::clone(&owner);
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let stop = thread::spawn(move || {
            stopping.stop_dunk("d1").unwrap();
            done_tx.send(()).unwrap();
        });
        let deadline = Instant::now() + Duration::from_secs(5);
        while !signal.cancelled.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        assert!(
            done_rx.try_recv().is_err(),
            "stop must wait for the blocked worker"
        );
        flock.dunks.remove("d1").unwrap();
        flock._load_dunks(
            serde_json::json!({"dunkers": [{"id": "d1", "text": "replacement", "send_count": 7}]})
                .as_object()
                .unwrap(),
        );
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        stop.join().unwrap();
        assert_eq!(
            0,
            crate::flock::dunks::DunkList::timer_fields(&old).send_count
        );
        let row = flock.dunks.get("d1").unwrap();
        assert_eq!(serde_json::json!(7), row["send_count"]);
        assert_eq!(serde_json::json!("replacement"), row["text"]);
        assert!(owner.dunk_timers.workers.lock().unwrap().is_empty());
        assert_eq!(1, flock.dunks.snapshot().1.len());
        owner.stop_and_join();
    }

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
            max_sends: Some("2".into()),
            hold_while_typing: Some("false".into()),
            ..AddArgs::default()
        };
        flock.dunks.add(pane.as_object(), &args).unwrap();
        let captured = flock.dunks.handle("d1").unwrap();
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
        let finished = crate::flock::dunks::DunkList::timer_fields(&captured);
        assert_eq!(2, finished.send_count);
        assert_eq!(Some("scheduled 2"), finished.last_sent_text.as_deref());
        owner.stop_and_join();
    }

    #[test]
    fn restored_overdue_timer_waits_grace_then_delivers_and_removes() {
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
        let before = crate::clock::now_s();
        flock._load_dunks(
            serde_json::json!({"dunkers": [{
                "id": "d1", "running": true, "next_send_at": before - 100.0,
                "target_pane_id": "w1:p1", "text": "restored {count}",
                "max_sends": 1, "hold_while_typing": false
            }]})
            .as_object()
            .unwrap(),
        );
        let captured = flock.dunks.handle("d1").unwrap();
        let due = crate::flock::dunks::DunkList::timer_fields(&captured)
            .next_send_at
            .unwrap();
        assert!(due >= before + 5.0);
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
        let deadline = Instant::now() + Duration::from_secs(10);
        while flock.dunks.get("d1").is_ok() {
            assert!(Instant::now() < deadline, "restored timer did not deliver");
            if crate::clock::now_s() < due - 0.05 {
                assert_eq!(
                    0,
                    crate::flock::dunks::DunkList::timer_fields(&captured).send_count
                );
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            1,
            crate::flock::dunks::DunkList::timer_fields(&captured).send_count
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
        let deadline = Instant::now() + Duration::from_secs(5);
        while flock.dunks.get("d1").unwrap()["send_count"]
            .as_i64()
            .unwrap()
            == 0
        {
            assert!(
                Instant::now() < deadline,
                "interval wake did not cause actual delivery"
            );
            thread::sleep(Duration::from_millis(10));
        }
        owner.stop_dunk("d1").unwrap();
        assert!(owner.dunk_timers.workers.lock().unwrap().is_empty());
        let stopped_count = flock.dunks.get("d1").unwrap()["send_count"].clone();
        thread::sleep(Duration::from_millis(1100));
        assert_eq!(stopped_count, flock.dunks.get("d1").unwrap()["send_count"]);
        assert_eq!(
            serde_json::json!(false),
            flock.dunks.get("d1").unwrap()["running"]
        );
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
            if row["status"] == "Busy (working)" {
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
