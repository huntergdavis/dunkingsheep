//! Game save-state HTTP handlers ported from `web/app.py`.
//!
//! The runtime owns DOSBox state and file operations behind `GameStateBackend`;
//! this module owns request validation, auth, and Python-compatible responses.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    body::to_bytes,
    extract::{Extension, Request},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use super::auth::{AuthBackend, AuthError, Identity, RequestScheme};

pub type GameStateFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, GameStateError>> + Send + 'a>>;

/// Runtime operations used by the three game-state endpoints.
///
/// A concrete adapter can call the game session/client; route tests inject a
/// fake so they never contact a running emulator or write save-state files.
pub trait GameStateBackend: Send + Sync + 'static {
    fn list_states(&self) -> GameStateFuture<'_>;
    fn savestate(&self, tag: String) -> GameStateFuture<'_>;
    fn loadstate(&self, slot: i64) -> GameStateFuture<'_>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GameStateError {
    Game(String),
    Failure(String),
}

/// Mount the three game-state routes using the default HTTP scheme.
pub fn router() -> Router {
    router_with_scheme(RequestScheme::Http)
}

/// Mount game-state handlers; the containing application supplies auth and
/// game backends as extensions and chooses the externally visible scheme.
pub fn router_with_scheme(scheme: RequestScheme) -> Router {
    Router::new()
        .route("/api/game/states", get(game_states))
        .route("/api/game/savestate", post(game_savestate))
        .route("/api/game/loadstate", post(game_loadstate))
        .layer(Extension(scheme))
}

async fn game_states(
    Extension(game): Extension<Arc<dyn GameStateBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    match game.list_states().await {
        Ok(states) => Json(states).into_response(),
        Err(_) => internal_error(),
    }
}

async fn game_savestate(
    Extension(game): Extension<Arc<dyn GameStateBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    let parsed_body = match request_json(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Err(response) = authenticate(auth.as_ref(), &headers, scheme).await {
        return response;
    }
    let body = match parsed_body {
        Some(Value::Object(body)) => body,
        Some(input) => return model_error(input),
        None => return missing_body_error(),
    };
    let tag = match body.get("tag") {
        None => String::new(),
        Some(Value::String(tag)) if tag.chars().count() <= 80 => tag.clone(),
        Some(Value::String(tag)) => {
            return validation_error(
                "string_too_long",
                vec![json!("body"), json!("tag")],
                "String should have at most 80 characters",
                json!(tag),
                Some(json!({"max_length": 80})),
            );
        }
        Some(input) => {
            return validation_error(
                "string_type",
                vec![json!("body"), json!("tag")],
                "Input should be a valid string",
                input.clone(),
                None,
            );
        }
    };

    match game.savestate(tag).await {
        Ok(state) => Json(state).into_response(),
        Err(GameStateError::Game(detail)) => detail_response(StatusCode::BAD_REQUEST, detail),
        Err(GameStateError::Failure(_)) => internal_error(),
    }
}

async fn game_loadstate(
    Extension(game): Extension<Arc<dyn GameStateBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    let parsed_body = match request_json(request).await {
        Ok(body) => body,
        Err(response) => return response,
    };
    if let Err(response) = authenticate(auth.as_ref(), &headers, scheme).await {
        return response;
    }
    let body = match parsed_body {
        Some(Value::Object(body)) => body,
        Some(input) => return model_error(input),
        None => return missing_body_error(),
    };
    let Some(input) = body.get("slot") else {
        return validation_error(
            "missing",
            vec![json!("body"), json!("slot")],
            "Field required",
            Value::Object(body),
            None,
        );
    };
    let Some(slot) = python_integer(input) else {
        let bound = match input {
            Value::String(value) => match clean_pydantic_integer_string(value) {
                Ok(normalized) => numeric_slot_bound(&normalized),
                Err(IntegerStringError::TooManyDigits) => {
                    return slot_integer_string_too_large_error(input);
                }
                Err(IntegerStringError::Invalid) => None,
            },
            Value::Number(number) => numeric_slot_bound(&number.to_string()),
            _ => None,
        };
        match bound {
            Some(SlotBound::Minimum) => return slot_below_minimum_error(input),
            Some(SlotBound::Maximum) => return slot_exceeds_maximum_error(input),
            None => {}
        }
        let (kind, message) = match input {
            Value::String(_) => (
                "int_parsing",
                "Input should be a valid integer, unable to parse string as an integer",
            ),
            Value::Number(number) if number.as_f64().is_some_and(|value| value.fract() != 0.0) => (
                "int_from_float",
                "Input should be a valid integer, got a number with a fractional part",
            ),
            _ => ("int_type", "Input should be a valid integer"),
        };
        return validation_error(
            kind,
            vec![json!("body"), json!("slot")],
            message,
            input.clone(),
            None,
        );
    };
    if slot < 1 {
        return slot_below_minimum_error(input);
    }
    if slot > 99 {
        return slot_exceeds_maximum_error(input);
    }

    match game.loadstate(slot).await {
        Ok(state) => Json(state).into_response(),
        Err(GameStateError::Game(detail)) => detail_response(StatusCode::BAD_REQUEST, detail),
        Err(GameStateError::Failure(_)) => internal_error(),
    }
}

async fn request_json(request: Request) -> Result<Option<Value>, Response> {
    let parse_as_json = request_json_content_type(request.headers());
    let bytes = to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|_| {
            detail_response(
                StatusCode::BAD_REQUEST,
                "There was an error parsing the body",
            )
        })?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if !parse_as_json {
        return Ok(Some(Value::String(
            String::from_utf8_lossy(&bytes).into_owned(),
        )));
    }
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        let error_text = error.to_string();
        let invalid_escape = (error_text.starts_with("invalid escape")
            || error_text.starts_with("EOF while parsing a string"))
            .then(|| python_invalid_escape(&bytes))
            .flatten();
        let trailing_comma = error_text
            .starts_with("trailing comma")
            .then(|| python_trailing_comma(&bytes))
            .flatten();
        let offset = if let Some(invalid_escape) = invalid_escape {
            invalid_escape.offset()
        } else if error_text.starts_with("EOF while parsing an object") {
            String::from_utf8_lossy(&bytes).chars().count()
        } else if error_text.starts_with("EOF while parsing a string") {
            python_unterminated_string_offset(&bytes)
                .unwrap_or_else(|| error.column().saturating_sub(1))
        } else if error_text.starts_with("EOF while parsing") {
            String::from_utf8_lossy(&bytes).chars().count()
        } else if let Some((offset, _)) = trailing_comma {
            offset
        } else {
            python_json_offset(&bytes, error.line(), error.column())
        };
        validation_error(
            "json_invalid",
            vec![json!("body"), json!(offset)],
            "JSON decode error",
            json!({}),
            Some(json!({
                "error": python_json_error(
                    &error_text,
                    trailing_comma.map(|(_, kind)| kind),
                    invalid_escape,
                )
            })),
        )
    })
}

fn request_json_content_type(headers: &HeaderMap) -> bool {
    let Some(content_type) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        // The installed FastAPI strict-content-type default treats absent headers as raw bytes.
        return false;
    };
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    let Some((top_level, subtype)) = media_type.split_once('/') else {
        return false;
    };
    top_level.eq_ignore_ascii_case("application")
        && (subtype.eq_ignore_ascii_case("json") || subtype.to_ascii_lowercase().ends_with("+json"))
}

fn python_unterminated_string_offset(bytes: &[u8]) -> Option<usize> {
    let text = String::from_utf8_lossy(bytes);
    let mut string_start = None;
    let mut escaped = false;

    for (character_offset, character) in text.char_indices() {
        if string_start.is_some() {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                string_start = None;
            }
        } else if character == '"' {
            string_start = Some(character_offset);
        }
    }

    string_start.map(|byte_offset| text[..byte_offset].chars().count())
}

#[derive(Clone, Copy)]
enum PythonInvalidEscape {
    General { offset: usize },
    Unicode { offset: usize },
}

impl PythonInvalidEscape {
    fn offset(self) -> usize {
        match self {
            Self::General { offset } | Self::Unicode { offset } => offset,
        }
    }
}

fn python_invalid_escape(bytes: &[u8]) -> Option<PythonInvalidEscape> {
    let text = String::from_utf8_lossy(bytes);
    let characters: Vec<char> = text.chars().collect();
    let mut in_string = false;
    let mut index = 0;

    while index < characters.len() {
        let character = characters[index];
        if !in_string {
            if character == '"' {
                in_string = true;
            }
            index += 1;
            continue;
        }

        if character == '"' {
            in_string = false;
            index += 1;
        } else if character == '\\' {
            let escape = characters.get(index + 1).copied()?;
            if escape == 'u' {
                let valid_hex_digits = characters
                    .get(index + 2..index + 6)
                    .is_some_and(|digits| digits.iter().all(|digit| digit.is_ascii_hexdigit()));
                if !valid_hex_digits {
                    // Python points at the `u` in "Invalid \\uXXXX escape".
                    return Some(PythonInvalidEscape::Unicode { offset: index + 1 });
                }
                index += 6;
            } else if matches!(escape, '"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't') {
                index += 2;
            } else {
                return Some(PythonInvalidEscape::General { offset: index });
            }
        } else {
            index += 1;
        }
    }

    None
}

#[derive(Clone, Copy)]
enum JsonContainer {
    Object,
    Array,
}

fn python_json_error(
    error: &str,
    trailing_container: Option<JsonContainer>,
    invalid_escape: Option<PythonInvalidEscape>,
) -> &'static str {
    if error.starts_with("key must be a string")
        || error.starts_with("EOF while parsing an object")
    {
        "Expecting property name enclosed in double quotes"
    } else if error.starts_with("trailing comma") {
        match trailing_container {
            Some(JsonContainer::Object) => "Illegal trailing comma before end of object",
            Some(JsonContainer::Array) => "Illegal trailing comma before end of array",
            None => "Invalid JSON",
        }
    } else if matches!(invalid_escape, Some(PythonInvalidEscape::Unicode { .. })) {
        "Invalid \\uXXXX escape"
    } else if error.starts_with("EOF while parsing a string") {
        "Unterminated string starting at"
    } else if error.starts_with("invalid escape") {
        match invalid_escape {
            Some(PythonInvalidEscape::General { .. }) => "Invalid \\escape",
            None => "Invalid JSON",
        }
    } else if error.starts_with("EOF while parsing") || error.starts_with("expected value") {
        "Expecting value"
    } else if error.starts_with("expected `:`") {
        "Expecting ':' delimiter"
    } else if error.starts_with("expected `,`") {
        "Expecting ',' delimiter"
    } else {
        "Invalid JSON"
    }
}

fn python_json_offset(bytes: &[u8], line: usize, column: usize) -> usize {
    let text = String::from_utf8_lossy(bytes);
    let mut lines = text.split_inclusive('\n');
    let mut preceding_characters = 0;
    for _ in 1..line {
        let Some(previous_line) = lines.next() else {
            return text.chars().count();
        };
        preceding_characters += previous_line.chars().count();
    }
    let current_line = lines.next().unwrap_or_default();
    let current_line = current_line.strip_suffix('\n').unwrap_or(current_line);
    let mut byte_offset = column.saturating_sub(1).min(current_line.len());
    while !current_line.is_char_boundary(byte_offset) {
        byte_offset -= 1;
    }
    preceding_characters + current_line[..byte_offset].chars().count()
}

fn python_trailing_comma(bytes: &[u8]) -> Option<(usize, JsonContainer)> {
    let text = String::from_utf8_lossy(bytes);
    let mut in_string = false;
    let mut escaped = false;
    let mut containers = Vec::new();

    for (byte_offset, character) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '"' {
                in_string = false;
            }
        } else if character == '"' {
            in_string = true;
        } else if character == '{' {
            containers.push(JsonContainer::Object);
        } else if character == '[' {
            containers.push(JsonContainer::Array);
        } else if character == ',' {
            let after_comma = &text[byte_offset + character.len_utf8()..];
            let closing = after_comma
                .trim_start_matches(|character| matches!(character, ' ' | '\n' | '\r' | '\t'))
                .chars()
                .next();
            let container = match (closing, containers.last()) {
                (Some('}'), Some(JsonContainer::Object)) => Some(JsonContainer::Object),
                (Some(']'), Some(JsonContainer::Array)) => Some(JsonContainer::Array),
                _ => None,
            };
            if let Some(container) = container {
                return Some((text[..byte_offset].chars().count(), container));
            }
        } else if matches!(character, '}' | ']') {
            containers.pop();
        }
    }
    None
}

fn python_integer(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number
            .as_i64()
            .or_else(|| number.as_u64().and_then(|value| i64::try_from(value).ok()))
            .or_else(|| {
                number
                    .as_f64()
                    .filter(|value| value.fract() == 0.0)
                    .and_then(|value| i64::try_from(value as i128).ok())
            }),
        Value::String(value) => clean_pydantic_integer_string(value).ok()?.parse().ok(),
        Value::Bool(value) => Some(i64::from(*value)),
        Value::Null | Value::Array(_) | Value::Object(_) => None,
    }
}

#[derive(Clone, Copy)]
enum SlotBound {
    Minimum,
    Maximum,
}

fn numeric_slot_bound(value: &str) -> Option<SlotBound> {
    let (negative, digits) = if let Some(digits) = value.strip_prefix('-') {
        (true, digits)
    } else if let Some(digits) = value.strip_prefix('+') {
        (false, digits)
    } else {
        (false, value)
    };
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let significant = digits.trim_start_matches('0');
    if negative || significant.is_empty() {
        Some(SlotBound::Minimum)
    } else if significant.len() > 2 || significant > "99" {
        Some(SlotBound::Maximum)
    } else {
        None
    }
}

const PYDANTIC_MAX_INT_DIGITS: usize = 4_300;

enum IntegerStringError {
    Invalid,
    TooManyDigits,
}

fn clean_pydantic_integer_string(value: &str) -> Result<String, IntegerStringError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(IntegerStringError::Invalid);
    }
    let bytes = value.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if *byte == b'_'
            && (index == 0
                || index + 1 == bytes.len()
                || !bytes[index - 1].is_ascii_digit()
                || !bytes[index + 1].is_ascii_digit())
        {
            return Err(IntegerStringError::Invalid);
        }
    }
    let mut cleaned: String = value.chars().filter(|character| *character != '_').collect();
    if let Some(decimal) = cleaned.find('.') {
        let fraction = &cleaned[decimal + 1..];
        if fraction.is_empty() || !fraction.bytes().all(|byte| byte == b'0') {
            return Err(IntegerStringError::Invalid);
        }
        cleaned.truncate(decimal);
    }
    let digits = cleaned
        .strip_prefix('-')
        .or_else(|| cleaned.strip_prefix('+'))
        .unwrap_or(&cleaned);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(IntegerStringError::Invalid);
    }
    if digits.len() > PYDANTIC_MAX_INT_DIGITS {
        return Err(IntegerStringError::TooManyDigits);
    }
    if cleaned.starts_with('+') {
        cleaned.remove(0);
    }
    Ok(cleaned)
}

fn model_error(input: Value) -> Response {
    validation_error(
        "model_attributes_type",
        vec![json!("body")],
        "Input should be a valid dictionary or object to extract fields from",
        input,
        None,
    )
}

fn slot_exceeds_maximum_error(input: &Value) -> Response {
    validation_error(
        "less_than_equal",
        vec![json!("body"), json!("slot")],
        "Input should be less than or equal to 99",
        input.clone(),
        Some(json!({"le": 99})),
    )
}

fn slot_below_minimum_error(input: &Value) -> Response {
    validation_error(
        "greater_than_equal",
        vec![json!("body"), json!("slot")],
        "Input should be greater than or equal to 1",
        input.clone(),
        Some(json!({"ge": 1})),
    )
}

fn slot_integer_string_too_large_error(input: &Value) -> Response {
    validation_error(
        "int_parsing_size",
        vec![json!("body"), json!("slot")],
        "Unable to parse input string as an integer, exceeded maximum size",
        input.clone(),
        None,
    )
}

fn missing_body_error() -> Response {
    validation_error(
        "missing",
        vec![json!("body")],
        "Field required",
        Value::Null,
        None,
    )
}

fn validation_error(
    kind: &str,
    location: Vec<Value>,
    message: &str,
    input: Value,
    context: Option<Value>,
) -> Response {
    let mut issue = serde_json::Map::new();
    issue.insert("type".into(), json!(kind));
    issue.insert("loc".into(), Value::Array(location));
    issue.insert("msg".into(), json!(message));
    issue.insert("input".into(), input);
    if let Some(context) = context {
        issue.insert("ctx".into(), context);
    }
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [Value::Object(issue)]})),
    )
        .into_response()
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
        .then(|| detail_response(StatusCode::FORBIDDEN, "Cross-origin request refused"))
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
            "Too many attempts; retry in five minutes",
        ),
        AuthError::IncorrectPassword => {
            detail_response(StatusCode::UNAUTHORIZED, "Incorrect password")
        }
        AuthError::SignInRequired => detail_response(StatusCode::UNAUTHORIZED, "Sign in required"),
        AuthError::Internal(_) => internal_error(),
    }
}

fn detail_response(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({"detail": detail.into()}))).into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        http::Request as HttpRequest,
    };
    use std::{
        future::Future,
        pin::pin,
        sync::Mutex,
        task::{Context, Poll, Waker},
    };
    use tower::ServiceExt;

    struct FakeGame {
        calls: Mutex<Vec<String>>,
        result: Mutex<Result<Value, GameStateError>>,
    }

    impl FakeGame {
        fn new(result: Value) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                result: Mutex::new(Ok(result)),
            }
        }

        fn fail(&self, error: GameStateError) {
            *self.result.lock().unwrap() = Err(error);
        }
    }

    impl GameStateBackend for FakeGame {
        fn list_states(&self) -> GameStateFuture<'_> {
            self.calls.lock().unwrap().push("list_states".into());
            let result = self.result.lock().unwrap().clone();
            Box::pin(async move { result })
        }

        fn savestate(&self, tag: String) -> GameStateFuture<'_> {
            self.calls.lock().unwrap().push(format!("savestate:{tag}"));
            let result = self.result.lock().unwrap().clone();
            Box::pin(async move { result })
        }

        fn loadstate(&self, slot: i64) -> GameStateFuture<'_> {
            self.calls.lock().unwrap().push(format!("loadstate:{slot}"));
            let result = self.result.lock().unwrap().clone();
            Box::pin(async move { result })
        }
    }

    struct FakeAuth {
        result: Result<Identity, AuthError>,
    }

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _address: &'a str,
            _password: &'a str,
        ) -> super::super::auth::AuthFuture<'a, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unused".into())) })
        }

        fn identity<'a>(
            &'a self,
            _bearer: Option<&'a str>,
            _cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<Identity, AuthError>> {
            let result = self.result.clone();
            Box::pin(async move { result })
        }

        fn logout<'a>(
            &'a self,
            _cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unused".into())) })
        }

        fn change_password<'a>(
            &'a self,
            _current: &'a str,
            _new: &'a str,
            _keep_cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unused".into())) })
        }
    }

    fn app(game: Arc<dyn GameStateBackend>, auth: Arc<dyn AuthBackend>) -> Router {
        router().layer(Extension(auth)).layer(Extension(game))
    }

    fn request(method: &str, path: &str, body: &str) -> HttpRequest<Body> {
        request_with_content_type(method, path, body, "application/json")
    }

    fn request_with_content_type(
        method: &str,
        path: &str,
        body: &str,
        content_type: &str,
    ) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "desk.test")
            .header(header::CONTENT_TYPE, content_type)
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    fn request_without_content_type(method: &str, path: &str, body: &str) -> HttpRequest<Body> {
        HttpRequest::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "desk.test")
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker: &Waker = Waker::noop();
        let mut context = Context::from_waker(waker);
        let mut future = pin!(future);
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                return output;
            }
        }
    }

    fn authorized() -> Arc<dyn AuthBackend> {
        Arc::new(FakeAuth {
            result: Ok(Identity::Api),
        })
    }

    fn response(app: Router, request: HttpRequest<Body>) -> (StatusCode, String, Value) {
        let response = block_on(app.oneshot(request)).unwrap();
        let status = response.status();
        assert_eq!(
            "application/json",
            response
                .headers()
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap()
        );
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        let json = serde_json::from_str(&body).unwrap_or(Value::Null);
        (status, body, json)
    }

    #[test]
    fn game_states_returns_backend_json() {
        let game = Arc::new(FakeGame::new(json!({"run_name": null, "states": []})));
        let (status, body, value) = response(
            app(game.clone(), authorized()),
            request("GET", "/api/game/states", ""),
        );
        assert_eq!(StatusCode::OK, status);
        assert_eq!(r#"{"run_name":null,"states":[]}"#, body);
        assert_eq!(json!({"run_name": null, "states": []}), value);
        assert_eq!(vec!["list_states"], *game.calls.lock().unwrap());
    }

    #[test]
    fn game_savestate_defaults_tag_and_returns_backend_json() {
        let game = Arc::new(FakeGame::new(json!({"slot": 1, "tag": "slot 1"})));
        let (status, body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/savestate", "{}"),
        );
        assert_eq!(StatusCode::OK, status);
        assert_eq!(r#"{"slot":1,"tag":"slot 1"}"#, body);
        assert_eq!(json!({"slot": 1, "tag": "slot 1"}), value);
        assert_eq!(vec!["savestate:"], *game.calls.lock().unwrap());
    }

    #[test]
    fn game_savestate_maps_game_error_to_python_400_detail() {
        let game = Arc::new(FakeGame::new(Value::Null));
        game.fail(GameStateError::Game(
            "All save-state slots are in use".into(),
        ));
        let (status, body, value) = response(
            app(game, authorized()),
            request("POST", "/api/game/savestate", r#"{"tag":"quick"}"#),
        );
        assert_eq!(StatusCode::BAD_REQUEST, status);
        assert_eq!(r#"{"detail":"All save-state slots are in use"}"#, body);
        assert_eq!(json!({"detail": "All save-state slots are in use"}), value);
    }

    #[test]
    fn game_loadstate_passes_validated_slot_and_returns_backend_json() {
        let game = Arc::new(FakeGame::new(
            json!({"slot": 8, "loaded": true, "verified": true}),
        ));
        let (status, body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/loadstate", r#"{"slot":8}"#),
        );
        assert_eq!(StatusCode::OK, status);
        assert_eq!(r#"{"slot":8,"loaded":true,"verified":true}"#, body);
        assert_eq!(json!({"slot": 8, "loaded": true, "verified": true}), value);
        assert_eq!(vec!["loadstate:8"], *game.calls.lock().unwrap());
    }

    #[test]
    fn game_loadstate_maps_game_error_to_python_400_detail() {
        let game = Arc::new(FakeGame::new(Value::Null));
        game.fail(GameStateError::Game(
            "That save state is not available".into(),
        ));
        let (status, body, value) = response(
            app(game, authorized()),
            request("POST", "/api/game/loadstate", r#"{"slot":4}"#),
        );
        assert_eq!(StatusCode::BAD_REQUEST, status);
        assert_eq!(r#"{"detail":"That save state is not available"}"#, body);
        assert_eq!(json!({"detail": "That save state is not available"}), value);
    }

    #[test]
    fn game_loadstate_rejects_out_of_range_slot_without_calling_backend() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let (status, body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/loadstate", r#"{"slot":0}"#),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
        assert_eq!(
            json!({"detail":[{"type":"greater_than_equal","loc":["body","slot"],
                "msg":"Input should be greater than or equal to 1","input":0,"ctx":{"ge":1}}]}),
            value
        );
        assert_eq!(serde_json::to_string(&value).unwrap(), body);
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_loadstate_rejects_large_unsigned_slot_with_python_upper_bound_error() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let slot = 9_223_372_036_854_775_908u64;
        let body = format!(r#"{{"slot":{slot}}}"#);
        let (status, _body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/loadstate", &body),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
        assert_eq!(
            json!({"detail":[{"type":"less_than_equal","loc":["body","slot"],
                "msg":"Input should be less than or equal to 99",
                "input":9_223_372_036_854_775_908u64,"ctx":{"le":99}}]}),
            value
        );
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_loadstate_maps_large_numeric_string_to_python_upper_bound_error() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let (status, _body, value) = response(
            app(game.clone(), authorized()),
            request(
                "POST",
                "/api/game/loadstate",
                r#"{"slot":"9223372036854775808"}"#,
            ),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
        assert_eq!(
            json!({"detail":[{"type":"less_than_equal","loc":["body","slot"],
                "msg":"Input should be less than or equal to 99",
                "input":"9223372036854775808","ctx":{"le":99}}]}),
            value
        );
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_loadstate_preserves_arbitrary_precision_json_integer_bounds() {
        let game = Arc::new(FakeGame::new(Value::Null));
        for (slot, expected_error) in [
            ("18446744073709551616", "less_than_equal"),
            ("-9223372036854775809", "greater_than_equal"),
        ] {
            let body = format!(r#"{{"slot":{slot}}}"#);
            let (status, _body, value) = response(
                app(game.clone(), authorized()),
                request("POST", "/api/game/loadstate", &body),
            );
            assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
            assert_eq!(expected_error, value["detail"][0]["type"]);
            assert_eq!(slot, value["detail"][0]["input"].to_string());
        }
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_loadstate_enforces_pydantic_integer_string_digit_limit() {
        let game = Arc::new(FakeGame::new(Value::Null));
        for (digit_count, expected_error) in [(4_300, "less_than_equal"), (4_301, "int_parsing_size")] {
            let slot = "9".repeat(digit_count);
            let body = format!(r#"{{"slot":"{slot}"}}"#);
            let (status, _body, value) = response(
                app(game.clone(), authorized()),
                request("POST", "/api/game/loadstate", &body),
            );
            assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
            assert_eq!(expected_error, value["detail"][0]["type"]);
            assert_eq!(slot, value["detail"][0]["input"]);
            if expected_error == "int_parsing_size" {
                assert_eq!(
                    "Unable to parse input string as an integer, exceeded maximum size",
                    value["detail"][0]["msg"]
                );
            }
            assert!(game.calls.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn game_loadstate_coerces_pydantic_integer_string_forms_before_bounds_checks() {
        let cases = [
            ("+1", Some(1), None),
            ("-1", None, Some("greater_than_equal")),
            ("1_0", Some(10), None),
            ("1_00", None, Some("less_than_equal")),
            ("1.0", Some(1), None),
            ("100.0", None, Some("less_than_equal")),
            (
                "9_223_372_036_854_775_808",
                None,
                Some("less_than_equal"),
            ),
        ];
        for (value, expected_slot, expected_error) in cases {
            let game = Arc::new(FakeGame::new(Value::Null));
            let body = format!(r#"{{"slot":"{value}"}}"#);
            let (status, _body, response) = response(
                app(game.clone(), authorized()),
                request("POST", "/api/game/loadstate", &body),
            );
            if let Some(slot) = expected_slot {
                assert_eq!(StatusCode::OK, status, "input {value:?}");
                assert_eq!(
                    vec![format!("loadstate:{slot}")],
                    game.calls.lock().unwrap().clone(),
                    "input {value:?}"
                );
            } else {
                assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status, "input {value:?}");
                assert_eq!(expected_error.unwrap(), response["detail"][0]["type"]);
                assert_eq!(value, response["detail"][0]["input"]);
                assert!(game.calls.lock().unwrap().is_empty());
            }
        }
    }

    #[test]
    fn game_loadstate_requires_slot_with_python_validation_shape() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let (status, _body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/loadstate", "{}"),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
        assert_eq!(
            json!({"detail":[{"type":"missing","loc":["body","slot"],
                "msg":"Field required","input":{}}]}),
            value
        );
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_savestate_rejects_long_tag_with_python_validation_shape() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let long_tag = "x".repeat(81);
        let body = serde_json::to_string(&json!({"tag": long_tag})).unwrap();
        let (status, _body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/savestate", &body),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
        assert_eq!("string_too_long", value["detail"][0]["type"]);
        assert_eq!(80, value["detail"][0]["ctx"]["max_length"]);
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_savestate_maps_incomplete_json_object_to_python_decode_error() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let (status, _body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/savestate", "{"),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
        assert_eq!(
            json!({"detail":[{"type":"json_invalid","loc":["body",1],
                "msg":"JSON decode error","input":{},
                "ctx":{"error":"Expecting property name enclosed in double quotes"}}]}),
            value
        );
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_state_model_routes_map_invalid_json_escape_to_python_decode_error() {
        let game = Arc::new(FakeGame::new(Value::Null));
        for path in ["/api/game/savestate", "/api/game/loadstate"] {
            for (body, offset, error) in [
                (r#"{"tag":"\q"}"#, 8, r"Invalid \escape"),
                (r#"{"tag":"\u12G4"}"#, 9, r"Invalid \uXXXX escape"),
                (r#"{"tag":"\u12"}"#, 9, r"Invalid \uXXXX escape"),
            ] {
                let (status, _body, value) = response(
                    app(game.clone(), authorized()),
                    request("POST", path, body),
                );
                assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
                assert_eq!(
                    json!({"detail":[{"type":"json_invalid","loc":["body",offset],
                        "msg":"JSON decode error","input":{},
                        "ctx":{"error":error}}]}),
                    value
                );
            }
        }
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_savestate_maps_trailing_object_comma_to_python_decode_error() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let (status, _body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/savestate", r#"{"tag":"quick",}"#),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
        assert_eq!(
            json!({"detail":[{"type":"json_invalid","loc":["body",14],
                "msg":"JSON decode error","input":{},
                "ctx":{"error":"Illegal trailing comma before end of object"}}]}),
            value
        );
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_savestate_maps_array_comma_and_multiline_json_errors_to_python() {
        let game = Arc::new(FakeGame::new(Value::Null));
        for (body, offset, error) in [
            ("[1,]", 2, "Illegal trailing comma before end of array"),
            ("{\"tag\":", 7, "Expecting value"),
            ("{\"tag\":\n}", 8, "Expecting value"),
        ] {
            let (status, _body, value) = response(
                app(game.clone(), authorized()),
                request("POST", "/api/game/savestate", body),
            );
            assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
            assert_eq!("json_invalid", value["detail"][0]["type"]);
            assert_eq!(offset, value["detail"][0]["loc"][1]);
            assert_eq!(error, value["detail"][0]["ctx"]["error"]);
        }
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_savestate_maps_unterminated_json_string_to_python_decode_error() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let (status, _body, value) = response(
            app(game.clone(), authorized()),
            request("POST", "/api/game/savestate", "{\"tag\":\"quick"),
        );
        assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
        assert_eq!(
            json!({"detail":[{"type":"json_invalid","loc":["body",7],
                "msg":"JSON decode error","input":{},
                "ctx":{"error":"Unterminated string starting at"}}]}),
            value
        );
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_savestate_decodes_malformed_json_before_auth_and_origin_checks() {
        let game = Arc::new(FakeGame::new(Value::Null));
        for path in ["/api/game/savestate", "/api/game/loadstate"] {
            let unauthenticated = Arc::new(FakeAuth {
                result: Err(AuthError::SignInRequired),
            });
            let (status, _body, value) = response(
                app(game.clone(), unauthenticated.clone()),
                request("POST", path, "{"),
            );
            assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
            assert_eq!(
                json!({"detail":[{"type":"json_invalid","loc":["body",1],
                    "msg":"JSON decode error","input":{},
                    "ctx":{"error":"Expecting property name enclosed in double quotes"}}]}),
                value
            );

            let cross_origin_request = HttpRequest::builder()
                .method("POST")
                .uri(path)
                .header(header::HOST, "desk.test")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ORIGIN, "https://foreign.test")
                .body(Body::from("{"))
                .unwrap();
            let (status, _body, value) =
                response(app(game.clone(), authorized()), cross_origin_request);
            assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
            assert_eq!("json_invalid", value["detail"][0]["type"]);
            assert_eq!(1, value["detail"][0]["loc"][1]);

            let (status, _body, value) = response(
                app(game.clone(), unauthenticated),
                request("POST", path, "{}"),
            );
            assert_eq!(StatusCode::UNAUTHORIZED, status);
            assert_eq!(json!({"detail":"Sign in required"}), value);
        }
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_state_model_routes_reject_json_text_with_non_json_content_type() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let game_app = app(game.clone(), authorized());
        for (path, body) in [
            ("/api/game/savestate", r#"{"tag":"quick"}"#),
            ("/api/game/loadstate", r#"{"slot":3}"#),
        ] {
            let (status, _body, value) = response(
                game_app.clone(),
                request_with_content_type("POST", path, body, "text/plain"),
            );
            assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
            assert_eq!(
                json!({"detail":[{"type":"model_attributes_type","loc":["body"],
                    "msg":"Input should be a valid dictionary or object to extract fields from",
                    "input":body}]}),
                value
            );
        }
        assert!(game.calls.lock().unwrap().is_empty());

        for (path, body) in [
            ("/api/game/savestate", r#"{"tag":"quick"}"#),
            ("/api/game/loadstate", r#"{"slot":3}"#),
        ] {
            let (status, _body, _value) = response(
                game_app.clone(),
                request_with_content_type("POST", path, body, "application/vnd.example+json"),
            );
            assert_eq!(StatusCode::OK, status);
        }
        assert_eq!(
            vec!["savestate:quick".to_owned(), "loadstate:3".to_owned()],
            game.calls.lock().unwrap().clone()
        );
    }

    #[test]
    fn game_state_model_routes_treat_missing_content_type_as_raw_before_dependencies() {
        let game = Arc::new(FakeGame::new(Value::Null));
        for (path, body) in [
            ("/api/game/savestate", r#"{"tag":"quick"}"#),
            ("/api/game/loadstate", r#"{"slot":3}"#),
        ] {
            let (status, _body, value) = response(
                app(game.clone(), authorized()),
                request_without_content_type("POST", path, body),
            );
            assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
            assert_eq!(
                json!({"detail":[{"type":"model_attributes_type","loc":["body"],
                    "msg":"Input should be a valid dictionary or object to extract fields from",
                    "input":body}]}),
                value
            );
        }
        assert!(game.calls.lock().unwrap().is_empty());

        for path in ["/api/game/savestate", "/api/game/loadstate"] {
            let unauthenticated = Arc::new(FakeAuth {
                result: Err(AuthError::SignInRequired),
            });
            let (status, _body, value) = response(
                app(game.clone(), unauthenticated),
                request_without_content_type("POST", path, "{"),
            );
            assert_eq!(StatusCode::UNAUTHORIZED, status);
            assert_eq!(json!({"detail":"Sign in required"}), value);

            let unauthenticated = Arc::new(FakeAuth {
                result: Err(AuthError::SignInRequired),
            });
            let (status, _body, value) = response(
                app(game.clone(), unauthenticated),
                request_without_content_type("POST", path, ""),
            );
            assert_eq!(StatusCode::UNAUTHORIZED, status);
            assert_eq!(json!({"detail":"Sign in required"}), value);

            let cross_origin_request = HttpRequest::builder()
                .method("POST")
                .uri(path)
                .header(header::HOST, "desk.test")
                .header(header::ORIGIN, "https://foreign.test")
                .body(Body::from("{"))
                .unwrap();
            let (status, _body, value) =
                response(app(game.clone(), authorized()), cross_origin_request);
            assert_eq!(StatusCode::FORBIDDEN, status);
            assert_eq!(json!({"detail":"Cross-origin request refused"}), value);

            let (status, _body, value) = response(
                app(game.clone(), authorized()),
                request_without_content_type("POST", path, ""),
            );
            assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
            assert_eq!(
                json!({"detail":[{"type":"missing","loc":["body"],
                    "msg":"Field required","input":null}]}),
                value
            );
        }
    }

    #[test]
    fn empty_model_bodies_authenticate_before_missing_body_validation() {
        let game = Arc::new(FakeGame::new(Value::Null));
        for path in ["/api/game/savestate", "/api/game/loadstate"] {
            let unauthenticated = Arc::new(FakeAuth {
                result: Err(AuthError::SignInRequired),
            });
            let (status, _body, value) = response(
                app(game.clone(), unauthenticated),
                request("POST", path, ""),
            );
            assert_eq!(StatusCode::UNAUTHORIZED, status);
            assert_eq!(json!({"detail":"Sign in required"}), value);

            let (status, _body, value) = response(
                app(game.clone(), authorized()),
                request("POST", path, ""),
            );
            assert_eq!(StatusCode::UNPROCESSABLE_ENTITY, status);
            assert_eq!(
                json!({"detail":[{"type":"missing","loc":["body"],
                    "msg":"Field required","input":null}]}),
                value
            );
        }
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_states_requires_authentication_and_preserves_json_error_body() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let auth = Arc::new(FakeAuth {
            result: Err(AuthError::SignInRequired),
        });
        let (status, body, value) = response(
            app(game.clone(), auth),
            request("GET", "/api/game/states", ""),
        );
        assert_eq!(StatusCode::UNAUTHORIZED, status);
        assert_eq!(r#"{"detail":"Sign in required"}"#, body);
        assert_eq!(json!({"detail": "Sign in required"}), value);
        assert!(game.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn game_states_rejects_cross_origin_before_auth_or_backend() {
        let game = Arc::new(FakeGame::new(Value::Null));
        let mut request = request("GET", "/api/game/states", "");
        request
            .headers_mut()
            .insert(header::ORIGIN, "https://evil.example".parse().unwrap());
        let (status, body, value) = response(app(game.clone(), authorized()), request);
        assert_eq!(StatusCode::FORBIDDEN, status);
        assert_eq!(r#"{"detail":"Cross-origin request refused"}"#, body);
        assert_eq!(json!({"detail": "Cross-origin request refused"}), value);
        assert!(game.calls.lock().unwrap().is_empty());
    }
}
