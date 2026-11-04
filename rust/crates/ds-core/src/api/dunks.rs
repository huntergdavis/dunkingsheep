//! T39 add/update/remove dunk API adapters.

use super::{python_bool_arg, python_value_string, truthy, Args, HandlerContext};
use crate::error::DunkError;
use crate::flock::dunks::{self, AddArgs, UpdateArgs};
use serde_json::{Map, Value};

fn target_pane(
    context: &HandlerContext<'_>,
    target: Option<&Value>,
) -> Result<Option<Value>, DunkError> {
    let Some(target) = target.filter(|value| truthy(value)) else {
        return Ok(None);
    };
    let panes = context.env.list_panes_grouped();
    if target.is_object() {
        return crate::flock::resolve::resolve_target_dict(&panes, target).map(Some);
    }
    let spec = python_value_string(target);
    let hint = context
        .env
        .terminal()
        .lock()
        .map_err(|_| DunkError::Refused("terminal adapter lock poisoned".into()))?
        .server_error_hint();
    crate::flock::resolve::resolve_target(&panes, &spec, context.self_pane_id(), &hint)
        .cloned()
        .map(Some)
}

fn pane_map(pane: Option<&Value>) -> Result<Option<Map<String, Value>>, DunkError> {
    pane.map(|pane| {
        pane.as_object()
            .cloned()
            .ok_or_else(|| DunkError::Refused("resolved target is not a pane object".into()))
    })
    .transpose()
}

fn value_text(args: &Args, key: &str) -> Option<String> {
    args.get(key)
        .filter(|value| !value.is_null())
        .map(python_value_string)
}

fn interval_text(value: &Value) -> Result<String, DunkError> {
    match value {
        Value::Bool(_) => Err(DunkError::Refused(
            "interval must be a number of minutes".into(),
        )),
        Value::Number(number) => {
            let minutes = crate::github::parse_python_json_float(&number.to_string())
                .map_err(|error| DunkError::Refused(error.into()))?;
            dunks::parse_interval_number(minutes)?;
            Ok(number.to_string())
        }
        _ => {
            let text = python_value_string(value);
            dunks::parse_interval(&text)?;
            Ok(text)
        }
    }
}

fn positive_int_text(value: &Value) -> String {
    match value {
        Value::Bool(value) => i32::from(*value).to_string(),
        Value::Number(number) => number
            .as_i64()
            .map(|value| value.to_string())
            .or_else(|| {
                number
                    .as_f64()
                    .filter(|value| value.is_finite())
                    .map(|value| (value as i64).to_string())
            })
            .unwrap_or_else(|| python_value_string(value)),
        _ => python_value_string(value),
    }
}

fn add_args(args: &Args) -> Result<AddArgs, DunkError> {
    let interval = args
        .get("interval_minutes")
        .filter(|value| !value.is_null())
        .map(interval_text)
        .transpose()?;
    let only_when = value_text(args, "only_when");
    dunks::check_only_when(only_when.as_deref())?;
    let max_sends = args
        .get("max_sends")
        .filter(|value| !value.is_null())
        .map(positive_int_text);
    dunks::check_max_sends(max_sends.as_deref())?;
    Ok(AddArgs {
        text: value_text(args, "text"),
        interval,
        name: value_text(args, "name"),
        start: python_bool_arg(args, "start", true),
        only_when,
        max_sends,
        skip_if_unconsumed: Some(python_bool_arg(args, "skip_if_unconsumed", false).to_string()),
        hold_while_typing: Some(python_bool_arg(args, "hold_while_typing", true).to_string()),
    })
}

/// `dunk_api.h_add_dunk`; Python validates scalar fields before resolving the optional target.
pub fn h_add_dunk(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let parsed = add_args(&args)?;
    let target = target_pane(context, args.get("target"))?;
    let pane = pane_map(target.as_ref())?;
    context
        .env
        .add_dunk(pane.as_ref(), &parsed)
        .map(Value::Object)
}

fn update_args(args: &Args) -> UpdateArgs {
    let mut update = UpdateArgs::default();
    for key in ["text", "name"] {
        if let Some(value) = args.get(key).filter(|value| !value.is_null()) {
            let text = python_value_string(value);
            if key == "text" {
                update.text = Some(text);
            } else {
                update.name = Some(text);
            }
        }
    }
    if let Some(value) = args
        .get("interval_minutes")
        .filter(|value| !value.is_null())
    {
        update.interval = Some(python_value_string(value));
    }
    if let Some(value) = args.get("only_when") {
        update.only_when = Some(
            value
                .as_str()
                .map(str::to_owned)
                .or_else(|| (!value.is_null()).then(|| python_value_string(value))),
        );
    }
    if let Some(value) = args.get("max_sends") {
        update.max_sends = Some((!value.is_null()).then(|| positive_int_text(value)));
    }
    if args
        .get("skip_if_unconsumed")
        .is_some_and(|value| !value.is_null())
    {
        update.skip_if_unconsumed =
            Some(python_bool_arg(args, "skip_if_unconsumed", false).to_string());
    }
    if args
        .get("hold_while_typing")
        .is_some_and(|value| !value.is_null())
    {
        update.hold_while_typing =
            Some(python_bool_arg(args, "hold_while_typing", true).to_string());
    }
    update
}

/// `dunk_api.h_update_dunk`; explicit null clears only the two nullable fields.
pub fn h_update_dunk(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let update = update_args(&args);
    let has_target = args.get("target").is_some_and(|value| !value.is_null());
    if !has_target
        && update.text.is_none()
        && update.name.is_none()
        && update.interval.is_none()
        && update.only_when.is_none()
        && update.max_sends.is_none()
        && update.skip_if_unconsumed.is_none()
        && update.hold_while_typing.is_none()
    {
        return Err(DunkError::Refused("nothing to update".into()));
    }
    let id = args
        .get("id")
        .ok_or_else(|| DunkError::Refused("'id'".into()))?;
    let target = target_pane(context, args.get("target"))?;
    let pane = pane_map(target.as_ref())?;
    context
        .env
        .update_dunk(&python_value_string(id), pane.as_ref(), &update)
        .map(Value::Object)
}

/// `dunk_api.h_remove_dunk`.
pub fn h_remove_dunk(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args
        .get("id")
        .ok_or_else(|| DunkError::Refused("'id'".into()))?;
    context
        .env
        .remove_dunk(&python_value_string(id))
        .map(Value::Object)
}
