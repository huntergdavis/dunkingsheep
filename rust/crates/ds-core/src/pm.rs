//! `pm.rs` (T54, slice 3): project PMs, lane->PM routing and the one-per-project digest.
//! Ports `dunk_pm.py`.
//!
//! A PM does not get a stream of reports. It publishes ONE digest per project per window,
//! stored under a stable digest id (`pm-digest:<project>`) so the VP reads a single updatable
//! message instead of a queue. Routing is a registry, not a hard-coded rule: a project names its
//! PM pane and its member lanes.
//!
//! Text that a human or a test reads (routing refusals, the digest body) is kept identical to
//! the Python. Error texts that embed a Python exception string (`json.JSONDecodeError`,
//! `ValueError`) cannot be byte-identical and say so in the PR.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::config::Env;
use crate::error::DunkError;
use crate::flock::commands_inbox::{notice_text, InboxEnv};
use crate::flock::Flock;
use crate::inbox;
use crate::inbox_store::DigestUpsert;

type Object = Map<String, Value>;

/// `PmError`: a named refusal for a bad routing file or digest payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PmError(pub String);

impl fmt::Display for PmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PmError {}

/// One project's routing entry (`{pm, members, mode, vp_pane}`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub pm: String,
    pub members: Vec<String>,
    pub mode: String,
    pub vp_pane: String,
}

/// `{project: entry}` in file order.
pub type Routing = Vec<(String, Entry)>;

/// PM command sources. Unlike inbox-send routing, API PM views must fail closed when the
/// routing configuration is unavailable or invalid. Runtime composition belongs to t1605.
pub trait PmEnv: InboxEnv {
    fn config(&self) -> Result<&Env, DunkError>;
}

/// `DEFAULT_VP_PANE`: `DUNKINGSHEEP_VP_PANE`, else `w9:pH`.
pub fn default_vp_pane(env: &Env) -> String {
    env.get("DUNKINGSHEEP_VP_PANE")
        .unwrap_or("w9:pH")
        .to_string()
}

fn home(env: &Env) -> PathBuf {
    PathBuf::from(env.get("HOME").unwrap_or("/"))
}

/// `Path.expanduser`.
fn expand_user(path: &str, env: &Env) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => home(env).join(rest),
        None if path == "~" => home(env),
        None => PathBuf::from(path),
    }
}

/// `ROUTING`: `DUNKINGSHEEP_PM_ROUTING`, else `~/.config/dunkingsheep/pm-routing.json` (the
/// Python uses `Path.home()` here, not `DUNKINGSHEEP_DIR`; kept).
pub fn default_routing_path(env: &Env) -> PathBuf {
    match env.get("DUNKINGSHEEP_PM_ROUTING") {
        // Python constructs Path(os.environ.get(...)); an explicitly empty value is `.`.
        Some("") => PathBuf::from("."),
        Some(path) => PathBuf::from(path),
        _ => home(env)
            .join(".config")
            .join("dunkingsheep")
            .join("pm-routing.json"),
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_none_or(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

/// Python's `str()` of a JSON value (scalars exactly; containers as compact JSON).
fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// `str(entry.get(key) or default)`.
fn str_or(entry: &Object, key: &str, default: &str) -> String {
    match entry.get(key).filter(|v| truthy(v)) {
        Some(value) => py_str(value),
        None => default.to_string(),
    }
}

/// `load_routing`: `{project: {pm, members, mode, vp_pane}}`. Empty if there is no file.
pub fn load_routing(
    path: Option<&Path>,
    vp_pane: Option<&str>,
    env: &Env,
) -> Result<Routing, PmError> {
    let default = default_routing_path(env);
    let path = path.map_or(default, Path::to_path_buf);
    let path = expand_user(&path.to_string_lossy(), env);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let text = fs::read_to_string(&path).map_err(|e| {
        PmError(format!(
            "PM routing {} is not readable JSON: {e}",
            path.display()
        ))
    })?;
    let data: Value = serde_json::from_str(&text).map_err(|e| {
        PmError(format!(
            "PM routing {} is not readable JSON: {e}",
            path.display()
        ))
    })?;
    let Value::Object(data) = data else {
        return Err(PmError(format!(
            "PM routing {} must be an object of projects",
            path.display()
        )));
    };
    let fallback_vp = vp_pane
        .filter(|v| !v.is_empty())
        .map_or_else(|| default_vp_pane(env), str::to_string);
    let mut routing = Vec::new();
    for (project, entry) in &data {
        let Value::Object(entry) = entry else {
            return Err(PmError(format!("PM routing {project} must be an object")));
        };
        let pm = str_or(entry, "pm", "").trim().to_string();
        if pm.is_empty() {
            return Err(PmError(format!("PM routing {project} has no pm pane")));
        }
        let members: Vec<String> = match entry.get("members").filter(|v| truthy(v)) {
            None => Vec::new(),
            Some(Value::Array(items)) => {
                items.iter().map(|i| py_str(i).trim().to_string()).collect()
            }
            Some(Value::String(text)) => text
                .chars()
                .map(|c| c.to_string().trim().to_string())
                .collect(),
            Some(Value::Object(map)) => map.keys().map(|k| k.trim().to_string()).collect(),
            Some(_) => {
                return Err(PmError(format!(
                    "PM routing {project} members must be a list"
                )))
            }
        };
        let mode = str_or(entry, "mode", "redirect");
        if mode != "redirect" && mode != "flag" {
            return Err(PmError(format!(
                "PM routing {project} mode must be redirect or flag"
            )));
        }
        let vp = str_or(entry, "vp_pane", &fallback_vp);
        routing.push((
            project.clone(),
            Entry {
                pm,
                members,
                mode,
                vp_pane: vp,
            },
        ));
    }
    Ok(routing)
}

/// `project_for`: the project whose member list contains this pane, if any.
pub fn project_for<'a>(
    pane_id: Option<&str>,
    routing: &'a Routing,
) -> Option<(&'a str, &'a Entry)> {
    let pane = pane_id.filter(|p| !p.is_empty())?;
    routing
        .iter()
        .find(|(_, entry)| entry.members.iter().any(|m| m == pane))
        .map(|(project, entry)| (project.as_str(), entry))
}

/// `redirect`: where a lane's message to the VP should actually go, as
/// `(target_pane, project, mode)`; `None` when it should stay. The PM is never redirected to
/// itself.
pub fn redirect(pane_id: Option<&str>, routing: &Routing) -> Option<(String, String, String)> {
    let (project, entry) = project_for(pane_id, routing)?;
    if pane_id == Some(entry.pm.as_str()) {
        return None;
    }
    Some((entry.pm.clone(), project.to_string(), entry.mode.clone()))
}

/// `POOLRAD_CHECKPOINT`.
pub fn default_checkpoint(env: &Env) -> PathBuf {
    let raw = env
        .get("POOLRAD_CHECKPOINT")
        .unwrap_or("~/workspace/poolrad-decomp/docs/re/function_accounting_checkpoint.json");
    expand_user(raw, env)
}

/// Python's `int(x)` on a JSON value, with the exception text on failure.
fn py_int(value: &Value) -> Result<i64, String> {
    match value {
        Value::Number(n) => n
            .as_i64()
            .or_else(|| n.as_f64().map(|f| f as i64))
            .ok_or_else(|| "int too large".to_string()),
        Value::Bool(b) => Ok(i64::from(*b)),
        Value::String(text) => text
            .trim()
            .parse()
            .map_err(|_| format!("invalid literal for int() with base 10: '{text}'")),
        Value::Null => Err(
            "int() argument must be a string, a bytes-like object or a real number, not 'NoneType'"
                .into(),
        ),
        Value::Array(_) => Err(
            "int() argument must be a string, a bytes-like object or a real number, not 'list'"
                .into(),
        ),
        Value::Object(_) => Err(
            "int() argument must be a string, a bytes-like object or a real number, not 'dict'"
                .into(),
        ),
    }
}

/// Python's `str(OSError)` for the failures a file read can have.
pub(crate) fn os_error_text(error: &std::io::Error, path: &Path) -> String {
    match error.raw_os_error() {
        Some(code) => {
            let text = error.to_string();
            let reason = text.split(" (os error").next().unwrap_or(&text);
            format!("[Errno {code}] {reason}: '{}'", path.display())
        }
        None => error.to_string(),
    }
}

fn read_checkpoint(path: &Path) -> Result<Vec<Value>, String> {
    let text = fs::read_to_string(path).map_err(|e| os_error_text(&e, path))?;
    let data: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    let field = |key: &str| -> Result<i64, String> {
        match data.get(key) {
            Some(value) => py_int(value),
            None => Err(format!("'{key}'")),
        }
    };
    let bodies = field("strict_full_c_originals")?;
    let total = field("verified_game_identities")?;
    let commit = data
        .get("reviewed_change")
        .filter(|v| truthy(v))
        .and_then(|c| c.get("reviewed_commit"))
        .filter(|v| truthy(v))
        .map_or_else(|| "unknown".to_string(), py_str);
    let modified = fs::metadata(path)
        .and_then(|m| m.modified())
        .map_err(|e| os_error_text(&e, path))?;
    // Local wall-clock, readable in the digest: "as of 2026-10-21 19:40".
    let as_of = chrono::DateTime::<chrono::Local>::from(modified)
        .format("%Y-%m-%d %H:%M")
        .to_string();
    let reference = format!("{} (reviewed_commit {commit})", path.display());
    Ok(vec![
        json!({"name": "strict C bodies", "value": bodies, "unit": format!("/{total}"),
            "evidence_ref": reference, "as_of": as_of}),
        json!({"name": "verified game identities", "value": total, "unit": "",
            "evidence_ref": reference, "as_of": as_of}),
    ])
}

/// `checkpoint_numbers`: the project's numbers AT PUBLISH TIME. `None` = no source for this
/// project.
///
/// A digest is read instead of the reports, so a remembered number is a confident wrong answer
/// with a source attached. On a failed read the number is "unavailable", never yesterday's value.
pub fn checkpoint_numbers(project: &str, path: Option<&Path>, env: &Env) -> Option<Vec<Value>> {
    if project != "poolrad" {
        return None;
    }
    let path = path.map_or_else(
        || default_checkpoint(env),
        |p| expand_user(&p.to_string_lossy(), env),
    );
    Some(read_checkpoint(&path).unwrap_or_else(|reason| {
        vec![
            json!({"name": "strict C bodies", "value": "unavailable", "unit": "",
            "evidence_ref": format!("{}: {reason}", path.display()), "as_of": null}),
        ]
    }))
}

/// `render_digest`'s keyword arguments.
#[derive(Debug, Clone, Default)]
pub struct Digest<'a> {
    pub changed: &'a [String],
    pub blocked: &'a [String],
    pub owner_decisions: &'a [String],
    pub numbers: &'a [Value],
    /// Defaults to "30 min" when `None`.
    pub window: Option<&'a str>,
    pub note: Option<&'a str>,
}

fn section(lines: &mut Vec<String>, items: &[String]) {
    if items.is_empty() {
        lines.push("- nothing".to_string());
    }
    lines.extend(items.iter().map(|item| format!("- {item}")));
}

/// `render_digest`: the four-section digest. Empty sections say so, never vanish.
pub fn render_digest(project: &str, digest: &Digest<'_>) -> String {
    let mut lines = vec![
        format!("# PM digest: {project}"),
        format!("_window: {}_", digest.window.unwrap_or("30 min")),
        String::new(),
    ];
    if let Some(note) = digest.note.filter(|n| !n.is_empty()) {
        lines.push(note.to_string());
        lines.push(String::new());
    }
    lines.push("## Changed".into());
    section(&mut lines, digest.changed);
    lines.push(String::new());
    lines.push("## Blocked".into());
    section(&mut lines, digest.blocked);
    if digest.blocked.len() > 6 {
        lines.push(format!(
            "- PM note: {} blocked items; past six this is no longer a digest - split the project or escalate one owner decision.",
            digest.blocked.len()
        ));
    }
    lines.push(String::new());
    lines.push("## Owner decisions".into());
    section(&mut lines, digest.owner_decisions);
    lines.push(String::new());
    lines.push("## Numbers".into());
    for item in digest.numbers {
        match item {
            Value::Object(map) => {
                let get = |key: &str, default: &str| {
                    map.get(key).map_or_else(|| default.to_string(), py_str)
                };
                let name = get("name", "?");
                let value = get("value", "?");
                let unit = get("unit", "");
                let reference = map
                    .get("evidence_ref")
                    .filter(|v| truthy(v))
                    .map_or_else(|| "no evidence ref".to_string(), py_str);
                let as_of = map.get("as_of").filter(|v| truthy(v)).map(py_str);
                let suffix = format!(
                    " (evidence: {reference}{})",
                    as_of.map_or_else(String::new, |a| format!("; as of {a}"))
                );
                lines.push(format!(
                    "{}{suffix}",
                    format!("- {name}: {value} {unit}").trim_end()
                ));
            }
            other => lines.push(format!("- {}", py_str(other))),
        }
    }
    if digest.numbers.is_empty() {
        lines.push("- nothing".into());
    }
    format!("{}\n", lines.join("\n").trim())
}

/// `digest_id`.
pub fn digest_id(project: &str) -> String {
    format!("pm-digest:{project}")
}

fn payload_items(payload: Option<&Value>, key: &str) -> Vec<String> {
    match payload.and_then(|value| value.get(key)) {
        Some(Value::Array(items)) => items.iter().map(py_str).collect(),
        Some(Value::Object(items)) => items.keys().cloned().collect(),
        Some(Value::String(text)) => text.chars().map(|ch| ch.to_string()).collect(),
        _ => Vec::new(),
    }
}

fn payload_numbers(payload: Option<&Value>) -> Vec<Value> {
    match payload.and_then(|value| value.get("numbers")) {
        Some(Value::Array(items)) => items.clone(),
        Some(Value::Object(items)) => items.keys().map(|key| json!(key)).collect(),
        Some(Value::String(text)) => text.chars().map(|ch| json!(ch.to_string())).collect(),
        _ => Vec::new(),
    }
}

impl Flock {
    /// `Flock.pm_digest`: publish/update the stable per-project row, then attempt its pane
    /// notification. A notification failure leaves the stored digest and a false result.
    pub fn pm_digest(
        &self,
        env: &dyn PmEnv,
        project: &str,
        payload: Option<&Value>,
        to: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> Result<Value, DunkError> {
        let config = env.config()?;
        let empty_payload = Value::String(String::new());
        let normalized_payload = payload
            .filter(|value| truthy(value))
            .or(Some(&empty_payload));
        let object = match normalized_payload {
            Some(Value::Object(map)) => Some(map.clone()),
            Some(Value::String(text)) => Some(
                parse_payload(Some(text)).map_err(|error| DunkError::Refused(error.to_string()))?,
            ),
            _ => None,
        };
        let demo = object
            .as_ref()
            .and_then(|payload| payload.get("demo"))
            .is_some_and(truthy);
        let body = if let Some(payload) = &object {
            let live = checkpoint_numbers(project, None, config);
            let object_value = Value::Object(payload.clone());
            let numbers = live.unwrap_or_else(|| payload_numbers(Some(&object_value)));
            let changed = payload_items(Some(&object_value), "changed");
            let blocked = payload_items(Some(&object_value), "blocked");
            let owner_decisions = payload_items(Some(&object_value), "owner_decisions");
            let note = payload
                .get("note")
                .filter(|value| truthy(value))
                .map(py_str);
            render_digest(
                project,
                &Digest {
                    changed: &changed,
                    blocked: &blocked,
                    owner_decisions: &owner_decisions,
                    numbers: &numbers,
                    window: Some("30 min"),
                    note: note.as_deref(),
                },
            )
        } else {
            normalized_payload.map(py_str).unwrap_or_default()
        };

        let target = to
            .filter(|target| !target.is_empty())
            .map(str::to_owned)
            .unwrap_or_else(|| default_vp_pane(config));
        let (mailbox, label, pane_id) =
            self.resolve_mailbox(env, &target, self_pane_id, false, "")?;
        let digest_id = digest_id(project);
        let action = if env.store().digest_exists(&digest_id).unwrap_or(false) {
            "updated"
        } else {
            "created"
        };
        let subject = if demo {
            format!("PM digest (DEMO): {project}")
        } else {
            format!("PM digest: {project}")
        };
        let message_id = env.store().upsert_digest(DigestUpsert {
            mailbox: &mailbox,
            digest_id: &digest_id,
            body: &body,
            subject: Some(&subject),
            sender: self_pane_id,
            mailbox_label: Some(&label),
            pane_id: pane_id.as_deref(),
            agent_only: false,
        })?;

        // Read back the stored row after the write has completed; notification is deliberately
        // later so failures cannot erase a successful digest.
        let now = env.now();
        let (summary, stored_body) = inbox::get(env.pool(), &message_id, now)?;
        let read_at = inbox::status(env.pool(), &message_id, now)?.read_at;
        let mut message = json!({
            "message_id": summary.message_id,
            "mailbox": summary.mailbox,
            "destination": summary.destination,
            "sender": summary.sender,
            "subject": summary.subject,
            "state": summary.state,
            "created_at": summary.created_at,
            "waiting_s": summary.waiting_s,
            "body_bytes": summary.body_bytes,
            "digest_id": summary.digest_id,
            "transport": summary.transport,
            "reply_to": summary.reply_to,
            "external_id": summary.external_id,
            "delivered_at": summary.delivered_at,
            "delivery_transport": summary.delivery_transport,
            "attachments": [],
            "body": stored_body,
            "read_at": read_at,
            "action": action,
        });
        if let Some(pane) = pane_id.as_deref() {
            let notified = (|| -> Result<(), DunkError> {
                let unread = inbox::count(env.pool(), &mailbox, &[], now)?.unread;
                let notice = notice_text(
                    &mailbox,
                    unread,
                    self_pane_id,
                    Some(&format!("PM digest: {project}")),
                );
                env.send_now(pane, &notice, &message_id)?;
                Ok(())
            })()
            .is_ok();
            message["notified"] = json!(notified);
        }
        message["digest_id"] = json!(digest_id);
        Ok(message)
    }
}

/// `parse_payload`: a JSON digest payload or a plain body string (which becomes `{"note": ...}`).
pub fn parse_payload(raw: Option<&str>) -> Result<Object, PmError> {
    let text = raw.unwrap_or("").trim();
    if text.starts_with('{') {
        if let Ok(Value::Object(map)) = serde_json::from_str(text) {
            return Ok(map);
        }
    }
    let mut map = Object::new();
    map.insert("note".into(), json!(text));
    Ok(map)
}

#[cfg(test)]
mod tests;
