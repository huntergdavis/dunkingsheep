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
                "internal error: [Errno 2] No such file or directory: ''".into(),
            ));
        }
        StateStore::open(&self.0).map_err(crate::error::internal_error)
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
        let Ok(registry) = self.terminal.flock.with_roles(|roles, _| roles.describe()) else {
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
            .map_err(|e| crate::error::internal_error(DunkError::Refused(e)))?
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
            "internal error: invalid literal for int() with base 10: 'bad'"
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
            "internal error: Error binding parameter 1: type 'list' is not supported"
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
            "internal error: invalid literal for int() with base 10: 'bad'"
        );
        for token in ["1e999", "-1e999"] {
            let limit: Value = serde_json::from_str(token).unwrap();
            let error = dispatcher
                .dispatch(
                    "state_latest",
                    &json!({"limit":limit,"pane_id":[1]}),
                    &worker(),
                )
                .unwrap_err();
            assert_eq!(
                error.to_string(),
                "internal error: cannot convert float infinity to integer"
            );
        }
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
        assert_eq!(
            error.to_string(),
            "internal error: [Errno 2] No such file or directory: ''"
        );
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
    #[test]
    fn health_state_python_differential_replay_covers_every_request_shape() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;
        use std::process::Command;
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("not-created-yet");
        let services = services(dir.path(), config.to_str().unwrap());
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let traffic = dir.path().join("health-state.jsonl");
        // Real Python entry points create the expectations in an isolated configuration.
        let oracle = Command::new("python3").arg("-c").arg(r#"
import ast,json,os,pathlib,sys,types
root,traffic=sys.argv[1:]
sys.path.insert(0,root)
import dunk_core,dunk_health,dunk_roles,dunk_state,dunk_tasks
ns={}
module=ast.parse(pathlib.Path(root,'dunk_api.py').read_text())
for node in module.body:
    if isinstance(node,ast.FunctionDef) and node.name in ('h_state_capture','h_state_latest','h_health','_opt'):
        exec(compile(ast.Module(body=[node],type_ignores=[]),'dunk_api.py','exec'),ns)
store_dir=os.path.join(os.path.dirname(traffic),'oracle-stores')
os.makedirs(store_dir,exist_ok=True)
roles=dunk_roles.Roles(path=os.path.join(store_dir,'roles.json'),tokens_path=os.path.join(store_dir,'tokens.json'))
roles.register('w1:p1','worker',provider='codex')
tasks=dunk_tasks.Tasks(path=os.path.join(store_dir,'tasks.db'))
shim=types.SimpleNamespace(herdr=types.SimpleNamespace(list_panes=lambda:[]),roles=roles)
flock=types.SimpleNamespace(memory=None,resolve_lanes=lambda:[],roles=roles,tasks=tasks)
flock.registry_pane_gap=lambda:dunk_core.Flock.registry_pane_gap(shim)
flock.health=lambda **kw:dunk_health.run_health(flock,**kw)
dunk_state._proc_snapshot=lambda:({}, {})
with open(traffic,'w') as out:
    for cmd,args in [('health',{}),('state_capture',{'source':[1]}),('state_latest',{'limit':0}),('health',{'deep':True}),('health',{'repair':True}),('state_latest',{'limit':5}),('state_latest',{'pane_id':'w1:p1'}),('state_latest',{'project':'proj'})]:
        try: response={'ok':True,'result':ns['h_'+cmd](flock,args,None)}
        except Exception as e: response={'ok':False,'error':'internal error: '+str(e)}
        out.write(json.dumps({'cmd':cmd,'args':args,'response':response})+'\n')
"#).arg(&root).arg(&traffic)
            .env("HOME", dir.path()).env("DUNKINGSHEEP_DIR", &config).output().unwrap();
        assert!(
            oracle.status.success(),
            "{}",
            String::from_utf8_lossy(&oracle.stderr)
        );
        // Remove only this fixture's generated DB/directory, restoring identical initial state.
        std::fs::remove_dir_all(&config).unwrap();
        let socket = dir.path().join("replay.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        listener.set_nonblocking(true).unwrap();
        let dispatcher = services.production_dispatcher();
        let thread = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for _ in 0..8 {
                let deadline = std::time::Instant::now() + Duration::from_secs(30);
                let (mut stream, _) = loop {
                    match listener.accept() {
                        Ok(stream) => break stream,
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                std::time::Instant::now() < deadline,
                                "replay client did not connect"
                            );
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("replay accept: {error}"),
                    }
                };
                let mut line = String::new();
                BufReader::new(stream.try_clone().unwrap())
                    .read_line(&mut line)
                    .unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                let command = request["cmd"].as_str().unwrap();
                seen.push(command.to_owned());
                let response = match dispatcher.dispatch(command, &request["args"], &worker()) {
                    Ok(result) => json!({"ok":true,"result":result}),
                    Err(error) => json!({"ok":false,"error":error.to_string()}),
                };
                writeln!(stream, "{response}").unwrap();
            }
            dispatcher.close();
            seen
        });
        let result = Command::new("python3")
            .arg(root.join("tools/diff_harness.py"))
            .args(["replay", "--traffic"])
            .arg(&traffic)
            .arg("--against")
            .arg(&socket)
            .args(["--commands", "health,state_capture,state_latest"])
            .output()
            .unwrap();
        let seen = thread.join().unwrap();
        // Python order: quick health, state_capture, state_latest limit 0, deep health, repair
        // health, then state_latest with a positive limit, a pane filter and a project filter
        // (t3571). The Python oracle runs a real roles registry and task store (the same lane and
        // empty task store the Rust services hold), and the real Flock.registry_pane_gap over an
        // empty pane list. Filters run over an empty snapshot, so they prove argument handling
        // and ordering, not row selection.
        assert_eq!(
            seen,
            [
                "health",
                "state_capture",
                "state_latest",
                "health",
                "health",
                "state_latest",
                "state_latest",
                "state_latest"
            ]
        );
        assert!(
            result.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        assert!(String::from_utf8_lossy(&result.stdout)
            .contains("replayed 8 request(s): 0 diff(s), 0 error(s)"));
    }
    #[test]
    fn health_state_core_capture_uses_real_tasks_and_one_fake_process_snapshot() {
        use crate::state::proc::{Kids, ProcReader, Starts};
        use std::{cell::Cell, collections::BTreeMap, sync::Mutex};
        #[derive(Default)]
        struct NoProc(Cell<usize>);
        impl ProcReader for NoProc {
            fn proc_children(&self) -> Kids {
                panic!("extra process walk")
            }
            fn proc_snapshot(&self) -> (Kids, Starts) {
                self.0.set(self.0.get() + 1);
                (Kids::new(), Starts::new())
            }
            fn proc_start_ticks(&self, _: i64) -> Option<i64> {
                None
            }
            fn cmdline(&self, _: i64) -> Vec<String> {
                panic!("no process")
            }
            fn open_session_file(&self, _: i64) -> Option<String> {
                panic!("no process")
            }
            fn env_of(&self, _: i64) -> BTreeMap<String, String> {
                panic!("no process")
            }
            fn proc_start(&self, _: i64) -> Option<f64> {
                None
            }
            fn active_session_file(
                &self,
                _: Option<&str>,
                _: Option<f64>,
            ) -> (Option<String>, usize) {
                (None, 0)
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let services = services(dir.path(), dir.path().to_str().unwrap());
        let tasks = services.flock().stores().tasks.unwrap();
        let handle = services.runtime_handle();
        let task = handle
            .block_on(tasks.add("global", "capture lease", "body"))
            .unwrap();
        handle.block_on(tasks.insert_lease(&task.id, "w1:p1", 1.0, 1.0, 100.0));
        let mut terminal = FakeTerminal::new();
        terminal.panes.truncate(1);
        terminal.panes[0].command = "codex".into();
        terminal.panes[0].pane_pid = None;
        let terminal: Mutex<Box<dyn crate::terminal::Terminal>> = Mutex::new(Box::new(terminal));
        let request = worker();
        let adapter = RequestRuntimeAdapter::new(services.flock(), &request, handle, &terminal);
        let env = CoreCaptureEnv {
            adapter: &adapter,
            now: 2.0,
        };
        let proc = NoProc::default();
        let store = StateStore::open(dir.path().join("agent-state.db")).unwrap();
        let result = crate::state::capture::capture_raw(
            &env,
            &proc,
            &store,
            dir.path(),
            &json!(123),
            Some(2.0),
        )
        .unwrap();
        assert_eq!(result, json!({"at":2.0,"rows":1,"source":123}));
        assert_eq!(proc.0.get(), 1);
        let rows = store.latest(None, None, 10).unwrap();
        assert_eq!(rows[0]["task_id"], task.id);
        assert_eq!(rows[0]["source"], "123");
        assert_eq!(rows[0]["harness"], "codex");
        assert_eq!(rows[0]["session_id"], Value::Null);
        assert!(rows[0]["session_reason"].is_string());
        services.shutdown();
    }
}
