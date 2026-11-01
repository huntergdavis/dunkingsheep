//! `dunk_http.py` (T55, `http.rs`, slice 1): the route logic of the Command Center's HTTP
//! facade. The facade is THIN: no model, no state of its own, 127.0.0.1 only. Every response is
//! JSON; an object response carries `as_of`, `/api/terminals` is a bare array (its DS3
//! contract), and a field that cannot be computed is null WITH a reason, never a zero.
//!
//! IT IS NO LONGER READ-ONLY, and the exception is exactly one route: `POST
//! /api/terminals/<name>/resize`. Everything else is a GET. There is no authentication here
//! (loopback and filesystem permissions are the boundary).
//!
//! This slice is the pure part: `route`, `route_post`, `terminals`, `resize_terminal`,
//! `unique_names`, `error_body`, over two small traits (`Daemon`, `Backend`) so tests use fakes.
//! NOT in this slice: the TCP listener / request parsing (`Handler`, `serve`, `main`): RUST_PORT
//! lists no HTTP-server crate for `ds-core` (axum is named only for the phase-3 `ds-web`), so
//! that slice needs a decision: a hand-rolled HTTP/1.1 loop over tokio or the approved crate.
//!
//! Routes: GET /healthz | /health, /api/roster, /api/messages/state?ids=i1,i2,
//! /api/reply-stats[?mailboxes=a,b], /api/projects/<name> ('global' for the global store),
//! /api/terminals; POST /api/terminals/<name>/resize ({"cols": int, "rows": int}).

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{json, Map, Value};

/// The one write route takes a tiny JSON body; anything larger is not a resize request.
pub const MAX_POST_BYTES: usize = 4096;

/// An answer that is not a success, carrying what the caller needs to act on it. `context` is
/// echoed back verbatim and is deliberately LIMITED: for a refused resize it holds the name and
/// the size ASKED FOR (`requested`) and never a measured geometry field, so a caller cannot read
/// a refusal as applied.
#[derive(Debug, Clone, PartialEq)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
    pub context: Map<String, Value>,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            message: message.into(),
            context: Map::new(),
        }
    }

    fn with_context(mut self, context: Map<String, Value>) -> ApiError {
        self.context = context;
        self
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// `error_body`: the JSON a client sees for a refusal: `error` first, then only echoed context.
pub fn error_body(error: &ApiError) -> Value {
    let mut out = Map::new();
    out.insert("error".into(), json!(error.message));
    for (key, value) in &error.context {
        out.insert(key.clone(), value.clone());
    }
    Value::Object(out)
}

/// Why a daemon call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonError {
    /// `DaemonUnavailable`: the daemon could not be reached (or started).
    Unavailable(String),
    /// Any other failure of the call (a timeout, a closed socket, a refusal).
    Failed(String),
}

/// The daemon socket: `_call(command, **args)`.
pub trait Daemon {
    fn call(&self, command: &str, args: &Value) -> Result<Value, DaemonError>;
}

/// The terminal backend this host actually runs (`dunk_terminal.get_backend()`).
pub trait Backend {
    fn name(&self) -> String;
    /// `capabilities()`: e.g. `resize`.
    fn capabilities(&self) -> BTreeSet<String>;
    /// `list_panes_grouped()` as pane dicts (`pane_id`, `label`, `agent`, `cols`, `rows`, ...).
    fn list_panes_grouped(&self) -> Vec<Value>;
    /// `resize(pane_id, cols, rows)`: `Ok(measured result)` or `Err(the backend's own words)`.
    /// `cols`/`rows` are passed through as given, so the backend refuses a non-integer itself.
    fn resize(
        &self,
        pane_id: &str,
        cols: &Value,
        rows: &Value,
    ) -> Result<Map<String, Value>, String>;
}

/// Python `repr(str)`.
fn py_repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') {
        format!("\"{s}\"")
    } else {
        format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

fn text_of(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn truthy_text(v: Option<&Value>) -> Option<String> {
    let t = text_of(v);
    if t.is_empty() {
        None
    } else {
        Some(t)
    }
}

/// `urllib.parse.unquote`: `%XX` sequences decoded as UTF-8 (invalid bytes become U+FFFD).
pub fn unquote(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// `parse_qs`: `a=1&a=2&b=3` into name -> values; `+` is a space, `%XX` is decoded, and a blank
/// value is dropped (the stdlib default `keep_blank_values=False`). Pairs split on `&` ONLY: since
/// Python 3.10 a `;` is data, not a separator (`parse_qs('ids=a;b&x=1')` is `{'ids': ['a;b'],
/// 'x': ['1']}`), so `?ids=i1;i2` must reach the daemon whole.
pub fn parse_query(query: &str) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        if value.is_empty() {
            continue;
        }
        out.entry(unquote(&name.replace('+', " ")))
            .or_default()
            .push(unquote(&value.replace('+', " ")));
    }
    out
}

/// `_unique_names`: give every pane a name a caller can address it by, made unique. The name a
/// lane is addressed by is the registry label recorded at start_agent (falling back to the pane
/// title, then the pane id). That label is NOT unique on a real host (two panes carry
/// 'opencode-ds41-b'), so the second and later holders get `-2`, `-3`, ... in the stable order
/// `list_panes_grouped` returns - the same pane gets the same suffix on every call.
pub fn unique_names(panes: &[Value]) -> Vec<(String, Value)> {
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut seen: BTreeMap<String, u64> = BTreeMap::new();
    let mut named = Vec::new();
    for pane in panes {
        let base = truthy_text(pane.get("label"))
            .or_else(|| truthy_text(pane.get("terminal_title_stripped")))
            .or_else(|| truthy_text(pane.get("pane_id")))
            .unwrap_or_else(|| "terminal".into());
        let base = base.trim().to_string();
        let base = if base.is_empty() {
            "terminal".to_string()
        } else {
            base
        };
        let mut candidate = base.clone();
        while used.contains(&candidate) {
            let n = seen.entry(base.clone()).or_insert(1);
            *n += 1;
            candidate = format!("{base}-{n}");
        }
        used.insert(candidate.clone());
        named.push((candidate, pane.clone()));
    }
    named
}

/// `_lane_states`: pane status from the daemon, plus the reason when it cannot be had. A state
/// that cannot be computed is null WITH its reason, and a daemon outage downgrades one field
/// instead of failing the whole list.
fn lane_states(daemon: &dyn Daemon) -> (BTreeMap<String, Value>, Option<String>) {
    match daemon.call("list_panes", &json!({})) {
        Err(DaemonError::Unavailable(e)) | Err(DaemonError::Failed(e)) => {
            (BTreeMap::new(), Some(format!("daemon call failed: {e}")))
        }
        Ok(payload) => {
            let states = payload
                .get("panes")
                .and_then(Value::as_array)
                .map(|panes| {
                    panes
                        .iter()
                        .filter_map(|p| truthy_text(p.get("pane_id")).map(|id| (id, p.clone())))
                        .collect()
                })
                .unwrap_or_default();
            (states, None)
        }
    }
}

/// `terminals`: the DS3 contract, a top-level array of `{name, agent, state, cols, rows}`.
/// `pane_id` rides along so a caller can name a pane when a label collided, and `state_reason`
/// exists only when `state` is null.
pub fn terminals(backend: &dyn Backend, daemon: &dyn Daemon) -> Value {
    let panes = backend.list_panes_grouped();
    let (states, reason) = lane_states(daemon);
    let mut out = Vec::new();
    for (name, pane) in unique_names(&panes) {
        let pane_id = text_of(pane.get("pane_id"));
        let status = states.get(&pane_id).cloned().unwrap_or_else(|| json!({}));
        let state = status.get("agent_status").cloned().unwrap_or(Value::Null);
        let mut item = Map::new();
        item.insert("name".into(), json!(name));
        item.insert(
            "agent".into(),
            truthy_text(pane.get("agent"))
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        item.insert("state".into(), state.clone());
        item.insert(
            "cols".into(),
            pane.get("cols").cloned().unwrap_or(Value::Null),
        );
        item.insert(
            "rows".into(),
            pane.get("rows").cloned().unwrap_or(Value::Null),
        );
        item.insert(
            "pane_id".into(),
            pane.get("pane_id").cloned().unwrap_or(Value::Null),
        );
        if state.is_null() {
            let why = truthy_text(status.get("agent_status_reason"))
                .or(reason.clone())
                .unwrap_or_else(|| "no activity source for this pane".into());
            item.insert("state_reason".into(), json!(why));
        }
        out.push(Value::Object(item));
    }
    Value::Array(out)
}

fn call(daemon: &dyn Daemon, command: &str, args: Value) -> Result<Value, ApiError> {
    daemon.call(command, &args).map_err(|error| match error {
        DaemonError::Unavailable(e) => {
            ApiError::new(503, format!("Dunking Sheep unavailable: {e}"))
        }
        DaemonError::Failed(e) => ApiError::new(503, format!("daemon call failed: {e}")),
    })
}

/// `route`: the GET half.
pub fn route(
    path: &str,
    query: &BTreeMap<String, Vec<String>>,
    backend: &dyn Backend,
    daemon: &dyn Daemon,
) -> Result<Value, ApiError> {
    match path {
        "/healthz" | "/health" => return Ok(json!({"ok": true, "service": "dunkingsheep-api"})),
        "/api/roster" => return call(daemon, "roster", json!({})),
        "/api/messages/state" => {
            let ids = query
                .get("ids")
                .and_then(|v| v.first())
                .cloned()
                .unwrap_or_default();
            if ids.trim().is_empty() {
                return Err(ApiError::new(400, "ids is required, e.g. ?ids=i1,i2"));
            }
            return call(daemon, "message_state", json!({"message_ids": ids}));
        }
        "/api/reply-stats" => {
            let boxes = query
                .get("mailboxes")
                .and_then(|v| v.first())
                .filter(|b| !b.is_empty());
            return call(
                daemon,
                "reply_stats",
                match boxes {
                    Some(b) => json!({"mailboxes": b}),
                    None => json!({}),
                },
            );
        }
        "/api/terminals" => return Ok(terminals(backend, daemon)),
        _ => {}
    }
    if let Some(rest) = path.strip_prefix("/api/projects/") {
        let name = rest.trim_matches('/');
        if name.is_empty() {
            return Err(ApiError::new(404, "project name required"));
        }
        return call(daemon, "project_summary", json!({"project": name}));
    }
    Err(ApiError::new(404, format!("no route {path}")))
}

/// `route_post`: the write half. Exactly one route exists, and it is named here, not guessed. A
/// resize is addressed by the SAME name `GET /api/terminals` reported, resolved against the same
/// list, so a caller never has to hold a pane id - and a collision-suffixed name resolves to the
/// same pane every time, because the suffix order is stable.
pub fn route_post(
    path: &str,
    payload: &Value,
    backend: &dyn Backend,
    as_of: &str,
) -> Result<Value, ApiError> {
    const PREFIX: &str = "/api/terminals/";
    const SUFFIX: &str = "/resize";
    if path.starts_with(PREFIX) && path.ends_with(SUFFIX) {
        let end = path.len().saturating_sub(SUFFIX.len());
        let middle = if end >= PREFIX.len() {
            &path[PREFIX.len()..end]
        } else {
            ""
        };
        let name = unquote(middle.trim_matches('/'));
        if name.is_empty() {
            return Err(ApiError::new(400, "terminal name required"));
        }
        return resize_terminal(&name, payload, backend, as_of);
    }
    Err(ApiError::new(404, format!("no write route {path}")))
}

/// `resize_terminal`.
pub fn resize_terminal(
    name: &str,
    payload: &Value,
    backend: &dyn Backend,
    as_of: &str,
) -> Result<Value, ApiError> {
    if !backend.capabilities().contains("resize") {
        return Err(ApiError::new(
            501,
            format!("backend {} cannot resize panes", py_repr(&backend.name())),
        ));
    }
    let Some(object) = payload.as_object() else {
        return Err(ApiError::new(
            400,
            "body must be a JSON object with cols and rows",
        ));
    };
    let missing: Vec<&str> = ["cols", "rows"]
        .into_iter()
        .filter(|k| !object.contains_key(*k))
        .collect();
    if !missing.is_empty() {
        return Err(ApiError::new(
            400,
            format!("missing {}", missing.join(", ")),
        ));
    }
    let panes = backend.list_panes_grouped();
    let Some((_, target)) = unique_names(&panes)
        .into_iter()
        .find(|(candidate, _)| candidate == name)
    else {
        return Err(ApiError::new(
            404,
            format!("no terminal named {}", py_repr(name)),
        ));
    };
    let pane_id = text_of(target.get("pane_id"));
    match backend.resize(&pane_id, &object["cols"], &object["rows"]) {
        Err(message) => {
            // The ASK only - no measured geometry, no resized_panes - so a refusal cannot be
            // misread as a partial success.
            let mut context = Map::new();
            context.insert("name".into(), json!(name));
            context.insert(
                "pane_id".into(),
                target.get("pane_id").cloned().unwrap_or(Value::Null),
            );
            context.insert(
                "requested".into(),
                json!({"cols": object["cols"], "rows": object["rows"]}),
            );
            Err(ApiError::new(400, message).with_context(context))
        }
        Ok(result) => {
            let mut out = Map::new();
            out.insert("as_of".into(), json!(as_of));
            out.insert("name".into(), json!(name));
            for (key, value) in result {
                out.insert(key, value);
            }
            Ok(Value::Object(out))
        }
    }
}

/// `_now`: UTC `%Y-%m-%dT%H:%M:%SZ`.
pub fn now_iso() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
#[path = "http_tests.rs"]
mod http_tests;
