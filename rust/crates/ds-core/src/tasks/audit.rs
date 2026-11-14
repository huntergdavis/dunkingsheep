//! Audit metadata for new task transitions. This never supplies authorization.
use serde_json::{json, Value};

pub(crate) fn actor(actor: Option<&str>, fallback: &str) -> String {
    actor
        .filter(|a| !a.trim().is_empty())
        .unwrap_or(fallback)
        .to_owned()
}

pub(super) fn transition_actor(event: &str, supplied: Option<&str>) -> Option<String> {
    if matches!(event, "blocked" | "unblocked" | "cancelled") {
        Some(actor(supplied, "service:tasks"))
    } else {
        supplied.map(str::to_owned)
    }
}

pub(super) fn status_meta(
    event: &str,
    reason: Option<&str>,
    previous: Option<&str>,
) -> Option<String> {
    if !matches!(event, "blocked" | "unblocked" | "cancelled") {
        return reason.map(|r| json!({"reason":r}).to_string());
    }
    let mut meta = json!({"reason":reason.filter(|r|!r.is_empty()).map(str::to_owned)
        .unwrap_or_else(||format!("task {event} (no reason supplied)"))});
    if let Some(previous) = previous {
        meta["previous_block_reason"] = Value::String(previous.to_owned());
    }
    Some(meta.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::{FieldEdit, Tasks};
    use rusqlite::{params, Connection};

    async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
        let dir = crate::testsupport::dir(&format!("task-audit-{tag}"));
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        crate::testsupport::Scratch::new(tasks, dir)
    }
    fn event(tasks: &Tasks, id: &str, name: &str) -> (String, Value) {
        let (actor,meta):(String,String)=Connection::open(tasks.path()).unwrap().query_row(
            "SELECT actor,meta FROM task_events WHERE task_id=?1 AND event=?2 ORDER BY id DESC LIMIT 1",
            params![id,name],|r|Ok((r.get(0)?,r.get(1)?))).unwrap();
        (actor, serde_json::from_str(&meta).unwrap())
    }

    #[tokio::test]
    async fn audit_preserves_who_and_why_across_every_manual_transition() {
        let tasks = store("manual").await;
        let t = tasks.add("global", "work", "").await.unwrap();
        tasks
            .assign_with_audit(
                &t.id,
                &["%2"],
                "set",
                false,
                None,
                "%1",
                Some("handoff to available lane"),
            )
            .await
            .unwrap();
        let (actor, meta) = event(&tasks, &t.id, "assigned");
        assert_eq!("%1", actor);
        assert_eq!("handoff to available lane", meta["reason"]);
        tasks
            .set_status(&t.id, "blocked", Some("wait for CI"), Some("%2"))
            .await
            .unwrap();
        tasks
            .set_status(&t.id, "open", Some("CI passed"), Some("%3"))
            .await
            .unwrap();
        let (actor, meta) = event(&tasks, &t.id, "unblocked");
        assert_eq!("%3", actor);
        assert_eq!("CI passed", meta["reason"]);
        assert_eq!("wait for CI", meta["previous_block_reason"]);
        tasks
            .set_status(&t.id, "blocked", Some("wait for release"), Some("%2"))
            .await
            .unwrap();
        tasks
            .set_status(&t.id, "cancelled", Some("superseded"), Some("%1"))
            .await
            .unwrap();
        let (actor, meta) = event(&tasks, &t.id, "cancelled");
        assert_eq!("%1", actor);
        assert_eq!("superseded", meta["reason"]);
        assert_eq!("wait for release", meta["previous_block_reason"]);
        // Full task read uses this same persisted history, even after block_reason changed.
        let wire = tasks
            .read_visible_wire_with_notes(&t.id, None)
            .await
            .unwrap()
            .wire;
        assert!(wire["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|n| n["event"] == "unblocked"
                && n["actor"] == "%3"
                && n["reason"] == "CI passed"));
        let (actor, meta) = event(&tasks, &t.id, "blocked");
        assert_eq!("%2", actor);
        assert_eq!("wait for release", meta["reason"]);
    }

    #[tokio::test]
    async fn audit_updates_record_assignment_and_final_raw_block_reason_atomically() {
        let tasks = store("update").await;
        let t = tasks.add("global", "work", "").await.unwrap();
        tasks
            .update_with_status_and_raw_block_reason(
                &t.id,
                FieldEdit {
                    assignees: Some(vec!["%2".into()]),
                    ..Default::default()
                },
                Some("blocked"),
                Some("%1"),
                Some(Some(json!(false))),
                None,
            )
            .await
            .unwrap();
        let (actor, meta) = event(&tasks, &t.id, "blocked");
        assert_eq!("%1", actor);
        assert_eq!("0", meta["reason"]);
        let (actor, meta) = event(&tasks, &t.id, "assigned");
        assert_eq!("%1", actor);
        assert!(meta["reason"].is_string());
        tasks
            .update_with_status(&t.id, FieldEdit::default(), Some("open"), Some("%3"))
            .await
            .unwrap();
        let (actor, meta) = event(&tasks, &t.id, "unblocked");
        assert_eq!("%3", actor);
        assert_eq!("0", meta["previous_block_reason"]);
        assert!(meta["reason"].is_string());
    }

    #[tokio::test]
    async fn audit_internal_dependency_rule_names_itself_and_missing_reason_is_explicit() {
        let tasks = store("rule").await;
        let prerequisite = tasks.add("global", "first", "").await.unwrap();
        let t = tasks.add("global", "next", "").await.unwrap();
        tasks
            .block_by_actor(&t.id, &prerequisite.id, "%1")
            .await
            .unwrap();
        assert_eq!("%1", event(&tasks, &t.id, "blocked").0);
        tasks.complete(&prerequisite.id).await.unwrap();
        let (actor, meta) = event(&tasks, &t.id, "unblocked");
        assert_eq!("rule:auto-unblock", actor);
        assert_eq!(
            format!("auto: {} completed", prerequisite.id),
            meta["reason"]
        );
        tasks
            .set_status(&t.id, "blocked", None, None)
            .await
            .unwrap();
        let (actor, meta) = event(&tasks, &t.id, "blocked");
        assert_eq!("service:tasks", actor);
        assert_eq!("task blocked (no reason supplied)", meta["reason"]);
    }

    #[tokio::test]
    async fn audit_insert_failure_rolls_back_status_reason_and_claim_release() {
        for status in ["blocked", "open", "cancelled"] {
            let tasks = store(&format!("rollback-{status}")).await;
            let t = tasks.add("global", "work", "").await.unwrap();
            if status == "open" {
                tasks.block(&t.id, "original block").await.unwrap();
            }
            tasks.insert_lease(&t.id, "%1", 1.0, 1.0, 1e20).await;
            let before = tasks.get(&t.id).await.unwrap();
            Connection::open(tasks.path()).unwrap().execute_batch(
                "CREATE TRIGGER fail_audit BEFORE INSERT ON task_events WHEN NEW.event IN ('blocked','unblocked','cancelled') BEGIN SELECT RAISE(ABORT,'audit failure'); END;"
            ).unwrap();
            assert!(tasks
                .set_status(&t.id, status, Some("new reason"), Some("%1"))
                .await
                .is_err());
            let after = tasks.get(&t.id).await.unwrap();
            assert_eq!(before.status, after.status);
            assert_eq!(before.block_reason, after.block_reason);
            assert_eq!(1, tasks.leases(&t.id).await);
        }
    }
}

#[cfg(test)]
mod owner_answer_tests {
    use super::*;
    use crate::tasks::Tasks;
    #[tokio::test]
    async fn audit_owner_answer_preserves_the_owner_question_and_service_identity() {
        let dir = crate::testsupport::dir("task-audit-owner-answer");
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let t = tasks.add("global", "question", "").await.unwrap();
        tasks.block(&t.id, "needs-owner: proceed?").await.unwrap();
        let q = tasks.owner_question(&t.id, None).await.unwrap();
        let version = q["version"].as_str().unwrap().parse().unwrap();
        let result = tasks
            .owner_answer(&t.id, version, None, Some("yes"), None, None)
            .await
            .unwrap();
        assert_eq!("open", result["task"]["status"]);
        let read = tasks
            .read_visible_wire_with_notes(&t.id, None)
            .await
            .unwrap();
        let note = read.wire["notes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["event"] == "unblocked")
            .unwrap();
        assert_eq!("owner-service", note["actor"]);
        assert_eq!("owner answered: yes", note["reason"]);
        let meta: String = rusqlite::Connection::open(tasks.path())
            .unwrap()
            .query_row(
                "SELECT meta FROM task_events WHERE task_id=?1 AND event='unblocked'",
                [&t.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            "needs-owner: proceed?",
            serde_json::from_str::<Value>(&meta).unwrap()["previous_block_reason"]
        );
    }
}

#[cfg(test)]
mod reason_edit_tests {
    use super::*;
    use crate::tasks::{FieldEdit, Tasks};
    #[tokio::test]
    async fn audit_reason_only_update_keeps_both_reasons_and_a_new_block_generation() {
        let dir = crate::testsupport::dir("task-audit-reason-only");
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let task = tasks.add("global", "work", "").await.unwrap();
        tasks
            .set_status(&task.id, "blocked", Some("first reason"), Some("%1"))
            .await
            .unwrap();
        tasks
            .update(
                &task.id,
                FieldEdit {
                    block_reason: Some(Some("second reason".into())),
                    ..Default::default()
                },
                Some("%2"),
            )
            .await
            .unwrap();
        let conn = rusqlite::Connection::open(tasks.path()).unwrap();
        let mut stmt=conn.prepare("SELECT id,actor,meta FROM task_events WHERE task_id=? AND event='blocked' ORDER BY id").unwrap();
        let rows = stmt
            .query_map([&task.id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(2, rows.len());
        assert!(rows[1].0 > rows[0].0);
        assert_eq!("%1", rows[0].1);
        assert_eq!("%2", rows[1].1);
        assert_eq!(
            "first reason",
            serde_json::from_str::<Value>(&rows[0].2).unwrap()["reason"]
        );
        let meta: Value = serde_json::from_str(&rows[1].2).unwrap();
        assert_eq!("second reason", meta["reason"]);
        assert_eq!("first reason", meta["previous_block_reason"]);
    }
}

#[cfg(test)]
mod completion_actor_tests {
    use crate::tasks::{FieldEdit, Tasks};
    #[tokio::test]
    async fn audit_identity_cannot_replace_the_original_completion_or_metadata_actor() {
        let dir = crate::testsupport::dir("task-audit-completion-actor");
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        for actor in [None, Some("%legacy-pane")] {
            let task = tasks.add("global", "attribution", "").await.unwrap();
            tasks
                .update_with_status_and_raw_block_reason_audit(
                    &task.id,
                    FieldEdit {
                        priority: Some("high".into()),
                        assignees: Some(vec!["%2".into()]),
                        ..Default::default()
                    },
                    Some("done"),
                    actor,
                    None,
                    None,
                    Some("service:task-api"),
                )
                .await
                .unwrap();
            assert_eq!(
                actor,
                tasks.get(&task.id).await.unwrap().completed_by.as_deref()
            );
            let connection = rusqlite::Connection::open(tasks.path()).unwrap();
            for event in ["completed", "priority"] {
                let stored: Option<String> = connection
                    .query_row(
                        "SELECT actor FROM task_events WHERE task_id=?1 AND event=?2",
                        rusqlite::params![task.id, event],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(actor, stored.as_deref());
            }
            let assigned: String = connection
                .query_row(
                    "SELECT actor FROM task_events WHERE task_id=?1 AND event='assigned'",
                    [&task.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!("service:task-api", assigned);
        }
    }
}
