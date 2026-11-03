//! Owner-facing web routes ported from `web/app.py`.
//!
//! The route layer owns Python-compatible validation and response shaping. A
//! separately injected backend owns daemon RPC, desk roster lookup, and time,
//! so handler tests never contact a live daemon or terminal.

use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};

use axum::{
    body::to_bytes,
    extract::{Extension, Request},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use ds_core::owner::{owner_thread_role, OwnerThreadRole};
use serde_json::{json, Map, Value};

use super::auth::{AuthBackend, AuthError, Identity, RequestScheme};

const OWNER_SEND_LIMIT: usize = 16_000;
const CAPABILITY_TTL_S: f64 = 20.0;
const CAPABILITY_ERROR: &str = "the daemon could not describe this lane";

pub type OwnerWebFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A narrowly scoped async boundary for the owner routes.
///
/// Implementations must perform the named daemon RPCs, resolve the configured
/// Front Desk pane, and provide the current monotonic-ish wall clock. There are
/// deliberately no default implementations: an unconfigured operation must
/// not appear to succeed in either a production adapter or a test fake.
pub trait OwnerWebBackend: Send + Sync + 'static {
    fn rpc<'a>(
        &'a self,
        method: &'a str,
        args: Value,
    ) -> OwnerWebFuture<'a, Result<Value, OwnerWebError>>;

    fn desk_channel(&self) -> Option<Value>;

    fn now_seconds(&self) -> f64;
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnerWebError {
    Http { status: StatusCode, detail: String },
    Failure(String),
}

impl OwnerWebError {
    fn into_response(self) -> Response {
        match self {
            Self::Http { status, detail } => detail_response(status, detail),
            Self::Failure(detail) => detail_response(StatusCode::BAD_GATEWAY, detail),
        }
    }

    fn detail(&self) -> &str {
        match self {
            Self::Http { detail, .. } | Self::Failure(detail) => detail,
        }
    }
}

/// Per-router cache and injected backend. Keep one state instance for the life
/// of a mounted router to preserve the Python 20-second capability cache.
pub struct OwnerWebState {
    backend: Arc<dyn OwnerWebBackend>,
    capability_cache: Mutex<HashMap<String, (f64, Value)>>,
}

impl OwnerWebState {
    pub fn new(backend: Arc<dyn OwnerWebBackend>) -> Self {
        Self {
            backend,
            capability_cache: Mutex::new(HashMap::new()),
        }
    }

    async fn capability(&self, pane: &str) -> Value {
        let now = self.backend.now_seconds();
        if let Some((seen, value)) = self
            .capability_cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(pane)
            .cloned()
        {
            if now - seen < CAPABILITY_TTL_S {
                return value;
            }
        }

        let response = self
            .backend
            .rpc("delivery_capability", json!({"target": pane}))
            .await;
        if let Ok(Value::Object(object)) = response {
            let value = Value::Object(object);
            self.capability_cache
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .insert(pane.to_owned(), (self.backend.now_seconds(), value.clone()));
            value
        } else {
            Value::Object(Map::new())
        }
    }
}

/// Mount the three owner routes with the default HTTP request scheme.
pub fn router() -> Router {
    router_with_scheme(RequestScheme::Http)
}

/// Mount owner routes with the externally visible request scheme used by the
/// existing authentication backend's same-origin rule.
pub fn router_with_scheme(scheme: RequestScheme) -> Router {
    Router::new()
        .route("/api/capability", get(api_capability))
        .route("/api/owner-send", post(api_owner_send))
        .route("/api/owner-thread", get(api_owner_thread))
        .layer(Extension(scheme))
}

async fn api_capability(
    Extension(state): Extension<Arc<OwnerWebState>>,
    request: Request,
) -> Response {
    let query = match parse_query(request.uri().query()) {
        Ok(query) => query,
        Err(()) => return detail_response(StatusCode::BAD_REQUEST, "invalid query".to_owned()),
    };
    let Some(pane) = query.get("pane") else {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": [{
                "type": "missing", "loc": ["query", "pane"], "msg": "Field required"
            }]})),
        )
            .into_response();
    };
    let pane = pane.trim();
    let capability = state.capability(pane).await;
    if !is_truthy(&capability) {
        return detail_response(StatusCode::BAD_GATEWAY, CAPABILITY_ERROR.to_owned());
    }
    Json(capability).into_response()
}

async fn api_owner_send(
    Extension(state): Extension<Arc<OwnerWebState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    };

    let body_bytes = match to_bytes(request.into_body(), usize::MAX).await {
        Ok(body) => body,
        Err(_) => {
            return detail_response(
                StatusCode::BAD_REQUEST,
                "There was an error parsing the body".into(),
            )
        }
    };
    let body: Value = match serde_json::from_slice(&body_bytes) {
        Ok(body) => body,
        Err(_) => {
            return detail_response(
                StatusCode::BAD_REQUEST,
                "There was an error parsing the body".into(),
            )
        }
    };
    let Some(body) = body.as_object() else {
        return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response();
    };

    let text = python_string_or_empty(body.get("text")).trim().to_owned();
    if text.is_empty() {
        return detail_response(StatusCode::BAD_REQUEST, "nothing to send".into());
    }
    if text.chars().count() > OWNER_SEND_LIMIT {
        return detail_response(
            StatusCode::BAD_REQUEST,
            format!("keep it under {OWNER_SEND_LIMIT} characters"),
        );
    }
    if body.get("attachment_ids").is_some_and(is_truthy) {
        return detail_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "attachments are not supported on the owner mailbox yet".into(),
        );
    }

    let external_id = body
        .get("external_id")
        .filter(|value| is_truthy(value))
        .map(python_string)
        .filter(|value| !value.is_empty());
    let ingested = match state
        .backend
        .rpc(
            "owner_ingest",
            json!({"body": text, "transport": "web", "external_id": external_id}),
        )
        .await
    {
        Ok(value) => value,
        Err(OwnerWebError::Http { status, detail }) => return detail_response(status, detail),
        Err(OwnerWebError::Failure(error)) => {
            return detail_response(
                StatusCode::BAD_GATEWAY,
                format!("the owner mailbox refused the message: {error}"),
            )
        }
    };
    let message = ingested
        .get("message")
        .filter(|value| is_truthy(value))
        .and_then(Value::as_object);
    let message_id = message.and_then(|message| message.get("message_id"));
    if !message_id.is_some_and(is_truthy) {
        return detail_response(
            StatusCode::BAD_GATEWAY,
            "the daemon recorded no message id; not guessing".into(),
        );
    }
    let message_id = message_id.expect("checked above").clone();
    let mut result = json!({
        "ok": true,
        "message_id": message_id,
        "outcome": "stored",
        "duplicate": ingested.get("duplicate").is_some_and(is_truthy),
        "note": "Recorded in the owner mailbox. Stored is not read: the desk has not seen it until it answers."
    });

    let pane = match desk_pane(state.backend.as_ref()).await {
        Ok(pane) => pane,
        Err(_) => None,
    };
    let Some(pane) = pane else {
        result["notify"] = json!({
            "state": "desk_offline",
            "detail": "no live Front Desk lane to nudge; the message waits in the mailbox"
        });
        return Json(result).into_response();
    };
    let Some(pane_id) = pane.get("pane_id").and_then(Value::as_str) else {
        result["notify"] = json!({
            "state": "desk_offline",
            "detail": "no live Front Desk lane to nudge; the message waits in the mailbox"
        });
        return Json(result).into_response();
    };

    let prompt = format!(
        "Owner message {} (web): {text}\nAnswer it now: dunkingsheep owner reply {} \"<your answer>\"",
        python_string(&message_id),
        python_string(&message_id)
    );
    let sent = match send_to_pane(state.as_ref(), pane_id, &prompt).await {
        Ok(sent) => sent,
        Err(error) => {
            let direct_detail = error.detail().chars().take(200).collect::<String>();
            return nudge_or_report(state.as_ref(), result, pane_id, &direct_detail, &text).await;
        }
    };
    let outcome = sent
        .get("outcome")
        .filter(|value| is_truthy(value))
        .map(python_string)
        .unwrap_or_default()
        .to_lowercase();
    if sent.get("ok").is_some_and(is_truthy) && outcome == "ok" {
        result["notify"] = json!({
            "route": sent.get("route").cloned().unwrap_or(Value::Null),
            "pane": pane_id,
            "outcome": outcome,
            "detail": sent.get("detail").cloned().unwrap_or(json!("")),
            "state": "sent"
        });
        return Json(result).into_response();
    }
    if sent.get("ok").is_some_and(is_truthy) && matches!(outcome.as_str(), "queued" | "unconfirmed")
    {
        result["notify"] = json!({
            "route": sent.get("route").cloned().unwrap_or(Value::Null),
            "pane": pane_id,
            "outcome": outcome,
            "detail": sent.get("detail").cloned().unwrap_or(json!("")),
            "note": sent.get("note").cloned().unwrap_or(json!("")),
            "state": outcome
        });
        return Json(result).into_response();
    }
    let detail = sent
        .get("detail")
        .filter(|value| is_truthy(value))
        .map(python_string)
        .unwrap_or_default();
    let direct_detail = format!(
        "{}: {detail}",
        if outcome.is_empty() {
            "no outcome"
        } else {
            &outcome
        }
    )
    .chars()
    .take(200)
    .collect::<String>();
    nudge_or_report(state.as_ref(), result, pane_id, &direct_detail, &text).await
}

async fn api_owner_thread(
    Extension(state): Extension<Arc<OwnerWebState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let query = match parse_query(request.uri().query()) {
        Ok(query) => query,
        Err(()) => return detail_response(StatusCode::BAD_REQUEST, "invalid query".to_owned()),
    };
    let limit = match parse_thread_limit(query.get("limit").map(String::as_str)) {
        Ok(limit) => limit,
        Err(response) => return response,
    };
    match owner_thread(state.as_ref(), limit).await {
        Ok(result) => Json(result).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn authenticate(
    backend: &dyn AuthBackend,
    headers: &HeaderMap,
    scheme: RequestScheme,
) -> Result<Identity, Response> {
    if let Some(response) = same_origin_refusal(headers, scheme) {
        return Err(response);
    }
    let bearer = bearer_value(headers);
    let cookie = cookie_value(headers, "vp_session");
    backend
        .identity(bearer.as_deref(), cookie.as_deref())
        .await
        .map_err(auth_error_response)
}

fn same_origin_refusal(headers: &HeaderMap, scheme: RequestScheme) -> Option<Response> {
    let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let scheme = match scheme {
        RequestScheme::Http => "http",
        RequestScheme::Https => "https",
    };
    (origin != format!("{scheme}://{host}"))
        .then(|| detail_response(StatusCode::FORBIDDEN, "Cross-origin request refused".into()))
}

fn bearer_value(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key == name).then(|| value.to_owned())
        })
}

fn auth_error_response(error: AuthError) -> Response {
    match error {
        AuthError::TooManyAttempts => detail_response(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many attempts; retry in five minutes".into(),
        ),
        AuthError::IncorrectPassword => {
            detail_response(StatusCode::UNAUTHORIZED, "Incorrect password".into())
        }
        AuthError::SignInRequired => {
            detail_response(StatusCode::UNAUTHORIZED, "Sign in required".into())
        }
        AuthError::Internal(_) => {
            (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
        }
    }
}

fn detail_response(status: StatusCode, detail: String) -> Response {
    (status, Json(json!({"detail": detail}))).into_response()
}

fn parse_query(raw: Option<&str>) -> Result<HashMap<String, String>, ()> {
    fn decode(component: &str) -> Result<String, ()> {
        let bytes = component.as_bytes();
        let mut decoded = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'+' => {
                    decoded.push(b' ');
                    index += 1;
                }
                b'%' => {
                    let high = bytes
                        .get(index + 1)
                        .and_then(|byte| hex_value(*byte))
                        .ok_or(())?;
                    let low = bytes
                        .get(index + 2)
                        .and_then(|byte| hex_value(*byte))
                        .ok_or(())?;
                    decoded.push((high << 4) | low);
                    index += 3;
                }
                byte => {
                    decoded.push(byte);
                    index += 1;
                }
            }
        }
        String::from_utf8(decoded).map_err(|_| ())
    }

    fn hex_value(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let mut query = HashMap::new();
    for pair in raw
        .unwrap_or_default()
        .split('&')
        .filter(|pair| !pair.is_empty())
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(decode(key)?, decode(value)?);
    }
    Ok(query)
}

fn parse_thread_limit(raw: Option<&str>) -> Result<i64, Response> {
    let Some(raw) = raw else { return Ok(50) };
    let parsed = raw.trim().parse::<i128>();
    match parsed {
        Ok(limit) => Ok(limit.clamp(1, 200) as i64),
        Err(_) => Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": [{
                "type": "int_parsing",
                "loc": ["query", "limit"],
                "msg": "Input should be a valid integer, unable to parse string as an integer",
                "input": raw
            }]})),
        )
            .into_response()),
    }
}

async fn owner_thread(state: &OwnerWebState, limit: i64) -> Result<Value, OwnerWebError> {
    let data = match state
        .backend
        .rpc("owner_conversation", json!({"limit": limit}))
        .await
    {
        Ok(data) => data,
        Err(error @ OwnerWebError::Http { .. }) => return Err(error),
        Err(OwnerWebError::Failure(error)) => {
            return Err(OwnerWebError::Http {
                status: StatusCode::BAD_GATEWAY,
                detail: format!("the owner mailbox is unavailable: {error}"),
            })
        }
    };
    let messages = data
        .get("messages")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().map(owner_thread_item).collect::<Vec<_>>())
        .unwrap_or_default();
    let waiting = data
        .get("waiting")
        .cloned()
        .unwrap_or_else(|| Value::Array(Vec::new()));
    Ok(json!({"items": messages, "waiting": waiting, "source": "owner_conversation"}))
}

fn owner_thread_item(row: &Value) -> Value {
    let sender = row.get("sender").cloned().unwrap_or(Value::Null);
    let transport = row.get("transport").cloned().unwrap_or(Value::Null);
    let role = match owner_thread_role(sender.as_str(), transport.as_str()) {
        OwnerThreadRole::Owner => "owner",
        OwnerThreadRole::Agent => "agent",
        OwnerThreadRole::Unknown => "unknown",
    };
    let row_id = row.get("id").cloned().unwrap_or(Value::Null);
    let message_id = python_string(&row_id);
    let digits = message_id.trim_start_matches('i');
    let id = if !digits.is_empty() && digits.chars().all(|character| character.is_ascii_digit()) {
        digits.parse::<i64>().unwrap_or(0)
    } else {
        0
    };
    let body = row
        .get("body")
        .filter(|value| is_truthy(value))
        .cloned()
        .unwrap_or_else(|| json!(""));
    let created_at = row
        .get("created_at")
        .filter(|value| is_truthy(value))
        .map(value_as_python_float)
        .unwrap_or(0.0);
    let mut item = json!({
        "id": id,
        "message_id": row_id,
        "role": role,
        "sender": sender,
        "body": body,
        "created_at": utc_stamp(created_at),
        "transport": transport,
        "reply_to": row.get("reply_to").cloned().unwrap_or(Value::Null),
        "attachments": []
    });
    if role == "owner" {
        let answered = row.get("answered").is_some_and(is_truthy);
        item["delivery"] = json!(if answered { "acked" } else { "stored" });
        item["outcome"] = json!(if answered {
            "answered"
        } else {
            "stored in the owner mailbox · waiting for the desk"
        });
    }
    item
}

fn value_as_python_float(value: &Value) -> f64 {
    match value {
        Value::Number(number) => number.to_string().parse::<f64>().unwrap_or(0.0),
        Value::String(text) => text.parse::<f64>().unwrap_or(0.0),
        Value::Bool(flag) => f64::from(u8::from(*flag)),
        _ => 0.0,
    }
}

fn utc_stamp(timestamp: f64) -> String {
    let seconds = timestamp.floor().clamp(i64::MIN as f64, i64::MAX as f64) as i64;
    let days = seconds.div_euclid(86_400);
    let day_seconds = seconds.rem_euclid(86_400);
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 }.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += if month <= 2 { 1 } else { 0 };
    let hour = day_seconds / 3_600;
    let minute = (day_seconds % 3_600) / 60;
    let second = day_seconds % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

async fn desk_pane(backend: &dyn OwnerWebBackend) -> Result<Option<Value>, OwnerWebError> {
    let listing = backend.rpc("list_panes", json!({})).await?;
    let Some(panes) = listing.get("panes").and_then(Value::as_array) else {
        return Err(OwnerWebError::Failure(
            "pane roster response is missing its panes list".into(),
        ));
    };
    let config = backend
        .desk_channel()
        .filter(is_truthy)
        .unwrap_or_else(|| json!({"id": "cos", "match": "desk"}));
    Ok(match_pane(&config, panes))
}

fn match_pane(config: &Value, panes: &[Value]) -> Option<Value> {
    if let Some(pinned) = config.get("pane").filter(|value| is_truthy(value)) {
        return panes
            .iter()
            .find(|pane| pane.get("pane_id") == Some(pinned))
            .cloned();
    }
    let needle = config.get("match")?.as_str()?.to_lowercase();
    let is_exact = |pane: &Value| {
        let tab = pane
            .get("tab_label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let cwd = pane
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_lowercase();
        let cwd = cwd.trim_end_matches('/');
        tab == needle || cwd == needle || cwd.ends_with(&format!("/{needle}"))
    };
    if let Some(pane) = panes.iter().find(|pane| is_exact(pane)) {
        return Some(pane.clone());
    }
    panes
        .iter()
        .find(|pane| {
            pane.get("tab_label")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_lowercase()
                .contains(&needle)
                || pane
                    .get("workspace_label")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_lowercase()
                    .contains(&needle)
        })
        .cloned()
}

async fn send_to_pane(
    state: &OwnerWebState,
    pane: &str,
    text: &str,
) -> Result<Value, OwnerWebError> {
    if !pane.starts_with('%') {
        return Err(OwnerWebError::Http {
            status: StatusCode::BAD_REQUEST,
            detail: "a tmux pane id looks like %9".into(),
        });
    }
    if text.is_empty() {
        return Err(OwnerWebError::Http {
            status: StatusCode::BAD_REQUEST,
            detail: "nothing to send".into(),
        });
    }
    if text.chars().count() > OWNER_SEND_LIMIT {
        return Err(OwnerWebError::Http {
            status: StatusCode::BAD_REQUEST,
            detail: "keep it under 16000 characters".into(),
        });
    }

    let capability = state.capability(pane).await;
    if !is_truthy(&capability) {
        return Err(OwnerWebError::Http {
            status: StatusCode::BAD_GATEWAY,
            detail: CAPABILITY_ERROR.into(),
        });
    }
    if !capability.get("alive").is_some_and(is_truthy) {
        return Err(OwnerWebError::Http {
            status: StatusCode::CONFLICT,
            detail: format!("{pane} is not alive: nothing to send to"),
        });
    }
    let route = capability.get("send").cloned().unwrap_or(Value::Null);
    let route_name = route.as_str();
    if !matches!(route_name, Some("owner_push" | "guarded")) {
        return Err(OwnerWebError::Http {
            status: StatusCode::CONFLICT,
            detail: format!(
                "{pane} declares no send route ({}); not guessing",
                python_repr(&route)
            ),
        });
    }
    let (method, args) = if route_name == Some("owner_push") {
        ("owner_push", json!({"target": pane, "body": text}))
    } else {
        ("send_text", json!({"target": pane, "text": text}))
    };
    let response = state
        .backend
        .rpc(method, args)
        .await
        .map_err(|error| match error {
            OwnerWebError::Http { .. } => error,
            OwnerWebError::Failure(detail) => OwnerWebError::Http {
                status: StatusCode::BAD_GATEWAY,
                detail: format!("delivery path unavailable: {detail}"),
            },
        })?;
    let payload = if response.is_object() {
        response
    } else {
        Value::Object(Map::new())
    };
    let detail = payload
        .get("message")
        .filter(|value| is_truthy(value))
        .or_else(|| payload.get("outcome").filter(|value| is_truthy(value)))
        .map(python_string)
        .unwrap_or_default()
        .trim()
        .chars()
        .take(400)
        .collect::<String>();

    if route_name == Some("owner_push") {
        let outcome = payload
            .get("outcome")
            .filter(|value| is_truthy(value))
            .map(python_string)
            .unwrap_or_default()
            .trim()
            .to_lowercase();
        if outcome.is_empty() {
            return Err(OwnerWebError::Http {
                status: StatusCode::BAD_GATEWAY,
                detail: format!("the daemon stored the message but returned no outcome field, so the desk will not invent one: {}", detail.chars().take(160).collect::<String>()),
            });
        }
        return Ok(json!({
            "ok": true,
            "pane": pane,
            "route": route,
            "outcome": outcome,
            "push_id": payload.get("id").cloned().unwrap_or(Value::Null),
            "detail": detail,
            "note": "Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it."
        }));
    }

    let outcome = payload
        .get("outcome")
        .filter(|value| is_truthy(value))
        .map(python_string)
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let message_id = payload.get("message_id").cloned().unwrap_or(Value::Null);
    if outcome == "refused" {
        return Ok(json!({
            "ok": false,
            "pane": pane,
            "route": route,
            "outcome": outcome,
            "message_id": message_id,
            "detail": payload.get("message").filter(|value| is_truthy(value)).cloned().unwrap_or(json!(detail))
        }));
    }
    if outcome.is_empty() {
        return Err(OwnerWebError::Http {
            status: StatusCode::CONFLICT,
            detail: if detail.is_empty() {
                "No daemon outcome".into()
            } else {
                detail
            },
        });
    }
    if outcome == "queued" {
        return Ok(json!({
            "ok": true, "pane": pane, "route": route, "outcome": "queued",
            "queued": true, "message_id": message_id, "detail": detail,
            "note": "Held by the typing guard or not yet delivered. It has NOT arrived - do not read this as delivered."
        }));
    }
    if outcome == "unconfirmed" {
        return Ok(json!({
            "ok": true, "pane": pane, "route": route, "outcome": "unconfirmed",
            "unconfirmed": true, "message_id": message_id, "detail": detail,
            "note": "Neither witness fired. It may well have arrived - do NOT resend blindly; ask the lane first."
        }));
    }
    if outcome == "stored" {
        return Ok(json!({
            "ok": true, "pane": pane, "route": route, "outcome": "stored",
            "message_id": message_id, "detail": detail,
            "note": "Stored in a durable queue the lane drains itself. That is NOT 'read' - it has not arrived until the lane takes it."
        }));
    }
    if matches!(outcome.as_str(), "ok" | "sent") {
        return Ok(json!({
            "ok": true, "pane": pane, "route": route, "outcome": outcome,
            "message_id": message_id, "detail": detail,
            "note": "Confirmed by the box clearing or by the turn it started - that is delivery to the lane, not proof the agent acted on it."
        }));
    }
    Err(OwnerWebError::Http {
        status: StatusCode::BAD_GATEWAY,
        detail: format!(
            "the sender returned no outcome field, so the desk will not guess: {}",
            detail.chars().take(200).collect::<String>()
        ),
    })
}

async fn nudge_or_report(
    state: &OwnerWebState,
    mut result: Value,
    pane: &str,
    direct_failed: &str,
    text: &str,
) -> Response {
    let body = format!(
        "Owner message {} (web) is waiting in the owner mailbox: `dunkingsheep owner conversation`; answer with `owner reply {}`.\n\n{}",
        python_string(&result["message_id"]),
        python_string(&result["message_id"]),
        text
    );
    match state
        .backend
        .rpc(
            "inbox_send",
            json!({
                "to": pane,
                "notify": true,
                "subject": format!("owner message {}", python_string(&result["message_id"])),
                "body": body
            }),
        )
        .await
    {
        Ok(nudge) => {
            result["notify"] = json!({
                "state": "nudged",
                "pane": pane,
                "inbox_id": nudge.get("message_id").cloned().unwrap_or(Value::Null),
                "inbox_state": nudge.get("state").cloned().unwrap_or(Value::Null),
                "direct_failed": direct_failed
            });
        }
        Err(error) => {
            result["notify"] = json!({
                "state": "nudge_failed",
                "pane": pane,
                "detail": error.detail().chars().take(200).collect::<String>(),
                "direct_failed": direct_failed
            });
        }
    }
    Json(result).into_response()
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value
            .to_string()
            .parse::<f64>()
            .is_ok_and(|value| value != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
    }
}

fn python_string_or_empty(value: Option<&Value>) -> String {
    value
        .filter(|value| is_truthy(value))
        .map(python_string)
        .unwrap_or_default()
}

fn python_string(value: &Value) -> String {
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
                .map(python_repr)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!(
                    "{}: {}",
                    python_repr(&Value::String(key.clone())),
                    python_repr(value)
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn python_repr(value: &Value) -> String {
    match value {
        Value::String(value) => python_repr_string(value),
        Value::Array(_) | Value::Object(_) => python_string(value),
        _ => python_string(value),
    }
}

fn python_repr_string(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut output = String::with_capacity(value.len() + 2);
    output.push(quote);
    for character in value.chars() {
        match character {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            value if value == quote => {
                output.push('\\');
                output.push(value);
            }
            value if value.is_control() => output.push_str(&format!("\\u{:04x}", value as u32)),
            value => output.push(value),
        }
    }
    output.push(quote);
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::auth::AuthFuture;
    use axum::{
        body::{to_bytes, Body},
        http::{header, HeaderValue, Request},
    };
    use serde_json::Value;
    use std::{
        collections::{HashMap, VecDeque},
        sync::Mutex,
        task::{Context, Poll, Waker},
    };
    use tower::ServiceExt;

    struct FakeAuth {
        result: Mutex<Result<Identity, AuthError>>,
        seen: Mutex<Vec<(Option<String>, Option<String>)>>,
    }

    impl FakeAuth {
        fn accepting(identity: Identity) -> Self {
            Self {
                result: Mutex::new(Ok(identity)),
                seen: Mutex::new(Vec::new()),
            }
        }

        fn refusing(error: AuthError) -> Self {
            Self {
                result: Mutex::new(Err(error)),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _address: &'a str,
            _password: &'a str,
        ) -> AuthFuture<'a, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unexpected login".into())) })
        }

        fn identity<'a>(
            &'a self,
            bearer: Option<&'a str>,
            cookie: Option<&'a str>,
        ) -> AuthFuture<'a, Result<Identity, AuthError>> {
            let bearer = bearer.map(str::to_owned);
            let cookie = cookie.map(str::to_owned);
            Box::pin(async move {
                self.seen.lock().unwrap().push((bearer, cookie));
                self.result.lock().unwrap().clone()
            })
        }

        fn logout<'a>(&'a self, _cookie: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unexpected logout".into())) })
        }

        fn change_password<'a>(
            &'a self,
            _current: &'a str,
            _new: &'a str,
            _keep_cookie: Option<&'a str>,
        ) -> AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unexpected password change".into())) })
        }
    }

    struct FakeOwner {
        calls: Mutex<Vec<(String, Value)>>,
        replies: Mutex<HashMap<String, VecDeque<Result<Value, OwnerWebError>>>>,
        channel: Option<Value>,
        now: Mutex<f64>,
    }

    impl FakeOwner {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                replies: Mutex::new(HashMap::new()),
                channel: Some(json!({"id": "cos", "match": "desk"})),
                now: Mutex::new(100.0),
            }
        }

        fn reply(&self, method: &str, value: Value) {
            self.replies
                .lock()
                .unwrap()
                .entry(method.to_owned())
                .or_default()
                .push_back(Ok(value));
        }

        fn replace(&self, method: &str, result: Result<Value, OwnerWebError>) {
            self.replies
                .lock()
                .unwrap()
                .insert(method.to_owned(), VecDeque::from([result]));
        }

        fn methods(&self) -> Vec<String> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .map(|(method, _)| method.clone())
                .collect()
        }
    }

    impl OwnerWebBackend for FakeOwner {
        fn rpc<'a>(
            &'a self,
            method: &'a str,
            args: Value,
        ) -> OwnerWebFuture<'a, Result<Value, OwnerWebError>> {
            let method = method.to_owned();
            Box::pin(async move {
                self.calls.lock().unwrap().push((method.clone(), args));
                self.replies
                    .lock()
                    .unwrap()
                    .get_mut(&method)
                    .and_then(VecDeque::pop_front)
                    .unwrap_or_else(|| {
                        Err(OwnerWebError::Failure(format!(
                            "unexpected daemon RPC {method}"
                        )))
                    })
            })
        }

        fn desk_channel(&self) -> Option<Value> {
            self.channel.clone()
        }

        fn now_seconds(&self) -> f64 {
            *self.now.lock().unwrap()
        }
    }

    fn app(auth: Arc<FakeAuth>, backend: Arc<FakeOwner>) -> Router {
        let auth: Arc<dyn AuthBackend> = auth;
        let state = Arc::new(OwnerWebState::new(backend));
        router().layer(Extension(auth)).layer(Extension(state))
    }

    fn request(method: &str, uri: &str, body: Option<String>) -> Request<Body> {
        let mut builder = Request::builder().method(method).uri(uri);
        if body.is_some() {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        builder
            .body(body.map(Body::from).unwrap_or_else(Body::empty))
            .unwrap()
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker: &Waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = std::pin::pin!(future);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }

    fn response_json(response: Response) -> (StatusCode, Value, HeaderMap) {
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, json, headers)
    }

    fn authorized() -> Arc<FakeAuth> {
        Arc::new(FakeAuth::accepting(Identity::Owner))
    }

    fn set_send_responses(backend: &FakeOwner, outcome: &str) {
        backend.reply(
            "owner_ingest",
            json!({
                "message": {"message_id": "i7"}, "duplicate": false
            }),
        );
        backend.reply(
            "list_panes",
            json!({"panes": [{
                "pane_id": "%133", "tab_label": "sn-desk", "cwd": "/home/desk",
                "agent_status": null
            }]}),
        );
        backend.reply(
            "delivery_capability",
            json!({"alive": true, "send": "guarded"}),
        );
        backend.reply(
            "send_text",
            json!({
                "ok": outcome != "refused", "outcome": outcome, "message_id": "m9",
                "message": if outcome == "refused" { "not sent" } else { "sent" }
            }),
        );
    }

    #[test]
    fn capability_is_public_trimmed_and_cached_for_twenty_seconds() {
        let backend = Arc::new(FakeOwner::new());
        backend.reply(
            "delivery_capability",
            json!({
                "alive": true, "send": "guarded", "pane_id": "%9"
            }),
        );
        let auth = Arc::new(FakeAuth::refusing(AuthError::SignInRequired));
        let router = app(auth.clone(), backend.clone());

        for _ in 0..2 {
            let response = block_on(router.clone().oneshot(request(
                "GET",
                "/api/capability?pane=%20%259%20",
                None,
            )))
            .unwrap();
            let (status, body, headers) = response_json(response);
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["pane_id"], "%9");
            assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        }
        assert_eq!(backend.methods(), ["delivery_capability"]);
        assert!(auth.seen.lock().unwrap().is_empty());
        assert_eq!(backend.calls.lock().unwrap()[0].1["target"], "%9");
    }

    #[test]
    fn capability_failure_is_a_502_and_never_guesses() {
        let backend = Arc::new(FakeOwner::new());
        backend.replace(
            "delivery_capability",
            Err(OwnerWebError::Failure("socket refused".into())),
        );
        let response = block_on(app(authorized(), backend.clone()).oneshot(request(
            "GET",
            "/api/capability?pane=%2519",
            None,
        )))
        .unwrap();
        let (status, body, _) = response_json(response);
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body, json!({"detail": CAPABILITY_ERROR}));
        assert_eq!(backend.methods(), ["delivery_capability"]);
    }

    #[test]
    fn a_web_message_lands_in_the_owner_mailbox_and_is_typed_straight_to_the_desk() {
        let backend = Arc::new(FakeOwner::new());
        set_send_responses(&backend, "ok");
        let auth = Arc::new(FakeAuth::accepting(Identity::Api));
        let mut request = request(
            "POST",
            "/api/owner-send",
            Some(json!({"text": " hello desk ", "external_id": "e4"}).to_string()),
        );
        request.headers_mut().insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer api-token"),
        );
        let response = block_on(app(auth.clone(), backend.clone()).oneshot(request)).unwrap();
        let (status, body, headers) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(body["outcome"], "stored");
        assert_eq!(body["message_id"], "i7");
        assert_eq!(body["notify"]["state"], "sent");
        assert_eq!(body["notify"]["route"], "guarded");
        assert!(!body.to_string().contains("delivered"));
        assert_eq!(
            backend.methods(),
            [
                "owner_ingest",
                "list_panes",
                "delivery_capability",
                "send_text"
            ]
        );
        let calls = backend.calls.lock().unwrap();
        assert_eq!(
            calls[0].1,
            json!({"body": "hello desk", "transport": "web", "external_id": "e4"})
        );
        assert_eq!(calls[1].1, json!({}));
        assert_eq!(calls[2].1, json!({"target": "%133"}));
        assert_eq!(calls[3].1["target"], "%133");
        assert!(calls[3].1["text"]
            .as_str()
            .unwrap()
            .contains("owner reply i7"));
        assert_eq!(
            auth.seen.lock().unwrap().as_slice(),
            &[(Some("api-token".into()), None)]
        );
    }

    #[test]
    fn owner_send_authentication_precedes_json_parsing_and_all_daemon_calls() {
        let backend = Arc::new(FakeOwner::new());
        let auth = Arc::new(FakeAuth::refusing(AuthError::SignInRequired));
        let mut request = request("POST", "/api/owner-send", Some("not json".into()));
        request.headers_mut().insert(
            header::COOKIE,
            HeaderValue::from_static("other=x; vp_session=owner-session"),
        );
        let response = block_on(app(auth.clone(), backend.clone()).oneshot(request)).unwrap();
        let (status, body, _) = response_json(response);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"detail": "Sign in required"}));
        assert!(backend.calls.lock().unwrap().is_empty());
        assert_eq!(
            auth.seen.lock().unwrap().as_slice(),
            &[(None, Some("owner-session".into()))]
        );
    }

    #[test]
    fn owner_send_same_origin_refusal_precedes_identity_and_daemon_calls() {
        let backend = Arc::new(FakeOwner::new());
        let auth = authorized();
        let mut request = request(
            "POST",
            "/api/owner-send",
            Some(json!({"text": "hello"}).to_string()),
        );
        request
            .headers_mut()
            .insert(header::HOST, HeaderValue::from_static("desk.example"));
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_static("https://attacker.example"),
        );
        let response = block_on(app(auth.clone(), backend.clone()).oneshot(request)).unwrap();
        let (status, body, _) = response_json(response);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "Cross-origin request refused"}));
        assert!(auth.seen.lock().unwrap().is_empty());
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn empty_oversize_unicode_and_attachments_are_refused_before_ingest() {
        let auth = authorized();
        let backend = Arc::new(FakeOwner::new());
        let router = app(auth, backend.clone());
        let cases = [
            (
                json!({"text": "  "}),
                StatusCode::BAD_REQUEST,
                "nothing to send",
            ),
            (
                json!({"text": "😀".repeat(16_001)}),
                StatusCode::BAD_REQUEST,
                "keep it under 16000 characters",
            ),
            (
                json!({"text": "x", "attachment_ids": [1]}),
                StatusCode::UNPROCESSABLE_ENTITY,
                "attachments are not supported on the owner mailbox yet",
            ),
        ];
        for (body, expected_status, expected_detail) in cases {
            let response = block_on(router.clone().oneshot(request(
                "POST",
                "/api/owner-send",
                Some(body.to_string()),
            )))
            .unwrap();
            let (status, body, _) = response_json(response);
            assert_eq!(status, expected_status);
            assert_eq!(body["detail"], expected_detail);
        }
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn no_live_desk_still_stores_and_says_so() {
        let backend = Arc::new(FakeOwner::new());
        backend.reply(
            "owner_ingest",
            json!({
                "message": {"message_id": "i7"}, "duplicate": false
            }),
        );
        backend.reply("list_panes", json!({"panes": []}));
        let response = block_on(app(authorized(), backend.clone()).oneshot(request(
            "POST",
            "/api/owner-send",
            Some(json!({"text": "hi"}).to_string()),
        )))
        .unwrap();
        let (status, body, _) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["outcome"], "stored");
        assert_eq!(body["notify"]["state"], "desk_offline");
        assert_eq!(backend.methods(), ["owner_ingest", "list_panes"]);
    }

    #[test]
    fn a_failed_direct_send_falls_back_to_the_inbox_nudge() {
        let backend = Arc::new(FakeOwner::new());
        set_send_responses(&backend, "ok");
        backend.replace(
            "send_text",
            Err(OwnerWebError::Failure("terminal busy".into())),
        );
        backend.reply("inbox_send", json!({"message_id": "i8", "state": "queued"}));
        let response = block_on(app(authorized(), backend.clone()).oneshot(request(
            "POST",
            "/api/owner-send",
            Some(json!({"text": "hi"}).to_string()),
        )))
        .unwrap();
        let (status, body, _) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["notify"]["state"], "nudged");
        assert!(body["notify"]["direct_failed"]
            .as_str()
            .unwrap()
            .contains("terminal busy"));
        assert_eq!(
            backend.methods(),
            [
                "owner_ingest",
                "list_panes",
                "delivery_capability",
                "send_text",
                "inbox_send"
            ]
        );
        let nudge = backend.calls.lock().unwrap().last().unwrap().1.clone();
        assert_eq!(nudge["to"], "%133");
        assert_eq!(nudge["notify"], true);
        assert!(nudge["body"]
            .as_str()
            .unwrap()
            .contains("Owner message i7 (web)"));
    }

    #[test]
    fn a_queued_or_unconfirmed_direct_send_is_reported_as_such_and_never_doubled() {
        for outcome in ["queued", "unconfirmed"] {
            let backend = Arc::new(FakeOwner::new());
            set_send_responses(&backend, outcome);
            let response = block_on(app(authorized(), backend.clone()).oneshot(request(
                "POST",
                "/api/owner-send",
                Some(json!({"text": "hi"}).to_string()),
            )))
            .unwrap();
            let (status, body, _) = response_json(response);
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["notify"]["state"], outcome);
            assert!(!backend.methods().contains(&"inbox_send".into()));
        }
    }

    #[test]
    fn a_refused_direct_send_falls_back_to_the_nudge() {
        let backend = Arc::new(FakeOwner::new());
        set_send_responses(&backend, "refused");
        backend.reply("inbox_send", json!({"state": "queued"}));
        let response = block_on(app(authorized(), backend.clone()).oneshot(request(
            "POST",
            "/api/owner-send",
            Some(json!({"text": "hi"}).to_string()),
        )))
        .unwrap();
        let (status, body, _) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["outcome"], "stored");
        assert_eq!(body["notify"]["state"], "nudged");
        assert!(body["notify"]["direct_failed"]
            .as_str()
            .unwrap()
            .contains("refused"));
        assert_eq!(backend.methods().last().unwrap(), "inbox_send");
    }

    #[test]
    fn both_routes_failing_is_reported_not_hidden() {
        let backend = Arc::new(FakeOwner::new());
        set_send_responses(&backend, "ok");
        backend.replace(
            "send_text",
            Err(OwnerWebError::Failure("terminal busy".into())),
        );
        backend.replace(
            "inbox_send",
            Err(OwnerWebError::Failure("inbox down".into())),
        );
        let response = block_on(app(authorized(), backend.clone()).oneshot(request(
            "POST",
            "/api/owner-send",
            Some(json!({"text": "hi"}).to_string()),
        )))
        .unwrap();
        let (status, body, _) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["outcome"], "stored");
        assert_eq!(body["notify"]["state"], "nudge_failed");
        assert!(body["notify"]["detail"]
            .as_str()
            .unwrap()
            .contains("inbox down"));
        assert!(body["notify"]["direct_failed"]
            .as_str()
            .unwrap()
            .contains("terminal busy"));
    }

    #[test]
    fn the_outcome_never_claims_delivery() {
        let backend = Arc::new(FakeOwner::new());
        set_send_responses(&backend, "ok");
        let response = block_on(app(authorized(), backend).oneshot(request(
            "POST",
            "/api/owner-send",
            Some(json!({"text": "x"}).to_string()),
        )))
        .unwrap();
        let (status, body, _) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["outcome"], "stored");
        assert!(!body.to_string().to_lowercase().contains("delivered"));
    }

    #[test]
    fn a_quiet_desk_is_still_nudged() {
        let backend = Arc::new(FakeOwner::new());
        set_send_responses(&backend, "ok");
        let response = block_on(app(authorized(), backend.clone()).oneshot(request(
            "POST",
            "/api/owner-send",
            Some(json!({"text": "hi"}).to_string()),
        )))
        .unwrap();
        let (status, body, _) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["notify"]["state"], "sent");
        assert_eq!(backend.methods()[1], "list_panes");
    }

    #[test]
    fn the_thread_shows_questions_and_the_desks_replies_in_order() {
        let backend = Arc::new(FakeOwner::new());
        backend.reply("owner_conversation", json!({
            "messages": [
                {"id":"i5", "body":"q1", "created_at":1000.0, "transport":"web", "sender":null, "reply_to":null, "answered":true},
                {"id":"i6", "body":"a1", "created_at":1010.0, "transport":"web", "sender":"%133", "reply_to":"i5"},
                {"id":"i9", "body":"agent report", "created_at":1020.0, "transport":null, "sender":"%21", "reply_to":null, "answered":false},
                {"id":"i10", "body":"human follow-up", "created_at":1030.0, "transport":"web", "sender":null, "reply_to":"i9"},
                {"id":"i11", "body":"legacy row", "created_at":1040.0, "transport":null, "sender":null, "reply_to":null},
                {"id":"i12", "body":"generated report", "created_at":1050.0, "transport":"report", "sender":null, "reply_to":null}
            ],
            "waiting": ["i10"]
        }));
        let mut request = request("GET", "/api/owner-thread?limit=9999", None);
        request.headers_mut().insert(
            header::COOKIE,
            HeaderValue::from_static("vp_session=owner-session"),
        );
        let response = block_on(app(authorized(), backend.clone()).oneshot(request)).unwrap();
        let (status, body, headers) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "application/json");
        assert_eq!(body["source"], "owner_conversation");
        assert_eq!(body["waiting"], json!(["i10"]));
        assert_eq!(body["items"].as_array().unwrap().len(), 6);
        assert_eq!(body["items"][0]["role"], "owner");
        assert_eq!(body["items"][0]["id"], 5);
        assert_eq!(body["items"][0]["created_at"], "1970-01-01T00:16:40Z");
        assert_eq!(body["items"][0]["delivery"], "acked");
        assert_eq!(body["items"][1]["role"], "agent");
        assert_eq!(body["items"][2]["role"], "agent");
        assert_eq!(body["items"][3]["role"], "owner");
        assert_eq!(body["items"][3]["reply_to"], "i9");
        assert_eq!(body["items"][3]["delivery"], "stored");
        assert_eq!(body["items"][4]["role"], "unknown");
        assert_eq!(body["items"][5]["role"], "unknown");
        assert_eq!(backend.methods(), ["owner_conversation"]);
        assert_eq!(backend.calls.lock().unwrap()[0].1, json!({"limit": 200}));
    }

    #[test]
    fn owner_thread_requires_auth_before_the_mailbox_rpc_and_preserves_rpc_errors() {
        let backend = Arc::new(FakeOwner::new());
        let response = block_on(
            app(
                Arc::new(FakeAuth::refusing(AuthError::SignInRequired)),
                backend.clone(),
            )
            .oneshot(request("GET", "/api/owner-thread", None)),
        )
        .unwrap();
        let (status, body, _) = response_json(response);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"detail": "Sign in required"}));
        assert!(backend.calls.lock().unwrap().is_empty());

        backend.replace(
            "owner_conversation",
            Err(OwnerWebError::Failure("daemon down".into())),
        );
        let response = block_on(app(authorized(), backend.clone()).oneshot(request(
            "GET",
            "/api/owner-thread?limit=0",
            None,
        )))
        .unwrap();
        let (status, body, _) = response_json(response);
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(
            body,
            json!({"detail": "the owner mailbox is unavailable: daemon down"})
        );
        assert_eq!(backend.calls.lock().unwrap()[0].1, json!({"limit": 1}));
    }
}
