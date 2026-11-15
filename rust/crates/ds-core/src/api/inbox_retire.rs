//! `inbox_retire` (t5014): the mailbox retirement dry run. Read-only: it reports which
//! mailboxes the census rules would retire and why the others stay. Retiring needs a reviewed
//! list and the coordinator's GO, and no command does it yet.
use super::*;

impl Dispatcher {
    pub fn register_inbox_retire(&mut self) {
        self.register("inbox_retire", h_inbox_retire);
    }
}

pub fn h_inbox_retire(context: &HandlerContext<'_>, _args: Args) -> Result<Value, DunkError> {
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
    Ok(report)
}
