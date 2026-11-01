//! `dunk_api.h_retire_pane` (T37c slice 2): retire a pane - stop feeding it, stop paging about
//! it, KEEP its mail. Superuser-gated because it silences an alert, and the reason is required
//! because an unexplained silence is how an alert dies without anyone learning.
//!
//! The pane list, the superuser gate, the inbox reads and the retired-panes file are a trait
//! (`RetireEnv`) so the handler is tested without a daemon; `panes::retire`/`unretire`/
//! `all_retired` do the record keeping and `resolve::resolve_target` the pane resolution.
//! NOT in this slice: `respawn_pane` (T37c's other half), which needs turns, the outbox and the
//! delivery classifier.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::resolve::resolve_target;
use crate::error::DunkError;
use crate::panes;

/// What `h_retire_pane` reads from the rest of the daemon.
pub trait RetireEnv {
    /// The live panes as dicts (`herdr.list_panes_grouped()`), for `resolve_target`.
    fn panes(&self) -> Vec<Value>;
    fn server_error_hint(&self) -> String;
    /// `flock.require_superuser(self_pane_id, "retire_pane")`.
    fn require_superuser(&self, self_pane_id: Option<&str>, action: &str) -> Result<(), DunkError>;
    /// Every mailbox the pane owns (`inbox::mailboxes_for_pane`); a failed read is empty,
    /// because retirement must still record.
    fn mailboxes_of_pane(&self, pane_id: &str) -> Vec<String>;
    /// How much mail is now quiet: `inbox.count(pane_id).unread`, None when there is no inbox.
    fn unread(&self, pane_id: &str) -> Option<i64>;
    /// The retired-panes file (`retired-panes.json`).
    fn retired_path(&self) -> PathBuf;
}

fn text<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn pane_arg(args: &Value) -> Result<String, DunkError> {
    match args.get("pane") {
        None | Some(Value::Null) => Err(DunkError::Refused("'pane' is required".into())),
        Some(Value::String(s)) => Ok(s.clone()),
        Some(other) => Ok(other.to_string()),
    }
}

/// `h_retire_pane`: `action` is `list` (the default), `unretire` or `retire`.
pub fn h_retire_pane(
    env: &dyn RetireEnv,
    args: &Value,
    self_pane_id: Option<&str>,
    now: Option<f64>,
) -> Result<Value, DunkError> {
    let action = text(args, "action")
        .filter(|a| !a.is_empty())
        .unwrap_or("list");
    let path = env.retired_path();
    match action {
        "list" => {
            let retired = panes::all_retired(&path);
            return Ok(json!({"count": retired.len(), "retired": retired}));
        }
        "unretire" => {
            let record = panes::unretire(&pane_arg(args)?, &path)?;
            return serde_json::to_value(record).map_err(|e| DunkError::Refused(e.to_string()));
        }
        "retire" => {}
        _ => {
            return Err(DunkError::Refused(
                "retire action must be retire, unretire or list".into(),
            ))
        }
    }
    env.require_superuser(self_pane_id, "retire_pane")?;
    let spec = pane_arg(args)?;
    let live = env.panes();
    // A pane that is already gone is still recordable, by the id it was given.
    let (pane_id, labels): (String, Vec<String>) =
        match resolve_target(&live, &spec, self_pane_id, &env.server_error_hint()) {
            Ok(pane) => {
                let labels = ["tab_label", "agent_name", "label", "name"]
                    .iter()
                    .filter_map(|k| {
                        pane.get(*k)
                            .and_then(Value::as_str)
                            .filter(|v| !v.is_empty())
                            .map(String::from)
                    })
                    .collect();
                (
                    pane.get("pane_id")
                        .and_then(Value::as_str)
                        .map(String::from)
                        .unwrap_or_else(|| spec.clone()),
                    labels,
                )
            }
            Err(_) => (spec.clone(), Vec::new()),
        };
    // Every mailbox this pane owns, by its pane_id column: a name like 'crackdown-codex'
    // outlives the pane and is what the alert and the archive key on.
    let boxes = env.mailboxes_of_pane(&pane_id);
    let reason =
        text(args, "reason").ok_or_else(|| DunkError::Refused("'reason' is required".into()))?;
    let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    let box_refs: Vec<&str> = boxes.iter().map(String::as_str).collect();
    let record = panes::retire(
        &pane_id,
        reason,
        self_pane_id,
        &label_refs,
        &box_refs,
        &path,
        now,
    )?;
    // The mail is evidence: report how much is now quiet, and keep every row.
    let mut out = serde_json::to_value(record).map_err(|e| DunkError::Refused(e.to_string()))?;
    out["unread_kept"] = env
        .unread(&pane_id)
        .map(|n| json!(n))
        .unwrap_or(Value::Null);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Env {
        path: PathBuf,
        panes: Vec<Value>,
        superuser: bool,
        boxes: Vec<String>,
        unread: Option<i64>,
        // Removes the directory when the test is done with the env (t1490).
        _dir: tempfile::TempDir,
    }

    impl RetireEnv for Env {
        fn panes(&self) -> Vec<Value> {
            self.panes.clone()
        }
        fn server_error_hint(&self) -> String {
            "tmux server not running".into()
        }
        fn require_superuser(&self, _: Option<&str>, action: &str) -> Result<(), DunkError> {
            if self.superuser {
                Ok(())
            } else {
                Err(DunkError::Refused(format!(
                    "{action} requires a superuser (the owner role)"
                )))
            }
        }
        fn mailboxes_of_pane(&self, _: &str) -> Vec<String> {
            self.boxes.clone()
        }
        fn unread(&self, _: &str) -> Option<i64> {
            self.unread
        }
        fn retired_path(&self) -> PathBuf {
            self.path.clone()
        }
    }

    fn env(tag: &str) -> Env {
        let scratch = crate::testsupport::dir(&format!("retire-{tag}"));
        let dir = scratch.path().to_path_buf();
        Env {
            _dir: scratch,
            path: dir.join("retired-panes.json"),
            panes: vec![
                json!({"pane_id": "%7", "tab_label": "crackdown", "agent_name": "cd-codex", "label": "lbl", "name": ""}),
            ],
            superuser: true,
            boxes: vec!["crackdown-codex".into(), "alpha".into()],
            unread: Some(3),
        }
    }

    #[test]
    fn retire_records_labels_and_mailboxes_and_reports_the_mail_it_kept() {
        let e = env("record");
        let out = h_retire_pane(
            &e,
            &json!({"action": "retire", "pane": "%7", "reason": "superseded-by:%9"}),
            Some("%1"),
            Some(1000.0),
        )
        .unwrap();
        assert_eq!(out["pane_id"], "%7");
        assert_eq!(out["reason"], "superseded-by:%9");
        assert_eq!(out["by"], "%1");
        assert_eq!(
            out["labels"],
            json!(["cd-codex", "crackdown", "lbl"]),
            "empty names are dropped; the record sorts them"
        );
        assert_eq!(out["mailboxes"], json!(["alpha", "crackdown-codex"]));
        assert_eq!(out["unread_kept"], 3);
        assert_eq!(out["retired_at"], 1000.0);
        // and it is in the list
        let listed = h_retire_pane(&e, &json!({}), None, None).unwrap();
        assert_eq!(listed["count"], 1);
        assert_eq!(listed["retired"][0]["pane_id"], "%7");
    }

    #[test]
    fn retirement_is_superuser_gated_before_anything_is_written() {
        let mut e = env("gate");
        e.superuser = false;
        let err = h_retire_pane(
            &e,
            &json!({"action": "retire", "pane": "%7", "reason": "x"}),
            Some("%2"),
            None,
        )
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "retire_pane requires a superuser (the owner role)"
        );
        assert_eq!(
            h_retire_pane(&e, &json!({}), None, None).unwrap()["count"],
            0,
            "nothing was recorded"
        );
        // list and unretire are not gated (the Python gates only the retire branch)
        assert!(h_retire_pane(&e, &json!({"action": "list"}), None, None).is_ok());
    }

    #[test]
    fn a_pane_that_is_already_gone_is_still_recordable_by_id() {
        let mut e = env("gone");
        e.panes.clear();
        e.unread = None;
        let out = h_retire_pane(
            &e,
            &json!({"action": "retire", "pane": "%99", "reason": "died"}),
            Some("%1"),
            None,
        )
        .unwrap();
        assert_eq!(out["pane_id"], "%99");
        assert_eq!(out["labels"], json!([]));
        assert!(out["unread_kept"].is_null(), "no inbox: unknown, not zero");
    }

    #[test]
    fn a_reason_is_required_and_an_unexplained_silence_is_refused() {
        let e = env("reason");
        assert_eq!(
            h_retire_pane(&e, &json!({"action": "retire", "pane": "%7"}), None, None)
                .unwrap_err()
                .to_string(),
            "'reason' is required"
        );
        let blank = h_retire_pane(
            &e,
            &json!({"action": "retire", "pane": "%7", "reason": "  "}),
            None,
            None,
        )
        .unwrap_err();
        assert_eq!(
            blank.to_string(),
            "a reason is required (e.g. superseded-by:wW:p7)"
        );
        assert_eq!(
            h_retire_pane(&e, &json!({"action": "retire", "reason": "x"}), None, None)
                .unwrap_err()
                .to_string(),
            "'pane' is required"
        );
        assert_eq!(
            h_retire_pane(&e, &json!({}), None, None).unwrap()["count"],
            0,
            "no refusal wrote a record"
        );
    }

    #[test]
    fn unretire_brings_a_pane_back_and_refuses_one_that_was_not_retired() {
        let e = env("unretire");
        h_retire_pane(
            &e,
            &json!({"action": "retire", "pane": "%7", "reason": "x"}),
            None,
            None,
        )
        .unwrap();
        let back =
            h_retire_pane(&e, &json!({"action": "unretire", "pane": "%7"}), None, None).unwrap();
        assert_eq!(back["pane_id"], "%7");
        assert_eq!(
            h_retire_pane(&e, &json!({}), None, None).unwrap()["count"],
            0
        );
        let err = h_retire_pane(&e, &json!({"action": "unretire", "pane": "%7"}), None, None)
            .unwrap_err();
        assert_eq!(err.to_string(), "%7 is not retired");
    }

    #[test]
    fn an_unknown_action_is_refused_with_the_pythons_text() {
        let e = env("action");
        assert_eq!(
            h_retire_pane(&e, &json!({"action": "kill"}), None, None)
                .unwrap_err()
                .to_string(),
            "retire action must be retire, unretire or list"
        );
    }
}
