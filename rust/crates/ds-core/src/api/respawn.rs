//! The T37c `respawn_pane` API adapter.

use super::{python_bool_arg, Args, HandlerContext};
use crate::error::DunkError;
use serde_json::Value;

fn string_argv(value: Option<&Value>) -> Option<Vec<String>> {
    let values = value?.as_array()?;
    if values.is_empty() {
        return None;
    }
    values
        .iter()
        .map(|value| value.as_str().map(str::to_owned))
        .collect()
}

struct ParsedRespawnArgs<'a> {
    pane_id: &'a Value,
    argv: Vec<String>,
    rollback_argv: Option<Vec<String>>,
    expected_pid: Option<&'a Value>,
    expected_start: Option<&'a Value>,
    confirm_idle: bool,
    reason: Option<&'a Value>,
}

fn parse_args(args: &Args) -> Result<ParsedRespawnArgs<'_>, DunkError> {
    let argv = string_argv(args.get("argv")).ok_or_else(|| {
        DunkError::Refused("respawn_pane requires argv as a list of strings".into())
    })?;
    let rollback_argv = match args.get("rollback_argv") {
        None | Some(Value::Null) => None,
        Some(value) => Some(string_argv(Some(value)).ok_or_else(|| {
            DunkError::Refused("respawn_pane rollback_argv must be a list of strings".into())
        })?),
    };
    let pane_id = args
        .get("pane")
        .ok_or_else(|| DunkError::Refused("'pane'".into()))?;

    Ok(ParsedRespawnArgs {
        pane_id,
        argv,
        rollback_argv,
        expected_pid: args.get("expected_pid"),
        expected_start: args.get("expected_start"),
        confirm_idle: python_bool_arg(args, "confirm_idle", false),
        reason: args.get("reason"),
    })
}

/// `dunk_api.h_respawn_pane`; keep argument validation ahead of pane access and preserve
/// optional values as raw JSON for the guarded Flock operation.
pub fn h_respawn_pane(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let parsed = parse_args(&args)?;

    let mut operation = |services: &crate::flock::respawn::RespawnServices<'_>| {
        context.flock.respawn_pane(
            services,
            crate::flock::respawn::RespawnRequest {
                pane_id: parsed.pane_id,
                argv: &parsed.argv,
                rollback_argv: parsed.rollback_argv.as_deref(),
                expected_pid: parsed.expected_pid,
                expected_start: parsed.expected_start,
                confirm_idle: parsed.confirm_idle,
                reason: parsed.reason,
                self_pane_id: context.self_pane_id(),
            },
        )
    };
    context.env.with_respawn_pane_env(&mut operation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn h_respawn_pane_preserves_raw_arguments_and_python_bool_conversion() {
        let args = json!({
            "pane": {"pane_id": "w1:p1"},
            "argv": ["agent", "--literal", "$(keep)"],
            "rollback_argv": ["old-agent"],
            "expected_pid": {"raw": 7},
            "expected_start": ["start", false],
            "confirm_idle": [],
            "reason": {"because": "raw"}
        })
        .as_object()
        .unwrap()
        .clone();
        let parsed = parse_args(&args).unwrap();
        assert_eq!(parsed.argv, ["agent", "--literal", "$(keep)"]);
        assert_eq!(
            parsed
                .rollback_argv
                .as_deref()
                .map(|values| values[0].as_str()),
            Some("old-agent")
        );
        assert_eq!(parsed.pane_id, &args["pane"]);
        assert_eq!(parsed.expected_pid, Some(&args["expected_pid"]));
        assert_eq!(parsed.expected_start, Some(&args["expected_start"]));
        assert_eq!(parsed.reason, Some(&args["reason"]));
        assert!(!parsed.confirm_idle, "Python bool([]) is false");

        let truthy_string = json!({
            "pane": "w1:p1",
            "argv": ["agent"],
            "confirm_idle": " YES "
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(parse_args(&truthy_string).unwrap().confirm_idle);

        let false_string = json!({
            "pane": "w1:p1",
            "argv": ["agent"],
            "confirm_idle": "false"
        })
        .as_object()
        .unwrap()
        .clone();
        assert!(!parse_args(&false_string).unwrap().confirm_idle);
    }
}
