//! Guarded same-pane replacement. API validation/wiring remains the t1562 consumer.
use super::{
    agent_verify::{self, VerifyBackend},
    clear_box::ClearBoxEnv,
    commands_inbox::InboxEnv,
    Flock,
};
use crate::{config::Env, error::DunkError, terminal::Terminal};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

/// Borrow existing request services; no independent store or cached idle verdict.
pub struct RespawnServices<'a> {
    pub terminal: &'a Mutex<Box<dyn Terminal>>,
    pub delivery: &'a dyn ClearBoxEnv,
    pub inbox: &'a dyn InboxEnv,
    pub runtime: &'a tokio::runtime::Handle,
    pub env: &'a Env,
    pub verify_seconds: f64,
    #[cfg(test)]
    pub before_native_recheck: Option<&'a dyn Fn()>,
}
pub struct RespawnRequest<'a> {
    pub pane_id: &'a Value,
    pub argv: &'a [String],
    pub rollback_argv: Option<&'a [String]>,
    pub expected_pid: Option<&'a Value>,
    pub expected_start: Option<&'a Value>,
    pub confirm_idle: bool,
    pub reason: Option<&'a Value>,
    pub self_pane_id: Option<&'a str>,
}
fn refusal(text: impl Into<String>) -> DunkError {
    DunkError::Refused(text.into())
}
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(v) => *v,
        Value::String(v) => !v.is_empty(),
        Value::Array(v) => !v.is_empty(),
        Value::Object(v) => !v.is_empty(),
        Value::Number(v) => v.as_f64() != Some(0.0),
    }
}
fn py_str(value: &Value) -> String {
    match value {
        Value::String(v) => v.clone(),
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        other => other.to_string(),
    }
}
fn field<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
}
fn native_error(error: crate::native_session::NativeSessionError) -> DunkError {
    let text = format!("respawn-pane refused: {error}");
    if error.retryable() {
        DunkError::Retryable(text)
    } else {
        refusal(text)
    }
}
impl Flock {
    pub fn respawn_pane(
        &self,
        services: &RespawnServices<'_>,
        request: RespawnRequest<'_>,
    ) -> Result<Value, DunkError> {
        self.require_superuser(request.self_pane_id, "respawn-pane", services.delivery)?;
        let pane_id = if truthy(request.pane_id) {
            py_str(request.pane_id).trim().to_owned()
        } else {
            String::new()
        };
        if pane_id.is_empty() {
            return Err(refusal("respawn-pane requires a pane"));
        }
        if request.argv.is_empty() {
            return Err(refusal("respawn-pane requires an argv list"));
        }
        if !request.confirm_idle {
            return Err(refusal(
                "respawn-pane requires --confirm-idle with positive evidence",
            ));
        }
        let (Some(expected_pid), Some(expected_start)) = (
            request.expected_pid.filter(|v| !v.is_null()),
            request.expected_start.filter(|v| !v.is_null()),
        ) else {
            return Err(refusal(
                "respawn-pane requires --expected-pid and --expected-start",
            ));
        };
        let pane = services
            .terminal
            .lock()
            .expect("terminal lock")
            .get_pane(&pane_id)
            .ok_or_else(|| refusal(format!("respawn-pane: pane {pane_id} is not visible")))?;
        let pane_pid = pane
            .pane_pid
            .ok_or_else(|| refusal(format!("respawn-pane: pane {pane_id} has no live pane pid")))?;
        if integer(expected_pid)? != pane_pid.to_string() {
            return Err(refusal(format!(
                "respawn-pane: pane {pane_id} pid is {pane_pid}, expected {}",
                py_str(expected_pid)
            )));
        }
        let expected_start = py_str(expected_start);
        if crate::proc::process_start(pane_pid).unwrap_or_else(|| "None".into()) != expected_start {
            return Err(refusal(format!("respawn-pane: pane {pane_id} process start does not match expected {expected_start}")));
        }
        if !services
            .terminal
            .lock()
            .expect("terminal lock")
            .supports_guarded_respawn()
        {
            return Err(refusal(
                "respawn-pane: the active backend does not support guarded respawn",
            ));
        }
        let pane_value = super::lanes::pane_to_json(&pane, None);
        let pane_agent = field(&pane_value, "agent").unwrap_or("").to_lowercase();
        if pane_agent == "codex" {
            let shared = crate::native_session::realpath(
                &crate::terminal::tmux_registry::expand_home("~/.codex", services.env)
                    .to_string_lossy(),
            );
            for part in request
                .argv
                .iter()
                .chain(request.rollback_argv.unwrap_or(&[]))
            {
                if let Some(home) = part.strip_prefix("CODEX_HOME=") {
                    let home = crate::terminal::tmux_registry::expand_home(home, services.env);
                    if crate::native_session::realpath(&home.to_string_lossy()) == shared {
                        return Err(refusal("respawn-pane refused: use an isolated per-lane CODEX_HOME, not the shared ~/.codex home"));
                    }
                }
            }
        }
        {
            let turns = self.turn_states.lock().expect("turn lock");
            let outbox = self.outbox.lock().expect("outbox lock");
            if turns
                .get(&pane_id)
                .is_some_and(|t| t.ended_at.is_none_or(|at| at == 0.0))
            {
                return Err(refusal(format!(
                    "respawn-pane refused: pane {pane_id} has an active turn"
                )));
            }
            let queued = outbox
                .get(&pane_id)
                .map(|messages| messages.iter().filter(|m| m.status == "queued").count())
                .unwrap_or(0);
            if queued > 0 {
                return Err(refusal(format!(
                    "respawn-pane refused: pane {pane_id} has {queued} queued outbox message(s)"
                )));
            }
        }
        let tasks = self
            .stores()
            .tasks
            .ok_or_else(|| refusal("respawn-pane refused: task store is unavailable"))?;
        if tokio::runtime::Handle::try_current().is_ok() {
            return Err(refusal("respawn-pane requires a standard request thread"));
        }
        let leases = services
            .runtime
            .block_on(tasks.leases_for_pane(&pane_id, None))?;
        if !leases.is_empty() {
            return Err(refusal(format!(
                "respawn-pane refused: pane {pane_id} has a live task lease ({})",
                leases
                    .iter()
                    .map(|l| l.task_id.clone())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let unread = self
            .inbox_count(services.inbox, None, Some(&pane_id))
            .map_err(|e| refusal(format!("respawn-pane refused: unread count failed: {e}")))?
            .unread;
        if unread != 0 {
            return Err(refusal(format!(
                "respawn-pane refused: pane {pane_id} has {unread} unread message(s)"
            )));
        }
        let delivery = self.delivery_state(services.delivery, &pane_id, None, None, None);
        if delivery["state"] != "ready" || delivery.get("foreign_text").is_some_and(truthy) {
            return Err(refusal(format!(
                "respawn-pane refused: pane {pane_id} is not confirmed idle (state={}, reason={})",
                repr(&delivery["state"]),
                repr(&delivery["reason"])
            )));
        }
        let snapshot = if pane_agent.trim() == "codex"
            || request.argv.iter().any(|v| basename(v) == "codex")
        {
            crate::native_session::check_native_session_quiescence(request.argv, Some(&pane.cwd))
                .map_err(native_error)?
        } else {
            None
        };
        let old = Value::Object(self.launch(&pane_id).unwrap_or_default());
        let rollback = request
            .rollback_argv
            .filter(|v| !v.is_empty())
            .map(<[String]>::to_vec)
            .unwrap_or_else(|| {
                old["argv"]
                    .as_array()
                    .map(|a| a.iter().map(py_str).collect())
                    .unwrap_or_default()
            });
        if rollback.is_empty() {
            return Err(refusal(format!(
                "respawn-pane refused: no rollback argv for pane {pane_id}"
            )));
        }
        let reason = request
            .reason
            .filter(|v| truthy(v))
            .map(py_str)
            .unwrap_or_else(|| "respawn_pane".into());
        let old_agent = field(&old, "agent")
            .unwrap_or(&pane_agent)
            .trim()
            .to_lowercase();
        let mut rollback_names = BTreeSet::new();
        if !old_agent.is_empty() {
            rollback_names.insert(old_agent.clone());
        }
        let mut names = rollback_names.clone();
        for part in request.argv.iter().chain(&rollback) {
            let base = basename(part);
            if crate::proc::AGENT_NAMES.contains(&base.as_str()) {
                names.insert(base);
            }
        }
        for part in &rollback {
            let base = basename(part);
            if crate::proc::AGENT_NAMES.contains(&base.as_str()) {
                rollback_names.insert(base);
            }
        }
        let failure_context = Failure {
            services,
            pane_id: &pane_id,
            old_pid: pane_pid,
            rollback: &rollback,
            old_agent: &old_agent,
            names: &rollback_names,
        };
        #[cfg(test)]
        if let Some(hook) = services.before_native_recheck {
            hook();
        }
        if let Some(snapshot) = snapshot {
            snapshot.assert_unchanged().map_err(native_error)?;
        }
        let result = services
            .terminal
            .lock()
            .expect("terminal lock")
            .guarded_respawn(&pane_id, request.argv, pane_pid, &expected_start);
        let replacement = match result {
            Ok(pane) => pane,
            Err(error) => {
                let current = services
                    .terminal
                    .lock()
                    .expect("terminal lock")
                    .get_pane(&pane_id)
                    .and_then(|p| p.pane_pid);
                return Err(self.respawn_failed(
                    &failure_context,
                    current,
                    format!("respawn-pane failed: {error}"),
                ));
            }
        };
        let Some(new_pid) = replacement.pane_pid else {
            return Err(self.respawn_failed(
                &failure_context,
                None,
                "respawn-pane returned no new pane pid".into(),
            ));
        };
        let failure = if new_pid == pane_pid || !crate::proc::process_alive(new_pid, None) {
            Some("respawn-pane did not produce a live replacement process".into())
        } else {
            verify(services, &pane_id, new_pid, &old_agent, &names)
                .err()
                .map(|e| e.to_string())
        };
        if let Some(message) = failure {
            return Err(self.respawn_failed(&failure_context, Some(new_pid), message));
        }
        let refs: Vec<_> = request.argv.iter().map(String::as_str).collect();
        self.record_launch(
            &pane_id,
            &[
                (
                    "agent",
                    old.get("agent")
                        .filter(|v| truthy(v))
                        .cloned()
                        .unwrap_or_else(|| pane_value["agent"].clone()),
                ),
                ("argv", json!(request.argv)),
                ("command", json!(crate::terminal::tmux::shell_join(&refs))),
                (
                    "cwd",
                    old.get("cwd")
                        .filter(|v| truthy(v))
                        .cloned()
                        .unwrap_or_else(|| json!(pane.cwd)),
                ),
                ("source", json!("respawn_pane")),
                ("previous", old),
                ("reason", json!(reason)),
            ],
        );
        self.respawn_metric("pane.respawn", &pane_id, "ok", Some(&reason));
        *self.last_message.lock().expect("last message lock") =
            format!("Respawned pane {pane_id} ({pane_pid} -> {new_pid})");
        Ok(
            json!({"pane_id":pane_id,"old_pid":pane_pid,"pane_pid":new_pid,"argv":request.argv,"rollback_argv":rollback,"reason":reason}),
        )
    }
    fn respawn_metric(&self, kind: &str, pane: &str, outcome: &str, reason: Option<&str>) {
        if let Some(metrics) = self.stores().metrics {
            metrics.record(&crate::metrics::Event {
                kind: kind.into(),
                pane_id: Some(pane.into()),
                source: Some("respawn_pane".into()),
                outcome: Some(outcome.into()),
                reason: reason.map(str::to_owned),
                ..Default::default()
            });
        }
    }

    fn respawn_failed(
        &self,
        context: &Failure<'_>,
        new_pid: Option<i64>,
        message: String,
    ) -> DunkError {
        self.respawn_metric("pane.respawn", context.pane_id, "failed", Some(&message));
        let mut suffix = "";
        if let Some(pid) = new_pid.filter(|pid| *pid != context.old_pid) {
            if let Some(start) = crate::proc::process_start(pid) {
                let result = context
                    .services
                    .terminal
                    .lock()
                    .expect("terminal lock")
                    .guarded_respawn(context.pane_id, context.rollback, pid, &start);
                let verified = result
                    .as_ref()
                    .ok()
                    .and_then(|pane| pane.pane_pid)
                    .is_some_and(|pid| {
                        !context.names.is_empty()
                            && crate::proc::process_alive(pid, None)
                            && verify(
                                context.services,
                                context.pane_id,
                                pid,
                                context.old_agent,
                                context.names,
                            )
                            .is_ok()
                    });
                suffix = if verified {
                    " (rolled back)"
                } else {
                    " (rollback not verified)"
                };
                let detail = result
                    .err()
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "rollback not verified".into());
                self.respawn_metric(
                    "pane.respawn.rollback",
                    context.pane_id,
                    if verified { "ok" } else { "failed" },
                    if verified { None } else { Some(&detail) },
                );
            }
        }
        refusal(format!("{message}{suffix}"))
    }
}
struct Failure<'a> {
    services: &'a RespawnServices<'a>,
    pane_id: &'a str,
    old_pid: i64,
    rollback: &'a [String],
    old_agent: &'a str,
    names: &'a BTreeSet<String>,
}
fn basename(text: &str) -> String {
    std::path::Path::new(text)
        .file_name()
        .and_then(|p| p.to_str())
        .unwrap_or("")
        .to_lowercase()
}
fn repr(value: &Value) -> String {
    if let Value::String(value) = value {
        agent_verify::repr_name(value)
    } else {
        py_str(value)
    }
}
struct TerminalVerify<'a>(&'a Mutex<Box<dyn Terminal>>);
impl VerifyBackend for TerminalVerify<'_> {
    fn get_pane(&self, id: &str) -> Option<Value> {
        self.0
            .lock()
            .expect("terminal lock")
            .get_pane(id)
            .map(|p| super::lanes::pane_to_json(&p, None))
    }
    fn read_pane(&mut self, id: &str, lines: usize) -> Option<String> {
        self.0
            .lock()
            .expect("terminal lock")
            .read_pane(id, lines as i64, false)
            .ok()
            .flatten()
    }
    fn agent_child_pids(&mut self, pid: i64) -> Option<Vec<Value>> {
        crate::proc::pane_agent_pids(Some(pid)).map(|entries|entries.into_iter().map(|e|json!({"pid":e.pid,"comm":e.comm,"state":e.state,"start":e.start,"argv":e.argv})).collect())
    }
    fn process_start(&self, pid: i64) -> Option<i64> {
        crate::proc::process_start(pid)?.parse().ok()
    }
}
fn verify(
    services: &RespawnServices<'_>,
    pane: &str,
    pid: i64,
    old_agent: &str,
    names: &BTreeSet<String>,
) -> Result<(), DunkError> {
    if names.is_empty() {
        return Ok(());
    }
    let name = if old_agent.is_empty() {
        names.first().expect("nonempty names")
    } else {
        old_agent
    };
    // Python respawn uses screen verification without a kind, followed by the full descendant walk.
    agent_verify::verify_agent_started(
        &mut TerminalVerify(services.terminal),
        name,
        Some(pane),
        None,
        services.verify_seconds,
    )?;
    if !agent_evidence(pid, names) {
        return Err(refusal(format!(
            "respawn-pane: pane {pane} has a live wrapper but no {} child process",
            names.iter().cloned().collect::<Vec<_>>().join("/")
        )));
    }
    Ok(())
}
fn agent_evidence(root: i64, names: &BTreeSet<String>) -> bool {
    let since = crate::proc::process_start(root).and_then(|s| s.parse::<u64>().ok());
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return false;
    };
    let mut procs = BTreeMap::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i64>().ok())
        else {
            continue;
        };
        let Some(info) = crate::proc::stat(pid) else {
            continue;
        };
        if crate::proc::DEAD_STATES.contains(&info.state.as_str())
            || matches!(info.state.as_str(), "T" | "t")
        {
            continue;
        }
        if since.is_some_and(|since| {
            info.start
                .parse::<u64>()
                .map_or(true, |start| start < since)
        }) {
            continue;
        }
        procs.insert(pid, info);
    }
    let mut stack = vec![root];
    let mut seen = BTreeSet::new();
    while let Some(pid) = stack.pop() {
        if !seen.insert(pid) {
            continue;
        }
        for (&child, info) in &procs {
            if info.ppid == pid {
                stack.push(child);
            }
        }
        if procs
            .get(&pid)
            .is_some_and(|info| names.contains(&basename(&info.comm)))
        {
            return true;
        }
    }
    false
}
fn integer(value: &Value) -> Result<String, DunkError> {
    let raw = match value {
        Value::Bool(v) => return Ok(if *v { "1" } else { "0" }.into()),
        Value::String(v) => v.clone(),
        Value::Number(v) => {
            let text = v.to_string();
            if text.contains(['.', 'e', 'E']) {
                let f = crate::github::parse_python_json_float(&text).map_err(refusal)?;
                if !f.is_finite() {
                    return Err(refusal("cannot convert float infinity to integer"));
                }
                format!("{:.0}", f.trunc())
            } else {
                text
            }
        }
        _ => {
            return Err(refusal(
                "int() argument must be a string, a bytes-like object or a real number",
            ))
        }
    };
    let text = raw.trim();
    let negative = text.starts_with('-');
    let body = text.strip_prefix(['-', '+']).unwrap_or(text);
    let mut digits = String::new();
    let mut previous = false;
    let mut chars = body.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(d) = python_decimal_digit(c) {
            digits.push(char::from(b'0' + d));
            previous = true;
        } else if c == '_'
            && previous
            && chars
                .peek()
                .and_then(|c| python_decimal_digit(*c))
                .is_some()
        {
            previous = false;
        } else {
            return Err(refusal(format!(
                "invalid literal for int() with base 10: {}",
                agent_verify::repr_name(&raw)
            )));
        }
    }
    if !previous {
        return Err(refusal(format!(
            "invalid literal for int() with base 10: {}",
            agent_verify::repr_name(&raw)
        )));
    }
    if digits.len() > 4300 {
        return Err(refusal(
            "Exceeds the limit (4300 digits) for integer string conversion",
        ));
    }
    let digits = digits.trim_start_matches('0');
    Ok(if digits.is_empty() {
        "0".into()
    } else if negative {
        format!("-{digits}")
    } else {
        digits.into()
    })
}

// Unicode decimal conversion table reused from memory/service.rs and api.rs.
fn python_decimal_digit(character: char) -> Option<u8> {
    // Unicode 16.0 Decimal_Number ranges, matching the Python 3.14 runtime used by this port.
    // The two multi-set ranges are mathematical styled digits and segmented digits.
    const DECIMAL_RANGES: &[(u32, u32)] = &[
        (0x0030, 0x0039),
        (0x0660, 0x0669),
        (0x06F0, 0x06F9),
        (0x07C0, 0x07C9),
        (0x0966, 0x096F),
        (0x09E6, 0x09EF),
        (0x0A66, 0x0A6F),
        (0x0AE6, 0x0AEF),
        (0x0B66, 0x0B6F),
        (0x0BE6, 0x0BEF),
        (0x0C66, 0x0C6F),
        (0x0CE6, 0x0CEF),
        (0x0D66, 0x0D6F),
        (0x0DE6, 0x0DEF),
        (0x0E50, 0x0E59),
        (0x0ED0, 0x0ED9),
        (0x0F20, 0x0F29),
        (0x1040, 0x1049),
        (0x1090, 0x1099),
        (0x17E0, 0x17E9),
        (0x1810, 0x1819),
        (0x1946, 0x194F),
        (0x19D0, 0x19D9),
        (0x1A80, 0x1A89),
        (0x1A90, 0x1A99),
        (0x1B50, 0x1B59),
        (0x1BB0, 0x1BB9),
        (0x1C40, 0x1C49),
        (0x1C50, 0x1C59),
        (0xA620, 0xA629),
        (0xA8D0, 0xA8D9),
        (0xA900, 0xA909),
        (0xA9D0, 0xA9D9),
        (0xA9F0, 0xA9F9),
        (0xAA50, 0xAA59),
        (0xABF0, 0xABF9),
        (0xFF10, 0xFF19),
        (0x104A0, 0x104A9),
        (0x10D30, 0x10D39),
        (0x10D40, 0x10D49),
        (0x11066, 0x1106F),
        (0x110F0, 0x110F9),
        (0x11136, 0x1113F),
        (0x111D0, 0x111D9),
        (0x112F0, 0x112F9),
        (0x11450, 0x11459),
        (0x114D0, 0x114D9),
        (0x11650, 0x11659),
        (0x116C0, 0x116C9),
        (0x116D0, 0x116E3),
        (0x11730, 0x11739),
        (0x118E0, 0x118E9),
        (0x11950, 0x11959),
        (0x11BF0, 0x11BF9),
        (0x11C50, 0x11C59),
        (0x11D50, 0x11D59),
        (0x11DA0, 0x11DA9),
        (0x11F50, 0x11F59),
        (0x16130, 0x16139),
        (0x16A60, 0x16A69),
        (0x16AC0, 0x16AC9),
        (0x16B50, 0x16B59),
        (0x16D70, 0x16D79),
        (0x1CCF0, 0x1CCF9),
        (0x1D7CE, 0x1D7FF),
        (0x1E140, 0x1E149),
        (0x1E2F0, 0x1E2F9),
        (0x1E4F0, 0x1E4F9),
        (0x1E5F1, 0x1E5FA),
        (0x1E950, 0x1E959),
        (0x1FBF0, 0x1FBF9),
    ];

    let codepoint = character as u32;
    DECIMAL_RANGES.iter().find_map(|&(start, end)| {
        if (start..=end).contains(&codepoint) {
            Some(((codepoint - start) % 10) as u8)
        } else {
            None
        }
    })
}

#[cfg(test)]
#[path = "respawn_tests.rs"]
mod tests;
