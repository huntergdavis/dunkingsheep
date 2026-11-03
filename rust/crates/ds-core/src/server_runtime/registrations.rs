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
    handlers.register_list_panes_read_pane_workspaces();
    handlers.register_retire_pane();
    handlers.register_superuser_status_add_remove();
    handlers.register_superuser_recover_tokens();
    handlers.register_gh_token();
    handlers.register_review_sync_list();
    handlers.register_poolrad_sync_progress();
    handlers.register_report_run_list();
    handlers.register_owner_stats_receipts_conversation();
    handlers.register_owner_delivered_message_state_reply_stats();
    handlers.register_owner_reply_ingest_outbox();
    handlers.register_pm_digest_routing();
    handlers.register_task_follow_unfollow_followers();
    handlers.register_task_add_list_read();
    handlers.register_task_delete();
    handlers.register_task_complete();
    handlers.register_send_text_keys_flush_box();
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

    use crate::config::Env;
    use crate::flock::loop_env::LoopTerminalFactory;
    use crate::flock::panes::Stores;
    use crate::metrics::MetricsStore;
    use crate::roles::Roles;
    use crate::server::RequestCtx;
    use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs};
    use crate::tasks::Tasks;
    use crate::terminal::{
        fakes::FakeTerminal, AgentSpec, Pane, Resize, Tab, Terminal, TerminalError, Workspace,
    };
    use serde_json::json;
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::{AtomicBool, Ordering};
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
            })
        })
    }

    fn make_services(
        tag: &str,
        terminal_factory: LoopTerminalFactory,
        metrics: Option<Arc<MetricsStore>>,
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
            persist_enabled: false,
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
            "task_follow",
            "task_unfollow",
            "task_followers",
            "memory_recall",
            "memory_context",
            "memory_search",
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
        assert!(observed.keys.iter().any(|observation| {
            observation.pane == "w1:p1"
                && observation.keys.len() == 2
                && observation.keys[0] == "ctrl+c"
                && observation.keys[1] == "Enter"
        }));
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
}
