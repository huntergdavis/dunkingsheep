//! Ports of tests/test_dunk_next_job_handoff.py and the `active_handoffs` half of
//! tests/test_dunk_cancelled_handoff.py (t461). The `advance_next_job` halves of those
//! files need `next_job_plan` and are NOT ported here.

use super::*;

const PANE: &str = "%4";

async fn store(tag: &str) -> Tasks {
    let dir = std::env::temp_dir().join(format!("ds-nj-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tasks.db");
    let _ = std::fs::remove_file(&path);
    Tasks::open(&path).await.unwrap()
}

async fn events(store: &Tasks, task: &str, event: &str) -> Vec<(Option<String>, Option<String>)> {
    let (task, event) = (task.to_string(), event.to_string());
    store
        .writer
        .call(move |conn| {
            let mut stmt = conn
                .prepare("SELECT actor, meta FROM task_events WHERE task_id = ? AND event = ? ORDER BY at")
                .map_err(db::map_sqlite)?;
            let rows = stmt
                .query_map(params![task, event], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(db::map_sqlite)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(db::map_sqlite)
        })
        .await
        .unwrap()
}

fn state(s: &str) -> Option<Map<String, Value>> {
    let mut m = Map::new();
    m.insert("state".into(), json!(s));
    Some(m)
}

async fn handoff(store: &Tasks, task: &str, st: &str, message_id: &str) {
    store
        .record_next_job_handoff(
            task,
            PANE,
            Some("t0"),
            Some(message_id),
            1,
            Some(st),
            None,
            state(st),
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn the_record_uses_a_unique_event_name_and_carries_the_shape() {
    let store = store("shape").await;
    let a = store
        .add("global", "A: the task that finished", "")
        .await
        .unwrap();
    let b = store.add("global", "B: the next job", "").await.unwrap();
    let out = store
        .record_next_job_handoff(
            &b.id,
            PANE,
            Some(&a.id),
            Some("m8454"),
            1,
            Some("delivered"),
            None,
            None,
        )
        .await
        .unwrap();
    let rows = events(&store, &b.id, "next_job_handoff").await;
    assert_eq!(
        1,
        rows.len(),
        "the event name is unique, so a PR handoff cannot produce it"
    );
    let meta: Value = serde_json::from_str(rows[0].1.as_deref().unwrap()).unwrap();
    assert_eq!(json!(a.id), meta["from_task"]);
    assert_eq!(json!("m8454"), meta["message_id"]);
    assert_eq!(json!(1), meta["attempt"]);
    assert_eq!(Some(PANE.to_string()), rows[0].0);
    assert_eq!(Some("m8454".to_string()), out.message_id);
}

#[tokio::test]
async fn a_record_without_the_shape_does_not_count() {
    // Clause 4: a note with the right name and no shape is not a handoff.
    let store = store("noshape").await;
    let b = store.add("global", "B", "").await.unwrap();
    store
        .record_next_job_handoff(&b.id, PANE, None, None, 0, Some("pending"), None, None)
        .await
        .unwrap();
    let rows = events(&store, &b.id, "next_job_handoff").await;
    let meta: Value = serde_json::from_str(rows[0].1.as_deref().unwrap()).unwrap();
    assert!(meta["from_task"].is_null() && meta["message_id"].is_null());
    assert!(
        store.next_job_handoffs(None).await.unwrap().is_empty(),
        "not an executable intent"
    );
}

#[tokio::test]
async fn the_plain_handoff_name_is_untouched() {
    let store = store("plain").await;
    let a = store.add("global", "A", "").await.unwrap();
    store
        .note(&a.id, "handoff", "%35", Some("PR handed to reviewer"))
        .await
        .unwrap();
    assert_eq!(1, events(&store, &a.id, "handoff").await.len());
    assert!(events(&store, &a.id, "next_job_handoff").await.is_empty());
}

#[tokio::test]
async fn the_latest_state_of_each_handoff_wins_and_keeps_its_first_position() {
    let store = store("latest").await;
    let a = store.add("global", "A", "").await.unwrap();
    let b = store.add("global", "B", "").await.unwrap();
    handoff(&store, &a.id, "pending", "m1").await;
    handoff(&store, &b.id, "pending", "m2").await;
    handoff(&store, &a.id, "sent", "m1").await;
    let all = store.next_job_handoffs(Some(PANE)).await.unwrap();
    let ids: Vec<_> = all
        .iter()
        .map(|h| h["message_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(vec!["m1", "m2"], ids);
    assert_eq!(json!("sent"), all[0]["state"]);
    assert!(store
        .next_job_handoffs(Some("%99"))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn a_cancelled_historical_intent_does_not_count_as_active() {
    let store = store("cancelled").await;
    let t = store.add("global", "historical intent", "").await.unwrap();
    handoff(&store, &t.id, "cancelled", "m-hist").await;
    let owned = vec![t.id.clone()];
    assert!(store
        .active_handoffs(PANE, &owned)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn every_other_state_still_blocks_and_an_unknown_one_fails_closed() {
    let store = store("blocking").await;
    let owned_task = store.add("global", "owned", "").await.unwrap();
    let owned = vec![owned_task.id.clone()];
    for st in [
        "pending",
        "sending",
        "sent",
        "started",
        "failed",
        "uncertain",
        "undeliverable",
        "CANCELLED",
        " cancelled ",
        "",
        "something-new",
    ] {
        handoff(&store, &owned_task.id, st, "m-x").await;
        let active = store.active_handoffs(PANE, &owned).await.unwrap();
        assert_eq!(1, active.len(), "state {st:?} must block");
    }
}

#[tokio::test]
async fn only_handoffs_naming_an_owned_task_count() {
    let store = store("owned").await;
    let mine = store.add("global", "mine", "").await.unwrap();
    let other = store.add("global", "other", "").await.unwrap();
    handoff(&store, &other.id, "pending", "m-other").await;
    assert!(store
        .active_handoffs(PANE, &[mine.id.clone()])
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn update_next_job_appends_progress_and_keeps_the_chain() {
    let store = store("update").await;
    let t = store.add("global", "job", "").await.unwrap();
    handoff(&store, &t.id, "pending", "m1").await;
    let h = store.next_job_handoffs(Some(PANE)).await.unwrap().remove(0);
    let mut changes = Map::new();
    changes.insert("state".into(), json!("sending"));
    changes.insert("attempt".into(), json!(2));
    let updated = store
        .update_next_job(&h, None, changes)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(json!("sending"), updated["state"]);
    let now = store.next_job_handoffs(Some(PANE)).await.unwrap();
    assert_eq!(1, now.len());
    assert_eq!(json!("sending"), now[0]["state"]);
    assert_eq!(json!(2), now[0]["attempt"]);
    assert_eq!(
        json!("t0"),
        now[0]["from_task"],
        "details carry the chain forward"
    );
    assert_eq!(
        2,
        events(&store, &t.id, "next_job_handoff").await.len(),
        "append-only"
    );
}

#[tokio::test]
async fn update_next_job_with_a_stale_expectation_writes_nothing() {
    let store = store("cas").await;
    let t = store.add("global", "job", "").await.unwrap();
    handoff(&store, &t.id, "pending", "m1").await;
    let h = store.next_job_handoffs(Some(PANE)).await.unwrap().remove(0);
    let mut expected = Map::new();
    expected.insert("state".into(), json!("sent"));
    let mut changes = Map::new();
    changes.insert("state".into(), json!("started"));
    assert!(store
        .update_next_job(&h, Some(expected), changes)
        .await
        .unwrap()
        .is_none());
    assert_eq!(1, events(&store, &t.id, "next_job_handoff").await.len());
}

#[tokio::test]
async fn updating_an_unknown_handoff_is_a_refusal() {
    let store = store("unknown").await;
    let mut ghost = Map::new();
    ghost.insert("pane_id".into(), json!(PANE));
    ghost.insert("message_id".into(), json!("nope"));
    assert!(store
        .update_next_job(&ghost, None, Map::new())
        .await
        .is_err());
}
