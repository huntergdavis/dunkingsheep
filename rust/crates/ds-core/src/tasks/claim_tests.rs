//! T12 slice 1 tests: the claimability rules, the refusal reasons, leases and heartbeat.

use super::*;
use crate::tasks::FieldEdit;

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

async fn set_kind_author(tasks: &Tasks, id: &str, kind: &'static str, author: &'static str) {
    let id = id.to_string();
    tasks
        .writer
        .call(move |conn| {
            conn.execute(
                "UPDATE tasks SET kind = ?, author = ? WHERE id = ?",
                rusqlite::params![kind, author, id],
            )
            .map_err(crate::db::map_sqlite)?;
            Ok(())
        })
        .await
        .unwrap();
}

fn claimed_id(outcome: &ClaimOutcome) -> String {
    match outcome {
        ClaimOutcome::Claimed { task, .. } => task.id.clone(),
        ClaimOutcome::Refused { refusal, .. } => panic!("refused: {refusal:?}"),
    }
}

#[tokio::test]
async fn claim_next_takes_the_highest_priority_then_oldest_and_records_the_claim() {
    let tasks = store("claim-order").await;
    let low = tasks.add("global", "low", "").await.unwrap();
    let urgent = tasks.add("global", "urgent", "").await.unwrap();
    tasks
        .update(
            &urgent.id,
            FieldEdit {
                priority: Some("urgent".into()),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    let got = tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap();
    assert_eq!(claimed_id(&got), urgent.id);
    let ClaimOutcome::Claimed { task, .. } = got else {
        unreachable!()
    };
    assert_eq!(task.status, "in_progress");
    assert_eq!(decode_list(Some(&task.assignees)), vec!["%1"]);
    assert_eq!(tasks.leases_for(&urgent.id, None).await.unwrap().len(), 1);
    assert_eq!(
        tasks.events(&urgent.id).await,
        2,
        "the priority edit and the claim"
    );
    // t145: a pane holding in-progress work is refused a second task.
    match tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Refused { refusal, .. } => {
            assert_eq!(refusal.reason, "busy");
            assert_eq!(refusal.task_ids, vec![urgent.id.clone()]);
        }
        other => panic!("{other:?}"),
    }
    let second = tasks
        .claim_next(
            "global",
            "%1",
            ClaimOptions {
                allow_busy: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(claimed_id(&second), low.id);
}

#[tokio::test]
async fn claim_next_skips_blocked_wrong_kind_and_own_author_and_says_why() {
    let tasks = store("claim-filters").await;
    let dep = tasks.add("global", "dep", "").await.unwrap();
    let blocked = tasks.add("global", "blocked", "").await.unwrap();
    tasks
        .update(
            &blocked.id,
            FieldEdit {
                depends_on: Some(vec![dep.id.clone()]),
                ..Default::default()
            },
            None,
        )
        .await
        .unwrap();
    // `dep` is open and claimable; `blocked` waits on it.
    let review = tasks.add("global", "review", "").await.unwrap();
    set_kind_author(&tasks, &review.id, "review", "%9").await;
    let none = tasks
        .claim_next(
            "global",
            "%2",
            ClaimOptions {
                kinds: Some(vec!["work".into()]),
                exclude_author: None,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        claimed_id(&none),
        dep.id,
        "a worker never pulls a review; blocked is skipped"
    );
    let own = tasks
        .claim_next(
            "global",
            "%3",
            ClaimOptions {
                kinds: Some(vec!["review".into()]),
                exclude_author: Some("%9".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    match own {
        // The reason is computed over ALL candidates, like the Python's `_refusal`: `dep` is
        // now owned by %2, so that is what the refusal reports.
        ClaimOutcome::Refused { refusal, .. } => assert_eq!(refusal.reason, "owned_elsewhere"),
        other => panic!("the author never reviews its own head: {other:?}"),
    }
    let theirs = tasks
        .claim_next(
            "global",
            "%4",
            ClaimOptions {
                kinds: Some(vec!["review".into()]),
                exclude_author: Some("%5".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(claimed_id(&theirs), review.id);
}

#[tokio::test]
async fn claim_next_scope_mine_and_refusals_are_named() {
    let tasks = store("claim-mine").await;
    assert!(tasks
        .claim_next("mine", "", ClaimOptions::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("requires a known caller pane"));
    assert!(tasks
        .claim_next("global", "", ClaimOptions::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("requires a caller pane id"));
    assert!(tasks
        .claim_next("nowhere", "%1", ClaimOptions::default())
        .await
        .is_err());
    match tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Refused { refusal, .. } => assert_eq!(refusal.reason, "empty"),
        other => panic!("{other:?}"),
    }
    let theirs = tasks.add("global", "theirs", "").await.unwrap();
    let mine = tasks.add("global", "mine", "").await.unwrap();
    tasks.assign(&theirs.id, &["%7"], "add").await.unwrap();
    tasks.assign(&mine.id, &["%1"], "add").await.unwrap();
    match tasks
        .claim_next("global", "%2", ClaimOptions::default())
        .await
        .unwrap()
    {
        ClaimOutcome::Refused { refusal, .. } => {
            assert_eq!(refusal.reason, "owned_elsewhere");
            assert_eq!(
                refusal.owned_by,
                Some(vec!["%1".to_string(), "%7".to_string()])
            );
        }
        other => panic!("{other:?}"),
    }
    let got = tasks
        .claim_next("mine", "%1", ClaimOptions::default())
        .await
        .unwrap();
    assert_eq!(
        claimed_id(&got),
        mine.id,
        "'mine' takes only tasks naming this pane"
    );
}
