//! The T39 turn-end, sidebar poll and roster handlers.

use serde_json::{json, Value};

use super::{Args, HandlerContext};
use crate::error::DunkError;

/// `dunk_api.h_turn_end`.
pub fn h_turn_end(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let self_pane_id = context.self_pane_id();
    let requested = args.get("pane");
    if requested.is_some_and(|value| super::truthy(value) && value.as_str().is_none()) {
        return Err(DunkError::Refused(
            "turn event must target the caller's own pane".into(),
        ));
    }
    let pane = requested.and_then(Value::as_str);
    let pane_pid = self_pane_id
        .and_then(|id| context.env.get_pane(id))
        .and_then(|info| info.get("pane_pid").and_then(Value::as_i64));
    context.flock.require_turn_caller(
        pane,
        self_pane_id,
        pane_pid,
        context.request.caller.as_ref(),
    )?;

    let raw_reason = args
        .get("reason")
        .filter(|reason| super::truthy(reason))
        .cloned()
        .unwrap_or_else(|| json!("turn_end"));
    let reason = super::python_value_string(&raw_reason);
    let turn_id = args.get("turn_id").and_then(Value::as_str);
    let mut result = context.flock.turn_end(
        pane,
        &reason,
        self_pane_id,
        turn_id,
        true,
        context.env.turn_hooks()?,
    )?;
    if result.get("ended").and_then(Value::as_bool) == Some(true) {
        result["reason"] = raw_reason;
    }
    Ok(result)
}

/// Poll distinguishes domain errors (partial response) from raw limit conversion errors.
enum PollInboxError {
    Partial(DunkError),
    Propagate(DunkError),
}

/// One uncached count and unread-list read using the request's validated inbox services.
fn poll_inbox(
    context: &HandlerContext<'_>,
    args: &Args,
    count_out: &mut Option<Value>,
) -> Result<(String, Vec<Value>), PollInboxError> {
    let mailbox = args
        .get("mailbox")
        .filter(|value| super::truthy(value))
        .map(super::python_value_string);
    let mut limit_error = None;
    let mut overflow_error = None;
    let result = context.env.with_inbox_env(&mut |env| {
        let counts = context
            .flock
            .inbox_count(env, mailbox.as_deref(), context.self_pane_id())?;
        *count_out = Some(json!({
            "mailbox": counts.mailbox,
            "unread": counts.unread,
            "total": counts.total,
            "archived": counts.archived,
            "undeliverable": counts.undeliverable,
            "oldest_unread_s": counts.oldest_unread_s,
            "attachments_unread": counts.attachments_unread,
            "from_lanes": counts.from_lanes,
        }));

        // Python evaluates int(limit) only after inbox_count returns and before inbox_list
        // resolves the mailbox. Keep that order so a bad mailbox wins over a bad limit.
        let limit = match super::owner_outbox_limit_arg(args, 5) {
            Ok(super::OwnerOutboxLimit::Sqlite(limit)) => limit,
            Ok(super::OwnerOutboxLimit::Overflow) => {
                // Python's int() accepts arbitrary-size integers; SQLite binding fails only
                // after inbox_list has resolved and authorized the mailbox.
                context.flock.mailbox_or_self(
                    env,
                    mailbox.as_deref(),
                    context.self_pane_id(),
                    false,
                    true,
                )?;
                overflow_error = Some(DunkError::Refused(
                    "Python int too large to convert to SQLite INTEGER".into(),
                ));
                return Ok(Value::Null);
            }
            Err(error) => {
                limit_error = Some(error);
                return Ok(Value::Null);
            }
        };
        let key = context.flock.mailbox_or_self(
            env,
            mailbox.as_deref(),
            context.self_pane_id(),
            false,
            true,
        )?;
        let rows = crate::inbox::list_signed(env.pool(), &key, true, limit, env.now())?;
        let messages = rows
            .iter()
            .map(|row| {
                let full = env.store().get_message(&row.message_id)?;
                let mut wire = super::owner_message_value(&full);
                wire.as_object_mut()
                    .expect("message wire is an object")
                    .remove("body");
                wire.as_object_mut()
                    .expect("message wire is an object")
                    .remove("read_at");
                Ok(wire)
            })
            .collect::<Result<Vec<_>, DunkError>>()?;
        Ok(json!({"mailbox": key, "messages": messages}))
    });
    if let Some(error) = limit_error {
        return Err(PollInboxError::Propagate(error));
    }
    if let Some(error) = overflow_error {
        return Err(PollInboxError::Propagate(error));
    }
    let result = result.map_err(PollInboxError::Partial)?;
    let mailbox = result["mailbox"].as_str().unwrap_or_default().to_owned();
    let messages = result["messages"].as_array().cloned().unwrap_or_default();
    Ok((mailbox, messages))
}

/// `dunk_api.h_poll`.
pub fn h_poll(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let mut status = crate::flock::status::poll_status(context.env.status_env()?);
    status.insert(
        "self_pane_id".into(),
        context
            .self_pane_id()
            .map(|pane| json!(pane))
            .unwrap_or(Value::Null),
    );
    let dunks = || {
        Value::Array(
            context
                .flock
                .dunks
                .list()
                .into_iter()
                .map(Value::Object)
                .collect(),
        )
    };
    let mut result = json!({"status": status});

    let mut inbox_count = None;
    match poll_inbox(context, &args, &mut inbox_count) {
        Ok((mailbox, messages)) => {
            result["inbox_count"] = inbox_count.expect("successful poll recorded inbox count");
            result["dunks"] = json!({
                "dunks": dunks(),
                "mailbox": mailbox,
                "messages": messages,
            });
        }
        Err(PollInboxError::Partial(error)) => {
            if let Some(count) = inbox_count {
                result["inbox_count"] = count;
            }
            result["error"] = json!(error.to_string());
            result["dunks"] = json!({"dunks": dunks(), "messages": []});
        }
        Err(PollInboxError::Propagate(error)) => return Err(error),
    }

    if super::python_bool_arg(&args, "panes", false) {
        result["panes"] = super::h_list_panes(context, args.clone())?;
    }
    if super::python_bool_arg(&args, "tasks", false) {
        let scope = args
            .get("task_scope")
            .filter(|value| !value.is_null())
            .map(super::python_value_string);
        let assignee = args
            .get("assignee")
            .filter(|value| super::truthy(value))
            .map(super::python_value_string);
        let limit = super::python_int_arg(&args, "task_limit", 50)?;
        let (scope, tasks) = context.block_on(context.flock.task_list_wire(
            scope.as_deref(),
            None,
            assignee.as_deref(),
            super::python_bool_arg(&args, "include_done", false),
            Some(limit),
            context.self_pane_id(),
            None,
            false,
        ))??;
        result["tasks"] = json!({
            "scope": scope.unwrap_or_else(|| "visible".into()),
            "pane_id": context.self_pane_id(),
            "tasks": tasks.into_iter().map(|task| task.wire).collect::<Vec<_>>(),
        });
    }
    Ok(result)
}

/// `dunk_api.h_roster`.
pub fn h_roster(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let mut result = crate::flock::roster::roster(context.env.roster_env()?, context.env.now_s())
        .map_err(DunkError::Refused)?;
    filter_project(&mut result, args.get("project"))?;
    Ok(result)
}

fn filter_project(result: &mut Value, requested: Option<&Value>) -> Result<(), DunkError> {
    if let Some(project) = requested.filter(|v| !v.is_null()) {
        let project = project
            .as_str()
            .filter(|s| !s.is_empty() && s.trim() == *s)
            .ok_or_else(|| {
                DunkError::Refused(
                    "project must be a non-empty name without surrounding whitespace".into(),
                )
            })?;
        let lanes = result["lanes"]
            .as_array_mut()
            .ok_or_else(|| DunkError::Refused("roster has no lanes".into()))?;
        lanes.retain(|lane| {
            lane["membership"]["scope"] == "project" && lane["membership"]["project"] == project
        });
        let count = lanes.len();
        result["project"] = json!(project);
        result["count"] = json!(count);
    }
    Ok(())
}

#[cfg(test)]
mod project_filter_tests {
    use super::*;
    #[test]
    fn named_project_uses_only_explicit_matching_membership() {
        let original = json!({"as_of":"now","lanes":[
            {"pane_id":"%1","membership":{"scope":"project","project":"global"}},
            {"pane_id":"%2","membership":{"scope":"global","project":null}},
            {"pane_id":"%3","membership":{"scope":"project","project":"alpha"}},
            {"pane_id":"%4","membership":{"scope":"stale","project":"alpha"}},
            {"pane_id":"%5","membership":{"scope":"unknown","project":null}}
        ]});
        let mut result = original.clone();
        filter_project(&mut result, Some(&json!("global"))).unwrap();
        assert_eq!(result["count"], 1);
        assert_eq!(result["lanes"][0]["pane_id"], "%1");
        let mut empty = original.clone();
        filter_project(&mut empty, Some(&json!("missing"))).unwrap();
        assert_eq!(empty["count"], 0);
        assert_eq!(empty["lanes"], json!([]));
        let mut all = original.clone();
        filter_project(&mut all, None).unwrap();
        assert_eq!(all, original);
        assert!(filter_project(&mut all, Some(&json!(" alpha "))).is_err());
    }
}
