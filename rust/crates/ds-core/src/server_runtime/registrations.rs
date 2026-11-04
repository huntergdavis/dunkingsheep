//! Assembly of the real handlers available to the staged runtime.
//!
//! Remaining handlers and production daemon startup are tracked by t1605.
//! Tests reject duplicate attempts in this fixed assembly; the underlying dispatcher
//! retains its existing replacement behavior at runtime.
use super::services::RuntimeServices;
use crate::api::Dispatcher;
use crate::server::Dispatcher as ServerDispatcher;
use std::sync::Arc;

pub(crate) fn production_handlers() -> Dispatcher {
    let mut handlers = Dispatcher::new();
    handlers.register_status_whoami_turn_start();
    handlers.register_help();
    handlers.register_lanes();
    handlers.register_context_supersede_retire();
    handlers.register_memory_recall_search_context();
    handlers.register_memory_stats_status_ingest();
    handlers.register_list_panes_read_pane_workspaces();
    handlers.register_retire_pane();
    handlers.register_superuser_status_add_remove();
    handlers.register_superuser_recover_tokens();
    handlers.register_gh_token();
    handlers.register_review_sync_list();
    handlers.register_poolrad_sync_progress();
    handlers.register_report_run_list();
    handlers.register_owner_stats_receipts_conversation();
    handlers.register_owner_push_poll_take();
    handlers.register_owner_delivered_message_state_reply_stats();
    handlers.register_owner_reply_ingest_outbox();
    handlers.register_pm_digest_routing();
    handlers.register_task_follow_unfollow_followers();
    handlers.register_task_add_list_read();
    handlers.register_task_delete();
    handlers.register_task_complete();
    handlers.register_task_mutation_leases();
    handlers.register_task_statistics();
    handlers.register_metrics();
    handlers.register_dunk_mutations();
    handlers.register_dunk_lifecycle();
    handlers.register_stop_all_fire_list_dunks();
    handlers.register_send_text_keys_flush_box();
    handlers.register_clear_box_delivery_capability();
    handlers.register_respawn_pane();
    handlers
}

impl RuntimeServices {
    /// Bind the currently implemented handlers to this one owned service set.
    pub fn production_dispatcher(self: &Arc<Self>) -> Arc<dyn ServerDispatcher> {
        self.dispatcher(production_handlers())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use crate::api::CommandEnv;
    use crate::config::Env;
    use crate::error::DunkError;
    use crate::flock::loop_env::LoopTerminalFactory;
    use crate::flock::panes::Stores;
    use crate::flock::status::StatusEnv;
    use crate::metrics::MetricsStore;
    use crate::roles::Roles;
    use crate::server::RequestCtx;
    use crate::server_runtime::{
        RequestRuntimeAdapter, RuntimeObservability, RuntimeServiceInputs,
    };
    use crate::tasks::Tasks;
    use crate::terminal::{
        fakes::FakeTerminal, AgentSpec, Pane, Resize, Tab, Terminal, TerminalError, Workspace,
    };
    use serde_json::{json, Map, Value};
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use std::thread::ThreadId;
    use std::time::{Duration, Instant};
    use tokio::runtime::Builder;

    #[derive(Debug, Default)]
    struct SendObservations {
        factory_threads: Vec<ThreadId>,
        text: Vec<(String, String)>,
        keys: Vec<KeyObservation>,
    }

    #[derive(Debug)]
    struct KeyObservation {
        pane: String,
        keys: Vec<String>,
    }

    #[derive(Debug)]
    struct DispatcherTerminal {
        inner: FakeTerminal,
        observations: Arc<Mutex<SendObservations>>,
        read_script: Mutex<VecDeque<Option<String>>>,
        shared_screen: Option<Arc<Mutex<String>>>,
    }

    fn tail_lines(screen: &str, lines: i64) -> String {
        screen
            .lines()
            .rev()
            .take(lines.max(0) as usize)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("\n")
    }

    impl Terminal for DispatcherTerminal {
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
            self.observations
                .lock()
                .unwrap()
                .text
                .push((pane_id.to_owned(), text.to_owned()));
            self.inner.send_text(pane_id, text)
        }
        fn send_text_and_enter(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
            self.observations
                .lock()
                .unwrap()
                .text
                .push((pane_id.to_owned(), text.to_owned()));
            self.inner.send_text_and_enter(pane_id, text)
        }
        fn send_keys(&mut self, pane_id: &str, keys: &[&str]) -> Result<(), TerminalError> {
            self.observations.lock().unwrap().keys.push(KeyObservation {
                pane: pane_id.to_owned(),
                keys: keys.iter().map(|key| (*key).to_owned()).collect(),
            });
            self.inner.send_keys(pane_id, keys)
        }
        fn read_pane(
            &self,
            pane_id: &str,
            lines: i64,
            ansi: bool,
        ) -> Result<Option<String>, TerminalError> {
            if let Some(screen) = &self.shared_screen {
                return Ok(Some(tail_lines(&screen.lock().unwrap(), lines)));
            }
            if let Some(screen) = self.read_script.lock().unwrap().pop_front() {
                return Ok(screen.map(|screen| tail_lines(&screen, lines)));
            }
            self.inner.read_pane(pane_id, lines, ansi)
        }
        fn agent_status(&self, pane_id: &str) -> Option<String> {
            self.inner.agent_status(pane_id)
        }
        fn resize(&mut self, pane_id: &str, cols: i64, rows: i64) -> Result<Resize, TerminalError> {
            self.inner.resize(pane_id, cols, rows)
        }
    }

    fn send_factory(
        observations: Arc<Mutex<SendObservations>>,
        flush_script_next_request: Arc<AtomicBool>,
        worker_gate: Arc<(Mutex<bool>, Condvar)>,
    ) -> LoopTerminalFactory {
        let request_thread = std::thread::current().id();
        Arc::new(move || {
            let current_thread = std::thread::current().id();
            observations
                .lock()
                .unwrap()
                .factory_threads
                .push(current_thread);
            if current_thread != request_thread {
                let (released, wake) = &*worker_gate;
                let mut released = released.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
            }
            let mut inner = FakeTerminal::new();
            for pane in &mut inner.panes {
                pane.pane_pid = Some(std::process::id() as i64);
            }
            let read_script = if current_thread == request_thread
                && flush_script_next_request.swap(false, Ordering::SeqCst)
            {
                VecDeque::from([
                    Some("│ ❯ stranded text\n──────────\n".into()),
                    Some("❯\u{a0}\nnew task?\n".into()),
                    Some("❯\u{a0}\nnew task?\n".into()),
                    Some("❯\u{a0}\nnew task?\n".into()),
                    Some("❯\u{a0}\nnew task?\n".into()),
                    Some("❯\u{a0}\nnew task?\n".into()),
                ])
            } else {
                VecDeque::new()
            };
            Box::new(DispatcherTerminal {
                inner,
                observations: Arc::clone(&observations),
                read_script: Mutex::new(read_script),
                shared_screen: None,
            })
        })
    }

    fn manual_fire_factory(
        observations: Arc<Mutex<SendObservations>>,
        screen: Arc<Mutex<String>>,
        outbox_started: std::sync::mpsc::Sender<()>,
        release_outbox: Arc<(Mutex<bool>, Condvar)>,
    ) -> LoopTerminalFactory {
        let request_thread = std::thread::current().id();
        let factory_calls = Arc::new(AtomicUsize::new(0));
        Arc::new(move || {
            let current_thread = std::thread::current().id();
            observations
                .lock()
                .unwrap()
                .factory_threads
                .push(current_thread);
            let invocation = factory_calls.fetch_add(1, Ordering::SeqCst);
            if invocation >= 2 && current_thread != request_thread {
                let _ = outbox_started.send(());
                let (released, wake) = &*release_outbox;
                let mut released = released.lock().unwrap();
                while !*released {
                    released = wake.wait(released).unwrap();
                }
            }
            let mut inner = FakeTerminal::new();
            for pane in &mut inner.panes {
                pane.pane_pid = Some(std::process::id() as i64);
            }
            Box::new(DispatcherTerminal {
                inner,
                observations: Arc::clone(&observations),
                read_script: Mutex::new(VecDeque::new()),
                shared_screen: Some(Arc::clone(&screen)),
            })
        })
    }

    fn make_services(
        tag: &str,
        terminal_factory: LoopTerminalFactory,
        metrics: Option<Arc<MetricsStore>>,
    ) -> (tempfile::TempDir, Arc<RuntimeServices>) {
        make_services_with_persistence(tag, terminal_factory, metrics, false)
    }

    fn make_services_with_persistence(
        tag: &str,
        terminal_factory: LoopTerminalFactory,
        metrics: Option<Arc<MetricsStore>>,
        persist_enabled: bool,
    ) -> (tempfile::TempDir, Arc<RuntimeServices>) {
        let dir = tempfile::tempdir().unwrap();
        let runtime = Builder::new_multi_thread().enable_all().build().unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join(format!("{tag}-tasks.db"))))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let inbox = Arc::new(
            crate::inbox_store::Store::open(
                dir.path().join(format!("{tag}-inbox.db")),
                dir.path().join(format!("{tag}-blobs")),
            )
            .unwrap(),
        );
        let env = Env::default();
        let mut roles = Roles::new(
            &env,
            Some(dir.path().join(format!("{tag}-roles.json"))),
            Some(dir.path().join(format!("{tag}-tokens.json"))),
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
        roles
            .register("w2:p2", "owner", None, None, None, None, None, None)
            .unwrap();
        roles
            .register("w3:p3", "desk", None, None, None, None, None, None)
            .unwrap();
        let executable = dir.path().join(format!("{tag}-daemon"));
        std::fs::write(&executable, b"test daemon identity").unwrap();
        let stores = Stores {
            tasks: Some(tasks),
            inbox: Some(inbox),
            metrics,
            ..Stores::default()
        };
        let observability = stores.metrics.is_none().then(|| RuntimeObservability {
            state_dir: dir.path().join(format!("{tag}-state")),
            memory_db: None,
        });
        let services = RuntimeServices::new(RuntimeServiceInputs {
            launches_file: dir.path().join(format!("{tag}-launches.json")),
            lane_registry_file: dir.path().join(format!("{tag}-lanes.json")),
            persist_enabled,
            stores,
            roles,
            legacy_superusers: None,
            release_version: "test-release".into(),
            state_file: dir.path().join(format!("{tag}-whole-state.json")),
            executable,
            env,
            observability,
            runtime,
            terminal_factory,
        })
        .unwrap();
        (dir, services)
    }

    fn request(pane: &str) -> RequestCtx {
        RequestCtx {
            caller: Some(crate::proc::Caller::new(
                Some(123),
                Some("start".into()),
                Some("codex".into()),
                Some(pane.into()),
                "agent",
            )),
            self_pane_id: Some(pane.into()),
            socket_peer: Some(true),
            ..RequestCtx::default()
        }
    }

    struct CountingStatusEnv<'a> {
        inner: &'a dyn StatusEnv,
        version_calls: &'a AtomicUsize,
    }

    impl StatusEnv for CountingStatusEnv<'_> {
        fn version(&self) -> String {
            self.version_calls.fetch_add(1, Ordering::SeqCst);
            self.inner.version()
        }
        fn federated_input(&self) -> Value {
            self.inner.federated_input()
        }
        fn code_status(&self) -> Map<String, Value> {
            self.inner.code_status()
        }
        fn dunks(&self) -> &crate::flock::dunks::DunkList {
            self.inner.dunks()
        }
        fn started_at(&self) -> f64 {
            self.inner.started_at()
        }
        fn pid(&self) -> u32 {
            self.inner.pid()
        }
        fn now(&self) -> f64 {
            self.inner.now()
        }
        fn queued_messages(&self) -> usize {
            self.inner.queued_messages()
        }
        fn inbox_unread(&self) -> Result<i64, DunkError> {
            self.inner.inbox_unread()
        }
        fn task_totals(&self) -> Result<Value, DunkError> {
            self.inner.task_totals()
        }
        fn task_followers(&self) -> usize {
            self.inner.task_followers()
        }
        fn quarantined_mailboxes(&self) -> Option<Value> {
            self.inner.quarantined_mailboxes()
        }
        fn last_message(&self) -> String {
            self.inner.last_message()
        }
        fn herdr_available(&self) -> bool {
            self.inner.herdr_available()
        }
        fn herdr_hint(&self) -> String {
            self.inner.herdr_hint()
        }
        fn state_file(&self) -> Option<String> {
            self.inner.state_file()
        }
        fn inbox_count(&self, pane: &str) -> Result<Value, String> {
            self.inner.inbox_count(pane)
        }
    }

    fn cached_dunks(
        services: &RuntimeServices,
        request: &RequestCtx,
        version_calls: &AtomicUsize,
    ) -> Value {
        let terminal = Mutex::new((services.terminal_factory)());
        let adapter = RequestRuntimeAdapter::new(
            services.flock.as_ref(),
            request,
            services.runtime.handle(),
            &terminal,
        );
        let status = adapter.status_env().unwrap();
        let status = CountingStatusEnv {
            inner: status,
            version_calls,
        };
        services
            .flock
            .view_cache()
            .view(&status, request.self_pane_id.as_deref(), false, 3_600.0)
    }

    #[test]
    fn production_registration_inventory_is_exact_and_has_no_duplicates() {
        let handlers = production_handlers();
        let attempts = handlers.registration_attempts();
        let actual: BTreeSet<_> = attempts.iter().copied().collect();
        let expected: BTreeSet<_> = [
            "owner_stats",
            "owner_receipts",
            "message_state",
            "reply_stats",
            "lanes",
            "lane_set",
            "retire_pane",
            "help",
            "status",
            "turn_start",
            "send_text",
            "send_keys",
            "flush_box",
            "clear_box",
            "delivery",
            "delivery_capability",
            "respawn_pane",
            "report_run",
            "report_list",
            "owner_conversation",
            "owner_reply",
            "owner_ingest",
            "owner_outbox",
            "owner_delivered",
            "pm_digest",
            "pm_routing",
            "task_add",
            "task_list",
            "task_read",
            "task_delete",
            "task_complete",
            "task_update",
            "task_assign",
            "task_note",
            "task_block",
            "task_unblock",
            "task_cancel",
            "task_next",
            "task_heartbeat",
            "task_release",
            "task_stats",
            "task_metrics",
            "task_reap",
            "metrics",
            "add_dunk",
            "update_dunk",
            "remove_dunk",
            "start_dunk",
            "stop_dunk",
            "toggle_dunk",
            "stop_all",
            "fire_dunk",
            "list_dunks",
            "owner_push",
            "owner_poll",
            "owner_take",
            "task_follow",
            "task_unfollow",
            "task_followers",
            "memory_recall",
            "memory_context",
            "memory_search",
            "memory_stats",
            "memory_status",
            "memory_ingest",
            "list_panes",
            "read_pane",
            "list_workspaces",
            "gh_token",
            "review_sync",
            "review_list",
            "context_supersede",
            "context_retire",
            "poolrad_sync",
            "poolrad_progress",
            "whoami",
            "lane_register",
            "lane_unregister",
            "token_issue",
            "token_revoke",
            "superuser_status",
            "superuser_add",
            "superuser_remove",
            "superuser_recover",
        ]
        .into_iter()
        .collect();
        assert_eq!(actual, expected);
        assert_eq!(
            attempts.len(),
            actual.len(),
            "duplicate registration attempt"
        );
    }

    #[test]
    fn production_dispatcher_routes_respawn_to_the_guarded_handler() {
        let (_dir, services) = make_services(
            "respawn-route",
            Arc::new(|| Box::new(FakeTerminal::new())),
            None,
        );
        let dispatcher = services.production_dispatcher();
        let error = dispatcher
            .dispatch(
                "respawn_pane",
                &json!({
                    "pane": "w1:p1",
                    "expected_pid": 1101,
                    "expected_start": "unused",
                    "argv": ["custom-agent"],
                    "rollback_argv": ["old-custom-agent"],
                    "confirm_idle": 0
                }),
                &request("w2:p2"),
            )
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("respawn-pane requires --confirm-idle"),
            "production route did not reach the accepted guard: {error}"
        );
        dispatcher.close();
        services.shutdown();
    }

    #[test]
    fn production_dispatcher_routes_dunk_mutations_through_runtime_services() {
        let (dir, services) = make_services_with_persistence(
            "dunk-mutations-route",
            Arc::new(|| Box::new(FakeTerminal::new())),
            None,
            true,
        );
        let dispatcher = services.production_dispatcher();
        let owner = request("w2:p2");
        let status_reads = AtomicUsize::new(0);
        assert!(cached_dunks(&services, &owner, &status_reads)["dunks"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(status_reads.load(Ordering::SeqCst), 1);
        let added = dispatcher
            .dispatch(
                "add_dunk",
                &json!({
                    "target": "w3:p3",
                    "text": "integration",
                    "interval_minutes": 1,
                    "start": true
                }),
                &owner,
            )
            .unwrap();
        assert_eq!(added["id"], "d1");
        assert_eq!(services.flock.dunks.list().len(), 1);
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);
        assert_eq!(
            services.outbox.dunk_timer_generation("d1").unwrap(),
            Some(0)
        );
        let after_add = cached_dunks(&services, &owner, &status_reads);
        assert_eq!(after_add["dunks"][0]["target_pane_id"], "w3:p3");
        assert_eq!(status_reads.load(Ordering::SeqCst), 2);

        let updated = dispatcher
            .dispatch(
                "update_dunk",
                &json!({"id": "d1", "text": "updated", "interval_minutes": 2}),
                &owner,
            )
            .unwrap();
        assert_eq!(updated["text"], "updated");
        assert_eq!(updated["interval_minutes"], 2.0);
        assert!(updated["next_send_at"].as_f64().is_some());
        assert_eq!(
            services.outbox.dunk_timer_generation("d1").unwrap(),
            Some(1)
        );
        assert_eq!(
            cached_dunks(&services, &owner, &status_reads)["dunks"][0]["text"],
            "updated"
        );
        assert_eq!(status_reads.load(Ordering::SeqCst), 3);

        // Equal intervals and falsey targets produce an empty Python-compatible changed list.
        // Block persistence to prove a successful no-op neither saves nor wakes the running
        // timer, and leave the already-primed shared view valid.
        let state_path = dir.path().join("dunk-mutations-route-whole-state.json");
        std::fs::remove_file(&state_path).unwrap();
        std::fs::create_dir(&state_path).unwrap();
        let same_interval = dispatcher
            .dispatch(
                "update_dunk",
                &json!({"id": "d1", "interval_minutes": 2}),
                &owner,
            )
            .unwrap();
        assert_eq!(same_interval["interval_minutes"], 2.0);
        let falsey_target = dispatcher
            .dispatch("update_dunk", &json!({"id": "d1", "target": ""}), &owner)
            .unwrap();
        assert_eq!(falsey_target["target_pane_id"], "w3:p3");
        assert_eq!(
            services.outbox.dunk_timer_generation("d1").unwrap(),
            Some(1),
            "successful no-op updates do not wake the timer"
        );
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);
        assert_eq!(
            cached_dunks(&services, &owner, &status_reads)["dunks"][0]["interval_minutes"],
            2.0,
            "successful no-op updates preserve the primed view"
        );
        assert_eq!(
            status_reads.load(Ordering::SeqCst),
            3,
            "successful no-op updates do not invalidate the shared cache"
        );
        std::fs::remove_dir(&state_path).unwrap();

        let removed = dispatcher
            .dispatch("remove_dunk", &json!({"id": "d1"}), &owner)
            .unwrap();
        assert_eq!(removed["running"], false);
        assert!(services.flock.dunks.list().is_empty());
        assert_eq!(services.outbox.dunk_timer_worker_count(), 0);
        assert!(cached_dunks(&services, &owner, &status_reads)["dunks"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(status_reads.load(Ordering::SeqCst), 4);

        let before_refusals = status_reads.load(Ordering::SeqCst);
        let invalid_interval = dispatcher
            .dispatch(
                "add_dunk",
                &json!({
                    "target": "w3:p3",
                    "text": "invalid interval",
                    "interval_minutes": 0
                }),
                &owner,
            )
            .unwrap_err();
        assert!(invalid_interval
            .to_string()
            .contains("interval must be positive"));
        let invalid_target = dispatcher
            .dispatch(
                "add_dunk",
                &json!({
                    "target": "missing target",
                    "text": "invalid target",
                    "interval_minutes": 1
                }),
                &owner,
            )
            .unwrap_err();
        assert!(invalid_target.to_string().contains("no pane matches"));
        let invalid_update = dispatcher
            .dispatch(
                "update_dunk",
                &json!({"id": "d404", "text": "missing dunk"}),
                &owner,
            )
            .unwrap_err();
        assert!(invalid_update.to_string().contains("no dunk with id"));
        let invalid_remove = dispatcher
            .dispatch("remove_dunk", &json!({"id": "d404"}), &owner)
            .unwrap_err();
        assert!(invalid_remove.to_string().contains("no dunk with id"));
        assert!(services.flock.dunks.list().is_empty());
        assert!(cached_dunks(&services, &owner, &status_reads)["dunks"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(status_reads.load(Ordering::SeqCst), before_refusals);

        let other_flock = crate::flock::Flock::new(
            dir.path().join("other-launches.json"),
            dir.path().join("other-lanes.json"),
            false,
        );
        let other_terminal = Mutex::new(Box::new(FakeTerminal::new()) as Box<dyn Terminal>);
        let mismatch = RequestRuntimeAdapter::new(
            &other_flock,
            &owner,
            services.runtime.handle(),
            &other_terminal,
        )
        .with_dunk_runtime(&services)
        .err()
        .expect("foreign Flock must refuse the dunk runtime");
        assert!(mismatch.to_string().contains("dunk runtime flock mismatch"));
        assert_eq!(services.flock.dunks.list().len(), 0);
        assert!(cached_dunks(&services, &owner, &status_reads)["dunks"]
            .as_array()
            .unwrap()
            .is_empty());
        assert_eq!(status_reads.load(Ordering::SeqCst), before_refusals);

        // Flock.save invalidates before the fallible write. The mutation remains in memory,
        // matching Python's append-then-_save order rather than inventing rollback behavior.
        let state_path = dir.path().join("dunk-mutations-route-whole-state.json");
        std::fs::remove_file(&state_path).unwrap();
        std::fs::create_dir(&state_path).unwrap();
        let save_error = dispatcher
            .dispatch(
                "add_dunk",
                &json!({
                    "target": "w3:p3",
                    "text": "kept after save failure",
                    "interval_minutes": 1,
                    "start": false
                }),
                &owner,
            )
            .unwrap_err();
        assert!(save_error.to_string().contains("rename"));
        assert_eq!(services.flock.dunks.list().len(), 1);
        let after_save_error = cached_dunks(&services, &owner, &status_reads);
        assert_eq!(after_save_error["dunks"][0]["id"], "d2");
        assert_eq!(
            status_reads.load(Ordering::SeqCst),
            before_refusals + 1,
            "save invalidates before its fallible persistence write"
        );

        let worker = request("w1:p1");
        let denied = dispatcher
            .dispatch(
                "add_dunk",
                &json!({
                    "target": "w3:p3",
                    "text": "denied",
                    "interval_minutes": 1
                }),
                &worker,
            )
            .unwrap_err();
        assert!(denied.to_string().contains("requires a superuser"));

        dispatcher.close();
        services.shutdown();
    }

    #[test]
    fn h_start_dunk_matches_python() {
        let (dir, services) = make_services_with_persistence(
            "start-dunk-parity",
            Arc::new(|| Box::new(FakeTerminal::new())),
            None,
            true,
        );
        let dispatcher = services.production_dispatcher();
        let owner = request("w2:p2");
        let worker = request("w1:p1");
        dispatcher
            .dispatch(
                "add_dunk",
                &json!({"target": "w3:p3", "text": "start", "interval_minutes": 1, "start": false}),
                &owner,
            )
            .unwrap();
        dispatcher
            .dispatch(
                "add_dunk",
                &json!({"text": "no target", "start": false}),
                &owner,
            )
            .unwrap();

        let status_reads = AtomicUsize::new(0);
        let cached = cached_dunks(&services, &owner, &status_reads);
        assert_eq!(cached["dunks"].as_array().unwrap().len(), 2);
        assert_eq!(status_reads.load(Ordering::SeqCst), 1);

        let denied = dispatcher
            .dispatch("start_dunk", &json!({}), &worker)
            .unwrap_err();
        assert_eq!(
            denied.to_string(),
            "start_dunk requires a superuser (the owner role); w1:p1 is worker. Non-owners use the inbox and task queue."
        );
        let missing = dispatcher
            .dispatch("start_dunk", &json!({}), &owner)
            .unwrap_err();
        assert_eq!(
            missing.to_string(),
            "start_dunk: missing required argument(s): id"
        );

        let no_target = dispatcher
            .dispatch("start_dunk", &json!({"id": "d2"}), &owner)
            .unwrap_err();
        assert_eq!(no_target.to_string(), "d2 has no target");
        assert_eq!(
            services.flock.dunks.get("d2").unwrap()["running"],
            false,
            "Python changes status but does not mark an untargeted dunk running"
        );
        assert_eq!(
            services.flock.dunks.get("d2").unwrap()["status"],
            "No target - press c"
        );
        assert_eq!(services.outbox.dunk_timer_worker_count(), 0);
        assert_eq!(
            status_reads.load(Ordering::SeqCst),
            1,
            "the pre-save no-target refusal does not invalidate the cached view"
        );

        // Python Flock._find stringifies its raw id, then accepts a bare numeric id.
        let started = dispatcher
            .dispatch("start_dunk", &json!({"id": 1}), &owner)
            .unwrap();
        assert_eq!(started["id"], "d1");
        assert_eq!(started["running"], true);
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);
        assert_eq!(
            services.outbox.dunk_timer_generation("d1").unwrap(),
            Some(0)
        );
        assert_eq!(
            cached_dunks(&services, &owner, &status_reads)["dunks"][0]["running"],
            true
        );
        assert_eq!(status_reads.load(Ordering::SeqCst), 2);

        // An already-running start saves like Python, without creating a second worker.
        let started_again = dispatcher
            .dispatch("start_dunk", &json!({"id": "d1"}), &owner)
            .unwrap();
        assert_eq!(started_again["running"], true);
        assert_eq!(started_again["started_at"], started["started_at"]);
        assert_eq!(started_again["last_error"], started["last_error"]);
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);
        assert_eq!(
            started_again["next_send_at"], started["next_send_at"],
            "an already-running Python start preserves its existing deadline"
        );
        assert_eq!(
            services.outbox.dunk_timer_generation("d1").unwrap(),
            Some(0)
        );
        assert_eq!(
            cached_dunks(&services, &owner, &status_reads)["dunks"][0]["running"],
            true
        );
        assert_eq!(status_reads.load(Ordering::SeqCst), 3);

        // Accepted idempotent starts still save/invalidate, even if persistence then fails.
        let state_path = dir.path().join("start-dunk-parity-whole-state.json");
        std::fs::remove_file(&state_path).unwrap();
        std::fs::create_dir(&state_path).unwrap();
        let save_error = dispatcher
            .dispatch("start_dunk", &json!({"id": "d1"}), &owner)
            .unwrap_err();
        assert!(save_error.to_string().contains("rename"));
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);
        assert_eq!(
            services.outbox.dunk_timer_generation("d1").unwrap(),
            Some(0)
        );
        assert_eq!(services.flock.dunks.get("d1").unwrap()["running"], true);
        assert_eq!(
            cached_dunks(&services, &owner, &status_reads)["dunks"][0]["next_send_at"],
            started["next_send_at"]
        );
        assert_eq!(status_reads.load(Ordering::SeqCst), 4);
        std::fs::remove_dir(&state_path).unwrap();
        services.stop_dunk("d1").unwrap();
        services.shutdown();
    }

    #[test]
    fn h_start_dunk_resumes_running_row_without_live_worker() {
        let (_dir, services) = make_services(
            "start-dunk-missing-worker",
            Arc::new(|| Box::new(FakeTerminal::new())),
            None,
        );
        let dispatcher = services.production_dispatcher();
        let owner = request("w2:p2");
        dispatcher
            .dispatch(
                "add_dunk",
                &json!({"target": "w3:p3", "text": "resume", "interval_minutes": 1, "start": false}),
                &owner,
            )
            .unwrap();
        let first = dispatcher
            .dispatch("start_dunk", &json!({"id": "d1"}), &owner)
            .unwrap();
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);

        services.outbox.stop_dunk_timer_worker_only("d1").unwrap();
        assert_eq!(services.flock.dunks.get("d1").unwrap()["running"], true);
        assert_eq!(services.outbox.dunk_timer_worker_count(), 0);

        let resumed = dispatcher
            .dispatch("start_dunk", &json!({"id": "d1"}), &owner)
            .unwrap();
        assert_eq!(resumed["running"], true);
        assert_ne!(resumed["started_at"], first["started_at"]);
        assert_ne!(resumed["next_send_at"], first["next_send_at"]);
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);
        assert_eq!(
            services.outbox.dunk_timer_generation("d1").unwrap(),
            Some(0)
        );

        services.stop_dunk("d1").unwrap();
        services.shutdown();
    }

    #[test]
    fn h_start_dunk_reaps_finished_worker_and_resumes_running_row() {
        let (_dir, services) = make_services(
            "start-dunk-finished-worker",
            Arc::new(|| Box::new(FakeTerminal::new())),
            None,
        );
        let dispatcher = services.production_dispatcher();
        let owner = request("w2:p2");
        dispatcher
            .dispatch(
                "add_dunk",
                &json!({"target": "w3:p3", "text": "resume", "interval_minutes": 1, "start": false}),
                &owner,
            )
            .unwrap();
        let first = dispatcher
            .dispatch("start_dunk", &json!({"id": "d1"}), &owner)
            .unwrap();
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);

        services
            .outbox
            .install_finished_dunk_timer_worker("d1")
            .unwrap();
        assert_eq!(services.flock.dunks.get("d1").unwrap()["running"], true);
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);
        assert_eq!(
            services.outbox.dunk_timer_generation("d1").unwrap(),
            Some(0)
        );

        let resumed = dispatcher
            .dispatch("start_dunk", &json!({"id": "d1"}), &owner)
            .unwrap();
        assert_eq!(resumed["running"], true);
        assert_ne!(resumed["started_at"], first["started_at"]);
        assert_ne!(resumed["next_send_at"], first["next_send_at"]);
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);
        assert_eq!(
            services.outbox.dunk_timer_generation("d1").unwrap(),
            Some(0)
        );

        services.stop_dunk("d1").unwrap();
        services.shutdown();
    }

    #[test]
    fn h_start_and_stop_dunk_serialize_row_and_worker_lifetimes() {
        let (_dir, services) = make_services(
            "start-stop-dunk-race",
            Arc::new(|| Box::new(FakeTerminal::new())),
            None,
        );
        let dispatcher = services.production_dispatcher();
        let owner = request("w2:p2");
        dispatcher
            .dispatch(
                "add_dunk",
                &json!({"target": "w3:p3", "text": "race", "interval_minutes": 1, "start": true}),
                &owner,
            )
            .unwrap();
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);

        let barrier = Arc::new(std::sync::Barrier::new(3));
        let start_dispatcher = Arc::clone(&dispatcher);
        let start_barrier = Arc::clone(&barrier);
        let start = std::thread::spawn(move || {
            let owner = request("w2:p2");
            start_barrier.wait();
            start_dispatcher.dispatch("start_dunk", &json!({"id": "d1"}), &owner)
        });
        let stop_dispatcher = Arc::clone(&dispatcher);
        let stop_barrier = Arc::clone(&barrier);
        let stop = std::thread::spawn(move || {
            let owner = request("w2:p2");
            stop_barrier.wait();
            stop_dispatcher.dispatch("stop_dunk", &json!({"id": "d1"}), &owner)
        });
        barrier.wait();
        start.join().unwrap().unwrap();
        stop.join().unwrap().unwrap();

        let row = services.flock.dunks.get("d1").unwrap();
        if row["running"] == true {
            assert!(row["next_send_at"].is_number());
            assert_eq!(services.outbox.dunk_timer_worker_count(), 1);
            services.stop_dunk("d1").unwrap();
        } else {
            assert_eq!(row["next_send_at"], Value::Null);
            assert_eq!(row["status"], "Stopped");
            assert_eq!(services.outbox.dunk_timer_worker_count(), 0);
            std::thread::sleep(Duration::from_millis(50));
            let after_join = services.flock.dunks.get("d1").unwrap();
            assert_eq!(after_join["next_send_at"], Value::Null);
            assert_eq!(after_join["status"], "Stopped");
        }
        services.shutdown();
    }

    #[test]
    fn h_start_dunk_invalidates_view() {
        let (_dir, services) = make_services_with_persistence(
            "start-dunk-cache",
            Arc::new(|| Box::new(FakeTerminal::new())),
            None,
            true,
        );
        let dispatcher = services.production_dispatcher();
        let owner = request("w2:p2");
        dispatcher
            .dispatch(
                "add_dunk",
                &json!({"target": "w3:p3", "text": "cache", "interval_minutes": 1, "start": false}),
                &owner,
            )
            .unwrap();
        let status_reads = AtomicUsize::new(0);
        cached_dunks(&services, &owner, &status_reads);
        assert_eq!(status_reads.load(Ordering::SeqCst), 1);

        dispatcher
            .dispatch("start_dunk", &json!({"id": "d1"}), &owner)
            .unwrap();
        assert_eq!(
            cached_dunks(&services, &owner, &status_reads)["dunks"][0]["running"],
            true
        );
        assert_eq!(status_reads.load(Ordering::SeqCst), 2);
        services.stop_dunk("d1").unwrap();
        services.shutdown();
    }

    #[test]
    fn h_stop_dunk_matches_python() {
        let (dir, services) = make_services_with_persistence(
            "stop-dunk-parity",
            Arc::new(|| Box::new(FakeTerminal::new())),
            None,
            true,
        );
        let dispatcher = services.production_dispatcher();
        let owner = request("w2:p2");
        for text in ["normal stop", "save failure stop"] {
            dispatcher
                .dispatch(
                    "add_dunk",
                    &json!({"target": "w3:p3", "text": text, "interval_minutes": 1, "start": true}),
                    &owner,
                )
                .unwrap();
        }
        let status_reads = AtomicUsize::new(0);
        cached_dunks(&services, &owner, &status_reads);
        assert_eq!(services.outbox.dunk_timer_worker_count(), 2);

        let stopped = dispatcher
            .dispatch("stop_dunk", &json!({"id": "d1"}), &owner)
            .unwrap();
        assert_eq!(stopped["running"], false);
        assert_eq!(stopped["next_send_at"], Value::Null);
        assert_eq!(stopped["status"], "Stopped");
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);
        assert_eq!(
            cached_dunks(&services, &owner, &status_reads)["dunks"][0]["running"],
            false
        );
        assert_eq!(status_reads.load(Ordering::SeqCst), 2);
        let sent_after_join = services.flock.dunks.get("d1").unwrap()["send_count"].clone();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            services.flock.dunks.get("d1").unwrap()["send_count"],
            sent_after_join
        );

        // Python _save runs even when stop is repeated; successful repeat invalidates views.
        dispatcher
            .dispatch("stop_dunk", &json!({"id": "d1"}), &owner)
            .unwrap();
        assert_eq!(
            status_reads.load(Ordering::SeqCst),
            2,
            "repeat stop invalidates lazily when the view is next requested"
        );
        cached_dunks(&services, &owner, &status_reads);
        assert_eq!(status_reads.load(Ordering::SeqCst), 3);

        // A persistence failure happens after stop mutation, worker join, and cache invalidation.
        let state_path = dir.path().join("stop-dunk-parity-whole-state.json");
        std::fs::remove_file(&state_path).unwrap();
        std::fs::create_dir(&state_path).unwrap();
        let save_error = dispatcher
            .dispatch("stop_dunk", &json!({"id": "d2"}), &owner)
            .unwrap_err();
        assert!(save_error.to_string().contains("rename"));
        assert_eq!(services.flock.dunks.get("d2").unwrap()["running"], false);
        assert_eq!(
            services.flock.dunks.get("d2").unwrap()["next_send_at"],
            Value::Null
        );
        assert_eq!(services.outbox.dunk_timer_worker_count(), 0);
        assert_eq!(
            cached_dunks(&services, &owner, &status_reads)["dunks"][1]["status"],
            "Stopped"
        );
        assert_eq!(status_reads.load(Ordering::SeqCst), 4);
        std::fs::remove_dir(&state_path).unwrap();
        services.shutdown();
    }

    #[test]
    fn h_toggle_dunk_matches_python() {
        let (_dir, services) = make_services_with_persistence(
            "toggle-dunk-parity",
            Arc::new(|| Box::new(FakeTerminal::new())),
            None,
            true,
        );
        let dispatcher = services.production_dispatcher();
        let owner = request("w2:p2");
        dispatcher
            .dispatch(
                "add_dunk",
                &json!({"target": "w3:p3", "text": "toggle", "interval_minutes": 1, "start": false}),
                &owner,
            )
            .unwrap();
        let lifetime = services.flock.dunks.handle("d1").unwrap();
        let status_reads = AtomicUsize::new(0);
        cached_dunks(&services, &owner, &status_reads);

        let started = dispatcher
            .dispatch("toggle_dunk", &json!({"id": 1}), &owner)
            .unwrap();
        assert_eq!(started["running"], true);
        assert!(Arc::ptr_eq(
            &lifetime,
            &services.flock.dunks.handle("d1").unwrap()
        ));
        assert_eq!(services.outbox.dunk_timer_worker_count(), 1);
        assert_eq!(
            cached_dunks(&services, &owner, &status_reads)["dunks"][0]["running"],
            true
        );
        assert_eq!(status_reads.load(Ordering::SeqCst), 2);

        let stopped = dispatcher
            .dispatch("toggle_dunk", &json!({"id": "d1"}), &owner)
            .unwrap();
        assert_eq!(stopped["running"], false);
        assert_eq!(stopped["next_send_at"], Value::Null);
        assert_eq!(stopped["status"], "Stopped");
        assert_eq!(services.outbox.dunk_timer_worker_count(), 0);
        assert!(Arc::ptr_eq(
            &lifetime,
            &services.flock.dunks.handle("d1").unwrap()
        ));
        assert_eq!(
            cached_dunks(&services, &owner, &status_reads)["dunks"][0]["running"],
            false
        );
        assert_eq!(status_reads.load(Ordering::SeqCst), 3);
        services.shutdown();
    }

    #[test]
    fn assembled_dispatcher_executes_real_identity_task_and_permission_paths() {
        let observations = Arc::new(Mutex::new(SendObservations::default()));
        let flush_script = Arc::new(AtomicBool::new(false));
        let worker_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (_dir, services) = make_services(
            "registered",
            send_factory(
                Arc::clone(&observations),
                Arc::clone(&flush_script),
                Arc::clone(&worker_gate),
            ),
            None,
        );
        let dispatcher = services.production_dispatcher();
        let worker = request("w1:p1");
        let status = dispatcher.dispatch("status", &json!({}), &worker).unwrap();
        assert!(status.is_object());
        let who = dispatcher.dispatch("whoami", &json!({}), &worker).unwrap();
        assert_eq!(who["pane_id"], "w1:p1");
        assert_eq!(who["role"], "worker");
        let added = dispatcher
            .dispatch("task_add", &json!({"title":"registration task"}), &worker)
            .unwrap();
        let id = added["id"].as_str().unwrap();
        let read = dispatcher
            .dispatch("task_read", &json!({"id":id}), &worker)
            .unwrap();
        assert_eq!(read["title"], "registration task");
        let listed = dispatcher
            .dispatch("task_list", &json!({}), &worker)
            .unwrap();
        assert!(listed["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|task| task["id"] == id));
        let missing = dispatcher
            .dispatch("task_add", &json!({}), &worker)
            .unwrap_err()
            .to_string();
        assert!(missing.contains("missing required argument"));
        let missing_complete = dispatcher
            .dispatch("task_complete", &json!({}), &worker)
            .unwrap_err()
            .to_string();
        assert!(missing_complete.contains("missing required argument"));
        let extra = dispatcher
            .dispatch("task_list", &json!({"unexpected":true}), &worker)
            .unwrap_err()
            .to_string();
        assert!(extra.contains("unknown argument"));
        let denied = dispatcher
            .dispatch("review_sync", &json!({}), &worker)
            .unwrap_err()
            .to_string();
        assert!(denied.contains("superuser"));
        let private = dispatcher
            .dispatch(
                "task_add",
                &json!({"title":"private", "scope":"self"}),
                &worker,
            )
            .unwrap();
        let stranger = request("w9:p9");
        let hidden = dispatcher.dispatch("task_read", &json!({"id":private["id"]}), &stranger);
        assert!(
            hidden.is_err(),
            "another pane must not read the private task"
        );
        let foreign_turn = dispatcher
            .dispatch("turn_start", &json!({"pane":"w2:p2"}), &worker)
            .unwrap_err()
            .to_string();
        assert!(foreign_turn.contains("own pane"));
        let bad_send = dispatcher
            .dispatch("send_text", &json!({"target":"w1:p1", "text":7}), &worker)
            .unwrap_err()
            .to_string();
        assert!(bad_send.contains("text must be a string"));
        let pid = std::process::id() as i64;
        let current = crate::proc::stat(pid).unwrap();
        let mut authenticated = request("w1:p1");
        authenticated.caller = Some(crate::proc::Caller::new(
            Some(pid),
            Some(current.start),
            Some("codex".into()),
            Some("w1:p1".into()),
            "agent",
        ));
        let turn = dispatcher
            .dispatch("turn_start", &json!({}), &authenticated)
            .unwrap();
        assert_eq!(turn["pane_id"], "w1:p1");
        assert!(turn["turn_id"].as_str().is_some());
        let immediate = dispatcher
            .dispatch(
                "send_text",
                &json!({"target":"w1:p1", "text":"assembly smoke", "hold_while_typing":false, "press_enter":false}),
                &authenticated,
            )
            .unwrap();
        assert_eq!(immediate["pane_id"], "w1:p1");
        assert_eq!(immediate["text"], "assembly smoke");
        assert_eq!(immediate["source"], "send_text");
        assert_eq!(immediate["status"], "sent");

        let key_send = dispatcher
            .dispatch(
                "send_keys",
                &json!({"target":"w1:p1", "keys":"ctrl+c Enter", "hold_while_typing":false}),
                &authenticated,
            )
            .unwrap();
        assert_eq!(key_send["text"], "ctrl+c Enter");
        assert_eq!(key_send["status"], "sent");

        let queued = dispatcher
            .dispatch(
                "send_text",
                &json!({"target":"w1:p1", "text":"queued assembly", "hold_while_typing":true, "wait_s":0}),
                &authenticated,
            )
            .unwrap();
        assert_eq!(queued["queued"], true);
        assert_eq!(queued["source"], "send_text");
        {
            let (released, wake) = &*worker_gate;
            *released.lock().unwrap() = true;
            wake.notify_all();
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let history = services.flock.message_history.lock().unwrap();
            if history.iter().any(|message| {
                message.id == queued["message_id"].as_str().unwrap_or_default()
                    && message.status == "sent"
            }) {
                break;
            }
            drop(history);
            assert!(Instant::now() < deadline, "queued message did not complete");
            std::thread::sleep(Duration::from_millis(10));
        }

        flush_script.store(true, Ordering::SeqCst);
        let flushed = dispatcher
            .dispatch("flush_box", &json!({"target":"w1:p1"}), &authenticated)
            .unwrap();
        assert_eq!(flushed["flushed"], true);
        assert_eq!(flushed["text"], "stranded text");

        let history = services.flock.message_history.lock().unwrap();
        let direct = history
            .iter()
            .find(|message| message.text == "assembly smoke")
            .unwrap();
        assert!(!direct.hold_while_typing);
        assert!(!direct.press_enter);
        assert_eq!(direct.source, "send_text");
        let queued_message = history
            .iter()
            .find(|message| message.id == queued["message_id"].as_str().unwrap())
            .unwrap();
        assert!(queued_message.hold_while_typing);
        assert_eq!(queued_message.status, "sent");
        assert!(history.iter().any(|message| {
            message.kind == "keys" && message.text == "ctrl+c Enter" && message.status == "sent"
        }));
        drop(history);
        let role_denied = dispatcher
            .dispatch(
                "send_text",
                &json!({"target":"w2:p2", "text":"foreign"}),
                &authenticated,
            )
            .unwrap_err()
            .to_string();
        assert!(role_denied.contains("send_text to another pane requires a superuser"));
        let observed = observations.lock().unwrap();
        assert!(observed
            .text
            .iter()
            .any(|(pane, text)| pane == "w1:p1" && text == "assembly smoke"));
        let key_batches: Vec<_> = observed
            .keys
            .iter()
            .filter(|observation| observation.pane == "w1:p1")
            .collect();
        assert!(key_batches.iter().any(|observation| {
            observation.keys.len() == 1 && observation.keys[0] == "ctrl+c Enter"
        }));
        assert!(
            key_batches
                .iter()
                .filter(|observation| observation.keys.len() == 1 && observation.keys[0] == "Enter")
                .count()
                >= 1
        );
        assert!(
            observed
                .factory_threads
                .iter()
                .any(|thread| *thread != std::thread::current().id()),
            "queued delivery must use worker-local terminal factory"
        );
        drop(observed);
        let desk = request("w3:p3");
        let denied = dispatcher
            .dispatch("review_sync", &json!({}), &desk)
            .unwrap_err()
            .to_string();
        assert!(denied.contains("superuser") || denied.contains("not permitted"));
        services.shutdown();
    }

    #[test]
    fn assembled_dispatcher_executes_task_statistics_with_real_store_privacy() {
        let observations = Arc::new(Mutex::new(SendObservations::default()));
        let flush_script = Arc::new(AtomicBool::new(false));
        let worker_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (_dir, services) = make_services(
            "registered-task-statistics",
            send_factory(observations, flush_script, worker_gate),
            None,
        );
        let dispatcher = services.production_dispatcher();
        let worker = request("w1:p1");
        let tasks = services.flock().stores().tasks.unwrap();
        let runtime = services.runtime_handle();

        for (scope, title) in [
            ("global", "visible global task"),
            ("w1:p1", "visible private task"),
            ("w3:p3", "hidden private task"),
        ] {
            runtime.block_on(tasks.add(scope, title, "")).unwrap();
        }
        let stats = dispatcher
            .dispatch("task_stats", &json!({}), &worker)
            .unwrap();
        assert_eq!(stats["totals"]["open"], 2);
        let metrics = dispatcher
            .dispatch(
                "task_metrics",
                &json!({"window_s":"3600","bucket":""}),
                &worker,
            )
            .unwrap();
        assert_eq!(metrics["scope"], "visible");
        assert_eq!(metrics["window"]["seconds"], 3600);
        assert_eq!(metrics["window"]["bucket"], "hour");
        assert!(metrics["throughput"]["created"].as_i64().unwrap() > 0);

        let invalid_window = dispatcher
            .dispatch(
                "task_metrics",
                &json!({"window_s":[],"scope":"w3:p3","since":{"bad":"type"}}),
                &worker,
            )
            .unwrap_err()
            .to_string();
        assert!(
            invalid_window
                .contains("float() argument must be a string or a real number, not 'list'"),
            "{invalid_window}"
        );

        let precedence = dispatcher
            .dispatch(
                "task_metrics",
                &json!({"scope":"w3:p3","since":{"bad":"type"}}),
                &worker,
            )
            .unwrap_err()
            .to_string();
        assert!(
            precedence.contains("per-pane tasks are private"),
            "{precedence}"
        );

        let denied = dispatcher
            .dispatch("task_stats", &json!({"scope":"w3:p3"}), &worker)
            .unwrap_err()
            .to_string();
        assert!(denied.contains("per-pane tasks are private"), "{denied}");
    }

    #[test]
    fn assembled_dispatcher_reports_real_metrics_and_named_missing_store_refusal() {
        let observations = Arc::new(Mutex::new(SendObservations::default()));
        let flush_script = Arc::new(AtomicBool::new(false));
        let worker_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let dir = tempfile::tempdir().unwrap();
        let metrics = Arc::new(MetricsStore::open(dir.path().join("metrics.db")).unwrap());
        let event_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        metrics.record(&crate::metrics::Event {
            at: event_at,
            kind: "message.sent".into(),
            pane_id: Some("worker-pane".into()),
            mailbox: Some("team".into()),
            sender: Some("synthetic-agent".into()),
            ..Default::default()
        });
        let (_services_dir, services) = make_services(
            "registered-metrics",
            send_factory(observations, flush_script, worker_gate),
            Some(metrics),
        );
        let dispatcher = services.production_dispatcher();
        services
            .flock()
            .stores()
            .inbox
            .unwrap()
            .send(&crate::inbox_store::Outgoing {
                mailbox: "private@example.test",
                body: Some("PRIVATE INBOX BODY"),
                subject: Some("PRIVATE INBOX SUBJECT"),
                sender: Some("private sender prose"),
                ..Default::default()
            })
            .unwrap();
        let report = dispatcher
            .dispatch("metrics", &json!({"window_s":3600}), &request("w1:p1"))
            .unwrap();
        assert_eq!(report["counts"]["events"], 1);
        assert_eq!(report["counts"]["by_kind"]["message.sent"], 1);
        assert_eq!(report["counts"]["by_pane"]["worker-pane"], 1);
        assert_eq!(report["counts"]["by_recipient"]["team"], 1);
        assert_eq!(report["counts"]["by_sender"]["synthetic-agent"], 1);
        assert!(
            report.get("events").is_none(),
            "raw events must not be exposed"
        );
        assert!(report["coverage"]["window_has_events"].as_bool().unwrap());
        let serialized = report.to_string();
        for private_value in [
            "PRIVATE INBOX BODY",
            "PRIVATE INBOX SUBJECT",
            "private@example.test",
            "private sender prose",
        ] {
            assert!(
                !serialized.contains(private_value),
                "metrics leaked {private_value:?}"
            );
        }

        let invalid_window = dispatcher
            .dispatch(
                "metrics",
                &json!({"window_s":[],"until":{},"since":[]}),
                &request("w1:p1"),
            )
            .unwrap_err()
            .to_string();
        assert!(
            invalid_window
                .contains("float() argument must be a string or a real number, not 'list'"),
            "{invalid_window}"
        );

        let empty = dispatcher
            .dispatch(
                "metrics",
                &json!({"since":event_at + 100.0,"until":event_at + 200.0}),
                &request("w1:p1"),
            )
            .unwrap();
        assert_eq!(empty["counts"]["events"], 0);
        assert_eq!(empty["coverage"]["events_since"], event_at);
        assert!(!empty["coverage"]["window_has_events"].as_bool().unwrap());

        let mut stores = services.flock().stores();
        stores.inbox = None;
        services.flock().attach_stores(stores.clone());
        let missing_inbox = dispatcher
            .dispatch("metrics", &json!({}), &request("w1:p1"))
            .unwrap_err()
            .to_string();
        assert_eq!(missing_inbox, "inbox store is not wired");

        stores.metrics = None;
        services.flock().attach_stores(stores);
        let missing = dispatcher
            .dispatch("metrics", &json!({}), &request("w1:p1"))
            .unwrap_err()
            .to_string();
        assert_eq!(missing, "metrics store is not wired");
    }

    #[test]
    fn assembled_task_reap_refuses_fixture_and_reaps_expired_lease_after_live_list_failure() {
        let observations = Arc::new(Mutex::new(SendObservations::default()));
        let flush_script = Arc::new(AtomicBool::new(false));
        let worker_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (dir, fixture_services) = make_services(
            "registered-task-reap-fixture",
            send_factory(
                Arc::clone(&observations),
                Arc::clone(&flush_script),
                Arc::clone(&worker_gate),
            ),
            None,
        );
        let dispatcher = fixture_services.production_dispatcher();
        let fixture_refusal = dispatcher
            .dispatch("task_reap", &json!({}), &request("w2:p2"))
            .unwrap_err()
            .to_string();
        assert!(
            fixture_refusal.contains("alive_source='live'"),
            "{fixture_refusal}"
        );
        let worker_refusal = dispatcher
            .dispatch("task_reap", &json!({}), &request("w1:p1"))
            .unwrap_err()
            .to_string();
        assert!(
            worker_refusal.contains("task_reap is not permitted for role worker"),
            "{worker_refusal}"
        );

        let missing_tmux = dir.path().join("owned-missing-tmux-command");
        let live_factory: LoopTerminalFactory = Arc::new(move || {
            Box::new(crate::terminal::tmux::TmuxBackend::new(
                missing_tmux.clone(),
            ))
        });
        let (_live_dir, live_services) =
            make_services("registered-task-reap-live", live_factory, None);
        let live_tasks = live_services.flock().stores().tasks.unwrap();
        let handle = live_services.runtime_handle();
        let task = handle
            .block_on(live_tasks.add("global", "expired lease", ""))
            .unwrap();
        handle
            .block_on(live_tasks.set_status(&task.id, "in_progress", None, None))
            .unwrap();
        handle.block_on(live_tasks.insert_lease(&task.id, "w9:p9", 1.0, 1.0, 2.0));
        let live_dispatcher = live_services.production_dispatcher();
        let reaped = live_dispatcher
            .dispatch("task_reap", &json!({}), &request("w2:p2"))
            .unwrap();
        assert_eq!(
            reaped["requeued"],
            json!([{"task_id":task.id,"pane_id":"w9:p9","reason":"expired"}])
        );
        assert_eq!(
            handle.block_on(live_tasks.get(&task.id)).unwrap().status,
            "open"
        );
        assert_eq!(handle.block_on(live_tasks.leases(&task.id)), 0);
        assert!(handle
            .block_on(live_tasks.events_named(&task.id))
            .iter()
            .any(|event| event == "requeued"));
        let again = live_dispatcher
            .dispatch("task_reap", &json!({}), &request("w2:p2"))
            .unwrap();
        assert_eq!(again["requeued"], json!([]));
    }

    #[test]
    fn assembled_dispatcher_executes_canonical_memory_handlers_and_enqueue_receipts() {
        let observations = Arc::new(Mutex::new(SendObservations::default()));
        let flush_script = Arc::new(AtomicBool::new(false));
        let worker_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (dir, services) = make_services(
            "registered-memory",
            send_factory(observations, flush_script, worker_gate),
            None,
        );
        let path = dir.path().join("memory.db");
        drop(crate::db::open_writer(&path).unwrap());
        let memory = Arc::new(
            crate::memory::service::MemoryService::open(
                &path,
                1,
                Duration::from_secs(1),
                Duration::from_secs(1),
            )
            .unwrap(),
        );
        services
            .flock()
            .attach_memory_service(Arc::clone(&memory))
            .unwrap();
        let stores = services.flock().stores();
        assert!(Arc::ptr_eq(
            &services.flock().memory_service().unwrap(),
            &memory
        ));
        assert!(Arc::ptr_eq(
            stores.memory.as_ref().unwrap(),
            memory.read_pool()
        ));

        let dispatcher = services.production_dispatcher();
        let worker = request("w1:p1");
        let stats = dispatcher
            .dispatch("memory_stats", &json!({}), &worker)
            .unwrap();
        assert_eq!(stats, memory.stats().unwrap());
        assert_eq!(stats["sources"], 0);
        assert_eq!(stats["queue_depth"], 0);
        let status = dispatcher
            .dispatch("memory_status", &json!({}), &worker)
            .unwrap();
        assert_eq!(status, memory.status().unwrap());
        assert_eq!(status["ingestor"]["running"], false);

        let denied_worker = dispatcher
            .dispatch("memory_ingest", &json!({}), &worker)
            .unwrap_err()
            .to_string();
        assert!(
            denied_worker.contains("requires a superuser"),
            "{denied_worker}"
        );
        let owner = request("w2:p2");
        let missing_path = dispatcher
            .dispatch("memory_ingest", &json!({}), &owner)
            .unwrap_err()
            .to_string();
        assert_eq!(
            missing_path,
            "memory_ingest: missing required argument(s): path"
        );

        let codex_dir = dir.path().join(".codex");
        std::fs::create_dir_all(&codex_dir).unwrap();
        let inferred_path = codex_dir.join("thread_history.jsonl");
        std::fs::write(
            &inferred_path,
            "{\"type\":\"session\",\"id\":\"queued-codex\",\"harness\":\"codex\",\"project\":\"demo\"}\n",
        )
        .unwrap();
        let inferred = dispatcher
            .dispatch(
                "memory_ingest",
                &json!({"path":inferred_path,"harness":"","project":""}),
                &owner,
            )
            .unwrap();
        assert_eq!(
            inferred,
            json!({
                "source_id":1,
                "queue_id":1,
                "path":inferred_path.to_string_lossy(),
                "harness":"codex",
                "project":null
            })
        );

        let explicit_path = dir.path().join("explicit-history.jsonl");
        std::fs::write(&explicit_path, "{}\n").unwrap();
        let explicit = dispatcher
            .dispatch(
                "memory_ingest",
                &json!({"path":explicit_path,"harness":"claude","project":"explicit-demo"}),
                &owner,
            )
            .unwrap();
        assert_eq!(
            explicit,
            json!({
                "source_id":2,
                "queue_id":2,
                "path":explicit_path.to_string_lossy(),
                "harness":"claude",
                "project":"explicit-demo"
            })
        );

        let queued_rows = memory
            .read_pool()
            .with(|connection| {
                let mut statement = connection
                    .prepare(
                        "SELECT q.id, q.source_id, q.state, s.path, s.harness, s.project, s.state \
                         FROM ingest_queue q JOIN sources s ON s.id=q.source_id ORDER BY q.id",
                    )
                    .unwrap();
                let rows = statement
                    .query_map([], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, String>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, Option<String>>(5)?,
                            row.get::<_, String>(6)?,
                        ))
                    })
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                Ok(rows)
            })
            .unwrap();
        assert_eq!(
            queued_rows,
            vec![
                (
                    1,
                    1,
                    "queued".into(),
                    inferred_path.to_string_lossy().into_owned(),
                    "codex".into(),
                    None,
                    "pending".into(),
                ),
                (
                    2,
                    2,
                    "queued".into(),
                    explicit_path.to_string_lossy().into_owned(),
                    "claude".into(),
                    Some("explicit-demo".into()),
                    "pending".into(),
                ),
            ]
        );
        let after_enqueue = dispatcher
            .dispatch("memory_status", &json!({}), &worker)
            .unwrap();
        assert_eq!(after_enqueue["queue"]["queued"], 2);
        assert_eq!(after_enqueue["ingestor"]["running"], false);
    }

    #[test]
    fn assembled_dispatcher_refuses_memory_handlers_without_canonical_service() {
        let observations = Arc::new(Mutex::new(SendObservations::default()));
        let flush_script = Arc::new(AtomicBool::new(false));
        let worker_gate = Arc::new((Mutex::new(false), Condvar::new()));
        let (dir, services) = make_services(
            "registered-memory-missing",
            send_factory(observations, flush_script, worker_gate),
            None,
        );
        let dispatcher = services.production_dispatcher();
        let worker = request("w1:p1");
        for command in ["memory_stats", "memory_status"] {
            let refused = dispatcher
                .dispatch(command, &json!({}), &worker)
                .unwrap_err()
                .to_string();
            assert!(
                refused.contains("memory service is not enabled"),
                "{command}: {refused}"
            );
        }
        let path = dir.path().join("source.jsonl");
        std::fs::write(&path, "{}\n").unwrap();
        let refused = dispatcher
            .dispatch("memory_ingest", &json!({"path":path}), &request("w2:p2"))
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("memory service is not enabled"),
            "{refused}"
        );
    }

    #[test]
    fn production_dispatcher_owner_queue_uses_shared_lazy_service() {
        let observations = Arc::new(Mutex::new(SendObservations::default()));
        let gate = Arc::new((Mutex::new(true), Condvar::new()));
        let (_dir, services) = make_services(
            "owner-queue-runtime",
            send_factory(
                Arc::clone(&observations),
                Arc::new(AtomicBool::new(false)),
                gate,
            ),
            None,
        );
        let dispatcher = services.production_dispatcher();
        let owner = request("w2:p2");
        assert!(dispatcher
            .dispatch(
                "owner_push",
                &json!({"target":"w3:p3","body":"missing path"}),
                &owner,
            )
            .unwrap_err()
            .to_string()
            .contains("runtime owner push path is not wired"));
        let path = tempfile::tempdir().unwrap();
        services
            .attach_owner_push_path(path.path().join("owner.db"))
            .unwrap();
        let pushed = dispatcher
            .dispatch(
                "owner_push",
                &json!({"target":"w3:p3","body":"queue one"}),
                &owner,
            )
            .unwrap();
        assert_eq!(pushed["outcome"], "stored");
        dispatcher
            .dispatch(
                "owner_push",
                &json!({"target":"w3:p3","body":"queue two"}),
                &owner,
            )
            .unwrap();
        let desk = request("w3:p3");
        let wrong = dispatcher
            .dispatch("owner_poll", &json!({"pane_id":"w3:p3"}), &owner)
            .unwrap_err();
        assert!(wrong
            .to_string()
            .contains("Only a lane may report its own polling"));
        let poll = dispatcher
            .dispatch("owner_poll", &json!({}), &desk)
            .unwrap();
        assert_eq!(poll["pane_id"], "w3:p3");
        let taken = dispatcher
            .dispatch("owner_take", &json!({}), &desk)
            .unwrap();
        assert_eq!(taken["messages"][0]["body"], "queue one");
        assert_eq!(taken["messages"][1]["body"], "queue two");
        let drained = dispatcher
            .dispatch("owner_take", &json!({}), &desk)
            .unwrap();
        assert_eq!(
            drained["messages"].as_array().unwrap().len(),
            0,
            "committed receipt is not redelivered"
        );
        services.shutdown();
    }

    #[test]
    fn production_dispatcher_task_mutation_handlers_use_real_tasks_and_identity() {
        let observations = Arc::new(Mutex::new(SendObservations::default()));
        let (_dir, services) = make_services(
            "task-mutations-runtime",
            send_factory(
                Arc::clone(&observations),
                Arc::new(AtomicBool::new(false)),
                Arc::new((Mutex::new(true), Condvar::new())),
            ),
            None,
        );
        let dispatcher = services.production_dispatcher();
        let owner = request("w2:p2");
        let pid = std::process::id() as i64;
        let process = crate::proc::stat(pid).unwrap();
        let mut worker = request("w1:p1");
        worker.caller = Some(crate::proc::Caller::new(
            Some(pid),
            Some(process.start),
            Some("codex".into()),
            Some("w1:p1".into()),
            "agent",
        ));
        let task = dispatcher
            .dispatch(
                "task_add",
                &json!({"title":"registered mutation","body":"before"}),
                &worker,
            )
            .unwrap();
        let id = task["id"].as_str().unwrap();
        let updated = dispatcher
            .dispatch(
                "task_update",
                &json!({"id":id,"title":"updated","body":"after"}),
                &owner,
            )
            .unwrap();
        assert_eq!(updated["title"], "updated");
        dispatcher
            .dispatch(
                "task_assign",
                &json!({"id":id,"assignees":["w1:p1"]}),
                &owner,
            )
            .unwrap();
        let claimed = dispatcher
            .dispatch("task_next", &json!({"scope":"global"}), &worker)
            .unwrap();
        assert_eq!(claimed["task"]["id"], id);
        let heartbeat = dispatcher
            .dispatch("task_heartbeat", &json!({"id":id}), &worker)
            .unwrap();
        assert_eq!(heartbeat["task_id"], id);
        dispatcher
            .dispatch(
                "task_note",
                &json!({"id":id,"reason":"real runtime note"}),
                &worker,
            )
            .unwrap();
        let released = dispatcher
            .dispatch("task_release", &json!({"id":id}), &worker)
            .unwrap();
        assert_eq!(released["status"], "open");
        let blocked = dispatcher
            .dispatch("task_block", &json!({"id":id,"reason":"wait"}), &worker)
            .unwrap();
        assert_eq!(blocked["status"], "blocked");
        let unblocked = dispatcher
            .dispatch("task_unblock", &json!({"id":id}), &owner)
            .unwrap();
        assert_eq!(unblocked["status"], "open");
        let denied = dispatcher
            .dispatch(
                "task_assign",
                &json!({"id":id,"assignees":["w1:p1"]}),
                &worker,
            )
            .unwrap_err();
        assert!(denied.to_string().contains("not permitted for role worker"));
        let unchanged = dispatcher
            .dispatch("task_read", &json!({"id":id}), &owner)
            .unwrap();
        assert_eq!(
            unchanged["assignees"],
            json!(["w1:p1"]),
            "refused assignment must leave state unchanged"
        );
        let cancelled = dispatcher
            .dispatch("task_cancel", &json!({"id":id,"reason":"done"}), &owner)
            .unwrap();
        assert_eq!(cancelled["status"], "cancelled");
        services.shutdown();
    }

    #[test]
    fn dispatcher_manual_fire_holds_then_delivers_through_real_outbox_callback() {
        let observations = Arc::new(Mutex::new(SendObservations::default()));
        let screen = Arc::new(Mutex::new("❯ human draft".to_owned()));
        let (outbox_started_tx, outbox_started_rx) = std::sync::mpsc::channel();
        let release_outbox = Arc::new((Mutex::new(false), Condvar::new()));
        let terminal_factory = manual_fire_factory(
            Arc::clone(&observations),
            Arc::clone(&screen),
            outbox_started_tx,
            Arc::clone(&release_outbox),
        );
        let (_dir, services) = make_services("manual-fire-held", terminal_factory, None);
        let pane = json!({"pane_id":"w1:p1","tab_label":"1"});
        services
            .add_dunk(
                pane.as_object(),
                &crate::flock::dunks::AddArgs {
                    text: Some("held {id}".into()),
                    start: false,
                    ..Default::default()
                },
            )
            .unwrap();
        let mut handlers = crate::api::Dispatcher::new();
        handlers.register_stop_all_fire_list_dunks();
        let dispatcher = services.dispatcher(handlers);
        let result = dispatcher
            .dispatch(
                "fire_dunk",
                &json!({"id":"d1","wait":true}),
                &request("w2:p2"),
            )
            .unwrap();
        assert_eq!(result["queued"], true);
        let message_id = result["message_id"].as_str().expect("queued message id");
        assert!(services
            .flock()
            .outbox
            .lock()
            .unwrap()
            .get("w1:p1")
            .unwrap()
            .iter()
            .any(|message| message.id == message_id));

        outbox_started_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("canonical outbox worker created its own terminal");
        *screen.lock().unwrap() = "❯ ".into();
        let (released, wake) = &*release_outbox;
        *released.lock().unwrap() = true;
        wake.notify_all();

        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        loop {
            if services.flock().dunks.get("d1").unwrap()["send_count"] == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "queued fire action was not delivered by the outbox callback"
            );
            std::thread::yield_now();
        }
        assert_eq!(
            services.flock().dunks.get("d1").unwrap()["last_sent_text"],
            "held d1"
        );
        assert_eq!(observations.lock().unwrap().text.len(), 1);
        services.shutdown();
    }
}
