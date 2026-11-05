//! Process-owned runtime resources shared by request dispatch and background delivery.
//!
//! This component deliberately stops at resource ownership and request dispatch. The
//! daemon entry point must still bind its remaining runners and drain/join connection
//! threads before dropping these services (the final integration gate is t1605).

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

use crate::memory::service::MemoryService;
use std::time::Duration;
use tokio::runtime::Runtime;

use super::{dispatcher::RuntimeDispatcher, outbox_runtime::OutboxRuntime, LoopRuntimeAdapter};
use crate::config::Env;
use crate::error::{refusal, DunkError};
use crate::flock::loop_env::LoopTerminalFactory;
use crate::flock::loops_stuck::StalePanelReporter;
use crate::flock::panes::Stores;
use crate::flock::status_runtime::StatusState;
use crate::flock::Flock;
use crate::roles::Roles;
use crate::server::Dispatcher;
use crate::superusers::Superusers;
use crate::tasks::next_job_runtime::NextJobs;

/// Explicit observability inputs for [`Flock::wire_observability`].
pub struct RuntimeObservability {
    pub state_dir: PathBuf,
    pub memory_db: Option<PathBuf>,
}

/// A path already resolved by startup and timing parsed from its captured environment.
/// HOME/default resolution remains with the daemon assembly; no process environment reads.
#[derive(Debug, Clone)]
pub struct RuntimeMemoryConfig {
    path: PathBuf,
    poll: Duration,
    throttle: f64,
}
impl RuntimeMemoryConfig {
    pub fn captured(path: PathBuf, env: &Env) -> Result<Self, DunkError> {
        if path.as_os_str().is_empty() || path == std::path::Path::new(":memory:") {
            return Err(refusal(
                "runtime memory requires an explicit database file path",
            ));
        }
        let raw = env.get("DUNKINGSHEEP_MEMORY_THROTTLE_S").unwrap_or("0.1");
        let seconds = crate::github::parse_python_float(raw).ok_or_else(|| {
            refusal(format!(
                "could not convert memory throttle string to float: {raw:?}"
            ))
        })?;
        Ok(Self {
            path,
            poll: Duration::from_millis(500),
            throttle: seconds,
        })
    }
}

/// Own startup rollback even after the Flock has acquired an additional service Arc.
struct StartingMemory(Option<Arc<MemoryService>>);
impl Drop for StartingMemory {
    fn drop(&mut self) {
        if let Some(service) = &self.0 {
            if let Err(error) = service.stop_and_join() {
                eprintln!("memory startup cleanup failed: {error}");
            }
        }
    }
}

/// The concrete resources captured once at daemon startup.
///
/// `stores` must contain the already opened task and inbox stores. When `observability` is
/// present, its explicit paths open metrics and the canonical memory service; otherwise a
/// caller must supply an already opened metrics store in `stores`. Roles and the legacy
/// superuser policy are also the actual opened instances, never defaults synthesized here.
pub struct RuntimeServiceInputs {
    pub launches_file: PathBuf,
    pub lane_registry_file: PathBuf,
    pub persist_enabled: bool,
    pub stores: Stores,
    pub roles: Roles,
    pub legacy_superusers: Option<Superusers>,
    pub release_version: String,
    pub state_file: PathBuf,
    pub executable: PathBuf,
    pub env: Env,
    pub observability: Option<RuntimeObservability>,
    /// The owned daemon runtime. A detached `Handle` is not sufficient lifetime ownership.
    pub runtime: Runtime,
    /// The same configured factory is used by the outbox and next-job manager.
    pub terminal_factory: LoopTerminalFactory,
}

/// One process's Flock, managers, terminal factory, and Tokio runtime.
///
/// The `OutboxRuntime` is stopped and joined before the Flock, its stores, or the runtime are
/// dropped. This does not join `FlockServer` request threads; that remains a t1605 shutdown
/// requirement.
pub struct RuntimeServices {
    // Keep the outbox before its backing resources; `Drop` first joins its workers explicitly.
    pub(super) outbox: OutboxRuntime,
    pub(super) next_jobs: NextJobs,
    pub(super) flock: Arc<Flock>,
    pub(super) terminal_factory: LoopTerminalFactory,
    pub(super) runtime: Runtime,
    pub(super) stuck_reporter: StalePanelReporter,
    pub(crate) allocator: crate::flock::allocator::Allocator,
    pub(super) owner_push: super::owner_push::OwnerPushService,
    stopped: AtomicBool,
    shutdown_lock: Mutex<()>,
    shutdown_failure: Mutex<Option<String>>,
    memory: Option<Arc<MemoryService>>,
}

impl RuntimeServices {
    /// Build the one canonical Flock and its two request/background managers.
    pub fn new(inputs: RuntimeServiceInputs) -> Result<Arc<Self>, DunkError> {
        let memory = if inputs.persist_enabled {
            let path = inputs
                .observability
                .as_ref()
                .and_then(|o| o.memory_db.clone())
                .ok_or_else(|| {
                    refusal("persistent runtime requires a startup-selected memory database path")
                })?;
            Some(RuntimeMemoryConfig::captured(path, &inputs.env)?)
        } else {
            None
        };
        Self::new_with_memory(inputs, memory)
    }

    /// Standard-thread startup, including when metrics were opened separately. The caller
    /// supplies the already captured path/config; a persistent runtime cannot omit memory.
    pub fn new_with_memory(
        inputs: RuntimeServiceInputs,
        memory: Option<RuntimeMemoryConfig>,
    ) -> Result<Arc<Self>, DunkError> {
        Self::build(inputs, memory, None, |_, _| {})
    }

    /// Build services with the already decoded whole-Flock state. Restoration happens
    /// after stores are attached and before OutboxRuntime starts restored dunk timers.
    pub(crate) fn new_with_memory_and_snapshot(
        inputs: RuntimeServiceInputs,
        memory: Option<RuntimeMemoryConfig>,
        snapshot: Option<serde_json::Value>,
    ) -> Result<Arc<Self>, DunkError> {
        Self::build(inputs, memory, snapshot, |_, _| {})
    }

    fn build(
        inputs: RuntimeServiceInputs,
        memory: Option<RuntimeMemoryConfig>,
        snapshot: Option<serde_json::Value>,
        before_memory_start: impl FnOnce(&Flock, &Option<Arc<MemoryService>>),
    ) -> Result<Arc<Self>, DunkError> {
        if inputs.persist_enabled != memory.is_some() {
            return Err(refusal(
                "runtime memory configuration must match persistence",
            ));
        }
        if memory.is_some() && inputs.stores.memory.is_some() {
            return Err(refusal(
                "runtime canonical memory conflicts with an attached read pool",
            ));
        }
        for (name, path) in [
            ("launches file", &inputs.launches_file),
            ("lane registry file", &inputs.lane_registry_file),
            ("whole-state file", &inputs.state_file),
            ("executable identity", &inputs.executable),
        ] {
            if path.as_os_str().is_empty() {
                return Err(refusal(format!("runtime {name} must be configured")));
            }
        }
        if inputs.release_version.trim().is_empty() {
            return Err(refusal("runtime release version must be configured"));
        }
        if !inputs.executable.is_file() {
            return Err(refusal("runtime executable identity is not a file"));
        }
        if inputs.state_file == inputs.launches_file
            || inputs.state_file == inputs.lane_registry_file
            || inputs.state_file == inputs.executable
        {
            return Err(refusal(
                "runtime whole-state file must be distinct from launches, lanes, and executable",
            ));
        }
        if inputs.launches_file == inputs.lane_registry_file {
            return Err(refusal(
                "runtime launches and lane registry files must be distinct",
            ));
        }
        if inputs.launches_file == inputs.executable
            || inputs.lane_registry_file == inputs.executable
        {
            return Err(refusal(
                "runtime launches and lane registry files must be distinct from executable identity",
            ));
        }
        if inputs.stores.tasks.is_none() {
            return Err(refusal("runtime task store is not attached"));
        }
        if inputs.stores.inbox.is_none() {
            return Err(refusal("runtime inbox store is not attached"));
        }
        match (&inputs.observability, &inputs.stores.metrics) {
            (Some(_), Some(_)) => {
                return Err(refusal(
                    "runtime observability config conflicts with an attached metrics store",
                ));
            }
            (None, None) => return Err(refusal("runtime metrics store is not attached")),
            _ => {}
        }
        if inputs.observability.is_some() && inputs.stores.memory.is_some() {
            return Err(refusal(
                "runtime observability config conflicts with an attached memory store",
            ));
        }
        if let Some(observability) = &inputs.observability {
            if observability.state_dir.as_os_str().is_empty() {
                return Err(refusal(
                    "runtime observability state directory must be configured",
                ));
            }
        }

        let flock = Arc::new(Flock::new(
            &inputs.launches_file,
            &inputs.lane_registry_file,
            inputs.persist_enabled,
        ));
        flock.attach_stores(inputs.stores);
        if let Some(observability) = inputs.observability {
            flock.wire_observability(&observability.state_dir, None)?;
        }
        // Declare the runtime first so rollback joins memory before runtime destruction.
        let runtime = inputs.runtime;
        let mut starting_memory = StartingMemory(match memory {
            Some(config) => Some(Arc::new(MemoryService::create_at_start(
                &config.path,
                2,
                config.poll,
                crate::memory::jsonl::Throttle::Python(config.throttle),
            )?)),
            None => None,
        });
        if let Some(service) = &starting_memory.0 {
            flock.attach_memory_service(Arc::clone(service))?;
        }
        flock.attach_roles(inputs.roles, inputs.legacy_superusers);
        flock.attach_status_state(StatusState::at_start(
            inputs.release_version,
            &inputs.state_file,
            &inputs.executable,
            inputs.env.clone(),
        ))?;

        if let Some(snapshot) = snapshot.as_ref() {
            flock.restore_snapshot(snapshot);
        }

        // Keep the restored Flock state visible before any runtime worker, including the
        // first-run memory ingestor, can observe or act on this service set.
        before_memory_start(&flock, &starting_memory.0);
        if let Some(service) = &starting_memory.0 {
            service.start()?;
        }

        let terminal_factory = inputs.terminal_factory;
        let stuck_reporter = StalePanelReporter::new();
        let outbox = OutboxRuntime::new_with_reporter(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::clone(&terminal_factory),
            stuck_reporter.clone(),
        )?;
        let gate_env = inputs.env;
        let env_provider = Arc::new(move || Ok(gate_env.clone()));
        let next_jobs = NextJobs::with_providers(
            Arc::clone(&flock),
            env_provider,
            Some(Arc::clone(&terminal_factory)),
        );

        Ok(Arc::new(Self {
            outbox,
            next_jobs,
            flock,
            terminal_factory,
            runtime,
            stuck_reporter,
            allocator: crate::flock::allocator::Allocator::new(),
            owner_push: super::owner_push::OwnerPushService::default(),
            stopped: AtomicBool::new(false),
            shutdown_lock: Mutex::new(()),
            shutdown_failure: Mutex::new(None),
            memory: starting_memory.0.take(),
        }))
    }

    pub fn start_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let row = self.outbox.start_dunk(id)?;
        self.flock.save()?;
        Ok(row)
    }

    pub fn stop_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let row = self.outbox.stop_dunk(id)?;
        self.flock.save()?;
        Ok(row)
    }

    pub fn toggle_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let row = self.outbox.toggle_dunk(id)?;
        self.flock.save()?;
        Ok(row)
    }

    pub fn add_dunk(
        &self,
        pane: Option<&serde_json::Map<String, serde_json::Value>>,
        args: &crate::flock::dunks::AddArgs,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        self.outbox.add_dunk(pane, args)
    }

    pub fn update_dunk(
        &self,
        id: &str,
        pane: Option<&serde_json::Map<String, serde_json::Value>>,
        args: &crate::flock::dunks::UpdateArgs,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        self.outbox.update_dunk(id, pane, args)
    }

    pub fn remove_dunk(
        &self,
        id: &str,
    ) -> Result<serde_json::Map<String, serde_json::Value>, DunkError> {
        let row = self.outbox.remove_dunk(id)?;
        self.flock.save()?;
        Ok(row)
    }

    pub fn stop_all_dunks(
        &self,
    ) -> Result<Vec<serde_json::Map<String, serde_json::Value>>, DunkError> {
        self.outbox.stop_all_dunks()
    }

    pub fn fire_dunk(
        &self,
        id: &str,
        countdown_s: i64,
        wait: bool,
    ) -> Result<serde_json::Value, DunkError> {
        if self.is_stopped() {
            return Err(refusal("manual dunk fire runtime is shutting down"));
        }
        self.outbox.fire_dunk(id, countdown_s, wait)
    }

    #[cfg(test)]
    pub(crate) fn set_manual_fire_test_gates(
        &self,
        countdown: Option<Arc<super::outbox_runtime::CountdownGate>>,
        submit: Option<Arc<super::outbox_runtime::SubmitGate>>,
    ) {
        self.outbox.set_manual_fire_test_gates(countdown, submit);
    }

    #[cfg(test)]
    pub(crate) fn manual_fire_is_stopped(&self) -> bool {
        self.outbox.manual_fire_is_stopped()
    }

    #[cfg(test)]
    pub(crate) fn fail_next_manual_fire_spawn(&self) {
        self.outbox.fail_next_manual_fire_spawn();
    }

    #[cfg(test)]
    pub(crate) fn completed_manual_fires(&self) -> Vec<(String, String)> {
        self.outbox.completed_manual_fires()
    }

    pub fn halt_dunks_aimed_at(&self, panes: &[String]) -> Result<Vec<String>, DunkError> {
        self.outbox.halt_dunks_aimed_at(panes)
    }

    /// Capture the startup-selected owner database path exactly once, without opening it.
    /// The startup owner must attach before serving. No environment or path defaults are
    /// inferred here, including for empty or whitespace-containing paths.
    pub fn attach_owner_push_path(&self, path: PathBuf) -> Result<(), DunkError> {
        self.owner_push.attach_path(path)
    }

    /// Build the stuck observer adapter with the canonical Flock, outbox, and reporter.
    /// The ordinary loop-adapter constructor intentionally does not imply these services.
    #[allow(dead_code)] // The dependent startup integration consumes this staged binder.
    pub(crate) fn stuck_loop_adapter(self: &Arc<Self>) -> Result<LoopRuntimeAdapter, DunkError> {
        LoopRuntimeAdapter::new(Arc::clone(&self.flock), Arc::clone(&self.terminal_factory))
            .with_stuck_services(Arc::clone(self))
    }

    /// Bind every push-loop callback to this process's canonical services.
    #[allow(dead_code)] // Consumed by the dependent loop startup integration.
    pub(crate) fn push_loop_adapter(self: &Arc<Self>) -> Result<LoopRuntimeAdapter, DunkError> {
        let stores = self.flock.stores();
        if stores.tasks.is_none() {
            return Err(refusal("push runtime task store is not attached"));
        }
        if stores.inbox.is_none() {
            return Err(refusal("push runtime inbox store is not attached"));
        }
        if !self.next_jobs.owns_flock(self.flock.as_ref()) {
            return Err(refusal("push runtime next jobs flock mismatch"));
        }
        LoopRuntimeAdapter::new(Arc::clone(&self.flock), Arc::clone(&self.terminal_factory))
            .with_stuck_services(Arc::clone(self))
    }

    /// Bind the caller's real API registry to these canonical services.
    pub fn dispatcher(self: &Arc<Self>, handlers: crate::api::Dispatcher) -> Arc<dyn Dispatcher> {
        Arc::new(RuntimeDispatcher::new(Arc::clone(self), handlers))
    }

    /// Resolve lane evidence using a terminal owned by the blocking worker.
    pub(crate) async fn allocator_lanes(
        self: &Arc<Self>,
    ) -> Result<Vec<serde_json::Value>, DunkError> {
        LoopRuntimeAdapter::new(Arc::clone(&self.flock), Arc::clone(&self.terminal_factory))
            .with_loop_terminal(|flock, terminal| {
                flock.resolve_lanes(terminal, crate::clock::now_s())
            })
            .await
            .map_err(|error| refusal(format!("allocator terminal worker failed: {error}")))?
    }

    /// Inspect unread and composer state through the real validated inbox view.
    pub(crate) async fn allocator_probe(
        self: &Arc<Self>,
        pane: String,
        info: serde_json::Value,
    ) -> Result<(i64, serde_json::Value), DunkError> {
        let services = Arc::clone(self);
        LoopRuntimeAdapter::new(Arc::clone(&self.flock), Arc::clone(&self.terminal_factory))
            .with_loop_terminal(move |flock, terminal| {
                struct TrustedWorker<'a>(&'a Flock);
                impl Drop for TrustedWorker<'_> {
                    fn drop(&mut self) {
                        self.0.clear_caller();
                        self.0.clear_socket_peer();
                    }
                }
                flock.clear_caller();
                flock.clear_socket_peer();
                let _trusted = TrustedWorker(flock);
                let request = crate::server::RequestCtx::default();
                let adapter = super::RequestRuntimeAdapter::new(
                    flock,
                    &request,
                    terminal.runtime,
                    terminal.terminal,
                );
                let view = super::inbox_data::InboxRuntimeView::new(
                    adapter,
                    Arc::clone(&services.flock),
                    Arc::clone(&services.outbox.pool),
                )?;
                let unread = flock.inbox_count(&view, None, Some(&pane))?.unread;
                let delivery = if unread == 0 {
                    flock.delivery_state(terminal, &pane, Some("idle"), Some(&info), None)
                } else {
                    serde_json::Value::Null
                };
                Ok((unread, delivery))
            })
            .await
            .map_err(|error| refusal(format!("allocator terminal worker failed: {error}")))?
    }

    pub(crate) async fn allocator_gate(
        &self,
        pane: &str,
        lanes: Vec<serde_json::Value>,
    ) -> Option<String> {
        self.next_jobs.gate(pane, Some(lanes)).await
    }

    pub(crate) async fn allocator_materialise(
        &self,
        handoff: &serde_json::Map<String, serde_json::Value>,
    ) {
        if let Err(error) = self.next_jobs.materialise(handoff).await {
            eprintln!("allocation intent retained for next-job recovery: {error:?}");
        }
    }

    /// Run one allocator pass using this service set and its shared manager.
    #[allow(dead_code)] // The production push loop starts in t1605.
    pub(crate) async fn allocator_tick(self: &Arc<Self>) -> Result<serde_json::Value, DunkError> {
        crate::flock::allocator::Allocator::tick(Arc::clone(self)).await
    }

    pub(crate) fn runtime_handle(&self) -> &tokio::runtime::Handle {
        self.runtime.handle()
    }

    /// The one Flock owned by this process runtime.
    pub fn flock(&self) -> &Arc<Flock> {
        &self.flock
    }

    /// Stop/join outbox workers while their Flock, stores, and runtime remain alive.
    /// The startup owner calls this after FlockServer has stopped accepting and drained/joined
    /// its request threads (the remaining t1605 shutdown gate). Call on a standard thread;
    /// async owners must await `shutdown_async` so no Tokio worker waits on SyncWriter.
    pub fn shutdown(&self) {
        if let Err(error) = self.shutdown_checked() {
            eprintln!("runtime shutdown failed: {error}");
        }
    }

    /// Stop all service owners and return the first failure to the daemon lifecycle.
    pub fn shutdown_checked(&self) -> Result<(), DunkError> {
        assert!(
            self.memory.is_none() || tokio::runtime::Handle::try_current().is_err(),
            "persistent runtime shutdown requires a standard thread; use shutdown_async"
        );
        // Every caller waits for the same completed teardown, not merely a stop flag.
        let _shutdown = self.shutdown_lock.lock().expect("runtime shutdown lock");
        if self.stopped.swap(true, Ordering::AcqRel) {
            if let Some(error) = self
                .shutdown_failure
                .lock()
                .expect("runtime shutdown failure lock")
                .as_ref()
            {
                return Err(refusal(error.clone()));
            }
        } else {
            let mut failures = Vec::new();
            if let Err(error) = self.outbox.stop_timer_workers_checked() {
                failures.push(error.to_string());
            }
            if let Err(error) = self.outbox.stop_manual_fire_workers_checked() {
                failures.push(error.to_string());
            }
            self.allocator.stop_and_join();
            if let Err(error) = self.outbox.stop_outbox_workers_checked() {
                failures.push(error.to_string());
            }
            if let Some(memory) = &self.memory {
                if let Err(error) = memory.stop_and_join() {
                    failures.push(error.to_string());
                }
            }
            if !failures.is_empty() {
                *self
                    .shutdown_failure
                    .lock()
                    .expect("runtime shutdown failure lock") = Some(failures.join("; "));
            }
        }
        if let Some(error) = self
            .shutdown_failure
            .lock()
            .expect("runtime shutdown failure lock")
            .as_ref()
        {
            return Err(refusal(error.clone()));
        }
        Ok(())
    }

    /// Await the same complete teardown without running synchronous owners on Tokio workers.
    /// The final request-drain prerequisite is unchanged.
    pub async fn shutdown_async(self: &Arc<Self>) -> Result<(), DunkError> {
        let services = Arc::clone(self);
        crate::flock::loop_env::with_sync_store(move || {
            std::thread::Builder::new()
                .name("runtime-shutdown".into())
                .spawn(move || services.shutdown_checked())
                .map_err(|error| refusal(format!("runtime shutdown thread: {error}")))?
                .join()
                .map_err(|_| refusal("runtime shutdown thread panicked"))
        })
        .await
        .map_err(|error| refusal(format!("runtime shutdown worker: {error}")))?
    }

    pub(super) fn is_stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
}

impl Drop for RuntimeServices {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::Duration;

    use serde_json::{json, Value};
    use tokio::runtime::{Builder, Runtime};

    use super::*;
    use crate::api;
    use crate::flock::panes::Stores;
    use crate::proc::{process_start, Caller};
    use crate::server::RequestCtx;
    use crate::tasks::Tasks;
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    fn startup_inputs(
        dir: &tempfile::TempDir,
        runtime: Runtime,
        env: Env,
        terminal_factory: LoopTerminalFactory,
        stores: Stores,
    ) -> RuntimeServiceInputs {
        let pid = i64::from(std::process::id());
        let mut roles = Roles::new(
            &env,
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register(
                "w1:p1",
                "worker",
                Some(pid),
                process_start(pid).as_deref(),
                Some("codex"),
                None,
                None,
                None,
            )
            .unwrap();
        roles
            .register("w2:p2", "owner", None, None, None, None, None, None)
            .unwrap();
        let executable = dir.path().join("daemon-test-executable");
        std::fs::write(&executable, b"real test executable identity").unwrap();
        let observability = stores.metrics.is_none().then(|| RuntimeObservability {
            state_dir: dir.path().join("state-dir"),
            memory_db: None,
        });
        RuntimeServiceInputs {
            launches_file: dir.path().join("launches.json"),
            lane_registry_file: dir.path().join("lanes.json"),
            persist_enabled: false,
            stores,
            roles,
            legacy_superusers: Some(
                Superusers::new(dir.path().join("superusers.json"), true).unwrap(),
            ),
            release_version: "test-release".into(),
            state_file: dir.path().join("whole-state.json"),
            executable,
            env,
            observability,
            runtime,
            terminal_factory,
        }
    }

    fn configured_env(dir: &tempfile::TempDir, dispatch: &str) -> Env {
        let config = dir.path().to_string_lossy().into_owned();
        Env::from_pairs(&[
            ("DUNKINGSHEEP_DIR", config.as_str()),
            ("DUNKINGSHEEP_TURN_END_DISPATCH", dispatch),
            ("DUNKINGSHEEP_DISPATCH_LANES", "w1:p1"),
            ("DUNKINGSHEEP_MAILBOX", "w1:p1"),
        ])
    }

    fn tasks_and_inbox(
        runtime: &Runtime,
        dir: &tempfile::TempDir,
    ) -> (Arc<Tasks>, Arc<crate::inbox_store::Store>) {
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let inbox = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        (tasks, inbox)
    }

    #[test]
    fn snapshot_restores_before_one_timer_starts_and_owner_push_stays_lazy() {
        let dir = crate::testsupport::dir("runtime-snapshot-before-workers");
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("daemon runtime");
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let stores = Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let env = configured_env(&dir, "0");
        let inputs = startup_inputs(
            &dir,
            runtime,
            env,
            Arc::new(|| Box::new(FakeTerminal::new())),
            stores,
        );
        let owner_path = dir.path().join("owner-push.db");
        let before = crate::clock::now_s();
        let snapshot = json!({
            "version": 1,
            "dunkers": [{
                "id": "d1",
                "running": true,
                "next_send_at": before - 60.0,
                "target_pane_id": "w1:p1",
                "text": "restored {count}",
                "max_sends": 1,
                "hold_while_typing": false
            }]
        });

        let services = RuntimeServices::new_with_memory_and_snapshot(inputs, None, Some(snapshot))
            .expect("canonical services restore before timer startup");
        let row = services.flock.dunks.get("d1").expect("restored dunk");
        assert!(row["next_send_at"].as_f64().unwrap() >= before + 4.0);
        assert_eq!(1, services.outbox.dunk_timer_worker_count());
        services
            .attach_owner_push_path(owner_path.clone())
            .expect("attach lazy owner path");
        assert!(
            !owner_path.exists(),
            "attaching the path does not open its DB"
        );
        services
            .shutdown_checked()
            .expect("restored timer joins before resource close");
        assert_eq!(0, services.outbox.dunk_timer_worker_count());
    }

    #[test]
    fn memory_worker_start_boundary_observes_restored_flock_state() {
        let dir = crate::testsupport::dir("runtime-memory-after-restore");
        let runtime = Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("daemon runtime");
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let stores = Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let env = configured_env(&dir, "0");
        let memory_path = dir.path().join("memory.db");
        let mut inputs = startup_inputs(
            &dir,
            runtime,
            env.clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
            stores,
        );
        inputs.persist_enabled = true;
        inputs
            .observability
            .as_mut()
            .expect("runtime opens its metrics store")
            .memory_db = Some(memory_path.clone());
        let memory = Some(RuntimeMemoryConfig::captured(memory_path, &env).unwrap());
        let snapshot = json!({
            "version": 1,
            "dunkers": [{
                "id": "d-restored",
                "running": false,
                "target_pane_id": "w1:p1",
                "text": "restored state",
                "max_sends": 1,
                "hold_while_typing": false
            }]
        });

        let services = RuntimeServices::build(inputs, memory, Some(snapshot), |flock, memory| {
            let memory = memory
                .as_ref()
                .expect("memory service exists before its worker starts");
            let status = memory.status().expect("memory service status");
            assert_eq!(
                false, status["ingestor"]["running"],
                "memory ingestor remains stopped at the restore boundary"
            );
            assert!(
                flock.dunks.get("d-restored").is_ok(),
                "snapshot state is restored at the memory-start boundary"
            );
        })
        .expect("build and start memory after snapshot restoration");
        services
            .shutdown_checked()
            .expect("memory and runtime workers are joined");
    }

    fn fake_factory_with_current_pane() -> LoopTerminalFactory {
        Arc::new(|| {
            let mut terminal = FakeTerminal::new();
            terminal.panes[0].command = "codex".into();
            terminal.panes[0].pane_pid = Some(i64::from(std::process::id()));
            Box::new(terminal)
        })
    }

    #[test]
    fn runtime_allocator_claims_real_offered_task_for_authenticated_idle_pane() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let task = handle
            .block_on(tasks.add("global", "offered", "acceptance"))
            .unwrap();
        handle
            .block_on(tasks.update(
                &task.id,
                crate::tasks::FieldEdit {
                    assignees: Some(vec!["w2:p2".into()]),
                    ..Default::default()
                },
                None,
            ))
            .unwrap();
        handle
            .block_on(tasks.allocation_offer(&task.id, "code", &serde_json::json!([]), "w2:p2"))
            .unwrap();
        handle
            .block_on(tasks.allocation_register(
                "w1:p1",
                &serde_json::json!(["code"]),
                &serde_json::json!(["worker"]),
                &serde_json::json!([]),
                "w2:p2",
                &serde_json::json!(true),
            ))
            .unwrap();
        let stores = Stores {
            tasks: Some(Arc::clone(&tasks)),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let env = configured_env(&dir, "1");
        let factory: LoopTerminalFactory = Arc::new(|| {
            let mut terminal = FakeTerminal::new();
            terminal.panes[0].command = "codex".into();
            terminal.panes[0].pane_pid = Some(i64::from(std::process::id()));
            terminal
                .screens
                .insert("w1:p1".into(), "› Ask Codex to do anything\n".into());
            Box::new(terminal)
        });
        let services =
            RuntimeServices::new(startup_inputs(&dir, runtime, env, factory, stores)).unwrap();
        let pid = i64::from(std::process::id());
        services.flock().set_caller(Some(serde_json::json!({"pid":pid,"start":process_start(pid),"name":"codex","pane_id":"w1:p1","source":"agent"})));
        services
            .flock()
            .turn_start(
                None,
                None,
                None,
                Some("w1:p1"),
                None,
                &crate::flock::commands_turns::NoHooks,
            )
            .unwrap();
        services
            .flock()
            .turn_end(
                None,
                "done",
                Some("w1:p1"),
                None,
                false,
                &crate::flock::commands_turns::NoHooks,
            )
            .unwrap();
        services.flock().clear_caller();
        let result = handle.block_on(services.allocator_tick()).unwrap();
        assert_eq!(result["allocated"][0]["task_id"], task.id);
        assert_eq!(
            handle.block_on(tasks.get(&task.id)).unwrap().status,
            "in_progress"
        );
        assert_eq!(handle.block_on(services.allocator_tick()).unwrap(), result);
        services.shutdown();
    }

    #[test]
    fn runtime_allocator_caller_cancellation_does_not_abandon_commit_before_shutdown() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let task = handle
            .block_on(tasks.add("global", "offered", "acceptance"))
            .unwrap();
        handle
            .block_on(tasks.update(
                &task.id,
                crate::tasks::FieldEdit {
                    assignees: Some(vec!["w2:p2".into()]),
                    ..Default::default()
                },
                None,
            ))
            .unwrap();
        handle
            .block_on(tasks.allocation_offer(&task.id, "code", &serde_json::json!([]), "w2:p2"))
            .unwrap();
        handle
            .block_on(tasks.allocation_register(
                "w1:p1",
                &serde_json::json!(["code"]),
                &serde_json::json!(["worker"]),
                &serde_json::json!([]),
                "w2:p2",
                &serde_json::json!(true),
            ))
            .unwrap();
        let stores = Stores {
            tasks: Some(Arc::clone(&tasks)),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let env = configured_env(&dir, "1");
        let factory: LoopTerminalFactory = Arc::new(|| {
            let mut terminal = FakeTerminal::new();
            terminal.panes[0].command = "codex".into();
            terminal.panes[0].pane_pid = Some(i64::from(std::process::id()));
            terminal
                .screens
                .insert("w1:p1".into(), "› Ask Codex to do anything\n".into());
            Box::new(terminal)
        });
        let services =
            RuntimeServices::new(startup_inputs(&dir, runtime, env, factory, stores)).unwrap();
        let pid = i64::from(std::process::id());
        services.flock().set_caller(Some(serde_json::json!({"pid":pid,"start":process_start(pid),"name":"codex","pane_id":"w1:p1","source":"agent"})));
        services
            .flock()
            .turn_start(
                None,
                None,
                None,
                Some("w1:p1"),
                None,
                &crate::flock::commands_turns::NoHooks,
            )
            .unwrap();
        services
            .flock()
            .turn_end(
                None,
                "done",
                Some("w1:p1"),
                None,
                false,
                &crate::flock::commands_turns::NoHooks,
            )
            .unwrap();
        services.flock().clear_caller();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        services.allocator.set_commit_barrier(Arc::new(
            crate::tasks::allocator::AllocationCommitBarrier {
                entered: entered_tx,
                release: std::sync::Mutex::new(release_rx),
            },
        ));
        let running_services = Arc::clone(&services);
        let caller = handle.spawn(async move { running_services.allocator_tick().await });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        // Simulate the next eligible tick without waiting thirty wall-clock seconds.
        services.allocator.seed_cache(serde_json::json!({}));
        let overlapping = Arc::clone(&services);
        let second = handle.spawn(async move { overlapping.allocator_tick().await });
        assert!(
            services.allocator.wait_for_active(2),
            "eligible ticks must not be single-flight"
        );
        caller.abort();
        // Break the later materialisation gate only after the allocation preflight.
        std::fs::write(dir.path().join("retired-panes.json"), "{").unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let stopping = Arc::clone(&services);
        let shutdown = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            stopping.shutdown();
            done_tx.send(()).unwrap();
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        release_tx.send(()).unwrap();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        shutdown.join().unwrap();
        let _ = handle.block_on(second);
        assert_eq!(
            handle.block_on(tasks.get(&task.id)).unwrap().status,
            "in_progress"
        );
        let handoffs = handle
            .block_on(tasks.next_job_handoffs(Some("w1:p1")))
            .unwrap();
        assert_eq!(handoffs.len(), 1);
        assert_eq!(handoffs[0]["state"], "pending");
    }

    #[test]
    fn runtime_allocator_refuses_foreign_caller_unread_and_blocked_screen() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let task = handle
            .block_on(tasks.add("global", "guarded offer", "acceptance"))
            .unwrap();
        handle
            .block_on(tasks.update(
                &task.id,
                crate::tasks::FieldEdit {
                    assignees: Some(vec!["w2:p2".into()]),
                    ..Default::default()
                },
                None,
            ))
            .unwrap();
        handle
            .block_on(tasks.allocation_offer(&task.id, "code", &serde_json::json!([]), "w2:p2"))
            .unwrap();
        handle
            .block_on(tasks.allocation_register(
                "w1:p1",
                &serde_json::json!(["code"]),
                &serde_json::json!(["worker"]),
                &serde_json::json!([]),
                "w2:p2",
                &serde_json::json!(true),
            ))
            .unwrap();
        let stores = Stores {
            tasks: Some(Arc::clone(&tasks)),
            inbox: Some(Arc::clone(&inbox)),
            ..Stores::default()
        };
        let env = configured_env(&dir, "1");
        let factory: LoopTerminalFactory = Arc::new(|| {
            let mut terminal = FakeTerminal::new();
            terminal.panes[0].command = "codex".into();
            terminal.panes[0].pane_pid = Some(i64::from(std::process::id()));
            terminal
                .screens
                .insert("w1:p1".into(), "Session expired\n".into());
            Box::new(terminal)
        });
        let services =
            RuntimeServices::new(startup_inputs(&dir, runtime, env, factory, stores)).unwrap();
        let set_ended = |caller: serde_json::Value| {
            services.flock().set_caller(Some(caller));
            services
                .flock()
                .turn_start(
                    None,
                    None,
                    None,
                    Some("w1:p1"),
                    None,
                    &crate::flock::commands_turns::NoHooks,
                )
                .unwrap();
            services
                .flock()
                .turn_end(
                    None,
                    "done",
                    Some("w1:p1"),
                    None,
                    false,
                    &crate::flock::commands_turns::NoHooks,
                )
                .unwrap();
            services.flock().clear_caller();
        };
        set_ended(serde_json::json!({"pid":i32::MAX, "start":"1"}));
        services
            .flock()
            .turn_states
            .lock()
            .unwrap()
            .get_mut("w1:p1")
            .unwrap()
            .ended_at = Some(0.0);
        let zero_end = handle.block_on(services.allocator_tick()).unwrap();
        assert_eq!(zero_end["workers"]["w1:p1"], "not_idle");
        set_ended(serde_json::json!({"pid":i32::MAX, "start":"1"}));
        services.allocator.seed_cache(serde_json::json!({}));
        let foreign = handle.block_on(services.allocator_tick()).unwrap();
        assert_eq!(foreign["workers"]["w1:p1"], "caller_proc_missing");
        assert_eq!(foreign["allocated"], serde_json::json!([]));
        let pid = i64::from(std::process::id());
        set_ended(
            serde_json::json!({"pid":pid,"start":process_start(pid),"name":"codex","pane_id":"w1:p1","source":"agent"}),
        );
        inbox
            .send(&crate::inbox_store::Outgoing {
                mailbox: "π - work",
                pane_id: Some("w1:p1"),
                body: Some("unread"),
                ..Default::default()
            })
            .unwrap();
        services.allocator.seed_cache(serde_json::json!({}));
        let unread = handle.block_on(services.allocator_tick()).unwrap();
        assert_eq!(unread["workers"]["w1:p1"], "unread_or_unknown");
        inbox.read("π - work", 10, false, true).unwrap();
        services.allocator.seed_cache(serde_json::json!({}));
        let blocked = handle.block_on(services.allocator_tick()).unwrap();
        assert_eq!(blocked["workers"]["w1:p1"], "delivery_not_ready");
        assert_eq!(blocked["allocated"], serde_json::json!([]));
        assert_eq!(handle.block_on(tasks.get(&task.id)).unwrap().status, "open");
        services.shutdown();
    }

    #[test]
    fn runtime_allocator_reports_missing_caller_without_claim() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        handle
            .block_on(tasks.allocation_register(
                "w1:p1",
                &serde_json::json!(["code"]),
                &serde_json::json!(["worker"]),
                &serde_json::json!([]),
                "w2:p2",
                &serde_json::json!(true),
            ))
            .unwrap();
        let stores = Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let env = configured_env(&dir, "1");
        let services = RuntimeServices::new(startup_inputs(
            &dir,
            runtime,
            env,
            fake_factory_with_current_pane(),
            stores,
        ))
        .unwrap();
        services.flock().clear_caller();
        services
            .flock()
            .turn_start(
                None,
                None,
                None,
                Some("w1:p1"),
                None,
                &crate::flock::commands_turns::NoHooks,
            )
            .unwrap();
        services
            .flock()
            .turn_end(
                None,
                "done",
                Some("w1:p1"),
                None,
                false,
                &crate::flock::commands_turns::NoHooks,
            )
            .unwrap();
        let result = handle.block_on(services.allocator_tick()).unwrap();
        assert_eq!(result["workers"]["w1:p1"], "caller_missing");
        assert_eq!(result["allocated"], serde_json::json!([]));
        services.shutdown();
    }

    #[test]
    fn runtime_allocator_error_preserves_previous_cache_and_entry_time() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let stores = Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let env = configured_env(&dir, "1");
        let services = RuntimeServices::new(startup_inputs(
            &dir,
            runtime,
            env,
            fake_factory_with_current_pane(),
            stores,
        ))
        .unwrap();
        let connection = crate::db::open_writer(&dir.path().join("tasks.db")).unwrap();
        connection
            .execute_batch("DROP TABLE allocation_workers")
            .unwrap();
        services
            .allocator
            .seed_cache(serde_json::json!({"old":true}));
        assert!(handle.block_on(services.allocator_tick()).is_err());
        assert_eq!(
            handle.block_on(services.allocator_tick()).unwrap(),
            serde_json::json!({"old":true})
        );
        services.shutdown();
    }

    #[test]
    fn runtime_allocator_preserves_busy_budget_without_holding_flock_locks() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        handle
            .block_on(tasks.allocation_register(
                "w1:p1",
                &serde_json::json!(["code"]),
                &serde_json::json!(["worker"]),
                &serde_json::json!([]),
                "w2:p2",
                &serde_json::json!(true),
            ))
            .unwrap();
        let stores = Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let env = configured_env(&dir, "0");
        let services = RuntimeServices::new(startup_inputs(
            &dir,
            runtime,
            env,
            fake_factory_with_current_pane(),
            stores,
        ))
        .unwrap();
        let blocker = crate::db::open_writer(&dir.path().join("tasks.db")).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = std::time::Instant::now();
        let result = handle
            .block_on(async {
                tokio::time::timeout(
                    std::time::Duration::from_secs(15),
                    services.allocator_tick(),
                )
                .await
            })
            .unwrap();
        assert!(matches!(result, Err(DunkError::Busy)));
        assert!(started.elapsed() >= std::time::Duration::from_millis(crate::db::BUSY_TIMEOUT_MS));
        blocker.execute_batch("ROLLBACK").unwrap();
        services.shutdown();
    }

    #[test]
    fn runtime_allocator_cancel_before_actor_enqueue_does_not_fabricate_commit() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let task = handle
            .block_on(tasks.add("global", "offered", "acceptance"))
            .unwrap();
        handle
            .block_on(tasks.update(
                &task.id,
                crate::tasks::FieldEdit {
                    assignees: Some(vec!["w2:p2".into()]),
                    ..Default::default()
                },
                None,
            ))
            .unwrap();
        handle
            .block_on(tasks.allocation_offer(&task.id, "code", &serde_json::json!([]), "w2:p2"))
            .unwrap();
        handle
            .block_on(tasks.allocation_register(
                "w1:p1",
                &serde_json::json!(["code"]),
                &serde_json::json!(["worker"]),
                &serde_json::json!([]),
                "w2:p2",
                &serde_json::json!(true),
            ))
            .unwrap();
        let stores = Stores {
            tasks: Some(Arc::clone(&tasks)),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let env = configured_env(&dir, "1");
        let factory: LoopTerminalFactory = Arc::new(|| {
            let mut terminal = FakeTerminal::new();
            terminal.panes[0].command = "codex".into();
            terminal.panes[0].pane_pid = Some(i64::from(std::process::id()));
            terminal
                .screens
                .insert("w1:p1".into(), "› Ask Codex to do anything\n".into());
            Box::new(terminal)
        });
        let services =
            RuntimeServices::new(startup_inputs(&dir, runtime, env, factory, stores)).unwrap();
        let pid = i64::from(std::process::id());
        services.flock().set_caller(Some(serde_json::json!({"pid":pid,"start":process_start(pid),"name":"codex","pane_id":"w1:p1","source":"agent"})));
        services
            .flock()
            .turn_start(
                None,
                None,
                None,
                Some("w1:p1"),
                None,
                &crate::flock::commands_turns::NoHooks,
            )
            .unwrap();
        services
            .flock()
            .turn_end(
                None,
                "done",
                Some("w1:p1"),
                None,
                false,
                &crate::flock::commands_turns::NoHooks,
            )
            .unwrap();
        services.flock().clear_caller();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        services
            .allocator
            .set_before_enqueue(entered_tx, release_rx);
        let running = Arc::clone(&services);
        let caller = handle.spawn(async move { running.allocator_tick().await });
        entered_rx
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap();
        caller.abort();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let stopping = Arc::clone(&services);
        let shutdown = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            stopping.shutdown();
            done_tx.send(()).unwrap();
        });
        started_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !services.is_stopped() {
            assert!(
                std::time::Instant::now() < deadline,
                "shutdown did not start"
            );
            std::thread::yield_now();
        }
        assert!(matches!(
            done_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        release_tx.send(()).unwrap();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        shutdown.join().unwrap();
        assert_eq!(handle.block_on(tasks.get(&task.id)).unwrap().status, "open");
        assert!(handle
            .block_on(tasks.next_job_handoffs(Some("w1:p1")))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn runtime_allocator_obeys_disabled_gate_with_real_services() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        handle
            .block_on(tasks.allocation_register(
                "w1:p1",
                &serde_json::json!(["code"]),
                &serde_json::json!(["worker"]),
                &serde_json::json!([]),
                "w2:p2",
                &serde_json::json!(true),
            ))
            .unwrap();
        let stores = Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let env = configured_env(&dir, "0");
        let services = RuntimeServices::new(startup_inputs(
            &dir,
            runtime,
            env,
            fake_factory_with_current_pane(),
            stores,
        ))
        .unwrap();
        let result = handle.block_on(services.allocator_tick()).unwrap();
        assert_eq!(result["workers"]["w1:p1"], "disabled");
        assert_eq!(result["allocated"], serde_json::json!([]));
        services.shutdown();
    }

    #[test]
    fn runtime_allocator_no_workers_and_shutdown_use_owned_services() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let handle = runtime.handle().clone();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let stores = Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            ..Stores::default()
        };
        let env = configured_env(&dir, "1");
        let services = RuntimeServices::new(startup_inputs(
            &dir,
            runtime,
            env,
            fake_factory_with_current_pane(),
            stores,
        ))
        .unwrap();
        services
            .allocator
            .seed_cache(serde_json::json!({"old":true}));
        assert_eq!(
            handle.block_on(services.allocator_tick()).unwrap(),
            serde_json::json!({})
        );
        assert_eq!(
            handle.block_on(services.allocator_tick()).unwrap(),
            serde_json::json!({"old":true})
        );
        services.shutdown();
        assert!(handle.block_on(services.allocator_tick()).is_err());
    }

    #[test]
    fn runtime_services_owns_one_configured_flock_and_canonical_managers() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let task = runtime
            .block_on(tasks.add("global", "startup-owned handoff", "body"))
            .unwrap();
        runtime
            .block_on(tasks.assign(&task.id, &["w1:p1"], "set"))
            .unwrap();
        let env = configured_env(&dir, "1");
        let worker_threads = Arc::new(Mutex::new(Vec::new()));
        let called_threads = Arc::clone(&worker_threads);
        let factory: LoopTerminalFactory = Arc::new(move || {
            called_threads
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            let mut terminal = FakeTerminal::new();
            terminal.panes[0].command = "codex".into();
            terminal.panes[0].pane_pid = Some(i64::from(std::process::id()));
            Box::new(terminal)
        });
        let stores = Stores {
            tasks: Some(Arc::clone(&tasks)),
            inbox: Some(Arc::clone(&inbox)),
            ..Stores::default()
        };
        let inputs = startup_inputs(&dir, runtime, env.clone(), factory, stores);
        let state_file = inputs.state_file.clone();
        let services = RuntimeServices::new(inputs).unwrap();

        assert!(Arc::ptr_eq(&services.flock, &services.outbox.flock));
        assert!(services.next_jobs.owns_flock(&services.flock));
        let attached = services.flock.stores();
        assert!(Arc::ptr_eq(attached.tasks.as_ref().unwrap(), &tasks));
        assert!(Arc::ptr_eq(attached.inbox.as_ref().unwrap(), &inbox));
        assert!(attached.metrics.is_some());
        let state = services.flock.status_state().unwrap();
        assert_eq!(state.version, "test-release");
        assert_eq!(state.state_file, state_file);
        assert_eq!(state.env.get("DUNKINGSHEEP_DISPATCH_LANES"), Some("w1:p1"));

        let pid = i64::from(std::process::id());
        let caller = Caller::new(
            Some(pid),
            process_start(pid),
            Some("codex".into()),
            Some("w1:p1".into()),
            "peer",
        );
        let result = services
            .runtime
            .block_on(
                services
                    .next_jobs
                    .advance("w1:p1", "turn.end", None, Some(caller), None),
            )
            .unwrap();
        assert_eq!(
            result.plan.get("action").and_then(Value::as_str),
            Some("claim")
        );
        assert_eq!(
            result.plan.get("task_id").and_then(Value::as_str),
            Some(task.id.as_str())
        );

        let leases = services
            .runtime
            .block_on(tasks.leases_for_pane("w1:p1", None))
            .unwrap();
        assert!(leases
            .iter()
            .any(|lease| lease.task_id == task.id && lease.caller_pid == Some(pid)));
        let handoffs = services
            .runtime
            .block_on(tasks.next_job_handoffs(Some("w1:p1")))
            .unwrap();
        assert_eq!(handoffs.len(), 1);
        let message_id = handoffs[0]["message_id"].as_str().unwrap();
        let status =
            crate::inbox::status(inbox.read_pool(), message_id, crate::clock::now_s()).unwrap();
        assert_eq!(status.pane_id.as_deref(), Some("w1:p1"));
        let thread_ids = worker_threads.lock().unwrap();
        assert!(
            !thread_ids.is_empty(),
            "canonical factory must serve next-job work"
        );
        assert!(thread_ids
            .iter()
            .all(|id| *id != std::thread::current().id()));
        drop(thread_ids);
        services.shutdown();
    }

    #[test]
    fn runtime_dispatcher_binds_actual_startup_services() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let task = runtime
            .block_on(tasks.add("global", "observe actual turn", "body"))
            .unwrap();
        let message_id = "nj-00000000000000000000000000000001";
        let attempt_at = crate::clock::now_s() - 2.0;
        let details = json!({
            "state":"sent",
            "attempt":1,
            "attempt_at":attempt_at,
            "observed_turn":null,
            "transport_id":"nj-00000000000000000000000000000001:1"
        })
        .as_object()
        .unwrap()
        .clone();
        runtime
            .block_on(tasks.record_next_job_handoff(
                &task.id,
                "w1:p1",
                None,
                Some(message_id),
                1,
                None,
                Some(attempt_at),
                Some(details),
            ))
            .unwrap();
        let env = configured_env(&dir, "0");
        let services = RuntimeServices::new(startup_inputs(
            &dir,
            runtime,
            env,
            fake_factory_with_current_pane(),
            Stores {
                tasks: Some(Arc::clone(&tasks)),
                inbox: Some(Arc::clone(&inbox)),
                ..Stores::default()
            },
        ))
        .unwrap();
        let mut handlers = api::Dispatcher::new();
        handlers.register_status_whoami_turn_start();
        let dispatcher = services.dispatcher(handlers);

        let pid = i64::from(std::process::id());
        let request = RequestCtx {
            caller: Some(Caller::new(
                Some(pid),
                process_start(pid),
                Some("codex".into()),
                Some("w1:p1".into()),
                "agent",
            )),
            self_pane_id: Some("w1:p1".into()),
            socket_peer: Some(true),
            ..RequestCtx::default()
        };
        let started = dispatcher
            .dispatch(
                "turn_start",
                &json!({"source":"startup-test", "handoff_id":message_id}),
                &request,
            )
            .unwrap();
        assert_eq!(started["pane_id"], "w1:p1");
        assert_eq!(services.flock.current_caller(), None);

        let handoffs = services
            .runtime
            .block_on(tasks.next_job_handoffs(Some("w1:p1")))
            .unwrap();
        assert_eq!(handoffs[0]["observed_turn"]["handoff_id"], message_id);
        assert_eq!(handoffs[0]["observed_turn"]["source"], "startup-test");
        let metrics = services.flock.stores().metrics.unwrap();
        let events = metrics
            .events_between(0.0, crate::clock::now_s() + 1.0)
            .unwrap();
        assert!(events.iter().any(|event| {
            event.kind == "turn.start" && event.pane_id.as_deref() == Some("w1:p1")
        }));
        dispatcher.close();
    }

    #[test]
    fn runtime_services_shutdown_preserves_store_runtime_lifetime() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::sync_channel(0);
        let release_rx = Mutex::new(release_rx);
        let factory: LoopTerminalFactory = Arc::new(move || {
            started_tx.send(()).unwrap();
            release_rx.lock().unwrap().recv().unwrap();
            Box::new(FakeTerminal::new())
        });
        let services = RuntimeServices::new(startup_inputs(
            &dir,
            runtime,
            Env::default(),
            factory,
            Stores {
                tasks: Some(Arc::clone(&tasks)),
                inbox: Some(Arc::clone(&inbox)),
                ..Stores::default()
            },
        ))
        .unwrap();
        let weak_flock = Arc::downgrade(&services.flock);
        let weak_tasks = Arc::downgrade(&tasks);
        let weak_inbox = Arc::downgrade(&inbox);
        let message = services
            .flock
            .new_message("w1:p1", "w1:p1", "pending worker", "test")
            .unwrap();
        services
            .flock
            .enqueue(
                message,
                services.outbox.pool.as_ref(),
                &super::super::outbox_runtime::FlockSnapshot,
            )
            .unwrap();
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("worker factory did not start");

        let service_for_shutdown = Arc::clone(&services);
        let (done_tx, done_rx) = mpsc::channel();
        let shutdown = std::thread::spawn(move || {
            service_for_shutdown.shutdown();
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(50)).is_err());
        assert!(Arc::ptr_eq(&services.flock, &services.outbox.flock));
        assert!(services.flock.stores().tasks.is_some());
        assert!(services.flock.stores().inbox.is_some());
        assert_eq!(services.runtime.block_on(async { 7 }), 7);

        release_tx.send(()).unwrap();
        done_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("shutdown did not join the pending outbox worker");
        shutdown.join().unwrap();
        services.shutdown();
        drop(tasks);
        drop(inbox);
        drop(services);
        assert!(weak_flock.upgrade().is_none());
        assert!(weak_tasks.upgrade().is_none());
        assert!(weak_inbox.upgrade().is_none());
    }

    #[test]
    fn runtime_services_refuses_missing_startup_resources_and_mismatched_adapters() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let mut inputs = startup_inputs(
            &dir,
            runtime,
            Env::default(),
            fake_factory_with_current_pane(),
            Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Stores::default()
            },
        );
        inputs.stores.tasks = None;
        assert!(RuntimeServices::new(inputs)
            .err()
            .unwrap()
            .to_string()
            .contains("task store is not attached"));

        let dir = tempfile::tempdir().unwrap();
        let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let mut inputs = startup_inputs(
            &dir,
            runtime,
            Env::default(),
            fake_factory_with_current_pane(),
            Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Stores::default()
            },
        );
        inputs.release_version.clear();
        assert!(RuntimeServices::new(inputs)
            .err()
            .unwrap()
            .to_string()
            .contains("release version must be configured"));

        let dir = tempfile::tempdir().unwrap();
        let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let mut inputs = startup_inputs(
            &dir,
            runtime,
            Env::default(),
            fake_factory_with_current_pane(),
            Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Stores::default()
            },
        );
        inputs.launches_file = inputs.lane_registry_file.clone();
        assert!(RuntimeServices::new(inputs)
            .err()
            .unwrap()
            .to_string()
            .contains("launches and lane registry files must be distinct"));

        for collide_launches in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
            let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
            let mut inputs = startup_inputs(
                &dir,
                runtime,
                Env::default(),
                fake_factory_with_current_pane(),
                Stores {
                    tasks: Some(tasks),
                    inbox: Some(inbox),
                    ..Stores::default()
                },
            );
            let executable = inputs.executable.clone();
            let executable_contents = std::fs::read(&executable).unwrap();
            if collide_launches {
                inputs.launches_file = executable.clone();
            } else {
                inputs.lane_registry_file = executable.clone();
            }
            assert!(RuntimeServices::new(inputs)
                .err()
                .unwrap()
                .to_string()
                .contains("distinct from executable identity"));
            assert_eq!(std::fs::read(executable).unwrap(), executable_contents);
        }

        let dir = tempfile::tempdir().unwrap();
        let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
        let (tasks, inbox) = tasks_and_inbox(&runtime, &dir);
        let services = RuntimeServices::new(startup_inputs(
            &dir,
            runtime,
            Env::default(),
            fake_factory_with_current_pane(),
            Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Stores::default()
            },
        ))
        .unwrap();
        let other = Flock::new(
            dir.path().join("other-launches"),
            dir.path().join("other-lanes"),
            false,
        );
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let request = RequestCtx::default();
        let adapter = crate::server_runtime::RequestRuntimeAdapter::new(
            &other,
            &request,
            services.runtime.handle(),
            &terminal,
        );
        assert!(adapter.with_outbox(&services.outbox).is_err());
        let adapter = crate::server_runtime::RequestRuntimeAdapter::new(
            &other,
            &request,
            services.runtime.handle(),
            &terminal,
        );
        assert!(adapter.with_next_jobs(&services.next_jobs).is_err());
        services.shutdown();
    }

    fn memory_inputs(dir: &tempfile::TempDir, env: Env, persist: bool) -> RuntimeServiceInputs {
        let runtime = Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let (tasks, inbox) = tasks_and_inbox(&runtime, dir);
        let mut inputs = startup_inputs(
            dir,
            runtime,
            env,
            fake_factory_with_current_pane(),
            Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Stores::default()
            },
        );
        inputs.persist_enabled = persist;
        inputs.observability.as_mut().unwrap().memory_db =
            Some(dir.path().join("memory/nested/memory.db"));
        inputs
    }

    #[test]
    fn runtime_memory_first_run_existing_db_and_exact_alias_survive_observability() {
        let dir = tempfile::tempdir().unwrap();
        let services = RuntimeServices::new(memory_inputs(&dir, Env::default(), true)).unwrap();
        let service = services.flock.memory_service().unwrap();
        assert!(Arc::ptr_eq(&service, services.memory.as_ref().unwrap()));
        assert!(Arc::ptr_eq(
            service.read_pool(),
            services.flock.stores().memory.as_ref().unwrap()
        ));
        assert_eq!(
            service.status().unwrap()["ingestor"]["throttle_s"],
            json!(0.1)
        );
        let worker = service.worker_id().unwrap();
        service.start().unwrap();
        assert_eq!(Some(worker), service.worker_id());
        // A late observability call must neither open this invalid path nor replace the alias.
        services
            .flock
            .wire_observability(&dir.path().join("metrics-later"), Some(dir.path()))
            .unwrap();
        assert!(Arc::ptr_eq(
            service.read_pool(),
            services.flock.stores().memory.as_ref().unwrap()
        ));
        let initial_stores = services.flock.stores();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let a = Arc::clone(&barrier);
        let flock = Arc::clone(&services.flock);
        let stores = initial_stores.clone();
        let attach_stores = std::thread::spawn(move || {
            a.wait();
            for _ in 0..30 {
                flock.attach_stores(stores.clone());
            }
        });
        let b = Arc::clone(&barrier);
        let flock = Arc::clone(&services.flock);
        let owner = Arc::clone(&service);
        let attach_service = std::thread::spawn(move || {
            b.wait();
            for _ in 0..30 {
                flock.attach_memory_service(Arc::clone(&owner)).unwrap();
            }
        });
        barrier.wait();
        attach_stores.join().unwrap();
        attach_service.join().unwrap();
        assert!(Arc::ptr_eq(
            service.read_pool(),
            services.flock.stores().memory.as_ref().unwrap()
        ));
        let path = service.path().to_owned();
        let source = dir.path().join("source.jsonl");
        std::fs::write(
            &source,
            "{\"role\":\"user\",\"text\":\"persisted memory needle\",\"session_id\":\"fixture\"}\n",
        )
        .unwrap();
        service.pause();
        service
            .enqueue(source.to_str().unwrap(), Some("generic"), None)
            .unwrap();
        services
            .runtime
            .block_on(services.shutdown_async())
            .unwrap();
        services.shutdown();
        assert!(service.worker_id().is_none());
        assert!(service.checkpoint().is_err());
        assert!(services
            .flock
            .attach_memory_service(Arc::clone(&service))
            .is_err());
        drop(services);
        let reopened =
            MemoryService::open(path, 2, Duration::from_millis(500), Duration::ZERO).unwrap();
        assert_eq!(reopened.stats().unwrap()["queue"]["queued"], 1);
        reopened.stop_and_join().unwrap();
    }

    #[test]
    fn runtime_memory_absence_and_startup_config_errors_are_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let services = RuntimeServices::new(memory_inputs(
            &dir,
            Env::from_pairs(&[("DUNKINGSHEEP_MEMORY_THROTTLE_S", "bad")]),
            false,
        ))
        .unwrap();
        assert!(services.memory.is_none());
        assert!(services.flock.memory_service().is_none());
        assert!(services.flock.stores().memory.is_none());
        assert!(!dir.path().join("memory").exists());
        services.shutdown();
        drop(services);
        let path = dir.path().join("new.db");
        for raw in ["", "wrong", "1__0"] {
            let result = RuntimeMemoryConfig::captured(
                path.clone(),
                &Env::from_pairs(&[("DUNKINGSHEEP_MEMORY_THROTTLE_S", raw)]),
            );
            assert!(result
                .err()
                .unwrap()
                .to_string()
                .contains("could not convert"));
        }
        for raw in [
            "-1",
            "NaN",
            "-inf",
            "Infinity",
            "1e300",
            "-0.0",
            "1e-300",
            " ١_٢.٥e-2 ",
        ] {
            let config = RuntimeMemoryConfig::captured(
                path.clone(),
                &Env::from_pairs(&[("DUNKINGSHEEP_MEMORY_THROTTLE_S", raw)]),
            )
            .unwrap();
            let expected = crate::github::parse_python_float(raw).unwrap();
            assert_eq!(config.poll, Duration::from_millis(500));
            assert!(
                config.throttle.to_bits() == expected.to_bits()
                    || (config.throttle.is_nan() && expected.is_nan())
            );
        }
        let mut missing = memory_inputs(&dir, Env::default(), true);
        missing.observability.as_mut().unwrap().memory_db = None;
        assert!(RuntimeServices::new(missing)
            .err()
            .unwrap()
            .to_string()
            .contains("startup-selected"));
        assert!(!path.exists());
        // An existing non-database must fail, never silently disable memory or replace it.
        let mut bad = memory_inputs(&dir, Env::default(), true);
        std::fs::write(&path, b"not sqlite").unwrap();
        bad.observability.as_mut().unwrap().memory_db = Some(path.clone());
        assert!(RuntimeServices::new(bad).is_err());
        assert_eq!(std::fs::read(path).unwrap(), b"not sqlite");
    }

    #[test]
    fn runtime_memory_partial_startup_failure_joins_worker_and_closes_writer() {
        let dir = tempfile::tempdir().unwrap();
        let inputs = memory_inputs(
            &dir,
            Env::from_pairs(&[("DUNKINGSHEEP_VIEW_CACHE_S", "bad")]),
            true,
        );
        let config = RuntimeMemoryConfig::captured(
            inputs
                .observability
                .as_ref()
                .unwrap()
                .memory_db
                .clone()
                .unwrap(),
            &inputs.env,
        )
        .unwrap();
        let mut observed = None;
        let error = RuntimeServices::build(inputs, Some(config), |owner| {
            let service = owner.as_ref().unwrap();
            assert!(service.worker_id().is_some());
            observed = Some(Arc::clone(service));
        })
        .err()
        .unwrap();
        assert!(error.to_string().contains("DUNKINGSHEEP_VIEW_CACHE_S"));
        let service = observed.unwrap();
        assert!(service.worker_id().is_none());
        assert!(service.start().is_err());
        assert!(service.checkpoint().is_err());
        service.stop_and_join().unwrap();
    }

    #[test]
    fn runtime_memory_final_join_waits_for_real_worker_after_python_stop_bound() {
        let dir = tempfile::tempdir().unwrap();
        let services = RuntimeServices::new(memory_inputs(&dir, Env::default(), true)).unwrap();
        let service = services.memory.as_ref().unwrap().clone();
        assert!(service.stop().unwrap());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let release_rx = Mutex::new(release_rx);
        service.set_parse_hook(Arc::new(move || {
            entered_tx.send(()).unwrap();
            release_rx
                .lock()
                .unwrap()
                .recv_timeout(Duration::from_secs(30))
                .unwrap();
        }));
        let source = dir.path().join("gated.jsonl");
        std::fs::write(&source, "{\"role\":\"user\",\"text\":\"gated message\"}\n").unwrap();
        service
            .enqueue(source.to_str().unwrap(), Some("generic"), None)
            .unwrap();
        service.start().unwrap();
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        let before = std::time::Instant::now();
        assert!(
            !service.stop().unwrap(),
            "busy Python-style stop retains worker"
        );
        assert!(before.elapsed() >= Duration::from_secs(5));
        assert!(service.worker_id().is_some());
        let (done_tx, done_rx) = mpsc::channel();
        let first = Arc::clone(&services);
        let tx = done_tx.clone();
        let join1 = std::thread::spawn(move || {
            first.shutdown();
            tx.send(()).unwrap();
        });
        let second = Arc::clone(&services);
        let join2 = std::thread::spawn(move || {
            second.shutdown();
            done_tx.send(()).unwrap();
        });
        assert!(done_rx.recv_timeout(Duration::from_millis(100)).is_err());
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while service.checkpoint().is_ok() {
            assert!(
                std::time::Instant::now() < deadline,
                "final stop did not begin"
            );
            std::thread::yield_now();
        }
        let flock = Arc::clone(&services.flock);
        let owner = Arc::clone(&service);
        let (attached_tx, attached_rx) = mpsc::channel();
        let attacher = std::thread::spawn(move || {
            attached_tx
                .send(flock.attach_memory_service(owner))
                .unwrap();
        });
        assert!(
            attached_rx
                .recv_timeout(Duration::from_millis(100))
                .is_err(),
            "attachment must serialize behind final join"
        );
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        join1.join().unwrap();
        join2.join().unwrap();
        assert!(attached_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .is_err());
        attacher.join().unwrap();
        assert!(service.worker_id().is_none());
        assert!(service.start().is_err());
        assert!(service
            .enqueue(source.to_str().unwrap(), None, None)
            .is_err());
        assert!(
            services.flock.stores().tasks.is_some(),
            "stores outlive joined worker"
        );
    }
}
