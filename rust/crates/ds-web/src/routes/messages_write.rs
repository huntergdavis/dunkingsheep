//! Message creation, urgent-send, and brief routes from `web/app.py`.
//!
//! Persistence and transport stay behind the injected backend. The handlers own the
//! HTTP contract (validation, auth, status, and JSON shaping) and can be tested without
//! opening the desk database or contacting a pane.

use super::response_error::WebResponseError;

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    body::to_bytes,
    extract::{Extension, Path, Request},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Map, Number, Value};

use super::auth::{AuthBackend, AuthError, Identity, RequestScheme};

pub type MessagesWriteFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MessagesWriteError {
    Http { status: StatusCode, detail: String },
    Failure,
}

impl MessagesWriteError {
    fn into_response(self) -> Response {
        match self {
            Self::Http { status, detail } => detail_response(status, detail),
            Self::Failure => internal_error(),
        }
    }
}

/// Application operations for message routes. A runtime adapter owns message storage,
/// delivery locks, and the daemon client; tests use a deterministic fake.
pub trait MessagesWriteBackend: Send + Sync + 'static {
    fn post_message<'a>(
        &'a self,
        data: MessageIn,
        identity: Identity,
    ) -> MessagesWriteFuture<'a, Result<Value, MessagesWriteError>>;

    fn send_now<'a>(
        &'a self,
        message_id: Number,
    ) -> MessagesWriteFuture<'a, Result<Value, MessagesWriteError>>;

    fn post_brief<'a>(
        &'a self,
        data: BriefIn,
    ) -> MessagesWriteFuture<'a, Result<Value, MessagesWriteError>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessageIn {
    pub channel: String,
    pub body: String,
    pub attachment_ids: Vec<String>,
    pub external_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BriefIn {
    pub answers: Vec<Number>,
    pub channel: String,
    pub body: String,
    pub attachment_ids: Vec<String>,
}

/// Mount these three endpoints without registering them in the production router.
pub fn router(
    backend: Arc<dyn MessagesWriteBackend>,
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
) -> Router {
    Router::new()
        .route("/api/messages", post(post_message))
        .route("/api/messages/{message_id}/send-now", post(send_now))
        .route("/api/brief", post(post_brief))
        .layer(Extension(scheme))
        .layer(Extension(auth))
        .layer(Extension(backend))
}

async fn post_message(
    Extension(backend): Extension<Arc<dyn MessagesWriteBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    let body = match request_json(request).await {
        Ok(body) => body,
        Err(response) => return response.into_response(),
    };
    let identity = match authenticate(auth.as_ref(), &headers, scheme).await {
        Ok(identity) => identity,
        Err(response) => return response.into_response(),
    };
    let Some(body) = body else {
        return required_body_error();
    };
    let data = match message_input(&body) {
        Ok(data) => data,
        Err(response) => return response.into_response(),
    };
    match backend.post_message(data, identity).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn send_now(
    Extension(backend): Extension<Arc<dyn MessagesWriteBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    Path(message_id): Path<String>,
    headers: HeaderMap,
) -> Response {
    let identity = match authenticate(auth.as_ref(), &headers, scheme).await {
        Ok(identity) => identity,
        Err(response) => return response.into_response(),
    };
    let Some(message_id) = python_path_integer(&message_id) else {
        return path_integer_error(&message_id);
    };
    if identity != Identity::Owner {
        return detail_response(StatusCode::FORBIDDEN, "Owner sign-in required");
    }
    match backend.send_now(message_id).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn post_brief(
    Extension(backend): Extension<Arc<dyn MessagesWriteBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    let body = match request_json(request).await {
        Ok(body) => body,
        Err(response) => return response.into_response(),
    };
    let identity = match authenticate(auth.as_ref(), &headers, scheme).await {
        Ok(identity) => identity,
        Err(response) => return response.into_response(),
    };
    let Some(body) = body else {
        return required_body_error();
    };
    let data = match brief_input(&body) {
        Ok(data) => data,
        Err(response) => return response.into_response(),
    };
    if identity != Identity::Api {
        return detail_response(StatusCode::FORBIDDEN, "API token required");
    }
    match backend.post_brief(data).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

fn message_input(body: &Value) -> Result<MessageIn, WebResponseError> {
    let Some(object) = body.as_object() else {
        return Err(model_attributes_error(body).into());
    };
    let mut errors = Vec::new();
    let channel =
        string_field(object, "channel", body, true, 0, usize::MAX, &mut errors).unwrap_or_default();
    let text =
        string_field(object, "body", body, false, 0, 12_000, &mut errors).unwrap_or_default();
    let attachment_ids = string_list(object, "attachment_ids", 10, &mut errors);
    let external_id = optional_string(object, "external_id", body, 100, &mut errors);
    if !errors.is_empty() {
        return Err(validation_errors(errors).into());
    }
    Ok(MessageIn {
        channel,
        body: text.trim().to_owned(),
        attachment_ids,
        external_id,
    })
}

fn brief_input(body: &Value) -> Result<BriefIn, WebResponseError> {
    let Some(object) = body.as_object() else {
        return Err(model_attributes_error(body).into());
    };
    let mut errors = Vec::new();
    // Keep this order aligned with BriefIn in web/app.py: answers, channel, body,
    // attachment_ids. Pydantic reports every field error in declaration order.
    let answers = integer_list(object, "answers", 500, &mut errors);
    let channel =
        string_field(object, "channel", body, true, 0, usize::MAX, &mut errors).unwrap_or_default();
    let text = string_field(object, "body", body, true, 1, 20_000, &mut errors).unwrap_or_default();
    let attachment_ids = string_list(object, "attachment_ids", 10, &mut errors);
    if !errors.is_empty() {
        return Err(validation_errors(errors).into());
    }
    Ok(BriefIn {
        answers,
        channel,
        body: text.trim().to_owned(),
        attachment_ids,
    })
}

fn model_attributes_error(body: &Value) -> Response {
    validation_error(json!({
        "type": "model_attributes_type",
        "loc": ["body"],
        "msg": "Input should be a valid dictionary or object to extract fields from",
        "input": body
    }))
}

fn string_field(
    object: &Map<String, Value>,
    name: &str,
    body: &Value,
    required: bool,
    min: usize,
    max: usize,
    errors: &mut Vec<Value>,
) -> Option<String> {
    let Some(value) = object.get(name) else {
        if required {
            errors.push(missing_field_error(name, body));
        }
        return None;
    };
    let Some(text) = value.as_str() else {
        errors.push(json!({
            "type": "string_type", "loc": ["body", name],
            "msg": "Input should be a valid string", "input": value
        }));
        return None;
    };
    let length = text.chars().count();
    if length < min {
        errors.push(json!({
            "type": "string_too_short", "loc": ["body", name],
            "msg": format!("String should have at least {min} character"),
            "input": value, "ctx": {"min_length": min}
        }));
        return None;
    }
    if length > max {
        errors.push(json!({
            "type": "string_too_long", "loc": ["body", name],
            "msg": format!("String should have at most {max} characters"),
            "input": value, "ctx": {"max_length": max}
        }));
        return None;
    }
    Some(text.to_owned())
}

fn optional_string(
    object: &Map<String, Value>,
    name: &str,
    body: &Value,
    max: usize,
    errors: &mut Vec<Value>,
) -> Option<String> {
    match object.get(name) {
        None | Some(Value::Null) => None,
        Some(value) => string_field(
            &Map::from_iter([(name.to_owned(), value.clone())]),
            name,
            body,
            true,
            0,
            max,
            errors,
        ),
    }
}

fn string_list(
    object: &Map<String, Value>,
    name: &str,
    max: usize,
    errors: &mut Vec<Value>,
) -> Vec<String> {
    let Some(value) = object.get(name) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        errors.push(json!({
            "type": "list_type", "loc": ["body", name],
            "msg": "Input should be a valid list", "input": value
        }));
        return Vec::new();
    };
    if items.len() > max {
        errors.push(json!({
            "type": "too_long", "loc": ["body", name],
            "msg": format!("List should have at most {max} items after validation, not {}", items.len()),
            "input": value, "ctx": {"field_type": "List", "max_length": max, "actual_length": items.len()}
        }));
        return Vec::new();
    }
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| match item.as_str() {
            Some(text) => Some(text.to_owned()),
            None => {
                errors.push(json!({
                    "type": "string_type", "loc": ["body", name, index],
                    "msg": "Input should be a valid string", "input": item
                }));
                None
            }
        })
        .collect()
}

fn integer_list(
    object: &Map<String, Value>,
    name: &str,
    max: usize,
    errors: &mut Vec<Value>,
) -> Vec<Number> {
    let Some(value) = object.get(name) else {
        return Vec::new();
    };
    let Some(items) = value.as_array() else {
        errors.push(json!({
            "type": "list_type", "loc": ["body", name],
            "msg": "Input should be a valid list", "input": value
        }));
        return Vec::new();
    };
    if items.len() > max {
        errors.push(json!({
            "type": "too_long", "loc": ["body", name],
            "msg": format!("List should have at most {max} items after validation, not {}", items.len()),
            "input": value, "ctx": {"field_type": "List", "max_length": max, "actual_length": items.len()}
        }));
        return Vec::new();
    }
    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            let number = match item {
                Value::Number(number) if integer_number_lexeme(number) => Some(number),
                _ => None,
            };
            match number {
                Some(number) if positive_integer_lexeme(number) => Some(number.clone()),
                Some(_) => {
                    errors.push(json!({
                        "type": "greater_than", "loc": ["body", name, index],
                        "msg": "Input should be greater than 0", "input": item,
                        "ctx": {"gt": 0}
                    }));
                    None
                }
                None => {
                    errors.push(json!({
                        "type": "int_type", "loc": ["body", name, index],
                        "msg": "Input should be a valid integer", "input": item
                    }));
                    None
                }
            }
        })
        .collect()
}

/// Pydantic's strict `int` accepts only JSON integer tokens here. With
/// `serde_json/arbitrary_precision`, Number retains the token instead of
/// rounding or narrowing it through a machine integer or float.
fn integer_number_lexeme(number: &Number) -> bool {
    let text = number.to_string();
    let digits = text.strip_prefix('-').unwrap_or(&text);
    !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit())
}

fn positive_integer_lexeme(number: &Number) -> bool {
    let text = number.to_string();
    !text.starts_with('-')
        && text
            .bytes()
            .any(|byte| byte.is_ascii_digit() && byte != b'0')
}

/// Match the installed Pydantic path `int` coercion observed by the ASGI oracle:
/// whitespace/signs, digit separators, and zero-only decimal fractions are
/// accepted; exponent and fractional values are rejected.
fn python_path_integer(input: &str) -> Option<Number> {
    let input = input.trim();
    if input.is_empty() {
        return None;
    }
    let (negative, unsigned) = if let Some(rest) = input.strip_prefix('-') {
        (true, rest)
    } else if let Some(rest) = input.strip_prefix('+') {
        (false, rest)
    } else {
        (false, input)
    };
    let mut parts = unsigned.split('.');
    let whole = parts.next()?;
    let fraction = parts.next();
    if parts.next().is_some() {
        return None;
    }
    let whole = python_digit_segment(whole)?;
    if let Some(fraction) = fraction {
        let fraction = python_digit_segment(fraction)?;
        if !fraction.bytes().all(|byte| byte == b'0') {
            return None;
        }
    }
    let significant = whole.trim_start_matches('0');
    let magnitude = if significant.is_empty() {
        "0"
    } else {
        significant
    };
    let canonical = if negative && magnitude != "0" {
        format!("-{magnitude}")
    } else {
        magnitude.to_owned()
    };
    serde_json::from_str::<Number>(&canonical).ok()
}

fn python_digit_segment(input: &str) -> Option<String> {
    if input.is_empty() {
        return None;
    }
    let bytes = input.as_bytes();
    if !bytes[0].is_ascii_digit() || !bytes[bytes.len() - 1].is_ascii_digit() {
        return None;
    }
    let mut digits = String::with_capacity(input.len());
    let mut previous_was_digit = false;
    for &byte in bytes {
        match byte {
            b'0'..=b'9' => {
                digits.push(char::from(byte));
                previous_was_digit = true;
            }
            b'_' if previous_was_digit => previous_was_digit = false,
            _ => return None,
        }
    }
    if !previous_was_digit {
        return None;
    }
    Some(digits)
}

fn missing_field_error(name: &str, body: &Value) -> Value {
    json!({
        "type": "missing", "loc": ["body", name],
        "msg": "Field required", "input": body
    })
}

async fn request_json(request: Request) -> Result<Option<Value>, WebResponseError> {
    let is_json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(is_json_media_type);
    let bytes = to_bytes(request.into_body(), usize::MAX)
        .await
        .map_err(|_| {
            detail_response(
                StatusCode::UNPROCESSABLE_ENTITY,
                "There was an error parsing the body",
            )
        })?;
    if bytes.is_empty() {
        return Ok(None);
    }
    if !is_json {
        return Ok(Some(Value::String(
            String::from_utf8_lossy(&bytes).into_owned(),
        )));
    }
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| {
            let input = String::from_utf8_lossy(&bytes);
            (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({"detail": [{
                "type": "json_invalid", "loc": ["body", json_error_position(&input, &error)],
                "msg": "JSON decode error", "input": {}, "ctx": {"error": python_json_error(&error)}
            }]})),
        )
            .into_response()
        })
        .map_err(WebResponseError::from)
}

fn is_json_media_type(content_type: &str) -> bool {
    let media_type = content_type.split(';').next().unwrap_or_default().trim();
    let Some((main, subtype)) = media_type.split_once('/') else {
        return false;
    };
    main.trim().eq_ignore_ascii_case("application")
        && (subtype.trim().eq_ignore_ascii_case("json")
            || subtype.trim().to_ascii_lowercase().ends_with("+json"))
}

fn json_error_position(input: &str, error: &serde_json::Error) -> usize {
    let line_start = input
        .split_inclusive('\n')
        .take(error.line().saturating_sub(1))
        .map(str::len)
        .sum::<usize>();
    let byte_position = line_start
        .saturating_add(error.column().saturating_sub(1))
        .min(input.len());
    input[..byte_position].chars().count()
}

fn python_json_error(error: &serde_json::Error) -> String {
    let message = error.to_string();
    let message = message
        .split_once(" at line ")
        .map_or(message.as_str(), |(message, _)| message);
    match message {
        "expected value" => "Expecting value".to_owned(),
        "key must be a string" => "Expecting property name enclosed in double quotes".to_owned(),
        "expected `:`" | "expected ':'" => "Expecting ':' delimiter".to_owned(),
        "expected `,` or `}`"
        | "expected ',' or '}'"
        | "expected `,` or `]`"
        | "expected ',' or ']'" => "Expecting ',' delimiter".to_owned(),
        _ => message.to_owned(),
    }
}

async fn authenticate(
    auth: &dyn AuthBackend,
    headers: &HeaderMap,
    scheme: RequestScheme,
) -> Result<Identity, WebResponseError> {
    if let Some(origin) = headers
        .get_all(header::ORIGIN)
        .iter()
        .next()
        .map(header_latin1)
    {
        let host = headers
            .get_all(header::HOST)
            .iter()
            .next()
            .map(header_latin1)
            .unwrap_or_default();
        let scheme_text = match scheme {
            RequestScheme::Http => "http",
            RequestScheme::Https => "https",
        };
        if !origin.is_empty() && origin != format!("{scheme_text}://{host}") {
            return Err(
                detail_response(StatusCode::FORBIDDEN, "Cross-origin request refused").into(),
            );
        }
    }
    let authorization = headers
        .get_all(header::AUTHORIZATION)
        .iter()
        .next()
        .map(header_latin1);
    let bearer = authorization
        .as_deref()
        .and_then(|value| value.strip_prefix("Bearer "));
    let cookie = session_cookie(headers);
    auth.identity(bearer, cookie.as_deref())
        .await
        .map_err(auth_error_response)
        .map_err(WebResponseError::from)
}

fn header_latin1(value: &HeaderValue) -> String {
    value
        .as_bytes()
        .iter()
        .map(|byte| char::from(*byte))
        .collect()
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let mut session = None;
    for header in headers.get_all(header::COOKIE).iter() {
        let value = header_latin1(header);
        for part in value.split(';') {
            let (name, value) = part.split_once('=').unwrap_or(("", part));
            let name = name.trim();
            let value = value.trim();
            if name == "vp_session" {
                session = Some(python_cookie_unquote(value));
            }
        }
    }
    session
}

fn python_cookie_unquote(value: &str) -> String {
    let characters: Vec<char> = value.chars().collect();
    if characters.len() < 2 || characters[0] != '"' || characters[characters.len() - 1] != '"' {
        return value.to_owned();
    }
    let mut result = String::new();
    let mut index = 1;
    let end = characters.len() - 1;
    while index < end {
        if characters[index] == '\\' {
            index += 1;
            if index < end {
                result.push(characters[index]);
            }
        } else {
            result.push(characters[index]);
        }
        index += 1;
    }
    result
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

fn path_integer_error(input: &str) -> Response {
    validation_error(json!({
        "type": "int_parsing", "loc": ["path", "message_id"],
        "msg": "Input should be a valid integer, unable to parse string as an integer",
        "input": input
    }))
}

fn required_body_error() -> Response {
    validation_error(json!({
        "type": "missing", "loc": ["body"], "msg": "Field required", "input": null
    }))
}

fn validation_error(error: Value) -> Response {
    validation_errors(vec![error])
}

fn validation_errors(errors: Vec<Value>) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": errors})),
    )
        .into_response()
}

fn detail_response(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({"detail": detail.into()}))).into_response()
}

fn internal_error() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::pin,
        sync::Mutex,
        task::{Context, Poll, Waker},
    };

    use axum::{
        body::{to_bytes, Body},
        http::{header, Request},
    };
    use tower::ServiceExt;

    use super::*;

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

    #[derive(Default)]
    struct FakeAuth;

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> super::super::auth::AuthFuture<'a, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }
        fn identity<'a>(
            &'a self,
            bearer: Option<&'a str>,
            cookie: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<Identity, AuthError>> {
            Box::pin(async move {
                let valid_bearer = match bearer {
                    Some("owner-token") => Ok(Identity::Owner),
                    Some("api-token") => Ok(Identity::Api),
                    _ => Err(AuthError::SignInRequired),
                };
                match valid_bearer {
                    Ok(identity) => Ok(identity),
                    Err(_) => match cookie {
                        Some("owner-token") => Ok(Identity::Owner),
                        _ => Err(AuthError::SignInRequired),
                    },
                }
            })
        }
        fn logout<'a>(
            &'a self,
            _: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }
        fn change_password<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: Option<&'a str>,
        ) -> super::super::auth::AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Err(AuthError::SignInRequired) })
        }
    }

    #[derive(Default)]
    struct FakeMessages {
        calls: Mutex<Vec<String>>,
        error: Option<MessagesWriteError>,
    }

    impl MessagesWriteBackend for FakeMessages {
        fn post_message<'a>(
            &'a self,
            data: MessageIn,
            identity: Identity,
        ) -> MessagesWriteFuture<'a, Result<Value, MessagesWriteError>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("post:{identity:?}:{}:{}", data.channel, data.body));
            let result = match &self.error {
                Some(error) => Err(error.clone()),
                None => Ok(
                    json!({"id": 7, "channel": data.channel, "body": data.body, "delivery": "sent"}),
                ),
            };
            Box::pin(async move { result })
        }
        fn send_now<'a>(
            &'a self,
            message_id: Number,
        ) -> MessagesWriteFuture<'a, Result<Value, MessagesWriteError>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("send:{message_id}"));
            let result = match &self.error {
                Some(error) => Err(error.clone()),
                None => Ok(json!({"id": message_id, "delivery": "sent"})),
            };
            Box::pin(async move { result })
        }
        fn post_brief<'a>(
            &'a self,
            data: BriefIn,
        ) -> MessagesWriteFuture<'a, Result<Value, MessagesWriteError>> {
            self.calls.lock().unwrap().push(format!(
                "brief:{}:{}:[{}]",
                data.channel,
                data.body,
                data.answers
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            let result = match &self.error {
                Some(error) => Err(error.clone()),
                None => Ok(json!({"id": 9, "channel": data.channel, "body": data.body})),
            };
            Box::pin(async move { result })
        }
    }

    fn request(path: &str, body: &str, token: &str) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri(path)
            .header(header::HOST, "desk.test")
            .header(header::ORIGIN, "http://desk.test")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::AUTHORIZATION, format!("Bearer {token}"))
            .body(Body::from(body.to_owned()))
            .unwrap()
    }

    fn app(backend: Arc<FakeMessages>) -> Router {
        router(backend, Arc::new(FakeAuth), RequestScheme::Http)
    }

    fn response(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[test]
    fn post_message_validates_and_forwards_normalized_message() {
        let backend = Arc::new(FakeMessages::default());
        let result = block_on(app(backend.clone()).oneshot(request(
            "/api/messages",
            r#"{"channel":"pane-worker","body":"  hello  ","attachment_ids":["a1"],"external_id":"ext-1"}"#,
            "owner-token",
        )))
        .unwrap();
        let (status, body) = response(result);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"id": 7, "channel": "pane-worker", "body": "hello", "delivery": "sent"})
        );
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["post:Owner:pane-worker:hello"]
        );
    }

    #[test]
    fn post_message_accumulates_model_errors_in_declared_field_order() {
        let backend = Arc::new(FakeMessages::default());
        let long_body = "x".repeat(12_001);
        let payload = json!({
            "channel": 7,
            "body": long_body,
            "attachment_ids": [null]
        });
        let result = block_on(app(backend.clone()).oneshot(request(
            "/api/messages",
            &payload.to_string(),
            "owner-token",
        )))
        .unwrap();
        let (status, body) = response(result);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let errors = body["detail"].as_array().unwrap();
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0]["type"], "string_type");
        assert_eq!(errors[0]["loc"], json!(["body", "channel"]));
        assert_eq!(errors[1]["type"], "string_too_long");
        assert_eq!(errors[1]["loc"], json!(["body", "body"]));
        assert_eq!(errors[1]["ctx"]["max_length"], 12_000);
        assert_eq!(errors[2]["type"], "string_type");
        assert_eq!(errors[2]["loc"], json!(["body", "attachment_ids", 0]));
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn auth_dependency_runs_before_invalid_body_validation() {
        let backend = Arc::new(FakeMessages::default());
        let unauthorized = block_on(app(backend.clone()).oneshot(request(
            "/api/messages",
            r#"{"channel":7,"body":"ok"}"#,
            "expired-token",
        )))
        .unwrap();
        let (status, body) = response(unauthorized);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"detail": "Sign in required"}));
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn malformed_json_is_decoded_before_auth_dependency() {
        let backend = Arc::new(FakeMessages::default());
        let result = block_on(app(backend.clone()).oneshot(request(
            "/api/messages",
            "{invalid json",
            "expired-token",
        )))
        .unwrap();
        let (status, body) = response(result);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["detail"][0]["type"], "json_invalid");
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn post_message_rejects_non_utf8_origin_instead_of_treating_it_as_absent() {
        let backend = Arc::new(FakeMessages::default());
        let mut request = request(
            "/api/messages",
            r#"{"channel":"pane-worker","body":"hello"}"#,
            "owner-token",
        );
        request.headers_mut().insert(
            header::ORIGIN,
            HeaderValue::from_bytes(b"http://desk.test\xff").unwrap(),
        );
        let result = block_on(app(backend.clone()).oneshot(request)).unwrap();
        let (status, body) = response(result);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "Cross-origin request refused"}));
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn post_message_cookie_uses_all_headers_last_value_and_python_unquote() {
        let backend = Arc::new(FakeMessages::default());
        let cookie_cases: [&[&str]; 3] = [
            &["vp_session=expired; vp_session=\"owner-token\""],
            &["vp_session=expired; vp_session=\"owner\\-token\""],
            &["vp_session=expired", "vp_session=owner-token"],
        ];
        for cookie_headers in cookie_cases {
            let mut request = request(
                "/api/messages",
                r#"{"channel":"pane-worker","body":"hello"}"#,
                "expired-token",
            );
            for value in cookie_headers {
                request
                    .headers_mut()
                    .append(header::COOKIE, HeaderValue::from_static(value));
            }
            let result = block_on(app(backend.clone()).oneshot(request)).unwrap();
            let (status, _) = response(result);
            assert_eq!(status, StatusCode::OK);
        }
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec![
                "post:Owner:pane-worker:hello",
                "post:Owner:pane-worker:hello",
                "post:Owner:pane-worker:hello"
            ]
        );
    }

    #[test]
    fn post_message_prefers_valid_bearer_and_falls_back_to_cookie() {
        let backend = Arc::new(FakeMessages::default());
        for token in ["api-token", "expired-token"] {
            let mut request = request(
                "/api/messages",
                r#"{"channel":"pane-worker","body":"hello"}"#,
                token,
            );
            request.headers_mut().append(
                header::COOKIE,
                HeaderValue::from_static("vp_session=owner-token"),
            );
            let result = block_on(app(backend.clone()).oneshot(request)).unwrap();
            let (status, _) = response(result);
            assert_eq!(status, StatusCode::OK);
        }
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["post:Api:pane-worker:hello", "post:Owner:pane-worker:hello"]
        );
    }

    #[test]
    fn post_message_rejects_empty_text_without_attachments() {
        let backend = Arc::new(FakeMessages {
            error: Some(MessagesWriteError::Http {
                status: StatusCode::UNPROCESSABLE_ENTITY,
                detail: "Message is empty".into(),
            }),
            ..FakeMessages::default()
        });
        // The source rejects this after channel lookup; the route backend provides that
        // lookup and its error remains an ordinary 422 detail response.
        let result = block_on(app(backend.clone()).oneshot(request(
            "/api/messages",
            r#"{"channel":"pane-worker","body":" "}"#,
            "owner-token",
        )))
        .unwrap();
        let (status, body) = response(result);
        // Whitespace passes model validation; the backend applies `_post_message`'s
        // channel lookup and application-level empty check before persistence.
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body, json!({"detail": "Message is empty"}));
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["post:Owner:pane-worker:"]
        );
    }

    #[test]
    fn send_now_requires_owner_and_returns_backend_message() {
        let backend = Arc::new(FakeMessages::default());
        let rejected = block_on(app(backend.clone()).oneshot(request(
            "/api/messages/12/send-now",
            "",
            "api-token",
        )))
        .unwrap();
        let (status, body) = response(rejected);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "Owner sign-in required"}));
        assert!(backend.calls.lock().unwrap().is_empty());

        let accepted = block_on(app(backend.clone()).oneshot(request(
            "/api/messages/12/send-now",
            "",
            "owner-token",
        )))
        .unwrap();
        let (status, body) = response(accepted);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"id": 12, "delivery": "sent"}));
        assert_eq!(*backend.calls.lock().unwrap(), vec!["send:12"]);
    }

    #[test]
    fn send_now_validates_path_before_endpoint_role_check() {
        let backend = Arc::new(FakeMessages::default());
        let result = block_on(app(backend.clone()).oneshot(request(
            "/api/messages/not-an-integer/send-now",
            "",
            "api-token",
        )))
        .unwrap();
        let (status, body) = response(result);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["detail"][0]["type"], "int_parsing");
        assert_eq!(body["detail"][0]["loc"], json!(["path", "message_id"]));
        assert!(backend.calls.lock().unwrap().is_empty());

        let unauthenticated = block_on(app(backend.clone()).oneshot(request(
            "/api/messages/not-an-integer/send-now",
            "",
            "expired-token",
        )))
        .unwrap();
        let (status, body) = response(unauthenticated);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"detail": "Sign in required"}));
    }

    #[test]
    fn send_now_preserves_python_sized_path_integer_before_role_check() {
        let backend = Arc::new(FakeMessages::default());
        let huge = "1267650600228229401496703205376";
        let rejected = block_on(app(backend.clone()).oneshot(request(
            &format!("/api/messages/{huge}/send-now"),
            "",
            "api-token",
        )))
        .unwrap();
        let (status, body) = response(rejected);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "Owner sign-in required"}));
        assert!(backend.calls.lock().unwrap().is_empty());

        let accepted = block_on(app(backend.clone()).oneshot(request(
            &format!("/api/messages/{huge}/send-now"),
            "",
            "owner-token",
        )))
        .unwrap();
        let (status, body) = response(accepted);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"].to_string(), huge);
        assert_eq!(*backend.calls.lock().unwrap(), vec![format!("send:{huge}")]);
    }

    #[test]
    fn send_now_path_integer_matches_python_coercion_without_float_rounding() {
        for (input, expected) in [
            ("+7", "7"),
            ("007", "7"),
            ("1_0", "10"),
            ("1.0", "1"),
            ("1_0.00", "10"),
            ("-0.0", "0"),
        ] {
            assert_eq!(
                python_path_integer(input).map(|number| number.to_string()),
                Some(expected.to_owned()),
                "input {input:?}"
            );
        }
        for input in ["1.5", "1e2", "1.", ".0", "1__0", "_10", "10_"] {
            assert_eq!(python_path_integer(input), None, "input {input:?}");
        }
    }

    #[test]
    fn send_now_preserves_backend_http_error() {
        let backend = Arc::new(FakeMessages {
            error: Some(MessagesWriteError::Http {
                status: StatusCode::CONFLICT,
                detail: "Message is being recovered; try again shortly".into(),
            }),
            ..FakeMessages::default()
        });
        let result =
            block_on(app(backend).oneshot(request("/api/messages/12/send-now", "", "owner-token")))
                .unwrap();
        let (status, body) = response(result);
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(
            body,
            json!({"detail": "Message is being recovered; try again shortly"})
        );
    }

    #[test]
    fn post_brief_requires_api_identity_and_forwards_answers() {
        let backend = Arc::new(FakeMessages::default());
        let rejected = block_on(app(backend.clone()).oneshot(request(
            "/api/brief",
            r#"{"channel":"pane-worker","body":"done","answers":[4]}"#,
            "owner-token",
        )))
        .unwrap();
        let (status, body) = response(rejected);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "API token required"}));
        assert!(backend.calls.lock().unwrap().is_empty());

        let accepted = block_on(app(backend.clone()).oneshot(request(
            "/api/brief",
            r#"{"channel":"pane-worker","body":"  done  ","answers":[4]}"#,
            "api-token",
        )))
        .unwrap();
        let (status, body) = response(accepted);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"id": 9, "channel": "pane-worker", "body": "done"})
        );
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["brief:pane-worker:done:[4]"]
        );
    }

    #[test]
    fn post_brief_validates_model_before_endpoint_role_check() {
        let backend = Arc::new(FakeMessages::default());
        let result = block_on(app(backend.clone()).oneshot(request(
            "/api/brief",
            r#"{"answers":["bad"],"channel":7,"body":""}"#,
            "owner-token",
        )))
        .unwrap();
        let (status, body) = response(result);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        let errors = body["detail"].as_array().unwrap();
        assert_eq!(errors.len(), 3);
        assert_eq!(errors[0]["type"], "int_type");
        assert_eq!(errors[0]["loc"], json!(["body", "answers", 0]));
        assert_eq!(errors[1]["type"], "string_type");
        assert_eq!(errors[1]["loc"], json!(["body", "channel"]));
        assert_eq!(errors[2]["type"], "string_too_short");
        assert_eq!(errors[2]["loc"], json!(["body", "body"]));
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn post_brief_preserves_python_sized_strict_integer_answers() {
        let backend = Arc::new(FakeMessages::default());
        let huge = "1267650600228229401496703205376";
        let rejected = block_on(app(backend.clone()).oneshot(request(
            "/api/brief",
            &format!(r#"{{"channel":"pane-worker","body":"done","answers":[{huge}]}}"#),
            "owner-token",
        )))
        .unwrap();
        let (status, body) = response(rejected);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "API token required"}));
        assert!(backend.calls.lock().unwrap().is_empty());

        let accepted = block_on(app(backend.clone()).oneshot(request(
            "/api/brief",
            &format!(r#"{{"channel":"pane-worker","body":"done","answers":[{huge}]}}"#),
            "api-token",
        )))
        .unwrap();
        let (status, _) = response(accepted);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec![format!("brief:pane-worker:done:[{huge}]")]
        );
    }

    #[test]
    fn post_brief_enforces_strict_positive_integer_token_types() {
        let backend = Arc::new(FakeMessages::default());
        let huge_negative = "-1267650600228229401496703205376";
        for (token, expected_type) in [
            ("1.0", "int_type"),
            ("1e0", "int_type"),
            ("true", "int_type"),
            ("\"1\"", "int_type"),
            (huge_negative, "greater_than"),
        ] {
            let result = block_on(app(backend.clone()).oneshot(request(
                "/api/brief",
                &format!(r#"{{"channel":"pane-worker","body":"done","answers":[{token}]}}"#),
                "api-token",
            )))
            .unwrap();
            let (status, body) = response(result);
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "token {token:?}");
            assert_eq!(body["detail"][0]["type"], expected_type, "token {token:?}");
            assert_eq!(body["detail"][0]["loc"], json!(["body", "answers", 0]));
        }
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn brief_requires_nonempty_body_and_positive_integer_answers() {
        let backend = Arc::new(FakeMessages::default());
        let empty = block_on(app(backend.clone()).oneshot(request(
            "/api/brief",
            r#"{"channel":"pane-worker","body":""}"#,
            "api-token",
        )))
        .unwrap();
        assert_eq!(response(empty).0, StatusCode::UNPROCESSABLE_ENTITY);
        let invalid_answer = block_on(app(backend.clone()).oneshot(request(
            "/api/brief",
            r#"{"channel":"pane-worker","body":"ok","answers":[0]}"#,
            "api-token",
        )))
        .unwrap();
        assert_eq!(response(invalid_answer).0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn unauthenticated_message_request_returns_sign_in_error() {
        let backend = Arc::new(FakeMessages::default());
        let result = block_on(app(backend.clone()).oneshot(request(
            "/api/messages",
            r#"{"channel":"pane-worker"}"#,
            "missing-token",
        )))
        .unwrap();
        let (status, body) = response(result);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"detail": "Sign in required"}));
        assert!(backend.calls.lock().unwrap().is_empty());
    }
}
