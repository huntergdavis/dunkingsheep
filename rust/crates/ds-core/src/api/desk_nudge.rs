//! t1613 / t1352: the desk nudge for a Telegram owner question.
//!
//! The web route nudges the desk itself (`ds-web` `routes/owner.rs`). A Telegram question reaches the
//! daemon through `owner_ingest` and was recorded without anyone being told. These helpers are the
//! pure part of the fix: the desk pane choice (the web's `match_pane` rule for the default desk
//! channel, `{"id": "cos", "match": "desk"}`) and the text of the one-line inbox nudge.

use serde_json::Value;

/// The default desk match, as the web route uses it when no channel config is given.
pub const DESK_MATCH: &str = "desk";

/// A pane's label field, lowercased; a missing or non-text field is the empty string.
fn label(pane: &Value, key: &str) -> String {
    pane.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_lowercase()
}

/// An exact match: the tab label is the needle, or the working directory is the needle or ends in
/// `/needle`. Duplicate labels such as `sn-desk` do not match this way; the working directory decides.
fn is_exact(pane: &Value, needle: &str) -> bool {
    let cwd = label(pane, "cwd");
    let cwd = cwd.trim_end_matches('/');
    label(pane, "tab_label") == needle || cwd == needle || cwd.ends_with(&format!("/{needle}"))
}

/// The desk pane, or `None` when there is no desk lane to nudge. An exact match is preferred over a
/// partial one (the tab or workspace label containing the needle), exactly as the web route does.
pub fn desk_pane_for(panes: &[Value]) -> Option<&Value> {
    let needle = DESK_MATCH;
    panes
        .iter()
        .find(|pane| is_exact(pane, needle))
        .or_else(|| {
            panes.iter().find(|pane| {
                label(pane, "tab_label").contains(needle)
                    || label(pane, "workspace_label").contains(needle)
            })
        })
}

/// The inbox subject of a nudge for one owner message.
pub fn nudge_subject(message_id: &str) -> String {
    format!("owner message {message_id}")
}

/// The body of a nudge for one owner message: where it waits and how to answer it. The text matches
/// the web route's nudge, with the transport named.
pub fn nudge_body(message_id: &str, transport: &str, text: &str) -> String {
    format!(
        "Owner message {message_id} ({transport}) is waiting in the owner mailbox: `dunkingsheep owner conversation`. Use `owner reply {message_id}` only for the final answer, not a routing acknowledgement. A final response may ask for needed clarification or explain an error. If supervisor work is needed, run `dunkingsheep owner handoff {message_id}` and leave the question pending. Quiet, failed or timed-out handoffs need recovery; queued does not mean picked up.\n\n{text}"
    )
}

/// The body of the owner lane's nudge for a Telegram message (t4645). The owner lane is told as
/// well as the desk, so the text says who else knows and how to answer without doing it twice.
pub fn owner_lane_nudge_body(message_id: &str, text: &str) -> String {
    format!(
        "Owner message {message_id} (telegram) arrived in the owner mailbox: `dunkingsheep owner conversation`. The front desk was told as well and may answer it. If you answer it yourself, use `owner reply {message_id}` once, for the final answer; the reply goes back through Telegram.\n\n{text}"
    )
}

/// The one registered owner lane to nudge, or `None` when there is not exactly one, or when it is
/// the desk pane that was already nudged.
pub fn owner_lane_for<'a>(owners: &'a [String], desk_pane: Option<&str>) -> Option<&'a str> {
    match owners {
        [only] if !only.is_empty() && Some(only.as_str()) != desk_pane => Some(only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn pane(id: &str, tab: &str, workspace: &str, cwd: &str) -> Value {
        json!({"pane_id": id, "tab_label": tab, "workspace_label": workspace, "cwd": cwd})
    }

    #[test]
    fn the_owner_lane_is_nudged_only_when_there_is_exactly_one_and_it_is_not_the_desk() {
        let one = ["%1".to_string()];
        assert_eq!(owner_lane_for(&one, Some("%133")), Some("%1"));
        assert_eq!(owner_lane_for(&one, None), Some("%1"));
        assert_eq!(owner_lane_for(&one, Some("%1")), None);
        assert_eq!(owner_lane_for(&[], Some("%133")), None);
        assert_eq!(owner_lane_for(&["%1".into(), "%2".into()], None), None);
        assert_eq!(owner_lane_for(&[String::new()], None), None);
        let body = owner_lane_nudge_body("i9", "are you there?");
        assert!(body.starts_with("Owner message i9 (telegram) arrived in the owner mailbox"));
        assert!(body.ends_with("\n\nare you there?"));
    }

    #[test]
    fn duplicate_desk_labels_resolve_by_working_directory_not_order() {
        // Three lanes labelled sn-desk, and the real desk working directory listed last.
        let panes = [
            pane("%129", "sn-desk", "dunk", "/home/hunter/workspace/sn-desk"),
            pane(
                "%132",
                "sn-desk",
                "dunk",
                "/home/hunter/workspace/sn-desk-2",
            ),
            pane("%133", "sn-desk", "dunk", "/home/hunter/workspace/desk"),
        ];
        assert_eq!(desk_pane_for(&panes).unwrap()["pane_id"], "%133");
    }

    #[test]
    fn an_exact_tab_label_beats_a_partial_label_listed_first() {
        let panes = [
            pane("%129", "sn-desk", "dunk", "/x/sn-desk"),
            pane("%130", "desk", "dunk", "/x/other"),
        ];
        assert_eq!(desk_pane_for(&panes).unwrap()["pane_id"], "%130");
    }

    #[test]
    fn a_partial_match_is_used_only_when_no_exact_match_exists() {
        let panes = [pane("%129", "sn-desk", "dunk", "/x/sn-desk")];
        assert_eq!(desk_pane_for(&panes).unwrap()["pane_id"], "%129");
    }

    #[test]
    fn no_desk_lane_means_no_nudge_target() {
        let panes = [pane("%1", "vp", "dunk", "/x/vp")];
        assert!(desk_pane_for(&panes).is_none());
        assert!(desk_pane_for(&[]).is_none());
    }

    #[test]
    fn the_nudge_names_the_message_transport_and_the_answer_command() {
        assert_eq!(nudge_subject("i9"), "owner message i9");
        let body = nudge_body("i9", "telegram", "is the desk up?");
        assert!(body.starts_with("Owner message i9 (telegram) is waiting"));
        assert!(body.contains("`owner reply i9`"));
        assert!(body.contains("only for the final answer, not a routing acknowledgement"));
        assert!(body.ends_with("is the desk up?"));
    }
}
