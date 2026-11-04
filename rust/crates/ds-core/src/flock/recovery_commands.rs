//! Post-upgrade recover/doctor producers. Runtime adapter assembly is a separate gate.
//! Ports dunk_core.Flock's recovery methods; does not replace native-session recovery.

use super::{truthy, Flock};
use crate::{clock, error::DunkError};
use serde_json::{json, Value};

/// Preserve Python conversion errors separately from backend/domain failures.
#[derive(Debug, thiserror::Error)]
pub enum RecoveryCommandError {
    #[error("{message}")]
    Python { kind: &'static str, message: String },
    #[error(transparent)]
    Backend(#[from] DunkError),
}
pub type RecoveryResult<T> = Result<T, RecoveryCommandError>;

/// Required request-scoped services. No method has an empty/success default.
/// The production adapter must use canonical backend/memory/guard services and
/// guarded submission; tests may provide deterministic isolated implementations.
/// Recordable pane identities must be canonical string IDs; raw metadata values
/// remain uncoerced. A malformed identity is refused before inserting a launch.
pub trait RecoverEnv {
    fn list_panes(&self) -> RecoveryResult<Vec<Value>>;
    fn get_pane(&self, pane_id: &Value) -> RecoveryResult<Option<Value>>;
    fn read_visible(&self, pane_id: &Value, lines: usize) -> RecoveryResult<Option<Value>>;
    fn version(&self) -> RecoveryResult<Value>;
    fn guard_status(&self) -> RecoveryResult<Value>;
    fn has_memory(&self) -> bool;
    fn latest_session(&self, harness: &str, cwd: &Value) -> RecoveryResult<Option<Value>>;
    /// Final backend safety checks remain mandatory even after the producer's fresh get.
    fn submit_command(&self, pane_id: &Value, command: &str) -> RecoveryResult<(Value, Value)>;
}

fn python_error(kind: &'static str, message: impl Into<String>) -> RecoveryCommandError {
    RecoveryCommandError::Python {
        kind,
        message: message.into(),
    }
}
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        Value::Number(n) if n.is_f64() => "float",
        Value::Number(_) => "int",
    }
}
fn field<'a>(value: &'a Value, key: &str) -> RecoveryResult<&'a Value> {
    let object = value.as_object().ok_or_else(|| {
        python_error(
            "AttributeError",
            format!("'{}' object has no attribute 'get'", type_name(value)),
        )
    })?;
    Ok(object.get(key).unwrap_or(&Value::Null))
}
fn required<'a>(value: &'a Value, key: &str) -> RecoveryResult<&'a Value> {
    let object = value.as_object().ok_or_else(|| {
        python_error(
            "TypeError",
            format!("'{}' object is not subscriptable", type_name(value)),
        )
    })?;
    object.get(key).ok_or_else(|| {
        python_error(
            "KeyError",
            crate::recovery_native::prompt_python_string_repr(key),
        )
    })
}
fn either<'a>(first: &'a Value, second: &'a Value) -> &'a Value {
    if truthy(first) {
        first
    } else {
        second
    }
}
fn py_string(value: &Value) -> String {
    if let Value::String(text) = value {
        text.clone()
    } else {
        crate::api::python_value_repr(value)
    }
}
// Comparison only: never normalize the raw value retained in launch memory.
// Like memory/service.rs's private numeric cache-key primitive, integral floats
// become their exact fixed-point integer digits; integer tokens never pass via f64.
#[derive(PartialEq)]
enum MetadataNumber {
    Integer(String),
    Float(f64),
}
fn metadata_number(value: &Value) -> Option<MetadataNumber> {
    let raw = match value {
        Value::Bool(value) => {
            return Some(MetadataNumber::Integer(
                if *value { "1" } else { "0" }.into(),
            ))
        }
        Value::Number(value) => value.to_string(),
        _ => return None,
    };
    if !raw.contains(['.', 'e', 'E']) {
        return Some(MetadataNumber::Integer(if raw == "-0" {
            "0".into()
        } else {
            raw
        }));
    }
    let number = crate::github::parse_python_json_float(&raw).ok()?;
    if number.is_finite() && number.fract() == 0.0 {
        let digits = if number == 0.0 {
            "0".into()
        } else {
            format!("{number:.0}")
        };
        Some(MetadataNumber::Integer(digits))
    } else {
        // Existing exponent carriers can represent infinity. No new NaN encoding.
        Some(MetadataNumber::Float(number))
    }
}
fn python_metadata_equal(left: &Value, right: &Value) -> bool {
    if let (Some(left), Some(right)) = (metadata_number(left), metadata_number(right)) {
        return left == right;
    }
    match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::String(left), Value::String(right)) => left == right,
        (Value::Array(left), Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| python_metadata_equal(left, right))
        }
        (Value::Object(left), Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, left)| {
                    right
                        .get(key)
                        .is_some_and(|right| python_metadata_equal(left, right))
                })
        }
        _ => false,
    }
}

fn py_space(ch: char) -> bool {
    ch.is_whitespace() || matches!(ch, '\u{1c}'..='\u{1f}')
}

/// shlex.quote deliberately accepts falsey nonstrings as the empty word.
fn quote(value: &Value) -> RecoveryResult<String> {
    if !truthy(value) {
        return Ok("''".into());
    }
    let text = value.as_str().ok_or_else(|| {
        python_error(
            "TypeError",
            format!(
                "expected string or bytes-like object, got '{}'",
                type_name(value)
            ),
        )
    })?;
    if text
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        Ok(text.into())
    } else {
        Ok(format!("'{}'", text.replace('\'', "'\"'\"'")))
    }
}
fn join_argv(argv: &Value) -> RecoveryResult<String> {
    let words: Vec<Value> = match argv {
        Value::Array(values) => values.clone(),
        Value::Object(values) => values.keys().cloned().map(Value::String).collect(),
        Value::String(text) => text.chars().map(|c| Value::String(c.to_string())).collect(),
        _ => {
            return Err(python_error(
                "TypeError",
                format!("'{}' object is not iterable", type_name(argv)),
            ))
        }
    };
    words
        .iter()
        .map(quote)
        .collect::<RecoveryResult<Vec<_>>>()
        .map(|v| v.join(" "))
}
fn has_resume_flag(argv: &Value) -> bool {
    ["--session", "--resume", "resume"]
        .iter()
        .any(|flag| match argv {
            Value::Array(values) => values.iter().any(|v| v == flag),
            Value::Object(values) => values.contains_key(*flag),
            Value::String(text) => text.contains(flag),
            _ => false, // join_argv already rejected non-iterables.
        })
}
fn resume_command(agent: &Value, session: &Value) -> RecoveryResult<Option<String>> {
    if !truthy(agent) || !truthy(session) {
        return Ok(None);
    }
    let prefix = match agent.as_str() {
        Some("pi") => "pi --session",
        Some("codex") => "codex resume",
        Some("claude") => "claude --resume",
        _ => return Ok(None),
    };
    Ok(Some(format!("{prefix} {}", quote(session)?)))
}
fn empty_shell(pane: &Value) -> RecoveryResult<bool> {
    if !truthy(pane) || truthy(field(pane, "agent")?) {
        return Ok(false);
    }
    let status = field(pane, "agent_status")?;
    if truthy(status) && status != "unknown" {
        return Ok(false);
    }
    let raw = field(pane, "terminal_title_stripped")?;
    let title = if truthy(raw) {
        raw.as_str().ok_or_else(|| {
            python_error(
                "AttributeError",
                format!("'{}' object has no attribute 'strip'", type_name(raw)),
            )
        })?
    } else {
        ""
    }
    .trim_matches(py_space);
    // Python ^[^@\s]+@[^:\s]+:, including Python's four extra whitespace controls.
    let Some((user, host)) = title.split_once('@') else {
        return Ok(false);
    };
    let Some((host, _)) = host.split_once(':') else {
        return Ok(false);
    };
    Ok(!user.is_empty()
        && !host.is_empty()
        && !user.chars().any(py_space)
        && !host.chars().any(py_space))
}
fn with_cwd(cwd: &Value, command: String) -> RecoveryResult<String> {
    if truthy(cwd) && !command.is_empty() && !command.starts_with("cd ") {
        Ok(format!("cd {} && {command}", quote(cwd)?))
    } else {
        Ok(command)
    }
}
fn launch_for(
    env: &dyn RecoverEnv,
    dunk: Option<&Value>,
    entry: &Value,
    cwd: &Value,
) -> RecoveryResult<Option<(String, &'static str)>> {
    let empty = json!({});
    let entry = either(entry, &empty);
    let argv = field(entry, "argv")?;
    let joined = if truthy(argv) {
        Some(join_argv(argv)?)
    } else {
        None
    };
    if has_resume_flag(argv) {
        if let Some(command) = joined {
            return Ok(Some((command, "recorded-argv")));
        }
    }
    let fallback = dunk
        .map(|d| field(d, "target_agent"))
        .transpose()?
        .unwrap_or(&Value::Null);
    let agent = either(field(entry, "agent")?, fallback);
    let mut session = field(entry, "session")?.clone();
    let mut source = "recorded-session";
    if !truthy(&session) && truthy(agent) && truthy(cwd) && env.has_memory() {
        if let Some(harness @ ("pi" | "codex" | "claude" | "opencode")) = agent.as_str() {
            if let Some(row) = env.latest_session(harness, cwd)? {
                if truthy(&row) {
                    session = required(&row, "id")?.clone();
                    source = "memory-latest";
                }
            }
        }
    }
    match resume_command(agent, &session)? {
        Some(command) => Ok(Some((command, source))),
        None => Ok(joined.map(|command| (command, "recorded-argv"))),
    }
}
fn pane_index(panes: &[Value]) -> RecoveryResult<Vec<(Value, &Value)>> {
    panes
        .iter()
        .map(|pane| {
            let id = field(pane, "pane_id")?;
            if matches!(id, Value::Array(_) | Value::Object(_)) {
                return Err(python_error(
                    "TypeError",
                    format!("unhashable type: '{}'", type_name(id)),
                ));
            }
            Ok((id.clone(), pane))
        })
        .collect()
}
fn pane_for<'a>(panes: &'a [(Value, &'a Value)], id: &Value) -> Option<&'a Value> {
    panes
        .iter()
        .rev()
        .find(|(key, _)| key == id)
        .map(|(_, pane)| *pane)
}
fn normpath(value: &Value) -> RecoveryResult<String> {
    let path = value.as_str().ok_or_else(|| {
        python_error(
            "TypeError",
            format!(
                "expected str, bytes or os.PathLike object, not {}",
                type_name(value)
            ),
        )
    })?;
    if path.is_empty() {
        return Ok(".".into());
    }
    let root = if path.starts_with("//") && !path.starts_with("///") {
        "//"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut parts = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => (),
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            ".." if !root.is_empty() => (),
            _ => parts.push(part),
        }
    }
    let result = format!("{root}{}", parts.join("/"));
    Ok(if result.is_empty() {
        ".".into()
    } else {
        result
    })
}
impl Flock {
    fn recovery_dunks(&self) -> Vec<Value> {
        self.dunks
            .snapshot()
            .1
            .into_iter()
            .map(|row| {
                Value::Object(
                    row.into_iter()
                        .map(|(key, value)| (key.into(), value))
                        .collect(),
                )
            })
            .collect()
    }
    fn doctor_warnings(&self, panes: &[(Value, &Value)]) -> RecoveryResult<Vec<Value>> {
        let mut warnings = Vec::new();
        for dunk in self.recovery_dunks() {
            let id = field(&dunk, "target_pane_id")?;
            if !truthy(field(&dunk, "running")?) || !truthy(id) {
                continue;
            }
            let Some(pane) = pane_for(panes, id) else {
                warnings.push(json!({"dunk": dunk["id"], "pane_id": id, "kind": "pane-missing", "detail": "target pane no longer exists"}));
                continue;
            };
            let target_agent = field(&dunk, "target_agent")?;
            let live_agent = field(pane, "agent")?;
            if truthy(target_agent) && truthy(live_agent) && target_agent != live_agent {
                warnings.push(json!({"dunk": dunk["id"], "pane_id": id, "kind": "agent-mismatch", "target_agent": target_agent, "pane_agent": live_agent, "detail": format!("dunk says {}, pane is {}", py_string(target_agent), py_string(live_agent))}));
            }
            let target_cwd = field(&dunk, "target_cwd")?;
            let live_cwd = either(field(pane, "cwd")?, field(pane, "foreground_cwd")?);
            if truthy(target_cwd)
                && truthy(live_cwd)
                && normpath(target_cwd)? != normpath(live_cwd)?
            {
                warnings.push(json!({"dunk": dunk["id"], "pane_id": id, "kind": "cwd-mismatch", "target_cwd": target_cwd, "pane_cwd": live_cwd, "detail": format!("dunk says {}, pane is {}", py_string(target_cwd), py_string(live_cwd))}));
            }
        }
        Ok(warnings)
    }

    // The recovery seam carries raw Python metadata. Keep its truthiness/error
    // ordering while updating the same canonical launch map (not a second store).
    fn snapshot_recovery_launches(&self, panes: &[Value]) -> RecoveryResult<()> {
        if !self.persist_enabled {
            return Ok(());
        }
        let mut changed = false;
        {
            let mut launches = self.launches.lock().expect("launches lock");
            for pane in panes {
                let empty = json!({});
                let session = either(field(pane, "agent_session")?, &empty);
                let pane_agent = field(pane, "agent")?;
                let agent = if truthy(pane_agent) {
                    pane_agent
                } else {
                    field(session, "agent")?
                };
                let value = field(session, "value")?;
                if !truthy(agent) || !truthy(value) {
                    continue;
                }
                // Canonical launch identities are backend pane IDs, not JSON keys
                // synthesized from arbitrary metadata.
                let id = field(pane, "pane_id")?.as_str().ok_or_else(|| {
                    DunkError::Identity("recovery recording requires a string pane_id".into())
                })?;
                let entry = launches.entry(id.to_owned()).or_default();
                let fields = [
                    ("agent", agent),
                    ("session", value),
                    ("session_kind", field(session, "kind")?),
                    (
                        "cwd",
                        either(field(pane, "cwd")?, field(pane, "foreground_cwd")?),
                    ),
                    ("terminal_title", field(pane, "terminal_title_stripped")?),
                ];
                for (key, value) in fields {
                    if !value.is_null()
                        && entry
                            .get(key)
                            .is_none_or(|old| !python_metadata_equal(old, value))
                    {
                        entry.insert(key.to_owned(), value.clone());
                        changed = true;
                    }
                }
                if changed && entry.get("source").is_none_or(Value::is_null) {
                    entry.insert("source".into(), json!("herdr"));
                }
            }
        }
        if changed {
            self.save_launches();
        }
        Ok(())
    }

    /// Python Flock.recover. Caller authorization belongs to the consuming API leaf.
    pub fn recover(
        &self,
        env: &dyn RecoverEnv,
        apply: bool,
        record: bool,
    ) -> RecoveryResult<Value> {
        let panes = env.list_panes()?;
        let by_id = pane_index(&panes)?;
        if record {
            self.snapshot_recovery_launches(&panes)?;
        }
        let mut plan = Vec::new();
        let mut seen = Vec::new();
        for dunk in self.recovery_dunks() {
            let id = field(&dunk, "target_pane_id")?;
            if !truthy(field(&dunk, "running")?) || !truthy(id) {
                continue;
            }
            seen.push(id.clone());
            let pane = pane_for(&by_id, id);
            let cwd = either(
                pane.filter(|p| truthy(p))
                    .map(|p| field(p, "cwd"))
                    .transpose()?
                    .unwrap_or(&Value::Null),
                field(&dunk, "target_cwd")?,
            );
            let agent = either(
                pane.filter(|p| truthy(p))
                    .map(|p| field(p, "agent"))
                    .transpose()?
                    .unwrap_or(&Value::Null),
                field(&dunk, "target_agent")?,
            );
            let mut item = json!({"pane_id": id, "dunk": dunk["id"], "agent": agent, "cwd": cwd, "target_agent": dunk["target_agent"], "target_cwd": dunk["target_cwd"]});
            match pane {
                None => {
                    item["action"] = json!("pane-missing");
                    item["reason"] = json!("target pane no longer exists");
                }
                Some(pane) if truthy(field(pane, "agent")?) => {
                    item["action"] = json!("already-running");
                    item["reason"] = json!(format!("{} is live", py_string(field(pane, "agent")?)));
                }
                Some(pane) if !empty_shell(pane)? => {
                    item["action"] = json!("not-empty-shell");
                    item["reason"] = json!("pane is not a bare shell");
                }
                Some(_) => {
                    let entry = id
                        .as_str()
                        .and_then(|id| self.launch(id))
                        .map(Value::Object)
                        .unwrap_or(Value::Null);
                    if let Some((command, source)) = launch_for(env, Some(&dunk), &entry, cwd)? {
                        item["action"] = json!("launch");
                        item["command"] =
                            json!(with_cwd(either(field(&dunk, "target_cwd")?, cwd), command)?);
                        item["source"] = json!(source);
                    } else {
                        item["action"] = json!("no-session");
                        item["reason"] =
                            json!("no recorded launch or unique session; start manually");
                    }
                }
            }
            plan.push(item);
        }
        // Copy canonical state, never hold launch or dunk locks across backend/memory calls.
        let launches = self.launches.lock().expect("launches lock").clone();
        for (id, entry) in launches {
            let id = Value::String(id);
            if seen.contains(&id) {
                continue;
            }
            let Some(pane) = pane_for(&by_id, &id) else {
                continue;
            };
            if !empty_shell(pane)? {
                continue;
            }
            let entry = Value::Object(entry);
            let cwd = field(&entry, "cwd")?;
            if let Some((command, source)) = launch_for(env, None, &entry, cwd)? {
                plan.push(json!({"pane_id": id, "dunk": null, "agent": field(&entry, "agent")?, "cwd": cwd, "action": "launch", "command": with_cwd(cwd, command)?, "source": source, "reason": "recorded pane (no running dunk)"}));
            }
        }
        let warnings = self.doctor_warnings(&by_id)?;
        let mut applied = Vec::new();
        if apply {
            for item in &mut plan {
                if item["action"] != "launch" {
                    continue;
                }
                let pane = env.get_pane(&item["pane_id"])?;
                // Python evaluates pane.get("agent") before _is_empty_shell, even
                // for falsey non-object backend responses (which must raise).
                let still_empty = match pane.as_ref() {
                    None => false,
                    Some(pane) => !truthy(field(pane, "agent")?) && empty_shell(pane)?,
                };
                if still_empty {
                    let (ok, detail) = env.submit_command(
                        &item["pane_id"],
                        item["command"].as_str().expect("constructed command"),
                    )?;
                    item["applied"] = json!(truthy(&ok));
                    item["apply_detail"] = detail;
                    if truthy(&ok) {
                        self.record_launch(
                            item["pane_id"].as_str().expect("canonical pane id"),
                            &[
                                ("agent", item["agent"].clone()),
                                ("command", item["command"].clone()),
                                ("cwd", item["cwd"].clone()),
                                ("source", json!("recover")),
                                ("applied_at", json!(clock::now_s())),
                            ],
                        );
                        applied.push(item["pane_id"].clone());
                    }
                } else {
                    item["applied"] = json!(false);
                    item["apply_reason"] = json!("pane is no longer an empty shell");
                }
            }
        }
        let ok = warnings.is_empty()
            && plan
                .iter()
                .all(|item| matches!(item["action"].as_str(), Some("launch" | "already-running")));
        let version = env.version()?;
        let count = self.launches.lock().expect("launches lock").len();
        Ok(
            json!({"ok": ok, "apply": apply, "herdr": version, "plan": plan, "warnings": warnings, "applied": applied, "launches": count}),
        )
    }

    /// Read-only Python Flock.doctor. Only guard-status exceptions become a report.
    pub fn doctor(&self, env: &dyn RecoverEnv) -> RecoveryResult<Value> {
        let panes = env.list_panes()?;
        let by_id = pane_index(&panes)?;
        let read_ok = match panes.first() {
            Some(pane) => Some(env.read_visible(required(pane, "pane_id")?, 3)?.is_some()),
            None => None,
        };
        let guard = env
            .guard_status()
            .unwrap_or_else(|error| json!({"error": error.to_string()}));
        let warnings = self.doctor_warnings(&by_id)?;
        let guard_ok = truthy(either(
            field(&guard, "shadowing_herdr")?,
            field(&guard, "installed")?,
        ));
        let version = env.version()?;
        let mut agents = 0;
        let mut sessions = 0;
        for pane in &panes {
            agents += usize::from(truthy(field(pane, "agent")?));
            sessions += usize::from(truthy(field(pane, "agent_session")?));
        }
        let pane_get = match panes.first() {
            Some(pane) => env.get_pane(required(pane, "pane_id")?)?.is_some(),
            None => false,
        };
        let count = self.launches.lock().expect("launches lock").len();
        Ok(
            json!({"ok": warnings.is_empty() && read_ok == Some(true) && guard_ok, "herdr_version": version, "panes": {"total": panes.len(), "with_agent": agents, "with_session": sessions}, "surfaces": {"pane_list": !panes.is_empty(), "pane_get": pane_get, "read_visible": read_ok, "guard": guard}, "warnings": warnings, "launches": count}),
        )
    }
}

#[cfg(test)]
#[path = "recovery_commands_tests.rs"]
mod tests;
