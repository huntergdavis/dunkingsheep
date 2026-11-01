//! `flock/commands_inbox.rs` (T34, slice 1): the read side of the inbox commands and the
//! mailbox default every one of them goes through. Ports `Flock._mailbox_exists`,
//! `_own_mailbox`, `_mailbox_or_self` (incl. the `own_only` lock), `inbox_count`,
//! `inbox_list`, `inbox_status`, `inbox_stats`, `inbox_recover`.
//!
//! Slice 2 adds `inbox_ack`/`archive`/`cancel`/`delete`/`fetch`; slice 3 adds `inbox_send` and
//! `inbox_read`. The outbox (`send_now`, the queued-notice fold), the PM routing file, the
//! tasks view and metrics have no Rust owner yet, so they sit behind [`InboxEnv`] hooks.
//! NOT ported: the `last_message` status string and the `log.info` line of `inbox_send`. `_own_mailbox`'s 30 s cache is not ported: it only saves the pane-list walk.

use crate::error::DunkError;
use crate::inbox::{self, Counts, MailboxStats, Status, Summary};
use std::path::{Path, PathBuf};

use crate::inbox_store::{Archived, Fetched, Message, Outgoing, Store, Transition};
use serde_json::Value;

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
    /// `dunk_pm.load_routing()`; an unreadable file is an empty routing (the send path
    /// swallows it: "routing must never block a send").
    fn pm_routing(&self) -> Vec<RouteEntry>;
    /// `_refresh_queued_notice` + folding: when a notice for `pane_id` is still waiting,
    /// update it to `text`, attach `message_id` to it and return its id.
    fn fold_into_queued_notice(
        &self,
        pane_id: &str,
        text: &str,
        message_id: &str,
    ) -> Option<String>;
    /// `send_now(pane_id, text, wait_s=0, notify_for=message_id)`.
    fn send_now(&self, pane_id: &str, text: &str, message_id: &str) -> Result<Nudge, DunkError>;
    /// `self.metrics.record("inbox.sent", ..., bytes=.., attempts=..)`.
    fn record_sent(
        &self,
        pane_id: Option<&str>,
        mailbox: &str,
        sender: Option<&str>,
        bytes: usize,
        attachments: usize,
    );
    /// `self.tasks.for_pane(pane)`, already rendered as the wire's JSON.
    fn tasks_for_pane(&self, pane_id: &str) -> Value;
}

/// One project in the PM routing file (`dunk_pm.load_routing`).
#[derive(Debug, Clone, PartialEq)]
pub struct RouteEntry {
    pub project: String,
    pub pm: String,
    pub members: Vec<String>,
    /// `redirect` or `flag`.
    pub mode: String,
    pub vp_pane: String,
}

/// What `send_now` answered.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Nudge {
    pub queued: bool,
    pub message: Option<String>,
}

/// `SYSTEM_SENDERS`.
pub const SYSTEM_SENDERS: [&str; 1] = ["watchdog"];

/// `NOTICE_PREFIX`.
pub const NOTICE_PREFIX: &str = "[inbox] ";

/// `_notice_text`: the one short line typed into a pane to say mail has arrived. It names the
/// inbox, says who it is from and what it is about, and stays short on purpose: a short send
/// is the one that always submits.
pub fn notice_text(
    mailbox: &str,
    unread: i64,
    sender: Option<&str>,
    subject: Option<&str>,
) -> String {
    let mut head = format!("{NOTICE_PREFIX}new message for {mailbox}");
    if unread > 1 {
        head.push_str(&format!(" ({unread} unread)"));
    }
    if let Some(sender) = sender.filter(|s| !s.is_empty()) {
        head.push_str(&format!(" from {sender}"));
    }
    if let Some(subject) = subject.filter(|s| !s.is_empty()) {
        let trimmed: String = subject.trim().chars().take(60).collect();
        head.push_str(&format!(": {trimmed}"));
    }
    format!("{head}. Read it with: dunkingsheep inbox read")
}

/// `inbox_send`'s arguments.
#[derive(Debug, Clone, Default)]
pub struct SendArgs<'a> {
    pub to: &'a str,
    pub body: &'a str,
    pub subject: Option<&'a str>,
    pub attach: &'a [PathBuf],
    pub notify: bool,
    pub self_pane_id: Option<&'a str>,
    pub system_sender: Option<&'a str>,
}

/// What `inbox_send` returns: the stored message's id plus the fields the Python adds.
#[derive(Debug, Clone, PartialEq)]
pub struct SendResult {
    pub message_id: String,
    pub mailbox: String,
    pub label: String,
    pub pane_id: Option<String>,
    pub sender: Option<String>,
    /// `{"from": "vp", "to": pm, "project": ..}` or `{"flag": project}`.
    pub redirected: Option<Value>,
    pub notified: bool,
    /// `Some("undeliverable")` with `reason` when a system sender's send was refused.
    pub outcome: Option<String>,
    pub reason: Option<String>,
}

/// `inbox_read`'s result.
#[derive(Debug, Clone, PartialEq)]
pub struct ReadResult {
    pub mailbox: String,
    pub messages: Vec<Message>,
    /// `Some` only when a caller pane is known.
    pub tasks: Option<Value>,
    /// `Some` only when the batch was marked read (not peek, not empty).
    pub marked_read: Option<usize>,
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

impl Flock {
    /// `inbox_send`: put a message in another agent's inbox. Nothing is typed into their
    /// terminal except, when `notify`, a short line saying they have mail.
    ///
    /// `system_sender` names a non-agent sender (the watchdog). It is accepted only from
    /// SYSTEM_SENDERS and only from a caller with no pane (a systemd unit); an agent always
    /// runs in a pane, so it cannot claim the name.
    pub fn inbox_send(&self, env: &dyn InboxEnv, args: &SendArgs) -> Result<SendResult, DunkError> {
        let self_pane = args.self_pane_id.filter(|p| !p.is_empty());
        if let Some(system) = args.system_sender {
            if !SYSTEM_SENDERS.contains(&system) {
                return Err(refused(format!(
                    "unknown system sender {}",
                    py_repr(system)
                )));
            }
            // Over the socket a system sender must hold the system token; in-process
            // callers (the daemon's own loops, tests) are trusted code.
            if self.socket_peer_ok().is_some() {
                let role = self_pane
                    .map(|p| env.caller_role(p))
                    .unwrap_or_else(|| "anonymous".to_string());
                if !matches!(role.as_str(), "system" | "owner") {
                    return Err(refused(format!(
                        "system sender {} requires the system token; a pane-less process \
                         without one is anonymous (FLAT_FLEET.md 4.1)",
                        py_repr(system)
                    )));
                }
            }
            if self_pane.is_some() {
                return Err(refused(
                    "a system sender cannot be claimed from a pane".to_string(),
                ));
            }
        }
        let mut undeliverable_reason = None;
        let (mut mailbox, mut label, mut pane_id) =
            match self.resolve_mailbox(env, args.to, self_pane, true, "") {
                Ok(found) => found,
                Err(refusal) => {
                    // A SYSTEM SENDER MUST NEVER BE KILLED BY A FAIL-CLOSED REFUSAL. The
                    // watchdog escalates to a lane that may have been handed over minutes
                    // ago; raising here took the exception watchdog down twice on
                    // 2026-10-24 and the fleet ran blind for nine minutes. The message is
                    // STORED under the name it was addressed to, marked undeliverable with
                    // the refusal as its reason. Agents still get the hard refusal.
                    if args.system_sender.is_none() {
                        return Err(refusal);
                    }
                    undeliverable_reason = Some(refusal.to_string());
                    let name = args.to.trim().to_string();
                    (name.clone(), name, None)
                }
            };
        // The sender is the raw pane id, never a mailbox label. On 2026-10-19 wS:p1's mail
        // arrived "from codex poolrad" because the label resolved through a sibling pane.
        let sender: Option<String> = args
            .system_sender
            .map(str::to_string)
            .or_else(|| self_pane.map(str::to_string));
        // PM routing: a lane that messages the VP out of habit is redirected to its project
        // PM (or flagged). Routing must never block a send.
        let mut body = args.body.to_string();
        let mut subject = args.subject.map(str::to_string);
        let mut redirected = None;
        let routing = env.pm_routing();
        let project = routing.iter().find(|entry| {
            pane_id.is_some()
                && pane_id.as_deref() == Some(entry.vp_pane.as_str())
                && self_pane.is_some_and(|p| entry.members.iter().any(|m| m == p))
        });
        if let (Some(entry), Some(me)) = (project, self_pane) {
            // `dunk_pm.redirect`: the PM is never redirected to itself.
            if me != entry.pm {
                if entry.mode == "redirect" {
                    if let Ok(found) = self.resolve_mailbox(env, &entry.pm, Some(me), false, "") {
                        (mailbox, label, pane_id) = found;
                        body = format!(
                            "[redirected from the VP to the {} PM]\n\n{body}",
                            entry.project
                        );
                        redirected = Some(serde_json::json!({
                            "from": "vp", "to": entry.pm, "project": entry.project}));
                    }
                } else {
                    subject = Some(
                        format!(
                            "[lane mail to VP: {}] {}",
                            entry.project,
                            subject.as_deref().unwrap_or("")
                        )
                        .trim()
                        .to_string(),
                    );
                    redirected = Some(serde_json::json!({"flag": entry.project}));
                }
            }
        }
        let message_id = env.store().send(&Outgoing {
            mailbox: &mailbox,
            body: Some(&body),
            subject: subject.as_deref(),
            sender: sender.as_deref(),
            mailbox_label: Some(&label),
            pane_id: pane_id.as_deref(),
            attachments: args.attach,
            ..Default::default()
        })?;
        let mut notified = false;
        if args.notify {
            if let Some(pane) = pane_id.as_deref() {
                let unread = inbox::count(env.pool(), &mailbox, &[], env.now())?.unread;
                let text = notice_text(&mailbox, unread, sender.as_deref(), subject.as_deref());
                // A notice for this pane may already be waiting. Say the new count rather
                // than stacking a second one: an agent that reads its inbox once has
                // handled both messages.
                if let Some(existing) = env.fold_into_queued_notice(pane, &text, &message_id) {
                    env.store().record_notify(
                        &message_id,
                        "queued",
                        Some(&format!("folded into {existing}")),
                        None,
                        None,
                    )?;
                    notified = true;
                } else {
                    match env.send_now(pane, &text, &message_id) {
                        Ok(nudge) => {
                            env.store().record_notify(
                                &message_id,
                                if nudge.queued { "queued" } else { "sent" },
                                nudge.message.as_deref(),
                                None,
                                None,
                            )?;
                            notified = true;
                        }
                        Err(error) => {
                            env.store().record_notify(
                                &message_id,
                                "failed",
                                Some(&error.to_string()),
                                None,
                                None,
                            )?;
                        }
                    }
                }
            }
        }
        env.record_sent(
            pane_id.as_deref(),
            &mailbox,
            sender.as_deref(),
            body.len(),
            args.attach.len(),
        );
        Ok(SendResult {
            message_id,
            mailbox,
            label,
            pane_id,
            sender,
            redirected,
            notified,
            outcome: undeliverable_reason
                .as_ref()
                .map(|_| "undeliverable".to_string()),
            reason: undeliverable_reason,
        })
    }

    /// `inbox_read`: read a batch WITHOUT consuming it, build the whole result, then commit.
    ///
    /// The order is the point (VP, t93): marking read first meant a reader whose rendering
    /// or task lookup failed had already consumed the batch, and pi-review-1 issued a verdict
    /// on a head whose withdrawal message it never saw. So: (1) fetch without marking, (2)
    /// build the FULL result, including the caller's task queues, (3) only then mark read.
    /// A failure in 1 or 2 leaves the batch unread, which is the safe direction.
    pub fn inbox_read(
        &self,
        env: &dyn InboxEnv,
        mailbox: Option<&str>,
        limit: usize,
        peek: bool,
        self_pane_id: Option<&str>,
    ) -> Result<ReadResult, DunkError> {
        let key = self.mailbox_or_self(env, mailbox, self_pane_id, false, true)?;
        let mut messages = env.store().read(&key, limit, true, false)?;
        // The owner's directive: mail and the reader's task queues travel together, so one
        // inbox read is the whole handoff. This runs BEFORE the mark.
        let tasks = self_pane_id
            .filter(|p| !p.is_empty())
            .map(|p| env.tasks_for_pane(p));
        if peek || messages.is_empty() {
            return Ok(ReadResult {
                mailbox: key,
                messages,
                tasks,
                marked_read: None,
            });
        }
        let ids: Vec<String> = messages
            .iter()
            .map(|m| m.summary.message_id.clone())
            .collect();
        let marked = env.store().mark_read(&ids, None)?;
        // t128: the mark commits BEFORE the socket response reaches the client, so a
        // response lost after this point used to silently consume the batch. The receipt
        // bounds recovery to the latest delivered batch per mailbox.
        env.store().record_delivery(&key, &ids, None)?;
        let now = env.now();
        for message in &mut messages {
            message.summary.state = "read".to_string();
            message.read_at = Some(now);
            env.record_metric(
                "inbox.read",
                None,
                &key,
                message.summary.sender.as_deref(),
                Some(((now - message.summary.created_at) * 1000.0) as i64),
            );
        }
        Ok(ReadResult {
            mailbox: key,
            messages,
            tasks,
            marked_read: Some(marked),
        })
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
