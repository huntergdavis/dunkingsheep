//! `dunk_roles.py`: the role allowlist and `allowed()` (FLAT_FLEET.md 4.3).
//!
//! First slice of the roles port: the TABLES and the one decision function. Binding a pane
//! to a process, the registry file and service tokens (`register`, `role_of`, tokens) are
//! not here; they need `Flock` and `dunk_callers` (T30, T40).
//!
//! The tables below are GENERATED from the Python module's own sets (sorted, so a diff
//! against the Python is a diff of membership), at the commit that added the desk grant of
//! `task_block`, `task_unblock` and `task_update` (t1170; `task_assign` is deliberately NOT
//! granted - its handler refuses a desk). `tests` pin each table's size so a one-sided edit
//! to either language fails here or in the Python tests.

/// Commands every role may call (`READ_ONLY`).
pub const READ_ONLY: [&str; 47] = [
    "delivery",
    "delivery_capability",
    "dispatch",
    "doctor",
    "get_dunk",
    "get_message",
    "health",
    "help",
    "inbox_count",
    "inbox_stats",
    "inbox_status",
    "lane_register",
    "lanes",
    "list_dunks",
    "list_messages",
    "list_panes",
    "list_workspaces",
    "memory_context",
    "memory_recall",
    "memory_search",
    "memory_stats",
    "memory_status",
    "message_state",
    "metrics",
    "owner_receipts",
    "owner_stats",
    "pm_digest",
    "pm_routing",
    "poll",
    "poolrad_progress",
    "project_summary",
    "reply_stats",
    "report_list",
    "review_list",
    "roster",
    "state_latest",
    "status",
    "superuser_recover",
    "superuser_status",
    "task_followers",
    "task_list",
    "task_metrics",
    "task_read",
    "task_stats",
    "turn_end",
    "turn_start",
    "whoami",
];

/// `_WORKER` minus `READ_ONLY`: what a worker or reviewer adds (both share one set).
pub const WORKER_EXTRA: [&str; 28] = [
    "context_add",
    "context_supersede",
    "flush_box",
    "gh_token",
    "inbox_ack",
    "inbox_archive",
    "inbox_cancel",
    "inbox_delete",
    "inbox_fetch",
    "inbox_list",
    "inbox_read",
    "inbox_recover",
    "inbox_send",
    "read_pane",
    "send_keys",
    "send_text",
    "state_capture",
    "task_add",
    "task_block",
    "task_complete",
    "task_follow",
    "task_heartbeat",
    "task_next",
    "task_note",
    "task_release",
    "task_unblock",
    "task_unfollow",
    "task_update",
];

/// `ALLOW["system"]` minus `READ_ONLY`.
pub const SYSTEM_EXTRA: [&str; 30] = [
    "context_add",
    "flush_box",
    "gh_token",
    "inbox_fetch",
    "inbox_read",
    "inbox_send",
    "owner_conversation",
    "owner_delivered",
    "owner_ingest",
    "owner_outbox",
    "owner_poll",
    "owner_push",
    "owner_reply",
    "owner_take",
    "poolrad_sync",
    "read_pane",
    "recover",
    "report_run",
    "respawn_pane",
    "review_sync",
    "send_keys",
    "state_capture",
    "task_add",
    "task_assign",
    "task_block",
    "task_cancel",
    "task_note",
    "task_reap",
    "task_unblock",
    "task_update",
];

/// `ALLOW["desk"]` minus `READ_ONLY`.
pub const DESK_EXTRA: [&str; 15] = [
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
];

/// `ALLOW["planner"]` minus `READ_ONLY`.
pub const PLANNER_EXTRA: [&str; 3] = ["inbox_read", "task_add", "task_note"];

/// Verbs no token role may call: closing, shaping and stopping the fleet stays with a human.
pub const WEB_DENIED: [&str; 30] = [
    "add_dunk",
    "clear_box",
    "close",
    "create_tab",
    "create_workspace",
    "fire_dunk",
    "focus",
    "gh_token",
    "herdr",
    "lane_register",
    "lane_set",
    "memory_backfill",
    "memory_ingest",
    "remove_dunk",
    "rename",
    "respawn_pane",
    "retire_pane",
    "shutdown",
    "split_pane",
    "start_agent",
    "start_dunk",
    "stop_all",
    "stop_dunk",
    "superuser_add",
    "superuser_recover",
    "superuser_remove",
    "toggle_dunk",
    "token_issue",
    "token_revoke",
    "update_dunk",
];

/// `allowed(role, command)`: may `role` call `command`?
///
/// `owner` may call everything; `web` may call everything but `WEB_DENIED`; the lane roles
/// get `READ_ONLY` plus their own extras; an UNKNOWN role gets `READ_ONLY` only (the
/// Python's `ALLOW.get(role, READ_ONLY)`).
pub fn allowed(role: &str, command: &str) -> bool {
    let extra: &[&str] = match role {
        "owner" => return true,
        "web" => return !WEB_DENIED.contains(&command),
        "system" => &SYSTEM_EXTRA,
        "desk" => &DESK_EXTRA,
        "planner" => &PLANNER_EXTRA,
        "worker" | "reviewer" => &WORKER_EXTRA,
        _ => &[],
    };
    READ_ONLY.contains(&command) || extra.contains(&command)
}

#[cfg(test)]
#[path = "roles_tests.rs"]
mod roles_tests;
