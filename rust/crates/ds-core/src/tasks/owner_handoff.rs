//! Idempotent task storage for the bounded owner handoff. This is not an API
//! authorization boundary; callers must validate both the request and target.
use super::Tasks;
use crate::{db, error::DunkError};
use rusqlite::{params, OptionalExtension, TransactionBehavior};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HandoffMapping {
    pub task: String,
    pub pane: String,
    pub pid: i64,
    pub start: String,
    pub boot: String,
    pub status: Option<String>,
    pub reconciled: bool,
}

impl Tasks {
    /// Read the stored identity and task; the caller still must revalidate live authority.
    #[allow(dead_code)]
    pub(crate) async fn owner_handoff_mapping(
        &self,
        question_id: &str,
    ) -> Result<Option<HandoffMapping>, DunkError> {
        let question_id = question_id.to_owned();
        self.writer.call(move |connection| {
            type MappingRow = (String, String, i64, String, String, Option<String>, bool);
            let row: Option<MappingRow> = connection.query_row(
                "SELECT h.task_id, h.pane_id, h.supervisor_pid, h.supervisor_start, h.supervisor_boot, t.status, r.question_id IS NOT NULL FROM owner_handoff_tasks h LEFT JOIN tasks t ON t.id = h.task_id LEFT JOIN owner_handoff_reconciled r ON r.question_id = h.question_id WHERE h.question_id = ?",
                [&question_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?)),
            ).optional().map_err(db::map_sqlite)?;
            Ok(row.map(|(task, pane, pid, start, boot, status, reconciled)| HandoffMapping { task, pane, pid, start, boot, status, reconciled }))
        }).await
    }

    /// Durable candidates for a recovery sweep. A service must revalidate each
    /// question and supervisor before acting; this scan provides no authority.
    #[allow(dead_code)]
    pub(crate) async fn owner_handoff_recovery_candidates(
        &self,
        after: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, String, bool, i64, String, String)>, DunkError> {
        let after = after.to_owned();
        let limit = limit.clamp(1, 128) as i64;
        self.writer.call(move |connection| {
            let mut stmt = connection.prepare("SELECT h.question_id, h.pane_id, h.task_id, t.id IS NOT NULL, h.supervisor_pid, h.supervisor_start, h.supervisor_boot FROM owner_handoff_tasks h LEFT JOIN tasks t ON t.id = h.task_id LEFT JOIN owner_handoff_reconciled r ON r.question_id = h.question_id WHERE r.question_id IS NULL AND h.question_id > ? ORDER BY h.question_id LIMIT ?").map_err(db::map_sqlite)?;
            let rows = stmt.query_map(params![after, limit], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?, row.get(6)?))).map_err(db::map_sqlite)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(db::map_sqlite)
        }).await
    }

    #[allow(dead_code)]
    pub(crate) async fn ensure_owner_handoff_task(
        &self,
        question_id: &str,
        pane: &str,
        supervisor_pid: i64,
        supervisor_start: &str,
        supervisor_boot: &str,
    ) -> Result<String, DunkError> {
        if question_id.trim().is_empty()
            || pane.trim().is_empty()
            || supervisor_pid <= 1
            || supervisor_start.trim().is_empty()
            || supervisor_boot.trim().is_empty()
        {
            return Err(DunkError::Refused(
                "question and supervisor are required".into(),
            ));
        }
        let (question_id, pane, supervisor_start, supervisor_boot) = (
            question_id.to_owned(),
            pane.to_owned(),
            supervisor_start.to_owned(),
            supervisor_boot.to_owned(),
        );
        let (id, created) = self.writer.call(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db::map_sqlite)?;
            let reconciled: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM owner_handoff_reconciled WHERE question_id = ?)", [&question_id], |r| r.get(0)).map_err(db::map_sqlite)?;
            if reconciled { return Err(DunkError::Refused("terminal handoff requires explicit recovery".into())); }
            let existing: Option<(String, String, i64, String, String)> = tx.query_row("SELECT pane_id, task_id, supervisor_pid, supervisor_start, supervisor_boot FROM owner_handoff_tasks WHERE question_id = ?", [&question_id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional().map_err(db::map_sqlite)?;
            if let Some((old_pane, id, old_pid, old_start, old_boot)) = existing {
                if old_pane != pane || old_pid != supervisor_pid || old_start != supervisor_start || old_boot != supervisor_boot { return Err(DunkError::Refused("supervisor changed; handoff recovery required".into())); }
                tx.commit().map_err(db::map_sqlite)?;
                return Ok((id, false));
            }
            let next: i64 = tx.query_row("INSERT INTO counters (name, value) VALUES ('task', 1) ON CONFLICT(name) DO UPDATE SET value = value + 1 RETURNING value", [], |r| r.get(0)).map_err(db::map_sqlite)?;
            let id = format!("t{next}");
            let now = crate::clock::now_s();
            let body = format!("Inspect owner question {question_id}. If still unanswered, use owner reply for the final answer; otherwise reconcile without sending a second answer.");
            let assignees = serde_json::to_string(&[&pane]).map_err(|e| DunkError::Refused(e.to_string()))?;
            tx.execute("INSERT INTO tasks (id, scope, title, body, status, priority, assignees, depends_on, created_at, updated_at, created_by) VALUES (?, 'global', 'Answer owner question', ?, 'open', 'high', ?, '[]', ?, ?, 'owner-handoff')", params![id, body, assignees, now, now]).map_err(db::map_sqlite)?;
            tx.execute("INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_to) VALUES (?, 'created', ?, 'owner-handoff', 'global', 'high', 'open')", params![id, now]).map_err(db::map_sqlite)?;
            tx.execute("INSERT INTO owner_handoff_tasks (question_id, pane_id, task_id, supervisor_pid, supervisor_start, supervisor_boot) VALUES (?, ?, ?, ?, ?, ?)", params![question_id, pane, id, supervisor_pid, supervisor_start, supervisor_boot]).map_err(db::map_sqlite)?;
            tx.commit().map_err(db::map_sqlite)?;
            Ok((id, true))
        }).await?;
        if created {
            self.fire(&id, "created", None, None);
        }
        Ok(id)
    }

    /// Call only after the inbox has confirmed an answer and reconciled its notice.
    /// Cancel an obsolete task only if unclaimed; active work needs review.
    #[allow(dead_code)]
    pub(crate) async fn reconcile_answered_owner_handoff(
        &self,
        question_id: &str,
    ) -> Result<(), DunkError> {
        let question_id = question_id.to_owned();
        let changed = self.writer.call(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db::map_sqlite)?;
            let task: Option<(String, String)> = tx.query_row("SELECT t.id, t.status FROM owner_handoff_tasks h JOIN tasks t ON t.id = h.task_id WHERE h.question_id = ?", [&question_id], |r| Ok((r.get(0)?, r.get(1)?))).optional().map_err(db::map_sqlite)?;
            let Some((id, status)) = task else {
                let mapped: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM owner_handoff_tasks WHERE question_id = ?)", [&question_id], |r| r.get(0)).map_err(db::map_sqlite)?;
                if mapped { return Err(DunkError::Refused("handoff task row is missing; owner review required".into())); }
                tx.commit().map_err(db::map_sqlite)?; return Ok(None);
            };
            if matches!(status.as_str(), "cancelled" | "done") {
                tx.execute("INSERT OR IGNORE INTO owner_handoff_reconciled (question_id, reconciled_at) VALUES (?, ?)", params![question_id, crate::clock::now_s()]).map_err(db::map_sqlite)?;
                tx.commit().map_err(db::map_sqlite)?; return Ok(None);
            }
            let leased: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM task_leases WHERE task_id = ?)", [&id], |r| r.get(0)).map_err(db::map_sqlite)?;
            if status != "open" || leased { return Err(DunkError::Refused("handoff task active or terminal; owner review required".into())); }
            let now = crate::clock::now_s();
            tx.execute("UPDATE tasks SET status = 'cancelled', cancelled_at = ?, updated_at = ? WHERE id = ?", params![now, now, id]).map_err(db::map_sqlite)?;
            tx.execute("INSERT INTO task_events (task_id, event, at, actor, scope, priority, status_from, status_to, meta) VALUES (?, 'cancelled', ?, 'rule:owner-handoff-recovery', 'global', 'high', 'open', 'cancelled', ?)", params![id, now,serde_json::json!({"reason":"owner handoff reconciled","question_id":question_id}).to_string()]).map_err(db::map_sqlite)?;
            tx.execute("INSERT OR IGNORE INTO owner_handoff_reconciled (question_id, reconciled_at) VALUES (?, ?)", params![question_id, now]).map_err(db::map_sqlite)?;
            tx.commit().map_err(db::map_sqlite)?;
            Ok(Some(id))
        }).await?;
        if let Some(id) = changed {
            self.fire(&id, "cancelled", None, None);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn retries_reuse_task_and_recovery_cancels_unclaimed_work() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tasks.db");
        let first = Tasks::open(&path).await.unwrap();
        let second = Tasks::open(&path).await.unwrap();
        let (a, b) = tokio::join!(
            first.ensure_owner_handoff_task("i1", "%1", 42, "start", "boot"),
            second.ensure_owner_handoff_task("i1", "%1", 42, "start", "boot")
        );
        let id = a.unwrap();
        assert_eq!(id, b.unwrap());
        assert!(first
            .ensure_owner_handoff_task("i1", "%2", 42, "start", "boot")
            .await
            .is_err());
        assert!(first
            .ensure_owner_handoff_task("i1", "%1", 43, "start", "boot")
            .await
            .is_err());
        assert!(first
            .ensure_owner_handoff_task("i1", "%1", 42, "new-start", "boot")
            .await
            .is_err());
        assert!(first
            .ensure_owner_handoff_task("i1", "%1", 42, "start", "new-boot")
            .await
            .is_err());
        assert_eq!(first.get(&id).await.unwrap().assignees, "[\"%1\"]");
        first.reconcile_answered_owner_handoff("i1").await.unwrap();
        assert_eq!(first.get(&id).await.unwrap().status, "cancelled");
        assert!(first
            .ensure_owner_handoff_task("i1", "%1", 42, "start", "boot")
            .await
            .is_err());
        let active = first
            .ensure_owner_handoff_task("i2", "%1", 42, "start", "boot")
            .await
            .unwrap();
        let claim = first
            .claim_next("mine", "%1", crate::tasks::ClaimOptions::default())
            .await
            .unwrap();
        assert!(matches!(claim, crate::tasks::ClaimOutcome::Claimed { .. }));
        assert!(first.reconcile_answered_owner_handoff("i2").await.is_err());
        assert_eq!(first.get(&active).await.unwrap().status, "in_progress");
        let later = first
            .ensure_owner_handoff_task("i3", "%1", 42, "start", "boot")
            .await
            .unwrap();
        let first_page = first
            .owner_handoff_recovery_candidates("", 1)
            .await
            .unwrap();
        assert_eq!(first_page.len(), 1);
        assert_eq!(first_page[0].0, "i2");
        let second_page = first
            .owner_handoff_recovery_candidates(&first_page[0].0, 1)
            .await
            .unwrap();
        assert_eq!(second_page.len(), 1);
        assert_eq!(second_page[0].2, later);
        assert!(first
            .owner_handoff_recovery_candidates(&second_page[0].0, 1)
            .await
            .unwrap()
            .is_empty());
        let orphan = first
            .ensure_owner_handoff_task("i4", "%1", 42, "start", "boot")
            .await
            .unwrap();
        let orphan_for_delete = orphan.clone();
        first
            .writer
            .call(move |connection| {
                connection
                    .execute("DELETE FROM tasks WHERE id = ?", [&orphan_for_delete])
                    .map_err(db::map_sqlite)?;
                Ok(())
            })
            .await
            .unwrap();
        let orphan_page = first
            .owner_handoff_recovery_candidates("i3", 1)
            .await
            .unwrap();
        assert_eq!(orphan_page[0].2, orphan);
        assert!(!orphan_page[0].3);
        assert!(first.reconcile_answered_owner_handoff("i4").await.is_err());
    }
}

/// One recovery pass for answered questions. It intentionally does not wake or
/// create notices; callers must surface returned errors and schedule retries.
#[allow(dead_code)]
pub(crate) async fn reconcile_answered_handoffs(
    tasks: &Tasks,
    inbox: &std::sync::Arc<crate::inbox_store::Store>,
    after: &str,
    limit: usize,
) -> Result<(Vec<(String, String)>, Option<String>), DunkError> {
    let page_size = limit.clamp(1, 128);
    let candidates = tasks
        .owner_handoff_recovery_candidates(after, page_size)
        .await?;
    // A scheduler resets to the start after None, so new earlier IDs are revisited.
    let next = if candidates.len() == page_size {
        candidates.last().map(|row| row.0.clone())
    } else {
        None
    };
    let mut unresolved = Vec::new();
    for (question, _pane, task, task_exists, _pid, _start, _boot) in candidates {
        let store = inbox.clone();
        let question_for_store = question.clone();
        let answered = match tokio::task::spawn_blocking(move || {
            store.reconcile_answered_owner_notice(&question_for_store)
        })
        .await
        {
            Ok(Ok(answered)) => answered,
            Ok(Err(error)) => {
                unresolved.push((question, error.to_string()));
                continue;
            }
            Err(error) => {
                unresolved.push((question, format!("handoff recovery worker failed: {error}")));
                continue;
            }
        };
        if !task_exists {
            unresolved.push((
                question,
                format!("handoff task {task} is missing; owner review required"),
            ));
            continue;
        }
        if answered {
            if let Err(error) = tasks.reconcile_answered_owner_handoff(&question).await {
                unresolved.push((question, error.to_string()));
            }
        }
    }
    Ok((unresolved, next))
}

#[cfg(test)]
mod recovery_tests {
    use super::*;
    use crate::inbox_store::{Outgoing, Store};

    #[tokio::test(flavor = "multi_thread")]
    async fn recovery_reconciles_an_answer_after_interrupted_handoff() {
        let dir = tempfile::tempdir().unwrap();
        let inbox_path = dir.path().join("inbox.db");
        let blobs = dir.path().join("blobs");
        let inbox = tokio::task::spawn_blocking(move || Store::open(&inbox_path, &blobs))
            .await
            .unwrap()
            .unwrap();
        let inbox = std::sync::Arc::new(inbox);
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let store = inbox.clone();
        let question = tokio::task::spawn_blocking(move || {
            store.send(&Outgoing {
                mailbox: "owner",
                body: Some("question"),
                transport: Some("web"),
                ..Outgoing::default()
            })
        })
        .await
        .unwrap()
        .unwrap();
        let task = tasks
            .ensure_owner_handoff_task(&question, "%1", 42, "start", "boot")
            .await
            .unwrap();
        let store = inbox.clone();
        let q = question.clone();
        let task_for_notice = task.clone();
        let notice = tokio::task::spawn_blocking(move || {
            let handoff = store.ensure_owner_handoff_notice(&q, "%1", &task_for_notice)?;
            store.send(&Outgoing {
                mailbox: "owner",
                body: Some("final"),
                reply_to: Some(&q),
                ..Outgoing::default()
            })?;
            Ok::<_, DunkError>(handoff.notice_id)
        })
        .await
        .unwrap()
        .unwrap();
        assert!(reconcile_answered_handoffs(&tasks, &inbox, "", 128)
            .await
            .unwrap()
            .0
            .is_empty());
        assert_eq!(tasks.get(&task).await.unwrap().status, "cancelled");
        let store = inbox.clone();
        let state = tokio::task::spawn_blocking(move || {
            store.get_message(&notice).map(|m| m.summary.state)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(state, "cancelled");
        assert!(tasks
            .owner_handoff_recovery_candidates("", 128)
            .await
            .unwrap()
            .is_empty());
        let store = inbox.clone();
        let q2 = tokio::task::spawn_blocking(move || {
            store.send(&Outgoing {
                mailbox: "owner",
                body: Some("second"),
                transport: Some("telegram"),
                ..Outgoing::default()
            })
        })
        .await
        .unwrap()
        .unwrap();
        let task2 = tasks
            .ensure_owner_handoff_task(&q2, "%1", 42, "start", "boot")
            .await
            .unwrap();
        let store = inbox.clone();
        let q2_for_store = q2.clone();
        let task2_for_notice = task2.clone();
        let notice2 = tokio::task::spawn_blocking(move || {
            store
                .ensure_owner_handoff_notice(&q2_for_store, "%1", &task2_for_notice)
                .map(|h| h.notice_id)
        })
        .await
        .unwrap()
        .unwrap();
        tasks.cancel(&task2, "no longer needed").await.unwrap();
        let store = inbox.clone();
        tokio::task::spawn_blocking(move || {
            store.send(&Outgoing {
                mailbox: "owner",
                body: Some("final"),
                reply_to: Some(&q2),
                ..Outgoing::default()
            })
        })
        .await
        .unwrap()
        .unwrap();
        assert!(reconcile_answered_handoffs(&tasks, &inbox, "", 128)
            .await
            .unwrap()
            .0
            .is_empty());
        let store = inbox.clone();
        let state2 = tokio::task::spawn_blocking(move || {
            store.get_message(&notice2).map(|m| m.summary.state)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(state2, "cancelled");
        assert!(tasks
            .owner_handoff_recovery_candidates("", 128)
            .await
            .unwrap()
            .is_empty());
        // A missing task must be reported without blocking a later recoverable handoff.
        let store = inbox.clone();
        let (q3, q4) = tokio::task::spawn_blocking(move || {
            let q3 = store.send(&Outgoing {
                mailbox: "owner",
                body: Some("third"),
                transport: Some("web"),
                ..Outgoing::default()
            })?;
            let q4 = store.send(&Outgoing {
                mailbox: "owner",
                body: Some("fourth"),
                transport: Some("web"),
                ..Outgoing::default()
            })?;
            store.send(&Outgoing {
                mailbox: "owner",
                body: Some("final"),
                reply_to: Some(&q4),
                ..Outgoing::default()
            })?;
            Ok::<_, DunkError>((q3, q4))
        })
        .await
        .unwrap()
        .unwrap();
        let orphan = tasks
            .ensure_owner_handoff_task(&q3, "%1", 42, "start", "boot")
            .await
            .unwrap();
        let store = inbox.clone();
        let q3_for_store = q3.clone();
        let orphan_for_notice = orphan.clone();
        let orphan_notice = tokio::task::spawn_blocking(move || {
            let notice =
                store.ensure_owner_handoff_notice(&q3_for_store, "%1", &orphan_for_notice)?;
            store.send(&Outgoing {
                mailbox: "owner",
                body: Some("final"),
                reply_to: Some(&q3_for_store),
                ..Outgoing::default()
            })?;
            Ok::<_, DunkError>(notice.notice_id)
        })
        .await
        .unwrap()
        .unwrap();
        let healthy = tasks
            .ensure_owner_handoff_task(&q4, "%1", 42, "start", "boot")
            .await
            .unwrap();
        tasks
            .writer
            .call(move |connection| {
                connection
                    .execute("DELETE FROM tasks WHERE id = ?", [&orphan])
                    .map_err(db::map_sqlite)?;
                Ok(())
            })
            .await
            .unwrap();
        let (unresolved, _) = reconcile_answered_handoffs(&tasks, &inbox, "", 128)
            .await
            .unwrap();
        assert_eq!(unresolved.len(), 1);
        assert_eq!(unresolved[0].0, q3);
        assert_eq!(tasks.get(&healthy).await.unwrap().status, "cancelled");
        let store = inbox.clone();
        let orphan_notice_state = tokio::task::spawn_blocking(move || {
            store.get_message(&orphan_notice).map(|m| m.summary.state)
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(orphan_notice_state, "cancelled");
        tokio::task::spawn_blocking(move || drop(inbox))
            .await
            .unwrap();
    }
}

/// Recheck the exact task after notice creation. A known terminal or changed mapping
/// cancels only its matching queued/read notice. Storage errors never permit a wake.
#[allow(dead_code)]
pub(crate) async fn postcheck_pending_notice(
    tasks: &Tasks,
    inbox: &std::sync::Arc<crate::inbox_store::Store>,
    question: &str,
    original: &HandoffMapping,
) -> Result<(), DunkError> {
    let (current, lookup_error) = match tasks.owner_handoff_mapping(question).await {
        Ok(current) => (current, None),
        Err(error) => (None, Some(error.to_string())),
    };
    let active = current.as_ref().is_some_and(|row| {
        !row.reconciled
            && row.task == original.task
            && row.pane == original.pane
            && row.pid == original.pid
            && row.start == original.start
            && row.boot == original.boot
            && matches!(row.status.as_deref(), Some("open" | "in_progress"))
    });
    if active {
        return Ok(());
    }
    let (q, task, pane) = (
        question.to_owned(),
        original.task.clone(),
        original.pane.clone(),
    );
    let store = inbox.clone();
    let cause = lookup_error.unwrap_or_else(|| "handoff task or identity changed".into());
    let compensation =
        tokio::task::spawn_blocking(move || store.cancel_owner_handoff_notice(&q, &task, &pane))
            .await
            .map_err(|error| {
                DunkError::Refused(format!(
                    "{cause}; notice compensation worker failed: {error}"
                ))
            })?;
    match compensation {
        Ok(state) => Err(DunkError::Refused(format!(
            "{cause}; explicit recovery required (prior notice state: {})",
            state.as_deref().unwrap_or("missing")
        ))),
        Err(error) => Err(DunkError::Refused(format!(
            "{cause}; notice compensation and durable attention failed: {error}"
        ))),
    }
}

/// Result of rematerialising a notice, not evidence of supervisor pickup.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PendingHandoff {
    Answered,
    Waiting {
        task: String,
        notice: String,
        transport: String,
        state: String,
        waiting_s: f64,
    },
}

/// Recreate an unanswered notice after interruption. The caller must obtain `target`
/// from a fresh trusted live-pane check and revalidate it before any wake; this helper
/// itself grants no authority and performs no notification.
#[allow(dead_code)]
pub(crate) async fn recover_pending_handoff(
    tasks: &Tasks,
    inbox: &std::sync::Arc<crate::inbox_store::Store>,
    question: &str,
    target: &crate::api::owner_handoff::LiveBinding,
) -> Result<PendingHandoff, DunkError> {
    recover_pending_handoff_inner(tasks, inbox, question, target, || async {}).await
}

async fn recover_pending_handoff_inner<F, Fut>(
    tasks: &Tasks,
    inbox: &std::sync::Arc<crate::inbox_store::Store>,
    question: &str,
    target: &crate::api::owner_handoff::LiveBinding,
    after_mapping: F,
) -> Result<PendingHandoff, DunkError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let mapping = tasks
        .owner_handoff_mapping(question)
        .await?
        .ok_or_else(|| DunkError::Refused("no handoff mapping".into()))?;
    if mapping.reconciled {
        return Err(DunkError::Refused(
            "terminal handoff requires explicit recovery".into(),
        ));
    }
    if mapping.pane != target.pane()
        || mapping.pid != target.pid()
        || mapping.start != target.start()
        || mapping.boot != target.boot()
    {
        return Err(DunkError::Refused(
            "supervisor identity changed; handoff recovery required".into(),
        ));
    }
    if !matches!(mapping.status.as_deref(), Some("open" | "in_progress")) {
        return Err(DunkError::Refused(
            "handoff task is not active; owner review required".into(),
        ));
    }
    after_mapping().await;
    let store = inbox.clone();
    let q = question.to_owned();
    let pane = mapping.pane.clone();
    let task = mapping.task.clone();
    let notice = tokio::task::spawn_blocking(move || {
        if store.reconcile_answered_owner_notice(&q)? {
            return Ok(None);
        }
        if let Err(error) = store.validate_owner_handoff_question(&q) {
            return if store.reconcile_answered_owner_notice(&q)? {
                Ok(None)
            } else {
                Err(error)
            };
        }
        match store.ensure_owner_handoff_notice(&q, &pane, &task) {
            Ok(notice) => {
                if store.reconcile_answered_owner_notice(&q)? {
                    Ok(None)
                } else {
                    Ok(Some(notice))
                }
            }
            Err(error) => {
                if store.reconcile_answered_owner_notice(&q)? {
                    Ok(None)
                } else {
                    Err(error)
                }
            }
        }
    })
    .await
    .map_err(|error| DunkError::Refused(format!("handoff recovery worker failed: {error}")))??;
    match notice {
        Some(notice) => {
            postcheck_pending_notice(tasks, inbox, question, &mapping).await?;
            Ok(PendingHandoff::Waiting {
                task: mapping.task,
                notice: notice.notice_id,
                transport: notice.transport,
                state: notice.notice_state,
                waiting_s: (crate::clock::now_s() - notice.created_at).max(0.0),
            })
        }
        None => {
            tasks.reconcile_answered_owner_handoff(question).await?;
            Ok(PendingHandoff::Answered)
        }
    }
}

#[cfg(test)]
mod pending_tests {
    use super::*;
    use crate::inbox_store::{Outgoing, Store};

    #[tokio::test(flavor = "multi_thread")]
    async fn owner_handoff_postcheck_store_failure_cancels_and_records_attention() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.db");
        let blobs = dir.path().join("blobs");
        let inbox = std::sync::Arc::new(
            tokio::task::spawn_blocking(move || Store::open(&path, &blobs))
                .await
                .unwrap()
                .unwrap(),
        );
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let store = inbox.clone();
        let question = tokio::task::spawn_blocking(move || {
            store.send(&Outgoing {
                mailbox: "owner",
                body: Some("question"),
                transport: Some("web"),
                ..Outgoing::default()
            })
        })
        .await
        .unwrap()
        .unwrap();
        let task = tasks
            .ensure_owner_handoff_task(&question, "%1", 42, "start", "boot")
            .await
            .unwrap();
        let binding = crate::api::owner_handoff::LiveBinding::for_test("%1", 42, "start", "boot");
        let result = recover_pending_handoff_inner(&tasks, &inbox, &question, &binding, || async {
            tasks
                .writer
                .call(|connection| {
                    connection
                        .execute_batch(
                            "ALTER TABLE owner_handoff_tasks RENAME TO owner_handoff_tasks_hidden",
                        )
                        .map_err(db::map_sqlite)
                })
                .await
                .unwrap();
        })
        .await;
        assert!(result.is_err());
        tasks
            .writer
            .call(|connection| {
                connection
                    .execute_batch(
                        "ALTER TABLE owner_handoff_tasks_hidden RENAME TO owner_handoff_tasks",
                    )
                    .map_err(db::map_sqlite)
            })
            .await
            .unwrap();
        let store = inbox.clone();
        let attention =
            tokio::task::spawn_blocking(move || store.owner_handoff_attention_page("", 128))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(attention.len(), 1);
        assert_eq!(attention[0].0, question);
        assert_eq!(attention[0].1, task);
        assert_eq!(attention[0].3.as_deref(), Some("queued"));
        assert!(recover_pending_handoff(&tasks, &inbox, &question, &binding)
            .await
            .is_err());
        let store = inbox.clone();
        let original = tokio::task::spawn_blocking(move || store.get_message(&question))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.summary.state, "queued");
        tokio::task::spawn_blocking(move || drop(inbox))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn owner_handoff_task_cancelled_during_materialization_cancels_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.db");
        let query_path = path.clone();
        let blobs = dir.path().join("blobs");
        let inbox = std::sync::Arc::new(
            tokio::task::spawn_blocking(move || Store::open(&path, &blobs))
                .await
                .unwrap()
                .unwrap(),
        );
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let store = inbox.clone();
        let question = tokio::task::spawn_blocking(move || {
            store.send(&Outgoing {
                mailbox: "owner",
                body: Some("question"),
                transport: Some("web"),
                ..Outgoing::default()
            })
        })
        .await
        .unwrap()
        .unwrap();
        let task = tasks
            .ensure_owner_handoff_task(&question, "%1", 42, "start", "boot")
            .await
            .unwrap();
        let binding = crate::api::owner_handoff::LiveBinding::for_test("%1", 42, "start", "boot");
        let result = recover_pending_handoff_inner(&tasks, &inbox, &question, &binding, || async {
            tasks
                .cancel(&task, "cancel during notice creation")
                .await
                .unwrap();
        })
        .await;
        assert!(result.is_err());
        let q = question.clone();
        let state = tokio::task::spawn_blocking(move || {
            let connection = rusqlite::Connection::open(query_path).unwrap();
            connection.query_row("SELECT m.state FROM owner_handoffs h JOIN messages m ON m.id = h.notice_id WHERE h.question_id = ?", [&q], |r| r.get::<_, String>(0)).unwrap()
        }).await.unwrap();
        assert_eq!(state, "cancelled");
        let store = inbox.clone();
        let original = tokio::task::spawn_blocking(move || store.get_message(&question))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(original.summary.state, "queued");
        assert_eq!(original.summary.transport.as_deref(), Some("web"));
        tokio::task::spawn_blocking(move || drop(inbox))
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn owner_handoff_pending_notice_recovers_idempotently_and_fences_changed_target() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.db");
        let blobs = dir.path().join("blobs");
        let inbox = std::sync::Arc::new(
            tokio::task::spawn_blocking(move || Store::open(&path, &blobs))
                .await
                .unwrap()
                .unwrap(),
        );
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let store = inbox.clone();
        let question = tokio::task::spawn_blocking(move || {
            store.send(&Outgoing {
                mailbox: "owner",
                body: Some("question"),
                transport: Some("telegram"),
                ..Outgoing::default()
            })
        })
        .await
        .unwrap()
        .unwrap();
        let task = tasks
            .ensure_owner_handoff_task(&question, "%1", 42, "start", "boot")
            .await
            .unwrap();
        let good = crate::api::owner_handoff::LiveBinding::for_test("%1", 42, "start", "boot");
        let changed =
            crate::api::owner_handoff::LiveBinding::for_test("%1", 42, "new-start", "boot");
        let other_boot =
            crate::api::owner_handoff::LiveBinding::for_test("%1", 42, "start", "other-boot");
        assert!(
            recover_pending_handoff(&tasks, &inbox, &question, &other_boot)
                .await
                .is_err()
        );
        assert!(recover_pending_handoff(&tasks, &inbox, &question, &changed)
            .await
            .is_err());
        let first = recover_pending_handoff(&tasks, &inbox, &question, &good)
            .await
            .unwrap();
        let second = recover_pending_handoff(&tasks, &inbox, &question, &good)
            .await
            .unwrap();
        match (&first, &second) {
            (
                PendingHandoff::Waiting {
                    task: a,
                    notice: n1,
                    transport: t,
                    ..
                },
                PendingHandoff::Waiting { notice: n2, .. },
            ) => {
                assert_eq!(a, &task);
                assert_eq!(n1, n2);
                assert_eq!(t, "telegram");
            }
            _ => panic!("expected one stable waiting notice"),
        }
        let store = inbox.clone();
        let q = question.clone();
        tokio::task::spawn_blocking(move || {
            store.send(&Outgoing {
                mailbox: "owner",
                body: Some("answer"),
                reply_to: Some(&q),
                ..Outgoing::default()
            })
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(
            recover_pending_handoff(&tasks, &inbox, &question, &good)
                .await
                .unwrap(),
            PendingHandoff::Answered
        );
        assert_eq!(tasks.get(&task).await.unwrap().status, "cancelled");
        tokio::task::spawn_blocking(move || drop(inbox))
            .await
            .unwrap();
    }
}

/// One bounded recovery page. The caller must supply fresh, trusted target checks,
/// handle every returned status, revalidate before wake, and schedule subsequent pages.
#[allow(dead_code)] // Cursor is consumed when the recovery worker is wired.
#[derive(Debug, Default)]
pub(crate) struct RecoveryPage {
    pub unresolved: Vec<(String, String)>,
    pub pending: Vec<(String, PendingHandoff)>,
    pub next: Option<String>,
}

#[allow(dead_code)]
pub(crate) async fn recover_handoff_page<F>(
    tasks: &Tasks,
    inbox: &std::sync::Arc<crate::inbox_store::Store>,
    after: &str,
    limit: usize,
    verify_target: F,
) -> Result<RecoveryPage, DunkError>
where
    F: Fn() -> Result<crate::api::owner_handoff::LiveBinding, DunkError> + Send + Sync,
{
    let page_size = limit.clamp(1, 128);
    let candidates = tasks
        .owner_handoff_recovery_candidates(after, page_size)
        .await?;
    let mut report = RecoveryPage {
        next: if candidates.len() == page_size {
            candidates.last().map(|row| row.0.clone())
        } else {
            None
        },
        ..RecoveryPage::default()
    };
    for (question, _pane, task, task_exists, _pid, _start, _boot) in candidates {
        let store = inbox.clone();
        let q = question.clone();
        let answered =
            match tokio::task::spawn_blocking(move || store.reconcile_answered_owner_notice(&q))
                .await
            {
                Ok(Ok(answered)) => answered,
                Ok(Err(error)) => {
                    report.unresolved.push((question, error.to_string()));
                    continue;
                }
                Err(error) => {
                    report
                        .unresolved
                        .push((question, format!("handoff recovery worker failed: {error}")));
                    continue;
                }
            };
        if !task_exists {
            report.unresolved.push((
                question,
                format!("handoff task {task} is missing; owner review required"),
            ));
            continue;
        }
        if answered {
            if let Err(error) = tasks.reconcile_answered_owner_handoff(&question).await {
                report.unresolved.push((question, error.to_string()));
            }
            continue;
        }
        let target = match verify_target() {
            Ok(target) => target,
            Err(error) => {
                report.unresolved.push((question, error.to_string()));
                continue;
            }
        };
        match recover_pending_handoff(tasks, inbox, &question, &target).await {
            Ok(PendingHandoff::Answered) => {}
            Ok(waiting) => {
                if verify_target().is_ok_and(|again| again == target) {
                    report.pending.push((question, waiting));
                } else {
                    report.unresolved.push((
                        question,
                        "supervisor identity changed after notice; recovery required".into(),
                    ));
                }
            }
            Err(error) => report.unresolved.push((question, error.to_string())),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod page_tests {
    use super::*;
    use crate::inbox_store::{Outgoing, Store};

    #[tokio::test(flavor = "multi_thread")]
    async fn owner_handoff_recovery_page_cleans_answers_and_recreates_pending_notice() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.db");
        let blobs = dir.path().join("blobs");
        let inbox = std::sync::Arc::new(
            tokio::task::spawn_blocking(move || Store::open(&path, &blobs))
                .await
                .unwrap()
                .unwrap(),
        );
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let store = inbox.clone();
        let (answered, pending) = tokio::task::spawn_blocking(move || {
            let answered = store.send(&Outgoing {
                mailbox: "owner",
                body: Some("one"),
                transport: Some("web"),
                ..Outgoing::default()
            })?;
            let pending = store.send(&Outgoing {
                mailbox: "owner",
                body: Some("two"),
                transport: Some("telegram"),
                ..Outgoing::default()
            })?;
            store.send(&Outgoing {
                mailbox: "owner",
                body: Some("final"),
                reply_to: Some(&answered),
                ..Outgoing::default()
            })?;
            Ok::<_, DunkError>((answered, pending))
        })
        .await
        .unwrap()
        .unwrap();
        let answered_task = tasks
            .ensure_owner_handoff_task(&answered, "%1", 42, "start", "boot")
            .await
            .unwrap();
        let pending_task = tasks
            .ensure_owner_handoff_task(&pending, "%1", 42, "start", "boot")
            .await
            .unwrap();
        let target = crate::api::owner_handoff::LiveBinding::for_test("%1", 42, "start", "boot");
        let first = recover_handoff_page(&tasks, &inbox, "", 128, || Ok(target.clone()))
            .await
            .unwrap();
        assert!(first.unresolved.is_empty());
        assert_eq!(tasks.get(&answered_task).await.unwrap().status, "cancelled");
        assert_eq!(first.pending.len(), 1);
        assert_eq!(first.pending[0].0, pending);
        let notice = match &first.pending[0].1 {
            PendingHandoff::Waiting {
                task,
                notice,
                transport,
                ..
            } => {
                assert_eq!(task, &pending_task);
                assert_eq!(transport, "telegram");
                notice.clone()
            }
            _ => panic!("expected pending notice"),
        };
        let second = recover_handoff_page(&tasks, &inbox, "", 128, || Ok(target.clone()))
            .await
            .unwrap();
        assert!(
            matches!(&second.pending[0].1, PendingHandoff::Waiting { notice: same, .. } if same == &notice)
        );
        let changed =
            crate::api::owner_handoff::LiveBinding::for_test("%1", 42, "start", "other-boot");
        let third = recover_handoff_page(&tasks, &inbox, "", 128, || Ok(changed.clone()))
            .await
            .unwrap();
        assert!(third.pending.is_empty());
        assert_eq!(third.unresolved.len(), 1);
        tokio::task::spawn_blocking(move || drop(inbox))
            .await
            .unwrap();
    }
}
