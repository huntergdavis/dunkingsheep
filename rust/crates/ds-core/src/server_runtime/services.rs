//! Process-owned runtime resources shared by request dispatch and background delivery.
//!
//! This component deliberately stops at resource ownership and request dispatch. The
//! daemon entry point must still bind its remaining runners and drain/join connection
//! threads before dropping these services (the final integration gate is t1605).

use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};

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

/// The concrete resources captured once at daemon startup.
///
/// `stores` must contain the already opened task and inbox stores. When `observability` is
/// present, its explicit paths open metrics and the optional memory read pool; otherwise a
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
    stopped: AtomicBool,
}

impl RuntimeServices {
    /// Build the one canonical Flock and its two request/background managers.
    pub fn new(inputs: RuntimeServiceInputs) -> Result<Arc<Self>, DunkError> {
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
            flock
                .wire_observability(&observability.state_dir, observability.memory_db.as_deref())?;
        }
        flock.attach_roles(inputs.roles, inputs.legacy_superusers);
        flock.attach_status_state(StatusState::at_start(
            inputs.release_version,
            &inputs.state_file,
            &inputs.executable,
            inputs.env.clone(),
        ))?;

        let runtime = inputs.runtime;
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
            stopped: AtomicBool::new(false),
        }))
    }

    /// Build the stuck observer adapter with the canonical Flock, outbox, and reporter.
    /// The ordinary loop-adapter constructor intentionally does not imply these services.
    #[allow(dead_code)] // The dependent startup integration consumes this staged binder.
    pub(crate) fn stuck_loop_adapter(self: &Arc<Self>) -> Result<LoopRuntimeAdapter, DunkError> {
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
                let delivery =
                    flock.delivery_state(terminal, &pane, Some("idle"), Some(&info), None);
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
    /// its request threads (the remaining t1605 shutdown gate).
    pub fn shutdown(&self) {
        if !self.stopped.swap(true, Ordering::AcqRel) {
            self.allocator.stop_and_join();
            self.outbox.stop_and_join();
        }
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

    fn fake_factory_with_current_pane() -> LoopTerminalFactory {
        Arc::new(|| {
            let mut terminal = FakeTerminal::new();
            terminal.panes[0].command = "codex".into();
            terminal.panes[0].pane_pid = Some(i64::from(std::process::id()));
            Box::new(terminal)
        })
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
}
