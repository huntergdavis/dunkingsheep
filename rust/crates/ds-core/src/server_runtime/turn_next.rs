//! Request-local binding to the shared next-job manager and complete turn hooks.
use super::RequestRuntimeAdapter;
use crate::error::DunkError;
use crate::flock::commands_turns::{Readiness, TurnDispatchEnv, TurnHooks, TurnState};
use crate::metrics::Event;
use serde_json::Value;
use std::collections::BTreeMap;

impl RequestRuntimeAdapter<'_> {
    fn next_jobs(&self) -> Result<&crate::tasks::next_job_runtime::NextJobs, DunkError> {
        self.next_jobs
            .ok_or_else(|| DunkError::Refused("request next-job service is not attached".into()))
    }
    fn helper_turndispatchenv_advance_next_job(&self, pane: &str) -> Result<Value, DunkError> {
        let caller = self.request.caller.clone();
        let result = self.handler_context().block_on(
            self.next_jobs()?
                .advance(pane, "turn.end", None, caller, None),
        )??;
        Ok(Value::Object(result.plan))
    }
    fn helper_turndispatchenv_open_handoffs(
        &self,
        pane: &str,
        inspect_tasks: bool,
    ) -> Result<bool, DunkError> {
        let tasks = self
            .terminal
            .flock
            .stores()
            .tasks
            .ok_or_else(|| DunkError::Refused("task store unavailable".into()))?;
        let handoffs = self
            .handler_context()
            .block_on(tasks.next_job_handoffs(Some(pane)))??;
        if !inspect_tasks {
            return Ok(false);
        }
        for handoff in handoffs {
            let id = handoff
                .get("task_id")
                .and_then(Value::as_str)
                .ok_or_else(|| DunkError::Refused("handoff task id missing".into()))?;
            let task = self.handler_context().block_on(tasks.get(id))??;
            if !matches!(task.status.as_str(), "done" | "cancelled") {
                return Ok(true);
            }
        }
        Ok(false)
    }
}
impl TurnDispatchEnv for RequestRuntimeAdapter<'_> {
    fn dispatch_enabled(&self) -> bool {
        self.helper_turndispatchenv_dispatch_enabled()
    }
    fn unread_count(&self, pane: &str) -> Result<Option<i64>, String> {
        self.helper_turndispatchenv_unread_count(pane)
    }
    fn claimed_tasks(&self, pane: &str) -> Result<Vec<Value>, String> {
        self.helper_turndispatchenv_claimed_tasks(pane)
    }
    fn turn_end_plan(
        &self,
        pane: &str,
        tasks: &[Value],
        unread: Option<i64>,
        last: Option<f64>,
        nudges: &BTreeMap<String, f64>,
        now: f64,
    ) -> Value {
        self.helper_turndispatchenv_turn_end_plan(pane, tasks, unread, last, nudges, now)
    }
    fn pending_outbox(&self, pane: &str) -> Vec<String> {
        self.helper_turndispatchenv_pending_outbox(pane)
    }
    fn readiness(&self, pane: &str, unread: Option<i64>) -> Result<Readiness, String> {
        self.helper_turndispatchenv_readiness(pane, unread)
    }
    fn turn_end_text(&self, task: &Value) -> String {
        self.helper_turndispatchenv_turn_end_text(task)
    }
    fn send_now(&self, pane: &str, text: &str) -> Result<Value, String> {
        self.helper_turndispatchenv_send_now(pane, text)
    }
    fn record(&self, pane: &str, outcome: &str, reason: Option<&str>, bytes: Option<usize>) {
        self.helper_turndispatchenv_record(pane, outcome, reason, bytes)
    }
    fn advance_next_job(&self, pane: &str) -> Result<Value, DunkError> {
        self.helper_turndispatchenv_advance_next_job(pane)
    }
    fn open_handoffs(&self, pane: &str, inspect_tasks: bool) -> Result<bool, DunkError> {
        self.helper_turndispatchenv_open_handoffs(pane, inspect_tasks)
    }
}
impl TurnHooks for RequestRuntimeAdapter<'_> {
    fn record(
        &self,
        kind: &str,
        pane: &str,
        source: Option<&str>,
        reason: Option<&str>,
        outcome: &str,
        at: f64,
        duration: Option<i64>,
    ) {
        if let Some(metrics) = self.terminal.flock.stores().metrics {
            metrics.record(&Event {
                kind: kind.into(),
                pane_id: Some(pane.into()),
                source: source.map(str::to_owned),
                reason: reason.map(str::to_owned),
                outcome: Some(outcome.into()),
                at,
                duration_ms: duration,
                ..Event::default()
            });
        }
    }
    fn observe_turn(&self, pane: &str, state: &TurnState) -> Result<(), DunkError> {
        self.handler_context().block_on(
            self.next_jobs()?
                .observe_turn(pane.to_owned(), state.clone()),
        )??;
        Ok(())
    }
    fn dispatch_on_turn_end(&self, pane: &str) -> Result<Value, DunkError> {
        self.terminal.flock.dispatch_on_turn_end(pane, self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flock::{panes::Stores, Flock};
    use crate::server::RequestCtx;
    use crate::tasks::Tasks;
    use crate::terminal::{fakes::FakeTerminal, Terminal};
    use std::sync::{Arc, Mutex};

    #[test]
    fn production_env_turn_next_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let task = runtime
            .block_on(tasks.add("global", "handoff", "body"))
            .unwrap();
        runtime
            .block_on(tasks.record_next_job_handoff(
                &task.id,
                "w1:p1",
                None,
                Some("i1"),
                1,
                None,
                None,
                Some(serde_json::Map::from_iter([(
                    "state".into(),
                    serde_json::json!("cancelled"),
                )])),
            ))
            .unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        let inbox = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        flock.attach_stores(Stores {
            tasks: Some(Arc::clone(&tasks)),
            inbox: Some(inbox),
            ..Stores::default()
        });
        let jobs = crate::tasks::next_job_runtime::NextJobs::new(Arc::clone(&flock));
        let other = Flock::new(
            dir.path().join("other"),
            dir.path().join("other-lanes"),
            false,
        );
        let request = RequestCtx::default();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let absent = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        assert!(absent
            .helper_turndispatchenv_advance_next_job("w1:p1")
            .is_err());
        assert!(crate::api::CommandEnv::turn_hooks(&absent).is_err());
        assert!(
            RequestRuntimeAdapter::new(&other, &request, runtime.handle(), &terminal)
                .with_next_jobs(&jobs)
                .is_err()
        );
        let outbox = crate::server_runtime::outbox_runtime::OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
        )
        .unwrap();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal)
            .with_next_jobs(&jobs)
            .unwrap()
            .with_outbox(&outbox)
            .unwrap();
        let hooks = crate::api::CommandEnv::turn_hooks(&adapter).unwrap();
        let started = flock
            .turn_start(Some("w1:p1"), Some(30.0), Some("test"), None, None, hooks)
            .unwrap();
        assert_eq!(started["pane_id"], "w1:p1");
        let env: &dyn TurnDispatchEnv = &adapter;
        // A cancelled handoff still blocks while its task is open, even after reassignment.
        runtime
            .block_on(tasks.assign(&task.id, &["w9:p9"], "set"))
            .unwrap();
        assert!(env.open_handoffs("w1:p1", true).unwrap());
        assert!(!env.open_handoffs("w1:p1", false).unwrap());
        runtime.block_on(tasks.cancel(&task.id, "test")).unwrap();
        assert!(!env.open_handoffs("w1:p1", true).unwrap());
        runtime.block_on(tasks.delete(&task.id)).unwrap();
        assert!(
            env.open_handoffs("w1:p1", true).is_err(),
            "missing referenced task must not permit a nudge"
        );
        rusqlite::Connection::open(dir.path().join("tasks.db"))
            .unwrap()
            .execute_batch("DROP TABLE task_events")
            .unwrap();
        assert!(
            env.open_handoffs("w1:p1", false).is_err(),
            "even a claim must surface the handoff read failure"
        );
    }

    #[test]
    fn turn_observation_propagates_missing_task_store() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        let jobs = crate::tasks::next_job_runtime::NextJobs::new(Arc::clone(&flock));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let request = RequestCtx::default();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal)
            .with_next_jobs(&jobs)
            .unwrap();
        let hooks: &dyn TurnHooks = &adapter;
        let state = TurnState {
            started_at: 1.0,
            expires_at: 10.0,
            ended_at: None,
            ttl_s: 9.0,
            turn_id: "turn".into(),
            source: None,
            caller: None,
            handoff_id: Some("i1".into()),
        };
        assert!(hooks.observe_turn("w1:p1", &state).is_err());
        assert!(adapter
            .helper_turndispatchenv_open_handoffs("w1:p1", true)
            .is_err());
    }
    #[test]
    fn turn_next_real_advance_preserves_disabled_decision() {
        const CHILD: &str = "DS_T1904_DISABLED_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--exact").arg("server_runtime::turn_next::tests::turn_next_real_advance_preserves_disabled_decision")
                .arg("--nocapture").env(CHILD, "1").env("DUNKINGSHEEP_TURN_END_DISPATCH", "0").status().unwrap();
            assert!(status.success());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        flock.attach_stores(Stores {
            tasks: Some(Arc::clone(&tasks)),
            ..Stores::default()
        });
        let jobs = crate::tasks::next_job_runtime::NextJobs::new(Arc::clone(&flock));
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal)
            .with_next_jobs(&jobs)
            .unwrap();
        let plan = adapter
            .helper_turndispatchenv_advance_next_job("w1:p1")
            .unwrap();
        assert_eq!(plan["action"], "skip");
        assert_eq!(plan["reason"], "disabled");
        let decisions = runtime.block_on(tasks.next_job_decisions()).unwrap();
        assert!(decisions.iter().any(|(pane, _)| pane == "w1:p1"));
    }
    #[test]
    fn turn_next_real_enabled_claim_uses_shared_providers() {
        use crate::config::Env;
        use crate::flock::status_runtime::StatusState;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let task = runtime
            .block_on(tasks.add("global", "real claim", "body"))
            .unwrap();
        runtime
            .block_on(tasks.assign(&task.id, &["w1:p1"], "set"))
            .unwrap();
        let inbox = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        flock.attach_stores(Stores {
            tasks: Some(Arc::clone(&tasks)),
            inbox: Some(Arc::clone(&inbox)),
            ..Stores::default()
        });
        let root = dir.path().to_string_lossy().into_owned();
        let env = Env::from_pairs(&[
            ("DUNKINGSHEEP_DIR", &root),
            ("DUNKINGSHEEP_TURN_END_DISPATCH", "1"),
            ("DUNKINGSHEEP_DISPATCH_LANES", "w1:p1"),
            ("DUNKINGSHEEP_MAILBOX", "w1:p1"),
        ]);
        flock
            .attach_status_state(StatusState::at_start(
                "test",
                dir.path().join("state"),
                dir.path().join("daemon"),
                env.clone(),
            ))
            .unwrap();
        let calls = Arc::new(AtomicUsize::new(0));
        let provider_calls = Arc::clone(&calls);
        let provider = Arc::new(move || {
            provider_calls.fetch_add(1, Ordering::SeqCst);
            Ok(env.clone())
        });
        let worker_threads = Arc::new(Mutex::new(Vec::new()));
        let factory_threads = Arc::clone(&worker_threads);
        let factory: crate::flock::loop_env::LoopTerminalFactory = Arc::new(move || {
            factory_threads
                .lock()
                .unwrap()
                .push(std::thread::current().id());
            let mut terminal = FakeTerminal::new();
            terminal.panes[0].command = "codex".into();
            Box::new(terminal)
        });
        let jobs = crate::tasks::next_job_runtime::NextJobs::with_providers(
            Arc::clone(&flock),
            provider,
            Some(factory),
        );
        let pid = i64::from(std::process::id());
        let request = RequestCtx {
            caller: Some(crate::proc::Caller::new(
                Some(pid),
                crate::proc::process_start(pid),
                Some("test".into()),
                Some("w1:p1".into()),
                "peer",
            )),
            ..RequestCtx::default()
        };
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal)
            .with_next_jobs(&jobs)
            .unwrap();
        let plan = adapter
            .helper_turndispatchenv_advance_next_job("w1:p1")
            .unwrap();
        assert_eq!(plan["action"], "claim", "{plan}");
        assert_eq!(plan["task_id"], task.id);
        let leases = runtime
            .block_on(tasks.leases_for_pane("w1:p1", None))
            .unwrap();
        assert!(leases
            .iter()
            .any(|lease| lease.task_id == task.id && lease.caller_pid == Some(pid)));
        let handoffs = runtime
            .block_on(tasks.next_job_handoffs(Some("w1:p1")))
            .unwrap();
        assert_eq!(handoffs.len(), 1);
        let message = handoffs[0]
            .get("message_id")
            .and_then(Value::as_str)
            .unwrap();
        let status =
            crate::inbox::status(inbox.read_pool(), message, crate::clock::now_s()).unwrap();
        assert_eq!(status.pane_id.as_deref(), Some("w1:p1"));
        assert!(
            calls.load(Ordering::SeqCst) >= 2,
            "gate and materialisation must both use the provider"
        );
        let threads = worker_threads.lock().unwrap();
        assert!(!threads.is_empty());
        assert!(threads
            .iter()
            .all(|thread| *thread != std::thread::current().id()));
    }
}
