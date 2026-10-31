//! Ports of tests/test_dunk_roles.py: the desk's narrow grant (t1170) and the allowlist shape.

use super::*;

#[test]
fn the_desk_has_a_narrow_grant_of_reversible_queue_actions_and_nothing_destructive() {
    // owner decision 2026-10-31: block, unblock and update. Assign is NOT granted (its
    // handler refuses a desk anyway, t1172), so the allowlist must not promise it.
    for verb in ["task_block", "task_unblock", "task_update"] {
        assert!(allowed("desk", verb), "{verb}");
    }
    for verb in [
        "task_assign",
        "task_cancel",
        "task_delete",
        "task_complete",
        "task_next",
        "respawn_pane",
        "start_agent",
        "send_keys",
        "send_text",
        "lane_set",
        "close",
        "shutdown",
        "create_workspace",
        "superuser_add",
        "retire_pane",
        "add_dunk",
        "stop_all",
        "gh_token",
        "token_issue",
    ] {
        assert!(!allowed("desk", verb), "the desk must not have {verb}");
    }
}

#[test]
fn the_rest_of_the_allowlist_shape() {
    assert!(allowed("owner", "shutdown"));
    assert!(!allowed("web", "shutdown"));
    assert!(allowed("web", "send_text"));
    assert!(!allowed("desk", "send_text"));
    assert!(!allowed("desk", "inbox_send"));
    assert!(!allowed("desk", "task_complete"));
    assert!(allowed("desk", "task_add"));
    assert!(!allowed("planner", "inbox_send"));
    assert!(!allowed("anonymous", "task_next"));
    assert!(allowed("anonymous", "task_list"));
    assert!(!allowed("worker", "task_assign"));
    assert!(!allowed("system", "send_text"));
}

#[test]
fn every_role_may_read_and_an_unknown_role_gets_only_that() {
    for role in [
        "desk",
        "planner",
        "worker",
        "reviewer",
        "system",
        "anonymous",
        "nobody",
    ] {
        assert!(allowed(role, "task_list"), "{role}");
        assert!(allowed(role, "status"), "{role}");
    }
    assert!(
        !allowed("nobody", "task_add"),
        "an unknown role is read-only, like ALLOW.get(role, READ_ONLY)"
    );
}

#[test]
fn a_worker_and_a_reviewer_share_one_grant() {
    for verb in WORKER_EXTRA {
        assert_eq!(allowed("worker", verb), allowed("reviewer", verb), "{verb}");
        assert!(allowed("worker", verb), "{verb}");
    }
    assert!(allowed("worker", "task_next") && allowed("reviewer", "task_complete"));
}

#[test]
fn the_tables_have_the_pythons_sizes() {
    // Sizes of the sets in dunk_roles.py when these tables were generated (t1170). A
    // one-sided edit to either language changes a count here or fails a Python test.
    assert_eq!(47, READ_ONLY.len());
    assert_eq!(28, WORKER_EXTRA.len());
    assert_eq!(30, SYSTEM_EXTRA.len());
    assert_eq!(15, DESK_EXTRA.len());
    assert_eq!(3, PLANNER_EXTRA.len());
    assert_eq!(30, WEB_DENIED.len());
}

#[test]
fn the_desk_grant_is_exactly_the_reviewed_set() {
    let mut desk: Vec<&str> = DESK_EXTRA.to_vec();
    desk.sort_unstable();
    assert_eq!(
        vec![
            "inbox_ack",
            "inbox_fetch",
            "inbox_read",
            "owner_conversation",
            "owner_outbox",
            "owner_poll",
            "owner_reply",
            "owner_take",
            "read_pane",
            "report_run",
            "task_add",
            "task_block",
            "task_note",
            "task_unblock",
            "task_update",
        ],
        desk
    );
}

#[test]
fn web_may_do_everything_but_the_fleet_shaping_verbs() {
    for verb in WEB_DENIED {
        assert!(!allowed("web", verb), "{verb}");
        assert!(allowed("owner", verb), "owner may call {verb}");
    }
}
