//! `flock/loops.rs` (T36, slice 3): the task reaper loop, from `dunk_core._task_reaper_loop`.
//!
//! One TICK is a plain function over a [`ReaperEnv`] (the backend and the maintenance steps the
//! Python reaches through `self`), so a test drives it without a daemon or a timer; the loop is
//! a tokio task that calls the tick every `DUNKINGSHEEP_REAP_S` seconds (default 15) and stops on
//! a signal (`self._task_stop`).
//!
//! The tick, in the Python's order: list panes -> snapshot launches -> clear historical errors ->
//! inbox maintenance (undeliverable sweep + 24 h auto-archive); then, ONLY for a LIVE backend,
//! `Tasks::reap` with the live pane set and the caller-liveness check; then memory self-repair,
//! then the follower cleanup (a waiter whose pane is gone is dropped).
//!
//! A FIXTURE BACKEND CANNOT REAP, and must not try (t247): `reap` refuses a pane list that is not
//! tagged live, correctly, so the tick skips the reap instead of logging a refusal as a fault.
//!
//! The concrete production services are supplied by `LoopRuntimeAdapter`; every terminal or
//! synchronous-store hook is awaited so Tokio worker threads never drive SyncWriter directly.
//! [`LoopSet`] owns the push, stuck-observer, and reaper tasks as one stoppable group.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use super::Flock;
use crate::error::DunkError;
use crate::flock::loop_env::LoopFuture;
use crate::tasks::{CallerAlive, Requeued};

/// Cadences are explicit so a runtime or fixture can supply its own timing.
#[derive(Clone, Copy, Debug)]
pub struct LoopIntervals {
    pub push: Duration,
    pub stuck: Duration,
    pub reaper: Duration,
}

impl Default for LoopIntervals {
    fn default() -> Self {
        Self {
            push: super::loops_push::push_interval(),
            stuck: Duration::from_secs_f64(super::loops_stuck::STUCK_OBSERVE_TICK_S),
            reaper: reap_interval(),
        }
    }
}

struct LoopTasks {
    stop: tokio::sync::watch::Sender<bool>,
    joins: [Option<tokio::task::JoinHandle<()>>; 3],
    errors: Vec<tokio::task::JoinError>,
}

/// Own one set of background loops. Call `stop().await` before shutting down their services.
/// Dropping signals shutdown but cannot synchronously join async tasks.
#[derive(Default)]
#[must_use = "retain the loop owner and await stop before shutting down services"]
pub struct LoopSet {
    tasks: Option<LoopTasks>,
}

impl LoopSet {
    /// Start once on the caller's Tokio runtime. A second start retains the existing set.
    /// The environment must supply all three concrete loop interfaces; production uses the
    /// canonical LoopRuntimeAdapter with its recovery/dialog services attached.
    pub fn start<E>(&mut self, flock: Arc<Flock>, env: Arc<E>, intervals: LoopIntervals) -> bool
    where
        E: super::loops_push::PushEnv + super::loops_stuck::StuckEnv + ReaperEnv + 'static,
    {
        if self.tasks.is_some() {
            return false;
        }
        let (stop, receive) = tokio::sync::watch::channel(false);
        let push_env: Arc<dyn super::loops_push::PushEnv> = env.clone();
        let stuck_env: Arc<dyn super::loops_stuck::StuckEnv> = env.clone();
        let reaper_env: Arc<dyn ReaperEnv> = env;
        let joins = [
            Some(tokio::spawn(super::loops_push::run_push(
                Arc::clone(&flock),
                push_env,
                intervals.push,
                receive.clone(),
            ))),
            Some(tokio::spawn(super::loops_stuck::run_stuck_observer(
                stuck_env,
                intervals.stuck,
                receive.clone(),
            ))),
            Some(tokio::spawn(run_reaper(
                flock,
                reaper_env,
                intervals.reaper,
                receive,
            ))),
        ];
        self.tasks = Some(LoopTasks {
            stop,
            joins,
            errors: Vec::new(),
        });
        true
    }

    /// Signal every loop before joining any. In-flight blocking store/terminal work is allowed
    /// to finish; aborting its async wrapper would detach that work from shutdown ownership.
    /// If this future is cancelled, a later stop resumes joining the retained handles.
    pub async fn stop(&mut self) -> Result<(), Vec<tokio::task::JoinError>> {
        let Some(tasks) = self.tasks.as_mut() else {
            return Ok(());
        };
        tasks.stop.send_replace(true);
        for slot in &mut tasks.joins {
            if let Some(join) = slot.as_mut() {
                if let Err(error) = join.await {
                    tasks.errors.push(error);
                }
                *slot = None;
            }
        }
        let tasks = self
            .tasks
            .take()
            .expect("retained until every join completes");
        if tasks.errors.is_empty() {
            Ok(())
        } else {
            Err(tasks.errors)
        }
    }
}

impl Drop for LoopSet {
    fn drop(&mut self) {
        if let Some(tasks) = &self.tasks {
            tasks.stop.send_replace(true);
        }
    }
}

/// `TASK_REAP_S` (`DUNKINGSHEEP_REAP_S`, default 15 s), read per call here.
pub fn reap_interval() -> Duration {
    let secs = std::env::var("DUNKINGSHEEP_REAP_S")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|s| *s > 0.0)
        .unwrap_or(15.0);
    Duration::from_secs_f64(secs)
}

/// What one reaper tick reaches outside the task store. Every maintenance step is a hook that
/// must never break the tick (the Python wraps each in its own `except`).
pub trait ReaperEnv: Send + Sync {
    /// `self.herdr.list_panes()`: the backend's panes, or an error when it cannot answer.
    fn pane_snapshot(&self) -> LoopFuture<'_, (bool, Result<Vec<Value>, DunkError>)>;
    /// `_clear_historical_errors(panes)`.
    fn clear_historical_errors<'a>(&'a self, panes: &'a BTreeSet<String>) -> LoopFuture<'a, ()>;
    /// The inbox sweeps: `mark_undeliverable(panes)`, `auto_archive_undeliverable()`,
    /// `_archive_retired_inbox(pane_list)`.
    fn inbox_maintenance<'a>(
        &'a self,
        panes: &'a BTreeSet<String>,
        pane_list: &'a [Value],
    ) -> LoopFuture<'a, ()>;
    /// `self.memory.recover_leases()` + `requeue_partial()`: deterministic self-repair.
    fn memory_maintenance(&self) -> LoopFuture<'_, ()>;
    /// `self.last_message = "Requeued N task(s) (...)"`.
    fn set_last_message(&self, message: &str);
}

/// What a tick did, for the log and for tests.
#[derive(Debug, Default, PartialEq)]
pub struct ReaperTick {
    /// How many panes the backend listed; `None` when the listing failed.
    pub panes: Option<usize>,
    /// The backend is not live, so nothing was reaped.
    pub skipped_not_live: bool,
    pub requeued: Vec<Requeued>,
    /// A reap error (logged, never raised).
    pub reap_error: Option<String>,
}

impl Flock {
    /// Port `_clear_historical_errors`: mutate exact live targets and save the whole Flock once.
    pub(crate) fn clear_historical_errors(
        &self,
        live_panes: &BTreeSet<String>,
    ) -> Result<(), DunkError> {
        if self.dunks.clear_historical_errors(live_panes) {
            self.save()?;
        }
        Ok(())
    }

    /// `_task_reaper_loop`, one iteration.
    pub async fn reaper_tick(&self, env: &dyn ReaperEnv) -> ReaperTick {
        let mut tick = ReaperTick::default();
        let mut panes: Option<BTreeSet<String>> = None;
        let (is_live, listing) = env.pane_snapshot().await;
        match listing {
            Ok(list) => {
                self.snapshot_launches(&list);
                let ids: BTreeSet<String> = list
                    .iter()
                    .filter_map(|p| p.get("pane_id").and_then(Value::as_str).map(String::from))
                    .collect();
                env.clear_historical_errors(&ids).await;
                env.inbox_maintenance(&ids, &list).await;
                tick.panes = Some(ids.len());
                panes = Some(ids);
            }
            Err(error) => eprintln!("task reaper: pane listing failed: {error}"),
        }
        if let Some(tasks) = self.stores().tasks {
            if let Err(error) = tasks.sweep_auto_unblock().await {
                eprintln!("task reaper auto-unblock sweep failed: {error}");
            }
        }
        if !is_live {
            tick.skipped_not_live = true;
            return tick;
        }
        let Some(tasks) = self.stores().tasks else {
            tick.reap_error = Some("no task store is attached".into());
            return tick;
        };
        // A lease's recorded caller is checked against /proc (`process_alive`).
        let alive: CallerAlive = Box::new(crate::proc::process_alive);
        let live_panes = panes
            .as_ref()
            .map(|p| p.iter().cloned().collect::<Vec<_>>());
        match tasks
            .reap(live_panes, Some(alive), None, Some("live"))
            .await
        {
            Ok(requeued) => {
                if !requeued.is_empty() {
                    let ids: Vec<&str> = requeued
                        .iter()
                        .take(5)
                        .map(|r| r.task_id.as_str())
                        .collect();
                    env.set_last_message(&format!(
                        "Requeued {} task(s) ({})",
                        requeued.len(),
                        ids.join(", ")
                    ));
                    eprintln!("requeued task leases: {requeued:?}");
                }
                tick.requeued = requeued;
            }
            Err(error) => {
                eprintln!("task reaper failed: {error}");
                tick.reap_error = Some(error.to_string());
                return tick;
            }
        }
        env.memory_maintenance().await;
        if let Some(panes) = &panes {
            self.task_waiters
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .retain(|pane, _| panes.contains(pane));
        }
        tick
    }
}

/// The loop: tick every `interval` until `stop` flips to true (`self._task_stop.wait(...)`).
pub async fn run_reaper(
    flock: Arc<Flock>,
    env: Arc<dyn ReaperEnv>,
    interval: Duration,
    mut stop: tokio::sync::watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            changed = stop.changed() => {
                if changed.is_err() || *stop.borrow() {
                    return;
                }
            }
            _ = tokio::time::sleep(interval) => {
                flock.reaper_tick(env.as_ref()).await;
            }
        }
    }
}

#[cfg(test)]
mod owner_tests {
    use super::*;
    use crate::config::Env;
    use crate::flock::panes::Stores;
    use crate::roles::Roles;
    use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
    use crate::tasks::Tasks;
    use crate::terminal::fakes::FakeTerminal;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct Fixture {
        services: Arc<RuntimeServices>,
        runtime: tokio::runtime::Handle,
        tasks: Arc<Tasks>,
        busy: Arc<AtomicBool>,
        calls: Arc<AtomicUsize>,
        blocked: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
        entered: Arc<AtomicBool>,
        recovery: Arc<AtomicBool>,
        keys: Arc<AtomicUsize>,
        _dir: tempfile::TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            let handle = runtime.handle().clone();
            let tasks = Arc::new(
                runtime
                    .block_on(Tasks::open(dir.path().join("tasks.db")))
                    .unwrap(),
            );
            runtime.block_on(tasks.allocation_init()).unwrap();
            let inbox = Arc::new(
                crate::inbox_store::Store::open(
                    dir.path().join("inbox.db"),
                    dir.path().join("blobs"),
                )
                .unwrap(),
            );
            let mut roles = Roles::new(
                &Env::default(),
                Some(dir.path().join("roles.json")),
                Some(dir.path().join("tokens.json")),
                None,
            )
            .unwrap();
            roles
                .register("w1:p1", "worker", None, None, None, None, None, None)
                .unwrap();
            let busy = Arc::new(AtomicBool::new(false));
            let calls = Arc::new(AtomicUsize::new(0));
            let recovery = Arc::new(AtomicBool::new(false));
            let keys = Arc::new(AtomicUsize::new(0));
            let blocked = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
            let entered = Arc::new(AtomicBool::new(false));
            let factory_blocked = Arc::clone(&blocked);
            let factory_entered = Arc::clone(&entered);
            let factory_recovery = Arc::clone(&recovery);
            let factory_keys = Arc::clone(&keys);
            let factory_busy = Arc::clone(&busy);
            let factory_calls = Arc::clone(&calls);
            let services = RuntimeServices::new(RuntimeServiceInputs {
                launches_file: dir.path().join("launches.json"),
                lane_registry_file: dir.path().join("lanes.json"),
                persist_enabled: true,
                stores: Stores {
                    tasks: Some(Arc::clone(&tasks)),
                    inbox: Some(inbox),
                    ..Default::default()
                },
                roles,
                legacy_superusers: None,
                release_version: "test".into(),
                state_file: dir.path().join("state.json"),
                executable: std::env::current_exe().unwrap(),
                env: Env::default(),
                observability: Some(RuntimeObservability {
                    state_dir: dir.path().join("state-dir"),
                    memory_db: None,
                }),
                runtime,
                terminal_factory: Arc::new(move || {
                    factory_calls.fetch_add(1, Ordering::SeqCst);
                    let (lock, changed) = &*factory_blocked;
                    let mut waiting = lock.lock().unwrap();
                    while *waiting {
                        factory_entered.store(true, Ordering::SeqCst);
                        waiting = changed.wait(waiting).unwrap();
                    }
                    drop(waiting);
                    let mut terminal = FakeTerminal::new();
                    terminal.panes[0].pane_pid = None;
                    terminal.statuses.insert(
                        "w1:p1".into(),
                        if factory_busy.load(Ordering::SeqCst) {
                            "working"
                        } else {
                            "idle"
                        }
                        .into(),
                    );
                    if factory_recovery.load(Ordering::SeqCst) {
                        terminal.panes[0].pane_pid = Some(i64::from(std::process::id()));
                        terminal.panes[0].command = "codex".into();
                        Box::new(RecoveryTerminal {
                            inner: terminal,
                            keys: Arc::clone(&factory_keys),
                        })
                    } else {
                        Box::new(terminal)
                    }
                }),
            })
            .unwrap();
            Self {
                services,
                runtime: handle,
                tasks,
                busy,
                calls,
                blocked,
                entered,
                recovery,
                keys,
                _dir: dir,
            }
        }
    }

    use crate::terminal::{AgentSpec, Pane, Resize, Tab, Terminal, TerminalError, Workspace};
    use std::collections::BTreeMap;
    #[derive(Debug)]
    struct RecoveryTerminal {
        inner: FakeTerminal,
        keys: Arc<AtomicUsize>,
    }
    impl Terminal for RecoveryTerminal {
        fn version(&self) -> String {
            self.inner.version()
        }
        fn is_available(&self) -> bool {
            self.inner.is_available()
        }
        fn server_error_hint(&self) -> String {
            self.inner.server_error_hint()
        }
        fn list_panes(&self) -> Vec<Pane> {
            self.inner.list_panes()
        }
        fn get_pane(&self, pane_id: &str) -> Option<Pane> {
            self.inner.get_pane(pane_id)
        }
        fn list_tabs(&self) -> Vec<Tab> {
            self.inner.list_tabs()
        }
        fn list_workspaces(&self) -> Vec<Workspace> {
            self.inner.list_workspaces()
        }
        fn list_panes_grouped(&self) -> BTreeMap<String, Vec<Pane>> {
            self.inner.list_panes_grouped()
        }
        fn agent_child_pids(&self, pane_pid: i64) -> Result<Vec<i64>, TerminalError> {
            self.inner.agent_child_pids(pane_pid)
        }
        fn create_workspace(
            &mut self,
            label: Option<&str>,
            cwd: Option<&str>,
            focus: bool,
        ) -> Result<Workspace, TerminalError> {
            self.inner.create_workspace(label, cwd, focus)
        }
        fn create_tab(
            &mut self,
            workspace_id: Option<&str>,
            label: Option<&str>,
            cwd: Option<&str>,
            focus: bool,
        ) -> Result<Tab, TerminalError> {
            self.inner.create_tab(workspace_id, label, cwd, focus)
        }
        fn split_pane(
            &mut self,
            pane_id: &str,
            direction: &str,
            cwd: Option<&str>,
        ) -> Result<Pane, TerminalError> {
            self.inner.split_pane(pane_id, direction, cwd)
        }
        fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError> {
            self.inner.start_agent(spec)
        }
        fn rename(
            &mut self,
            kind: &str,
            target_id: &str,
            label: &str,
        ) -> Result<(), TerminalError> {
            self.inner.rename(kind, target_id, label)
        }
        fn close(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
            self.inner.close(kind, target_id)
        }
        fn focus(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
            self.inner.focus(kind, target_id)
        }
        fn run_command(&mut self, pane_id: &str, command: &str) -> Result<(), TerminalError> {
            self.inner.run_command(pane_id, command)
        }
        fn send_text(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
            self.inner.send_text(pane_id, text)
        }
        fn send_text_and_enter(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
            self.inner.send_text_and_enter(pane_id, text)
        }
        fn agent_status(&self, pane_id: &str) -> Option<String> {
            self.inner.agent_status(pane_id)
        }
        fn resize(&mut self, pane_id: &str, cols: i64, rows: i64) -> Result<Resize, TerminalError> {
            self.inner.resize(pane_id, cols, rows)
        }
        fn send_keys(&mut self, pane_id: &str, keys: &[&str]) -> Result<(), TerminalError> {
            assert_eq!(pane_id, "w1:p1");
            assert_eq!(keys, &["Enter"]);
            self.keys.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn read_pane(
            &self,
            pane_id: &str,
            lines: i64,
            ansi: bool,
        ) -> Result<Option<String>, TerminalError> {
            if pane_id != "w1:p1" {
                return self.inner.read_pane(pane_id, lines, ansi);
            }
            let body = if self.keys.load(Ordering::SeqCst) == 0 {
                "owned stranded text"
            } else {
                "\x1b[2mAsk Codex to do anything\x1b[0m"
            };
            Ok(Some(format!(
                "{}› {body}\n  model · /tmp/fixture · task\n",
                "\n".repeat(18)
            )))
        }
    }

    #[test]
    fn loop_set_real_observer_submits_owned_text_once_and_joins() {
        let fixture = Fixture::new();
        fixture.recovery.store(true, Ordering::SeqCst);
        let flock = Arc::clone(fixture.services.flock());
        {
            let mut config = flock.stuck_config.lock().unwrap();
            config.threshold_s = 0.02;
            config.observation_interval_s = 0.0;
        }
        let pid = i64::from(std::process::id());
        let mut message = crate::flock::sends::Message::new(
            "owned",
            "w1:p1",
            "writer",
            "owned stranded text",
            "send_text",
        );
        message.created_at = 1.0;
        message.status = "failed".into();
        message.attempt_generation = Some(format!(
            "pid:{pid}:start:{}",
            crate::proc::process_start(pid).unwrap()
        ));
        flock
            .sent_text_by_pane
            .lock()
            .unwrap()
            .insert("w1:p1".into(), vec!["owned stranded text".into()]);
        flock.note_stuck_candidate(&message);
        fixture.runtime.block_on(async {
            let env = Arc::new(fixture.services.push_loop_adapter().unwrap());
            let mut loops = LoopSet::default();
            loops.start(
                Arc::clone(&flock),
                env,
                LoopIntervals {
                    push: Duration::from_secs(3600),
                    stuck: Duration::from_millis(20),
                    reaper: Duration::from_secs(3600),
                },
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let consumed = flock
                        .stuck_candidates
                        .lock()
                        .unwrap()
                        .get("w1:p1")
                        .is_some_and(|entry| entry.recovery_outcome.as_deref() == Some("consumed"));
                    if consumed {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            loops.stop().await.unwrap();
        });
        assert_eq!(1, fixture.keys.load(Ordering::SeqCst));
        let calls = fixture.calls.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(75));
        assert_eq!(calls, fixture.calls.load(Ordering::SeqCst));
        fixture.services.shutdown();
    }
    #[test]
    fn loop_set_cancelled_stop_retains_inflight_blocking_work_for_join() {
        let fixture = Fixture::new();
        *fixture.blocked.0.lock().unwrap() = true;
        fixture.runtime.block_on(async {
            let mut loops = LoopSet::default();
            loops.start(
                Arc::clone(fixture.services.flock()),
                Arc::new(fixture.services.push_loop_adapter().unwrap()),
                LoopIntervals {
                    push: Duration::from_secs(3600),
                    stuck: Duration::from_millis(1),
                    reaper: Duration::from_millis(1),
                },
            );
            tokio::time::timeout(Duration::from_secs(5), async {
                while !fixture.entered.load(Ordering::SeqCst) {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(30), loops.stop())
                    .await
                    .is_err()
            );
            assert!(loops.tasks.is_some(), "cancelled join lost its handles");
            *fixture.blocked.0.lock().unwrap() = false;
            fixture.blocked.1.notify_all();
            tokio::time::timeout(Duration::from_secs(5), loops.stop())
                .await
                .unwrap()
                .unwrap();
            assert!(loops.tasks.is_none());
        });
        fixture.services.shutdown();
    }

    #[test]
    fn loop_set_periodic_push_claims_registered_lane_without_follow_and_joins() {
        let fixture = Fixture::new();
        fixture.runtime.block_on(async {
            let task = fixture
                .tasks
                .add("global", "assigned fixture work", "bounded fixture")
                .await
                .unwrap();
            let env = Arc::new(fixture.services.push_loop_adapter().unwrap());
            let flock = Arc::clone(fixture.services.flock());
            assert!(flock.task_waiters.lock().unwrap().is_empty());
            let intervals = LoopIntervals {
                push: Duration::from_millis(20),
                stuck: Duration::from_millis(25),
                reaper: Duration::from_millis(30),
            };
            let mut loops = LoopSet::default();
            assert!(loops.start(Arc::clone(&flock), Arc::clone(&env), intervals));
            assert!(!loops.start(Arc::clone(&flock), Arc::clone(&env), intervals));
            tokio::time::timeout(Duration::from_secs(5), async {
                while fixture.tasks.get(&task.id).await.unwrap().status != "in_progress" {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            loops.stop().await.unwrap();
            loops.stop().await.unwrap();
            let calls = fixture.calls.load(Ordering::SeqCst);
            flock.wake_push();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(
                calls,
                fixture.calls.load(Ordering::SeqCst),
                "joined loops still use terminal services"
            );
            assert!(loops.start(flock, env, intervals));
            loops.stop().await.unwrap();
        });
        let announcements = fixture
            .services
            .flock()
            .stores()
            .inbox
            .unwrap()
            .read("w1:p1", 10, true, false)
            .unwrap();
        assert!(
            !announcements.is_empty(),
            "real push did not persist its task announcement"
        );
        fixture.services.shutdown();
    }

    #[test]
    fn loop_set_turn_end_wake_skips_busy_lane_then_claims_when_idle() {
        let fixture = Fixture::new();
        fixture.busy.store(true, Ordering::SeqCst);
        fixture.runtime.block_on(async {
            let task = fixture
                .tasks
                .add("global", "wake fixture work", "bounded fixture")
                .await
                .unwrap();
            let env = Arc::new(fixture.services.push_loop_adapter().unwrap());
            let flock = Arc::clone(fixture.services.flock());
            let mut loops = LoopSet::default();
            loops.start(
                Arc::clone(&flock),
                env,
                LoopIntervals {
                    push: Duration::from_secs(3600),
                    stuck: Duration::from_secs(3600),
                    reaper: Duration::from_secs(3600),
                },
            );
            flock.wake_push();
            tokio::time::timeout(Duration::from_secs(5), async {
                while fixture.calls.load(Ordering::SeqCst) == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            tokio::time::sleep(Duration::from_millis(100)).await;
            assert_eq!(fixture.tasks.get(&task.id).await.unwrap().status, "open");
            fixture.busy.store(false, Ordering::SeqCst);
            flock.wake_push();
            tokio::time::timeout(Duration::from_secs(5), async {
                while fixture.tasks.get(&task.id).await.unwrap().status != "in_progress" {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            loops.stop().await.unwrap();
        });
        fixture.services.shutdown();
    }
}
