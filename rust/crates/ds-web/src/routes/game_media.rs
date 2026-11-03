//! Authenticated game media routes ported from `web/app.py`.
//!
//! The narrow backend seam keeps codeword capture and game process streaming
//! outside the HTTP layer, so these handlers can be exercised without a live
//! emulator, display, audio sink, or credentials.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    body::Body,
    extract::Extension,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{ser::SerializeMap, Serialize, Serializer};
use serde_json::{json, Value};

use super::auth::{AuthBackend, AuthError, RequestScheme};

pub type GameMediaFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GameStreamKind {
    Video,
    Audio,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GameMediaError {
    /// Matches the OSError/SubprocessError/ValueError cases swallowed while
    /// loading codeword templates by the Python route.
    Template(String),
    /// A gameplay `GameError`, returned as HTTP 400 by autocodeword.
    Game(String),
    /// Unexpected backend failures retain FastAPI's generic 500 response.
    Failure(String),
}

/// Ordered JSON object for the codeword response. Python preserves insertion
/// order when serializing the game result; retaining ordered fields keeps the
/// raw HTTP body byte-compatible without changing serde_json's workspace-wide
/// map behavior.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GameMediaJson(Vec<(String, Value)>);

impl GameMediaJson {
    pub fn new(fields: Vec<(String, Value)>) -> Self {
        Self(fields)
    }
}

impl Serialize for GameMediaJson {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, value) in &self.0 {
            map.serialize_entry(name, value)?;
        }
        map.end()
    }
}

/// Concrete route operations supplied by the game adapter.
///
/// No method has a default: a missing emulator operation must not look like a
/// successful response. Stream bodies remain streaming Axum bodies instead of
/// being buffered by the route layer.
pub trait GameMediaBackend: Send + Sync + 'static {
    fn templates_cached(&self) -> GameMediaFuture<'_, Result<Vec<Value>, GameMediaError>>;

    fn recognize_codeword<'a>(
        &'a self,
        templates: Vec<Value>,
    ) -> GameMediaFuture<'a, Result<GameMediaJson, GameMediaError>>;

    fn status(&self) -> GameMediaFuture<'_, Result<Value, GameMediaError>>;

    fn stream<'a>(
        &'a self,
        kind: GameStreamKind,
    ) -> GameMediaFuture<'a, Result<Body, GameMediaError>>;
}

struct GameMediaState {
    backend: Arc<dyn GameMediaBackend>,
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
}

/// Mount the game media routes with explicit game, auth, and trusted-scheme
/// dependencies. Production route registration remains the caller's job.
pub fn router(
    backend: Arc<dyn GameMediaBackend>,
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
) -> Router {
    let state = Arc::new(GameMediaState {
        backend,
        auth,
        scheme,
    });
    Router::new()
        .route("/api/game/autocodeword", post(game_autocodeword))
        .route("/api/game/video", get(game_video))
        .route("/api/game/audio", get(game_audio))
        .layer(Extension(state))
}

async fn game_autocodeword(
    Extension(state): Extension<Arc<GameMediaState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = authenticate(&state, &headers).await {
        return response;
    }

    let templates = match state.backend.templates_cached().await {
        Ok(templates) => templates,
        Err(GameMediaError::Template(_)) => Vec::new(),
        Err(_) => return internal_error(),
    };
    match state.backend.recognize_codeword(templates).await {
        Ok(result) => Json(result).into_response(),
        Err(GameMediaError::Game(detail)) => detail_response(StatusCode::BAD_REQUEST, detail),
        Err(_) => internal_error(),
    }
}

async fn game_video(
    Extension(state): Extension<Arc<GameMediaState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = authenticate(&state, &headers).await {
        return response;
    }
    let status = match state.backend.status().await {
        Ok(status) => status,
        Err(_) => return internal_error(),
    };
    if !status.get("running").is_some_and(json_truthy) {
        return detail_response(StatusCode::NOT_FOUND, "Poolrad is not running".into());
    }
    let body = match state.backend.stream(GameStreamKind::Video).await {
        Ok(body) => body,
        Err(_) => return internal_error(),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace;boundary=ffmpeg",
        )
        .header(header::CACHE_CONTROL, "no-store")
        .header("x-content-type-options", "nosniff")
        .body(body)
        .expect("static game video response headers are valid")
}

async fn game_audio(
    Extension(state): Extension<Arc<GameMediaState>>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = authenticate(&state, &headers).await {
        return response;
    }
    let status = match state.backend.status().await {
        Ok(status) => status,
        Err(_) => return internal_error(),
    };
    if !status.get("running").is_some_and(json_truthy)
        || !status.get("audio").is_some_and(json_truthy)
    {
        return detail_response(StatusCode::NOT_FOUND, "Poolrad audio is unavailable".into());
    }
    let body = match state.backend.stream(GameStreamKind::Audio).await {
        Ok(body) => body,
        Err(_) => return internal_error(),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "audio/ogg")
        .header(header::CACHE_CONTROL, "no-store")
        .body(body)
        .expect("static game audio response headers are valid")
}

async fn authenticate(state: &GameMediaState, headers: &HeaderMap) -> Option<Response> {
    if let Some(response) = same_origin(headers, state.scheme) {
        return Some(response);
    }
    let bearer = bearer_value(headers);
    let cookie = cookie_value(headers);
    state
        .auth
        .identity(bearer.as_deref(), cookie.as_deref())
        .await
        .err()
        .map(auth_error_response)
}

fn same_origin(headers: &HeaderMap, scheme: RequestScheme) -> Option<Response> {
    let origin = headers.get(header::ORIGIN)?;
    let origin = match origin.to_str() {
        Ok(origin) => origin,
        Err(_) => return Some(cross_origin_response()),
    };
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let scheme = match scheme {
        RequestScheme::Http => "http",
        RequestScheme::Https => "https",
    };
    (origin != format!("{scheme}://{host}")).then(cross_origin_response)
}

fn bearer_value(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::to_owned)
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    let mut cookie = None;
    for value in headers.get_all(header::COOKIE).iter() {
        let value = value
            .as_bytes()
            .iter()
            .map(|byte| char::from(*byte))
            .collect::<String>();
        for part in value.split(';') {
            let (key, value) = part.split_once('=').unwrap_or(("", part));
            if key.trim() == "vp_session" {
                cookie = Some(unquote_cookie_value(value.trim()));
            }
        }
    }
    cookie.filter(|value| !value.is_empty())
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
        AuthError::Internal(_) => internal_error(),
    }
}

fn cross_origin_response() -> Response {
    detail_response(StatusCode::FORBIDDEN, "Cross-origin request refused".into())
}

fn detail_response(status: StatusCode, detail: String) -> Response {
    (status, Json(json!({"detail": detail}))).into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value
            .as_i64()
            .map(|value| value != 0)
            .or_else(|| value.as_u64().map(|value| value != 0))
            .or_else(|| value.as_f64().map(|value| value != 0.0))
            .unwrap_or(true),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}
