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
        flock::Flock,
        roles::Roles,
        server::RequestCtx,
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
}
