//! `flock/commands_owner_views.rs` (T38, slice 1): the owner-push handlers of `dunk_api`
//! (`h_owner_push`, `h_owner_poll`, `h_owner_take`, `h_owner_stats`, `h_owner_receipts`).
//! Each takes the args object the wire carries plus the caller's pane id and returns the
//! JSON the Python returns, so the command table (T39) can call them directly.
//!
//! NOT in this slice: `h_roster`/`h_message_state`/`h_reply_stats`/`h_project_summary`
//! (dunk_views), `state_capture`/`state_latest`, `status`, `health` (daemon side),
//! `lanes`/`lane_set`, `capability`, and `owner_conversation/reply/ingest/outbox/delivered`.
//! `owner_push` takes the delivery-target resolver as a parameter because
//! `Flock::delivery_target` (terminal lookup plus `resolve_mailbox`) is not ported yet.

use serde_json::{json, Value};

use crate::error::DunkError;
use crate::owner::OwnerPush;

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

/// Python `args.get(key) or default` for a string: absent, null and "" all fall through.
fn text_or<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    text(args, key).filter(|s| !s.is_empty())
}

/// `h_owner_push`: owner -> agent delivery with NO inbox row and NO typing. `resolve` is
/// `flock.delivery_target(target, self_pane_id)` and returns the pane id (its refusals,
/// `dead_lane`/`unhoused_lane`/`unknown_name`, pass through untouched).
pub fn h_owner_push(
    store: &OwnerPush,
    args: &Value,
    resolve: &dyn Fn(&str) -> Result<String, DunkError>,
    now: f64,
) -> Result<Value, DunkError> {
    let target =
        text(args, "target").ok_or_else(|| DunkError::Refused("'target' is required".into()))?;
    let body = text(args, "body").ok_or_else(|| DunkError::Refused("'body' is required".into()))?;
    let pane = resolve(target)?;
    let pushed = store.push(
        &pane,
        body,
        text_or(args, "target"),
        text_or(args, "subject"),
        now,
    )?;
    // "stored" is its own outcome (VP ruling 2026-10-24): the message is durable and the
    // LANE drains it, so it has not been read - a caller that treats this as delivered
    // would be reporting a delivery nobody has made.
    let mut out = serde_json::to_value(pushed).map_err(|e| DunkError::Refused(e.to_string()))?;
    out["outcome"] = json!("stored");
    out["expect"] = json!(
        "the lane's own harness drains it (owner_take); no inbox row, nothing typed into the pane"
    );
    Ok(out)
}

/// `h_owner_poll`: the lane's harness says "I am polling" - including when there was
/// nothing. A heartbeat MUST come from the pane it claims (independent review 2026-10-24).
pub fn h_owner_poll(
    store: &OwnerPush,
    args: &Value,
    self_pane_id: Option<&str>,
    now: f64,
) -> Result<Value, DunkError> {
    let claimed = text(args, "pane_id").unwrap_or("").trim();
    let Some(me) = self_pane_id.filter(|p| !p.is_empty()) else {
        return Err(DunkError::Refused(
            "owner_poll needs a caller pane: a heartbeat without an identity is a declaration, not an observation".into(),
        ));
    };
    if !claimed.is_empty() && claimed != me {
        return Err(DunkError::Refused(format!(
            "owner_poll for {} refused: this call comes from {}. Only a lane may report its own polling.",
            py_repr(claimed),
            py_repr(me)
        )));
    }
    let by = text_or(args, "by").unwrap_or("harness");
    let polled = store.poll(me, Some(by), now)?;
    serde_json::to_value(polled).map_err(|e| DunkError::Refused(e.to_string()))
}

/// `h_owner_take`: what is waiting for this pane (the extension calls it about once a second).
pub fn h_owner_take(
    store: &OwnerPush,
    args: &Value,
    self_pane_id: Option<&str>,
    now: f64,
) -> Result<Value, DunkError> {
    let pane = text_or(args, "pane_id")
        .or(self_pane_id.filter(|p| !p.is_empty()))
        .ok_or_else(|| DunkError::Refused("owner_take needs a pane".into()))?;
    let messages = store.take(pane, text_or(args, "by"), 20, now)?;
    Ok(json!({"pane_id": pane, "messages": messages}))
}

/// `h_owner_stats`.
pub fn h_owner_stats(store: &OwnerPush, now: f64) -> Result<Value, DunkError> {
    Ok(json!({
        "stats": store.stats(86400.0, now)?,
        "pending": store.pending(None)?,
    }))
}

/// `h_owner_receipts`: READ-ONLY arrival timestamps for specific owner pushes (t66). It must
/// never call `take()`: a receipt must not consume what it reports on. `push_ids` is a list
/// or a comma string; non-digit entries are dropped, as the Python's `isdigit()` filter does.
pub fn h_owner_receipts(store: &OwnerPush, args: &Value) -> Result<Value, DunkError> {
    let raw: Vec<String> = match args.get("push_ids") {
        Some(Value::String(s)) => s
            .split(',')
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())
            .collect(),
        Some(Value::Array(items)) => items
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect(),
        _ => Vec::new(),
    };
    let ids: Vec<i64> = raw
        .iter()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()))
        .filter_map(|s| s.parse().ok())
        .collect();
    let receipts = store.receipts(&ids)?;
    serde_json::to_value(receipts).map_err(|e| DunkError::Refused(e.to_string()))
}

/// Python `repr(str)` for the quoted names in the refusals.
fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 1_000.0;

    fn store(tag: &str) -> OwnerPush {
        let dir = std::env::temp_dir().join(format!("ds-owner-views-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("owner-push.db");
        let _ = std::fs::remove_file(&path);
        OwnerPush::open(&path).unwrap()
    }

    fn live(target: &str) -> Result<String, DunkError> {
        Ok(target.trim_start_matches('~').to_string())
    }

    #[test]
    fn push_is_stored_not_delivered_and_take_drains_it() {
        let s = store("push");
        let out = h_owner_push(
            &s,
            &json!({"target": "%9", "body": "hi", "subject": "S"}),
            &live,
            NOW,
        )
        .unwrap();
        assert_eq!(out["outcome"], "stored");
        assert_eq!(out["pane_id"], "%9");
        assert!(out["expect"].as_str().unwrap().contains("no inbox row"));
        let taken = h_owner_take(&s, &json!({"by": "ext"}), Some("%9"), NOW + 1.0).unwrap();
        assert_eq!(taken["pane_id"], "%9");
        assert_eq!(taken["messages"][0]["body"], "hi");
        assert_eq!(taken["messages"][0]["taken_by"], "ext");
        assert_eq!(
            h_owner_take(&s, &json!({}), Some("%9"), NOW + 2.0).unwrap()["messages"],
            json!([])
        );
        assert!(h_owner_take(&s, &json!({}), None, NOW)
            .unwrap_err()
            .to_string()
            .contains("needs a pane"));
    }

    #[test]
    fn a_dead_target_refuses_before_anything_is_stored() {
        let s = store("dead");
        let dead = |_: &str| -> Result<String, DunkError> {
            Err(DunkError::Refused(
                "no live pane '%1': this terminal is gone. reason=dead_lane.".into(),
            ))
        };
        let err = h_owner_push(&s, &json!({"target": "%1", "body": "x"}), &dead, NOW).unwrap_err();
        assert!(err.to_string().contains("reason=dead_lane"));
        assert_eq!(h_owner_stats(&s, NOW).unwrap()["pending"], json!([]));
        assert!(
            h_owner_push(&s, &json!({"target": "%1", "body": "  "}), &live, NOW).is_err(),
            "an empty body is refused by the store"
        );
    }

    #[test]
    fn a_heartbeat_must_come_from_the_pane_it_claims() {
        let s = store("poll");
        let err = h_owner_poll(&s, &json!({}), None, NOW).unwrap_err();
        assert!(err.to_string().contains("needs a caller pane"));
        let err = h_owner_poll(&s, &json!({"pane_id": "%2"}), Some("%1"), NOW).unwrap_err();
        assert_eq!(err.to_string(), "owner_poll for '%2' refused: this call comes from '%1'. Only a lane may report its own polling.");
        let ok = h_owner_poll(&s, &json!({"pane_id": "%1", "by": ""}), Some("%1"), NOW).unwrap();
        assert_eq!(ok, json!({"pane_id": "%1", "at": NOW, "by": "harness"}));
        assert_eq!(s.last_poll("%1").unwrap(), Some(NOW));
    }

    #[test]
    fn stats_reports_counts_and_the_pending_rows() {
        let s = store("stats");
        h_owner_push(&s, &json!({"target": "%9", "body": "a"}), &live, NOW).unwrap();
        h_owner_push(&s, &json!({"target": "%9", "body": "b"}), &live, NOW).unwrap();
        h_owner_take(&s, &json!({"pane_id": "%9"}), None, NOW + 4.0).unwrap();
        h_owner_push(&s, &json!({"target": "%8", "body": "c"}), &live, NOW + 5.0).unwrap();
        let out = h_owner_stats(&s, NOW + 6.0).unwrap();
        assert_eq!(out["stats"]["pushed"], 3);
        assert_eq!(out["stats"]["taken"], 2);
        assert_eq!(out["stats"]["pending"], 1);
        assert_eq!(out["pending"].as_array().unwrap().len(), 1);
        assert_eq!(out["pending"][0]["pane_id"], "%8");
    }

    #[test]
    fn receipts_report_requested_returned_and_truncated_and_never_consume() {
        let s = store("receipts");
        let id = h_owner_push(
            &s,
            &json!({"target": "%9", "body": "SECRET", "subject": "SUBJ"}),
            &live,
            NOW,
        )
        .unwrap()["id"]
            .as_i64()
            .unwrap();
        let by_list = h_owner_receipts(&s, &json!({"push_ids": [id, "x", -4, 0]})).unwrap();
        assert_eq!(by_list["requested"], 1);
        assert_eq!(by_list["returned"], 1);
        assert_eq!(by_list["truncated"], false);
        let row = by_list["receipts"][0].as_object().unwrap();
        let mut keys: Vec<&str> = row.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            ["created_at", "id", "pane_id", "taken_at"],
            "no body, no subject"
        );
        let by_string =
            h_owner_receipts(&s, &json!({"push_ids": format!("{id}, , nope")})).unwrap();
        assert_eq!(by_string["returned"], 1);
        assert_eq!(
            h_owner_stats(&s, NOW).unwrap()["pending"]
                .as_array()
                .unwrap()
                .len(),
            1,
            "a receipt did not consume the push"
        );
        let many: Vec<i64> = (1..=250).collect();
        let big = h_owner_receipts(&s, &json!({"push_ids": many})).unwrap();
        assert_eq!(big["requested"], 250);
        assert_eq!(big["truncated"], true);
        assert!(big["returned"].as_i64().unwrap() <= 100);
        let none = h_owner_receipts(&s, &json!({})).unwrap();
        assert_eq!(
            (none["requested"].as_i64(), none["truncated"].as_bool()),
            (Some(0), Some(false)),
            "an empty request reports its own counts"
        );
    }
}
