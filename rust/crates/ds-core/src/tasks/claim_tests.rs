//! T12 slice 1 tests: the claimability rules, the refusal reasons, leases and heartbeat.

use super::*;

async fn store(tag: &str) -> Tasks {
    let dir = std::env::temp_dir().join(format!("ds-claim-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("tasks.db");
    let _ = std::fs::remove_file(&path);
    Tasks::open(&path).await.unwrap()
}

fn cand(id: &str, status: &str) -> Candidate {
    Candidate {
        id: id.into(),
        status: status.into(),
        ..Default::default()
    }
}

#[test]
fn an_unassigned_open_task_is_claimable_and_an_assigned_one_only_by_its_assignee() {
    let open = cand("t1", "open");
    assert!(claimable(&open, Some("%1")));
    let mut named = cand("t2", "open");
    named.assignees = vec!["%2".into()];
    assert!(!claimable(&named, Some("%1")), "F2: not by anyone else");
    assert!(
        claimable(&named, Some("%2")),
        "F2: a named assignee pulls its own task"
    );
    named.multi_assignee = true;
    assert!(claimable(&named, Some("%1")));
}

#[test]
fn a_leased_task_is_claimable_only_when_multi_and_never_by_its_holder() {
    let mut task = cand("t1", "in_progress");
    task.leased_by = vec!["%1".into()];
    assert!(!claimable(&task, Some("%2")));
    task.multi_assignee = true;
    assert!(claimable(&task, Some("%2")));
    assert!(!claimable(&task, Some("%1")), "already holds this one");
    let mut waiting = cand("t3", "open");
    waiting.allocation_waiting = true;
    assert!(!claimable(&waiting, Some("%1")));
    assert!(!claimable(&cand("t4", "done"), Some("%1")));
}

#[test]
fn refusal_reasons_are_empty_blocked_owned_elsewhere_leased_or_not_claimable() {
    assert_eq!(refusal(&[], None).reason, "empty");
    let mut blocked = cand("t1", "open");
    blocked.blocked_by = vec!["t0".into()];
    assert_eq!(refusal(&[blocked.clone()], Some("%1")).reason, "blocked");
    let mut owned = cand("t2", "open");
    owned.assignees = vec!["%9".into(), "%3".into()];
    let why = refusal(&[blocked.clone(), owned], Some("%1"));
    assert_eq!(why.reason, "owned_elsewhere");
    assert_eq!(why.task_ids, vec!["t2"]);
    assert_eq!(why.owned_by, Some(vec!["%3".to_string(), "%9".to_string()]));
    let mut leased = cand("t5", "in_progress");
    leased.leased_by = vec!["%7".into()];
    assert_eq!(
        refusal(&[leased, cand("t6", "open")], Some("%1")).reason,
        "leased"
    );
    assert_eq!(
        refusal(&[cand("t6", "done")], Some("%1")).reason,
        "not_claimable"
    );
}

#[tokio::test]
async fn expired_leases_are_ignored_and_busy_counts_only_in_progress_tasks() {
    let tasks = store("leases").await;
    let a = tasks.add("global", "a", "").await.unwrap();
    let b = tasks.add("global", "b", "").await.unwrap();
    tasks.claim_for_test(&a.id, "%1").await;
    tasks.claim_for_test(&b.id, "%1").await;
    tasks
        .set_status(&a.id, "in_progress", None, None)
        .await
        .unwrap();
    assert_eq!(tasks.leases_for(&a.id, None).await.unwrap().len(), 1);
    assert_eq!(tasks.leases_for_pane("%1", None).await.unwrap().len(), 2);
    let later = crate::clock::now_s() + 4000.0;
    assert!(tasks
        .leases_for(&a.id, Some(later))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(
        tasks
            .busy_task_ids("%1", crate::clock::now_s())
            .await
            .unwrap(),
        vec![a.id.clone()]
    );
}

#[tokio::test]
async fn heartbeat_extends_a_held_lease_and_refuses_a_stranger() {
    let tasks = store("heartbeat").await;
    let task = tasks.add("global", "t", "").await.unwrap();
    tasks.claim_for_test(&task.id, "%1").await;
    let (expires, secs) = tasks
        .heartbeat(
            &task.id,
            "%1",
            Some(60.0),
            Some((42, Some("s".into()), Some("pi".into()))),
        )
        .await
        .unwrap();
    assert_eq!(secs, 60.0);
    assert!(expires > crate::clock::now_s() && expires < crate::clock::now_s() + 61.0);
    let refused = tasks
        .heartbeat(&task.id, "%2", None, None)
        .await
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains(&format!(
            "no active lease for %2 on {}; claim it first",
            task.id
        )),
        "{refused}"
    );
}
