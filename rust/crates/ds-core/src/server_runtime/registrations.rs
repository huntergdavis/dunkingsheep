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
    use crate::terminal::{fakes::FakeTerminal, TerminalError};
    use serde_json::json;
    use tokio::runtime::Builder;

    fn factory(fail_grouped_panes: Option<TerminalError>) -> LoopTerminalFactory {
        Arc::new(move || {
            let mut terminal = FakeTerminal::new();
            terminal.fail_grouped_panes = fail_grouped_panes.clone();
            Box::new(terminal)
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
        let (_dir, services) = make_services("registered", factory(None), None);
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
        let desk = request("w3:p3");
        let denied = dispatcher
            .dispatch("review_sync", &json!({}), &desk)
            .unwrap_err()
            .to_string();
        assert!(denied.contains("superuser") || denied.contains("not permitted"));
        services.shutdown();
    }
}
