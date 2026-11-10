//! Idempotent task materialisation for a separately authenticated owner delegation.
//!
//! The caller must revalidate the live supervisor identity and reservation before and after
//! materialisation. This module provides no authority to select a different assignee.

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use super::Tasks;
use crate::{db, error::DunkError};

impl Tasks {
    /// Retire a superseded, unclaimed delegation task. The caller must first establish
    /// that this generation is no longer active; an in-progress task needs owner review.
    #[allow(dead_code)] // Recovery is not exposed until identity and wake fencing exist.
    pub(crate) async fn retire_stale_owner_delegation_task(
        &self,
        question_id: &str,
        generation: i64,
        incarnation: &str,
    ) -> Result<(), DunkError> {
        let (question_id, incarnation) = (question_id.to_owned(), incarnation.to_owned());
        let changed = self.writer.call(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db::map_sqlite)?;
            let known: Option<String> = tx.query_row("SELECT incarnation FROM owner_delegation_tasks WHERE question_id = ? AND generation = ?", params![question_id, generation], |row| row.get(0)).optional().map_err(db::map_sqlite)?;
            if known.as_deref().is_some_and(|v| v != incarnation) { return Err(DunkError::Refused("delegation task identity changed".into())); }
            tx.execute("INSERT OR IGNORE INTO owner_delegation_retired (question_id, generation, incarnation) VALUES (?, ?, ?)", params![question_id, generation, incarnation]).map_err(db::map_sqlite)?;
            let retired_incarnation: String = tx.query_row("SELECT incarnation FROM owner_delegation_retired WHERE question_id = ? AND generation = ?", params![question_id, generation], |row| row.get(0)).map_err(db::map_sqlite)?;
            if retired_incarnation != incarnation { return Err(DunkError::Refused("retired delegation identity changed".into())); }
            let task: Option<(String, String)> = tx.query_row(
                "SELECT t.id, t.status FROM owner_delegation_tasks d JOIN tasks t ON t.id = d.task_id WHERE d.question_id = ? AND d.generation = ? AND d.incarnation = ?",
                params![question_id, generation, incarnation], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional().map_err(db::map_sqlite)?;
            let Some((id, status)) = task else {
                if known.is_some() { return Err(DunkError::Refused("delegation journal references a missing task".into())); }
                tx.commit().map_err(db::map_sqlite)?;
                return Ok(None);
            };
            if status == "cancelled" { tx.commit().map_err(db::map_sqlite)?; return Ok(None); }
            let leased: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM task_leases WHERE task_id = ?)", [&id], |row| row.get(0)).map_err(db::map_sqlite)?;
            if status != "open" || leased { return Err(DunkError::Refused("delegation task is active or terminal; owner review required".into())); }
            let now = crate::clock::now_s();
            tx.execute("UPDATE tasks SET status = 'cancelled', cancelled_at = ?, updated_at = ? WHERE id = ?", params![now, now, id]).map_err(db::map_sqlite)?;
            tx.execute("INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_from, status_to) VALUES (?, 'cancelled', ?, 'owner-delegation-recovery', 'global', 'high', 'open', 'cancelled')", params![id, now]).map_err(db::map_sqlite)?;
            tx.commit().map_err(db::map_sqlite)?;
            Ok(Some(id))
        }).await?;
        if let Some(id) = changed {
            self.fire(&id, "cancelled", None, None);
        }
        Ok(())
    }

    /// Materialise a reserved task and its durable notice. Authentication, supervisor
    /// revalidation, orphan cleanup and notification wakeup are required of the caller.
    /// This helper is intentionally not exposed by an API until those pieces exist.
    #[allow(dead_code)]
    pub(crate) async fn materialise_owner_delegation(
        &self,
        inbox: std::sync::Arc<crate::inbox_store::Store>,
        proof: crate::inbox_store::owner_reservation::Reservation,
        pane: &str,
    ) -> Result<(String, String), DunkError> {
        let task = self
            .ensure_owner_delegation_task(
                &proof.question_id,
                proof.generation,
                &proof.incarnation,
                pane,
            )
            .await?;
        let task_for_notice = task.clone();
        let pane = pane.to_owned();
        let notice = tokio::task::spawn_blocking(move || {
            inbox.ensure_owner_delegation_notice(&proof, &pane, &task_for_notice)
        })
        .await
        .map_err(|error| {
            DunkError::Refused(format!("delegation notice worker failed: {error}"))
        })??;
        Ok((task, notice))
    }

    #[allow(dead_code)] // The delegation recovery worker is not yet wired.
    pub(crate) async fn ensure_owner_delegation_task(
        &self,
        question_id: &str,
        generation: i64,
        incarnation: &str,
        pane: &str,
    ) -> Result<String, DunkError> {
        if question_id.trim().is_empty()
            || generation < 1
            || incarnation.trim().is_empty()
            || pane.trim().is_empty()
        {
            return Err(DunkError::Refused(
                "complete delegation identity is required".into(),
            ));
        }
        let (question_id, incarnation, pane) = (
            question_id.to_owned(),
            incarnation.to_owned(),
            pane.to_owned(),
        );
        let (id, created) = self.writer.call(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db::map_sqlite)?;
            let retired: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM owner_delegation_retired WHERE question_id = ? AND generation = ?)", params![question_id, generation], |row| row.get(0)).map_err(db::map_sqlite)?;
            if retired { return Err(DunkError::Refused("delegation generation is retired".into())); }
            let existing: Option<(String, String, String)> = tx.query_row(
                "SELECT incarnation, pane_id, task_id FROM owner_delegation_tasks WHERE question_id = ? AND generation = ?",
                params![question_id, generation], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            ).optional().map_err(db::map_sqlite)?;
            if let Some((old_incarnation, old_pane, id)) = existing {
                if old_incarnation != incarnation || old_pane != pane {
                    return Err(DunkError::Refused("delegation task identity changed".into()));
                }
                tx.commit().map_err(db::map_sqlite)?;
                return Ok((id, false));
            }
            let next: i64 = tx.query_row(
                "INSERT INTO counters (name, value) VALUES ('task', 1) ON CONFLICT(name) DO UPDATE SET value = value + 1 RETURNING value",
                [], |row| row.get(0),
            ).map_err(db::map_sqlite)?;
            let id = format!("t{next}");
            let now = crate::clock::now_s();
            let body = format!("Answer owner question {question_id}; delegation generation {generation}. Use the authenticated delegation reply operation.");
            let assignees = serde_json::to_string(&[&pane]).map_err(|e| DunkError::Refused(e.to_string()))?;
            tx.execute(
                "INSERT INTO tasks (id, scope, title, body, status, priority, assignees, depends_on, created_at, updated_at, created_by) VALUES (?, 'global', 'Answer delegated owner question', ?, 'open', 'high', ?, '[]', ?, ?, 'owner-delegation')",
                params![id, body, assignees, now, now],
            ).map_err(db::map_sqlite)?;
            tx.execute("INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_to) VALUES (?, 'created', ?, 'owner-delegation', 'global', 'high', 'open')", params![id, now]).map_err(db::map_sqlite)?;
            tx.execute("INSERT INTO owner_delegation_tasks (question_id, generation, incarnation, pane_id, task_id) VALUES (?, ?, ?, ?, ?)", params![question_id, generation, incarnation, pane, id]).map_err(db::map_sqlite)?;
            tx.commit().map_err(db::map_sqlite)?;
            Ok((id, true))
        }).await?;
        if created {
            self.fire(&id, "created", None, None);
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_store::{Outgoing, Store};

    #[tokio::test]
    async fn retries_across_connections_reuse_the_task_and_fence_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");
        let first = Tasks::open(&path).await.unwrap();
        let second = Tasks::open(&path).await.unwrap();
        let (a, b) = tokio::join!(
            first.ensure_owner_delegation_task("i1", 1, "nonce", "%1"),
            second.ensure_owner_delegation_task("i1", 1, "nonce", "%1")
        );
        let id = a.unwrap();
        assert_eq!(id, b.unwrap());
        let task = first.get(&id).await.unwrap();
        assert_eq!(task.assignees, "[\"%1\"]");
        assert!(first
            .ensure_owner_delegation_task("i1", 1, "new nonce", "%1")
            .await
            .is_err());
        assert!(second
            .ensure_owner_delegation_task("i1", 1, "nonce", "%2")
            .await
            .is_err());
        drop(first);
        drop(second);
        let reopened = Tasks::open(&path).await.unwrap();
        assert_eq!(
            id,
            reopened
                .ensure_owner_delegation_task("i1", 1, "nonce", "%1")
                .await
                .unwrap()
        );
        assert!(reopened
            .retire_stale_owner_delegation_task("i1", 1, "wrong")
            .await
            .is_err());
        reopened
            .retire_stale_owner_delegation_task("i1", 1, "nonce")
            .await
            .unwrap();
        reopened
            .retire_stale_owner_delegation_task("i1", 1, "nonce")
            .await
            .unwrap();
        assert_eq!(reopened.get(&id).await.unwrap().status, "cancelled");
        assert!(reopened
            .ensure_owner_delegation_task("i1", 1, "nonce", "%1")
            .await
            .is_err());
        reopened
            .retire_stale_owner_delegation_task("i2", 1, "nonce")
            .await
            .unwrap();
        assert!(reopened
            .ensure_owner_delegation_task("i2", 1, "nonce", "%1")
            .await
            .is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reservation_task_and_web_answer_survive_interrupted_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let inbox_path = dir.path().join("inbox.db");
        let task_path = dir.path().join("tasks.db");
        let blobs = dir.path().join("blobs");
        let (inbox, question, proof) = tokio::task::block_in_place(|| {
            let inbox = Store::open(&inbox_path, &blobs).unwrap();
            let authority = inbox
                .authorize_delegation_supervisor("%1", 123, "start", "owner")
                .unwrap();
            let question = inbox
                .send(&Outgoing {
                    mailbox: "owner",
                    body: Some("question"),
                    transport: Some("web"),
                    ..Outgoing::default()
                })
                .unwrap();
            let proof = inbox
                .reserve_owner_question(&question, &authority, "desk")
                .unwrap();
            drop(inbox);
            let inbox = Store::open(&inbox_path, &blobs).unwrap();
            assert_eq!(
                inbox.active_owner_reservations().unwrap(),
                vec![proof.clone()]
            );
            (std::sync::Arc::new(inbox), question, proof)
        });
        let tasks = Tasks::open(&task_path).await.unwrap();
        let (task, notice) = tasks
            .materialise_owner_delegation(inbox.clone(), proof.clone(), "%1")
            .await
            .unwrap();
        assert_eq!(
            (task.clone(), notice.clone()),
            tasks
                .materialise_owner_delegation(inbox.clone(), proof.clone(), "%1")
                .await
                .unwrap()
        );
        tokio::task::block_in_place(|| {
            assert_eq!(
                notice,
                inbox
                    .ensure_owner_delegation_notice(&proof, "%1", &task)
                    .unwrap()
            );
            let visible = inbox
                .conversation_across_visible(
                    &["owner".into(), "codex-rust-owner".into()],
                    None,
                    None,
                )
                .unwrap();
            assert!(visible
                .iter()
                .all(|row| row.get("id").and_then(serde_json::Value::as_str)
                    != Some(notice.as_str())));
            let answer = inbox
                .answer_owner_question_guarded(&question, "final answer", Some("%1"), Some(&proof))
                .unwrap();
            let pending = inbox.owner_outbox(Some("web"), None).unwrap();
            assert_eq!(pending.len(), 1);
            assert_eq!(pending[0].summary.message_id, answer.summary.message_id);
            assert_eq!(
                pending[0].summary.reply_to.as_deref(),
                Some(question.as_str())
            );
            assert!(inbox.active_owner_reservations().unwrap().is_empty());
            assert!(inbox
                .ensure_owner_delegation_notice(&proof, "%1", &task)
                .is_err());
            drop(inbox);
        });
    }
}
