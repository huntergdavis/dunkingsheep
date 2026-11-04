//! Direct owner queue handlers, preserving Python evaluation and SQLite binding order.
use super::{python_value_type_name, truthy, Args, HandlerContext};
use crate::context::store::py_repr;
use crate::error::{refusal, DunkError};
use serde_json::{json, Value};

fn strip(value: &Value) -> Result<&str, DunkError> {
    if !truthy(value) {
        return Ok("");
    }
    value.as_str().map(py_strip).ok_or_else(|| {
        refusal(format!(
            "'{}' object has no attribute 'strip'",
            match value {
                Value::Number(number) if !number.to_string().contains(['.', 'e', 'E']) => "int",
                _ => python_value_type_name(value),
            }
        ))
    })
}

fn py_strip(value: &str) -> &str {
    value.trim_matches(|ch: char| ch.is_whitespace() || matches!(ch, '\u{1c}'..='\u{1f}'))
}

// Python str(target) uses Python repr recursively for containers and binary-float
// formatting for JSON float tokens; it is separate from the original SQLite value.
fn target_repr(value: &Value) -> String {
    match value {
        Value::String(value) => py_repr(value),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(target_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{}: {}", py_repr(key), target_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Number(value) => {
            let raw = value.to_string();
            if raw.contains(['.', 'e', 'E']) {
                crate::github::parse_python_json_float(&raw)
                    .map(crate::usage::py_float_repr)
                    .unwrap_or(raw)
            } else {
                raw
            }
        }
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Null => "None".into(),
    }
}

pub fn h_owner_push(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let raw_target = args.get("target").ok_or_else(|| refusal("'target'"))?;
    let target = if truthy(raw_target) {
        raw_target
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| target_repr(raw_target))
    } else {
        String::new()
    };
    let pane = context.flock.delivery_target(
        context.env.delivery_env()?,
        py_strip(&target),
        context.self_pane_id(),
    )?;
    let store = context.env.owner_push()?;
    let raw_body = args.get("body").ok_or_else(|| refusal("'body'"))?;
    let body = strip(raw_body)?;
    if body.is_empty() {
        return Err(refusal("an owner message needs a body"));
    }
    let pane_id = pane
        .get("pane_id")
        .and_then(Value::as_str)
        .ok_or_else(|| refusal("'pane_id'"))?;
    let pushed = store.push_wire(
        pane_id,
        body,
        args.get("target"),
        args.get("subject"),
        context.env.now_s(),
    )?;
    Ok(
        json!({"id": pushed.id, "pane_id": pushed.pane_id, "at": pushed.at,
        "outcome": "stored", "expect": "the lane's own harness drains it (owner_take); no inbox row, nothing typed into the pane"}),
    )
}

pub fn h_owner_poll(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let claimed = strip(args.get("pane_id").unwrap_or(&Value::Null))?;
    let me = context.self_pane_id().filter(|pane| !pane.is_empty()).ok_or_else(|| refusal(
        "owner_poll needs a caller pane: a heartbeat without an identity is a declaration, not an observation"))?;
    if !claimed.is_empty() && claimed != me {
        return Err(refusal(format!("owner_poll for {} refused: this call comes from {}. Only a lane may report its own polling.", py_repr(claimed), py_repr(me))));
    }
    let store = context.env.owner_push()?;
    let default_by = json!("harness");
    let by = args
        .get("by")
        .filter(|value| truthy(value))
        .unwrap_or(&default_by);
    let now = context.env.now_s();
    store.poll_wire(me, by, now)?;
    Ok(json!({"pane_id": me, "at": now, "by": by}))
}

pub fn h_owner_take(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let caller = context
        .self_pane_id()
        .map(|pane| json!(pane))
        .unwrap_or(Value::Null);
    let pane = args
        .get("pane_id")
        .filter(|value| truthy(value))
        .unwrap_or(&caller);
    if !truthy(pane) {
        return Err(refusal("owner_take needs a pane"));
    }
    let store = context.env.owner_push()?;
    let by = args.get("by").filter(|value| truthy(value)).unwrap_or(pane);
    let messages = store.take_preimage(pane, by, 20, context.env.now_s())?;
    Ok(json!({"pane_id": pane, "messages": messages}))
}

#[cfg(test)]
#[path = "owner_push_tests.rs"]
mod tests;
