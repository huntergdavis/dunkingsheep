//! `inbox_retire` (t5014): the mailbox retirement dry run, and the owner-only apply.
//!
//! Without `apply` it is read-only: it reports which mailboxes the census rules would retire and
//! why the others stay. With `apply` (owner only) it retires an EXPLICIT list of named mailboxes,
//! never a whole class: each named mailbox is classified again now and is retired only when it
//! is `retired` now; the store re-checks each one inside one transaction. Nothing is deleted.
use super::*;

impl Dispatcher {
    pub fn register_inbox_retire(&mut self) {
        self.register("inbox_retire", h_inbox_retire);
    }
}

pub fn h_inbox_retire(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let store = context
        .flock
        .stores()
        .inbox
        .ok_or_else(|| DunkError::Refused("inbox store is not wired".into()))?;
    let rows = store.mailbox_census()?;
    let mut identities = crate::inbox_retire::Identities::default();
    // Fail closed: a pane list that could not be read would make every live lane look gone.
    let panes = context.env.try_authority_panes().map_err(|error| {
        DunkError::Refused(format!(
            "inbox retire: the live pane list could not be read, so live lanes cannot be told apart from retired ones: {error}"
        ))
    })?;
    for pane in panes {
        for field in ["pane_id", "label", "tab_label", "agent_name", "name"] {
            if let Some(value) = pane
                .get(field)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                identities.live.push(value.to_string());
            }
        }
    }
    for lane in context.flock.lanes() {
        for field in ["pane_id", "mailbox"] {
            if let Some(value) = lane
                .get(field)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                identities.registered.push(value.to_string());
            }
        }
    }
    // The role registry is optional here: without one, nothing is added from it, and the report
    // says so instead of treating its absence as "no registered names".
    let roles = context.flock.with_roles(|roles, _| {
        Ok(roles
            .lanes
            .iter()
            .flat_map(|(pane, entry)| {
                let mut names = vec![pane.clone()];
                for field in ["label", "name", "mailbox"] {
                    if let Some(value) = entry
                        .get(field)
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty())
                    {
                        names.push(value.to_string());
                    }
                }
                names
            })
            .collect::<Vec<_>>())
    });
    let roles_note = match roles {
        Ok(names) => {
            identities.registered.extend(names);
            Value::Null
        }
        Err(error) => json!(format!("role registry not read: {error}")),
    };
    let mut report = crate::inbox_retire::report(&rows, &identities, context.env.now_s());
    report["role_registry"] = roles_note;
    if args.get("apply").and_then(Value::as_bool) != Some(true) {
        return Ok(report);
    }
    context
        .flock
        .require_superuser(context.self_pane_id(), "inbox retire --apply", context.env)?;
    let named: Vec<String> = args
        .get("mailboxes")
        .and_then(Value::as_array)
        .map(|names| {
            names
                .iter()
                .filter_map(|name| {
                    name.as_str()
                        .map(str::trim)
                        .filter(|n| !n.is_empty())
                        .map(String::from)
                })
                .collect()
        })
        .unwrap_or_default();
    if named.is_empty() {
        return Err(DunkError::Refused(
            "inbox retire --apply needs an explicit list: --mailbox NAME (repeatable); a whole class is never retired at once".into(),
        ));
    }
    let reason = args
        .get("reason")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|reason| !reason.is_empty())
        .ok_or_else(|| DunkError::Refused("inbox retire --apply needs --reason".into()))?
        .to_string();
    let (candidates, refused) = apply_selection(&report, &named);
    let by = context.self_pane_id().unwrap_or("owner").to_string();
    let outcome = if candidates.is_empty() {
        crate::inbox_store::mailbox_status::RetireOutcome::default()
    } else {
        store.retire_mailboxes(&candidates, &reason, &by, context.env.now_s())?
    };
    Ok(json!({
        "applied": true,
        "reason": reason,
        "retired": outcome.retired,
        "kept_at_recheck": outcome.kept.iter().map(|(mailbox, why)| json!({"mailbox": mailbox, "why": why})).collect::<Vec<_>>(),
        "refused": refused,
    }))
}

/// t5014: of the explicitly `named` mailboxes, those the report classes `retired` now, and the
/// others with their class and reasons (`not found` for a name the census does not have).
pub fn apply_selection(report: &Value, named: &[String]) -> (Vec<String>, Vec<Value>) {
    let mut candidates = Vec::new();
    let mut refused = Vec::new();
    for name in named {
        let entry = report["mailboxes"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry["mailbox"].as_str() == Some(name.as_str()));
        match entry {
            Some(entry) if entry["class"] == "retired" => candidates.push(name.clone()),
            Some(entry) => refused.push(json!({"mailbox": name, "class": entry["class"], "why": entry["why"]})),
            None => refused.push(json!({"mailbox": name, "class": "not found", "why": ["no mailbox of that exact name in the census"]})),
        }
    }
    (candidates, refused)
}

#[cfg(test)]
mod apply_tests {
    use super::apply_selection;
    use serde_json::json;

    #[test]
    fn only_explicitly_named_mailboxes_classed_retired_now_are_selected() {
        let report = json!({"mailboxes": [
            {"mailbox": "old-worker-7", "class": "retired", "why": []},
            {"mailbox": "old-worker-8", "class": "retired", "why": []},
            {"mailbox": "poolrad-vp", "class": "vp", "why": ["a VP, chief-of-staff or desk mailbox"]},
            {"mailbox": "busy", "class": "ambiguous", "why": ["2 unread message(s)"]},
        ]});
        let named = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        // Only what is named, even though old-worker-8 is a candidate too.
        let (candidates, refused) = apply_selection(
            &report,
            &named(&["old-worker-7", "poolrad-vp", "busy", "ghost"]),
        );
        assert_eq!(candidates, ["old-worker-7"]);
        let classes: Vec<_> = refused.iter().map(|r| r["class"].clone()).collect();
        assert_eq!(
            classes,
            [json!("vp"), json!("ambiguous"), json!("not found")]
        );
        assert_eq!(refused[1]["why"], json!(["2 unread message(s)"]));
    }
}
