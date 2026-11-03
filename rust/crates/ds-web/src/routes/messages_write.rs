//! Message creation, urgent-send, and brief routes from `web/app.py`.
//!
//! Persistence and transport stay behind the injected backend. The handlers own the
//! HTTP contract (validation, auth, status, and JSON shaping) and can be tested without
//! opening the desk database or contacting a pane.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    body::to_bytes,
    extract::{Extension, Path, Request},
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Json, Router,
};
use serde_json::{json, Map, Value};

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
        message_id: i64,
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
    pub answers: Vec<i64>,
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
        Err(response) => return response,
    };
    let identity = match authenticate(auth.as_ref(), &headers, scheme).await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    let Some(body) = body else {
        return required_body_error();
    };
    let data = match message_input(&body) {
        Ok(data) => data,
        Err(response) => return response,
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
        Err(response) => return response,
    };
    if identity != Identity::Owner {
        return detail_response(StatusCode::FORBIDDEN, "Owner sign-in required");
    }
    let Ok(message_id) = message_id.parse::<i64>() else {
        return path_integer_error(&message_id);
    };
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
        Err(response) => return response,
    };
    let identity = match authenticate(auth.as_ref(), &headers, scheme).await {
        Ok(identity) => identity,
        Err(response) => return response,
    };
    if identity != Identity::Api {
        return detail_response(StatusCode::FORBIDDEN, "API token required");
    }
    let Some(body) = body else {
        return required_body_error();
    };
    let data = match brief_input(&body) {
        Ok(data) => data,
        Err(response) => return response,
    };
    match backend.post_brief(data).await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

fn message_input(body: &Value) -> Result<MessageIn, Response> {
    let object = model_object(body)?;
    let channel =
        string_field(object, "channel", body, true, 0, usize::MAX)?.expect("required string");
    let text = string_field(object, "body", body, false, 0, 12_000)?.unwrap_or_default();
    let attachment_ids = string_list(object, "attachment_ids", body, 10)?;
    let external_id = optional_string(object, "external_id", body, 100)?;
    Ok(MessageIn {
        channel,
        body: text.trim().to_owned(),
        attachment_ids,
        external_id,
    })
}

fn brief_input(body: &Value) -> Result<BriefIn, Response> {
    let object = model_object(body)?;
    let channel =
        string_field(object, "channel", body, true, 0, usize::MAX)?.expect("required string");
    let text = string_field(object, "body", body, true, 1, 20_000)?.expect("required string");
    let attachment_ids = string_list(object, "attachment_ids", body, 10)?;
    let answers = integer_list(object, "answers", body, 500)?;
    Ok(BriefIn {
        answers,
        channel,
        body: text.trim().to_owned(),
        attachment_ids,
    })
}

fn model_object(body: &Value) -> Result<&Map<String, Value>, Response> {
    body.as_object().ok_or_else(|| {
        validation_error(json!({
            "type": "model_attributes_type",
            "loc": ["body"],
            "msg": "Input should be a valid dictionary or object to extract fields from",
            "input": body
        }))
    })
}

fn string_field<'a>(
    object: &'a Map<String, Value>,
    name: &str,
    body: &Value,
    required: bool,
    min: usize,
    max: usize,
) -> Result<Option<String>, Response> {
    let Some(value) = object.get(name) else {
        if required {
            return Err(missing_field(name, body));
        }
        return Ok(None);
    };
    let Some(text) = value.as_str() else {
        return Err(validation_error(json!({
            "type": "string_type", "loc": ["body", name],
            "msg": "Input should be a valid string", "input": value
        })));
    };
    let length = text.chars().count();
    if length < min {
        return Err(validation_error(json!({
            "type": "string_too_short", "loc": ["body", name],
            "msg": format!("String should have at least {min} character"),
            "input": value, "ctx": {"min_length": min}
        })));
    }
    if length > max {
        return Err(validation_error(json!({
            "type": "string_too_long", "loc": ["body", name],
            "msg": format!("String should have at most {max} characters"),
            "input": value, "ctx": {"max_length": max}
        })));
    }
    Ok(Some(text.to_owned()))
}

fn optional_string(
    object: &Map<String, Value>,
    name: &str,
    body: &Value,
    max: usize,
) -> Result<Option<String>, Response> {
    match object.get(name) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => string_field(
            &Map::from_iter([(name.to_owned(), value.clone())]),
            name,
            body,
            true,
            0,
            max,
        ),
    }
}

fn string_list(
    object: &Map<String, Value>,
    name: &str,
    _body: &Value,
    max: usize,
) -> Result<Vec<String>, Response> {
    let Some(value) = object.get(name) else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(validation_error(json!({
            "type": "list_type", "loc": ["body", name],
            "msg": "Input should be a valid list", "input": value
        })));
    };
    if items.len() > max {
        return Err(validation_error(json!({
            "type": "too_long", "loc": ["body", name],
            "msg": format!("List should have at most {max} items after validation, not {}", items.len()),
            "input": value, "ctx": {"field_type": "List", "max_length": max, "actual_length": items.len()}
        })));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            item.as_str().map(str::to_owned).ok_or_else(|| {
                validation_error(json!({
                    "type": "string_type", "loc": ["body", name, index],
                    "msg": "Input should be a valid string", "input": item
                }))
            })
        })
        .collect()
}

fn integer_list(
    object: &Map<String, Value>,
    name: &str,
    _body: &Value,
    max: usize,
) -> Result<Vec<i64>, Response> {
    let Some(value) = object.get(name) else {
        return Ok(Vec::new());
    };
    let Some(items) = value.as_array() else {
        return Err(validation_error(json!({
            "type": "list_type", "loc": ["body", name],
            "msg": "Input should be a valid list", "input": value
        })));
    };
    if items.len() > max {
        return Err(validation_error(json!({
            "type": "too_long", "loc": ["body", name],
            "msg": format!("List should have at most {max} items after validation, not {}", items.len()),
            "input": value, "ctx": {"field_type": "List", "max_length": max, "actual_length": items.len()}
        })));
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let number = match item {
                Value::Number(number) => number.as_i64(),
                _ => None,
            };
            match number {
                Some(number) if number > 0 => Ok(number),
                Some(_) => Err(validation_error(json!({
                    "type": "greater_than", "loc": ["body", name, index],
                    "msg": "Input should be greater than 0", "input": item,
                    "ctx": {"gt": 0}
                }))),
                None => Err(validation_error(json!({
                    "type": "int_type", "loc": ["body", name, index],
                    "msg": "Input should be a valid integer", "input": item
                }))),
            }
        })
        .collect()
}

fn missing_field(name: &str, body: &Value) -> Response {
    validation_error(json!({
        "type": "missing", "loc": ["body", name],
        "msg": "Field required", "input": body
    }))
}

async fn request_json(request: Request) -> Result<Option<Value>, Response> {
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
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
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
) -> Result<Identity, Response> {
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let scheme_text = match scheme {
            RequestScheme::Http => "http",
            RequestScheme::Https => "https",
        };
        if !origin.is_empty() && origin != format!("{scheme_text}://{host}") {
            return Err(detail_response(
                StatusCode::FORBIDDEN,
                "Cross-origin request refused",
            ));
        }
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| {
            value.split(';').find_map(|part| {
                let (name, value) = part.trim().split_once('=')?;
                (name.trim() == "vp_session").then(|| value.trim())
            })
        });
    auth.identity(bearer, cookie)
        .await
        .map_err(auth_error_response)
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
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": [error]})),
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
                match bearer.or(cookie) {
                    Some("owner-token") => Ok(Identity::Owner),
                    Some("api-token") => Ok(Identity::Api),
                    _ => Err(AuthError::SignInRequired),
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
            message_id: i64,
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
                "brief:{}:{}:{:?}",
                data.channel, data.body, data.answers
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
