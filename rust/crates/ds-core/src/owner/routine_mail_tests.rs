//! t3592: routine lane mail (review verdicts and notes, marked by subject prefix) is kept out of the owner
//! conversation. Owner decision and blocker requests, owner replies and unmarked legacy mail stay listed.
//! The body never decides. Coordinator decision i49515.
//!
//! The filter in `conversation()` is `is_routine_mail`; these fixtures pin its decision on constructed rows.

use serde_json::json;

use super::{is_routine_mail, ROUTINE_SUBJECT_PREFIXES};

fn row(subject: &str, body: &str, reply_to: Option<&str>) -> serde_json::Value {
    json!({
        "subject": subject,
        "body": body,
        "reply_to": reply_to,
        "mailbox": "codex-rust-owner",
        "state": "queued",
    })
}

#[test]
fn a_marked_review_verdict_is_routine_and_not_listed() {
    assert!(is_routine_mail(&row(
        "[review-verdict] PR #826 @ a60d5fd8",
        "accept",
        None
    )));
}

#[test]
fn a_marked_review_note_is_routine_and_not_listed() {
    assert!(is_routine_mail(&row(
        "[review-note] t3641 evidence",
        "NOT RUN locally; CI is the evidence",
        None
    )));
}

#[test]
fn an_owner_decision_request_and_an_owner_blocker_stay_listed() {
    assert!(!is_routine_mail(&row(
        "[owner-decision] t3592 marker",
        "which marker?",
        None
    )));
    assert!(!is_routine_mail(&row(
        "[owner-blocker] PR #810 needs a human",
        "stuck on the sync path",
        None
    )));
}

#[test]
fn an_unmarked_legacy_decision_stays_listed() {
    // Legacy traffic has no marker: it is kept so an old decision is not silently lost.
    assert!(!is_routine_mail(&row(
        "Decision needed on the deploy",
        "yes or no",
        None
    )));
}

#[test]
fn an_owner_reply_stays_listed_even_with_a_routine_looking_subject() {
    assert!(!is_routine_mail(&row(
        "[review-verdict] answer",
        "ok",
        Some("i48000")
    )));
}

#[test]
fn contradictory_body_wording_never_overrides_the_marker() {
    // A verdict marked routine stays routine although its body asks for a decision.
    assert!(is_routine_mail(&row(
        "[review-verdict] PR #1",
        "DECISION NEEDED: accept?",
        None
    )));
    // A decision request stays listed although its body calls itself a routine verdict.
    assert!(!is_routine_mail(&row(
        "[owner-decision] PR #1",
        "routine verdict, accept",
        None
    )));
}

#[test]
fn every_routine_prefix_is_the_listed_constant() {
    for prefix in ROUTINE_SUBJECT_PREFIXES {
        assert!(
            is_routine_mail(&row(prefix, "", None)),
            "{prefix} must be routine"
        );
    }
}
