//! Tests for the push sweep's pure decisions (dunk_core.py, t1053/t975/t1010/t1029).

use super::*;

fn mail(subject: &str, body: &str, created_at: Option<f64>) -> Mail {
    Mail {
        subject: Some(subject.to_string()),
        body: Some(body.to_string()),
        created_at,
    }
}

#[test]
fn no_unread_count_means_no_gate() {
    assert!(!unread_mail(
        0,
        &[mail("hi", "x", Some(1_000.0))],
        1_000.0,
        600.0
    ));
}

#[test]
fn task_announcements_never_gate_in_subject_or_body() {
    let now = 1_000.0;
    assert!(!unread_mail(
        2,
        &[mail("TASK t1 [high] x", "", Some(now))],
        now,
        600.0
    ));
    assert!(!unread_mail(
        2,
        &[mail("", "TASK t1 [high] x", Some(now))],
        now,
        600.0
    ));
    // a message that merely mentions TASK later does gate
    assert!(unread_mail(
        1,
        &[mail("re: TASK t1", "", Some(now))],
        now,
        600.0
    ));
}

#[test]
fn mail_older_than_the_grace_window_stops_gating() {
    let now = 2_000.0;
    assert!(unread_mail(
        1,
        &[mail("hi", "", Some(now - 600.0))],
        now,
        600.0
    ));
    assert!(!unread_mail(
        1,
        &[mail("hi", "", Some(now - 601.0))],
        now,
        600.0
    ));
}

#[test]
fn a_missing_or_zero_timestamp_reads_as_fresh() {
    let now = 2_000.0;
    assert!(unread_mail(1, &[mail("hi", "", None)], now, 600.0));
    assert!(unread_mail(1, &[mail("hi", "", Some(0.0))], now, 600.0));
}

#[test]
fn one_gating_message_among_skipped_ones_gates() {
    let now = 2_000.0;
    let rows = [
        mail("TASK t1", "", Some(now)),
        mail("old", "", Some(now - 9_999.0)),
        mail("fresh", "", Some(now - 5.0)),
    ];
    assert!(unread_mail(3, &rows, now, 600.0));
}

fn proc(comm: &str, state: &str) -> ChildProc {
    ChildProc {
        comm: comm.to_string(),
        state: state.to_string(),
    }
}

#[test]
fn a_direct_child_agent_is_alive_without_a_tree_walk() {
    let entries = [proc("bash", "S"), proc("claude", "S")];
    let alive = lane_agent_alive(Some(&entries), || panic!("must not walk the tree"));
    assert_eq!(Some(true), alive);
}

#[test]
fn a_zombie_agent_is_not_alive_and_the_tree_decides() {
    let entries = [proc("claude", "Z")];
    assert_eq!(
        Some(false),
        lane_agent_alive(Some(&entries), || Some(false))
    );
    assert_eq!(Some(true), lane_agent_alive(Some(&entries), || Some(true)));
}

#[test]
fn a_deep_agent_behind_a_wrapper_is_found_by_the_walk() {
    // `npx claude` -> node -> claude: only the descendant walk sees it (review t1029)
    let entries = [proc("node", "S")];
    assert_eq!(Some(true), lane_agent_alive(Some(&entries), || Some(true)));
}

#[test]
fn a_bare_shell_is_dead_and_an_unknown_table_or_failed_walk_is_not() {
    let shell = [proc("sh", "S")];
    assert_eq!(Some(false), lane_agent_alive(Some(&shell), || Some(false)));
    assert_eq!(None, lane_agent_alive(None, || panic!("no table, no walk")));
    assert_eq!(None, lane_agent_alive(Some(&shell), || None));
}

fn lanes(panes: &[&str]) -> BTreeSet<String> {
    panes.iter().map(|pane| pane.to_string()).collect()
}

#[test]
fn followers_keep_their_scope_and_registered_workers_and_reviewers_join() {
    let mut followers = BTreeMap::new();
    followers.insert("%1".to_string(), "%1".to_string());
    let registry = lanes(&["%1", "%2", "%3", "%4", "%5"]);
    let roles = |pane: &str| {
        match pane {
            "%1" => "worker",
            "%2" => "worker",
            "%3" => "reviewer",
            "%4" => "desk",
            _ => "anonymous",
        }
        .to_string()
    };
    let targets = push_targets(&followers, Some(&registry), roles);
    assert_eq!(
        Some(&("%1".to_string(), Via::Follower)),
        targets.get("%1"),
        "a follower keeps its own scope even if also registered"
    );
    assert_eq!(
        Some(&(TASK_GLOBAL.to_string(), Via::Registered)),
        targets.get("%2")
    );
    assert_eq!(
        Some(&(TASK_GLOBAL.to_string(), Via::Registered)),
        targets.get("%3")
    );
    assert!(!targets.contains_key("%4"), "a desk pane is never fed");
    assert!(
        !targets.contains_key("%5"),
        "a demoted/reused pane id is never fed (t975)"
    );
}

#[test]
fn an_unavailable_registry_leaves_followers_only() {
    let mut followers = BTreeMap::new();
    followers.insert("%1".to_string(), "global".to_string());
    let targets = push_targets(&followers, None, |_| panic!("no registry, no role lookups"));
    assert_eq!(1, targets.len());
}

#[test]
fn the_constants_are_the_pythons() {
    assert_eq!(["working", "blocked"], BUSY_STATUSES);
    assert_eq!(600.0, UNREAD_MAIL_GRACE_S);
    assert_eq!(24, AGENT_NAMES.len());
    assert!(DEAD_STATES.contains(&"Z"));
}
