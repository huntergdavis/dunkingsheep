//! t3592 repair (t3711/t3712): routine lane mail is excluded in the query, so the limit and the `before`
//! boundary apply to the rows that are listed. A window full of routine mail must not hide an older decision.

use super::*;

const DECISION: &str = "[owner-decision] older decision";
const VERDICT: &str = "[review-verdict] PR #1 accept";

fn send_subject(fixture: &Fixture, subject: &str, reply_to: Option<&str>) -> String {
    // A short pause keeps created_at distinct, so the boundary test is deterministic.
    std::thread::sleep(std::time::Duration::from_millis(2));
    fixture.send_outgoing(&Outgoing {
        mailbox: "owner",
        subject: Some(subject),
        body: Some("body"),
        sender: Some("fixture"),
        reply_to,
        ..Default::default()
    })
}

fn listed_ids(rows: &[Value]) -> Vec<String> {
    rows.iter()
        .filter_map(|row| row.get("id").and_then(Value::as_str).map(str::to_string))
        .collect()
}

fn window(fixture: &Fixture, limit: Option<i64>, before: Option<f64>) -> Vec<Value> {
    fixture
        .store
        .conversation_across_visible(&["owner".to_string()], limit, before)
        .expect("visible window")
}

#[test]
fn routine_rows_do_not_use_up_the_limit_so_an_older_decision_is_listed() {
    let fixture = Fixture::new();
    let decision = send_subject(&fixture, DECISION, None);
    for _ in 0..5 {
        send_subject(&fixture, VERDICT, None);
    }
    let rows = window(&fixture, Some(2), None);
    assert!(
        listed_ids(&rows).contains(&decision),
        "the older decision must survive a window full of routine rows"
    );
    assert!(
        rows.iter().all(|row| row["subject"] != VERDICT),
        "no routine verdict is listed"
    );
}

#[test]
fn the_before_boundary_still_reaches_an_older_decision_past_routine_rows() {
    let fixture = Fixture::new();
    let decision = send_subject(&fixture, DECISION, None);
    let first_verdict = send_subject(&fixture, VERDICT, None);
    let boundary = fixture
        .store
        .get_message(&first_verdict)
        .expect("the routine row exists")
        .summary
        .created_at;
    for _ in 0..2 {
        send_subject(&fixture, VERDICT, None);
    }
    // Only the decision is older than the boundary; the routine rows are excluded in the query, and the
    // limit applies to what is left, so the decision is listed.
    let rows = window(&fixture, Some(1), Some(boundary));
    assert_eq!(vec![decision], listed_ids(&rows));
}

#[test]
fn an_owner_reply_with_a_routine_looking_subject_stays_listed() {
    let fixture = Fixture::new();
    let reply = send_subject(&fixture, VERDICT, Some("i48000"));
    let rows = window(&fixture, None, None);
    assert_eq!(vec![reply], listed_ids(&rows));
}

#[test]
fn unmarked_legacy_mail_stays_listed_and_leading_space_does_not_hide_a_marker() {
    let fixture = Fixture::new();
    let legacy = send_subject(&fixture, "Decision needed on the deploy", None);
    let spaced = send_subject(&fixture, "   [review-note] spaced marker", None);
    let ids = listed_ids(&window(&fixture, None, None));
    assert!(ids.contains(&legacy), "unmarked legacy mail is kept");
    assert!(
        !ids.contains(&spaced),
        "leading whitespace is ignored, as in is_routine_mail"
    );
}
