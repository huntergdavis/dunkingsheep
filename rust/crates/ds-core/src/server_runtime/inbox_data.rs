//! Request-scoped inbox identity and backend sources.

use super::RequestRuntimeAdapter;
use crate::error::DunkError;

fn mailbox_override_from(value: Option<&str>) -> Option<String> {
    value
        .map(|s| s.trim_matches(|c: char| c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c)))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete InboxEnv assembly belongs to its dependent leaf.
    fn helper_inboxenv_caller_role(&self, pane_id: &str) -> Result<String, DunkError> {
        self.terminal
            .flock
            .caller_role(Some(pane_id), &self.terminal)
    }

    #[allow(dead_code)] // The complete InboxEnv assembly belongs to its dependent leaf.
    fn helper_inboxenv_mailbox_override(&self) -> Option<String> {
        mailbox_override_from(std::env::var("DUNKINGSHEEP_MAILBOX").ok().as_deref())
    }

    #[allow(dead_code)] // The complete InboxEnv assembly belongs to its dependent leaf.
    fn helper_inboxenv_backend_name(&self) -> Result<String, DunkError> {
        super::configured_backend_name()
    }
}

use super::outbox_runtime::FlockSnapshot;
use crate::flock::commands_inbox::{InboxEnv, Nudge, RouteEntry};
use crate::flock::mailbox::MailboxEnv;
use crate::flock::sends::OutboxWorkerPool;
use crate::flock::Flock;
use crate::inbox_store::Store;
use serde_json::Value;
use std::sync::Arc;

#[allow(dead_code)]
pub(super) struct InboxRuntimeView<'a> {
    pub(super) adapter: RequestRuntimeAdapter<'a>,
    pub(super) store: Arc<Store>,
    pub(super) flock: Arc<Flock>,
    pub(super) pool: Arc<OutboxWorkerPool>,
}
impl<'a> InboxRuntimeView<'a> {
    pub(super) fn new(
        adapter: RequestRuntimeAdapter<'a>,
        flock: Arc<Flock>,
        pool: Arc<OutboxWorkerPool>,
    ) -> Result<Self, DunkError> {
        if !std::ptr::eq(adapter.terminal.flock, flock.as_ref()) {
            return Err(DunkError::Refused("inbox outbox flock mismatch".into()));
        }
        let store = adapter
            .inbox
            .clone()
            .ok_or_else(|| DunkError::Refused("inbox store is not attached".into()))?;
        Ok(Self {
            adapter,
            store,
            flock,
            pool,
        })
    }
}
impl MailboxEnv for InboxRuntimeView<'_> {
    fn list_panes(&self) -> Vec<Value> {
        self.adapter.list_panes()
    }
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.adapter.list_panes_grouped()
    }
    fn try_list_panes_grouped(&self) -> Result<Vec<Value>, DunkError> {
        self.adapter.try_list_panes_grouped()
    }
    fn registered_pane(&self, pane: &str) -> (bool, bool, Option<String>) {
        self.adapter.registered_pane(pane)
    }
}
impl InboxEnv for InboxRuntimeView<'_> {
    fn caller_role(&self, pane: &str) -> Result<String, DunkError> {
        self.adapter.helper_inboxenv_caller_role(pane)
    }
    fn mailbox_override(&self) -> Option<String> {
        self.adapter.helper_inboxenv_mailbox_override()
    }
    fn backend_name(&self) -> Result<String, DunkError> {
        self.adapter.helper_inboxenv_backend_name()
    }
    fn lane_panes(&self) -> Vec<String> {
        self.adapter.helper_inboxenv_lane_panes()
    }
    fn pool(&self) -> &crate::db::ReadPool {
        self.store.read_pool()
    }
    fn store(&self) -> &Store {
        &self.store
    }
    fn now(&self) -> f64 {
        self.adapter.helper_inboxenv_now()
    }
    fn record_metric(
        &self,
        kind: &str,
        pane: Option<&str>,
        mailbox: &str,
        sender: Option<&str>,
        duration: Option<i64>,
        bytes: Option<i64>,
    ) {
        self.adapter
            .helper_inboxenv_record_metric(kind, pane, mailbox, sender, duration, bytes)
    }
    fn pm_routing(&self) -> Vec<RouteEntry> {
        self.adapter.helper_inboxenv_pm_routing()
    }
    fn fold_into_queued_notice(&self, pane: &str, text: &str, id: &str) -> Option<String> {
        self.fold_notice(pane, text, id)
    }
    fn send_now(&self, pane: &str, text: &str, id: &str) -> Result<Nudge, DunkError> {
        self.send_notice(pane, text, id, &FlockSnapshot)
    }
    fn record_sent(
        &self,
        pane: Option<&str>,
        mailbox: &str,
        sender: Option<&str>,
        bytes: usize,
        attachments: usize,
    ) {
        self.adapter
            .helper_inboxenv_record_sent(pane, mailbox, sender, bytes, attachments)
    }
    fn tasks_for_pane(&self, pane: &str) -> Result<Value, DunkError> {
        self.adapter.helper_inboxenv_tasks_for_pane(pane)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Env,
        flock::{panes::Stores, status_runtime::StatusState, Flock},
        inbox_store::{Outgoing, Store},
        roles::Roles,
        server::RequestCtx,
        server_runtime::{outbox_runtime::OutboxRuntime, RequestRuntimeAdapter},
        terminal::{fakes::FakeTerminal, Terminal},
    };
    use std::sync::Mutex;

    #[test]
    fn production_env_inbox_data_uses_concrete_services() {
        let dir = crate::testsupport::dir("inbox-data-runtime");
        let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
        let request = RequestCtx::default();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        assert!(adapter.helper_inboxenv_caller_role("w1:p1").is_err());
        let mut roles = Roles::new(
            &Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register("w1:p1", "reviewer", None, None, None, None, None, None)
            .unwrap();
        flock.attach_roles(roles, None);
        assert_eq!(
            adapter.helper_inboxenv_caller_role("w1:p1").unwrap(),
            "reviewer"
        );
        assert_eq!(
            adapter.helper_inboxenv_backend_name().unwrap(),
            super::super::configured_backend_name().unwrap()
        );
        assert_eq!(
            adapter.helper_inboxenv_mailbox_override(),
            mailbox_override_from(std::env::var("DUNKINGSHEEP_MAILBOX").ok().as_deref())
        );
        assert_eq!(
            mailbox_override_from(Some(" \u{1c}lane\u{1f} ")),
            Some("lane".into())
        );
        assert_eq!(mailbox_override_from(Some(" \u{1c}\t")), None);
        assert_eq!(mailbox_override_from(None), None);
    }

    #[test]
    fn production_env_inbox_count_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let mailbox = mailbox_override_from(std::env::var("DUNKINGSHEEP_MAILBOX").ok().as_deref())
            .unwrap_or_else(|| "%9".into());
        let store =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        for (index, sender) in ["%3", "%2", "%3", "%6"].into_iter().enumerate() {
            store
                .send(&Outgoing {
                    mailbox: &mailbox,
                    body: Some("queued message"),
                    subject: Some("count fixture"),
                    sender: Some(sender),
                    pane_id: Some("%9"),
                    external_id: Some(match index {
                        0 => "count-1",
                        1 => "count-2",
                        2 => "count-3",
                        _ => "count-4",
                    }),
                    ..Outgoing::default()
                })
                .unwrap();
        }

        let routing_path = dir.path().join("pm-routing.json");
        std::fs::write(
            &routing_path,
            r#"{"alpha":{"pm":"%1","members":["%3","%2","%3"],"mode":"redirect","vp_pane":"%0"}}"#,
        )
        .unwrap();
        let routing_path = routing_path.to_string_lossy().into_owned();
        let dir_path = dir.path().to_string_lossy().into_owned();
        let env = Env::from_pairs(&[
            ("DUNKINGSHEEP_DIR", dir_path.as_str()),
            ("DUNKINGSHEEP_PM_ROUTING", routing_path.as_str()),
        ]);
        let flock = Arc::new(Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        ));
        flock
            .attach_status_state(StatusState::at_start(
                "test",
                dir.path().join("state.json"),
                dir.path().join("daemon"),
                env,
            ))
            .unwrap();
        flock.attach_stores(Stores {
            inbox: Some(Arc::clone(&store)),
            ..Stores::default()
        });
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
        )
        .unwrap();
        let pool = Arc::clone(&owner.pool);
        let worker_flock = Arc::clone(&flock);
        let handle = runtime.handle().clone();
        let (counts, missing_mailbox_error) = runtime
            .block_on(async {
                tokio::task::spawn_blocking(move || {
                    let terminal: Mutex<Box<dyn Terminal>> =
                        Mutex::new(Box::new(FakeTerminal::new()));
                    let request = RequestCtx::default();
                    let adapter =
                        RequestRuntimeAdapter::new(&worker_flock, &request, &handle, &terminal);
                    let view =
                        InboxRuntimeView::new(adapter, Arc::clone(&worker_flock), pool).unwrap();
                    let counts = worker_flock.inbox_count(&view, None, Some("%9")).unwrap();
                    let missing = worker_flock
                        .inbox_count(&view, None, None)
                        .unwrap_err()
                        .to_string();
                    (counts, missing)
                })
                .await
            })
            .unwrap();
        assert_eq!(counts.mailbox, mailbox);
        assert_eq!(counts.total, 4);
        assert_eq!(counts.unread, 4);
        assert_eq!(counts.archived, 0);
        assert_eq!(counts.undeliverable, 0);
        assert_eq!(counts.attachments_unread, 0);
        assert_eq!(counts.from_lanes, 3);
        assert!(counts.oldest_unread_s.is_some());
        assert!(missing_mailbox_error.contains("no mailbox given and no caller pane to default to"));

        let broken_path = dir.path().join("broken-routing.json");
        std::fs::write(&broken_path, b"not JSON").unwrap();
        let broken_path = broken_path.to_string_lossy().into_owned();
        let broken_env = Env::from_pairs(&[
            ("DUNKINGSHEEP_DIR", dir_path.as_str()),
            ("DUNKINGSHEEP_PM_ROUTING", broken_path.as_str()),
        ]);
        let broken_flock = Arc::new(Flock::new(
            dir.path().join("broken-launches.json"),
            dir.path().join("broken-lanes.json"),
            false,
        ));
        broken_flock
            .attach_status_state(StatusState::at_start(
                "test",
                dir.path().join("broken-state.json"),
                dir.path().join("daemon"),
                broken_env,
            ))
            .unwrap();
        broken_flock.attach_stores(Stores {
            inbox: Some(store),
            ..Stores::default()
        });
        let broken_owner = OutboxRuntime::new(
            Arc::clone(&broken_flock),
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
        )
        .unwrap();
        let broken_pool = Arc::clone(&broken_owner.pool);
        let handle = runtime.handle().clone();
        let broken_counts = runtime
            .block_on(async {
                tokio::task::spawn_blocking(move || {
                    let terminal: Mutex<Box<dyn Terminal>> =
                        Mutex::new(Box::new(FakeTerminal::new()));
                    let request = RequestCtx::default();
                    let adapter =
                        RequestRuntimeAdapter::new(&broken_flock, &request, &handle, &terminal);
                    let view =
                        InboxRuntimeView::new(adapter, Arc::clone(&broken_flock), broken_pool)
                            .unwrap();
                    broken_flock.inbox_count(&view, None, Some("%9")).unwrap()
                })
                .await
            })
            .unwrap();
        assert_eq!(broken_counts.total, 4);
        assert_eq!(broken_counts.unread, 4);
        assert_eq!(broken_counts.from_lanes, 0);
        owner.stop_and_join();
        broken_owner.stop_and_join();
    }
}
