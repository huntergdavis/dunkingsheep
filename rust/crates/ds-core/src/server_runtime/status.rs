//! Complete request-thread status sources. CommandEnv checks startup state before exposing it.
use super::RequestRuntimeAdapter;
use crate::api::CommandEnv;
use crate::error::DunkError;
use crate::flock::{dunks::DunkList, status::StatusEnv, status_runtime::StatusState};
use serde_json::{json, Map, Value};

#[allow(dead_code)] // Final production dispatcher construction is gated on t1605.
impl RequestRuntimeAdapter<'_> {
    fn status_state(&self) -> &StatusState {
        self.terminal
            .flock
            .status_state()
            .expect("CommandEnv::status_env must verify startup configuration")
    }
    fn helper_statusenv_version(&self) -> String {
        self.status_state().version.clone()
    }
    fn helper_statusenv_federated_input(&self) -> Value {
        json!(self.status_state().federated_input)
    }
    fn helper_statusenv_code_status(&self) -> Map<String, Value> {
        self.status_state().code.code_status()
    }
    fn helper_statusenv_dunks(&self) -> &DunkList {
        &self.terminal.flock.dunks
    }
    fn helper_statusenv_started_at(&self) -> f64 {
        self.status_state().started_at
    }
    fn helper_statusenv_pid(&self) -> u32 {
        self.status_state().pid
    }
    fn helper_statusenv_queued_messages(&self) -> usize {
        self.terminal
            .flock
            .outbox
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .values()
            .map(Vec::len)
            .sum()
    }
    fn helper_statusenv_inbox_unread(&self) -> Result<i64, DunkError> {
        let store = self
            .terminal
            .flock
            .stores()
            .inbox
            .ok_or_else(|| DunkError::Refused("inbox store is not wired".into()))?;
        Ok(
            crate::inbox::stats(store.read_pool(), CommandEnv::now_s(self))?
                .iter()
                .map(|row| row.unread)
                .sum(),
        )
    }
    fn helper_statusenv_task_totals(&self) -> Result<Value, DunkError> {
        let store = self
            .terminal
            .flock
            .stores()
            .tasks
            .ok_or_else(|| DunkError::Refused("task store is not wired".into()))?;
        let stats = self.handler_context().block_on(store.stats(None, None))??;
        Ok(stats["totals"].clone())
    }
    fn helper_statusenv_task_followers(&self) -> usize {
        self.terminal
            .flock
            .task_waiters
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }
    fn helper_statusenv_quarantined_mailboxes(&self) -> Option<Value> {
        self.terminal.flock.quarantined_mailboxes()
    }
    fn helper_statusenv_last_message(&self) -> String {
        self.terminal.flock.last_message()
    }
    fn helper_statusenv_herdr_available(&self) -> bool {
        self.terminal
            .with_terminal(|terminal| terminal.is_available())
    }
    fn helper_statusenv_herdr_hint(&self) -> String {
        self.terminal
            .with_terminal(|terminal| terminal.server_error_hint())
    }
    fn helper_statusenv_state_file(&self) -> Option<String> {
        self.terminal.flock.persist_enabled.then(|| {
            self.status_state()
                .state_file
                .to_string_lossy()
                .into_owned()
        })
    }
    fn helper_statusenv_inbox_count(&self, pane: &str) -> Result<Value, String> {
        self.status_inbox_count(pane)
            .map_err(|error| error.to_string())
    }
    fn status_inbox_count(&self, pane: &str) -> Result<Value, DunkError> {
        if pane.is_empty() {
            return Err(DunkError::Refused(
                "no mailbox given and no caller pane to default to".into(),
            ));
        }
        let store = self
            .terminal
            .flock
            .stores()
            .inbox
            .ok_or_else(|| DunkError::Refused("inbox store is not wired".into()))?;
        let env = &self.status_state().env;
        let mailbox = self.terminal.flock.own_mailbox_from_sources(
            &self.terminal,
            || store.read_pool(),
            pane,
            || {
                env.get("DUNKINGSHEEP_MAILBOX")
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            },
            || super::normalize_backend_name(env.get("DUNKINGSHEEP_TERMINAL_BACKEND")),
        )?;
        let mut lanes: Vec<String> = crate::pm::load_routing(None, None, env)
            .unwrap_or_default()
            .into_iter()
            .flat_map(|(_, entry)| entry.members)
            .collect();
        lanes.sort();
        lanes.dedup();
        let c = crate::inbox::count(store.read_pool(), &mailbox, &lanes, CommandEnv::now_s(self))?;
        Ok(
            json!({"mailbox":c.mailbox,"unread":c.unread,"total":c.total,"archived":c.archived,
            "undeliverable":c.undeliverable,"oldest_unread_s":c.oldest_unread_s,
            "attachments_unread":c.attachments_unread,"from_lanes":c.from_lanes}),
        )
    }
}

impl StatusEnv for RequestRuntimeAdapter<'_> {
    fn version(&self) -> String {
        self.helper_statusenv_version()
    }
    fn federated_input(&self) -> Value {
        self.helper_statusenv_federated_input()
    }
    fn code_status(&self) -> Map<String, Value> {
        self.helper_statusenv_code_status()
    }
    fn dunks(&self) -> &DunkList {
        self.helper_statusenv_dunks()
    }
    fn started_at(&self) -> f64 {
        self.helper_statusenv_started_at()
    }
    fn pid(&self) -> u32 {
        self.helper_statusenv_pid()
    }
    fn now(&self) -> f64 {
        CommandEnv::now_s(self)
    }
    fn queued_messages(&self) -> usize {
        self.helper_statusenv_queued_messages()
    }
    fn inbox_unread(&self) -> Result<i64, DunkError> {
        self.helper_statusenv_inbox_unread()
    }
    fn task_totals(&self) -> Result<Value, DunkError> {
        self.helper_statusenv_task_totals()
    }
    fn task_followers(&self) -> usize {
        self.helper_statusenv_task_followers()
    }
    fn quarantined_mailboxes(&self) -> Option<Value> {
        self.helper_statusenv_quarantined_mailboxes()
    }
    fn last_message(&self) -> String {
        self.helper_statusenv_last_message()
    }
    fn herdr_available(&self) -> bool {
        self.helper_statusenv_herdr_available()
    }
    fn herdr_hint(&self) -> String {
        self.helper_statusenv_herdr_hint()
    }
    fn state_file(&self) -> Option<String> {
        self.helper_statusenv_state_file()
    }
    fn inbox_count(&self, pane: &str) -> Result<Value, String> {
        self.helper_statusenv_inbox_count(pane)
    }
}
#[cfg(test)]
mod tests;
