//! Isolated fleet routes. Production RPC, direct-message transport, local inventory,
//! and metrics connection ownership must be bound and verified under t1702 before mounting.
use super::response_error::WebResponseError;
mod inbox_summaries;
mod mailboxes;

use super::{
    auth::{AuthBackend, AuthError, Identity, RequestScheme},
    messages_write::MessageIn,
    panes::{PanesWebBackend, PanesWebError},
};
use crate::roster::{discovered_id, is_lane, match_pane};
use axum::{
    body::to_bytes,
    extract::{Extension, Request},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

pub type FleetFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[derive(Debug)]
pub enum FleetError {
    Http(StatusCode, String),
    Database(String),
    Other(String),
}
/// Required runtime operations, deliberately without success defaults. Summary owns
/// connect/query/close, including close on failure. Direct message transport must
/// implement Python _post_message(direct=True), preserving the supplied identity.
pub trait FleetSources: Send + Sync + 'static {
    fn registry(&self) -> Result<Value, FleetError>;
    fn session_mtimes(&self) -> Result<HashMap<String, f64>, FleetError>;
    fn inventory(&self, routed: &[String]) -> Value;
    fn configured_channels(&self) -> HashMap<String, Value>;
    fn post_direct_message<'a>(
        &'a self,
        data: MessageIn,
        identity: Identity,
    ) -> FleetFuture<'a, Result<Value, FleetError>>;
    fn summary<'a>(&'a self) -> FleetFuture<'a, Result<Value, FleetError>>;
}
pub struct FleetState {
    rpc: Arc<dyn PanesWebBackend>,
    sources: Arc<dyn FleetSources>,
    payload: Mutex<Option<(f64, Value)>>,
    capabilities: Mutex<HashMap<String, (f64, Value)>>,
}
impl FleetState {
    pub fn new(rpc: Arc<dyn PanesWebBackend>, sources: Arc<dyn FleetSources>) -> Self {
        Self {
            rpc,
            sources,
            payload: Mutex::new(None),
            capabilities: Mutex::new(HashMap::new()),
        }
    }
    async fn capability(&self, pane: &str) -> Value {
        let now = self.rpc.now_seconds();
        if let Some((at, value)) = self.capabilities.lock().unwrap().get(pane) {
            if now - at < 20.0 {
                return value.clone();
            }
        }
        match self
            .rpc
            .rpc("delivery_capability", json!({"target":pane}))
            .await
        {
            Ok(value) if value.is_object() => {
                self.capabilities
                    .lock()
                    .unwrap()
                    .insert(pane.into(), (now, value.clone()));
                value
            }
            _ => json!({}),
        }
    }
    fn cache(&self, value: Value) -> Response {
        *self.payload.lock().unwrap() = Some((self.rpc.now_seconds(), value.clone()));
        Json(value).into_response()
    }
    fn unavailable(&self, reason: &str) -> Response {
        self.cache(json!({"state":"unavailable","error":cut(reason,300),"rows":[],"count":null,"generated_at":self.rpc.checked_at()}))
    }
}
pub fn router(state: Arc<FleetState>, auth: Arc<dyn AuthBackend>, scheme: RequestScheme) -> Router {
    Router::new()
        .route("/api/pane-send", post(api_pane_send))
        .route("/api/pane-inbox", post(api_pane_inbox))
        .route("/api/fleet", get(api_fleet))
        .route("/api/projects", get(api_projects))
        .route("/api/mailboxes", get(mailboxes::api_mailboxes))
        .route(
            "/api/mailbox-messages",
            get(inbox_summaries::api_mailbox_messages),
        )
        .route("/api/ops-metrics", get(operations_metrics))
        .layer(Extension(state))
        .layer(Extension(auth))
        .layer(Extension(scheme))
}
fn detail(status: StatusCode, text: impl Into<String>) -> Response {
    (status, Json(json!({"detail":text.into()}))).into_response()
}
fn internal() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}
fn cut(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}
fn truth(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
        Value::Number(n) => n.as_f64() != Some(0.0),
    }
}
fn pystr(v: &Value) -> String {
    match v {
        Value::Null => "None".into(),
        Value::Bool(true) => "True".into(),
        Value::Bool(false) => "False".into(),
        Value::String(s) => s.clone(),
        Value::Array(a) => format!("[{}]", a.iter().map(repr).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => format!(
            "{{{}}}",
            o.iter()
                .map(|(k, v)| format!("{}: {}", repr(&json!(k)), repr(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        _ => v.to_string(),
    }
}
fn repr(v: &Value) -> String {
    if let Value::String(s) = v {
        format!(
            "'{}'",
            s.replace('\\', "\\\\")
                .replace('\'', "\\'")
                .replace('\n', "\\n")
                .replace('\r', "\\r")
                .replace('\t', "\\t")
        )
    } else {
        pystr(v)
    }
}
fn text(v: &Value) -> String {
    if truth(v) {
        pystr(v).trim().into()
    } else {
        String::new()
    }
}
fn error_response(e: FleetError) -> Response {
    match e {
        FleetError::Http(s, d) => detail(s, d),
        _ => internal(),
    }
}

async fn api_pane_send(
    Extension(state): Extension<Arc<FleetState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    let identity = match authenticate(auth.as_ref(), request.headers(), scheme).await {
        Ok(i) => i,
        Err(r) => return r.into_response(),
    };
    let bytes = match to_bytes(request.into_body(), usize::MAX).await {
        Ok(b) => b,
        Err(_) => return internal(),
    };
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return internal(),
    };
    if !body.is_object() {
        return internal();
    }
    if truth(&body["channel"]) {
        // Model construction occurs inside the Python handler: ValidationError is a
        // server error, not FastAPI's automatic request-model 422 response.
        let Some(channel) = body["channel"].as_str() else {
            return internal();
        };
        let message = body.get("text").cloned().unwrap_or(json!(""));
        let Some(message) = message.as_str() else {
            return internal();
        };
        let attachments = body.get("attachment_ids").cloned().unwrap_or(json!([]));
        let Some(attachments) = attachments.as_array() else {
            return internal();
        };
        if message.chars().count() > 12000
            || attachments.len() > 10
            || attachments.iter().any(|v| !v.is_string())
        {
            return internal();
        }
        let listing = match state.rpc.rpc("list_panes", json!({})).await {
            Ok(v) => v,
            Err(e) => {
                return rpc_error(
                    e,
                    "Dunking Sheep unavailable: ",
                    StatusCode::SERVICE_UNAVAILABLE,
                )
            }
        };
        let Some(panes) = listing["panes"].as_array() else {
            return internal();
        };
        let configured = state.sources.configured_channels();
        let target = if let Some(config) = configured.get(channel) {
            match match_pane(config, panes) {
                Ok(p) => p,
                Err(_) => return internal(),
            }
        } else {
            let mut found = None;
            for pane in panes {
                if discovered_id(pane["pane_id"].as_str().unwrap_or("")) == channel {
                    let allowed = is_lane(Some(pane)).unwrap_or(false)
                        || state
                            .capability(pane["pane_id"].as_str().unwrap_or(""))
                            .await["agent_alive"]
                            == true;
                    if allowed {
                        found = Some(pane.clone());
                        break;
                    }
                }
            }
            if found.is_none()
                && !(channel.starts_with("pane-")
                    && channel.len() > 5
                    && channel[5..]
                        .bytes()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-'))
            {
                return detail(StatusCode::NOT_FOUND, "Unknown channel");
            }
            found
        };
        if target.as_ref().is_none_or(|p| p["pane_id"] != body["pane"]) {
            return detail(
                StatusCode::CONFLICT,
                "Channel target changed; refresh before sending",
            );
        }
        if message.trim().is_empty() && attachments.is_empty() {
            return detail(StatusCode::UNPROCESSABLE_ENTITY, "Message is empty");
        }
        let data = MessageIn {
            channel: channel.into(),
            body: message.into(),
            attachment_ids: attachments
                .iter()
                .map(|v| v.as_str().unwrap().into())
                .collect(),
            external_id: None,
        };
        return match state.sources.post_direct_message(data, identity).await {
            Ok(v) => Json(v).into_response(),
            Err(e) => error_response(e),
        };
    }
    let pane = text(&body["pane"]);
    let message = text(&body["text"]);
    if !pane.starts_with('%') {
        return detail(StatusCode::BAD_REQUEST, "a tmux pane id looks like %9");
    }
    if message.is_empty() {
        return detail(StatusCode::BAD_REQUEST, "nothing to send");
    }
    if message.chars().count() > 16000 {
        return detail(StatusCode::BAD_REQUEST, "keep it under 16000 characters");
    }
    let cap = state.capability(&pane).await;
    if !truth(&cap) {
        return detail(
            StatusCode::BAD_GATEWAY,
            "the daemon could not describe this lane",
        );
    }
    if !truth(&cap["alive"]) {
        return detail(
            StatusCode::CONFLICT,
            format!("{pane} is not alive: nothing to send to"),
        );
    }
    let route = cap["send"].as_str().unwrap_or("");
    if !matches!(route, "owner_push" | "guarded") {
        return detail(
            StatusCode::CONFLICT,
            format!(
                "{pane} declares no send route ({}); not guessing",
                repr(&cap["send"])
            ),
        );
    }
    let (method, args) = if route == "owner_push" {
        ("owner_push", json!({"target":pane,"body":message}))
    } else {
        ("send_text", json!({"target":pane,"text":message}))
    };
    match state.rpc.rpc(method, args).await {
        Ok(v) => send_response(&pane, route, &v),
        Err(e) => rpc_error(e, "delivery path unavailable: ", StatusCode::BAD_GATEWAY),
    }
}
/// The largest `/api/pane-inbox` request body: 16000 characters of at most four bytes each, plus
/// room for JSON escaping and framing.
const PANE_INBOX_BODY_LIMIT: usize = 16000 * 6 + 1024;

/// A tmux pane id, nothing else: the target is never a label, a name or `self`.
fn tmux_pane_id(value: &Value) -> Option<&str> {
    let pane = value.as_str()?;
    let digits = pane.strip_prefix('%')?;
    (!digits.is_empty() && digits.len() <= 9 && digits.bytes().all(|b| b.is_ascii_digit()))
        .then_some(pane)
}

/// `POST /api/pane-inbox` (t4644): store one inbox message for a live lane. Rust desk extension
/// with no Python route. The same sign-in and same-origin check as `/api/pane-send`, then the
/// daemon's ordinary `inbox_send` under the desk's own service identity: nothing is typed into a
/// pane and no command the desk could not already call is used. The daemon decides where the
/// message is stored; its answer (id, mailbox, any redirect, whether the lane was told) is
/// returned as given, never invented.
async fn api_pane_inbox(
    Extension(state): Extension<Arc<FleetState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response.into_response();
    }
    let Ok(bytes) = to_bytes(request.into_body(), PANE_INBOX_BODY_LIMIT).await else {
        return detail(StatusCode::PAYLOAD_TOO_LARGE, "message too large");
    };
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(value @ Value::Object(_)) => value,
        _ => {
            return detail(
                StatusCode::BAD_REQUEST,
                "send a JSON object with pane and text",
            )
        }
    };
    let Some(pane) = tmux_pane_id(&body["pane"]) else {
        return detail(StatusCode::BAD_REQUEST, "a tmux pane id looks like %9");
    };
    let Some(message) = body["text"].as_str() else {
        return detail(StatusCode::BAD_REQUEST, "text must be a string");
    };
    if message.trim().is_empty() {
        return detail(StatusCode::BAD_REQUEST, "nothing to send");
    }
    if message.chars().count() > 16000 {
        return detail(StatusCode::BAD_REQUEST, "keep it under 16000 characters");
    }
    let cap = state.capability(pane).await;
    if !truth(&cap) {
        return detail(
            StatusCode::BAD_GATEWAY,
            "the daemon could not describe this lane",
        );
    }
    if !truth(&cap["alive"]) {
        return detail(
            StatusCode::CONFLICT,
            format!("{pane} is not alive: nothing to send to"),
        );
    }
    let stored = match state
        .rpc
        .rpc(
            "inbox_send",
            json!({"to":pane,"body":message,"notify":true}),
        )
        .await
    {
        Ok(value) => value,
        Err(e) => return rpc_error(e, "inbox unavailable: ", StatusCode::BAD_GATEWAY),
    };
    let Some(message_id) = stored["message_id"].as_str().filter(|id| !id.is_empty()) else {
        return detail(
            StatusCode::BAD_GATEWAY,
            "the daemon returned no message id, so the desk will not claim the message was stored",
        );
    };
    if truth(&stored["outcome"]) {
        // Only a refused system send carries an outcome; the desk never sends as one.
        return detail(
            StatusCode::BAD_GATEWAY,
            format!(
                "the inbox refused the message: {}",
                cut(&text(&stored["reason"]), 200)
            ),
        );
    }
    Json(json!({
        "ok": true,
        "pane": pane,
        "route": "inbox",
        "outcome": "stored",
        "message_id": message_id,
        "mailbox": stored["mailbox"],
        "state": stored["state"],
        "notified": stored["notified"] == true,
        "redirected": stored["redirected"],
        "note": "Stored in the lane's inbox. It has not been read until the lane opens its mail.",
    }))
    .into_response()
}
/// A lane's unread inbox count (t3335). The roster count is filed under both the lane's pane id
/// and its name; Python added the two lookups, which doubled the count whenever both keys named
/// the same lane. One lane has one count: the pane id is the exact key, the name the fallback
/// for a roster row that carries no pane id.
///
/// `None` when the roster gave no count for this lane (the roster call failed, or the lane is
/// not in it): the row then carries `unread: null` with `unread_state: "unknown"`, because a
/// count that could not be read is not zero (t4646 S5a).
fn lane_unread(waiting: &HashMap<String, i64>, pane_id: &str, name: &str) -> Option<i64> {
    waiting.get(pane_id).or_else(|| waiting.get(name)).copied()
}
fn rpc_error(e: PanesWebError, prefix: &str, status: StatusCode) -> Response {
    match e {
        PanesWebError::Http { status, detail: d } => detail(status, d),
        PanesWebError::Failure(e) => detail(status, format!("{prefix}{e}")),
    }
}
fn send_response(pane: &str, route: &str, payload: &Value) -> Response {
    let chosen = if truth(&payload["message"]) {
        &payload["message"]
    } else {
        &payload["outcome"]
    };
    let description = cut(&text(chosen), 400);
    let outcome = text(&payload["outcome"]).to_lowercase();
    if route == "owner_push" {
        if outcome.is_empty() {
            return detail(StatusCode::BAD_GATEWAY,format!("the daemon stored the message but returned no outcome field, so the desk will not invent one: {}",cut(&description,160)));
        }
        return Json(json!({"ok":true,"pane":pane,"route":route,"outcome":outcome,"push_id":payload["id"],"detail":description,"note":"Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it."})).into_response();
    }
    if outcome == "refused" {
        return Json(json!({"ok":false,"pane":pane,"route":route,"outcome":outcome,"message_id":payload["message_id"],"detail":if truth(&payload["message"]) {payload["message"].clone()} else {json!(description)}})).into_response();
    }
    if outcome.is_empty() {
        return detail(
            StatusCode::CONFLICT,
            if description.is_empty() {
                "No daemon outcome".into()
            } else {
                description
            },
        );
    }
    let note=match outcome.as_str() {
        "queued"=>"Held by the typing guard or not yet delivered. It has NOT arrived - do not read this as delivered.",
        "unconfirmed"=>"Neither witness fired. It may well have arrived - do NOT resend blindly; ask the lane first.",
        "stored"=>"Stored in a durable queue the lane drains itself. That is NOT 'read' - it has not arrived until the lane takes it.",
        "ok"|"sent"=>"Confirmed by the box clearing or by the turn it started - that is delivery to the lane, not proof the agent acted on it.",
        _=>return detail(StatusCode::BAD_GATEWAY,format!("the sender returned no outcome field, so the desk will not guess: {}",cut(&description,200))),
    };
    let mut result = json!({"ok":true,"pane":pane,"route":route,"outcome":outcome,"message_id":payload["message_id"],"detail":description,"note":note});
    if matches!(outcome.as_str(), "queued" | "unconfirmed") {
        result[&outcome] = json!(true)
    }
    Json(result).into_response()
}
async fn api_fleet(
    Extension(state): Extension<Arc<FleetState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response.into_response();
    }
    let now = state.rpc.now_seconds();
    if let Some((at, payload)) = state.payload.lock().unwrap().as_ref() {
        if now - at < 3.0 {
            return Json(payload.clone()).into_response();
        }
    }
    let listing = match state.rpc.rpc("list_panes", json!({})).await {
        Ok(v) => v,
        Err(e) => {
            return state.unavailable(&format!(
                "pane roster could not be read: {}",
                match e {
                    PanesWebError::Failure(s) => s,
                    PanesWebError::Http { detail, .. } => detail,
                }
            ))
        }
    };
    let Some(panes) = listing["panes"].as_array() else {
        return state.unavailable("pane roster response is missing its panes list");
    };
    if panes
        .iter()
        .any(|p| !p.is_object() || !p["pane_id"].is_string())
    {
        return state.unavailable("pane roster contains an invalid pane");
    }
    let registry = state.sources.registry().unwrap_or(json!({}));
    let mut activity = HashMap::new();
    let mut sessions = HashMap::new();
    let mut waiting = HashMap::new();
    if let Ok(value) = state.rpc.rpc("state_latest", json!({"limit":500})).await {
        if let Some(rows) = value["rows"].as_array() {
            for row in rows {
                let Some(pane) = row["pane_id"].as_str().filter(|s| !s.is_empty()) else {
                    continue;
                };
                if activity.contains_key(pane) {
                    continue;
                }
                if let Some(at) = row["at"].as_f64() {
                    activity.insert(pane.to_owned(), at);
                }
                sessions.insert(pane.to_owned(), row["session_id"].clone());
            }
        }
    }
    // t4644: the daemon's roster is the list of lanes. It holds detected agents and also live
    // panes with a valid project or global membership, whose `agent` field is empty in the pane
    // listing (most Claude lanes). Those are lanes too, so they are rows here, named by the roster.
    let mut roster_names: HashMap<String, Value> = HashMap::new();
    if let Ok(value) = state.rpc.rpc("roster", json!({})).await {
        if let Some(lanes) = value["lanes"].as_array() {
            for lane in lanes {
                if let Some(pane) = lane["pane_id"].as_str().filter(|s| !s.is_empty()) {
                    roster_names.insert(pane.to_owned(), lane["name"].clone());
                }
                if let Some(unread) = lane["inbox"]["unread"].as_i64() {
                    for key in ["name", "pane_id"] {
                        if let Some(k) = lane[key].as_str().filter(|s| !s.is_empty()) {
                            waiting.insert(k.to_owned(), unread);
                        }
                    }
                }
            }
        }
    }
    let turns = state.sources.session_mtimes().unwrap_or_default();
    let pending = state
        .rpc
        .rpc("owner_stats", json!({}))
        .await
        .ok()
        .and_then(|v| pending_counts(&v));
    let mut rows = Vec::new();
    for pane in panes {
        let id = pane["pane_id"].as_str().unwrap();
        if id.is_empty() || !(truth(&pane["agent"]) || roster_names.contains_key(id)) {
            continue;
        }
        let recorded = &registry[id];
        let recorded = if recorded.is_object() {
            if truth(&recorded["mailbox"]) {
                &recorded["mailbox"]
            } else {
                &recorded["name"]
            }
        } else {
            recorded
        };
        let name = if truth(recorded) {
            recorded.clone()
        } else if truth(&pane["label"]) {
            pane["label"].clone()
        } else if let Some(listed) = roster_names
            .get(id)
            .filter(|name| name.is_string() && truth(name))
        {
            listed.clone()
        } else {
            json!(id)
        };
        let Some(name_str) = name.as_str() else {
            return internal();
        };
        let cwd = pane["cwd"].as_str().unwrap_or("");
        let project = cwd.trim_end_matches('/').rsplit('/').next().unwrap_or("");
        let cap = state.capability(id).await;
        let session = sessions.get(id).cloned().unwrap_or(Value::Null);
        let sid = session.as_str().unwrap_or("");
        let last = turns
            .get(sid)
            .filter(|t| **t != 0.0)
            .or_else(|| turns.get(sid.split('-').next().unwrap_or("")))
            .filter(|t| **t != 0.0)
            .or_else(|| activity.get(id).filter(|t| **t != 0.0));
        let unread = lane_unread(&waiting, id, name_str);
        rows.push(json!({"pane_id":id,"name":name,"project":project,"cwd":pane["cwd"],"agent":pane["agent"],"alive":true,
            "last_activity_s":last.map(|t|(now-t) as i64),"unread":unread,"unread_state":if unread.is_some(){"ok"}else{"unknown"},
            "session":session,"send":cap["send"],"drains":cap["drains"],"pending":pending.as_ref().map(|p|p.get(id).copied().unwrap_or(0)),
            "pending_state":if pending.is_some(){"ok"}else{"unknown"},"can_type":cap["send"]=="guarded"}));
    }
    rows.sort_by(|a, b| {
        (a["project"].as_str(), a["pane_id"].as_str())
            .cmp(&(b["project"].as_str(), b["pane_id"].as_str()))
    });
    let ids = rows
        .iter()
        .map(|r| r["pane_id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let inventory = state.sources.inventory(&ids);
    state.cache(json!({"state":"ok","generated_at":state.rpc.checked_at(),"count":rows.len(),"rows":rows,"unrouted":inventory,"note":"shells and dead panes are not lanes and are not listed"}))
}
fn pending_counts(value: &Value) -> Option<HashMap<String, u64>> {
    if !value.is_object() || value["ok"] == false {
        return None;
    }
    let mut counts = HashMap::new();
    for row in value["pending"].as_array()? {
        if !row.is_object() || (!row["pane_id"].is_null() && !row["pane_id"].is_string()) {
            return None;
        }
        if let Some(id) = row["pane_id"].as_str().filter(|s| !s.is_empty()) {
            *counts.entry(id.to_owned()).or_insert(0) += 1;
        }
    }
    Some(counts)
}
async fn api_projects(
    Extension(state): Extension<Arc<FleetState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    headers: HeaderMap,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), &headers, scheme).await {
        return response.into_response();
    }
    match state.rpc.rpc("roster", json!({})).await {
        Ok(value) if value.get("lanes").and_then(Value::as_array).is_some() => {
            Json(value).into_response()
        }
        _ => detail(StatusCode::BAD_GATEWAY, "Project roster unavailable"),
    }
}

async fn operations_metrics(
    Extension(state): Extension<Arc<FleetState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    headers: HeaderMap,
) -> Response {
    if let Err(r) = authenticate(auth.as_ref(), &headers, scheme).await {
        return r.into_response();
    }
    match state.sources.summary().await {
        Ok(v) => Json(v).into_response(),
        Err(FleetError::Database(class)) => detail(
            StatusCode::SERVICE_UNAVAILABLE,
            format!("Operations metrics unavailable: {class}"),
        ),
        Err(_) => internal(),
    }
}

async fn authenticate(
    backend: &dyn AuthBackend,
    headers: &HeaderMap,
    scheme: RequestScheme,
) -> Result<Identity, WebResponseError> {
    if let Some(response) = same_origin_refusal(headers, scheme) {
        return Err(response.into());
    }
    let bearer = bearer_value(headers);
    let cookie = cookie_value(headers, "vp_session");
    backend
        .identity(bearer.as_deref(), cookie.as_deref())
        .await
        .map_err(auth_error_response)
        .map_err(WebResponseError::from)
}

fn same_origin_refusal(headers: &HeaderMap, scheme: RequestScheme) -> Option<Response> {
    let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
    if origin.is_empty() {
        return None;
    }
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let scheme = match scheme {
        RequestScheme::Http => "http",
        RequestScheme::Https => "https",
    };
    (origin != format!("{scheme}://{host}"))
        .then(|| detail(StatusCode::FORBIDDEN, "Cross-origin request refused"))
}

fn bearer_value(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let value = headers.get(header::COOKIE)?;
    let value = value
        .as_bytes()
        .iter()
        .map(|byte| char::from(*byte))
        .collect::<String>();
    let mut cookie = None;
    for part in value.split(';') {
        let (key, value) = part.split_once('=').unwrap_or(("", part));
        if key.trim() == name {
            cookie = Some(unquote_cookie_value(value.trim()));
        }
    }
    cookie
}

fn unquote_cookie_value(value: &str) -> String {
    let Some(value) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return value.to_owned();
    };

    let chars = value.chars().collect::<Vec<_>>();
    let mut unquoted = String::with_capacity(value.len());
    let mut index = 0;
    while index < chars.len() {
        if chars[index] == '\\' && index + 1 < chars.len() {
            if index + 3 < chars.len()
                && matches!(chars[index + 1], '0'..='3')
                && matches!(chars[index + 2], '0'..='7')
                && matches!(chars[index + 3], '0'..='7')
            {
                let octal = (chars[index + 1] as u32 - '0' as u32) * 64
                    + (chars[index + 2] as u32 - '0' as u32) * 8
                    + (chars[index + 3] as u32 - '0' as u32);
                unquoted.push(char::from(octal as u8));
                index += 4;
            } else {
                unquoted.push(chars[index + 1]);
                index += 2;
            }
        } else {
            unquoted.push(chars[index]);
            index += 1;
        }
    }
    unquoted
}

fn auth_error_response(error: AuthError) -> Response {
    match error {
        AuthError::TooManyAttempts => detail(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts; retry in five minutes",
        ),
        AuthError::IncorrectPassword => detail(StatusCode::UNAUTHORIZED, "Incorrect password"),
        AuthError::SignInRequired => detail(StatusCode::UNAUTHORIZED, "Sign in required"),
        AuthError::Internal(_) => internal(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        auth::AuthFuture,
        panes::{PanesWebFuture, ViewportSample},
    };
    use super::*;
    use axum::body::Body;
    use std::{
        pin::pin,
        task::{Context, Poll, Waker},
    };
    use tower::ServiceExt;
    #[derive(Default)]
    struct Fake {
        calls: Mutex<Vec<(String, Value)>>,
        replies: Mutex<HashMap<String, Value>>,
        now: Mutex<f64>,
        registry: Mutex<Value>,
        turns: Mutex<HashMap<String, f64>>,
        summary_error: Mutex<Option<String>>,
        local_failure: Mutex<bool>,
        channels: Mutex<HashMap<String, Value>>,
    }
    impl Fake {
        fn reply(&self, key: &str, value: Value) {
            self.replies.lock().unwrap().insert(key.into(), value);
        }
        fn calls(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(m, _)| m.clone())
                .collect()
        }
        fn setup() -> Arc<Self> {
            let f = Arc::new(Self::default());
            *f.now.lock().unwrap() = 100.;
            f
        }
    }
    impl PanesWebBackend for Fake {
        fn rpc<'a>(
            &'a self,
            method: &'a str,
            args: Value,
        ) -> PanesWebFuture<'a, Result<Value, PanesWebError>> {
            self.calls.lock().unwrap().push((method.into(), args));
            let value = self.replies.lock().unwrap().get(method).cloned();
            Box::pin(async move { value.ok_or(PanesWebError::Failure("offline".into())) })
        }
        fn update_viewport<'a>(
            &'a self,
            _: &'a str,
            _: ViewportSample,
            _: f64,
        ) -> PanesWebFuture<'a, Result<bool, PanesWebError>> {
            panic!("unexpected viewport")
        }
        fn viewports(&self) -> PanesWebFuture<'_, Result<Vec<Value>, PanesWebError>> {
            panic!("unexpected viewport")
        }
        fn roster_rows<'a>(
            &'a self,
            _: &'a [Value],
        ) -> PanesWebFuture<'a, Result<Vec<Value>, PanesWebError>> {
            panic!("unexpected roster callback")
        }
        fn now_seconds(&self) -> f64 {
            *self.now.lock().unwrap()
        }
        fn checked_at(&self) -> String {
            "2026-11-03T00:00:00Z".into()
        }
    }
    impl FleetSources for Fake {
        fn registry(&self) -> Result<Value, FleetError> {
            if *self.local_failure.lock().unwrap() {
                return Err(FleetError::Other("registry unavailable".into()));
            }
            Ok(self.registry.lock().unwrap().clone())
        }
        fn session_mtimes(&self) -> Result<HashMap<String, f64>, FleetError> {
            if *self.local_failure.lock().unwrap() {
                return Err(FleetError::Other("session roots unavailable".into()));
            }
            Ok(self.turns.lock().unwrap().clone())
        }
        fn inventory(&self, ids: &[String]) -> Value {
            self.calls
                .lock()
                .unwrap()
                .push(("inventory".into(), json!(ids)));
            json!({"state":"unavailable"})
        }
        fn configured_channels(&self) -> HashMap<String, Value> {
            self.channels.lock().unwrap().clone()
        }
        fn post_direct_message<'a>(
            &'a self,
            data: MessageIn,
            identity: Identity,
        ) -> FleetFuture<'a, Result<Value, FleetError>> {
            self.calls.lock().unwrap().push(("post_direct".into(),json!({"channel":data.channel,"body":data.body,"attachments":data.attachment_ids,"api":identity==Identity::Api})));
            Box::pin(async { Ok(json!({"id":7,"transport":{"outcome":"stored"}})) })
        }
        fn summary(&self) -> FleetFuture<'_, Result<Value, FleetError>> {
            self.calls
                .lock()
                .unwrap()
                .push(("summary".into(), Value::Null));
            let error = self.summary_error.lock().unwrap().clone();
            Box::pin(async move {
                match error {
                    None => Ok(json!({"summary":"dashboard"})),
                    Some(s) if s == "other" => Err(FleetError::Other(s)),
                    Some(s) => Err(FleetError::Database(s)),
                }
            })
        }
    }
    struct Auth(Option<Identity>);
    impl AuthBackend for Auth {
        fn identity<'a>(
            &'a self,
            _: Option<&'a str>,
            _: Option<&'a str>,
        ) -> AuthFuture<'a, Result<Identity, AuthError>> {
            Box::pin(async move {
                if let Some(identity) = self.0 {
                    Ok(identity)
                } else {
                    Err(AuthError::SignInRequired)
                }
            })
        }
        fn login<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> AuthFuture<'a, Result<String, AuthError>> {
            panic!("unexpected login")
        }
        fn logout<'a>(&'a self, _: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
            panic!("unexpected logout")
        }
        fn change_password<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: Option<&'a str>,
        ) -> AuthFuture<'a, Result<(), AuthError>> {
            panic!("unexpected password")
        }
    }
    fn run<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        let mut cx = Context::from_waker(Waker::noop());
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("fake unexpectedly pending"),
        }
    }
    fn app(f: Arc<Fake>, auth: bool) -> Router {
        router(
            Arc::new(FleetState::new(f.clone(), f)),
            Arc::new(Auth(auth.then_some(Identity::Api))),
            RequestScheme::Http,
        )
    }
    fn http(app: Router, path: &str, body: Option<&str>) -> (u16, Value) {
        let req = Request::builder()
            .method(if body.is_some() { "POST" } else { "GET" })
            .uri(path)
            .header("host", "desk.test")
            .body(Body::from(body.unwrap_or("").to_owned()))
            .unwrap();
        let r = run(app.oneshot(req)).unwrap();
        let status = r.status().as_u16();
        let bytes = run(to_bytes(r.into_body(), usize::MAX)).unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(json!(String::from_utf8_lossy(&bytes))),
        )
    }
    fn send(f: Arc<Fake>, body: &str) -> (u16, Value) {
        http(app(f, true), "/api/pane-send", Some(body))
    }
    #[test]
    fn projects_require_auth_and_return_the_daemon_roster() {
        let f = Fake::setup();
        assert_eq!(http(app(f.clone(), false), "/api/projects", None).0, 401);
        assert!(f.calls().is_empty());
        assert_eq!(http(app(f.clone(), true), "/api/projects", None).0, 502);
        let roster = json!({"as_of":"now","lanes":[
            {"pane_id":"%1","membership":{"scope":"project","project":"alpha"}},
            {"pane_id":"%2","membership":{"scope":"global","project":null}},
            {"pane_id":"%3","membership":{"scope":"stale","project":null}},
            {"pane_id":"%4","membership":{"scope":"unknown","project":null}}
        ]});
        f.reply("roster", roster.clone());
        assert_eq!(
            http(app(f.clone(), true), "/api/projects", None),
            (200, roster)
        );
    }

    fn owner_app(f: Arc<Fake>) -> Router {
        router(
            Arc::new(FleetState::new(f.clone(), f)),
            Arc::new(Auth(Some(Identity::Owner))),
            RequestScheme::Http,
        )
    }

    #[test]
    fn mailbox_index_requires_owner_and_same_origin_before_any_rpc() {
        let f = Fake::setup();
        assert_eq!(http(app(f.clone(), false), "/api/mailboxes", None).0, 401);
        assert_eq!(http(app(f.clone(), true), "/api/mailboxes", None).0, 403);
        let req = Request::builder()
            .uri("/api/mailboxes")
            .header("host", "desk.test")
            .header("origin", "https://other.test")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            run(owner_app(f.clone()).oneshot(req)).unwrap().status(),
            StatusCode::FORBIDDEN
        );
        assert!(f.calls().is_empty());
    }

    #[test]
    fn mailbox_index_pages_counts_and_joins_only_reported_membership() {
        let f = Fake::setup();
        f.reply("inbox_stats", json!({"mailboxes":[
            {"mailbox":"old lane","destination":"old lane","unread":1,"total":4,"body":"secret","subject":"private"},
            {"mailbox":"alpha (%2)","destination":"Alpha","unread":2,"total":7,"archived":1,"undeliverable":0,"oldest_unread_s":5},
            {"mailbox":"%3","unread":0,"total":1}
        ]}));
        f.reply("roster", json!({"lanes":[
            {"pane_id":"%2","name":"alpha","harness":"codex","membership":{"scope":"project","project":"arbitrary-project"},"body":"never forward"},
            {"pane_id":"%3","name":"other","membership":{"scope":"global","project":null}}
        ]}));
        let (status, first) = http(owner_app(f.clone()), "/api/mailboxes?limit=2", None);
        assert_eq!(status, 200);
        assert_eq!(first["total"], 3);
        assert_eq!(first["mailboxes"].as_array().unwrap().len(), 2);
        assert_eq!(first["mailboxes"][0]["mailbox"], "%3");
        assert_eq!(
            first["mailboxes"][1]["lanes"][0]["membership"]["project"],
            "arbitrary-project"
        );
        assert_eq!(first["mailboxes"][1]["unread"], 2);
        assert_eq!(first["next_after"], "alpha (%2)");
        let (_, last) = http(
            owner_app(f.clone()),
            "/api/mailboxes?limit=2&after=alpha%20%28%252%29",
            None,
        );
        assert_eq!(last["mailboxes"].as_array().unwrap().len(), 1);
        assert_eq!(last["mailboxes"][0]["mailbox"], "old lane");
        assert_eq!(last["mailboxes"][0]["mapping"], "unmapped");
        assert!(last["next_after"].is_null());
        assert!(!last.to_string().contains("secret"));
        assert!(last["mailboxes"][0].get("body").is_none());
        assert!(last["mailboxes"][0].get("subject").is_none());
        assert!(first["mailboxes"][1]["lanes"][0].get("body").is_none());
        assert_eq!(
            f.calls(),
            ["inbox_stats", "roster", "inbox_stats", "roster"]
        );
    }

    #[test]
    fn mailbox_index_handles_invalid_pages_and_unavailable_sources_without_guessing() {
        let f = Fake::setup();
        for path in [
            "/api/mailboxes?limit=0",
            "/api/mailboxes?limit=101",
            "/api/mailboxes?limit=x",
            "/api/mailboxes?after=%0A",
            "/api/mailboxes?after=",
        ] {
            assert_eq!(http(owner_app(f.clone()), path, None).0, 400);
        }
        assert!(f.calls().is_empty());
        assert_eq!(http(owner_app(f.clone()), "/api/mailboxes", None).0, 502);
        f.reply(
            "inbox_stats",
            json!({"mailboxes":[{"mailbox":"owner","unread":3,"total":5}]}),
        );
        let (status, result) = http(owner_app(f.clone()), "/api/mailboxes", None);
        assert_eq!(status, 200);
        assert_eq!(result["roster_state"], "unavailable");
        assert_eq!(result["mailboxes"][0]["mapping"], "unavailable");
        assert_eq!(result["mailboxes"][0]["lanes"], json!([]));
        assert_eq!(result["mailboxes"][0]["unread"], 3);
    }

    #[test]
    fn mailbox_index_is_admitted_by_the_production_decoded_path_layer() {
        let f = Fake::setup();
        let service = crate::web_server::decoded_path_routing(owner_app(f), false);
        let req = Request::builder()
            .uri("/api/mailboxes")
            .body(Body::empty())
            .unwrap();
        // A mounted directory with no daemon responds 502. A static-path fallback is 404.
        assert_eq!(
            run(service.oneshot(req)).unwrap().status(),
            StatusCode::BAD_GATEWAY
        );
    }

    fn inbox(f: Arc<Fake>, body: &str) -> (u16, Value) {
        http(app(f, true), "/api/pane-inbox", Some(body))
    }
    /// t4644: the owner's quick message is stored as an inbox message through the daemon's own
    /// inbox send; sign-in and validation come first, a dead or unknown lane gets nothing, and
    /// the answer is the daemon's.
    #[test]
    fn pane_inbox_stores_one_inbox_message_for_a_live_lane() {
        for body in ["{", r#"{"pane":"%9","text":"hi"}"#] {
            let f = Fake::setup();
            assert_eq!(
                http(app(f.clone(), false), "/api/pane-inbox", Some(body)),
                (401, json!({"detail":"Sign in required"}))
            );
            assert!(f.calls().is_empty());
        }
        for (body, error) in [
            (
                json!([1]).to_string(),
                "send a JSON object with pane and text",
            ),
            ("{".to_string(), "send a JSON object with pane and text"),
            (
                json!({"pane":"self","text":"hi"}).to_string(),
                "a tmux pane id looks like %9",
            ),
            (
                json!({"pane":"%9 x","text":"hi"}).to_string(),
                "a tmux pane id looks like %9",
            ),
            (
                json!({"pane":"ds-fable-writer","text":"hi"}).to_string(),
                "a tmux pane id looks like %9",
            ),
            (
                json!({"pane":"%9","text":7}).to_string(),
                "text must be a string",
            ),
            (
                json!({"pane":"%9","text":" "}).to_string(),
                "nothing to send",
            ),
            (
                json!({"pane":"%9","text":"x".repeat(16001)}).to_string(),
                "keep it under 16000 characters",
            ),
        ] {
            let f = Fake::setup();
            assert_eq!(inbox(f.clone(), &body), (400, json!({"detail":error})));
            assert!(f.calls().is_empty());
        }
        let f = Fake::setup();
        let huge = json!({"pane":"%9","text":"x".repeat(PANE_INBOX_BODY_LIMIT)}).to_string();
        assert_eq!(inbox(f.clone(), &huge).0, 413);
        assert!(f.calls().is_empty());
        for (cap, status) in [(json!({}), 502), (json!({"alive":false}), 409)] {
            let f = Fake::setup();
            f.reply("delivery_capability", cap);
            assert_eq!(inbox(f.clone(), r#"{"pane":"%9","text":"hi"}"#).0, status);
            assert_eq!(f.calls(), vec!["delivery_capability"]);
        }
        let f = Fake::setup();
        // A lane with no typing route is still a valid inbox target.
        f.reply(
            "delivery_capability",
            json!({"alive":true,"send":"unknown"}),
        );
        f.reply(
            "inbox_send",
            json!({"message_id":"i77","state":"queued","mailbox":"ds-fable-writer","notified":true}),
        );
        let (status, value) = inbox(f.clone(), r#"{"pane":"%9","text":" hello\nthere "}"#);
        assert_eq!(status, 200);
        assert_eq!(
            value,
            json!({"ok":true,"pane":"%9","route":"inbox","outcome":"stored","message_id":"i77",
                "mailbox":"ds-fable-writer","state":"queued","notified":true,"redirected":null,
                "note":"Stored in the lane's inbox. It has not been read until the lane opens its mail."})
        );
        assert_eq!(f.calls(), vec!["delivery_capability", "inbox_send"]);
        assert_eq!(
            f.calls.lock().unwrap()[1].1,
            json!({"to":"%9","body":" hello\nthere ","notify":true})
        );
        // No id, or a refusal outcome: the desk does not claim it was stored.
        for reply in [
            json!({"state":"queued"}),
            json!({"message_id":"i78","outcome":"undeliverable","reason":"no"}),
        ] {
            let f = Fake::setup();
            f.reply("delivery_capability", json!({"alive":true}));
            f.reply("inbox_send", reply);
            assert_eq!(inbox(f, r#"{"pane":"%9","text":"hi"}"#).0, 502);
        }
    }
    #[test]
    fn pane_send_auth_precedes_json_and_backend() {
        for body in ["{", r#"{"pane":"%9","text":"hi"}"#] {
            let f = Fake::setup();
            assert_eq!(
                http(app(f.clone(), false), "/api/pane-send", Some(body)),
                (401, json!({"detail":"Sign in required"}))
            );
            assert!(f.calls().is_empty());
        }
    }
    #[test]
    fn pane_send_direct_validation_precedes_capability() {
        for (body, error) in [
            (
                json!({"pane":"x","text":"hi"}),
                "a tmux pane id looks like %9",
            ),
            (json!({"pane":"%9","text":" "}), "nothing to send"),
            (
                json!({"pane":"%9","text":"x".repeat(16001)}),
                "keep it under 16000 characters",
            ),
        ] {
            let f = Fake::setup();
            assert_eq!(
                send(f.clone(), &body.to_string()),
                (400, json!({"detail":error}))
            );
            assert!(f.calls().is_empty());
        }
    }
    #[test]
    fn pane_send_capability_refusal_and_route_choice() {
        for (cap, status) in [
            (json!({}), 502),
            (json!({"alive":false}), 409),
            (json!({"alive":true,"send":"unknown"}), 409),
        ] {
            let f = Fake::setup();
            f.reply("delivery_capability", cap);
            assert_eq!(send(f.clone(), r#"{"pane":"%9","text":"hi"}"#).0, status);
            assert_eq!(f.calls(), vec!["delivery_capability"]);
            assert_eq!(f.calls.lock().unwrap()[0].1, json!({"target":"%9"}));
        }
    }
    #[test]
    fn pane_send_owner_push_outcome_is_not_delivery() {
        let f = Fake::setup();
        f.reply(
            "delivery_capability",
            json!({"alive":true,"send":"owner_push"}),
        );
        f.reply("owner_push", json!({"outcome":" Stored ","id":5}));
        let (s, v) = send(f.clone(), r#"{"pane":"%9","text":"hi"}"#);
        assert_eq!(s, 200);
        assert_eq!(v["outcome"], "stored");
        assert_eq!(v["push_id"], 5);
        assert!(v["note"].as_str().unwrap().contains("not 'read'"));
        assert_eq!(f.calls(), vec!["delivery_capability", "owner_push"]);
        f.reply("owner_push", json!({}));
        assert_eq!(send(f, r#"{"pane":"%9","text":"hi"}"#).0, 502);
    }
    #[test]
    fn pane_send_guarded_outcome_table() {
        for (outcome, status, ok) in [
            ("refused", 200, false),
            ("", 409, false),
            ("queued", 200, true),
            ("unconfirmed", 200, true),
            ("stored", 200, true),
            ("ok", 200, true),
            ("sent", 200, true),
            ("weird", 502, false),
        ] {
            let f = Fake::setup();
            f.reply(
                "delivery_capability",
                json!({"alive":true,"send":"guarded"}),
            );
            f.reply("send_text", json!({"outcome":outcome,"message_id":8}));
            let (s, v) = send(f.clone(), r#"{"pane":"%9","text":"hi"}"#);
            assert_eq!(s, status, "{outcome}");
            if s == 200 {
                assert_eq!(v["ok"], ok);
                assert_eq!(v["outcome"], outcome);
                assert_eq!(v["message_id"], 8);
            }
            assert_eq!(f.calls(), vec!["delivery_capability", "send_text"]);
        }
    }
    #[test]
    fn pane_send_channel_revalidation_and_message_model() {
        let f = Fake::setup();
        f.reply("list_panes", json!({"panes":[{"pane_id":"%9"}]}));
        f.channels
            .lock()
            .unwrap()
            .insert("owner".into(), json!({"pane":"%9"}));
        assert_eq!(
            send(
                f.clone(),
                r#"{"channel":"unknown","pane":"%9","text":"hi"}"#
            )
            .0,
            404
        );
        assert_eq!(
            send(f.clone(), r#"{"channel":"owner","pane":"%8","text":""}"#).0,
            409
        );
        assert_eq!(
            send(f.clone(), r#"{"channel":"owner","pane":"%9","text":""}"#).0,
            422
        );
        assert_eq!(
            send(f.clone(), r#"{"channel":"owner","pane":"%9","text":12}"#).0,
            500
        );
        assert!(!f.calls().contains(&"post_direct".into()));
        assert_eq!(
            send(
                f.clone(),
                r#"{"channel":"owner","pane":"%9","text":"hello"}"#
            )
            .0,
            200
        );
        assert_eq!(
            f.calls.lock().unwrap().last().unwrap().1,
            json!({"channel":"owner","body":"hello","attachments":[],"api":true})
        );
    }
    #[test]
    fn fleet_roster_unavailable_schema_and_cache() {
        for listing in [None, Some(json!({})), Some(json!({"panes":[{}]}))] {
            let f = Fake::setup();
            if let Some(v) = listing {
                f.reply("list_panes", v)
            }
            let a = app(f.clone(), true);
            let (s, v) = http(a.clone(), "/api/fleet", None);
            assert_eq!(s, 200);
            assert_eq!(v["state"], "unavailable");
            assert_eq!(v["count"], Value::Null);
            assert_eq!(v["rows"], json!([]));
            assert_eq!(http(a, "/api/fleet", None).1, v);
            assert_eq!(f.calls(), vec!["list_panes"]);
        }
    }
    /// t3335: one lane, one unread count, whichever keys the roster filed it under.
    #[test]
    fn lane_unread_is_counted_once_by_pane_id_then_by_name() {
        let both: HashMap<String, i64> = [("lane".to_string(), 3), ("%1".to_string(), 3)].into();
        assert_eq!(lane_unread(&both, "%1", "lane"), Some(3));
        // The row name is the pane id itself (no registry name, no label).
        assert_eq!(lane_unread(&both, "%1", "%1"), Some(3));
        let name_only: HashMap<String, i64> = [("lane".to_string(), 4)].into();
        assert_eq!(lane_unread(&name_only, "%1", "lane"), Some(4));
        // A lane the roster has no count for is unknown, not zero.
        assert_eq!(lane_unread(&name_only, "%2", "other"), None);
        assert_eq!(lane_unread(&HashMap::new(), "%1", "lane"), None);
        // A stale name entry never adds to the pane's own count.
        let differ: HashMap<String, i64> = [("lane".to_string(), 9), ("%1".to_string(), 2)].into();
        assert_eq!(lane_unread(&differ, "%1", "lane"), Some(2));
    }
    #[test]
    fn fleet_rows_filter_sort_and_activity_precedence() {
        let f = Fake::setup();
        f.reply("list_panes",json!({"panes":[{"pane_id":"%2","agent":"pi","cwd":"/b/"},{"pane_id":"%1","agent":"claude","cwd":"/a","label":"label"},{"pane_id":"%3","agent":""}]}));
        f.reply(
            "state_latest",
            json!({"rows":[{"pane_id":"%1","at":10,"session_id":"sid"}]}),
        );
        f.reply(
            "roster",
            json!({"lanes":[{"name":"lane","pane_id":"%1","inbox":{"unread":3}}]}),
        );
        f.reply("owner_stats", json!({"pending":[]}));
        f.reply(
            "delivery_capability",
            json!({"send":"guarded","drains":false}),
        );
        *f.registry.lock().unwrap() = json!({"%1":{"mailbox":"lane"}});
        f.turns.lock().unwrap().insert("sid".into(), 95.);
        let (s, v) = http(app(f, true), "/api/fleet", None);
        assert_eq!(s, 200);
        assert_eq!(v["count"], 2);
        let r = &v["rows"][0];
        assert_eq!(r["pane_id"], "%1");
        assert_eq!(r["name"], "lane");
        assert_eq!(r["project"], "a");
        assert_eq!(r["last_activity_s"], 5);
        // t3335: the roster's 3 unread, once. It was 6 while name and pane id were both added.
        assert_eq!(r["unread"], 3);
        assert_eq!(r["pending"], 0);
        assert_eq!(r["can_type"], true);
    }
    #[test]
    fn fleet_partial_sources_remain_unknown() {
        let f = Fake::setup();
        *f.local_failure.lock().unwrap() = true;
        f.reply(
            "list_panes",
            json!({"panes":[{"pane_id":"%1","agent":"pi"}]}),
        );
        let a = app(f.clone(), true);
        let (_, v) = http(a.clone(), "/api/fleet", None);
        let r = &v["rows"][0];
        assert_eq!(r["pending"], Value::Null);
        assert_eq!(r["pending_state"], "unknown");
        assert_eq!(r["last_activity_s"], Value::Null);
        assert_eq!(r["send"], Value::Null);
        assert_eq!(r["can_type"], false);
        assert_eq!(v["unrouted"]["state"], "unavailable");
        f.reply("delivery_capability", json!({"send":"guarded"}));
        *f.now.lock().unwrap() = 104.;
        http(a.clone(), "/api/fleet", None);
        let count = f
            .calls()
            .iter()
            .filter(|c| *c == "delivery_capability")
            .count();
        *f.now.lock().unwrap() = 108.;
        http(a.clone(), "/api/fleet", None);
        assert_eq!(
            f.calls()
                .iter()
                .filter(|c| *c == "delivery_capability")
                .count(),
            count
        );
        *f.now.lock().unwrap() = 125.;
        http(a, "/api/fleet", None);
        assert_eq!(
            f.calls()
                .iter()
                .filter(|c| *c == "delivery_capability")
                .count(),
            count + 1
        );
    }
    #[test]
    fn operations_metrics_auth_summary_and_database_errors() {
        let f = Fake::setup();
        assert_eq!(http(app(f.clone(), false), "/api/ops-metrics", None).0, 401);
        assert!(f.calls().is_empty());
        assert_eq!(
            http(app(f.clone(), true), "/api/ops-metrics", None),
            (200, json!({"summary":"dashboard"}))
        );
        for (class, status) in [("OperationalError", 503), ("OSError", 503), ("other", 500)] {
            *f.summary_error.lock().unwrap() = Some(class.into());
            let (s, v) = http(app(f.clone(), true), "/api/ops-metrics", None);
            assert_eq!(s, status);
            if status == 503 {
                assert_eq!(
                    v,
                    json!({"detail":format!("Operations metrics unavailable: {class}")})
                );
            }
        }
    }
    // Generated by executing web/app.py::_send_to_pane in an AST-isolated fake
    // daemon environment; no live application imports, DB, or pane access.
    #[test]
    fn pane_send_python_source_outcome_matrix() {
        let cases: Value = serde_json::from_str(r###"[{"route": "owner_push", "payload": {"outcome": "refused", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "owner_push", "outcome": "refused", "push_id": 5, "detail": "refused", "note": "Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it."}}, {"route": "owner_push", "payload": {"outcome": "", "message_id": 8, "id": 5}, "status": 502, "expected": {"detail": "the daemon stored the message but returned no outcome field, so the desk will not invent one: "}}, {"route": "owner_push", "payload": {"outcome": "queued", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "owner_push", "outcome": "queued", "push_id": 5, "detail": "queued", "note": "Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it."}}, {"route": "owner_push", "payload": {"outcome": "unconfirmed", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "owner_push", "outcome": "unconfirmed", "push_id": 5, "detail": "unconfirmed", "note": "Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it."}}, {"route": "owner_push", "payload": {"outcome": "stored", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "owner_push", "outcome": "stored", "push_id": 5, "detail": "stored", "note": "Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it."}}, {"route": "owner_push", "payload": {"outcome": "ok", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "owner_push", "outcome": "ok", "push_id": 5, "detail": "ok", "note": "Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it."}}, {"route": "owner_push", "payload": {"outcome": "sent", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "owner_push", "outcome": "sent", "push_id": 5, "detail": "sent", "note": "Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it."}}, {"route": "owner_push", "payload": {"outcome": "weird", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "owner_push", "outcome": "weird", "push_id": 5, "detail": "weird", "note": "Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it."}}, {"route": "owner_push", "payload": null, "status": 502, "expected": {"detail": "the daemon stored the message but returned no outcome field, so the desk will not invent one: "}}, {"route": "owner_push", "payload": {"message": " prose only "}, "status": 502, "expected": {"detail": "the daemon stored the message but returned no outcome field, so the desk will not invent one: prose only"}}, {"route": "guarded", "payload": {"outcome": "refused", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": false, "pane": "%9", "route": "guarded", "outcome": "refused", "message_id": 8, "detail": "refused"}}, {"route": "guarded", "payload": {"outcome": "", "message_id": 8, "id": 5}, "status": 409, "expected": {"detail": "No daemon outcome"}}, {"route": "guarded", "payload": {"outcome": "queued", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "guarded", "outcome": "queued", "queued": true, "message_id": 8, "detail": "queued", "note": "Held by the typing guard or not yet delivered. It has NOT arrived - do not read this as delivered."}}, {"route": "guarded", "payload": {"outcome": "unconfirmed", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "guarded", "outcome": "unconfirmed", "unconfirmed": true, "message_id": 8, "detail": "unconfirmed", "note": "Neither witness fired. It may well have arrived - do NOT resend blindly; ask the lane first."}}, {"route": "guarded", "payload": {"outcome": "stored", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "guarded", "outcome": "stored", "message_id": 8, "detail": "stored", "note": "Stored in a durable queue the lane drains itself. That is NOT 'read' - it has not arrived until the lane takes it."}}, {"route": "guarded", "payload": {"outcome": "ok", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "guarded", "outcome": "ok", "message_id": 8, "detail": "ok", "note": "Confirmed by the box clearing or by the turn it started - that is delivery to the lane, not proof the agent acted on it."}}, {"route": "guarded", "payload": {"outcome": "sent", "message_id": 8, "id": 5}, "status": 200, "expected": {"ok": true, "pane": "%9", "route": "guarded", "outcome": "sent", "message_id": 8, "detail": "sent", "note": "Confirmed by the box clearing or by the turn it started - that is delivery to the lane, not proof the agent acted on it."}}, {"route": "guarded", "payload": {"outcome": "weird", "message_id": 8, "id": 5}, "status": 502, "expected": {"detail": "the sender returned no outcome field, so the desk will not guess: weird"}}, {"route": "guarded", "payload": null, "status": 409, "expected": {"detail": "No daemon outcome"}}, {"route": "guarded", "payload": {"message": " prose only "}, "status": 409, "expected": {"detail": "prose only"}}]"###).unwrap();
        for case in cases.as_array().unwrap() {
            let f = Fake::setup();
            let route = case["route"].as_str().unwrap();
            f.reply("delivery_capability", json!({"alive":true,"send":route}));
            f.reply(
                if route == "guarded" {
                    "send_text"
                } else {
                    "owner_push"
                },
                case["payload"].clone(),
            );
            let (status, value) = send(f, r#"{"pane":"%9","text":"hi"}"#);
            assert_eq!(
                u64::from(status),
                case["status"].as_u64().unwrap(),
                "{case}"
            );
            assert_eq!(value, case["expected"], "{case}");
        }
    }
    #[test]
    fn mailbox_summary_route_requires_owner_and_same_origin_before_rpc() {
        let f = Fake::setup();
        assert_eq!(
            http(
                app(f.clone(), false),
                "/api/mailbox-messages?mailbox=owner",
                None
            )
            .0,
            401
        );
        assert_eq!(
            http(
                app(f.clone(), true),
                "/api/mailbox-messages?mailbox=owner",
                None
            )
            .0,
            403
        );
        let request = Request::builder()
            .uri("/api/mailbox-messages?mailbox=owner")
            .header("host", "desk.test")
            .header("origin", "http://elsewhere.test")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            run(owner_app(f.clone()).oneshot(request)).unwrap().status(),
            StatusCode::FORBIDDEN
        );
        for path in [
            "/api/mailbox-messages",
            "/api/mailbox-messages?mailbox=%0A",
            "/api/mailbox-messages?mailbox=a&limit=101",
            "/api/mailbox-messages?mailbox=a&before=",
        ] {
            assert_eq!(http(owner_app(f.clone()), path, None).0, 400);
        }
        assert!(f.calls().is_empty());
    }

    #[test]
    fn mailbox_summary_route_uses_literal_paging_and_typed_body_free_allowlist() {
        let f = Fake::setup();
        f.reply("inbox_list",json!({"mailbox":"self","order":"newest_first","next_before":"i7","body":"top secret","messages":[{
            "message_id":"i8","mailbox":"self","destination":"self","sender":"worker","subject":"Review", "state":"read","created_at":100,
            "body_bytes":42,"transport":"inbox","reply_to":null,"attachment_count":1,"audience":"agent","waiting_s":null,"body":"private body"
        }]}));
        let (status, page) = http(
            owner_app(f.clone()),
            "/api/mailbox-messages?mailbox=self&limit=2&before=i9",
            None,
        );
        assert_eq!(status, 200);
        assert_eq!(page["next_before"], "i7");
        assert_eq!(page["messages"][0]["audience"], "agent");
        assert_eq!(page["messages"][0]["subject"], "Review");
        assert!(page.get("body").is_none());
        assert!(page["messages"][0].get("body").is_none());
        assert_eq!(
            *f.calls.lock().unwrap(),
            vec![(
                "inbox_list".into(),
                json!({"mailbox":"self","limit":2,"before":"i9","newest_first":true,"literal_mailbox":true,"unread_only":false})
            )]
        );
        f.reply(
            "inbox_list",
            json!({"mailbox":"self","order":"newest_first","next_before":null,"messages":[]}),
        );
        assert_eq!(
            http(
                owner_app(f.clone()),
                "/api/mailbox-messages?mailbox=self",
                None
            )
            .1["messages"],
            json!([])
        );
        f.reply(
            "inbox_list",
            json!({"mailbox":"self","messages":[{"subject":{"body":"must refuse"}}]}),
        );
        assert_eq!(
            http(
                owner_app(f.clone()),
                "/api/mailbox-messages?mailbox=self",
                None
            )
            .0,
            502
        );
    }

    #[test]
    fn mailbox_summary_production_path_and_rpc_failure_do_not_fall_to_static() {
        let f = Fake::setup();
        let service = crate::web_server::decoded_path_routing(owner_app(f), false);
        let req = Request::builder()
            .uri("/api/mailbox-messages?mailbox=unknown")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            run(service.oneshot(req)).unwrap().status(),
            StatusCode::BAD_GATEWAY
        );
    }
}
