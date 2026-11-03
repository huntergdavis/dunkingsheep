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

/// Database/client boundary for the read routes. Implementations perform the
/// same reconciliation and SQL reads as `web.app.messages`, `deliveries`, and
/// `message_context`; tests inject deterministic results without live state.
pub trait MessagesReadBackend: Send + Sync + 'static {
    fn known_channel(&self, channel: &str) -> bool;

    fn messages<'a>(&'a self, channel: &'a str, before: i64, limit: i64) -> MessagesReadFuture<'a>;

    fn deliveries<'a>(&'a self, channel: &'a str, ids: &'a [i64]) -> MessagesReadFuture<'a>;

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
    let Some(channel) = query.get("channel") else {
        return missing_query("channel");
    };
    let before = match query.get("before") {
        Some(value) => match parse_python_int(value) {
            Some(value) => value,
            None => return invalid_integer("query", "before", value),
        },
        None => i64::MAX,
    };
    let before = if before == 0 { i64::MAX } else { before };
    let limit = match query.get("limit") {
        Some(value) => match parse_python_int(value) {
            Some(value) => value,
            None => return invalid_integer("query", "limit", value),
        },
        None => 50,
    }
    .clamp(1, 100);
    if !backend.known_channel(channel) {
        return detail_response(StatusCode::NOT_FOUND, "Unknown channel");
    }
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
    let Some(channel) = query.get("channel") else {
        return missing_query("channel");
    };
    let Some(raw_ids) = query.get("ids") else {
        return missing_query("ids");
    };
    if !backend.known_channel(channel) {
        return detail_response(StatusCode::NOT_FOUND, "Unknown channel");
    }
    let ids = match parse_message_ids(raw_ids) {
        Ok(ids) => ids,
        Err(()) => return detail_response(StatusCode::UNPROCESSABLE_ENTITY, "Invalid message IDs"),
    };
    if ids.len() > 100 || ids.iter().any(|id| *id < 1) {
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
    let message_id = match parse_python_int(raw_id) {
        Some(value) => value,
        None => return invalid_integer("path", "message_id", raw_id),
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

fn missing_query(field: &'static str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "missing", "loc": ["query", field], "msg": "Field required"
        }]})),
    )
        .into_response()
}

fn invalid_integer(location: &'static str, field: &'static str, input: &str) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [{
            "type": "int_parsing",
            "loc": [location, field],
            "msg": "Input should be a valid integer, unable to parse string as an integer",
            "input": input
        }]})),
    )
        .into_response()
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

fn parse_message_ids(raw: &str) -> Result<Vec<i64>, ()> {
    raw.split(',')
        .filter(|part| !part.is_empty())
        .map(|part| {
            // The Python route uses built-in int() here, which rejects a
            // decimal point even when the fractional part is all zeroes.
            (!part.contains('.'))
                .then(|| parse_python_int(part))
                .flatten()
        })
        .collect::<Option<Vec<_>>>()
        .ok_or(())
}

/// Pydantic's query integer conversion accepts whitespace, signs, underscores,
/// and decimal strings with an all-zero fractional part.
fn parse_python_int(raw: &str) -> Option<i64> {
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
    let magnitude = integer_part
        .bytes()
        .filter(|byte| *byte != b'_')
        .fold(0_i64, |value, byte| {
            value
                .saturating_mul(10)
                .saturating_add(i64::from(byte - b'0'))
        });
    Some(if negative {
        magnitude.saturating_neg()
    } else {
        magnitude
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

        fn deliveries<'a>(&'a self, channel: &'a str, ids: &'a [i64]) -> MessagesReadFuture<'a> {
            self.calls.lock().unwrap().push(format!(
                "deliveries:{channel}:{}",
                ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",")
            ));
            Box::pin(async move {
                if self.fail {
                    Err(MessagesReadError)
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
