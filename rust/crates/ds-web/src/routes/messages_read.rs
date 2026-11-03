//! Read-only message routes ported from `web/app.py`.
//!
//! The route layer owns authentication, query validation, and HTTP responses.
//! The injected backend owns the application database and the transport
//! reconciliation used by the Python handlers.

use std::{collections::HashMap, future::Future, pin::Pin, sync::Arc};

use axum::{
    extract::{Extension, Request},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};

use super::auth::{AuthBackend, AuthError, Identity, RequestScheme};

pub type MessagesReadFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, MessagesReadError>> + Send + 'a>>;
pub type MessageContextFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Option<Value>, MessagesReadError>> + Send + 'a>>;

/// Parsed Python `int` values outside SQLite's signed 64-bit range remain
/// distinguishable so the backend can preserve SQLite's overflow failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryId {
    Integer(i64),
    OutOfRange { negative: bool },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PythonInt {
    Value(i64),
    OutOfRange { negative: bool },
}

/// Database/client boundary for the read routes. Implementations perform the
/// same reconciliation and SQL reads as `web.app.messages`, `deliveries`, and
/// `message_context`; tests inject deterministic results without live state.
pub trait MessagesReadBackend: Send + Sync + 'static {
    fn known_channel(&self, channel: &str) -> bool;

    fn messages<'a>(&'a self, channel: &'a str, before: i64, limit: i64) -> MessagesReadFuture<'a>;

    fn deliveries<'a>(&'a self, channel: &'a str, ids: &'a [DeliveryId]) -> MessagesReadFuture<'a>;

    fn message_context(&self, message_id: i64) -> MessageContextFuture<'_>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MessagesReadError;

pub fn router() -> Router {
    router_with_scheme(RequestScheme::Http)
}

pub fn router_with_scheme(scheme: RequestScheme) -> Router {
    Router::new()
        .route("/api/messages", get(messages))
        .route("/api/deliveries", get(deliveries))
        .route("/api/messages/{message_id}/context", get(message_context))
        .layer(Extension(scheme))
}

async fn messages(
    Extension(backend): Extension<Arc<dyn MessagesReadBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let query = match parse_query(request.uri().query()) {
        Ok(query) => query,
        Err(()) => return detail_response(StatusCode::BAD_REQUEST, "invalid query"),
    };
    let mut validation_errors = Vec::new();
    let channel = query.get("channel");
    if channel.is_none() {
        validation_errors.push(missing_query_error("channel"));
    }
    let before = match query.get("before") {
        Some(value) => match parse_python_int(value) {
            Some(value) => Some(value),
            None => {
                validation_errors.push(invalid_integer_error("query", "before", value));
                None
            }
        },
        None => Some(PythonInt::Value(i64::MAX)),
    };
    let limit = match query.get("limit") {
        Some(value) => match parse_python_int(value) {
            Some(value) => Some(value),
            None => {
                validation_errors.push(invalid_integer_error("query", "limit", value));
                None
            }
        },
        None => Some(PythonInt::Value(50)),
    }
    .map(|value| match value {
        PythonInt::Value(value) => value.clamp(1, 100),
        PythonInt::OutOfRange { negative: true } => 1,
        PythonInt::OutOfRange { negative: false } => 100,
    });
    if !validation_errors.is_empty() {
        return validation_error_response(validation_errors);
    }
    let channel = channel.expect("required channel was validated");
    let limit = limit.expect("valid limit value or default");
    if !backend.known_channel(channel) {
        return detail_response(StatusCode::NOT_FOUND, "Unknown channel");
    }
    let before = match before.expect("valid before value or default") {
        PythonInt::Value(0) => i64::MAX,
        PythonInt::Value(value) => value,
        PythonInt::OutOfRange { .. } => return internal_error_response(),
    };
    match backend.messages(channel, before, limit).await {
        Ok(result) => Json(result).into_response(),
        Err(_) => internal_error_response(),
    }
}

async fn deliveries(
    Extension(backend): Extension<Arc<dyn MessagesReadBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let query = match parse_query(request.uri().query()) {
        Ok(query) => query,
        Err(()) => return detail_response(StatusCode::BAD_REQUEST, "invalid query"),
    };
    let mut validation_errors = Vec::new();
    let channel = query.get("channel");
    let raw_ids = query.get("ids");
    if channel.is_none() {
        validation_errors.push(missing_query_error("channel"));
    }
    if raw_ids.is_none() {
        validation_errors.push(missing_query_error("ids"));
    }
    if !validation_errors.is_empty() {
        return validation_error_response(validation_errors);
    }
    let channel = channel.expect("required channel was validated");
    let raw_ids = raw_ids.expect("required ids were validated");
    if !backend.known_channel(channel) {
        return detail_response(StatusCode::NOT_FOUND, "Unknown channel");
    }
    let ids = match parse_message_ids(raw_ids) {
        Ok(ids) => ids,
        Err(()) => return detail_response(StatusCode::UNPROCESSABLE_ENTITY, "Invalid message IDs"),
    };
    if ids.len() > 100
        || ids.iter().any(|id| match id {
            DeliveryId::Integer(value) => *value < 1,
            DeliveryId::OutOfRange { negative } => *negative,
        })
    {
        return detail_response(
            StatusCode::UNPROCESSABLE_ENTITY,
            "Too many or invalid message IDs",
        );
    }
    match backend.deliveries(channel, &ids).await {
        Ok(result) => Json(result).into_response(),
        Err(_) => internal_error_response(),
    }
}

async fn message_context(
    Extension(backend): Extension<Arc<dyn MessagesReadBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let raw_id = request
        .uri()
        .path()
        .strip_prefix("/api/messages/")
        .and_then(|suffix| suffix.strip_suffix("/context"))
        .unwrap_or_default();
    let decoded_id = decode_path_component(raw_id);
    let message_id = match parse_python_int(&decoded_id) {
        Some(PythonInt::Value(value)) => value,
        Some(PythonInt::OutOfRange { .. }) => return internal_error_response(),
        None => return invalid_integer("path", "message_id", &decoded_id),
    };
    match backend.message_context(message_id).await {
        Ok(Some(result)) => Json(result).into_response(),
        Ok(None) => detail_response(StatusCode::NOT_FOUND, "Message not found"),
        Err(_) => internal_error_response(),
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
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let cookie = cookie_value(headers, "vp_session");
    backend
        .identity(bearer, cookie.as_deref())
        .await
        .map_err(auth_error_response)
}

fn same_origin_refusal(headers: &HeaderMap, scheme: RequestScheme) -> Option<Response> {
    let origin = headers.get(header::ORIGIN)?;
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let scheme = match scheme {
        RequestScheme::Http => "http",
        RequestScheme::Https => "https",
    };
    let expected = format!("{scheme}://{host}");
    (origin.to_str().ok() != Some(expected.as_str()))
        .then(|| detail_response(StatusCode::FORBIDDEN, "Cross-origin request refused"))
}

fn decode_path_component(component: &str) -> String {
    fn hex(byte: u8) -> Option<u8> {
        match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            b'A'..=b'F' => Some(byte - b'A' + 10),
            _ => None,
        }
    }

    let bytes = component.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push((high << 4) | low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key.trim() == name).then(|| value.trim().to_owned())
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
        AuthError::Internal(_) => internal_error_response(),
    }
}

fn detail_response(status: StatusCode, detail: impl Into<String>) -> Response {
    (status, Json(json!({"detail": detail.into()}))).into_response()
}

fn missing_query_error(field: &'static str) -> Value {
    json!({
        "type": "missing",
        "loc": ["query", field],
        "msg": "Field required",
        "input": null
    })
}

fn invalid_integer_error(location: &'static str, field: &'static str, input: &str) -> Value {
    json!({
        "type": "int_parsing",
        "loc": [location, field],
        "msg": "Input should be a valid integer, unable to parse string as an integer",
        "input": input
    })
}

fn validation_error_response(errors: Vec<Value>) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": errors})),
    )
        .into_response()
}

fn invalid_integer(location: &'static str, field: &'static str, input: &str) -> Response {
    validation_error_response(vec![invalid_integer_error(location, field, input)])
}

fn internal_error_response() -> Response {
    (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error").into_response()
}

fn parse_query(raw: Option<&str>) -> Result<HashMap<String, String>, ()> {
    fn decode(component: &str) -> String {
        fn hex(byte: u8) -> Option<u8> {
            match byte {
                b'0'..=b'9' => Some(byte - b'0'),
                b'a'..=b'f' => Some(byte - b'a' + 10),
                b'A'..=b'F' => Some(byte - b'A' + 10),
                _ => None,
            }
        }
        let bytes = component.as_bytes();
        let mut output = Vec::with_capacity(bytes.len());
        let mut index = 0;
        while index < bytes.len() {
            match bytes[index] {
                b'+' => {
                    output.push(b' ');
                    index += 1;
                }
                b'%' if index + 2 < bytes.len() => {
                    match (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                        (Some(high), Some(low)) => {
                            output.push((high << 4) | low);
                            index += 3;
                        }
                        _ => {
                            output.push(b'%');
                            index += 1;
                        }
                    }
                }
                byte => {
                    output.push(byte);
                    index += 1;
                }
            }
        }
        String::from_utf8_lossy(&output).into_owned()
    }

    let mut query = HashMap::new();
    for pair in raw
        .unwrap_or_default()
        .split('&')
        .filter(|part| !part.is_empty())
    {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(decode(key), decode(value));
    }
    Ok(query)
}

fn parse_message_ids(raw: &str) -> Result<Vec<DeliveryId>, ()> {
    raw.split(',')
        .filter(|part| !part.is_empty())
        .map(parse_builtin_int)
        .collect::<Option<Vec<_>>>()
        .ok_or(())
}

fn parse_builtin_int(raw: &str) -> Option<DeliveryId> {
    const DECIMAL_ZEROES: &[u32] = &[
        0x0030, 0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66,
        0x0CE6, 0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946,
        0x19D0, 0x1A80, 0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0,
        0xA9F0, 0xAA50, 0xABF0, 0xFF10, 0x104A0, 0x10D30, 0x10D40, 0x11066, 0x110F0, 0x11136,
        0x111D0, 0x112F0, 0x11450, 0x114D0, 0x11650, 0x116C0, 0x116D0, 0x116DA, 0x11730, 0x118E0,
        0x11950, 0x11BF0, 0x11C50, 0x11D50, 0x11DA0, 0x11F50, 0x16130, 0x16A60, 0x16AC0, 0x16B50,
        0x16D70, 0x1CCF0, 0x1D7CE, 0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0,
        0x1E5F1, 0x1E950, 0x1FBF0,
    ];

    fn decimal_digit(character: char) -> Option<u8> {
        let codepoint = u32::from(character);
        DECIMAL_ZEROES.iter().find_map(|zero| {
            (codepoint >= *zero && codepoint < *zero + 10).then(|| (codepoint - *zero) as u8)
        })
    }

    let value = raw.trim();
    if value.chars().count() > 8_601 {
        return None;
    }
    let (negative, digits) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    let characters: Vec<char> = digits.chars().collect();
    let mut normalized = String::with_capacity(digits.len());
    let mut digit_count = 0;
    for (index, character) in characters.iter().copied().enumerate() {
        if character == '_' {
            if index == 0
                || index + 1 == characters.len()
                || decimal_digit(characters[index - 1]).is_none()
                || decimal_digit(characters[index + 1]).is_none()
            {
                return None;
            }
            continue;
        }
        normalized.push(char::from(b'0' + decimal_digit(character)?));
        digit_count += 1;
        if digit_count > 4_300 {
            return None;
        }
    }
    if normalized.is_empty() {
        return None;
    }
    let signed = if negative {
        format!("-{normalized}")
    } else {
        normalized
    };
    Some(match signed.parse::<i64>() {
        Ok(value) => DeliveryId::Integer(value),
        Err(_) => DeliveryId::OutOfRange { negative },
    })
}

/// Pydantic's query integer conversion accepts whitespace, signs, underscores,
/// and decimal strings with an all-zero fractional part.
fn parse_python_int(raw: &str) -> Option<PythonInt> {
    let value = raw.trim();
    let (negative, digits) = match value.as_bytes().first() {
        Some(b'-') => (true, &value[1..]),
        Some(b'+') => (false, &value[1..]),
        _ => (false, value),
    };
    if digits.is_empty()
        || digits.len() > 8_600
        || digits.bytes().filter(u8::is_ascii_digit).count() > 4_300
    {
        return None;
    }
    fn valid_sequence(value: &str) -> bool {
        let bytes = value.as_bytes();
        !bytes.is_empty()
            && bytes.iter().enumerate().all(|(index, byte)| match byte {
                b'0'..=b'9' => true,
                b'_' => {
                    index > 0
                        && index + 1 < bytes.len()
                        && bytes[index - 1].is_ascii_digit()
                        && bytes[index + 1].is_ascii_digit()
                }
                _ => false,
            })
    }
    let integer_part = match digits.split_once('.') {
        Some((whole, fraction)) => {
            if !valid_sequence(whole)
                || !valid_sequence(fraction)
                || fraction.bytes().any(|byte| byte != b'0')
            {
                return None;
            }
            whole
        }
        None => digits,
    };
    if !valid_sequence(integer_part) {
        return None;
    }
    let normalized: String = integer_part
        .chars()
        .filter(|character| *character != '_')
        .collect();
    let signed = if negative {
        format!("-{normalized}")
    } else {
        normalized
    };
    Some(match signed.parse::<i64>() {
        Ok(value) => PythonInt::Value(value),
        Err(_) => PythonInt::OutOfRange { negative },
    })
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        sync::{Arc, Mutex},
        task::{Context, Poll, Wake, Waker},
    };

    use axum::{
        body::{to_bytes, Body},
        http::StatusCode,
    };
    use tower::ServiceExt;

    use super::*;
    use crate::routes::auth::AuthFuture;

    #[derive(Default)]
    struct FakeBackend {
        calls: Mutex<Vec<String>>,
        fail: bool,
        context: Option<Value>,
    }

    impl MessagesReadBackend for FakeBackend {
        fn known_channel(&self, channel: &str) -> bool {
            channel == "cos" || channel == "pane-writer"
        }

        fn messages<'a>(
            &'a self,
            channel: &'a str,
            before: i64,
            limit: i64,
        ) -> MessagesReadFuture<'a> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("messages:{channel}:{before}:{limit}"));
            Box::pin(async move {
                if self.fail {
                    Err(MessagesReadError)
                } else {
                    Ok(json!({
                        "items": [{"id": 7, "channel": channel, "role": "owner", "body": "hi", "attachments": []}],
                        "has_more": false,
                        "last_delivery": null,
                        "next_before": 7
                    }))
                }
            })
        }

        fn deliveries<'a>(
            &'a self,
            channel: &'a str,
            ids: &'a [DeliveryId],
        ) -> MessagesReadFuture<'a> {
            self.calls.lock().unwrap().push(format!(
                "deliveries:{channel}:{}",
                ids.iter()
                    .map(|id| match id {
                        DeliveryId::Integer(value) => value.to_string(),
                        DeliveryId::OutOfRange { negative: true } => "negative-overflow".into(),
                        DeliveryId::OutOfRange { negative: false } => "positive-overflow".into(),
                    })
                    .collect::<Vec<_>>()
                    .join(",")
            ));
            Box::pin(async move {
                if self.fail
                    || ids
                        .iter()
                        .any(|id| matches!(id, DeliveryId::OutOfRange { negative: false }))
                {
                    Err(MessagesReadError)
                } else if ids.is_empty() {
                    Ok(json!({"deliveries": {}}))
                } else {
                    Ok(
                        json!({"deliveries": {"7": {"delivery": "delivered", "delivery_detail": ""}}}),
                    )
                }
            })
        }

        fn message_context(&self, message_id: i64) -> MessageContextFuture<'_> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("context:{message_id}"));
            Box::pin(async move {
                if self.fail {
                    Err(MessagesReadError)
                } else {
                    Ok(self.context.clone())
                }
            })
        }
    }

    struct FakeAuth {
        identity: Result<Identity, AuthError>,
    }

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
        ) -> AuthFuture<'a, Result<String, AuthError>> {
            Box::pin(async { Err(AuthError::Internal("unused".into())) })
        }

        fn identity<'a>(
            &'a self,
            _: Option<&'a str>,
            _: Option<&'a str>,
        ) -> AuthFuture<'a, Result<Identity, AuthError>> {
            Box::pin(async move { self.identity.clone() })
        }

        fn logout<'a>(&'a self, _: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Ok(()) })
        }

        fn change_password<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: Option<&'a str>,
        ) -> AuthFuture<'a, Result<(), AuthError>> {
            Box::pin(async { Ok(()) })
        }
    }

    fn app(backend: Arc<FakeBackend>, auth: Result<Identity, AuthError>) -> Router {
        Router::new()
            .merge(router())
            .layer(Extension(
                Arc::new(FakeAuth { identity: auth }) as Arc<dyn AuthBackend>
            ))
            .layer(Extension(backend as Arc<dyn MessagesReadBackend>))
    }

    fn request(uri: &str) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .header("host", "desk.test")
            .body(Body::empty())
            .unwrap()
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        struct NoopWake;
        impl Wake for NoopWake {
            fn wake(self: Arc<Self>) {}
        }
        let waker = Waker::from(Arc::new(NoopWake));
        let mut context = Context::from_waker(&waker);
        let mut future = std::pin::pin!(future);
        loop {
            match future.as_mut().poll(&mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn response_json(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let body = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        (status, serde_json::from_slice(&body).unwrap())
    }

    fn authorized() -> Result<Identity, AuthError> {
        Ok(Identity::Owner)
    }

    #[test]
    fn messages_matches_python_shape_and_clamps_limit() {
        let backend = Arc::new(FakeBackend::default());
        let response = block_on(
            app(backend.clone(), authorized())
                .oneshot(request("/api/messages?channel=cos&before=0&limit=500")),
        )
        .unwrap();
        let (status, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"][0]["id"], 7);
        assert_eq!(body["items"][0]["attachments"], json!([]));
        assert_eq!(body["has_more"], false);
        assert_eq!(body["last_delivery"], Value::Null);
        assert_eq!(body["next_before"], 7);
        assert_eq!(
            backend.calls.lock().unwrap().as_slice(),
            ["messages:cos:9223372036854775807:100"]
        );
    }

    #[test]
    fn messages_rejects_unknown_channel_and_invalid_integer_like_fastapi() {
        let backend = Arc::new(FakeBackend::default());
        let unknown = block_on(
            app(backend.clone(), authorized()).oneshot(request("/api/messages?channel=unknown")),
        )
        .unwrap();
        let (status, body) = response_json(unknown);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({"detail": "Unknown channel"}));

        let invalid = block_on(
            app(backend.clone(), authorized())
                .oneshot(request("/api/messages?channel=cos&limit=nope")),
        )
        .unwrap();
        let (status, body) = response_json(invalid);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["detail"][0]["type"], "int_parsing");
        assert_eq!(body["detail"][0]["loc"], json!(["query", "limit"]));
        assert_eq!(body["detail"][0]["input"], "nope");
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn messages_aggregates_query_validation_errors() {
        let backend = Arc::new(FakeBackend::default());
        let response = block_on(
            app(backend.clone(), authorized())
                .oneshot(request("/api/messages?before=nope&limit=bogus")),
        )
        .unwrap();
        let (status, body) = response_json(response);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body["detail"],
            json!([
                {"type":"missing", "loc":["query", "channel"], "msg":"Field required", "input":null},
                {"type":"int_parsing", "loc":["query", "before"], "msg":"Input should be a valid integer, unable to parse string as an integer", "input":"nope"},
                {"type":"int_parsing", "loc":["query", "limit"], "msg":"Input should be a valid integer, unable to parse string as an integer", "input":"bogus"}
            ])
        );
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn deliveries_aggregates_missing_query_errors() {
        let backend = Arc::new(FakeBackend::default());
        let response =
            block_on(app(backend.clone(), authorized()).oneshot(request("/api/deliveries")))
                .unwrap();
        let (status, body) = response_json(response);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body["detail"],
            json!([
                {"type":"missing", "loc":["query", "channel"], "msg":"Field required", "input":null},
                {"type":"missing", "loc":["query", "ids"], "msg":"Field required", "input":null}
            ])
        );
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn deliveries_parses_ids_and_returns_python_shape() {
        let backend = Arc::new(FakeBackend::default());
        let response = block_on(
            app(backend.clone(), authorized())
                .oneshot(request("/api/deliveries?channel=cos&ids=%207%2C%2B8%2C")),
        )
        .unwrap();
        let (status, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deliveries"]["7"]["delivery"], "delivered");
        assert_eq!(
            backend.calls.lock().unwrap().as_slice(),
            ["deliveries:cos:7,8"]
        );
    }

    #[test]
    fn deliveries_handles_empty_and_invalid_ids_without_backend_reads() {
        let backend = Arc::new(FakeBackend::default());
        let empty = block_on(
            app(backend.clone(), authorized())
                .oneshot(request("/api/deliveries?channel=cos&ids=,,")),
        )
        .unwrap();
        let (status, body) = response_json(empty);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"deliveries": {}}));
        assert_eq!(
            backend.calls.lock().unwrap().as_slice(),
            ["deliveries:cos:"]
        );

        let invalid = block_on(
            app(backend.clone(), authorized())
                .oneshot(request("/api/deliveries?channel=cos&ids=7,nope")),
        )
        .unwrap();
        let (status, body) = response_json(invalid);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body, json!({"detail": "Invalid message IDs"}));
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn deliveries_matches_python_integer_digits_and_overflow() {
        let backend = Arc::new(FakeBackend::default());
        let unicode = block_on(
            app(backend.clone(), authorized())
                .oneshot(request("/api/deliveries?channel=cos&ids=%D9%A7")),
        )
        .unwrap();
        let (status, body) = response_json(unicode);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["deliveries"]["7"]["delivery"], "delivered");
        assert_eq!(
            backend.calls.lock().unwrap().as_slice(),
            ["deliveries:cos:7"]
        );

        let overflow = block_on(app(backend.clone(), authorized()).oneshot(request(
            "/api/deliveries?channel=cos&ids=9223372036854775808",
        )))
        .unwrap();
        let bytes = block_on(to_bytes(overflow.into_body(), usize::MAX)).unwrap();
        assert_eq!(bytes.as_ref(), b"Internal Server Error");
        assert_eq!(
            backend.calls.lock().unwrap().as_slice(),
            ["deliveries:cos:7", "deliveries:cos:positive-overflow"]
        );
    }

    #[test]
    fn message_context_returns_item_and_not_found_error() {
        let backend = Arc::new(FakeBackend {
            context: Some(
                json!({"channel": "cos", "items": [{"id": 7}], "has_more": false, "next_before": 7}),
            ),
            ..Default::default()
        });
        let response = block_on(
            app(backend.clone(), authorized()).oneshot(request("/api/messages/7/context")),
        )
        .unwrap();
        let (status, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["channel"], "cos");
        assert_eq!(backend.calls.lock().unwrap().as_slice(), ["context:7"]);

        let missing = Arc::new(FakeBackend::default());
        let response =
            block_on(app(missing, authorized()).oneshot(request("/api/messages/8/context")))
                .unwrap();
        let (status, body) = response_json(response);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({"detail": "Message not found"}));

        let invalid = block_on(
            app(Arc::new(FakeBackend::default()), authorized())
                .oneshot(request("/api/messages/nope/context")),
        )
        .unwrap();
        let (status, body) = response_json(invalid);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["detail"][0]["loc"], json!(["path", "message_id"]));
        assert_eq!(body["detail"][0]["input"], "nope");
    }

    #[test]
    fn message_context_decodes_percent_encoded_path_id() {
        let backend = Arc::new(FakeBackend {
            context: Some(json!({"channel": "cos"})),
            ..Default::default()
        });
        let response = block_on(
            app(backend.clone(), authorized()).oneshot(request("/api/messages/%37/context")),
        )
        .unwrap();
        let (status, body) = response_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["channel"], "cos");
        assert_eq!(backend.calls.lock().unwrap().as_slice(), ["context:7"]);
    }

    #[test]
    fn message_integer_overflow_matches_sqlite_binding_failure() {
        let backend = Arc::new(FakeBackend::default());
        let before = block_on(app(backend.clone(), authorized()).oneshot(request(
            "/api/messages?channel=cos&before=9223372036854775808",
        )))
        .unwrap();
        assert_eq!(before.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(backend.calls.lock().unwrap().is_empty());

        let unknown_channel = block_on(app(backend.clone(), authorized()).oneshot(request(
            "/api/messages?channel=unknown&before=9223372036854775808",
        )))
        .unwrap();
        let (status, body) = response_json(unknown_channel);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({"detail": "Unknown channel"}));

        let context = block_on(
            app(backend.clone(), authorized())
                .oneshot(request("/api/messages/9223372036854775808/context")),
        )
        .unwrap();
        assert_eq!(context.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(backend.calls.lock().unwrap().is_empty());

        assert_eq!(
            parse_python_int("-9223372036854775808"),
            Some(PythonInt::Value(i64::MIN))
        );
        assert_eq!(
            parse_python_int("9223372036854775808"),
            Some(PythonInt::OutOfRange { negative: false })
        );
    }

    #[test]
    fn non_ascii_origin_is_refused() {
        let backend = Arc::new(FakeBackend::default());
        let request = Request::builder()
            .uri("/api/messages?channel=cos")
            .header("host", "desk.test")
            .header(
                header::ORIGIN,
                axum::http::HeaderValue::from_bytes(&[0xff]).unwrap(),
            )
            .body(Body::empty())
            .unwrap();
        let response = block_on(app(backend, authorized()).oneshot(request)).unwrap();
        let (status, body) = response_json(response);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "Cross-origin request refused"}));
    }

    #[test]
    fn message_routes_require_auth_and_hide_backend_failures() {
        let backend = Arc::new(FakeBackend::default());
        let unauthenticated = block_on(
            app(backend.clone(), Err(AuthError::SignInRequired))
                .oneshot(request("/api/messages?channel=cos")),
        )
        .unwrap();
        let (status, body) = response_json(unauthenticated);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"detail": "Sign in required"}));
        assert!(backend.calls.lock().unwrap().is_empty());

        let failing = Arc::new(FakeBackend {
            fail: true,
            ..Default::default()
        });
        let response =
            block_on(app(failing, authorized()).oneshot(request("/api/messages?channel=cos")))
                .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(bytes.as_ref(), b"Internal Server Error");
    }
}
