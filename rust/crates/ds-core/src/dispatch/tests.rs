//! Ported from tests/test_dunk_dispatch.py (LanePlanTests, DeriveFromBoardTests) and
//! tests/test_dunk_turn_end_dispatch.py (ClaimedWorkTests, TurnEndPolicyTests), same names and
//! order. NOT in this slice: QueueTests, TickTests (the sqlite Queue and `tick`, slice 5b),
//! TurnEndChainTests (a real Flock.turn_end chain) and CliReportTests (the CLI printer).

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
