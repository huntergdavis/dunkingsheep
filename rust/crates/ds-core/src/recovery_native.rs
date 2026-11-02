//! Process-bound native Codex session identity for recovery (t1633).
//!
//! This module resolves one exact session from the live Codex process, an explicit operator
//! binding, or a hook record bound to the process generation and pane. It does not read rollout
//! history and never selects the newest session found on disk.

use std::collections::{BTreeMap, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use serde_json::Value;

use crate::native_session::{self, NativeSessionError, NativeSessionSnapshot, Signature};
use crate::proc::{self, PaneProcess};

const CODEX_HOME: &str = "CODEX_HOME";
const STATE_DIR: &str = "DUNKINGSHEEP_CODEX_STATE_DIR";
const BINDINGS_ENV: &str = "DUNKINGSHEEP_RECOVERY_BINDINGS";

/// The decoded prompt transcript, authenticated to one live process and exact rollout file.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptSession {
    pub snapshot: NativeSessionSnapshot,
    pub text: String,
    pub pid: i64,
    pub start: String,
}

impl PromptSession {
    /// Refuse when either the bound process generation or its rollout file changed.
    pub fn assert_unchanged(&self) -> Result<(), NativeSessionError> {
        self.assert_unchanged_with(|| proc::process_alive(self.pid, Some(&self.start)))
    }

    fn assert_unchanged_with<P>(&self, mut process_alive: P) -> Result<(), NativeSessionError>
    where
        P: FnMut() -> bool,
    {
        if !process_alive() {
            return refuse("the bound Codex process changed");
        }
        self.snapshot.assert_unchanged()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PromptSessionCacheKey {
    pid: i64,
    start: String,
    session_id: String,
    path: String,
    cwd: String,
    signature: Signature,
}

#[derive(Default)]
struct PromptSessionCache {
    entries: VecDeque<(PromptSessionCacheKey, PromptSession)>,
}

static PROMPT_SESSION_CACHE: OnceLock<Mutex<PromptSessionCache>> = OnceLock::new();

/// Read the decoded transcript for the exact process-bound native session named by `pane`.
///
/// The process/session resolver and rollout reader remain the authority for session identity,
/// cwd and turn state. This function only extracts decoded message bodies from that same file.
pub fn read_prompt_session(pane: &Value) -> Result<PromptSession, NativeSessionError> {
    let binding = resolve_prompt_session(pane)?;
    let cache = PROMPT_SESSION_CACHE.get_or_init(|| Mutex::new(PromptSessionCache::default()));
    read_prompt_session_from_binding(&binding, cache)
}

fn read_prompt_session_from_binding(
    binding: &PromptSessionBinding,
    cache: &Mutex<PromptSessionCache>,
) -> Result<PromptSession, NativeSessionError> {
    read_prompt_session_from_binding_with(
        binding,
        cache,
        || {},
        || proc::process_alive(binding.pid, Some(&binding.start)),
    )
}

fn read_prompt_session_from_binding_with<A, P>(
    binding: &PromptSessionBinding,
    cache: &Mutex<PromptSessionCache>,
    after_cache_key: A,
    mut process_alive: P,
) -> Result<PromptSession, NativeSessionError>
where
    A: FnOnce(),
    P: FnMut() -> bool,
{
    let initial_signature = fs::metadata(&binding.rollout_path)
        .map(|metadata| native_session::signature(&metadata))
        .map_err(|_| NativeSessionError::Refusal("native session rollout is unreadable".into()))?;
    let initial_key = cache_key(binding, initial_signature);
    after_cache_key();

    let cached = cache
        .lock()
        .map_err(|_| NativeSessionError::Refusal("prompt session cache is unavailable".into()))?
        .entries
        .iter()
        .find(|(key, _)| *key == initial_key)
        .map(|(_, value)| value.clone());
    if let Some(cached) = cached {
        cached.assert_unchanged_with(&mut process_alive)?;
        return Ok(cached);
    }

    if !process_alive() {
        return refuse("the bound Codex process changed");
    }
    let rollout = native_session::read_rollout(
        &binding.rollout_path,
        &binding.session_id,
        Some(&binding.cwd),
    )?;
    if !rollout.active.is_empty() {
        return refuse("the native session has an active turn");
    }
    let snapshot = NativeSessionSnapshot {
        session_id: binding.session_id.clone(),
        rollout_path: binding.rollout_path.clone(),
        signature: rollout.signature,
        active_turns: rollout.active,
        started_turns: rollout.started,
        completed_turns: rollout.completed,
    };
    snapshot.assert_unchanged()?;
    let transcript = fs::read_to_string(&binding.rollout_path)
        .map_err(|_| NativeSessionError::Refusal("native session rollout is unreadable".into()))?;
    let text = decode_prompt_bodies(&transcript)?;
    if fs::metadata(&binding.rollout_path)
        .map(|metadata| native_session::signature(&metadata))
        .ok()
        != Some(snapshot.signature)
    {
        return Err(NativeSessionError::Changed(
            "native session changed during the prompt read".into(),
        ));
    }
    if !process_alive() {
        return refuse("the bound Codex process changed");
    }
    snapshot.assert_unchanged()?;
    let evidence = PromptSession {
        snapshot,
        text,
        pid: binding.pid,
        start: binding.start.clone(),
    };
    evidence.assert_unchanged()?;

    let key = cache_key(binding, evidence.snapshot.signature);
    let mut cache = cache
        .lock()
        .map_err(|_| NativeSessionError::Refusal("prompt session cache is unavailable".into()))?;
    cache.entries.push_back((key, evidence.clone()));
    while cache.entries.len() > 8 {
        cache.entries.pop_front();
    }
    Ok(evidence)
}

fn cache_key(binding: &PromptSessionBinding, signature: Signature) -> PromptSessionCacheKey {
    PromptSessionCacheKey {
        pid: binding.pid,
        start: binding.start.clone(),
        session_id: binding.session_id.clone(),
        path: binding.rollout_path.clone(),
        cwd: binding.cwd.clone(),
        signature,
    }
}

fn decode_prompt_bodies(transcript: &str) -> Result<String, NativeSessionError> {
    let mut bodies = Vec::new();
    for (index, line) in transcript.lines().enumerate() {
        let event: Value = serde_json::from_str(line).map_err(|_| {
            NativeSessionError::Refusal(format!(
                "native session rollout line {} is malformed",
                index + 1
            ))
        })?;
        let Some(event) = event.as_object() else {
            return refuse("native session rollout event is not an object");
        };
        let payload_value = event.get("payload");
        let payload_value = if prompt_truthy(payload_value) {
            payload_value.expect("truthy payload is present")
        } else {
            continue;
        };
        let Some(payload) = payload_value.as_object() else {
            return refuse("native session rollout payload is not an object");
        };
        match event.get("type").and_then(Value::as_str) {
            Some("response_item")
                if payload.get("type").and_then(Value::as_str) == Some("message") =>
            {
                let Some(content) = payload.get("content") else {
                    continue;
                };
                let Some(content) = content.as_array() else {
                    return refuse("native session message content is not a list");
                };
                for part in content {
                    if let Some(part) = part.as_object() {
                        bodies.push(prompt_str_or_empty(part.get("text")));
                    }
                }
            }
            Some("event_msg")
                if matches!(
                    payload.get("type").and_then(Value::as_str),
                    Some("user_message" | "agent_message")
                ) =>
            {
                bodies.push(prompt_str_or_empty(payload.get("message")));
            }
            _ => {}
        }
    }
    Ok(bodies.join("\n"))
}

fn prompt_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn prompt_str_or_empty(value: Option<&Value>) -> String {
    if !prompt_truthy(value) {
        return String::new();
    }
    prompt_python_str(value.expect("truthy value is present"))
}

fn prompt_python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(value) => value.clone(),
        Value::Number(value) => value.to_string(),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(prompt_python_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{}: {}",
                    prompt_python_repr(&Value::String(key.clone())),
                    prompt_python_repr(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn prompt_python_repr(value: &Value) -> String {
    match value {
        Value::String(value) => prompt_python_string_repr(value),
        Value::Array(_) | Value::Object(_) => prompt_python_str(value),
        other => prompt_python_str(other),
    }
}

fn prompt_python_string_repr(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut result = String::with_capacity(value.len() + 2);
    result.push(quote);
    for character in value.chars() {
        match character {
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            character if character == quote => {
                result.push('\\');
                result.push(character);
            }
            character if character.is_control() => {
                use std::fmt::Write as _;
                write!(result, "\\x{:02x}", character as u32).expect("write to string");
            }
            character => result.push(character),
        }
    }
    result.push(quote);
    result
}

/// The resolved process and session identity, without reading prompt/rollout history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptSessionBinding {
    pub pid: i64,
    pub start: String,
    pub session_id: String,
    pub rollout_path: String,
    pub cwd: String,
}

/// Resolve the pane's single live Codex process to its exact native session.
///
/// Missing process or session evidence refuses. `pane` uses `pane_pid` (falling back to `pid`),
/// `pane_id`, and `cwd` (falling back to `foreground_cwd`).
pub fn resolve_prompt_session(pane: &Value) -> Result<PromptSessionBinding, NativeSessionError> {
    let pane_pid = pane
        .get("pane_pid")
        .filter(|value| !value.is_null())
        .or_else(|| pane.get("pid"))
        .and_then(Value::as_i64);
    let entries = proc::pane_agent_pids(pane_pid).unwrap_or_default();
    let environment = entries
        .iter()
        .find(|entry| entry.comm == "codex" && !proc::DEAD_STATES.contains(&entry.state.as_str()))
        .map(|entry| process_environment(entry.pid))
        .transpose()?
        .unwrap_or_default();
    let binding_path = std::env::var_os(BINDINGS_ENV).map(PathBuf::from);
    resolve_prompt_session_from(pane, &entries, &environment, binding_path.as_deref())
}

/// Read only the two process environment values used to bind a native session.
pub fn process_environment(pid: i64) -> Result<BTreeMap<String, String>, NativeSessionError> {
    let bytes = fs::read(format!("/proc/{pid}/environ")).map_err(|_| {
        NativeSessionError::Refusal("could not read the Codex process environment".into())
    })?;
    let wanted = [CODEX_HOME.as_bytes(), STATE_DIR.as_bytes()];
    let mut result = BTreeMap::new();
    for entry in bytes.split(|byte| *byte == 0) {
        let Some(separator) = entry.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let (key, value_with_separator) = entry.split_at(separator);
        let value = &value_with_separator[1..];
        if !wanted.contains(&key) {
            continue;
        }
        let key = std::str::from_utf8(key)
            .map_err(|_| NativeSessionError::Refusal("invalid Codex environment key".into()))?;
        let value = std::str::from_utf8(value).map_err(|_| {
            NativeSessionError::Refusal("invalid Codex session environment value".into())
        })?;
        result.insert(key.to_string(), value.to_string());
    }
    Ok(result)
}

fn resolve_prompt_session_from(
    pane: &Value,
    entries: &[PaneProcess],
    environment: &BTreeMap<String, String>,
    binding_path: Option<&Path>,
) -> Result<PromptSessionBinding, NativeSessionError> {
    let roots = entries
        .iter()
        .filter(|entry| entry.comm == "codex" && !proc::DEAD_STATES.contains(&entry.state.as_str()))
        .collect::<Vec<_>>();
    if roots.len() != 1 {
        return refuse("one live Codex process is required");
    }
    let root = roots[0];
    let pid = root.pid;
    let Some(start) = root.start.as_deref().filter(|start| !start.is_empty()) else {
        return refuse("the Codex process identity is stale");
    };
    let start = start.to_string();
    if !proc::process_alive(pid, Some(&start)) {
        return refuse("the Codex process identity is stale");
    }

    let registered = registered_binding(pane, pid, &start, binding_path)?;
    let request = native_session::find_resume_request(&root.argv);
    let mut session_id = request
        .as_ref()
        .map(|request| request.session_id.as_str())
        .filter(|session_id| !session_id.is_empty())
        .map(str::to_string);

    if let Some(record) = registered.as_ref() {
        if session_id
            .as_ref()
            .is_some_and(|running| running != &record.session_id)
        {
            return refuse("registered session disagrees with the running resume");
        }
        if let (Some(process_home), Some(registered_home)) = (
            environment.get(CODEX_HOME).filter(|home| !home.is_empty()),
            record.codex_home.as_deref(),
        ) {
            if native_session::realpath(process_home) != native_session::realpath(registered_home) {
                return refuse("registered home disagrees with the running Codex home");
            }
        }
        session_id = Some(record.session_id.clone());
    }

    if session_id.is_none() {
        let Some(directory) = environment
            .get(STATE_DIR)
            .filter(|directory| !directory.is_empty())
        else {
            return refuse("no process-bound native session id");
        };
        session_id = hook_session(directory, pane, pid, &start)?;
    }

    let session_id = session_id
        .filter(|session_id| !session_id.is_empty())
        .ok_or_else(|| {
            NativeSessionError::Refusal("no unique process-bound native session id".into())
        })?;
    let home = environment
        .get(CODEX_HOME)
        .filter(|home| !home.is_empty())
        .cloned()
        .or_else(|| {
            registered
                .as_ref()
                .and_then(|record| record.codex_home.clone())
        })
        .ok_or_else(|| {
            NativeSessionError::Refusal("Codex process has no explicit CODEX_HOME".into())
        })?;

    let (row, rollout_path) = native_session::state_row(&home, &session_id)?;
    let cwd = pane
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .or_else(|| {
            pane.get("foreground_cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
        })
        .ok_or_else(|| {
            NativeSessionError::Refusal("pane has no current working directory".into())
        })?;
    if native_session::realpath(&row.cwd) != native_session::realpath(cwd) {
        return refuse("native session cwd mismatch");
    }

    Ok(PromptSessionBinding {
        pid,
        start,
        session_id,
        rollout_path,
        cwd: cwd.to_string(),
    })
}

struct RegisteredBinding {
    codex_home: Option<String>,
    session_id: String,
}

fn registered_binding(
    pane: &Value,
    pid: i64,
    start: &str,
    binding_path: Option<&Path>,
) -> Result<Option<RegisteredBinding>, NativeSessionError> {
    let Some(path) = binding_path else {
        return Ok(None);
    };
    let text = fs::read_to_string(path).map_err(|_| {
        NativeSessionError::Refusal("could not read registered native bindings".into())
    })?;
    let bindings: Value = serde_json::from_str(&text).map_err(|_| {
        NativeSessionError::Refusal("registered native bindings are invalid JSON".into())
    })?;
    let pane_id = pane.get("pane_id").and_then(Value::as_str).unwrap_or("");
    let Some(record) = bindings.get(pane_id).filter(|record| record.is_object()) else {
        return Ok(None);
    };
    let record_pid = record.get("pid").and_then(Value::as_i64);
    let record_start = py_text(record.get("start"));
    let pane_cwd = pane
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .unwrap_or("");
    let record_cwd = record
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .unwrap_or("");
    if record_pid != Some(pid)
        || record_start != start
        || native_session::realpath(record_cwd) != native_session::realpath(pane_cwd)
    {
        return refuse("registered native binding has the wrong process generation/cwd");
    }
    let codex_home = record
        .get("codex_home")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let session_id = record
        .get("session_id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_string);
    let (Some(codex_home), Some(session_id)) = (codex_home, session_id) else {
        return refuse("registered native binding is incomplete");
    };
    Ok(Some(RegisteredBinding {
        codex_home: Some(codex_home),
        session_id,
    }))
}

fn hook_session(
    directory: &str,
    pane: &Value,
    pid: i64,
    start: &str,
) -> Result<Option<String>, NativeSessionError> {
    let entries = match fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return refuse("could not read process-bound native session directory"),
    };
    let pane_id = pane.get("pane_id");
    let pane_cwd = pane
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|cwd| !cwd.is_empty())
        .or_else(|| {
            pane.get("foreground_cwd")
                .and_then(Value::as_str)
                .filter(|cwd| !cwd.is_empty())
        })
        .unwrap_or("");
    let mut matches = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|_| {
            NativeSessionError::Refusal(
                "could not read process-bound native session directory".into(),
            )
        })?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let text = fs::read_to_string(&path).map_err(|_| {
            NativeSessionError::Refusal("could not read process-bound native session record".into())
        })?;
        let record: Value = serde_json::from_str(&text).map_err(|_| {
            NativeSessionError::Refusal(
                "process-bound native session record is invalid JSON".into(),
            )
        })?;
        if record.get("root_pid").and_then(Value::as_i64) != Some(pid)
            || py_text(record.get("root_start")) != start
            || record.get("pane_id") != pane_id
            || native_session::realpath(record.get("cwd").and_then(Value::as_str).unwrap_or(""))
                != native_session::realpath(pane_cwd)
        {
            continue;
        }
        matches.push(
            record
                .get("session_id")
                .and_then(Value::as_str)
                .filter(|session_id| !session_id.is_empty())
                .map(str::to_string),
        );
    }
    if matches.len() != 1 {
        return Ok(None);
    }
    Ok(matches.pop().flatten())
}

fn py_text(value: Option<&Value>) -> String {
    match value {
        None | Some(Value::Null) => "None".into(),
        Some(Value::String(value)) => value.clone(),
        Some(Value::Bool(true)) => "True".into(),
        Some(Value::Bool(false)) => "False".into(),
        Some(value) => value.to_string(),
    }
}

fn refuse<T>(message: &str) -> Result<T, NativeSessionError> {
    Err(NativeSessionError::Refusal(message.to_string()))
}

#[cfg(test)]
#[path = "recovery_native_tests.rs"]
mod tests;
