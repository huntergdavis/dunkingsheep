//! T12 slice 1 tests: the claimability rules, the refusal reasons, leases and heartbeat.

use super::*;
use crate::tasks::FieldEdit;

async fn store(tag: &str) -> crate::testsupport::Scratch<Tasks> {
    let dir = crate::testsupport::dir(&format!("claim-{tag}"));
    let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
    crate::testsupport::Scratch::new(tasks, dir)
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
        3,
        "created, the priority edit and the claim"
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

fn assignees_of(task: &Task) -> Vec<String> {
    decode_list(Some(&task.assignees))
}

#[tokio::test]
async fn release_undoes_only_the_assignee_the_claim_added() {
    let tasks = store("release").await;
    let plain = tasks.add("global", "plain", "").await.unwrap();
    let got = tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap();
    assert_eq!(claimed_id(&got), plain.id);
    let back = tasks.release(&plain.id, "%1").await.unwrap();
    assert_eq!(back.status, "open");
    assert!(
        assignees_of(&back).is_empty(),
        "t595: the claim's own assignee goes with it"
    );
    assert!(tasks.leases_for(&plain.id, None).await.unwrap().is_empty());
    // An assignment the owner made BEFORE the claim survives a release.
    let owned = tasks.add("global", "owned", "").await.unwrap();
    tasks.assign(&owned.id, &["%2"], "add").await.unwrap();
    tasks
        .claim_next(
            "global",
            "%2",
            ClaimOptions {
                allow_busy: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let back = tasks.release(&owned.id, "%2").await.unwrap();
    assert_eq!(assignees_of(&back), vec!["%2"]);
    assert_eq!(back.status, "open");
    assert!(tasks.release("t999", "%2").await.is_err());
}

#[tokio::test]
async fn reap_requeues_an_expired_claim_keeps_its_owner_and_records_why() {
    let tasks = store("reap").await;
    let task = tasks.add("global", "t", "").await.unwrap();
    tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap();
    let before = tasks.events(&task.id).await;
    // Not expired yet, and no caller identity: nothing is reaped.
    assert!(tasks.reap(None, None, None, None).await.unwrap().is_empty());
    let later = crate::clock::now_s() + 4000.0;
    let gone = tasks.reap(None, None, Some(later), None).await.unwrap();
    assert_eq!(
        gone,
        vec![Requeued {
            task_id: task.id.clone(),
            pane_id: "%1".into(),
            reason: "expired"
        }]
    );
    let back = tasks.get(&task.id).await.unwrap();
    assert_eq!(back.status, "open");
    assert_eq!(
        assignees_of(&back),
        vec!["%1"],
        "t145: the OWNER is kept, the claim ends"
    );
    assert_eq!(tasks.events(&task.id).await, before + 1);
}

#[tokio::test]
async fn reap_needs_a_live_pane_list_and_a_vanished_pane_alone_is_not_death() {
    let tasks = store("reap-live").await;
    let task = tasks.add("global", "t", "").await.unwrap();
    tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap();
    let fixture = tasks
        .reap(Some(vec![]), None, None, Some("fixture"))
        .await
        .unwrap_err()
        .to_string();
    assert!(fixture.contains("alive_source='live'"), "{fixture}");
    assert!(tasks.reap(Some(vec![]), None, None, None).await.is_err());
    // The pane is missing from a LIVE list, but the lease is neither expired nor has a dead caller.
    assert!(tasks
        .reap(Some(vec!["%9".into()]), None, None, Some("live"))
        .await
        .unwrap()
        .is_empty());
    assert_eq!(tasks.get(&task.id).await.unwrap().status, "in_progress");
}

#[tokio::test]
async fn reap_a_dead_caller_requeues_and_another_live_lease_keeps_the_task_in_progress() {
    let tasks = store("reap-caller").await;
    let task = tasks.add("global", "t", "").await.unwrap();
    let opts = ClaimOptions {
        caller: Some((4242, Some("s".into()), Some("pi".into()))),
        ..Default::default()
    };
    tasks.claim_next("global", "%1", opts).await.unwrap();
    tasks.claim_for_test(&task.id, "%2").await;
    let dead: CallerAlive = Box::new(|pid, _| pid != 4242);
    let none = tasks.reap(None, Some(dead), None, None).await.unwrap();
    assert!(
        none.is_empty(),
        "%2 still holds a live lease: released, not requeued"
    );
    assert_eq!(tasks.get(&task.id).await.unwrap().status, "in_progress");
    assert_eq!(tasks.leases_for(&task.id, None).await.unwrap().len(), 1);
    // The last lease goes: now it is a real requeue, with the reason.
    let later = crate::clock::now_s() + 4000.0;
    let gone = tasks.reap(None, None, Some(later), None).await.unwrap();
    assert_eq!(gone.len(), 1);
    assert_eq!(gone[0].reason, "expired");
    assert_eq!(tasks.get(&task.id).await.unwrap().status, "open");
}

#[tokio::test]
async fn two_claimers_on_one_file_cannot_both_win_the_same_exclusive_task() {
    let first = store("race").await;
    let second = Tasks::open(first.path()).await.unwrap();
    let task = first.add("global", "only one", "").await.unwrap();
    let (a, b) = tokio::join!(
        first.claim_next("global", "%1", ClaimOptions::default()),
        second.claim_next("global", "%2", ClaimOptions::default()),
    );
    let outcomes = [a.unwrap(), b.unwrap()];
    let winners: Vec<&ClaimOutcome> = outcomes
        .iter()
        .filter(|o| matches!(o, ClaimOutcome::Claimed { .. }))
        .collect();
    assert_eq!(
        winners.len(),
        1,
        "BEGIN IMMEDIATE: exactly one claimer wins: {outcomes:?}"
    );
    assert_eq!(first.leases_for(&task.id, None).await.unwrap().len(), 1);
    let loser = outcomes
        .iter()
        .find(|o| matches!(o, ClaimOutcome::Refused { .. }))
        .unwrap();
    let ClaimOutcome::Refused { refusal, .. } = loser else {
        unreachable!()
    };
    assert_eq!(
        refusal.reason, "owned_elsewhere",
        "the loser sees the winner's commit"
    );
}

#[tokio::test]
async fn an_empty_kinds_list_applies_no_filter_like_the_pythons_falsy_set() {
    let tasks = store("empty-kinds").await;
    let task = tasks.add("global", "t", "").await.unwrap();
    let got = tasks
        .claim_next(
            "global",
            "%1",
            ClaimOptions {
                kinds: Some(vec![]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        claimed_id(&got),
        task.id,
        "Python: `set(kinds) if kinds else None`"
    );
}

// ---- t1270: the claim/lease tests of tests/test_dunk_tasks.py, ported by NAME ------------
//
// Same function names as the Python (`TaskStoreTests`, here batch 1). Python's
// `add(GLOBAL, title, priority=, assignees=, depends_on=, multi_assignee=)` is `add_with`.
// NOT portable, and why, is listed at the end of this section.

async fn add_with(
    tasks: &Tasks,
    title: &str,
    priority: Option<&str>,
    assignees: &[&str],
    depends_on: &[&str],
    multi: bool,
) -> Task {
    let task = tasks.add("global", title, "").await.unwrap();
    let edit = FieldEdit {
        priority: priority.map(str::to_string),
        assignees: (!assignees.is_empty())
            .then(|| assignees.iter().map(|s| s.to_string()).collect()),
        depends_on: (!depends_on.is_empty())
            .then(|| depends_on.iter().map(|s| s.to_string()).collect()),
        multi_assignee: multi.then_some(true),
        ..Default::default()
    };
    if edit.priority.is_none() && edit.assignees.is_none() && edit.depends_on.is_none() && !multi {
        return task;
    }
    tasks.update(&task.id, edit, None).await.unwrap()
}

fn claimed_task(outcome: ClaimOutcome) -> Option<Task> {
    match outcome {
        ClaimOutcome::Claimed { task, .. } => Some(*task),
        ClaimOutcome::Refused { .. } => None,
    }
}

fn refused_with(outcome: ClaimOutcome) -> Refusal {
    match outcome {
        ClaimOutcome::Refused { refusal, .. } => refusal,
        ClaimOutcome::Claimed { task, .. } => panic!("expected a refusal, claimed {}", task.id),
    }
}

fn lease(seconds: f64) -> ClaimOptions {
    ClaimOptions {
        lease_seconds: Some(seconds),
        ..Default::default()
    }
}

fn live(panes: &[&str]) -> Option<Vec<String>> {
    Some(panes.iter().map(|p| p.to_string()).collect())
}

#[tokio::test]
async fn test_dependencies_gate_next_until_done() {
    let tasks = store("py-deps-gate").await;
    let first = add_with(&tasks, "first", Some("low"), &[], &[], false).await;
    let second = add_with(&tasks, "second", Some("urgent"), &[], &[&first.id], false).await;
    let claimed = tasks
        .claim_next("global", "w1:p1", lease(60.0))
        .await
        .unwrap();
    assert_eq!(first.id, claimed_task(claimed).unwrap().id);
    assert!(claimed_task(
        tasks
            .claim_next("global", "w2:p2", ClaimOptions::default())
            .await
            .unwrap()
    )
    .is_none());
    tasks
        .task_complete(&first.id, Some("w1:p1"), true)
        .await
        .unwrap();
    let claimed = tasks
        .claim_next("global", "w2:p2", lease(60.0))
        .await
        .unwrap();
    assert_eq!(second.id, claimed_task(claimed).unwrap().id);
}

#[tokio::test]
async fn test_atomic_claim_never_double_assigns() {
    let tasks = store("py-atomic").await;
    for index in 0..20 {
        tasks
            .add("global", &format!("task {index}"), "")
            .await
            .unwrap();
    }
    let path = tasks.path().to_path_buf();
    let (first, second) = (
        Tasks::open(&path).await.unwrap(),
        Tasks::open(&path).await.unwrap(),
    );
    async fn worker(tasks: Tasks, pane: &'static str) -> Vec<String> {
        let mut found = Vec::new();
        loop {
            let options = ClaimOptions {
                lease_seconds: Some(300.0),
                allow_busy: true,
                ..Default::default()
            };
            match claimed_task(tasks.claim_next("global", pane, options).await.unwrap()) {
                Some(task) => found.push(task.id),
                None => break,
            }
        }
        found
    }
    let (a, b) = tokio::join!(
        tokio::spawn(worker(first, "w1:p1")),
        tokio::spawn(worker(second, "w2:p2"))
    );
    let mut ids = a.unwrap();
    ids.extend(b.unwrap());
    assert_eq!(20, ids.len());
    let unique: std::collections::BTreeSet<_> = ids.iter().collect();
    assert_eq!(20, unique.len(), "a task was claimed twice");
}

#[tokio::test]
async fn test_lease_expiry_requeues() {
    let tasks = store("py-lease-expiry").await;
    let task = tasks.add("global", "work", "").await.unwrap();
    // The lease is long and the clock is passed to `reap`, so nothing here depends on how fast the
    // host runs (t1424/t1367: a 50 ms lease plus a sleep failed under load average ~46).
    tasks
        .claim_next("global", "w1:p1", lease(60.0))
        .await
        .unwrap();
    let start = crate::clock::now_s();
    assert!(tasks
        .reap(live(&["w1:p1"]), None, Some(start), Some("live"))
        .await
        .unwrap()
        .is_empty());
    let requeued = tasks
        .reap(live(&["w1:p1"]), None, Some(start + 120.0), Some("live"))
        .await
        .unwrap();
    let pairs: Vec<(String, &str)> = requeued
        .iter()
        .map(|i| (i.task_id.clone(), i.reason))
        .collect();
    assert_eq!(vec![(task.id.clone(), "expired")], pairs);
    let reopened = tasks.get(&task.id).await.unwrap();
    assert_eq!("open", reopened.status);
    // t145/owner2317: the claim made w1:p1 the owner, and expiry reopens the task for that
    // owner instead of deleting the assignment.
    assert_eq!(vec!["w1:p1"], assignees_of(&reopened));
}

#[tokio::test]
async fn test_busy_lane_is_refused_a_second_task() {
    let tasks = store("py-busy-refused").await;
    let first = add_with(&tasks, "mine", Some("urgent"), &["w1:p1"], &[], false).await;
    let second = add_with(&tasks, "second", Some("low"), &[], &[], false).await;
    let claimed = tasks
        .claim_next("global", "w1:p1", lease(300.0))
        .await
        .unwrap();
    assert_eq!(first.id, claimed_task(claimed).unwrap().id);
    let refused = refused_with(
        tasks
            .claim_next("global", "w1:p1", lease(300.0))
            .await
            .unwrap(),
    );
    assert_eq!("busy", refused.reason);
    assert_eq!(vec![first.id.clone()], refused.task_ids);
    let still = tasks.get(&second.id).await.unwrap();
    assert_eq!("open", still.status);
    assert!(assignees_of(&still).is_empty());
    assert!(tasks.leases_for(&second.id, None).await.unwrap().is_empty());
}

#[tokio::test]
async fn test_busy_lane_may_take_parallel_work_only_with_the_explicit_flag() {
    let tasks = store("py-busy-flag").await;
    let first = add_with(&tasks, "first", None, &[], &[], false).await;
    let second = add_with(&tasks, "second", Some("urgent"), &[], &[], false).await;
    tasks
        .claim_next("global", "w1:p1", lease(300.0))
        .await
        .unwrap();
    assert!(claimed_task(
        tasks
            .claim_next("global", "w1:p1", ClaimOptions::default())
            .await
            .unwrap()
    )
    .is_none());
    let allowed = tasks
        .claim_next(
            "global",
            "w1:p1",
            ClaimOptions {
                lease_seconds: Some(300.0),
                allow_busy: true,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    // The Python's `second` is the urgent one claimed first; the allowed claim takes `first`.
    assert_eq!(first.id, claimed_task(allowed).unwrap().id);
    let held: std::collections::BTreeSet<String> = tasks
        .leases_for_pane("w1:p1", None)
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.task_id)
        .collect();
    assert_eq!(
        std::collections::BTreeSet::from([first.id.clone(), second.id.clone()]),
        held
    );
}

#[tokio::test]
async fn test_refusal_reason_names_why_nothing_was_claimable() {
    let tasks = store("py-refusal-reason").await;
    assert_eq!(
        "empty",
        refused_with(
            tasks
                .claim_next("global", "w1:p1", ClaimOptions::default())
                .await
                .unwrap()
        )
        .reason
    );
    let task = add_with(&tasks, "owned", None, &["w2:p2"], &[], false).await;
    let refused = refused_with(
        tasks
            .claim_next("global", "w1:p1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!("owned_elsewhere", refused.reason);
    assert_eq!(Some(vec!["w2:p2".to_string()]), refused.owned_by);
    assert_eq!(task.id, refused.task_ids[0]);
    add_with(&tasks, "blocked", None, &[], &[&task.id], false).await;
    let refused = refused_with(
        tasks
            .claim_next("global", "w1:p1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(
        "owned_elsewhere", refused.reason,
        "an open task owned elsewhere still explains the refusal"
    );
    let only_blocked = store("py-refusal-reason-blocked").await;
    let gate = only_blocked.add("global", "gate", "").await.unwrap();
    only_blocked.block(&gate.id, "paused").await.unwrap();
    let waiting = add_with(&only_blocked, "waiting", None, &[], &[&gate.id], false).await;
    let refused = refused_with(
        only_blocked
            .claim_next("global", "w1:p1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!("blocked", refused.reason);
    assert_eq!(vec![waiting.id], refused.task_ids);
}

#[tokio::test]
async fn test_expired_lease_keeps_the_durable_assignee_and_blocks_other_lanes() {
    let tasks = store("py-expired-keeps-owner").await;
    let task = add_with(&tasks, "owned work", None, &["w1:p1"], &[], false).await;
    // Long lease + an explicit reap clock instead of a 50 ms lease and a sleep (t1424: flaky
    // under host load).
    tasks
        .claim_next("global", "w1:p1", lease(60.0))
        .await
        .unwrap();
    let requeued = tasks
        .reap(
            live(&["w1:p1"]),
            None,
            Some(crate::clock::now_s() + 120.0),
            Some("live"),
        )
        .await
        .unwrap();
    let pairs: Vec<(String, &str)> = requeued
        .iter()
        .map(|i| (i.task_id.clone(), i.reason))
        .collect();
    assert_eq!(vec![(task.id.clone(), "expired")], pairs);
    let reopened = tasks.get(&task.id).await.unwrap();
    assert_eq!("open", reopened.status);
    assert_eq!(
        vec!["w1:p1"],
        assignees_of(&reopened),
        "expiry erased the durable owner"
    );
    let stolen = refused_with(
        tasks
            .claim_next("global", "w2:p2", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(
        "owned_elsewhere", stolen.reason,
        "another lane took expired owned work"
    );
    assert_eq!(Some(vec!["w1:p1".to_string()]), stolen.owned_by);
    let back = claimed_task(
        tasks
            .claim_next("global", "w1:p1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(task.id, back.expect("the owner could not reclaim").id);
}

#[tokio::test]
async fn test_release_keeps_the_durable_assignee_and_blocks_other_lanes() {
    let tasks = store("py-release-keeps-owner").await;
    let task = add_with(&tasks, "owned work", None, &["w1:p1"], &[], false).await;
    tasks
        .claim_next("global", "w1:p1", lease(300.0))
        .await
        .unwrap();
    tasks.release(&task.id, "w1:p1").await.unwrap();
    let released = tasks.get(&task.id).await.unwrap();
    assert_eq!("open", released.status);
    assert_eq!(
        vec!["w1:p1"],
        assignees_of(&released),
        "release erased the durable owner"
    );
    assert!(claimed_task(
        tasks
            .claim_next("global", "w2:p2", ClaimOptions::default())
            .await
            .unwrap()
    )
    .is_none());
    let again = claimed_task(
        tasks
            .claim_next("global", "w1:p1", ClaimOptions::default())
            .await
            .unwrap(),
    );
    assert_eq!(task.id, again.unwrap().id);
}

#[tokio::test]
async fn test_pane_gone_alone_does_not_requeue() {
    let tasks = store("py-pane-gone").await;
    let task = tasks.add("global", "work", "").await.unwrap();
    // Legacy lease: no caller identity -> unattributable, not dead.
    tasks
        .claim_next("global", "w1:p1", lease(300.0))
        .await
        .unwrap();
    assert!(tasks
        .reap(live(&["w2:p2"]), None, None, Some("live"))
        .await
        .unwrap()
        .is_empty());
    // A live agent behind a vanished pane is still a live claim.
    tasks.release(&task.id, "w1:p1").await.unwrap();
    let options = ClaimOptions {
        lease_seconds: Some(300.0),
        caller: Some((42, Some("42".into()), Some("pi".into()))),
        ..Default::default()
    };
    tasks.claim_next("global", "w1:p1", options).await.unwrap();
    let alive: CallerAlive = Box::new(|_pid, _start| true);
    assert!(tasks
        .reap(live(&["w2:p2"]), Some(alive), None, Some("live"))
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_pane_gone_with_a_dead_caller_requeues() {
    let tasks = store("py-dead-caller").await;
    let task = tasks.add("global", "work", "").await.unwrap();
    let options = ClaimOptions {
        lease_seconds: Some(300.0),
        caller: Some((42, Some("42".into()), Some("pi".into()))),
        ..Default::default()
    };
    tasks.claim_next("global", "w1:p1", options).await.unwrap();
    let dead: CallerAlive = Box::new(|_pid, _start| false);
    let requeued = tasks
        .reap(live(&["w2:p2"]), Some(dead), None, Some("live"))
        .await
        .unwrap();
    let pairs: Vec<(String, &str)> = requeued
        .iter()
        .map(|i| (i.task_id.clone(), i.reason))
        .collect();
    assert_eq!(vec![(task.id, "caller_dead")], pairs);
}

#[tokio::test]
async fn test_heartbeat_extends_and_requires_a_lease() {
    let tasks = store("py-heartbeat").await;
    let task = tasks.add("global", "work", "").await.unwrap();
    tasks
        .claim_next("global", "w1:p1", lease(30.0))
        .await
        .unwrap();
    let (_expires, seconds) = tasks
        .heartbeat(&task.id, "w1:p1", Some(600.0), None)
        .await
        .unwrap();
    assert_eq!(600.0, seconds);
    assert!(tasks
        .heartbeat(&task.id, "w2:p2", None, None)
        .await
        .is_err());
}

#[tokio::test]
async fn test_multi_assignee_allows_parallel_claims() {
    let tasks = store("py-multi").await;
    let task = add_with(&tasks, "shared", None, &[], &[], true).await;
    let one = claimed_task(
        tasks
            .claim_next("global", "w1:p1", lease(60.0))
            .await
            .unwrap(),
    )
    .unwrap();
    let two = claimed_task(
        tasks
            .claim_next("global", "w2:p2", lease(60.0))
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(task.id, one.id);
    assert_eq!(task.id, two.id);
    assert_eq!(vec!["w1:p1", "w2:p2"], assignees_of(&two));
    // Both leases are live, so neither is reaped.
    assert!(tasks
        .reap(live(&["w1:p1", "w2:p2"]), None, None, Some("live"))
        .await
        .unwrap()
        .is_empty());
}

// NOT PORTED from TaskStoreTests (batch 1), with reasons:
//  - test_every_refusal_is_logged_with_the_pane_and_reason,
//    test_reap_advisory_names_the_sole_owner_when_that_owner_is_gone,
//    test_reap_advisory_is_silent_when_the_owner_is_alive: they assert on Python `logging`
//    records; the Rust claim.rs writes those lines with `eprintln!` (claim.rs:355, :601), so
//    a test cannot capture them without a logging crate (RUST_PORT.md 7 says `tracing`).
//  - test_claim_requires_a_pane: Python passes `None`; `claim_next(&str)` cannot be given
//    one, so the case does not exist in the Rust types.

// t872 (Python PR #166), ported by name: blocking ends the claim, and the reaper never
// resurrects a blocked task.

#[tokio::test]
async fn blocking_deletes_the_lease() {
    let tasks = store("t872-block").await;
    let task = tasks.add("global", "t", "").await.unwrap();
    tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap();
    assert_eq!(tasks.leases_for(&task.id, None).await.unwrap().len(), 1);
    tasks.block(&task.id, "waiting").await.unwrap();
    assert!(tasks.leases_for(&task.id, None).await.unwrap().is_empty());
    assert_eq!(tasks.get(&task.id).await.unwrap().status, "blocked");
}

#[tokio::test]
async fn the_reaper_does_not_reopen_a_blocked_task() {
    let tasks = store("t872-reap-blocked").await;
    let task = tasks.add("global", "t", "").await.unwrap();
    tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap();
    tasks.block(&task.id, "waiting").await.unwrap();
    // Put a lease back by hand, exactly as the old block() left it.
    tasks.claim_for_test(&task.id, "%1").await;
    let later = crate::clock::now_s() + 4000.0;
    tasks.reap(None, None, Some(later), None).await.unwrap();
    assert_eq!(
        tasks.get(&task.id).await.unwrap().status,
        "blocked",
        "t872: a blocked task stays blocked when its last lease is reaped"
    );
}

#[tokio::test]
async fn the_reaper_still_reopens_an_in_progress_task() {
    let tasks = store("t872-reap-working").await;
    let task = tasks.add("global", "t", "").await.unwrap();
    tasks
        .claim_next("global", "%1", ClaimOptions::default())
        .await
        .unwrap();
    let later = crate::clock::now_s() + 4000.0;
    tasks.reap(None, None, Some(later), None).await.unwrap();
    assert_eq!(
        tasks.get(&task.id).await.unwrap().status,
        "open",
        "the rule is not 'never reopen': a worked task whose claim expires goes back to open"
    );
}
