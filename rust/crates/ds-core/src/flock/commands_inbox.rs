//! `flock/commands_inbox.rs` (T34, slice 1): the read side of the inbox commands and the
//! mailbox default every one of them goes through. Ports `Flock._mailbox_exists`,
//! `_own_mailbox`, `_mailbox_or_self` (incl. the `own_only` lock), `inbox_count`,
//! `inbox_list`, `inbox_status`, `inbox_stats`, `inbox_recover`.
//!
//! NOT in this slice (tracked on t802): `inbox_send` (PM redirect, system_sender gate,
//! notice folding: needs the outbox and `send_now`), `inbox_read` (needs the tasks view and
//! metrics), `inbox_ack`/`archive`/`cancel`/`delete`/`fetch` (the store has no write side
//! for them yet). `_own_mailbox`'s 30 s cache is not ported: it only saves the pane-list walk.

use crate::error::DunkError;
use crate::inbox::{self, Counts, MailboxStats, Status, Summary};
use crate::inbox_store::{Message, Store};

use super::mailbox::MailboxEnv;
use super::Flock;

/// What the inbox commands need from outside the lane registry and the pure resolver.
pub trait InboxEnv: MailboxEnv {
    /// `caller_role(pane)`.
    fn caller_role(&self, pane_id: &str) -> String;
    /// `DUNKINGSHEEP_MAILBOX`, trimmed; `None` when unset or blank.
    fn mailbox_override(&self) -> Option<String>;
    /// The backend's name, for the `_own_mailbox` refusal text.
    fn backend_name(&self) -> String;
    /// `dunk_pm.load_routing()` members, for `inbox_count`'s `lane_panes`.
    fn lane_panes(&self) -> Vec<String>;
    fn pool(&self) -> &crate::db::ReadPool;
    fn store(&self) -> &Store;
    fn now(&self) -> f64;
}

fn refused(text: String) -> DunkError {
    DunkError::Refused(text)
}

impl Flock {
    /// `_mailbox_exists`: true when the inbox already holds mail addressed to this name.
    /// A failing query is a `false`: a fallback must not raise.
    pub fn mailbox_exists(&self, env: &dyn InboxEnv, name: &str) -> bool {
        let (clause, params) = inbox::mailbox_where(name, "mailbox");
        let sql = format!("SELECT COUNT(*) FROM messages WHERE {clause}");
        env.pool()
            .with(|connection| {
                connection
                    .query_row(&sql, rusqlite::params_from_iter(params.iter()), |row| {
                        row.get::<_, i64>(0)
                    })
                    .map_err(|e| DunkError::Refused(e.to_string()))
            })
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// `_own_mailbox`: the caller's mailbox key. A lane's mailbox must not depend on the
    /// backend's pane ids, so a failed resolution falls back to `DUNKINGSHEEP_MAILBOX`, the
    /// lane registry, then a mailbox already named after the pane id.
    pub fn own_mailbox(&self, env: &dyn InboxEnv, self_pane_id: &str) -> Result<String, DunkError> {
        let mut resolved = false;
        let key = match self.resolve_mailbox(env, "self", Some(self_pane_id), false, "") {
            Ok((key, _, _)) => {
                resolved = true;
                key
            }
            Err(_) => {
                let mut key = env.mailbox_override();
                if key.is_none() {
                    key = self
                        .lane_registry(false)
                        .ok()
                        .and_then(|registry| registry.get(self_pane_id).cloned())
                        .and_then(|row| Flock::row_identity(Some(&row)).0);
                }
                if key.is_none() && self.mailbox_exists(env, self_pane_id) {
                    key = Some(self_pane_id.to_string());
                }
                key.ok_or_else(|| {
                    refused(format!(
                        "cannot resolve this lane's mailbox: {self_pane_id:?} is not a pane \
                         on the current backend ({}) and no mailbox is named after it; set \
                         DUNKINGSHEEP_MAILBOX to the lane's mailbox name",
                        env.backend_name()
                    ))
                })?
            }
        };
        // Only a REAL resolution is worth remembering: recording a fallback would poison the
        // registry with 'pane id -> pane id' and lose the lane's name.
        if resolved && Flock::plain_lane_name(&key) {
            self.remember_lane(self_pane_id, &key, false);
        }
        Ok(key)
    }

    /// `_mailbox_or_self`. Reading another lane's MAIL needs the owner role; counting it
    /// does not (`own_only`).
    pub fn mailbox_or_self(
        &self,
        env: &dyn InboxEnv,
        spec: Option<&str>,
        self_pane_id: Option<&str>,
        strict: bool,
        own_only: bool,
    ) -> Result<String, DunkError> {
        if let Some(spec) = spec.filter(|s| !s.is_empty()) {
            let resolved = self.resolve_mailbox(env, spec, self_pane_id, strict, "")?.0;
            if let (true, Some(pane)) = (own_only, self_pane_id.filter(|p| !p.is_empty())) {
                let role = env.caller_role(pane);
                if !matches!(role.as_str(), "owner" | "web" | "system") {
                    let own = self.own_mailbox(env, pane)?;
                    if resolved != own {
                        return Err(refused(format!(
                            "mailbox {} is not yours ({own}); reading another lane's mail \
                             needs the owner role",
                            py_repr(spec)
                        )));
                    }
                }
            }
            return Ok(resolved);
        }
        match self_pane_id.filter(|p| !p.is_empty()) {
            Some(pane) => self.own_mailbox(env, pane),
            None => Err(refused(
                "no mailbox given and no caller pane to default to".to_string(),
            )),
        }
    }

    /// `inbox_count`.
    pub fn inbox_count(
        &self,
        env: &dyn InboxEnv,
        mailbox: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<Counts, DunkError> {
        let key = self.mailbox_or_self(env, mailbox, self_pane_id, false, false)?;
        let mut lane_panes = env.lane_panes();
        lane_panes.sort();
        lane_panes.dedup();
        inbox::count(env.pool(), &key, &lane_panes, env.now())
    }

    /// `inbox_list`: `(mailbox, summaries)`.
    pub fn inbox_list(
        &self,
        env: &dyn InboxEnv,
        mailbox: Option<&str>,
        unread_only: bool,
        limit: usize,
        self_pane_id: Option<&str>,
    ) -> Result<(String, Vec<Summary>), DunkError> {
        let key = self.mailbox_or_self(env, mailbox, self_pane_id, false, true)?;
        let rows = inbox::list(env.pool(), &key, unread_only, limit, env.now())?;
        Ok((key, rows))
    }

    /// `inbox_status`.
    pub fn inbox_status(&self, env: &dyn InboxEnv, message_id: &str) -> Result<Status, DunkError> {
        inbox::status(env.pool(), message_id, env.now())
    }

    /// `inbox_stats`: the `mailboxes` list.
    pub fn inbox_stats(&self, env: &dyn InboxEnv) -> Result<Vec<MailboxStats>, DunkError> {
        inbox::stats(env.pool(), env.now())
    }

    /// `inbox_recover`: replay the latest delivered batch for display after a lost response.
    pub fn inbox_recover(
        &self,
        env: &dyn InboxEnv,
        mailbox: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<(String, Vec<Message>), DunkError> {
        let key = self.mailbox_or_self(env, mailbox, self_pane_id, false, false)?;
        let messages = env.store().recover_delivery(&key)?;
        Ok((key, messages))
    }
}

/// Python's `repr()` of a str, as the refusal text uses `{spec!r}`.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    format!("{quote}{s}{quote}")
}
