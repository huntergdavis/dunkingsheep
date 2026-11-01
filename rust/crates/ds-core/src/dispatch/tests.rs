//! Ported from tests/test_dunk_dispatch.py (LanePlanTests, DeriveFromBoardTests) and
//! tests/test_dunk_turn_end_dispatch.py (ClaimedWorkTests, TurnEndPolicyTests), same names and
//! order. Slice 5b adds QueueTests and TickTests (test_dunk_dispatch.py), NextJobPlanTests
//! (test_dunk_next_job_plan.py) and RetryPolicyTests (test_dunk_next_job_wiring.py). NOT ported:
//! TurnEndChainTests (a real Flock.turn_end chain), CliReportTests (the CLI printer) and the rest
//! of test_dunk_next_job_wiring.py (the NextJobs class) and test_dispatch_feeds_reviews.py (a
//! whole Flock).

use super::*;

const PANE: &str = "w1:p1";

fn ids(values: &[Value], key: &str) -> Vec<String> {
    values
        .iter()
        .map(|v| v[key].as_str().unwrap_or_default().to_string())
        .collect()
}

fn s(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|v| (*v).to_string()).collect()
}

// -- LanePlanTests ----------------------------------------------------------------------

fn pane(pane_id: &str) -> Value {
    json!({"pane_id": pane_id, "agent_name": pane_id, "agent_status": "idle"})
}

fn plan(panes: &[Value], states: &[(&str, &str, Option<&str>)], unread: &[(&str, i64)]) -> Value {
    let states: BTreeMap<String, (String, Option<String>)> = states
        .iter()
        .map(|(id, state, reason)| {
            (
                (*id).to_string(),
                ((*state).to_string(), reason.map(str::to_string)),
            )
        })
        .collect();
    let unread: BTreeMap<String, i64> = unread
        .iter()
        .map(|(id, n)| ((*id).to_string(), *n))
        .collect();
    lane_plan(
        panes,
        &|p| {
            let (state, reason) = &states[p["pane_id"].as_str().unwrap()];
            json!({"state": state, "reason": reason})
        },
        &|p| {
            unread
                .get(p["pane_id"].as_str().unwrap())
                .copied()
                .unwrap_or(0)
        },
        None,
    )
}

#[test]
fn test_a_ready_lane_with_unread_mail_has_an_assignment_already() {
    let plan = plan(
        &[pane("w1:p1")],
        &[("w1:p1", "ready", None)],
        &[("w1:p1", 3)],
    );
    assert_eq!(plan["ready"], json!([]));
    assert_eq!(plan["has_mail"][0]["unread"], 3);
}

#[test]
fn test_blocked_dead_and_working_are_never_ready() {
    let plan = plan(
        &[pane("a"), pane("b"), pane("c")],
        &[
            ("a", "blocked", Some("context full")),
            ("b", "dead", Some("dead session")),
            ("c", "working", Some("working")),
        ],
        &[],
    );
    assert_eq!(plan["ready"], json!([]));
    assert_eq!(plan["blocked"][0]["reason"], "context full");
    assert_eq!(plan["dead"][0]["reason"], "dead session");
    assert_eq!(plan["working"].as_array().unwrap().len(), 1);
}

#[test]
fn test_an_unknown_state_is_reported_as_blocked_not_ready() {
    let plan = plan(
        &[pane("a")],
        &[("a", "unknown", Some("pane unreadable"))],
        &[],
    );
    assert_eq!(plan["ready"], json!([]));
    assert_eq!(plan["blocked"][0]["reason"], "pane unreadable");
}

// -- DeriveFromBoardTests ----------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn board_task(
    id: &str,
    status: &str,
    priority: &str,
    depends: &[&str],
    leases: Vec<Value>,
    block_reason: Option<&str>,
    title: Option<&str>,
    created: f64,
) -> Value {
    json!({"id": id, "status": status, "priority": priority, "depends_on": depends, "leases": leases,
        "assignees": [], "block_reason": block_reason,
        "title": title.map_or_else(|| format!("task {id}"), str::to_string), "created_at": created})
}

fn open(id: &str) -> Value {
    board_task(id, "open", "medium", &[], vec![], None, None, 1.0)
}

#[test]
fn test_open_unblocked_unclaimed_tasks_become_items_in_priority_order() {
    let (items, coarse) = derive_board_items(
        &[
            board_task("t1", "open", "low", &[], vec![], None, None, 3.0),
            board_task("t2", "open", "high", &[], vec![], None, None, 2.0),
            board_task("t3", "open", "urgent", &[], vec![], None, None, 9.0),
        ],
        &BTreeSet::new(),
        None,
        90,
    );
    assert_eq!(ids(&items, "task_id"), ["t3", "t2", "t1"]);
    assert!(coarse.is_empty());
    assert!(items.iter().all(|item| item["derived"] == true));
    assert_eq!(dispatch_text(&items[1]), "[standby t2] task t2");
}

#[test]
fn test_everything_that_is_not_dispatchable_is_skipped() {
    let (items, _) = derive_board_items(
        &[
            board_task(
                "held",
                "open",
                "medium",
                &[],
                vec![],
                Some("HELD FOR OWNER DECISION: wait for owner"),
                None,
                1.0,
            ),
            board_task(
                "blocked",
                "open",
                "medium",
                &[],
                vec![],
                Some("waiting on vendor"),
                None,
                1.0,
            ),
            board_task(
                "dep",
                "open",
                "medium",
                &["unfinished"],
                vec![],
                None,
                None,
                1.0,
            ),
            board_task(
                "leased",
                "open",
                "medium",
                &[],
                vec![json!({"pane_id": "w1:p1"})],
                None,
                None,
                1.0,
            ),
            board_task(
                "working",
                "in_progress",
                "medium",
                &[],
                vec![],
                None,
                None,
                1.0,
            ),
            board_task("done", "done", "medium", &[], vec![], None, None, 1.0),
            open("recent"),
        ],
        &s(&["recent"]),
        None,
        90,
    );
    assert!(items.is_empty(), "{items:?}");
}

#[test]
fn test_a_dependency_that_is_done_no_longer_blocks() {
    let (items, _) = derive_board_items(
        &[
            board_task("first", "done", "medium", &[], vec![], None, None, 1.0),
            board_task(
                "second",
                "open",
                "medium",
                &["first"],
                vec![],
                None,
                None,
                1.0,
            ),
        ],
        &BTreeSet::new(),
        None,
        90,
    );
    assert_eq!(ids(&items, "task_id"), ["second"]);
}

#[test]
fn test_an_umbrella_title_is_flagged_for_splitting_not_queued_by_hand() {
    let long = "x".repeat(120);
    let (items, coarse) = derive_board_items(
        &[board_task(
            "big",
            "open",
            "medium",
            &[],
            vec![],
            None,
            Some(&long),
            1.0,
        )],
        &BTreeSet::new(),
        None,
        90,
    );
    assert_eq!(ids(&items, "task_id"), ["big"]);
    assert_eq!(
        coarse,
        ["big"],
        "a coarse task is a signal to split it, not to keep a hand list"
    );
}

// -- ClaimedWorkTests ---------------------------------------------------------------------

/// A task as the board decorates it. `claimed_by` is a live lease held by that pane.
fn task(
    id: &str,
    status: &str,
    priority: &str,
    assignees: &[&str],
    claimed_by: &[&str],
    depends: &[&str],
    block: Option<&str>,
) -> Value {
    let assigned: Vec<&str> = if assignees.is_empty() {
        claimed_by.to_vec()
    } else {
        assignees.to_vec()
    };
    json!({"id": id, "status": status, "priority": priority, "assignees": assigned,
        "claimed_by": claimed_by,
        "leases": claimed_by.iter().map(|p| json!({"pane_id": p, "expires_at": 9e9})).collect::<Vec<_>>(),
        "depends_on": depends, "block_reason": block, "title": format!("task {id}"), "created_at": 1.0})
}

fn claimed(id: &str, by: &[&str]) -> Value {
    task(id, "in_progress", "medium", &[], by, &[], None)
}

#[test]
fn test_a_task_the_lane_has_claimed_is_the_one_it_is_carried() {
    assert_eq!(
        ids(&claimed_tasks(&[claimed("t9", &[PANE])], PANE), "id"),
        ["t9"]
    );
    assert_eq!(
        claimed_work(&[claimed("t9", &[PANE])], PANE).unwrap()["id"],
        "t9"
    );
}

#[test]
fn test_an_assigned_open_programme_task_earns_no_nudge() {
    // The ruling's case: an umbrella sits assigned-and-open for days.
    let programme = task("programme", "open", "urgent", &[PANE], &[], &[], None);
    assert!(claimed_tasks(std::slice::from_ref(&programme), PANE).is_empty());
    let plan = turn_end_plan(&TurnEnd {
        pane_id: PANE,
        tasks: &[programme],
        unread: Some(0),
        now: Some(10_000.0),
        ..TurnEnd::default()
    });
    assert_eq!(
        (plan["action"].as_str(), plan["reason"].as_str()),
        (Some("skip"), Some("no_open_work"))
    );
}

#[test]
fn test_in_progress_without_a_lease_is_not_claimed_either() {
    // `claimed_by == pane` is the condition; setting status alone is not claiming.
    let unclaimed = task("t1", "in_progress", "medium", &[PANE], &[], &[], None);
    assert!(claimed_tasks(&[unclaimed], PANE).is_empty());
}

#[test]
fn test_priority_then_age_orders_the_claimed_tasks() {
    let got = claimed_tasks(
        &[
            task("low", "in_progress", "low", &[], &[PANE], &[], None),
            task("urgent", "in_progress", "urgent", &[], &[PANE], &[], None),
        ],
        PANE,
    );
    assert_eq!(ids(&got, "id"), ["urgent", "low"]);
}

#[test]
fn test_blocked_unfinished_done_and_foreign_tasks_are_not_claimed_work() {
    let cases = [
        (
            "blocked",
            task(
                "b",
                "in_progress",
                "medium",
                &[],
                &[PANE],
                &[],
                Some("HELD FOR OWNER DECISION"),
            ),
        ),
        (
            "dep-gated",
            task(
                "d",
                "in_progress",
                "medium",
                &[],
                &[PANE],
                &["unfinished"],
                None,
            ),
        ),
        ("done", task("x", "done", "medium", &[], &[PANE], &[], None)),
        (
            "cancelled",
            task("y", "cancelled", "medium", &[], &[PANE], &[], None),
        ),
        (
            "another lane's lease",
            task("z", "in_progress", "medium", &[], &["w1:p2"], &[], None),
        ),
        (
            "assigned to me, leased to another lane",
            task(
                "zz",
                "in_progress",
                "medium",
                &[PANE],
                &["w1:p2"],
                &[],
                None,
            ),
        ),
    ];
    for (label, candidate) in cases {
        assert!(claimed_tasks(&[candidate], PANE).is_empty(), "{label}");
    }
}

#[test]
fn test_a_finished_dependency_no_longer_blocks() {
    let got = claimed_work(
        &[
            task("first", "done", "medium", &[], &[PANE], &[], None),
            task(
                "second",
                "in_progress",
                "medium",
                &[],
                &[PANE],
                &["first"],
                None,
            ),
        ],
        PANE,
    );
    assert_eq!(got.unwrap()["id"], "second");
}

// -- TurnEndPolicyTests -------------------------------------------------------------------

fn turn_plan(tasks: &[Value], unread: Option<i64>, adjust: impl FnOnce(&mut TurnEnd<'_>)) -> Value {
    let mut input = TurnEnd {
        pane_id: PANE,
        tasks,
        unread,
        now: Some(10_000.0),
        ..TurnEnd::default()
    };
    adjust(&mut input);
    turn_end_plan(&input)
}

fn outcome(plan: &Value) -> (Option<&str>, Option<&str>) {
    (plan["action"].as_str(), plan["reason"].as_str())
}

#[test]
fn test_a_lane_holding_work_and_holding_no_mail_is_prompted() {
    let plan = turn_plan(&[claimed("t7", &[PANE])], Some(0), |_| {});
    assert_eq!(plan["action"], "nudge");
    assert_eq!(plan["trigger"], "turn.end");
    assert_eq!(plan["item"]["id"], "t7");
}

#[test]
fn test_a_lane_with_no_open_work_is_left_alone() {
    let plan = turn_plan(
        &[task("t7", "done", "medium", &[PANE], &[], &[], None)],
        Some(0),
        |_| {},
    );
    assert_eq!(outcome(&plan), (Some("skip"), Some("no_open_work")));
}

#[test]
fn test_a_lane_that_already_has_unread_mail_is_not_fed_twice() {
    let plan = turn_plan(&[claimed("t7", &[PANE])], Some(2), |_| {});
    assert_eq!(outcome(&plan), (Some("skip"), Some("unread_mail")));
}

#[test]
fn test_an_unreadable_mailbox_is_a_skip_not_a_guess() {
    let plan = turn_plan(&[claimed("t7", &[PANE])], None, |_| {});
    assert_eq!(outcome(&plan), (Some("skip"), Some("unread_unknown")));
}

#[test]
fn test_the_same_pane_is_not_prompted_twice_in_a_row() {
    let plan = turn_plan(&[claimed("t7", &[PANE])], Some(0), |i| {
        i.last_nudge_at = Some(9_990.0);
        i.min_gap_s = Some(30.0);
    });
    assert_eq!(outcome(&plan), (Some("skip"), Some("cooldown")));
}

#[test]
fn test_the_same_task_is_not_re_prompted_on_every_turn() {
    let plan = turn_plan(&[claimed("t7", &[PANE])], Some(0), |i| {
        i.task_nudges = BTreeMap::from([("t7".to_string(), 9_000.0)]);
        i.task_cooldown_s = Some(1800.0);
    });
    assert_eq!(outcome(&plan), (Some("skip"), Some("task_cooldown")));
}

#[test]
fn test_another_held_task_is_still_prompted_when_one_was_just_mentioned() {
    let plan = turn_plan(
        &[claimed("t7", &[PANE]), claimed("t8", &[PANE])],
        Some(0),
        |i| {
            i.task_nudges = BTreeMap::from([("t7".to_string(), 9_000.0)]);
            i.task_cooldown_s = Some(1800.0);
        },
    );
    assert_eq!(plan["action"], "nudge");
    assert_eq!(plan["item"]["id"], "t8");
}

#[test]
fn test_a_different_task_is_prompted_once_the_pane_gap_has_passed() {
    // t7 was prompted 1000 s ago; the lane now holds t8, which was never prompted.
    let plan = turn_plan(&[claimed("t8", &[PANE])], Some(0), |i| {
        i.last_nudge_at = Some(9_000.0);
        i.min_gap_s = Some(30.0);
        i.task_nudges = BTreeMap::from([("t7".to_string(), 9_000.0)]);
        i.task_cooldown_s = Some(1800.0);
    });
    assert_eq!(plan["action"], "nudge");
    assert_eq!(plan["item"]["id"], "t8");
}

#[test]
fn test_the_prompt_says_why_it_arrived_and_which_task_it_is() {
    let mut t7 = claimed("t7", &[PANE]);
    t7["title"] = json!("vitals row colour");
    let text = turn_end_text(&t7);
    assert!(text.starts_with("[turn-end t7] vitals row colour"));
    assert!(text.contains("trigger: turn.end"));
    assert!(text.contains("task complete t7"));
}

// -- beyond the Python tests --------------------------------------------------------------

#[test]
fn test_lane_plan_honours_the_allowlist_and_skips_paneless_rows() {
    let panes = [pane("a"), pane("b"), json!({"agent_name": "x"})];
    let allow = s(&["a"]);
    let got = lane_plan(&panes, &|_| json!({"state": "ready"}), &|_| 0, Some(&allow));
    assert_eq!(ids(got["ready"].as_array().unwrap(), "pane_id"), ["a"]);
}

#[test]
fn test_an_unknown_state_without_a_reason_says_unknown_state() {
    let got = lane_plan(&[pane("a")], &|_| json!({"state": "weird"}), &|_| 0, None);
    assert_eq!(got["blocked"][0]["reason"], "unknown state");
}

#[test]
fn test_turn_end_text_exact() {
    let text = turn_end_text(&claimed("t7", &[PANE]));
    assert_eq!(
        text,
        "[turn-end t7] task t7\ntrigger: turn.end - you ended your turn still holding this task (in_progress, priority medium).\nCarry on, or close it out:\n  dunkingsheep task note t7 --reason \"<what you did / did NOT verify>\"\n  dunkingsheep task complete t7"
    );
}

#[test]
fn test_no_pane_is_a_named_skip_and_dispatch_text_without_a_task_id() {
    let plan = turn_end_plan(&TurnEnd::default());
    assert_eq!(outcome(&plan), (Some("skip"), Some("no_pane")));
    assert_eq!(
        dispatch_text(&json!({"brief": "  a   b "})),
        "[standby] a b"
    );
}

// -- QueueTests (5b) --------------------------------------------------------------------

fn queue() -> (tempfile::TempDir, Queue) {
    let dir = tempfile::tempdir().expect("tempdir");
    let queue = Queue::open(dir.path().join("d.db")).expect("queue");
    (dir, queue)
}

fn item_ids(items: &[Value]) -> Vec<i64> {
    items.iter().map(|i| i["id"].as_i64().unwrap()).collect()
}

#[test]
fn test_items_are_ordered_and_a_taken_item_leaves_the_queue() {
    let (_dir, queue) = queue();
    let first = queue.add("first", Some("t6"), None).unwrap();
    queue.add("second", None, None).unwrap();
    let first_id = first["id"].as_i64().unwrap();
    assert_eq!(
        item_ids(&queue.items(true).unwrap()),
        [first_id, first_id + 1]
    );
    queue.take(first_id, "w1:p1", Some("i9"), None).unwrap();
    assert_eq!(queue.items(true).unwrap().len(), 1);
    let all = queue.items(false).unwrap();
    assert_eq!(all.len(), 2);
    let taken = all.iter().find(|i| i["id"] == first_id).unwrap();
    assert_eq!(
        (taken["taken_by"].as_str(), taken["sent_message"].as_str()),
        (Some("w1:p1"), Some("i9"))
    );
}

#[test]
fn test_an_item_cannot_be_taken_twice() {
    let (_dir, queue) = queue();
    let id = queue.add("only once", None, None).unwrap()["id"]
        .as_i64()
        .unwrap();
    queue.take(id, "w1:p1", None, None).unwrap();
    let error = queue.take(id, "w1:p2", None, None).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!("standby item {id} was already taken")
    );
}

#[test]
fn test_a_lane_reserved_item_is_not_offered_to_another_lane() {
    let (_dir, queue) = queue();
    let reserved = queue.add("for poolrad", None, Some("wW:p1")).unwrap();
    assert_eq!(queue.next_for("w9:pM").unwrap(), None);
    assert_eq!(
        queue.next_for("wW:p1").unwrap().unwrap()["id"],
        reserved["id"]
    );
}

#[test]
fn test_an_empty_brief_is_refused() {
    let (_dir, queue) = queue();
    assert_eq!(
        queue.add("   ", None, None).unwrap_err().to_string(),
        "a standby item needs a one-line brief"
    );
}

#[test]
fn test_remove_and_missing_items() {
    let (_dir, queue) = queue();
    let id = queue.add("x", None, None).unwrap()["id"].as_i64().unwrap();
    assert_eq!(queue.remove(id).unwrap(), json!({"removed": id}));
    assert_eq!(
        queue.remove(id).unwrap_err().to_string(),
        format!("no standby item {id}")
    );
    assert_eq!(
        queue.take(99, "w1:p1", None, None).unwrap_err().to_string(),
        "no standby item 99"
    );
}

// -- TickTests (5b) ---------------------------------------------------------------------

struct TickRig {
    _dir: tempfile::TempDir,
    queue: Queue,
    state: std::path::PathBuf,
    sent: Vec<(String, String)>,
}

impl TickRig {
    fn new() -> TickRig {
        let dir = tempfile::tempdir().expect("tempdir");
        let queue = Queue::open(dir.path().join("d.db")).expect("queue");
        let state = dir.path().join("state.json");
        TickRig {
            _dir: dir,
            queue,
            state,
            sent: Vec::new(),
        }
    }

    fn tick(
        &mut self,
        panes: &[Value],
        states: &[(&str, &str, Option<&str>)],
        now: f64,
        dry_run: bool,
        fail: Option<&str>,
        derived: &[Value],
    ) -> Value {
        let states: BTreeMap<String, (String, Option<String>)> = states
            .iter()
            .map(|(id, s, r)| ((*id).to_string(), ((*s).to_string(), r.map(str::to_string))))
            .collect();
        let sent = &mut self.sent;
        tick(
            &self.queue,
            panes,
            &|p| {
                let (state, reason) = &states[p["pane_id"].as_str().unwrap()];
                json!({"state": state, "reason": reason})
            },
            &|_| 0,
            &mut |pane_id, text| {
                if let Some(message) = fail {
                    return Err(message.to_string());
                }
                sent.push((pane_id.to_string(), text.to_string()));
                Ok(json!(format!("i{}", sent.len())))
            },
            &Tick {
                now: Some(now),
                dry_run,
                lanes: None,
                state_path: Some(&self.state),
                derived,
            },
        )
        .expect("tick")
    }
}

const READY: (&str, &str, Option<&str>) = ("w1:p1", "ready", None);

#[test]
fn test_only_the_ready_lane_is_fed_and_the_text_carries_the_task() {
    let mut rig = TickRig::new();
    rig.queue
        .add("advance strict C to 250", Some("t6"), None)
        .unwrap();
    let state = rig.tick(
        &[pane("w1:p1"), pane("w1:p2")],
        &[READY, ("w1:p2", "blocked", Some("context full"))],
        1000.0,
        false,
        None,
        &[],
    );
    assert_eq!(
        rig.sent,
        [(
            "w1:p1".to_string(),
            "[standby t6] advance strict C to 250".to_string()
        )]
    );
    assert_eq!(state["dispatched_count"], 1);
    assert_eq!(state["dispatched_1h"], 1);
}

#[test]
fn test_a_dry_run_sends_nothing_and_takes_nothing() {
    let mut rig = TickRig::new();
    rig.queue.add("do not send me", None, None).unwrap();
    let state = rig.tick(&[pane("w1:p1")], &[READY], 1000.0, true, None, &[]);
    assert!(rig.sent.is_empty());
    assert_eq!(state["dispatched"][0]["sent"], false);
    assert_eq!(
        rig.queue.items(true).unwrap().len(),
        1,
        "a dry run must not consume the item"
    );
}

#[test]
fn test_a_refused_send_is_recorded_and_leaves_the_item_queued() {
    let mut rig = TickRig::new();
    rig.queue.add("keep me", None, None).unwrap();
    let state = rig.tick(
        &[pane("w1:p1")],
        &[READY],
        1000.0,
        false,
        Some("typing hold"),
        &[],
    );
    assert!(state["dispatched"][0]["error"]
        .as_str()
        .unwrap()
        .contains("typing hold"));
    assert_eq!(
        rig.queue.items(true).unwrap().len(),
        1,
        "a failed send must not lose the assignment"
    );
}

#[test]
fn test_a_derived_board_item_fills_an_idle_lane_without_a_hand_item() {
    let mut rig = TickRig::new();
    let (derived, _coarse) = derive_board_items(
        &[board_task(
            "t44",
            "open",
            "medium",
            &[],
            vec![],
            None,
            Some("fold the flags"),
            1.0,
        )],
        &BTreeSet::new(),
        None,
        90,
    );
    let state = rig.tick(&[pane("w1:p1")], &[READY], 1000.0, false, None, &derived);
    assert_eq!(
        rig.sent,
        [(
            "w1:p1".to_string(),
            "[standby t44] fold the flags".to_string()
        )]
    );
    assert_eq!(state["derived_dispatched"], 1);
    assert_eq!(state["derived_available"], 1);
    assert_eq!(
        state["queue_empty"], false,
        "'queue empty' must mean no open work, not 'nobody topped it up'"
    );
}

#[test]
fn test_an_empty_queue_with_idle_lanes_is_stated_not_hidden() {
    let mut rig = TickRig::new();
    let state = rig.tick(
        &[pane("w1:p1"), pane("w1:p2")],
        &[READY, ("w1:p2", "ready", None)],
        1000.0,
        false,
        None,
        &[],
    );
    assert_eq!(state["queue_empty"], true);
    assert_eq!(state["idle_unfilled"], json!(["w1:p1", "w1:p2"]));
    assert!(summary(&state).contains("standby queue is empty"));
}

#[test]
fn test_dispatched_this_hour_counts_real_sends_and_expires() {
    let mut rig = TickRig::new();
    for index in 0..2 {
        rig.queue.add(&format!("item {index}"), None, None).unwrap();
        rig.tick(
            &[pane("w1:p1")],
            &[READY],
            1000.0 + f64::from(index),
            false,
            None,
            &[],
        );
    }
    let state = rig.tick(&[pane("w1:p1")], &[READY], 1100.0, false, None, &[]);
    assert_eq!(state["dispatched_1h"], 2);
    let later = rig.tick(&[pane("w1:p1")], &[READY], 9999.0, false, None, &[]);
    assert_eq!(later["dispatched_1h"], 0);
}

#[test]
fn test_the_state_file_is_indented_json_with_a_history_and_summary_lines() {
    let mut rig = TickRig::new();
    rig.queue.add("x", Some("t1"), None).unwrap();
    let state = rig.tick(
        &[pane("a"), pane("b")],
        &[("a", "ready", None), ("b", "dead", Some("dead session"))],
        1000.0,
        false,
        None,
        &[],
    );
    let text = std::fs::read_to_string(&rig.state).unwrap();
    assert!(
        text.starts_with("{\n  \"tick_at\": 1000.0,\n  \"dry_run\": false,"),
        "{text}"
    );
    assert_eq!(load_state(&rig.state)["history"][0]["task_id"], "t1");
    assert_eq!(dispatched_in_window(&rig.state, 1100.0, 3600.0), 1);
    assert_eq!(
        summary(&state),
        "idle lanes: 0\ndispatched this hour: 1\nblocked lanes: 0\ndead lanes: 1 (b)"
    );
}

// -- NextJobPlanTests and RetryPolicyTests (5b) -----------------------------------------

const NJ_PANE: &str = "%4";

fn nj_task(
    id: &str,
    status: &str,
    priority: &str,
    assignees: &[&str],
    created: f64,
    extra: Value,
) -> Value {
    let mut row = json!({"id": id, "status": status, "priority": priority, "assignees": assignees, "created_at": created});
    for (key, value) in extra.as_object().cloned().unwrap_or_default() {
        row[key] = value;
    }
    row
}

fn nj(id: &str) -> Value {
    nj_task(id, "open", "high", &[NJ_PANE], 1000.0, json!({}))
}

fn nj_plan(tasks: &[Value], adjust: impl FnOnce(&mut NextJob<'_>)) -> Value {
    let mut input = NextJob {
        trigger: "completion",
        pane_id: NJ_PANE,
        tasks,
        unread: Some(0),
        now: Some(2000.0),
        ..NextJob::default()
    };
    adjust(&mut input);
    next_job_plan(&input)
}

fn reason(plan: &Value) -> Option<&str> {
    plan["reason"].as_str()
}

#[test]
fn test_a_completion_with_assigned_open_work_claims_the_next_task() {
    let out = nj_plan(&[nj("t1")], |_| {});
    assert_eq!(
        (
            out["action"].as_str(),
            out["task_id"].as_str(),
            out["trigger"].as_str()
        ),
        (Some("claim"), Some("t1"), Some("completion"))
    );
}

#[test]
fn test_the_same_decision_applies_to_a_turn_end() {
    assert_eq!(
        nj_plan(&[nj("t1")], |i| i.trigger = "turn.end")["action"],
        "claim"
    );
}

#[test]
fn test_priority_then_age_orders_the_choice() {
    let out = nj_plan(
        &[
            nj_task("t-low", "open", "low", &[NJ_PANE], 1.0, json!({})),
            nj_task("t-urgent", "open", "urgent", &[NJ_PANE], 5.0, json!({})),
            nj_task("t-high", "open", "high", &[NJ_PANE], 2.0, json!({})),
        ],
        |_| {},
    );
    assert_eq!(out["task_id"], "t-urgent");
}

#[test]
fn test_no_assigned_work_records_explicit_no_work() {
    let out = nj_plan(
        &[nj_task("t1", "open", "high", &["%15"], 1000.0, json!({}))],
        |_| {},
    );
    assert_eq!(
        (out["action"].as_str(), reason(&out)),
        (Some("skip"), Some("no_assigned_work"))
    );
}

#[test]
fn test_blocked_and_dependency_gated_work_is_not_eligible() {
    let held = nj_task(
        "t1",
        "open",
        "high",
        &[NJ_PANE],
        1000.0,
        json!({"block_reason": "waiting on owner"}),
    );
    assert_eq!(reason(&nj_plan(&[held], |_| {})), Some("no_assigned_work"));
    let gated = nj_task(
        "t1",
        "open",
        "high",
        &[NJ_PANE],
        1000.0,
        json!({"blocked_by": ["t0"]}),
    );
    assert_eq!(reason(&nj_plan(&[gated], |_| {})), Some("no_assigned_work"));
}

#[test]
fn test_completed_or_cancelled_work_is_not_eligible() {
    for status in ["done", "cancelled", "in_progress"] {
        let row = nj_task("t1", status, "high", &[NJ_PANE], 1000.0, json!({}));
        assert_eq!(
            reason(&nj_plan(&[row], |_| {})),
            Some("no_assigned_work"),
            "{status}"
        );
    }
}

#[test]
fn test_unread_mail_is_a_skip() {
    assert_eq!(
        reason(&nj_plan(&[nj("t1")], |i| i.unread = Some(2))),
        Some("unread_mail")
    );
}

#[test]
fn test_unknown_unread_is_a_skip_not_a_zero() {
    assert_eq!(
        reason(&nj_plan(&[nj("t1")], |i| i.unread = None)),
        Some("unread_unknown")
    );
}

#[test]
fn test_a_pending_delivery_is_a_skip() {
    assert_eq!(
        reason(&nj_plan(&[nj("t1")], |i| i.pending_delivery = true)),
        Some("pending_delivery")
    );
}

#[test]
fn test_no_pane_is_a_skip() {
    assert_eq!(
        reason(&nj_plan(&[nj("t1")], |i| i.pane_id = "")),
        Some("no_pane")
    );
}

#[test]
fn test_dispatch_controls_are_inputs_to_the_shared_decision() {
    for trigger in ["completion", "turn.end", "restart"] {
        for why in ["disabled", "not_allowed", "readiness_unknown"] {
            let out = nj_plan(&[nj("t1")], |i| {
                i.trigger = trigger;
                i.dispatch_reason = Some(why);
            });
            assert_eq!(
                (out["action"].as_str(), reason(&out)),
                (Some("skip"), Some(why))
            );
        }
    }
}

#[test]
fn test_a_recent_prompt_holds_the_lane() {
    let out = nj_plan(&[nj("t1")], |i| {
        i.last_nudge_at = Some(1999.0);
        i.min_gap_s = Some(60.0);
    });
    assert_eq!(reason(&out), Some("cooldown"));
}

#[test]
fn test_a_task_prompted_recently_is_not_offered_again() {
    let out = nj_plan(&[nj("t1")], |i| {
        i.task_nudges = BTreeMap::from([("t1".to_string(), 1999.0)]);
        i.task_cooldown_s = Some(300.0);
    });
    assert_eq!(reason(&out), Some("task_cooldown"));
}

#[test]
fn test_overload_is_a_rate_not_a_scheduler() {
    // 50 completions in one second on the same lane: the cooldown is what bounds it, and the
    // decision is the same for the first and the fiftieth - no dependence on host timing.
    let first = nj_plan(&[nj("t1")], |i| {
        i.last_nudge_at = None;
        i.now = Some(2000.0);
    });
    let second = nj_plan(&[nj("t1")], |i| {
        i.last_nudge_at = Some(2000.0);
        i.now = Some(2000.0);
        i.min_gap_s = Some(60.0);
    });
    assert_eq!(first["action"], "claim");
    assert_eq!(reason(&second), Some("cooldown"));
}

#[test]
fn test_a_restart_reaches_the_same_decision_from_the_same_state() {
    // Same tasks/leases, fresh process, no in-memory history: same answer.
    let tasks = [nj("t1")];
    assert_eq!(nj_plan(&tasks, |_| {}), nj_plan(&tasks, |_| {}));
}

#[test]
fn test_retry_schedule_is_a_pure_rate_input() {
    let action = |attempt, last_at, now, max, gap| {
        delivery_retry_plan(attempt, last_at, now, max, gap)["action"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(action(0, None, 100.0, 4, None), "attempt");
    assert_eq!(action(2, Some(100.0), 159.0, 4, Some(30.0)), "wait");
    assert_eq!(action(2, Some(100.0), 160.0, 4, Some(30.0)), "attempt");
    assert_eq!(action(4, Some(100.0), 10000.0, 4, None), "stop");
    assert_eq!(
        delivery_retry_plan(2, Some(100.0), 159.0, 4, Some(30.0))["next_at"],
        json!(160.0)
    );
}
