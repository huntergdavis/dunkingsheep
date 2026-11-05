//! Transactional owner decisions on the task actor.
use super::{db, json, params, DunkError, OptionalExtension, Tasks};
use rusqlite::TransactionBehavior;
use serde_json::Value;

fn options(reason: &str) -> Result<Vec<(u64, String)>, DunkError> {
    let Some((_, raw)) = reason.split_once("| options:") else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for entry in raw.split(';') {
        let (number, label) = entry
            .trim()
            .split_once('=')
            .ok_or_else(|| DunkError::Refused("malformed owner options".into()))?;
        let number = number
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| DunkError::Refused("malformed owner options".into()))?;
        let label = label.trim();
        if label.is_empty() || result.iter().any(|(n, _)| *n == number) {
            return Err(DunkError::Refused("malformed owner options".into()));
        }
        result.push((number, label.to_owned()));
    }
    Ok(result)
}

impl Tasks {
    /// Read the current question and its immutable blocked-event generation together.
    pub async fn owner_question(&self, id: &str, pane: Option<&str>) -> Result<Value, DunkError> {
        let id = id.to_owned();
        let pane = pane.map(str::to_owned);
        self.writer.call(move |conn| {
            let tx = conn.transaction().map_err(db::map_sqlite)?;
            let (status, scope, reason): (String, String, Option<String>) = tx.query_row("SELECT status,scope,block_reason FROM tasks WHERE id=?", [&id], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional().map_err(db::map_sqlite)?.ok_or_else(|| DunkError::Refused(format!("no task with id {id:?}")))?;
            if pane.as_deref().is_some_and(|p| !p.is_empty() && scope != "global" && scope != p) { return Err(DunkError::Refused("task is not visible to this caller".into())); }
            let reason = reason.filter(|r| status == "blocked" && r.starts_with("needs-owner:")).ok_or_else(|| DunkError::Refused("task has no owner question".into()))?;
            let version: i64 = tx.query_row("SELECT id FROM task_events WHERE task_id=? AND event='blocked' ORDER BY id DESC LIMIT 1", [&id], |r| r.get(0)).optional().map_err(db::map_sqlite)?.ok_or_else(|| DunkError::Refused("owner question has no version".into()))?;
            let choices = options(&reason)?.into_iter().map(|(number,label)| json!({"number":number,"label":label})).collect::<Vec<_>>();
            Ok(json!({"task_id":id,"question":reason.trim_start_matches("needs-owner:").trim(),"version":version,"options":choices}))
        }).await
    }

    /// Answer only the specific blocked generation identified by its blocked-event ID.
    pub async fn owner_answer(
        &self,
        id: &str,
        version: i64,
        option: Option<u64>,
        reply: Option<&str>,
        actor: Option<&str>,
        pane: Option<&str>,
    ) -> Result<Value, DunkError> {
        let id = id.to_owned();
        let reply = reply.map(str::to_owned);
        let actor = actor.map(str::to_owned);
        let pane = pane.map(str::to_owned);
        self.writer.call(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(db::map_sqlite)?;
            let (status, scope, priority, reason): (String, String, String, Option<String>) = tx.query_row(
                "SELECT status,scope,priority,block_reason FROM tasks WHERE id=?", [&id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))
                .optional().map_err(db::map_sqlite)?.ok_or_else(|| DunkError::Refused(format!("no task with id {id:?}")))?;
            if pane.as_deref().is_some_and(|p| !p.is_empty() && scope != "global" && scope != p) { return Err(DunkError::Refused("task is not visible to this caller".into())); }
            let generation: Option<i64> = tx.query_row("SELECT id FROM task_events WHERE task_id=? AND event='blocked' ORDER BY id DESC LIMIT 1", [&id], |r| r.get(0)).optional().map_err(db::map_sqlite)?;
            if generation != Some(version) { return Err(DunkError::Refused("owner question has changed".into())); }
            if status != "blocked" {
                let previous: Option<String> = tx.query_row("SELECT meta FROM task_events WHERE task_id=? AND event='owner_answer' ORDER BY id DESC LIMIT 1", [&id], |r| r.get(0)).optional().map_err(db::map_sqlite)?;
                if status == "open" && previous.as_deref().and_then(|s| serde_json::from_str::<Value>(s).ok()).is_some_and(|v| v["version"] == version && v["option_number"] == json!(option) && v["reply"] == json!(reply)) { return Ok(json!({"task_id":id,"version":version,"changed":false})); }
                return Err(DunkError::Refused("task has no matching blocked owner question".into()));
            }
            let reason = reason.filter(|v| v.starts_with("needs-owner:")).ok_or_else(|| DunkError::Refused("task has no owner question".into()))?;
            let choices = options(&reason)?;
            let answer = if choices.is_empty() {
                if option.is_some() { return Err(DunkError::Refused("owner question has no options".into())); }
                reply.as_deref().map(str::trim).filter(|s| !s.is_empty()).ok_or_else(|| DunkError::Refused("a reply is required".into()))?.to_owned()
            } else {
                if reply.is_some() { return Err(DunkError::Refused("select an owner option".into())); }
                choices.iter().find(|(n,_)| Some(*n)==option).map(|(_,label)|label.clone()).ok_or_else(|| DunkError::Refused("unknown owner option".into()))?
            };
            let now = crate::clock::now_s();
            let display = match option { Some(number) => format!("{number}={answer}"), None => answer.clone() };
            let meta = json!({"version":version,"option_number":option,"option_label":answer,"reply":reply,"reason":display}).to_string();
            tx.execute("INSERT INTO task_events (task_id,event,at,actor,scope,priority,status_from,status_to,meta) VALUES (?,'owner_answer',?,?,?,?, 'blocked','open',?)", params![id,now,actor,scope,priority,meta]).map_err(db::map_sqlite)?;
            tx.execute("UPDATE tasks SET status='open', block_reason=NULL, updated_at=? WHERE id=? AND status='blocked'", params![now,id]).map_err(db::map_sqlite)?;
            tx.execute("INSERT INTO task_events (task_id,event,at,actor,scope,priority,status_from,status_to) VALUES (?,'unblocked',?,?,?,?, 'blocked','open')", params![id,now,actor,scope,priority]).map_err(db::map_sqlite)?;
            tx.commit().map_err(db::map_sqlite)?;
            Ok(json!({"task_id":id,"version":version,"changed":true,"answer":answer}))
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn decisions_are_atomic_idempotent_and_reject_stale_generations() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let task = tasks.add("global", "decision", "").await.unwrap();
        tasks
            .block(&task.id, "needs-owner: choose | options: 1=one; 2=two")
            .await
            .unwrap();
        let version = tasks.owner_question(&task.id, None).await.unwrap()["version"]
            .as_i64()
            .unwrap();
        let (a, b) = tokio::join!(
            tasks.owner_answer(&task.id, version, Some(1), None, Some("owner"), None),
            tasks.owner_answer(&task.id, version, Some(2), None, Some("owner"), None)
        );
        assert_ne!(a.is_ok(), b.is_ok());
        let winner = if a.is_ok() { 1 } else { 2 };
        assert_eq!(
            tasks
                .owner_answer(&task.id, version, Some(winner), None, Some("owner"), None)
                .await
                .unwrap()["changed"],
            false
        );
        tasks
            .block(&task.id, "needs-owner: choose | options: 1=one; 2=two")
            .await
            .unwrap();
        assert!(tasks
            .owner_answer(&task.id, version, Some(winner), None, Some("owner"), None)
            .await
            .is_err());
        let next = tasks.owner_question(&task.id, None).await.unwrap()["version"]
            .as_i64()
            .unwrap();
        assert_ne!(version, next);
        assert!(tasks
            .owner_answer(&task.id, next, Some(3), None, Some("owner"), None)
            .await
            .is_err());
        assert_eq!(tasks.get(&task.id).await.unwrap().status, "blocked");
    }
    #[tokio::test]
    async fn freeform_privacy_and_malformed_options_fail_closed() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let task = tasks.add("%1", "decision", "").await.unwrap();
        tasks
            .block(&task.id, "needs-owner: what now?")
            .await
            .unwrap();
        assert!(tasks.owner_question(&task.id, Some("%2")).await.is_err());
        let q = tasks.owner_question(&task.id, Some("%1")).await.unwrap();
        assert_eq!(q["options"], json!([]));
        let version = q["version"].as_i64().unwrap();
        assert!(tasks
            .owner_answer(&task.id, version, None, Some(" "), Some("%1"), Some("%1"))
            .await
            .is_err());
        assert_eq!(
            tasks
                .owner_answer(
                    &task.id,
                    version,
                    None,
                    Some("proceed"),
                    Some("%1"),
                    Some("%1")
                )
                .await
                .unwrap()["answer"],
            "proceed"
        );
        tasks
            .block(
                &task.id,
                "needs-owner: choose | options: 1=one; 1=duplicate",
            )
            .await
            .unwrap();
        assert!(tasks.owner_question(&task.id, Some("%1")).await.is_err());
    }
    #[tokio::test]
    async fn owner_answer_rolls_back_all_writes_on_event_failure() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let task = tasks.add("global", "decision", "").await.unwrap();
        tasks
            .block(&task.id, "needs-owner: pick | options: 1=yes")
            .await
            .unwrap();
        let version = tasks.owner_question(&task.id, None).await.unwrap()["version"]
            .as_i64()
            .unwrap();
        tasks.writer.call(|conn| {
            conn.execute_batch("CREATE TRIGGER fail_owner_unblock BEFORE INSERT ON task_events WHEN NEW.event='unblocked' BEGIN SELECT RAISE(ABORT, 'injected'); END;").map_err(db::map_sqlite)?;
            Ok(())
        }).await.unwrap();
        assert!(tasks
            .owner_answer(&task.id, version, Some(1), None, Some("owner"), None)
            .await
            .is_err());
        assert_eq!(tasks.get(&task.id).await.unwrap().status, "blocked");
        let id = task.id.clone();
        let count: i64 = tasks
            .writer
            .call(move |conn| {
                conn.query_row(
                    "SELECT COUNT(*) FROM task_events WHERE task_id=? AND event='owner_answer'",
                    [&id],
                    |row| row.get(0),
                )
                .map_err(db::map_sqlite)
            })
            .await
            .unwrap();
        assert_eq!(count, 0);
    }
}
