//! T1400 resource rule slice: identify callers whose recorded policy would flag or kill them.
//!
//! `task_leases.caller_start` is TEXT because the task caller API preserves `/proc/<pid>/stat`
//! field 22 verbatim. The load sampler's `Caller` uses that same starttime as `i64`; parse only
//! canonical decimal text and omit malformed/missing identities so they cannot produce findings.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use serde_json::{json, Map, Value};

use super::{finding, py_number, TaskData, SOURCE_RULE};
use crate::error::DunkError;
use crate::load::Caller;

type PolicyMap = Map<String, Value>;
type Collect<'a> = dyn Fn(&[Caller], f64, &PolicyMap, &Path) -> Result<Value, DunkError> + 'a;

/// Port `rule_resource_would_kill` through the existing load collector and policy evaluator.
pub fn rule_resource_would_kill(
    data: &TaskData,
    rule: &Value,
    now: f64,
) -> Result<Vec<Value>, DunkError> {
    let evaluate = |sample: &PolicyMap, policy: &PolicyMap| {
        crate::policy::evaluate(sample, policy).map_err(|error| error.to_string())
    };
    rule_resource_would_kill_with(
        data,
        rule,
        now,
        &move |callers, now, policies, state_path| {
            crate::load::collect(callers, now, Some(policies), &evaluate, state_path)
        },
    )
}

fn rule_resource_would_kill_with(
    data: &TaskData,
    rule: &Value,
    now: f64,
    collect: &Collect<'_>,
) -> Result<Vec<Value>, DunkError> {
    let policies_name = rule["policies_path"].as_str().unwrap_or("");
    let policies_path = Path::new(&crate::native_session::expanduser(policies_name)).to_path_buf();
    if !policies_path.exists() {
        return Ok(Vec::new());
    }
    let policies_text = fs::read_to_string(&policies_path).map_err(|error| {
        DunkError::Refused(format!(
            "resource.would_kill: cannot read {}: {error}",
            policies_path.display()
        ))
    })?;
    let policies_value: Value = serde_json::from_str(&policies_text).map_err(|error| {
        DunkError::Refused(format!(
            "resource.would_kill: cannot read {}: {error}",
            policies_path.display()
        ))
    })?;
    let policies = policies_value.as_object().ok_or_else(|| {
        DunkError::Refused(format!(
            "resource.would_kill: cannot read {}: policies must be an object",
            policies_path.display()
        ))
    })?;

    let callers = resource_callers(&data.leases);
    if callers.is_empty() {
        return Ok(Vec::new());
    }
    let state_name = rule["state_path"].as_str().unwrap_or("");
    let state_path = Path::new(&crate::native_session::expanduser(state_name)).to_path_buf();
    let result = collect(&callers, now, policies, &state_path)?;

    let mut leased: BTreeMap<String, &Value> = BTreeMap::new();
    for lease in &data.leases {
        let Some(task) = data
            .tasks
            .iter()
            .find(|task| task.get("id") == lease.get("task_id"))
        else {
            continue;
        };
        if task.get("status").and_then(Value::as_str) == Some("in_progress") {
            leased.insert(value_text(lease.get("pane_id")), task);
        }
    }

    let require_task = truthy(&rule["require_task"]);
    let mut out = Vec::new();
    for item in result
        .get("would_kill")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let agent = value_text(item.get("agent"));
        let pid = item.get("pid").cloned().unwrap_or(Value::Null);
        let pid_text = value_text(Some(&pid));
        let task = leased.get(&agent).copied();
        if require_task && task.is_none() {
            continue;
        }
        let doing = task.map_or_else(
            || "with no leased task".to_string(),
            |task| {
                format!(
                    "while working on {}: {}",
                    value_text(task.get("id")),
                    value_text(task.get("title"))
                )
            },
        );
        let findings = item
            .get("findings")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let measured = findings
            .iter()
            .filter(|finding| finding.get("measured").is_some_and(|value| truthy(value)))
            .map(|finding| {
                format!(
                    "{} {} > {} {}",
                    value_text(finding.get("rule")),
                    value_text(finding.get("measured")),
                    value_text(finding.get("threshold")),
                    value_text(finding.get("window"))
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let kill = item.get("kill").is_some_and(|value| truthy(value));
        let dry_run = item.get("dry_run").cloned().unwrap_or(json!(true));
        out.push(finding(
            "resource.would_kill",
            rule,
            &format!("{agent}:{pid_text}"),
            format!(
                "{agent} would be {} by resource policy {doing}",
                if kill && !truthy(&dry_run) {
                    "killed"
                } else {
                    "flagged"
                }
            ),
            "policy limits".into(),
            if measured.is_empty() {
                "no measured finding".into()
            } else {
                measured
            },
            "sample window".into(),
            json!({
                "agent": agent,
                "pid": pid,
                "task": task.map(|task| task.get("id").cloned().unwrap_or(Value::Null)),
                "kill": kill,
                "dry_run": dry_run,
                "findings": findings,
            }),
            SOURCE_RULE,
            false,
        ));
        if let Some(item) = out.last_mut() {
            item["severity"] = json!("info");
        }
    }
    Ok(out)
}

/// Convert the lease's text starttime to the same integer field used by `load::Caller`.
/// Missing, non-decimal, or noncanonical identity text is skipped (never coerced to zero).
fn resource_callers(leases: &[Value]) -> Vec<Caller> {
    let mut normalized = Vec::new();
    for lease in leases {
        let Some(pid) = lease
            .get("caller_pid")
            .and_then(value_i64)
            .filter(|pid| *pid > 0)
        else {
            continue;
        };
        let Some(start_text) = lease.get("caller_start").and_then(Value::as_str) else {
            continue;
        };
        let Ok(start) = start_text.parse::<i64>() else {
            continue;
        };
        if start < 0 || start.to_string() != start_text {
            continue;
        }
        let name = lease
            .get("caller_name")
            .filter(|value| truthy(value))
            .or_else(|| lease.get("pane_id"))
            .map(|value| value_text(Some(value)))
            .unwrap_or_else(|| format!("pid {pid}"));
        normalized.push(json!({
            "caller_pid": pid,
            "caller_start": start,
            "caller_name": name,
        }));
    }
    crate::load::registered_callers(&normalized)
}

fn value_i64(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_str()?.parse().ok())
}

fn value_text(value: Option<&Value>) -> String {
    value.map(py_text).unwrap_or_else(|| "None".into())
}

fn py_text(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::String(value) => value.clone(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::Number(_) => py_number(value),
        other => other.to_string(),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

#[cfg(test)]
#[path = "rules_resource_tests.rs"]
mod would_kill_tests;
