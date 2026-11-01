//! `policy.rs` (T54, slice 1): per-agent resource policy. Ports `dunk_policy.py`:
//! thresholds -> findings, the deny evaluation, and the conservative kill decision.
//!
//! Safety order, and every clause is deliberate (from the Python):
//! - DEFAULT OFF. Killing needs an explicit per-agent policy with `kill: true`.
//! - ATTRIBUTED ONLY. A pid is actionable only when the daemon registered its
//!   (pid, starttime); a bare process name is never enough.
//! - DENY LIST WINS. Refused by evaluation, not by luck.
//! - DRY RUN FIRST. A dry run records the decision and signals nothing.
//! - HEAVY WORK IS EXPECTED. Policies are per lane, never global.
//!
//! Samples, policies and records are JSON objects (insertion-ordered, like the Python dicts)
//! so the output is byte-comparable with the Python's.

use std::fmt;
use std::path::{Component, Path, PathBuf};

use nix::errno::Errno;
use nix::sys::signal::{kill, Signal};
use nix::unistd::Pid;
use serde_json::{json, Map, Value};

/// `DEFAULT_GRACE_S`.
pub const DEFAULT_GRACE_S: f64 = 30.0;

/// `PolicyError`: a named refusal for a malformed policy or sample.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyError(pub String);

impl fmt::Display for PolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for PolicyError {}

type Object = Map<String, Value>;

/// Python truthiness of a JSON value.
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

/// `_num`: `None` when absent or null, a positive number otherwise (a bool counts as the
/// int it is in Python: `True` is 1, `False` is refused).
fn num(policy: &Object, key: &str) -> Result<Option<f64>, PolicyError> {
    let value = match policy.get(key) {
        None | Some(Value::Null) => return Ok(None),
        Some(value) => value,
    };
    let number = match value {
        Value::Number(n) => n.as_f64(),
        Value::Bool(b) => Some(f64::from(u8::from(*b))),
        _ => None,
    };
    match number {
        Some(n) if n > 0.0 => Ok(Some(n)),
        _ => Err(PolicyError(format!(
            "policy.{key} must be a positive number"
        ))),
    }
}

/// Python `x or 0` then `>= threshold`: a missing, null or falsy measurement is 0.
fn measured_at_least(sample: &Object, key: &str, threshold: f64) -> bool {
    match sample.get(key) {
        Some(Value::Number(n)) => n.as_f64().is_some_and(|v| v >= threshold),
        Some(Value::Bool(true)) => 1.0 >= threshold,
        _ => 0.0 >= threshold,
    }
}

fn sample_field(sample: &Object, key: &str) -> Result<Value, PolicyError> {
    sample
        .get(key)
        .cloned()
        .ok_or_else(|| PolicyError(format!("sample.{key} is missing")))
}

fn finding(
    rule: &str,
    sample: &Object,
    measured_key: &str,
    threshold: f64,
    window: Option<f64>,
) -> Result<Value, PolicyError> {
    Ok(json!({
        "rule": rule,
        "agent": sample_field(sample, "name")?,
        "measured": sample_field(sample, measured_key)?,
        "threshold": threshold,
        "window": window,
        "pid": sample_field(sample, "pid")?,
    }))
}

/// `evaluate(sample, policy)`: findings for one caller sample against one agent's policy.
///
/// A threshold without its window is a policy error, not a guess. (The Python's `now`
/// argument is accepted and unused there; it is not carried over.)
pub fn evaluate(sample: &Object, policy: &Object) -> Result<Vec<Value>, PolicyError> {
    let mut out = Vec::new();
    if let Some(cpu) = num(policy, "cpu_pct_over")? {
        let window = num(policy, "cpu_window_s")?
            .ok_or_else(|| PolicyError("cpu_pct_over needs cpu_window_s".into()))?;
        if measured_at_least(sample, "cpu_pct", cpu) {
            out.push(finding("cpu", sample, "cpu_pct", cpu, Some(window))?);
        }
    }
    if let Some(rss) = num(policy, "rss_mb_over")? {
        let window = num(policy, "rss_window_s")?
            .ok_or_else(|| PolicyError("rss_mb_over needs rss_window_s".into()))?;
        if measured_at_least(sample, "rss_mb", rss) {
            out.push(finding("rss", sample, "rss_mb", rss, Some(window))?);
        }
    }
    let wall = num(policy, "wall_minutes_over")?;
    // A legitimate multi-hour compile or DOSBox run crosses any wall threshold. Wall-kill is
    // for lanes that are STUCK, not lanes that are SLOW: a lane opts out with wall_exempt.
    if let Some(wall) = wall {
        let exempt = policy.get("wall_exempt").is_some_and(truthy);
        if !exempt && measured_at_least(sample, "wall_minutes", wall) {
            out.push(finding("wall", sample, "wall_minutes", wall, None)?);
        }
    }
    Ok(out)
}

/// `os.path.realpath`: resolve symlinks for as much of the path as exists, keep the rest.
fn realpath(path: &str) -> String {
    fn resolve(path: &Path) -> PathBuf {
        if let Ok(real) = std::fs::canonicalize(path) {
            return real;
        }
        match (path.parent(), path.file_name()) {
            (Some(parent), Some(name)) if !parent.as_os_str().is_empty() => {
                resolve(parent).join(name)
            }
            _ => path.to_path_buf(),
        }
    }
    let absolute = if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    // Lexically drop `.` and fold `..` first (Python's realpath resolves them left to right).
    let mut clean = PathBuf::new();
    for part in absolute.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other.as_os_str()),
        }
    }
    resolve(&clean).to_string_lossy().into_owned()
}

/// What the deny evaluation is given (`denied`'s keyword arguments).
#[derive(Debug, Clone, Default)]
pub struct Deny<'a> {
    pub deny_pids: &'a [i64],
    pub deny_panes: &'a [i64],
    pub workspace_roots: &'a [String],
    pub cwd: Option<&'a str>,
    pub protected_panes: &'a [i64],
}

/// `denied(pid, ...)`: a reason string, or `None` when permitted. Pure, so a test (and the VP)
/// can prove a refused pid is refused by policy rather than by a lucky missing /proc entry.
pub fn denied(pid: Option<i64>, deny: &Deny<'_>) -> Option<String> {
    let shown = pid.map_or_else(|| "None".to_string(), |p| p.to_string());
    if pid.is_some_and(|p| deny.deny_pids.contains(&p)) {
        return Some(format!("pid {shown} is on the deny list"));
    }
    if pid.is_some_and(|p| deny.protected_panes.contains(&p)) {
        return Some(format!("pid {shown} is a protected pane"));
    }
    if let Some(cwd) = deny.cwd {
        let roots: Vec<String> = deny.workspace_roots.iter().map(|r| realpath(r)).collect();
        let real = realpath(cwd);
        if !roots.is_empty()
            && !roots
                .iter()
                .any(|root| real.starts_with(&format!("{root}/")) || real == *root)
        {
            return Some(format!("cwd {cwd} is not a workspace"));
        }
    }
    None
}

/// A signal failure, in the two kinds the Python tells apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignalError {
    /// `ProcessLookupError`.
    NoSuchProcess,
    /// `PermissionError`, with the text Python's `str(exc)` would carry.
    Permission(String),
}

/// The default `signal_fn`: `os.kill(pid, sig)`.
pub fn os_kill(pid: i64, signal: Signal) -> Result<(), SignalError> {
    send(pid, Some(signal))
}

fn send(pid: i64, signal: Option<Signal>) -> Result<(), SignalError> {
    let pid = i32::try_from(pid).map_err(|_| SignalError::NoSuchProcess)?;
    match kill(Pid::from_raw(pid), signal) {
        Ok(()) => Ok(()),
        Err(Errno::ESRCH) => Err(SignalError::NoSuchProcess),
        Err(errno) => Err(SignalError::Permission(format!(
            "[Errno {}] {}",
            errno as i32,
            errno.desc()
        ))),
    }
}

/// `kill_decision`'s keyword arguments.
pub struct KillOptions<'a> {
    /// The (pid, starttime) pairs the daemon recorded.
    pub registered: &'a [(i64, Option<i64>)],
    pub deny: Deny<'a>,
    pub dry_run: bool,
    pub grace_s: f64,
}

impl Default for KillOptions<'_> {
    fn default() -> Self {
        Self {
            registered: &[],
            deny: Deny::default(),
            dry_run: true,
            grace_s: DEFAULT_GRACE_S,
        }
    }
}

fn record_base(decision: &str, sample: &Object) -> Object {
    let mut record = Object::new();
    record.insert("decision".into(), json!(decision));
    record.insert(
        "agent".into(),
        sample.get("name").cloned().unwrap_or(Value::Null),
    );
    record
}

/// `kill_decision`: decide and (unless dry_run) act on one over-threshold caller.
///
/// A pid not in `registered` is unattributable and is never signalled. TERM, grace, then KILL.
/// `signal_fn` and `sleep_fn` are injectable like the Python's. The post-TERM liveness probe
/// (`os.kill(pid, 0)`) always goes to the OS, exactly as the Python does.
pub fn kill_decision(
    sample: &Object,
    policy: &Object,
    options: &KillOptions<'_>,
    signal_fn: &mut dyn FnMut(i64, Signal) -> Result<(), SignalError>,
    sleep_fn: &mut dyn FnMut(f64),
) -> Result<Value, PolicyError> {
    let findings = evaluate(sample, policy)?;
    if findings.is_empty() {
        let mut record = record_base("none", sample);
        record.insert("findings".into(), json!([]));
        return Ok(Value::Object(record));
    }
    if !policy.get("kill").is_some_and(truthy) {
        let mut record = record_base("alert-only", sample);
        record.insert("findings".into(), Value::Array(findings));
        return Ok(Value::Object(record));
    }
    let pid_value = sample.get("pid").cloned().unwrap_or(Value::Null);
    let pid = pid_value.as_i64();
    let start = sample.get("starttime").and_then(Value::as_i64);
    let attributed = pid.is_some_and(|p| options.registered.contains(&(p, start)));
    if !attributed {
        let mut record = record_base("refused", sample);
        record.insert("reason".into(), json!("unattributed pid (not registered)"));
        record.insert("findings".into(), Value::Array(findings));
        record.insert("pid".into(), pid_value);
        return Ok(Value::Object(record));
    }
    let deny = Deny {
        cwd: sample.get("cwd").and_then(Value::as_str),
        ..options.deny.clone()
    };
    if let Some(reason) = denied(pid, &deny) {
        let mut record = record_base("refused", sample);
        record.insert("reason".into(), json!(reason));
        record.insert("findings".into(), Value::Array(findings));
        record.insert("pid".into(), pid_value);
        return Ok(Value::Object(record));
    }
    let pid = pid.expect("attributed implies a pid");
    let mut record = record_base(
        if options.dry_run {
            "would-kill"
        } else {
            "kill"
        },
        sample,
    );
    record.insert("pid".into(), pid_value);
    record.insert("findings".into(), Value::Array(findings));
    record.insert(
        "sample".into(),
        json!({
            "cpu_pct": sample.get("cpu_pct").cloned().unwrap_or(Value::Null),
            "rss_mb": sample.get("rss_mb").cloned().unwrap_or(Value::Null),
        }),
    );
    if options.dry_run {
        return Ok(Value::Object(record));
    }
    let outcome = (|| -> Result<&'static str, SignalError> {
        signal_fn(pid, Signal::SIGTERM)?;
        record.insert("signals".into(), json!(["TERM"]));
        sleep_fn(options.grace_s);
        match send(pid, None) {
            Err(SignalError::NoSuchProcess) => return Ok("exited after TERM"),
            Err(other) => return Err(other),
            Ok(()) => {}
        }
        signal_fn(pid, Signal::SIGKILL)?;
        if let Some(Value::Array(signals)) = record.get_mut("signals") {
            signals.push(json!("KILL"));
        }
        Ok("KILL after grace")
    })();
    let text = match outcome {
        Ok(text) => text.to_string(),
        Err(SignalError::NoSuchProcess) => "already gone".to_string(),
        Err(SignalError::Permission(text)) => format!("permission refused: {text}"),
    };
    record.insert("outcome".into(), json!(text));
    Ok(Value::Object(record))
}

#[cfg(test)]
mod tests;
