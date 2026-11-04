//! Request-thread health/state composition over the canonical Flock services.
use super::RequestRuntimeAdapter;
use crate::api::{health_state::HealthStateEnv, CommandEnv};
use crate::error::DunkError;
use crate::health::HealthEnv;
use crate::state::{
    capture::{CaptureEnv, LaneActivity},
    proc::LiveProc,
    StateStore,
};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// Deliberately opened at command execution, not daemon startup. A failed open is retried on
/// the next request, and every open runs the existing non-destructive schema migration.
struct LazyStateStore(PathBuf);
impl LazyStateStore {
    fn open(&self) -> Result<StateStore, DunkError> {
        // Python os.makedirs(dirname('agent-state.db')) rejects an empty dirname.
        if self.0.parent().is_some_and(|p| p.as_os_str().is_empty()) {
            return Err(DunkError::Refused(
                "[Errno 2] No such file or directory: ''".into(),
            ));
        }
        StateStore::open(&self.0)
    }
}

struct CoreHealthEnv<'a, 'b> {
    adapter: &'a RequestRuntimeAdapter<'b>,
    memory: Option<Arc<crate::memory::service::MemoryService>>,
}
impl CoreHealthEnv<'_, '_> {
    fn memory(&self) -> Result<&crate::memory::service::MemoryService, DunkError> {
        self.memory
            .as_deref()
            .ok_or_else(|| DunkError::Refused("memory service disabled".into()))
    }
}
impl HealthEnv for CoreHealthEnv<'_, '_> {
    fn memory_enabled(&self) -> bool {
        self.memory.is_some()
    }
    fn memory_stats(&self) -> Result<Value, String> {
        self.memory()
            .and_then(|m| m.stats())
            .map_err(|e| e.to_string())
    }
    fn memory_status(&self) -> Result<Value, String> {
        self.memory()
            .and_then(|m| m.status())
            .map_err(|e| e.to_string())
    }
    fn recover_leases(&self) -> Result<(), DunkError> {
        self.memory()?.recover_leases().map(|_| ())
    }
    fn requeue_partial(&self) -> Result<(), DunkError> {
        self.memory()?.requeue_partial().map(|_| ())
    }
    fn checkpoint(&self) -> Result<(), DunkError> {
        self.memory()?.checkpoint().map(|_| ())
    }
    fn task_totals(&self) -> Result<Value, String> {
        (|| {
            let tasks = self
                .adapter
                .terminal
                .flock
                .stores()
                .tasks
                .ok_or_else(|| DunkError::Refused("task store is not wired".into()))?;
            let stats = self
                .adapter
                .handler_context()
                .block_on(tasks.stats(None, None))??;
            Ok(stats.get("totals").cloned().unwrap_or_else(|| json!({})))
        })()
        .map_err(|e: DunkError| e.to_string())
    }
    fn registry_pane_gap(&self) -> Option<Result<Vec<Value>, String>> {
        Some(Ok(self.adapter.health_registry_gap()))
    }
}

struct CoreCaptureEnv<'a, 'b> {
    adapter: &'a RequestRuntimeAdapter<'b>,
    now: f64,
}
impl CaptureEnv for CoreCaptureEnv<'_, '_> {
    fn resolve_lanes(&self) -> Result<Vec<Value>, DunkError> {
        self.adapter
            .terminal
            .flock
            .resolve_lanes(&self.adapter.terminal, self.now)
    }
    fn activity(&self, lane: &Value, now: f64) -> LaneActivity {
        let record = self.adapter.terminal.flock.roster_activity(lane, now);
        let text = |key| record.get(key).and_then(Value::as_str).map(str::to_owned);
        LaneActivity {
            state: text("state"),
            source: text("source"),
            reason: text("reason"),
        }
    }
    fn lease_task_id(&self, pane_id: &str, now: f64) -> Option<String> {
        // Only this lookup is swallowed by dunk_state.capture.
        let tasks = self.adapter.terminal.flock.stores().tasks?;
        self.adapter
            .handler_context()
            .block_on(tasks.leases_for_pane(pane_id, Some(now)))
            .ok()?
            .ok()?
            .first()
            .map(|lease| lease.task_id.clone())
    }
}

impl RequestRuntimeAdapter<'_> {
    fn health_registry_gap(&self) -> Vec<Value> {
        let Ok(terminal) = self.terminal.terminal.lock() else {
            return vec![];
        };
        let live: std::collections::BTreeSet<_> = terminal
            .list_panes()
            .into_iter()
            .map(|p| p.pane_id)
            .collect();
        drop(terminal);
        let Ok(registry) = self.terminal.flock.lane_registry(false) else {
            return vec![];
        };
        let Some(lanes) = registry.get("lanes").and_then(Value::as_object) else {
            return vec![];
        };
        let mut lanes: Vec<_> = lanes.iter().collect();
        lanes.sort_by_key(|(id, _)| *id);
        lanes.into_iter().filter(|(id, _)| !live.contains(*id)).map(|(id, entry)| {
            let pid = entry.get("pane_pid").and_then(Value::as_i64);
            json!({"pane":id, "label":entry.get("label"), "role":entry.get("role"),
                "pid":pid, "alive":pid.is_some_and(|pid| crate::proc::process_alive(pid, entry.get("start").and_then(Value::as_str)))})
        }).collect()
    }

    fn health_state_paths(&self, health: bool) -> Result<(PathBuf, PathBuf), DunkError> {
        let state = self.terminal.flock.status_state()?;
        // Both Python modules expand HOME eagerly at import; override selection differs.
        let home = state
            .python_home
            .expanduser_home()
            .map_err(DunkError::Refused)?
            .unwrap_or_else(|| std::path::Path::new("~"))
            .to_path_buf();
        let configured = state
            .env
            .get("DUNKINGSHEEP_DIR")
            .filter(|value| !health || !value.is_empty());
        let directory = configured
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config/dunkingsheep"));
        Ok((home, directory))
    }
}

impl HealthStateEnv for RequestRuntimeAdapter<'_> {
    fn health(&self, repair: bool, deep: bool) -> Result<Value, DunkError> {
        let (_, directory) = self.health_state_paths(true)?;
        let env = CoreHealthEnv {
            adapter: self,
            memory: self.terminal.flock.memory_service(),
        };
        crate::health::run_health(
            &env,
            repair,
            deep,
            &directory,
            self.now_s(),
            &crate::health::Thresholds::default(),
        )
    }
    fn capture(&self, source: &Value) -> Result<Value, DunkError> {
        let (home, directory) = self.health_state_paths(false)?;
        let store = LazyStateStore(directory.join("agent-state.db")).open()?;
        let now = self.now_s();
        let env = CoreCaptureEnv { adapter: self, now };
        let reader = LiveProc::new(home);
        crate::state::capture::capture_raw(&env, &reader, &store, &directory, source, Some(now))
    }
    fn with_state_store(
        &self,
        operation: &mut dyn FnMut(&StateStore) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError> {
        let (_, directory) = self.health_state_paths(false)?;
        let store = LazyStateStore(directory.join("agent-state.db")).open()?;
        operation(&store)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Env;
    use crate::flock::panes::Stores;
    use crate::roles::Roles;
    use crate::server::RequestCtx;
    use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
    use crate::terminal::fakes::FakeTerminal;
    use std::time::Duration;

    fn services(dir: &std::path::Path, state_dir: &str) -> Arc<RuntimeServices> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(crate::tasks::Tasks::open(dir.join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let inbox = Arc::new(
            crate::inbox_store::Store::open(dir.join("inbox.db"), dir.join("blobs")).unwrap(),
        );
        let env = Env::from_pairs(&[
            ("HOME", dir.to_str().unwrap()),
            ("DUNKINGSHEEP_DIR", state_dir),
        ]);
        let mut roles = Roles::new(
            &env,
            Some(dir.join("roles.json")),
            Some(dir.join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register(
                "w1:p1",
                "worker",
                None,
                None,
                Some("codex"),
                None,
                None,
                None,
            )
            .unwrap();
        let executable = dir.join("daemon");
        std::fs::write(&executable, b"fixture executable identity").unwrap();
        RuntimeServices::new(RuntimeServiceInputs {
            launches_file: dir.join("launches.json"),
            lane_registry_file: dir.join("lanes.json"),
            state_file: dir.join("dunks.json"),
            executable,
            persist_enabled: false,
            stores: Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Stores::default()
            },
            roles,
            legacy_superusers: None,
            release_version: "test".into(),
            env,
            observability: Some(RuntimeObservability {
                state_dir: dir.join("observability"),
                memory_db: None,
            }),
            runtime,
            terminal_factory: Arc::new(|| {
                let mut terminal = FakeTerminal::new();
                terminal.panes.clear(); // No live pane/process fixture is consulted.
                Box::new(terminal)
            }),
        })
        .unwrap()
    }

    fn worker() -> RequestCtx {
        RequestCtx {
            caller: Some(crate::proc::Caller::new(
                Some(123),
                Some("start".into()),
                Some("codex".into()),
                Some("w1:p1".into()),
                "agent",
            )),
            self_pane_id: Some("w1:p1".into()),
            socket_peer: Some(true),
            ..RequestCtx::default()
        }
    }

    #[test]
    fn health_state_dispatcher_uses_real_memory_and_tasks_without_mutating_reads() {
        let dir = tempfile::tempdir().unwrap();
        let services = services(dir.path(), dir.path().to_str().unwrap());
        let path = dir.path().join("memory.db");
        drop(crate::db::open_writer(&path).unwrap());
        let memory = Arc::new(
            crate::memory::service::MemoryService::open(
                &path,
                1,
                Duration::from_secs(1),
                Duration::ZERO,
            )
            .unwrap(),
        );
        services
            .flock()
            .attach_memory_service(Arc::clone(&memory))
            .unwrap();
        let tasks = services.flock().stores().tasks.unwrap();
        services
            .runtime_handle()
            .block_on(tasks.add("global", "fixture", "body"))
            .unwrap();
        let before = memory.stats().unwrap();
        let snapshot = services.flock().snapshot();
        let dispatcher = services.production_dispatcher();
        let result = dispatcher
            .dispatch("health", &json!({"repair":[], "deep":[1]}), &worker())
            .unwrap();
        assert_eq!(result["checks"]["integrity"], "ok");
        assert_eq!(result["checks"]["memory"]["path"], before["path"]);
        assert_eq!(
            result["checks"]["tasks"],
            services
                .runtime_handle()
                .block_on(tasks.stats(None, None))
                .unwrap()["totals"]
        );
        assert_eq!(result["repairs"], json!([]));
        assert!(Arc::ptr_eq(
            &memory,
            &services.flock().memory_service().unwrap()
        ));
        assert_eq!(memory.stats().unwrap(), before);
        assert_eq!(services.flock().snapshot(), snapshot);
        assert!(!dir.path().join("health.json").exists());
        dispatcher.close();
    }

    #[test]
    fn health_state_dispatcher_disabled_memory_and_lazy_store_conversion_order() {
        let dir = tempfile::tempdir().unwrap();
        let services = services(dir.path(), dir.path().to_str().unwrap());
        let dispatcher = services.production_dispatcher();
        let health = dispatcher
            .dispatch("health", &json!({}), &worker())
            .unwrap();
        assert!(health["problems"]
            .as_array()
            .unwrap()
            .contains(&json!("memory service disabled")));
        assert!(health["checks"].get("memory").is_none());
        let path = dir.path().join("agent-state.db");
        assert!(!path.exists());
        let error = dispatcher
            .dispatch("state_latest", &json!({"limit":"bad"}), &worker())
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid literal for int() with base 10: 'bad'"
        );
        assert!(
            path.is_file(),
            "Python creates the store before int conversion"
        );
        let store = StateStore::open(&path).unwrap();
        store
            .add(&[json!({"pane_id":"1", "project":"2", "source":123})], 1.0)
            .unwrap();
        for limit in [
            Value::Null,
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
            json!(-1),
            json!(" １_０ "),
        ] {
            let result = dispatcher
                .dispatch(
                    "state_latest",
                    &json!({"pane_id":1,"project":2,"limit":limit}),
                    &worker(),
                )
                .unwrap();
            assert_eq!(result["count"], 1);
            assert_eq!(result["rows"][0]["source"], "123");
        }
        let error = dispatcher
            .dispatch(
                "state_latest",
                &json!({"pane_id":[1],"limit":"999999999999999999999999999"}),
                &worker(),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "Error binding parameter 1: type 'list' is not supported"
        );
        let error = dispatcher
            .dispatch(
                "state_latest",
                &json!({"pane_id":[1],"limit":"bad"}),
                &worker(),
            )
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid literal for int() with base 10: 'bad'"
        );
        dispatcher.close();
    }

    #[test]
    fn health_state_empty_config_fails_open_before_invalid_limit() {
        let dir = tempfile::tempdir().unwrap();
        let services = services(dir.path(), "");
        let dispatcher = services.production_dispatcher();
        let error = dispatcher
            .dispatch("state_latest", &json!({"limit":[1]}), &worker())
            .unwrap_err();
        assert_eq!(error.to_string(), "[Errno 2] No such file or directory: ''");
        dispatcher.close();
    }
    #[test]
    fn health_state_repair_uses_canonical_serialized_memory_writer() {
        let dir = tempfile::tempdir().unwrap();
        let services = services(dir.path(), dir.path().to_str().unwrap());
        let path = dir.path().join("memory.db");
        let sample = dir.path().join("sample.jsonl");
        std::fs::write(&sample, "{\"type\":\"user\"}\n").unwrap();
        {
            let mut connection = crate::db::open_writer(&path).unwrap();
            crate::memory::ingest::apply_full_schema(&connection).unwrap();
            crate::memory::ingest::enqueue(
                &mut connection,
                sample.to_str().unwrap(),
                Some("generic"),
                None,
            )
            .unwrap();
            crate::memory::ingest::claim_next(&mut connection, 60.0).unwrap();
            connection
                .execute("UPDATE ingest_queue SET lease_until=0", [])
                .unwrap();
        }
        let memory = Arc::new(
            crate::memory::service::MemoryService::open(
                &path,
                1,
                Duration::from_secs(1),
                Duration::ZERO,
            )
            .unwrap(),
        );
        services
            .flock()
            .attach_memory_service(Arc::clone(&memory))
            .unwrap();
        let dispatcher = services.production_dispatcher();
        let dry = dispatcher
            .dispatch("health", &json!({"repair":false}), &worker())
            .unwrap();
        assert_eq!(dry["checks"]["stale_leases"]["expired_leases"], 1);
        let fixed = dispatcher
            .dispatch("health", &json!({"repair":"false"}), &worker())
            .unwrap();
        assert_eq!(
            fixed["repairs"],
            json!(["requeued 1 expired + 0 partial ingest lease(s)"])
        );
        assert_eq!(
            crate::health::stale_leases(&path, crate::clock::now_s()).unwrap()["expired_leases"],
            0
        );
        assert!(Arc::ptr_eq(
            &memory,
            &services.flock().memory_service().unwrap()
        ));
        dispatcher.close();
    }
}
