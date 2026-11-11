//! Storage for the bounded owner-supervisor handoff. This module grants no authority;
//! the caller must authenticate the desk and resolve the fixed supervisor.

use super::{refuse, Store};
use crate::{
    clock::now_s, db::map_sqlite, error::DunkError, flock::mailbox::OWNER_MAILBOX,
    inbox::normalise_mailbox,
};
use rusqlite::{params, OptionalExtension, TransactionBehavior};

const SUPERVISOR: &str = "codex-rust-owner";

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OwnerHandoff {
    pub task_id: String,
    pub pane_id: String,
    pub notice_id: String,
    pub transport: String,
    pub notice_state: String,
    pub created_at: f64,
}

fn quarantine_notice_tx(
    tx: &rusqlite::Transaction<'_>,
    question_id: &str,
    task_id: &str,
    pane_id: &str,
) -> Result<Option<String>, DunkError> {
    let row: Option<(String, String)> = tx.query_row(
        "SELECT m.id, m.state FROM owner_handoffs h JOIN messages m ON m.id = h.notice_id WHERE h.question_id = ? AND h.task_id = ? AND h.pane_id = ? AND m.sender = 'owner-handoff'",
        params![question_id, task_id, pane_id], |r| Ok((r.get(0)?, r.get(1)?)),
    ).optional().map_err(map_sqlite)?;
    if let Some((id, state)) = &row {
        if matches!(state.as_str(), "queued" | "read") {
            tx.execute("UPDATE messages SET state = 'cancelled', cancelled_at = ? WHERE id = ? AND state IN ('queued', 'read')", params![now_s(), id]).map_err(map_sqlite)?;
        }
    }
    let prior = row.as_ref().map(|(_, state)| state.as_str());
    tx.execute("INSERT OR IGNORE INTO owner_handoff_attention (question_id, task_id, pane_id, prior_notice_state, created_at) VALUES (?, ?, ?, ?, ?)", params![question_id, task_id, pane_id, prior, now_s()]).map_err(map_sqlite)?;
    let recorded: (String, String) = tx
        .query_row(
            "SELECT task_id, pane_id FROM owner_handoff_attention WHERE question_id = ?",
            [question_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(map_sqlite)?;
    if recorded.0 != task_id || recorded.1 != pane_id {
        return Err(refuse(
            "handoff attention identity changed; owner review required",
        ));
    }
    Ok(row.map(|(_, state)| state))
}

impl Store {
    /// Quarantine a previously materialized notice when the task or target cannot be checked.
    /// None means there was no inbox mapping; the caller must still surface the failure.
    #[allow(dead_code)]
    pub(crate) fn quarantine_existing_owner_handoff(
        &self,
        question_id: &str,
    ) -> Result<Option<Option<String>>, DunkError> {
        let question_id = question_id.to_owned();
        self.writer.call(move |connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_sqlite)?;
            let mapping: Option<(String, String)> = tx
                .query_row(
                    "SELECT task_id, pane_id FROM owner_handoffs WHERE question_id = ?",
                    [&question_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(map_sqlite)?;
            let Some((task, pane)) = mapping else {
                tx.commit().map_err(map_sqlite)?;
                return Ok(None);
            };
            let state = quarantine_notice_tx(&tx, &question_id, &task, &pane)?;
            tx.commit().map_err(map_sqlite)?;
            Ok(Some(state))
        })
    }
    /// Validate before creating a task; the notice transaction must recheck afterwards.
    #[allow(dead_code)]
    pub(crate) fn validate_owner_handoff_question(
        &self,
        question_id: &str,
    ) -> Result<String, DunkError> {
        let question_id = question_id.to_owned();
        self.readers.with(move |connection| {
            type Question = (
                String,
                Option<String>,
                Option<String>,
                String,
                Option<String>,
            );
            let question: Option<Question> = connection
                .query_row(
                    "SELECT mailbox, reply_to, transport, state, sender FROM messages WHERE id = ?",
                    [&question_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
                )
                .optional()
                .map_err(map_sqlite)?;
            let (mailbox, parent, transport, state, sender) =
                question.ok_or_else(|| refuse("owner question does not exist"))?;
            if normalise_mailbox(&mailbox) != OWNER_MAILBOX
                || parent.as_deref().is_some_and(|v| !v.is_empty())
                || sender.as_deref().is_some_and(|v| !v.is_empty())
                || matches!(state.as_str(), "cancelled" | "archived")
            {
                return Err(refuse("handoff requires an active original owner question"));
            }
            let transport = transport
                .filter(|v| matches!(v.as_str(), "web" | "telegram"))
                .ok_or_else(|| refuse("handoff requires a web or telegram question"))?;
            let answered: bool = connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE reply_to = ?)",
                    [&question_id],
                    |r| r.get(0),
                )
                .map_err(map_sqlite)?;
            if answered {
                return Err(refuse(
                    "owner question already answered; reconcile handoff task",
                ));
            }
            Ok(transport)
        })
    }
    /// Retire an obsolete notice only once an answer is visible in the same database.
    #[allow(dead_code)]
    pub(crate) fn reconcile_answered_owner_notice(
        &self,
        question_id: &str,
    ) -> Result<bool, DunkError> {
        let question_id = question_id.to_owned();
        self.writer.call(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(map_sqlite)?;
            let answered: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE reply_to = ?)", [&question_id], |r| r.get(0)).map_err(map_sqlite)?;
            if answered {
                tx.execute("UPDATE messages SET state = 'cancelled', cancelled_at = ? WHERE id IN (SELECT notice_id FROM owner_handoffs WHERE question_id = ?) AND sender = 'owner-handoff' AND state IN ('queued', 'read')", params![now_s(), question_id]).map_err(map_sqlite)?;
            }
            tx.commit().map_err(map_sqlite)?;
            Ok(answered)
        })
    }

    /// Read unresolved compensation records in bounded lexical pages. A caller must
    /// surface them and explicitly resolve them; this function clears nothing.
    #[allow(dead_code)]
    pub(crate) fn owner_handoff_attention_page(
        &self,
        after: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, String, Option<String>)>, DunkError> {
        let after = after.to_owned();
        let limit = limit.clamp(1, 128) as i64;
        self.readers.with(move |connection| {
            let mut stmt = connection.prepare("SELECT question_id, task_id, pane_id, prior_notice_state FROM owner_handoff_attention WHERE question_id > ? ORDER BY question_id LIMIT ?").map_err(map_sqlite)?;
            let rows = stmt.query_map(params![after, limit], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).map_err(map_sqlite)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
        })
    }

    pub(crate) fn owner_handoff_attention(
        &self,
        question: &str,
    ) -> Result<Option<(String, String, Option<String>)>, DunkError> {
        let question = question.to_owned();
        self.readers.with(move |connection| {
            connection.query_row(
                "SELECT task_id, pane_id, prior_notice_state FROM owner_handoff_attention WHERE question_id = ?",
                [&question],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional().map_err(map_sqlite)
        })
    }

    /// Cancel an obsolete notice only when it still matches the exact task and pane.
    /// The caller must independently establish why the notice is no longer safe.
    #[allow(dead_code)]
    pub(crate) fn cancel_owner_handoff_notice(
        &self,
        question_id: &str,
        task_id: &str,
        pane_id: &str,
    ) -> Result<Option<String>, DunkError> {
        let (question_id, task_id, pane_id) = (
            question_id.to_owned(),
            task_id.to_owned(),
            pane_id.to_owned(),
        );
        self.writer.call(move |connection| {
            let tx = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_sqlite)?;
            let state = quarantine_notice_tx(&tx, &question_id, &task_id, &pane_id)?;
            tx.commit().map_err(map_sqlite)?;
            Ok(state)
        })
    }

    /// Atomically verify the original question remains unanswered and create an
    /// agent-only notice and stable mapping. The caller must verify the target.
    #[allow(dead_code)]
    pub(crate) fn ensure_owner_handoff_notice(
        &self,
        question_id: &str,
        pane_id: &str,
        task_id: &str,
    ) -> Result<OwnerHandoff, DunkError> {
        if question_id.trim().is_empty() || pane_id.trim().is_empty() || task_id.trim().is_empty() {
            return Err(refuse("question, verified pane and task are required"));
        }
        let (question_id, pane_id, task_id) = (
            question_id.to_owned(),
            pane_id.to_owned(),
            task_id.to_owned(),
        );
        self.writer.call(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(map_sqlite)?;
            let attention: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM owner_handoff_attention WHERE question_id = ?)", [&question_id], |r| r.get(0)).map_err(map_sqlite)?;
            if attention { return Err(refuse("handoff requires explicit recovery")); }
            type Question = (String, Option<String>, Option<String>, String, Option<String>);
            let question: Option<Question> = tx.query_row(
                "SELECT mailbox, reply_to, transport, state, sender FROM messages WHERE id = ?",
                [&question_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            ).optional().map_err(map_sqlite)?;
            let (mailbox, parent, transport, state, sender) = question.ok_or_else(|| refuse("owner question does not exist"))?;
            if normalise_mailbox(&mailbox) != OWNER_MAILBOX || parent.as_deref().is_some_and(|v| !v.is_empty()) || sender.as_deref().is_some_and(|v| !v.is_empty()) || matches!(state.as_str(), "cancelled" | "archived") {
                return Err(refuse("handoff requires an active original owner question"));
            }
            let transport = transport.filter(|v| matches!(v.as_str(), "web" | "telegram")).ok_or_else(|| refuse("handoff requires a web or telegram question"))?;
            let answered: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE reply_to = ?)", [&question_id], |r| r.get(0)).map_err(map_sqlite)?;
            if answered { return Err(refuse("owner question already answered; reconcile handoff task")); }
            let existing: Option<(String, String, String, f64)> = tx.query_row("SELECT task_id, pane_id, notice_id, created_at FROM owner_handoffs WHERE question_id = ?", [&question_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).optional().map_err(map_sqlite)?;
            let (notice_id, created_at) = if let Some((old_task, old_pane, notice, created)) = existing {
                if old_task != task_id || old_pane != pane_id { return Err(refuse("handoff target or task changed; recovery required")); }
                (notice, created)
            } else {
                let notice = super::next_message_id(&tx)?;
                let created = now_s();
                let subject = format!("TASK {task_id}: owner question");
                let body = format!("TASK {task_id}: inspect owner question {question_id}. If still unanswered, use owner reply for the final answer. If already answered, reconcile the task without sending a second answer.");
                tx.execute("INSERT INTO messages (id, mailbox, mailbox_label, pane_id, sender, subject, body, state, created_at) VALUES (?, ?, ?, ?, 'owner-handoff', ?, ?, 'queued', ?)", params![notice, SUPERVISOR, SUPERVISOR, pane_id, subject, body, created]).map_err(map_sqlite)?;
                tx.execute("INSERT INTO message_audiences (message_id, audience) VALUES (?, 'agent')", [&notice]).map_err(map_sqlite)?;
                tx.execute("INSERT INTO owner_handoffs (question_id, task_id, pane_id, notice_id, created_at) VALUES (?, ?, ?, ?, ?)", params![question_id, task_id, pane_id, notice, created]).map_err(map_sqlite)?;
                (notice, created)
            };
            let notice_state: String = tx.query_row("SELECT state FROM messages WHERE id = ? AND mailbox = ? AND pane_id = ?", params![notice_id, SUPERVISOR, pane_id], |r| r.get(0)).map_err(map_sqlite)?;
            if !matches!(notice_state.as_str(), "queued" | "read" | "acked") { return Err(refuse("terminal handoff notice requires recovery")); }
            tx.commit().map_err(map_sqlite)?;
            Ok(OwnerHandoff { task_id, pane_id, notice_id, transport, notice_state, created_at })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_store::Outgoing;

    #[test]
    fn owner_handoff_quarantine_contains_notice_and_blocks_recreation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("inbox.db"), &dir.path().join("blobs")).unwrap();
        let q = store
            .send(&Outgoing {
                mailbox: "owner",
                body: Some("question"),
                transport: Some("web"),
                ..Outgoing::default()
            })
            .unwrap();
        let notice = store.ensure_owner_handoff_notice(&q, "%1", "t1").unwrap();
        assert_eq!(
            store.quarantine_existing_owner_handoff(&q).unwrap(),
            Some(Some("queued".into()))
        );
        assert_eq!(
            store.get_message(&notice.notice_id).unwrap().summary.state,
            "cancelled"
        );
        assert_eq!(store.get_message(&q).unwrap().summary.state, "queued");
        assert!(store.ensure_owner_handoff_notice(&q, "%1", "t1").is_err());
        let attention = store.owner_handoff_attention_page("", 128).unwrap();
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].0, q);
        assert_eq!(attention[0].3.as_deref(), Some("queued"));
        assert_eq!(
            store.quarantine_existing_owner_handoff("missing").unwrap(),
            None
        );
    }

    #[test]
    fn notice_is_idempotent_agent_only_and_refuses_answered_questions() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("inbox.db"), &dir.path().join("blobs")).unwrap();
        for (transport, task) in [("web", "t1"), ("telegram", "t2")] {
            let q = store
                .send(&Outgoing {
                    mailbox: "owner",
                    body: Some("PRIVATE OWNER TEXT"),
                    transport: Some(transport),
                    ..Outgoing::default()
                })
                .unwrap();
            let a = store.ensure_owner_handoff_notice(&q, "%1", task).unwrap();
            assert_eq!(
                a,
                store.ensure_owner_handoff_notice(&q, "%1", task).unwrap()
            );
            assert_eq!(a.transport, transport);
            assert!(store.ensure_owner_handoff_notice(&q, "%2", task).is_err());
            let notice = store.get_message(&a.notice_id).unwrap();
            assert!(!notice.body.contains("PRIVATE OWNER TEXT"));
            let audience = store
                .readers
                .with(|connection| {
                    connection
                        .query_row(
                            "SELECT audience FROM message_audiences WHERE message_id = ?",
                            [&a.notice_id],
                            |row| row.get::<_, String>(0),
                        )
                        .map_err(map_sqlite)
                })
                .unwrap();
            assert_eq!(audience, "agent");
            store
                .send(&Outgoing {
                    mailbox: "owner",
                    body: Some("final"),
                    reply_to: Some(&q),
                    ..Outgoing::default()
                })
                .unwrap();
            assert!(store.ensure_owner_handoff_notice(&q, "%1", task).is_err());
            assert!(store.reconcile_answered_owner_notice(&q).unwrap());
            assert_eq!(
                store.get_message(&a.notice_id).unwrap().summary.state,
                "cancelled"
            );
        }
    }
}
