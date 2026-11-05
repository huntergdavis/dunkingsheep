//! Raw JSON boundary for `dunk_api.h_project_summary` / `dunk_views.project_summary`.

use super::{python_value_string, truthy, Args, HandlerContext};
use crate::error::DunkError;
use crate::flock::project_summary::{project_summary, stores_for_resolved};
use serde_json::Value;

/// Preserve Python's raw project label and validate path before any store/provider access.
pub fn h_project_summary(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let project = args.get("project").unwrap_or(&Value::Null);
    let global = project.is_null() || matches!(project.as_str(), Some("" | "global"));
    let raw_path = if global {
        None
    } else {
        match args.get("path").filter(|value| truthy(value)) {
            Some(Value::String(path)) => Some(path.clone()),
            Some(value) => {
                let kind = match value {
                    Value::Bool(_) => "bool",
                    Value::Number(number) if number.is_f64() => "float",
                    Value::Number(_) => "int",
                    Value::Array(_) => "list",
                    Value::Object(_) => "dict",
                    _ => unreachable!("null is falsey and strings are handled above"),
                };
                return Err(DunkError::Refused(format!(
                    "expected str, bytes or os.PathLike object, not {kind}"
                )));
            }
            None => Some(format!("~/workspace/{}", python_value_string(project))),
        }
    };
    let env = context.env.project_summary_env()?;
    let dir = raw_path
        .as_deref()
        .map(|raw| env.project_dir(raw))
        .transpose()?;
    let stores = stores_for_resolved(dir.as_deref(), env.global_context_root()?);
    // Only actual string labels can select a configured repository. Raw JSON is restored
    // below, so a bool/number/container is never silently converted in the response.
    let label = if global {
        "global"
    } else {
        project.as_str().unwrap_or("")
    };
    let mut result = project_summary(env, label, dir.as_deref(), &stores, context.env.now_s())?;
    if !global {
        result["project"] = project.clone();
    }
    Ok(result)
}

#[cfg(test)]
mod tests;
