//! `flock/commands_inbox.rs` (T34, slice 1): the read side of the inbox commands and the
//! mailbox default every one of them goes through. Ports `Flock._mailbox_exists`,
//! `_own_mailbox`, `_mailbox_or_self` (incl. the `own_only` lock), `inbox_count`,
//! `inbox_list`, `inbox_status`, `inbox_stats`, `inbox_recover`.
//!
//! Slice 2 adds `inbox_ack`/`archive`/`cancel`/`delete`/`fetch`.
//!
//! NOT in this slice (tracked on t802): `inbox_send` (PM redirect, system_sender gate,
//! notice folding: needs the outbox and `send_now`) and `inbox_read` (needs the tasks view). `_own_mailbox`'s 30 s cache is not ported: it only saves the pane-list walk.

use crate::error::DunkError;
use crate::inbox::{self, Counts, MailboxStats, Status, Summary};
use std::path::Path;

use crate::inbox_store::{Archived, Fetched, Message, Store, Transition};

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
    /// `self.metrics.record(kind, pane_id=.., mailbox=.., sender=.., source="inbox",
    /// outcome="ok", duration_ms=..)`. Metrics have no Rust owner yet, so the daemon wiring
    /// supplies this.
    fn record_metric(
        &self,
        kind: &str,
        pane_id: Option<&str>,
        mailbox: &str,
        sender: Option<&str>,
        duration_ms: Option<i64>,
    );
}

/// `inbox_ack`'s result: `_transition`'s fields plus `acked_in` and `still_pending`.
#[derive(Debug, Clone, PartialEq)]
pub struct AckResult {
    pub transition: Transition,
    pub acked_in: Option<String>,
    /// `Some` only when a mailbox scoped the ack (t128: what a recovered batch still holds).
    pub still_pending: Option<Vec<String>>,
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

impl Flock {
    /// `inbox_ack`: ack only messages in the caller's own mailbox. Sender is irrelevant; an
    /// id from another mailbox is refused, never marked read there. The no-mailbox,
    /// no-caller call (tests, in-process) keeps the old unscoped behaviour.
    pub fn inbox_ack(
        &self,
        env: &dyn InboxEnv,
        message_ids: &[String],
        mailbox: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<AckResult, DunkError> {
        let scoped =
            mailbox.is_some_and(|m| !m.is_empty()) || self_pane_id.is_some_and(|p| !p.is_empty());
        let key = if scoped {
            Some(self.mailbox_or_self(env, mailbox, self_pane_id, false, true)?)
        } else {
            None
        };
        let mut before = std::collections::BTreeMap::new();
        for id in message_ids {
            if let Ok(status) = inbox::status(env.pool(), id, env.now()) {
                before.insert(id.clone(), status);
            }
        }
        let transition = env.store().ack(message_ids, key.as_deref())?;
        // t128: shrink the delivery receipt to what is still unacked, so a recovered batch
        // stops replaying once it has been displayed+acked.
        let still_pending = match &key {
            Some(key) => Some(env.store().note_acked(key)?),
            None => None,
        };
        for id in &transition.changed {
            let Some(was) = before.get(id) else { continue };
            let started = was.read_at.unwrap_or(was.created_at);
            let after = inbox::status(env.pool(), id, env.now())
                .ok()
                .and_then(|s| s.read_at);
            let duration = after.map(|read| ((read - started) * 1000.0) as i64);
            env.record_metric(
                "inbox.acked",
                was.pane_id.as_deref(),
                key.as_deref().unwrap_or(""),
                was.sender.as_deref(),
                duration,
            );
        }
        Ok(AckResult {
            transition,
            acked_in: key,
            still_pending,
        })
    }

    /// `inbox_cancel`.
    pub fn inbox_cancel(
        &self,
        env: &dyn InboxEnv,
        message_ids: &[String],
    ) -> Result<Transition, DunkError> {
        env.store().cancel(message_ids)
    }

    /// `inbox_delete`.
    pub fn inbox_delete(
        &self,
        env: &dyn InboxEnv,
        message_ids: &[String],
    ) -> Result<Vec<String>, DunkError> {
        env.store().delete(message_ids)
    }

    /// `inbox_archive`: bulk-retire a mailbox (mark read and archived, never delete).
    pub fn inbox_archive(
        &self,
        env: &dyn InboxEnv,
        mailbox: &str,
        before: Option<f64>,
        states: Option<&[String]>,
    ) -> Result<Archived, DunkError> {
        env.store().archive(mailbox, before, states)
    }

    /// `inbox_fetch`: `dest` defaults to the caller's working directory.
    pub fn inbox_fetch(
        &self,
        env: &dyn InboxEnv,
        attachment_id: &str,
        dest: Option<&Path>,
    ) -> Result<Fetched, DunkError> {
        let cwd;
        let dest = match dest.filter(|d| !d.as_os_str().is_empty()) {
            Some(dest) => dest,
            None => {
                cwd = std::env::current_dir()
                    .map_err(|e| DunkError::Refused(format!("no working directory: {e}")))?;
                &cwd
            }
        };
        env.store().fetch(attachment_id, dest)
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
