//! Production `PmEnv` for the owner PM commands `pm_routing` and `pm_digest` (t1598).
//!
//! `PmEnv` requires the whole `InboxEnv` surface, so the PM view is lent to the handler for one
//! call, the way `with_inbox_env` lends the inbox view. It borrows the request's inbox view and
//! carries the request's configuration `Env`, captured from the process environment.

use serde_json::Value;

use super::RequestRuntimeAdapter;
use crate::config::Env;
use crate::db::ReadPool;
use crate::error::DunkError;
use crate::flock::commands_inbox::{InboxEnv, Nudge, RouteEntry};
use crate::flock::mailbox::MailboxEnv;
use crate::inbox_store::Store;
use crate::pm::PmEnv;

/// The request's inbox view, plus the configuration for this request.
struct PmRuntimeView<'a> {
    inbox: &'a dyn InboxEnv,
    config: &'a Env,
}

impl PmEnv for PmRuntimeView<'_> {
    fn config(&self) -> Result<&Env, DunkError> {
        Ok(self.config)
    }
}

impl MailboxEnv for PmRuntimeView<'_> {
    fn list_panes(&self) -> Vec<Value> {
        self.inbox.list_panes()
    }

    fn list_panes_grouped(&self) -> Vec<Value> {
        self.inbox.list_panes_grouped()
    }

    fn try_list_panes_grouped(&self) -> Result<Vec<Value>, DunkError> {
        self.inbox.try_list_panes_grouped()
    }

    fn registered_pane(&self, pane_id: &str) -> (bool, bool, Option<String>) {
        self.inbox.registered_pane(pane_id)
    }
}

impl InboxEnv for PmRuntimeView<'_> {
    fn caller_role(&self, pane_id: &str) -> Result<String, DunkError> {
        self.inbox.caller_role(pane_id)
    }

    fn mailbox_override(&self) -> Option<String> {
        self.inbox.mailbox_override()
    }

    fn inbox_silent_mailboxes(&self) -> Option<String> {
        self.inbox.inbox_silent_mailboxes()
    }

    fn backend_name(&self) -> Result<String, DunkError> {
        self.inbox.backend_name()
    }

    fn lane_panes(&self) -> Vec<String> {
        self.inbox.lane_panes()
    }

    fn pool(&self) -> &ReadPool {
        self.inbox.pool()
    }

    fn store(&self) -> &Store {
        self.inbox.store()
    }

    fn now(&self) -> f64 {
        self.inbox.now()
    }

    fn record_metric(
        &self,
        kind: &str,
        pane_id: Option<&str>,
        mailbox: &str,
        sender: Option<&str>,
        duration_ms: Option<i64>,
        bytes: Option<i64>,
    ) {
        self.inbox
            .record_metric(kind, pane_id, mailbox, sender, duration_ms, bytes)
    }

    fn pm_routing(&self) -> Vec<RouteEntry> {
        self.inbox.pm_routing()
    }

    fn fold_into_queued_notice(
        &self,
        pane_id: &str,
        text: &str,
        message_id: &str,
    ) -> Option<String> {
        self.inbox
            .fold_into_queued_notice(pane_id, text, message_id)
    }

    fn send_now(&self, pane_id: &str, text: &str, message_id: &str) -> Result<Nudge, DunkError> {
        self.inbox.send_now(pane_id, text, message_id)
    }

    fn record_sent(
        &self,
        pane_id: Option<&str>,
        mailbox: &str,
        sender: Option<&str>,
        bytes: usize,
        attachments: usize,
    ) {
        self.inbox
            .record_sent(pane_id, mailbox, sender, bytes, attachments)
    }

    fn tasks_for_pane(&self, pane_id: &str) -> Result<Value, DunkError> {
        self.inbox.tasks_for_pane(pane_id)
    }
}

/// Lend the PM view for one request. A refusal from the inbox view (no outbox, no inbox store) is
/// returned unchanged, so a PM command never answers from a partial environment.
pub(super) fn with_request_pm_env(
    adapter: &RequestRuntimeAdapter<'_>,
    operation: &mut dyn FnMut(&dyn PmEnv) -> Result<Value, DunkError>,
) -> Result<Value, DunkError> {
    let config = Env::from_process();
    adapter.with_request_inbox(&mut |inbox| {
        let view = PmRuntimeView {
            inbox,
            config: &config,
        };
        operation(&view)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flock::panes::Stores;
    use crate::roles::Roles;
    use crate::server::RequestCtx;
    use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
    use crate::tasks::Tasks;
    use crate::terminal::fakes::FakeTerminal;
    use crate::terminal::Terminal;
    use serde_json::json;
    use std::ffi::{OsStr, OsString};
    use std::sync::{Arc, Mutex, MutexGuard};

    /// Serialises the tests that set process variables, as `api::tests` does for its ledger.
    static PM_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn lock_env() -> MutexGuard<'static, ()> {
        PM_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Sets one process variable for a test and restores the previous value when dropped.
    struct Var(&'static str, Option<OsString>);

    impl Var {
        fn set(name: &'static str, value: &OsStr) -> Self {
            let previous = std::env::var_os(name);
            std::env::set_var(name, value);
            Self(name, previous)
        }
    }

    impl Drop for Var {
        fn drop(&mut self) {
            match self.1.take() {
                Some(previous) => std::env::set_var(self.0, previous),
                None => std::env::remove_var(self.0),
            }
        }
    }

    /// Real services over temporary stores, with one attached inbox store.
    fn fixture() -> (tempfile::TempDir, Arc<RuntimeServices>, Arc<Store>) {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let inbox =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        let env = Env::default();
        let mut roles = Roles::new(
            &env,
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register("w3:p3", "desk", None, None, None, None, None, None)
            .unwrap();
        let executable = dir.path().join("test-daemon");
        std::fs::write(&executable, b"fixture identity").unwrap();
        let services = RuntimeServices::new(RuntimeServiceInputs {
            launches_file: dir.path().join("launches.json"),
            lane_registry_file: dir.path().join("lanes.json"),
            persist_enabled: false,
            stores: Stores {
                tasks: Some(tasks),
                inbox: Some(Arc::clone(&inbox)),
                ..Stores::default()
            },
            roles,
            legacy_superusers: None,
            release_version: "fixture".into(),
            state_file: dir.path().join("state.json"),
            executable,
            env,
            observability: Some(RuntimeObservability {
                state_dir: dir.path().join("metrics"),
                memory_db: None,
            }),
            runtime,
            terminal_factory: Arc::new(|| Box::new(FakeTerminal::default())),
        })
        .unwrap();
        (dir, services, inbox)
    }

    fn desk() -> RequestCtx {
        RequestCtx {
            self_pane_id: Some("w3:p3".into()),
            socket_peer: Some(true),
            ..RequestCtx::default()
        }
    }

    fn routing_file(dir: &tempfile::TempDir, body: &str) -> std::path::PathBuf {
        let path = dir.path().join("pm-routing.json");
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn pm_routing_returns_routing_registry_and_vp_pane() {
        let (dir, services, _inbox) = fixture();
        let path = routing_file(
            &dir,
            r#"{"poolrad": {"pm": "w1:p1", "members": ["w1:p2"], "mode": "redirect", "vp_pane": "w9:pH"}}"#,
        );
        let _lock = lock_env();
        let _routing = Var::set("DUNKINGSHEEP_PM_ROUTING", path.as_os_str());
        let _vp_pane = Var::set("DUNKINGSHEEP_VP_PANE", OsStr::new("w9:pH"));
        let result = services
            .production_dispatcher()
            .dispatch("pm_routing", &json!({}), &desk())
            .unwrap();
        assert_eq!(result["routing"]["poolrad"]["pm"], "w1:p1", "{result}");
        assert_eq!(
            result["routing"]["poolrad"]["members"],
            json!(["w1:p2"]),
            "{result}"
        );
        assert_eq!(result["routing"]["poolrad"]["mode"], "redirect", "{result}");
        assert_eq!(result["registry"], path.to_str().unwrap(), "{result}");
        assert_eq!(result["vp_pane"], "w9:pH", "{result}");
    }

    #[test]
    fn pm_routing_bad_file_is_a_named_refusal() {
        let (dir, services, _inbox) = fixture();
        // An entry without `pm` is a routing load error, which the owner view must not hide.
        let path = routing_file(&dir, r#"{"poolrad": {"members": []}}"#);
        let _lock = lock_env();
        let _routing = Var::set("DUNKINGSHEEP_PM_ROUTING", path.as_os_str());
        let error = services
            .production_dispatcher()
            .dispatch("pm_routing", &json!({}), &desk())
            .unwrap_err();
        assert!(matches!(error, DunkError::Refused(_)), "{error}");
    }

    #[test]
    fn pm_routing_missing_file_is_empty_not_an_error() {
        let (dir, services, _inbox) = fixture();
        let path = dir.path().join("absent.json");
        let _lock = lock_env();
        let _routing = Var::set("DUNKINGSHEEP_PM_ROUTING", path.as_os_str());
        let result = services
            .production_dispatcher()
            .dispatch("pm_routing", &json!({}), &desk())
            .unwrap();
        assert_eq!(result["routing"], json!({}), "{result}");
        assert_eq!(result["registry"], path.to_str().unwrap(), "{result}");
    }

    #[test]
    fn pm_digest_creates_the_row_then_updates_it_in_place() {
        let (_dir, services, inbox) = fixture();
        let dispatcher = services.production_dispatcher();
        // `w8:p9` is not a listed pane, so the digest is stored and no notice is attempted.
        let args = json!({
            "project": "poolrad",
            "payload": {"note": "slice two check"},
            "to": "w8:p9",
        });
        let first = dispatcher.dispatch("pm_digest", &args, &desk()).unwrap();
        assert_eq!(first["action"], "created", "{first}");
        let digest_id = first["digest_id"].as_str().unwrap().to_string();
        assert!(inbox.digest_exists(&digest_id).unwrap(), "{first}");
        let second = dispatcher.dispatch("pm_digest", &args, &desk()).unwrap();
        assert_eq!(second["action"], "updated", "{second}");
        assert_eq!(second["digest_id"], digest_id.as_str(), "{second}");
    }

    #[test]
    fn pm_env_refuses_without_an_attached_outbox() {
        let (_dir, services, _inbox) = fixture();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::default()));
        let request = desk();
        // A request adapter built directly has no outbox, and the PM view needs the inbox view.
        let adapter = RequestRuntimeAdapter::new(
            services.flock.as_ref(),
            &request,
            runtime.handle(),
            &terminal,
        );
        let error = with_request_pm_env(&adapter, &mut |_| Ok(Value::Null)).unwrap_err();
        assert!(
            matches!(&error, DunkError::Refused(text) if text.contains("outbox")),
            "{error}"
        );
    }
}
