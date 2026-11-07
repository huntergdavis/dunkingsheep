//! t3539: the desk inbox read marks only the messages its response carries (`desk_inbox_read`).
//! Before the fix the whole batch was marked read, then the desk clamp dropped or cut messages, so an
//! owner message could be lost. These tests pin the decision on constructed results.

use super::*;
use serde_json::json;

fn inbox(count: usize, body: usize, tasks: Option<serde_json::Value>) -> serde_json::Value {
    let messages: Vec<serde_json::Value> = (0..count)
        .map(|k| {
            json!({
                "message_id": format!("i{k}"),
                "mailbox": "w1:p1",
                "body": "b".repeat(body),
            })
        })
        .collect();
    let mut result = json!({"mailbox": "w1:p1", "messages": messages});
    if let Some(tasks) = tasks {
        result["tasks"] = tasks;
    }
    result
}

fn carried_ids(result: &serde_json::Value) -> Vec<String> {
    result["messages"]
        .as_array()
        .map(|rows| {
            rows.iter()
                .map(|row| row["message_id"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_result_under_the_cap_carries_every_message_and_marks_them_all() {
    let full = inbox(2, 10, Some(json!([])));
    let (out, ids) = desk_inbox_read(&full, None);
    assert_eq!(full, out);
    assert_eq!(vec!["i0", "i1"], ids);
}

#[test]
fn an_over_cap_result_marks_only_the_messages_it_keeps_and_keeps_them_whole() {
    let full = inbox(10, 900, Some(json!([])));
    let (out, ids) = desk_inbox_read(&full, None);
    let carried = carried_ids(&out);
    assert!(!carried.is_empty() && carried.len() < 10, "{carried:?}");
    assert_eq!(carried, ids, "exactly the carried messages are marked read");
    assert_eq!(
        Some("i9"),
        ids.last().map(String::as_str),
        "the newest is kept"
    );
    for row in out["messages"].as_array().unwrap() {
        assert_eq!(
            900,
            row["body"].as_str().unwrap().len(),
            "a body is never cut"
        );
    }
    assert!(py_dumps(&out).len() <= DESK_RESULT_BYTES);
}

#[test]
fn task_queues_that_take_the_cap_leave_the_queues_out_and_keep_the_messages() {
    // The queues alone are over the cap, so no message fits beside them. The messages go out without
    // the queues, and only those are marked read.
    let queues: Vec<serde_json::Value> = (0..200)
        .map(|k| json!(format!("task-{k}-{}", "t".repeat(40))))
        .collect();
    let full = inbox(2, 300, Some(json!(queues)));
    let bare = inbox(2, 300, None);
    let (out, ids) = desk_inbox_read(&full, Some(&bare));
    assert_eq!(Some(true), out["tasks_left_out"].as_bool());
    assert!(out.get("tasks").is_none());
    assert_eq!(vec!["i0", "i1"], ids);
    assert_eq!(carried_ids(&out), ids);
    assert!(py_dumps(&out).len() <= DESK_RESULT_BYTES);
}

#[test]
fn a_message_too_big_to_fit_is_neither_cut_nor_marked_read() {
    // One message over the cap: the clamp can carry none, so the capped head is sent and nothing is
    // marked read. The message stays unread instead of being lost behind a mid-body cut.
    let full = inbox(1, 5000, Some(json!([])));
    let (out, ids) = desk_inbox_read(&full, None);
    assert!(
        ids.is_empty(),
        "nothing the response does not carry may be marked read"
    );
    assert_eq!(Some(true), out["truncated"].as_bool());
    assert!(out.get("messages").is_none());
}
