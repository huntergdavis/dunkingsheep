//! The three Python create handlers. Raw JSON survives until each reached operation.
use super::{python_bool_arg, python_scope_arg, Args, HandlerContext};
use crate::error::DunkError;
use crate::flock::commands_layout::{LayoutBackend, RawSubmit};
use crate::flock::{resolve, Flock};
use crate::terminal::raw::{optional, string, truthy};
use serde_json::{json, Value};

fn arg<'a>(args: &'a Args, key: &str) -> &'a Value {
    args.get(key).unwrap_or(&Value::Null)
}
fn what(kind: &str, label: &Value) -> String {
    format!("{kind} {}", optional(label).unwrap_or_default())
        .trim()
        .into()
}
fn after_create(
    flock: &Flock,
    result: crate::flock::commands_layout::RawCreateResult,
    command: &Value,
    what: &str,
    submit: RawSubmit<'_>,
) -> Result<Value, DunkError> {
    let result = result.map_err(|error| DunkError::Refused(error.to_string()))?;
    let result = result.map_err(|error| {
        DunkError::Refused(format!(
            "the terminal backend could not create the {what}: {error}"
        ))
    })?;
    let pane = ["root_pane", "pane", "agent"]
        .iter()
        .find_map(|key| result.get(key).filter(|v| truthy(v)))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let pane_id = pane.get("pane_id").cloned().unwrap_or(Value::Null);
    let mut out = json!({"workspace":result.get("workspace"), "tab":result.get("tab"), "pane":pane, "pane_id":pane_id, "ran":null});
    if truthy(command) && truthy(&pane_id) {
        let (ran, message) = submit(&string(&pane_id), command)?;
        if ran {
            out["ran"] = command.clone();
        } else {
            out["error"] = json!(format!(
                "created, but running the command failed: {message}"
            ));
        }
    }
    flock.set_last_message(format!("Created {what}"));
    Ok(out)
}

pub fn h_create_workspace(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    context
        .env
        .with_layout_create(&mut |backend, panes, submit| {
            let label = arg(&args, "label");
            let result = backend.create_workspace_raw(
                label,
                arg(&args, "cwd"),
                python_bool_arg(&args, "focus", false),
            );
            let mut out = after_create(
                context.flock,
                result,
                arg(&args, "command"),
                &what("workspace", label),
                submit,
            )?;
            if truthy(&out["pane_id"]) {
                let pane = string(&out["pane_id"]);
                let mailbox = arg(&args, "mailbox");
                if truthy(mailbox) {
                    let mailbox = string(mailbox);
                    context.flock.remember_lane(&pane, mailbox.trim(), true);
                    crate::flock::commands_layout::title_pane(backend, &pane, &mailbox);
                }
                let role = arg(&args, "role");
                if truthy(role) {
                    let label = if truthy(mailbox) { mailbox } else { label };
                    out["role"] = context.flock.lane_register(
                        Some(&pane),
                        Some(&string(role)),
                        python_scope_arg(args.get("provider")).as_deref(),
                        python_scope_arg(args.get("model")).as_deref(),
                        optional(label).as_deref(),
                        context.self_pane_id(),
                        panes,
                    )?;
                }
            }
            Ok(out)
        })
}

pub fn h_create_tab(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    context.env.with_layout_create(&mut |backend, _, submit| {
        let workspace = arg(&args, "workspace");
        // Python resolves self before fetching workspace rows, and leaves absent own workspace null.
        let workspace = if truthy(workspace) {
            let spec = string(workspace).trim().to_string();
            let spec = if matches!(spec.to_lowercase().as_str(), "self" | "me" | "here") {
                let own = resolve::self_pane(context.self_pane_id(), |id| backend.get_pane(id))?;
                optional(own.get("workspace_id").unwrap_or(&Value::Null)).unwrap_or_default()
            } else {
                spec
            };
            let rows = backend.list_workspaces();
            if rows.is_empty() {
                return Err(DunkError::Refused(backend.server_error_hint()));
            }
            let row = resolve::pick(&rows, &spec, "workspace", "workspace_id", "label")?;
            optional(row.get("workspace_id").unwrap_or(&Value::Null))
        } else {
            context
                .self_pane_id()
                .filter(|id| !id.is_empty())
                .and_then(|id| backend.get_pane(id))
                .and_then(|own| optional(own.get("workspace_id").unwrap_or(&Value::Null)))
        };
        let label = arg(&args, "label");
        let result = backend.create_tab_raw(
            workspace.as_deref(),
            label,
            arg(&args, "cwd"),
            python_bool_arg(&args, "focus", false),
        );
        after_create(
            context.flock,
            result,
            arg(&args, "command"),
            &what("tab", label),
            submit,
        )
    })
}

pub fn h_split_pane(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let target = args
        .get("target")
        .ok_or_else(|| DunkError::Refused("'target'".into()))?;
    let direction = optional(arg(&args, "direction"))
        .unwrap_or_else(|| "right".into())
        .to_lowercase();
    if !matches!(direction.as_str(), "right" | "down") {
        return Err(DunkError::Refused(
            "direction must be 'right' or 'down'".into(),
        ));
    }
    context
        .env
        .with_layout_create(&mut |backend: &mut dyn LayoutBackend, _, submit| {
            let pane = if target.is_object() {
                let id = target.get("pane_id").unwrap_or(&Value::Null);
                if !truthy(id) {
                    return Err(DunkError::Refused("target dict has no pane_id".into()));
                }
                backend
                    .list_panes()
                    .into_iter()
                    .find(|pane| {
                        pane.get("pane_id")
                            .is_some_and(|p| crate::terminal::raw::equal(p, id))
                    })
                    .unwrap_or_else(|| target.clone())
            } else {
                let spec = optional(target).unwrap_or_default();
                // resolve_target itself preserves required/self errors before inspecting rows.
                let rows = if spec.trim().is_empty()
                    || (matches!(spec.trim().to_lowercase().as_str(), "self" | "me" | "here")
                        && context.self_pane_id().is_none())
                {
                    vec![]
                } else {
                    backend.list_panes()
                };
                resolve::resolve_target(
                    &rows,
                    &spec,
                    context.self_pane_id(),
                    &backend.server_error_hint(),
                )?
                .clone()
            };
            let pane = string(pane.get("pane_id").unwrap_or(&Value::Null));
            let result = backend.split_pane_raw(
                &pane,
                &direction,
                arg(&args, "cwd"),
                python_bool_arg(&args, "focus", false),
            );
            after_create(context.flock, result, arg(&args, "command"), "pane", submit)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn after_create_preserves_raw_ran_only_when_submit_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let command = json!(["accepted", false]);
        let mut submit = |pane: &str, raw: &Value| {
            assert_eq!(pane, "%1");
            assert_eq!(raw, &command);
            Ok((true, "sent".into()))
        };
        let out = after_create(
            &flock,
            Ok(Ok(json!({"root_pane":{"pane_id":"%1"}}))),
            &command,
            "pane",
            &mut submit,
        )
        .unwrap();
        assert_eq!(out["ran"], command);
        assert_eq!(flock.last_message(), "Created pane");
    }
}
