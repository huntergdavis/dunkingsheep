//! `dunk_native_session.py` (T55, `native_session.rs`): the fail-closed native-session
//! activity guard for same-session Codex resume (t416).
//!
//! A guarded same-pane respawn replaces a pane's process; it does not stop a turn that an
//! older shared Codex app-server is still executing on the same native session. A blank pane
//! and the absence of a lease do not prove that turn is finished (measured 2026-10-29: turn
//! 01a18644-... was still running when the replacement started turn 01a18645-...). This module
//! reads the authoritative native state: the exact session row in the private
//! `CODEX_HOME/state_5.sqlite`, that row's exact rollout JSONL path, and the rollout's
//! `task_started`/`task_complete` lifecycle. It refuses on an unfinished turn, on ambiguous,
//! missing or truncated evidence, and on a rollout change between the check and the respawn.
//! A quiet session is allowed only when the trace is authenticated by its exact `session_meta`
//! line; an empty, missing, truncated or wrong-session trace is never equivalent to quiet.
//!
//! BOUND: the final stat re-read closes the window up to that read; a turn that starts after it
//! is not prevented. This is a bounded precondition, not atomic coordination.

use std::fmt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::error::DunkError;

/// `NativeSessionRefusal` and its two subclasses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeSessionError {
    /// The native session cannot be proven quiescent; callers must refuse.
    Refusal(String),
    /// The rollout changed after the quiescence snapshot was taken.
    Changed(String),
    /// The private state DB is busy: retryable, with a fixed message and no raw database text.
    Busy,
}

impl NativeSessionError {
    pub fn retryable(&self) -> bool {
        matches!(self, NativeSessionError::Busy)
    }
}

impl fmt::Display for NativeSessionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NativeSessionError::Refusal(m) | NativeSessionError::Changed(m) => f.write_str(m),
            NativeSessionError::Busy => {
                f.write_str("native session state DB busy; retry the request")
            }
        }
    }
}

impl std::error::Error for NativeSessionError {}

impl From<NativeSessionError> for DunkError {
    fn from(error: NativeSessionError) -> Self {
        if error.retryable() {
            DunkError::Retryable(error.to_string())
        } else {
            DunkError::Refused(error.to_string())
        }
    }
}

fn refuse<T>(message: impl Into<String>) -> Result<T, NativeSessionError> {
    Err(NativeSessionError::Refusal(message.into()))
}

/// `(st_dev, st_ino, st_size, st_mtime_ns)`.
pub type Signature = (u64, u64, u64, i128);

/// `_signature`.
pub fn signature(meta: &std::fs::Metadata) -> Signature {
    (
        meta.dev(),
        meta.ino(),
        meta.size(),
        i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
    )
}

/// `ResumeRequest`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeRequest {
    pub session_id: String,
    pub codex_home: Option<String>,
    pub cwd: Option<String>,
}

/// One turn that started and has not completed: `(turn_id, started timestamp)`.
pub type ActiveTurn = (String, Value);

/// `NativeSessionSnapshot`.
#[derive(Debug, Clone, PartialEq)]
pub struct NativeSessionSnapshot {
    pub session_id: String,
    pub rollout_path: String,
    pub signature: Signature,
    pub active_turns: Vec<ActiveTurn>,
    pub started_turns: Vec<String>,
    pub completed_turns: Vec<String>,
}

impl NativeSessionSnapshot {
    /// Refuse if the exact rollout changed after this snapshot was read.
    pub fn assert_unchanged(&self) -> Result<(), NativeSessionError> {
        let meta = std::fs::metadata(&self.rollout_path).map_err(|error| {
            NativeSessionError::Changed(format!(
                "native session rollout disappeared after the quiescence check: {error}"
            ))
        })?;
        if signature(&meta) != self.signature {
            return Err(NativeSessionError::Changed(
                "native session rollout changed after the quiescence check; the older turn may have started or advanced".into(),
            ));
        }
        Ok(())
    }
}

/// Python `repr(str)`.
fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

/// `repr` of a JSON value pulled from a record (strings quoted, None, numbers).
fn repr_value(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(s)) => py_repr(s),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(other) => other.to_string(),
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

/// `str(value)`.
fn py_str(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `os.path.realpath`: resolve symlinks; a path that does not exist stays lexical (absolute).
pub fn realpath(path: &str) -> String {
    let p = if path.is_empty() {
        std::env::current_dir().unwrap_or_default()
    } else {
        PathBuf::from(path)
    };
    match std::fs::canonicalize(&p) {
        Ok(real) => real.display().to_string(),
        Err(_) => {
            let abs = if p.is_absolute() {
                p
            } else {
                std::env::current_dir().unwrap_or_default().join(p)
            };
            let mut out = PathBuf::new();
            for part in abs.components() {
                match part {
                    std::path::Component::ParentDir => {
                        out.pop();
                    }
                    std::path::Component::CurDir => {}
                    other => out.push(other),
                }
            }
            out.display().to_string()
        }
    }
}

/// `os.path.expanduser` for a leading `~` or `~/`.
pub fn expanduser(path: &str) -> String {
    match (path, std::env::var("HOME")) {
        ("~", Ok(home)) => home,
        (p, Ok(home)) if p.starts_with("~/") => format!("{home}/{}", &p[2..]),
        _ => path.to_string(),
    }
}

fn basename(token: &str) -> &str {
    token.rsplit('/').next().unwrap_or(token)
}

/// `find_resume_request`: the exact Codex resume request in an argv, or None. Only a token
/// whose basename is `codex` starts the command; `resume` after that names the native session.
/// A resume with no id (picker mode) is not a same-session request and has `session_id == ""`.
pub fn find_resume_request(argv: &[String]) -> Option<ResumeRequest> {
    let codex_at = argv
        .iter()
        .position(|t| basename(t).to_lowercase() == "codex")?;
    let (mut home, mut cwd, mut session_id, mut found) = (None, None, String::new(), false);
    for (index, token) in argv.iter().enumerate() {
        if let Some(rest) = token.strip_prefix("CODEX_HOME=") {
            home = Some(rest.to_string());
        }
        if matches!(token.as_str(), "-C" | "--cd" | "--cwd") && index + 1 < argv.len() {
            cwd = Some(argv[index + 1].clone());
        }
        if index > codex_at && token == "resume" {
            found = true;
            if index + 1 < argv.len() && !argv[index + 1].starts_with('-') {
                session_id = argv[index + 1].clone();
            }
            break;
        }
    }
    found.then_some(ResumeRequest {
        session_id,
        codex_home: home,
        cwd,
    })
}

/// A `threads` row: `(id, rollout_path, cwd)`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRow {
    pub id: String,
    pub cwd: String,
}

/// `_state_row`: the exact thread row in the private state DB, and its rollout's real path.
pub fn state_row(
    codex_home: &str,
    session_id: &str,
) -> Result<(StateRow, String), NativeSessionError> {
    let db = realpath(&format!("{codex_home}/state_5.sqlite"));
    if !Path::new(&db).is_file() {
        return refuse(format!(
            "native session guard cannot find private state DB {db}"
        ));
    }
    let read = || -> rusqlite::Result<Option<(String, Option<String>, Option<String>)>> {
        let conn = Connection::open_with_flags(
            &db,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let mut stmt = conn.prepare("select id, rollout_path, cwd from threads where id = ?")?;
        let mut rows = stmt.query([session_id])?;
        match rows.next()? {
            Some(row) => Ok(Some((row.get(0)?, row.get(1)?, row.get(2)?))),
            None => Ok(None),
        }
    };
    let row = match read() {
        Ok(row) => row,
        Err(rusqlite::Error::SqliteFailure(e, _))
            if matches!(
                e.code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            ) =>
        {
            return Err(NativeSessionError::Busy);
        }
        Err(_) => return refuse("native session guard could not read private state DB"),
    };
    let Some((id, rollout, cwd)) = row else {
        return refuse(format!(
            "native session {} is not in the private state DB",
            py_repr(session_id)
        ));
    };
    if id != session_id {
        return refuse("native session state row id mismatch");
    }
    let rollout_path = realpath(rollout.as_deref().unwrap_or(""));
    if rollout_path.is_empty() || !Path::new(&rollout_path).is_file() {
        return refuse(format!(
            "native session {} rollout does not exist: {}",
            py_repr(session_id),
            py_repr(&rollout_path)
        ));
    }
    let name = Path::new(&rollout_path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if !name.contains(session_id) {
        return refuse(format!(
            "native session rollout path does not name session {}: {}",
            py_repr(session_id),
            py_repr(&rollout_path)
        ));
    }
    Ok((
        StateRow {
            id,
            cwd: cwd.unwrap_or_default(),
        },
        rollout_path,
    ))
}

/// `bytes.splitlines()`: `\n`, `\r\n` and `\r` only.
fn split_lines(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < data.len() {
        match data[i] {
            b'\n' => {
                out.push(&data[start..i]);
                start = i + 1;
            }
            b'\r' => {
                out.push(&data[start..i]);
                if data.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    if start < data.len() {
        out.push(&data[start..]);
    }
    out
}

fn blank(raw: &[u8]) -> bool {
    raw.iter()
        .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
}

/// What `_read_rollout` returns.
#[derive(Debug, Clone, PartialEq)]
pub struct Rollout {
    pub signature: Signature,
    pub started: Vec<String>,
    pub completed: Vec<String>,
    pub active: Vec<ActiveTurn>,
}

/// `_read_rollout`: parse a rollout. Authenticated quiet means the first non-empty record is a
/// `session_meta` for this exact session and cwd and the complete file has no active turn. An
/// empty, missing or truncated file is a reading failure, not evidence of quiet.
pub fn read_rollout(
    path: &str,
    session_id: &str,
    expected_cwd: Option<&str>,
) -> Result<Rollout, NativeSessionError> {
    let read = || -> std::io::Result<(Signature, Vec<u8>, Signature)> {
        let before = signature(&std::fs::metadata(path)?);
        let data = std::fs::read(path)?;
        let after = signature(&std::fs::metadata(path)?);
        Ok((before, data, after))
    };
    let (before, data, after) = read().map_err(|error| {
        NativeSessionError::Refusal(format!("native session rollout unreadable: {error}"))
    })?;
    if before != after {
        return Err(NativeSessionError::Changed(
            "native session rollout changed while it was being read".into(),
        ));
    }
    if !data.is_empty() && !data.ends_with(b"\n") {
        return refuse("native session rollout ends in a partial line");
    }
    // turn id -> (started, completed)
    let mut state: Vec<(String, Value, Option<Value>)> = Vec::new();
    let mut started: Vec<String> = Vec::new();
    let mut completed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut authenticated = false;
    for (index, raw) in split_lines(&data).into_iter().enumerate() {
        let line_number = index + 1;
        if blank(raw) {
            continue;
        }
        let event: Value = serde_json::from_slice(raw).map_err(|error| {
            NativeSessionError::Refusal(format!(
                "native session rollout line {line_number} is malformed: {error}"
            ))
        })?;
        let Some(event) = event.as_object() else {
            return refuse(format!(
                "native session rollout line {line_number} is not an object"
            ));
        };
        if line_number == 1 {
            if event.get("type").and_then(Value::as_str) != Some("session_meta") {
                return refuse("native session rollout has no authenticated session_meta");
            }
            let Some(meta) = event.get("payload").and_then(Value::as_object) else {
                return refuse("native session_meta has no payload");
            };
            let meta_id = meta
                .get("session_id")
                .filter(|v| truthy(Some(v)))
                .or_else(|| meta.get("id"));
            let named = if truthy(meta_id) {
                py_str(meta_id.unwrap_or(&Value::Null))
            } else {
                String::new()
            };
            if named != session_id {
                return refuse(format!(
                    "native session_meta names {}, not {}",
                    repr_value(meta_id),
                    py_repr(session_id)
                ));
            }
            if let (Some(expected), Some(meta_cwd)) = (
                expected_cwd.filter(|c| !c.is_empty()),
                meta.get("cwd").filter(|v| truthy(Some(v))),
            ) {
                if realpath(&py_str(meta_cwd)) != realpath(expected) {
                    return refuse(format!(
                        "native session_meta cwd {} does not match {}",
                        repr_value(Some(meta_cwd)),
                        py_repr(expected)
                    ));
                }
            }
            authenticated = true;
            continue;
        }
        if event.get("type").and_then(Value::as_str) != Some("event_msg") {
            continue;
        }
        let Some(payload) = event.get("payload").and_then(Value::as_object) else {
            return refuse(format!(
                "native session rollout line {line_number} has no payload"
            ));
        };
        if let Some(thread) = payload.get("thread_id").filter(|v| truthy(Some(v))) {
            if py_str(thread) != session_id {
                return refuse(format!(
                    "native session rollout line {line_number} names thread {}, not {}",
                    repr_value(Some(thread)),
                    py_repr(session_id)
                ));
            }
        }
        let event_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if event_type != "task_started" && event_type != "task_complete" {
            continue;
        }
        let Some(turn) = payload.get("turn_id").filter(|v| truthy(Some(v))) else {
            return refuse(format!(
                "native session {event_type} at line {line_number} has no turn id"
            ));
        };
        let turn_id = py_str(turn);
        let stamp = event.get("timestamp").cloned().unwrap_or(Value::Null);
        let position = state.iter().position(|(id, _, _)| *id == turn_id);
        if event_type == "task_started" {
            if position.is_some() {
                return refuse(format!(
                    "native session has duplicate task_started for turn {turn_id}"
                ));
            }
            state.push((turn_id.clone(), stamp, None));
            started.push(turn_id);
        } else {
            let Some(at) = position else {
                return refuse(format!(
                    "native session task_complete for turn {turn_id} has no start"
                ));
            };
            if state[at].2.is_some() {
                return refuse(format!(
                    "native session has duplicate task_complete for turn {turn_id}"
                ));
            }
            state[at].2 = Some(stamp);
            completed.insert(turn_id);
        }
    }
    if !authenticated {
        return refuse("native session rollout has no authenticated session_meta");
    }
    let active = started
        .iter()
        .filter_map(|id| {
            state
                .iter()
                .find(|(s, _, done)| s == id && done.is_none())
                .map(|(s, at, _)| (s.clone(), at.clone()))
        })
        .collect();
    Ok(Rollout {
        signature: after,
        started,
        completed: completed.into_iter().collect(),
        active,
    })
}

/// `check_native_session_quiescence`: a snapshot, or refuse. `None` means the incoming command
/// is not a same-session Codex resume. A same-session resume must carry an explicit isolated
/// `CODEX_HOME`; it is resolved through the exact thread row, never guessed from the pane.
pub fn check_native_session_quiescence(
    argv: &[String],
    pane_cwd: Option<&str>,
) -> Result<Option<NativeSessionSnapshot>, NativeSessionError> {
    let Some(request) = find_resume_request(argv) else {
        return Ok(None);
    };
    if request.session_id.is_empty() {
        return Ok(None);
    }
    let Some(codex_home) = request.codex_home.as_deref().filter(|h| !h.is_empty()) else {
        return refuse("same-session Codex resume requires an explicit isolated CODEX_HOME");
    };
    let home = realpath(&expanduser(codex_home));
    if home == realpath(&expanduser("~/.codex")) {
        return refuse("same-session Codex resume must not use the shared ~/.codex home");
    }
    let expected_cwd = request
        .cwd
        .as_deref()
        .filter(|c| !c.is_empty())
        .or(pane_cwd.filter(|c| !c.is_empty()));
    let (row, rollout_path) = state_row(&home, &request.session_id)?;
    let row_cwd = realpath(&row.cwd);
    if let Some(expected) = expected_cwd {
        if row_cwd != realpath(expected) {
            return refuse(format!(
                "native session cwd mismatch: state says {}, request says {}",
                py_repr(&row_cwd),
                py_repr(&realpath(expected))
            ));
        }
    }
    let rollout = read_rollout(&rollout_path, &request.session_id, expected_cwd)?;
    if !rollout.active.is_empty() {
        let turns: Vec<String> = rollout
            .active
            .iter()
            .map(|(id, at)| {
                format!(
                    "{id} (started {})",
                    if truthy(Some(at)) {
                        py_str(at)
                    } else {
                        "unknown".into()
                    }
                )
            })
            .collect();
        return refuse(format!(
            "native session {} has unfinished turn(s): {}",
            request.session_id,
            turns.join(", ")
        ));
    }
    Ok(Some(NativeSessionSnapshot {
        session_id: request.session_id,
        rollout_path,
        signature: rollout.signature,
        active_turns: rollout.active,
        started_turns: rollout.started,
        completed_turns: rollout.completed,
    }))
}

#[cfg(test)]
#[path = "native_session_tests.rs"]
mod native_session_tests;
