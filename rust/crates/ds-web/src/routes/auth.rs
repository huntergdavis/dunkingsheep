//! Authentication routes ported from `web/app.py` (`login`, `logout`, and
//! `change_password`). Persistent credentials and sessions remain behind the
//! injected backend so handlers can be exercised without a live desk database.

use std::{future::Future, pin::Pin, sync::Arc};

use axum::{
    body::Bytes,
    extract::{ConnectInfo, Extension, Json},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use serde::Deserialize;
use serde_json::{json, Map, Value};

const SESSION_COOKIE: &str = "vp_session";
const SESSION_MAX_AGE: u64 = 7 * 24 * 60 * 60;

pub type AuthFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Trusted external scheme for same-origin checks and login return URLs.
/// Configure this from the web application's transport or trusted proxy setup,
/// never directly from a client-supplied forwarding header.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RequestScheme {
    #[default]
    Http,
    Https,
}

impl RequestScheme {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Http => "http",
            Self::Https => "https",
        }
    }
}

/// The route boundary for credential and session operations. The production
/// adapter owns hashing, persistence, rate limiting, and session expiry.
pub trait AuthBackend: Send + Sync + 'static {
    fn login<'a>(
        &'a self,
        address: &'a str,
        password: &'a str,
    ) -> AuthFuture<'a, Result<String, AuthError>>;
    fn identity<'a>(
        &'a self,
        bearer: Option<&'a str>,
        cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<Identity, AuthError>>;
    fn logout<'a>(&'a self, cookie: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>>;
    fn change_password<'a>(
        &'a self,
        current: &'a str,
        new: &'a str,
        keep_cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<(), AuthError>>;
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum Identity {
    #[default]
    Owner,
    Api,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum AuthError {
    TooManyAttempts,
    IncorrectPassword,
    SignInRequired,
    Internal(String),
}

impl AuthError {
    fn response(&self) -> Response {
        let (status, detail) = match self {
            Self::TooManyAttempts => (
                StatusCode::TOO_MANY_REQUESTS,
                "Too many attempts; retry in five minutes".to_owned(),
            ),
            Self::IncorrectPassword => (StatusCode::UNAUTHORIZED, "Incorrect password".to_owned()),
            Self::SignInRequired => (StatusCode::UNAUTHORIZED, "Sign in required".to_owned()),
            Self::Internal(_) => {
                return (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error")
                    .into_response();
            }
        };
        (status, Json(json!({"detail": detail}))).into_response()
    }
}

#[derive(Deserialize)]
struct LoginBody {
    password: String,
    #[serde(default)]
    next: Option<String>,
}

#[derive(Deserialize)]
struct PasswordBody {
    current_password: String,
    new_password: String,
}

/// Mount the auth routes. The outer web application supplies its backend as
/// an `Extension<Arc<dyn AuthBackend>>`, keeping persistence out of handlers.
pub fn router() -> Router {
    router_with_scheme(RequestScheme::Http)
}

/// Mount auth routes with the externally visible request scheme. When TLS
/// terminates at a reverse proxy, the application must set this from its
/// trusted proxy configuration rather than accepting client forwarding headers.
pub fn router_with_scheme(scheme: RequestScheme) -> Router {
    Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/password", post(change_password))
        .layer(Extension(scheme))
}

async fn login(
    Extension(backend): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    ConnectInfo(address): ConnectInfo<std::net::SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> Response {
    if let Some(response) = same_origin(&headers, scheme) {
        return response;
    }
    let address = address.ip().to_string();
    match backend.login(&address, &body.password).await {
        Ok(token) => {
            let next = body
                .next
                .as_deref()
                .and_then(|next| safe_next(next, &headers, scheme));
            let mut response =
                Json(json!({"ok": true, "next": next.unwrap_or_else(|| "/dash/cos".to_owned())}))
                    .into_response();
            set_cookie(&mut response, &token);
            response
        }
        Err(error) => error.response(),
    }
}

async fn logout(
    Extension(backend): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = same_origin(&headers, scheme) {
        return response;
    }
    let cookie = cookie_value(&headers, SESSION_COOKIE);
    let bearer = bearer_value(&headers);
    match backend.identity(bearer.as_deref(), cookie.as_deref()).await {
        Err(error) => error.response(),
        Ok(_) => match backend.logout(cookie.as_deref()).await {
            Ok(()) => {
                let mut response = Json(json!({"ok": true})).into_response();
                delete_cookie(&mut response);
                response
            }
            Err(error) => error.response(),
        },
    }
}

/// The Pydantic v2 validation response FastAPI sends for `PasswordChange` (t3722): every error,
/// in model order, under `detail`, with status 422.
fn validation_detail(errors: Vec<Value>) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"detail": errors})),
    )
        .into_response()
}

/// One string field of the password body, validated as Pydantic validates `str` (with the
/// `new_password` length bounds). Pushes the error Pydantic would report; returns the value when valid.
/// What a validation error shows in place of a submitted password (owner decision i50225: submitted
/// passwords must not appear in validation error bodies). Pydantic would echo the value as `input`.
const REDACTED: &str = "[redacted]";

/// The `input` of a missing-field error. Pydantic echoes the whole submitted object, which can carry a
/// password under any key (a typo such as `old_password` or `password` included), so the object is
/// not echoed at all. An empty object carries nothing to redact and stays `{}` (owner i50225; t3724 R2).
fn missing_input(body: &Value) -> Value {
    match body.as_object() {
        Some(object) if object.is_empty() => json!({}),
        _ => json!(REDACTED),
    }
}

/// FastAPI decodes a body as JSON only when its content type is `application/json` or an
/// `application/*+json` subtype (parameters such as `charset` are ignored). Any other type, or none,
/// reaches the model as raw bytes and fails model validation (t3722 R1).
fn is_json_content(headers: &HeaderMap) -> bool {
    let Some(value) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
    else {
        return false;
    };
    let essence = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    match essence.split_once('/') {
        Some(("application", subtype)) => subtype == "json" || subtype.ends_with("+json"),
        _ => false,
    }
}

fn password_field(
    body: &Value,
    object: &Map<String, Value>,
    field: &str,
    length: Option<(usize, usize)>,
    errors: &mut Vec<Value>,
) -> Option<String> {
    let loc = json!(["body", field]);
    match object.get(field) {
        None => {
            errors.push(json!({"input": missing_input(body), "loc": loc, "msg": "Field required", "type": "missing"}));
            None
        }
        Some(Value::String(text)) => {
            if let Some((min, max)) = length {
                let chars = text.chars().count();
                if chars < min {
                    errors.push(json!({"ctx": {"min_length": min}, "input": REDACTED, "loc": loc,
                        "msg": format!("String should have at least {min} characters"), "type": "string_too_short"}));
                    return None;
                }
                if chars > max {
                    errors.push(json!({"ctx": {"max_length": max}, "input": REDACTED, "loc": loc,
                        "msg": format!("String should have at most {max} characters"), "type": "string_too_long"}));
                    return None;
                }
            }
            Some(text.clone())
        }
        Some(_) => {
            errors.push(json!({"input": REDACTED, "loc": loc, "msg": "Input should be a valid string", "type": "string_type"}));
            None
        }
    }
}

/// The encodings Python's `json.detect_encoding` can choose for a body (t3725 / owner i50587).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BodyEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    Utf32Le,
    Utf32Be,
}

/// `json.detect_encoding`: a BOM decides first (UTF-32, then UTF-16, then UTF-8), then the NUL pattern of
/// the first bytes. Returns the encoding and the length of the BOM the decoder consumes.
fn detect_body_encoding(raw: &[u8]) -> (BodyEncoding, usize) {
    if raw.starts_with(&[0x00, 0x00, 0xfe, 0xff]) {
        return (BodyEncoding::Utf32Be, 4);
    }
    if raw.starts_with(&[0xff, 0xfe, 0x00, 0x00]) {
        return (BodyEncoding::Utf32Le, 4);
    }
    if raw.starts_with(&[0xfe, 0xff]) {
        return (BodyEncoding::Utf16Be, 2);
    }
    if raw.starts_with(&[0xff, 0xfe]) {
        return (BodyEncoding::Utf16Le, 2);
    }
    if raw.starts_with(&[0xef, 0xbb, 0xbf]) {
        return (BodyEncoding::Utf8, 3);
    }
    if raw.len() >= 4 {
        if raw[0] == 0 {
            let encoding = if raw[1] != 0 {
                BodyEncoding::Utf16Be
            } else {
                BodyEncoding::Utf32Be
            };
            return (encoding, 0);
        }
        if raw[1] == 0 {
            let encoding = if raw[2] != 0 || raw[3] != 0 {
                BodyEncoding::Utf16Le
            } else {
                BodyEncoding::Utf32Le
            };
            return (encoding, 0);
        }
    } else if raw.len() == 2 {
        if raw[0] == 0 {
            return (BodyEncoding::Utf16Be, 0);
        }
        if raw[1] == 0 {
            return (BodyEncoding::Utf16Le, 0);
        }
    }
    (BodyEncoding::Utf8, 0)
}

/// The JSON parse error FastAPI reports (`json_invalid`). The position is the one the pinned route uses for a
/// body that cannot be decoded at all; the wording is the parser's own.
fn json_decode_error(text: String) -> Response {
    validation_detail(vec![json!({
        "ctx": {"error": text},
        "input": {},
        "loc": ["body", 0],
        "msg": "JSON decode error",
        "type": "json_invalid",
    })])
}

/// `json.loads(bytes)` (what FastAPI uses): detect the encoding, decode with `surrogatepass`, then parse.
/// A decode failure (odd or truncated units, a code point past U+10FFFF, invalid UTF-8) is Python's
/// UnicodeDecodeError, answered 400 with the generic body. Surrogates in ignored fields are
/// represented temporarily; password fields retain the owner-dispositioned refusal.
fn decode_json_body(raw: &[u8]) -> Result<Value, Box<Response>> {
    let parse_error = || {
        Box::new(
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"detail": "There was an error parsing the body"})),
            )
                .into_response(),
        )
    };
    let lone_surrogate = || Box::new(json_decode_error("lone surrogate in the body".to_owned()));
    let (encoding, skip) = detect_body_encoding(raw);
    let body = &raw[skip..];
    let mut surrogate_positions = Vec::new();
    let text = match encoding {
        BodyEncoding::Utf8 => {
            // UTF-8 keeps the parser's own handling; invalid UTF-8 is the accepted difference recorded in the tests.
            return serde_json::from_slice(body)
                .map_err(|error| Box::new(json_decode_error(error.to_string())));
        }
        BodyEncoding::Utf16Le | BodyEncoding::Utf16Be => {
            // An odd trailing byte is a truncated code unit: Python's UnicodeDecodeError.
            let (pairs, rest) = body.as_chunks::<2>();
            if !rest.is_empty() {
                return Err(parse_error());
            }
            let little = encoding == BodyEncoding::Utf16Le;
            let units: Vec<u16> = pairs
                .iter()
                .map(|pair| {
                    if little {
                        u16::from_le_bytes(*pair)
                    } else {
                        u16::from_be_bytes(*pair)
                    }
                })
                .collect();
            let mut text = String::new();
            for decoded in char::decode_utf16(units) {
                match decoded {
                    Ok(ch) => text.push(ch),
                    Err(_) => {
                        surrogate_positions.push(text.len());
                        text.push('\u{fffd}');
                    }
                }
            }
            text
        }
        BodyEncoding::Utf32Le | BodyEncoding::Utf32Be => {
            let (quads, rest) = body.as_chunks::<4>();
            if !rest.is_empty() {
                return Err(parse_error());
            }
            let little = encoding == BodyEncoding::Utf32Le;
            let codes: Vec<u32> = quads
                .iter()
                .map(|quad| {
                    if little {
                        u32::from_le_bytes(*quad)
                    } else {
                        u32::from_be_bytes(*quad)
                    }
                })
                .collect();
            let mut text = String::new();
            for quad in quads {
                let code = if little {
                    u32::from_le_bytes(*quad)
                } else {
                    u32::from_be_bytes(*quad)
                };
                if code > 0x10FFFF {
                    return Err(parse_error());
                }
                if (0xD800..=0xDFFF).contains(&code) {
                    surrogate_positions.push(text.len());
                    text.push('\u{fffd}');
                } else {
                    text.push(char::from_u32(code).ok_or_else(parse_error)?);
                }
            }
            text
        }
    };
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| Box::new(json_decode_error(error.to_string())))?;
    if !surrogate_positions.is_empty() {
        // Parse a second time with a different substitute only at the exact surrogate
        // positions. Existing literal characters cannot collide with this comparison.
        let mut alternate = text.as_bytes().to_vec();
        for position in surrogate_positions {
            alternate[position..position + 3].copy_from_slice("\u{fffe}".as_bytes());
        }
        let other: Value = serde_json::from_slice(&alternate)
            .map_err(|error| Box::new(json_decode_error(error.to_string())))?;
        for field in ["current_password", "new_password"] {
            if value.get(field) != other.get(field) {
                return Err(lone_surrogate());
            }
        }
    }
    Ok(value)
}

/// Parse and validate the password body in FastAPI's order: a JSON object, then `current_password`,
/// then `new_password`. A body that is not JSON or not an object is refused the same way FastAPI
/// refuses it (a JSON-decode error in one case; the decoder's own text is not reproduced).
fn password_body(raw: &[u8], json_content: bool) -> Result<PasswordBody, Box<Response>> {
    // An empty body is no body at all to FastAPI: a 'missing' error with input null, not a decode error.
    if raw.is_empty() {
        return Err(Box::new(validation_detail(vec![json!({
            "input": null,
            "loc": ["body"],
            "msg": "Field required",
            "type": "missing",
        })])));
    }
    // A body that is not declared JSON is never parsed: FastAPI reports the raw bytes as a model error,
    // and the bytes are redacted here (they carry the submitted passwords).
    if !json_content {
        return Err(Box::new(validation_detail(vec![json!({
            "input": REDACTED,
            "loc": ["body"],
            "msg": "Input should be a valid dictionary or object to extract fields from",
            "type": "model_attributes_type",
        })])));
    }
    let value = decode_json_body(raw)?;
    let Some(object) = value.as_object() else {
        return Err(Box::new(validation_detail(vec![json!({
            "input": REDACTED,
            "loc": ["body"],
            "msg": "Input should be a valid dictionary or object to extract fields from",
            "type": "model_attributes_type",
        })])));
    };
    let mut errors = Vec::new();
    let current = password_field(&value, object, "current_password", None, &mut errors);
    let new = password_field(&value, object, "new_password", Some((12, 200)), &mut errors);
    match (current, new) {
        (Some(current_password), Some(new_password)) if errors.is_empty() => Ok(PasswordBody {
            current_password,
            new_password,
        }),
        _ => Err(Box::new(validation_detail(errors))),
    }
}

async fn change_password(
    Extension(backend): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    headers: HeaderMap,
    raw: Bytes,
) -> Response {
    if let Some(response) = same_origin(&headers, scheme) {
        return response;
    }
    // FastAPI's order (t3722): same origin, then authentication, then body validation, then the
    // owner check inside the handler. Validation never runs ahead of authentication.
    let cookie = cookie_value(&headers, SESSION_COOKIE);
    let bearer = bearer_value(&headers);
    let identity = match backend.identity(bearer.as_deref(), cookie.as_deref()).await {
        Err(error) => return error.response(),
        Ok(identity) => identity,
    };
    let body = match password_body(&raw, is_json_content(&headers)) {
        Ok(body) => body,
        Err(response) => return *response,
    };
    match identity {
        Identity::Api => (
            StatusCode::FORBIDDEN,
            Json(json!({"detail": "Owner sign-in required"})),
        )
            .into_response(),
        Identity::Owner => match backend
            .change_password(
                &body.current_password,
                &body.new_password,
                cookie.as_deref(),
            )
            .await
        {
            Ok(()) => Json(json!({"ok": true})).into_response(),
            Err(AuthError::IncorrectPassword) => (
                StatusCode::FORBIDDEN,
                Json(json!({"detail": "Current password is incorrect"})),
            )
                .into_response(),
            Err(error) => error.response(),
        },
    }
}

fn same_origin(headers: &HeaderMap, scheme: RequestScheme) -> Option<Response> {
    let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
    let host = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if origin != format!("{}://{host}", scheme.as_str()) {
        Some(
            (
                StatusCode::FORBIDDEN,
                Json(json!({"detail": "Cross-origin request refused"})),
            )
                .into_response(),
        )
    } else {
        None
    }
}

fn safe_next(value: &str, headers: &HeaderMap, request_scheme: RequestScheme) -> Option<String> {
    let candidate = value.trim();
    if candidate.is_empty() {
        return None;
    }
    // Browsers treat backslashes as slashes in HTTP(S) URLs, so a path like
    // `/\\evil.example` can become a cross-origin URL after navigation.
    if candidate.contains('\\') {
        return None;
    }
    // The WHATWG URL parser removes tabs and newlines before navigation.
    if candidate
        .chars()
        .any(|character| character.is_ascii_control())
    {
        return None;
    }
    if candidate.starts_with('/') && !candidate.starts_with("//") {
        return Some(candidate.to_owned());
    }
    let (scheme, remainder) = candidate.split_once("://")?;
    let authority = remainder.split(['/', '?', '#']).next()?;
    let (host, port) = authority.rsplit_once(':')?;
    let request_host = headers.get(header::HOST)?.to_str().ok()?;
    let request_port = request_host.rsplit_once(':')?.1;
    let request_host = request_host.rsplit_once(':')?.0;
    if host.eq_ignore_ascii_case(request_host)
        && port == request_port
        && scheme == request_scheme.as_str()
        && matches!(scheme, "http" | "https")
    {
        Some(candidate.to_owned())
    } else {
        None
    }
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

fn bearer_value(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    value.strip_prefix("Bearer ").map(str::to_owned)
}

fn set_cookie(response: &mut Response, token: &str) {
    let value = format!(
        "{SESSION_COOKIE}={token}; Path=/; Max-Age={SESSION_MAX_AGE}; HttpOnly; SameSite=Strict"
    );
    if let Ok(value) = HeaderValue::from_str(&value) {
        response.headers_mut().append(header::SET_COOKIE, value);
    }
}

fn delete_cookie(response: &mut Response) {
    response.headers_mut().append(header::SET_COOKIE, HeaderValue::from_static("vp_session=; expires=Thu, 01 Jan 1970 00:00:00 GMT; Max-Age=0; Path=/; HttpOnly; SameSite=Strict"));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::{
        sync::Mutex,
        task::{Context, Poll, Waker},
    };

    struct FakeAuth {
        identity: Mutex<Result<Identity, AuthError>>,
        login_error: Mutex<Option<AuthError>>,
        changed_password: Mutex<Option<(String, String, Option<String>)>>,
        logged_out: Mutex<Option<Option<String>>>,
    }

    impl Default for FakeAuth {
        fn default() -> Self {
            Self {
                identity: Mutex::new(Ok(Identity::Owner)),
                login_error: Mutex::new(None),
                changed_password: Mutex::new(None),
                logged_out: Mutex::new(None),
            }
        }
    }

    impl AuthBackend for FakeAuth {
        fn login<'a>(
            &'a self,
            _address: &'a str,
            password: &'a str,
        ) -> AuthFuture<'a, Result<String, AuthError>> {
            let configured_error = self.login_error.lock().expect("login lock").clone();
            Box::pin(async move {
                if let Some(error) = configured_error {
                    Err(error)
                } else if password == "correct" {
                    Ok("session-token".into())
                } else {
                    Err(AuthError::IncorrectPassword)
                }
            })
        }

        fn identity<'a>(
            &'a self,
            _bearer: Option<&'a str>,
            _cookie: Option<&'a str>,
        ) -> AuthFuture<'a, Result<Identity, AuthError>> {
            let result = self.identity.lock().expect("identity lock").clone();
            Box::pin(async move { result })
        }

        fn logout<'a>(&'a self, cookie: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
            *self.logged_out.lock().expect("logout lock") = Some(cookie.map(str::to_owned));
            Box::pin(async { Ok(()) })
        }

        fn change_password<'a>(
            &'a self,
            current: &'a str,
            new: &'a str,
            keep_cookie: Option<&'a str>,
        ) -> AuthFuture<'a, Result<(), AuthError>> {
            *self.changed_password.lock().expect("password lock") =
                Some((current.into(), new.into(), keep_cookie.map(str::to_owned)));
            Box::pin(async { Ok(()) })
        }
    }

    /// Owner decision i50225: a submitted password must not appear in a validation error body. Each
    /// invalid shape below carries scratch sentinel passwords; none may come back in the response.
    #[test]
    fn password_validation_errors_never_echo_a_submitted_password() {
        const CURRENT: &str = "SENTINEL-CURRENT-t3722";
        const NEW_VALID: &str = "SENTINEL-NEW-t3722-valid";
        const NEW_SHORT: &str = "SENT-SHORT";
        let too_long = format!("{NEW_VALID}{}", "x".repeat(220));
        let bodies = [
            json!({"new_password": NEW_SHORT}),
            json!({"current_password": CURRENT}),
            json!({"current_password": CURRENT, "new_password": NEW_SHORT}),
            json!({"current_password": CURRENT, "new_password": too_long}),
            json!({"current_password": [CURRENT], "new_password": NEW_VALID}),
            json!({"current_password": 12345, "new_password": NEW_VALID}),
            // Under keys other than the two named fields (t3724 R2): a typo must not leak either.
            json!({"old_password": CURRENT}),
            json!({"password": CURRENT, "new_password": NEW_VALID}),
            json!({CURRENT: "typed into a key", "new_password": NEW_VALID}),
            json!([CURRENT, NEW_SHORT]),
            json!(CURRENT),
        ];
        for body in bodies {
            let raw = body.to_string();
            let response = match password_body(raw.as_bytes(), true) {
                Err(response) => *response,
                Ok(_) => panic!("{raw} must be refused"),
            };
            let bytes = block_on(axum::body::to_bytes(response.into_body(), usize::MAX)).unwrap();
            let text = String::from_utf8_lossy(&bytes);
            for secret in [CURRENT, NEW_SHORT, NEW_VALID] {
                assert!(!text.contains(secret), "{raw} echoes {secret} in {text}");
            }
        }
    }

    /// t3724 R1: a body that is not declared JSON is refused without being parsed, and its bytes (which
    /// carry the passwords) are not echoed.
    #[test]
    fn password_body_refuses_a_non_json_content_type_without_echoing_the_body() {
        const CURRENT: &str = "SENTINEL-CURRENT-t3722";
        let raw = json!({"current_password": CURRENT, "new_password": "SENTINEL-NEW-t3722-valid"})
            .to_string();
        let response = match password_body(raw.as_bytes(), false) {
            Err(response) => *response,
            Ok(_) => panic!("a non-JSON body must not be parsed"),
        };
        let bytes = block_on(axum::body::to_bytes(response.into_body(), usize::MAX)).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains(CURRENT), "the body is echoed: {text}");
        assert_eq!(
            serde_json::from_str::<Value>(&text).unwrap(),
            json!({"detail": [{"input": "[redacted]", "loc": ["body"], "msg": "Input should be a valid dictionary or object to extract fields from", "type": "model_attributes_type"}]}),
        );
    }

    /// t3725 (owner i50493): a UTF-8 BOM is dropped before the JSON is read, as Python does, so a
    /// BOM-prefixed body parses to the same password fields as the same body without the BOM.
    #[test]
    fn password_body_accepts_a_utf8_bom_as_python_does() {
        let plain = br#"{"current_password":"old-password-x","new_password":"a sufficiently long password"}"#;
        let mut bom = vec![0xef, 0xbb, 0xbf];
        bom.extend_from_slice(plain);
        let with_bom = match password_body(&bom, true) {
            Ok(body) => body,
            Err(_) => panic!("a BOM-prefixed body must parse"),
        };
        let without = match password_body(plain, true) {
            Ok(body) => body,
            Err(_) => panic!("the plain body must parse"),
        };
        assert_eq!(with_bom.current_password, without.current_password);
        assert_eq!(with_bom.new_password, without.new_password);
    }

    /// t3725 / owner i50587: the encodings match Python's `json.detect_encoding` and `json.loads` decoding.
    #[test]
    fn body_encoding_detection_matches_python() {
        for (raw, expected, skip) in [
            (
                &[0x00u8, 0x00, 0xfe, 0xff, 0x00][..],
                BodyEncoding::Utf32Be,
                4,
            ),
            (
                &[0xff, 0xfe, 0x00, 0x00, 0x7b][..],
                BodyEncoding::Utf32Le,
                4,
            ),
            (&[0xfe, 0xff, 0x00, 0x7b][..], BodyEncoding::Utf16Be, 2),
            (&[0xff, 0xfe, 0x7b, 0x00][..], BodyEncoding::Utf16Le, 2),
            (&[0xef, 0xbb, 0xbf, 0x7b][..], BodyEncoding::Utf8, 3),
            (&[0x00, 0x7b, 0x00, 0x7d][..], BodyEncoding::Utf16Be, 0),
            (&[0x7b, 0x00, 0x7d, 0x00][..], BodyEncoding::Utf16Le, 0),
            (&[0x00, 0x00, 0x00, 0x7b][..], BodyEncoding::Utf32Be, 0),
            (&[0x7b, 0x00, 0x00, 0x00][..], BodyEncoding::Utf32Le, 0),
            (&[0x00, 0x7b][..], BodyEncoding::Utf16Be, 0),
            (&[0x7b, 0x00][..], BodyEncoding::Utf16Le, 0),
            (&b"{}"[..], BodyEncoding::Utf8, 0),
            (&b"{\"a\":1}"[..], BodyEncoding::Utf8, 0),
        ] {
            assert_eq!(detect_body_encoding(raw), (expected, skip), "{raw:?}");
        }
    }

    #[test]
    fn utf16_and_utf32_bodies_parse_to_the_same_fields_as_utf8() {
        let text = r#"{"current_password":"old-password-x","new_password":"a sufficiently long password"}"#;
        let utf8 = match password_body(text.as_bytes(), true) {
            Ok(body) => body,
            Err(_) => panic!("utf-8 parses"),
        };
        let utf16 = |be: bool| -> Vec<u8> {
            text.encode_utf16()
                .flat_map(|unit| {
                    if be {
                        unit.to_be_bytes()
                    } else {
                        unit.to_le_bytes()
                    }
                })
                .collect()
        };
        let utf32 = |be: bool| -> Vec<u8> {
            text.chars()
                .flat_map(|c| {
                    if be {
                        (c as u32).to_be_bytes()
                    } else {
                        (c as u32).to_le_bytes()
                    }
                })
                .collect()
        };
        let mut variants: Vec<Vec<u8>> = vec![utf16(false), utf16(true), utf32(false), utf32(true)];
        variants.push([&[0xff, 0xfe][..], &utf16(false)].concat());
        variants.push([&[0xfe, 0xff][..], &utf16(true)].concat());
        variants.push([&[0xff, 0xfe, 0x00, 0x00][..], &utf32(false)].concat());
        variants.push([&[0x00, 0x00, 0xfe, 0xff][..], &utf32(true)].concat());
        for raw in variants {
            let parsed = match password_body(&raw, true) {
                Ok(body) => body,
                Err(_) => panic!("a valid UTF-16/UTF-32 body parses"),
            };
            assert_eq!(parsed.current_password, utf8.current_password, "{raw:?}");
            assert_eq!(parsed.new_password, utf8.new_password, "{raw:?}");
        }
    }

    #[test]
    fn ignored_unpaired_surrogates_do_not_reject_valid_password_fields() {
        // An escaped private-use scalar in a password is not the placeholder.
        let collision =
            r#"{"current_password":"\ue000","new_password":"long-enough-password","ignored":""#;
        let units: Vec<u16> = collision
            .encode_utf16()
            .chain([0xD800])
            .chain(r#""}"#.encode_utf16())
            .collect();
        let raw: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        assert!(password_body(&raw, true).is_ok());
        for key in [false, true] {
            let (prefix, suffix) = if key {
                (
                    r#"{"current_password":"wrong","new_password":"long-enough-password",""#,
                    r#"":"ok"}"#,
                )
            } else {
                (
                    r#"{"current_password":"wrong","new_password":"long-enough-password","ignored":""#,
                    r#""}"#,
                )
            };
            let units: Vec<u16> = prefix
                .encode_utf16()
                .chain([0xD800])
                .chain(suffix.encode_utf16())
                .collect();
            for be in [false, true] {
                let raw: Vec<u8> = units
                    .iter()
                    .flat_map(|u| if be { u.to_be_bytes() } else { u.to_le_bytes() })
                    .collect();
                assert!(password_body(&raw, true).is_ok());
            }
            let codes: Vec<u32> = prefix
                .chars()
                .map(u32::from)
                .chain([0xD800])
                .chain(suffix.chars().map(u32::from))
                .collect();
            for be in [false, true] {
                let raw: Vec<u8> = codes
                    .iter()
                    .flat_map(|u| if be { u.to_be_bytes() } else { u.to_le_bytes() })
                    .collect();
                assert!(password_body(&raw, true).is_ok());
            }
        }
    }

    #[test]
    fn utf16_and_utf32_decode_errors_are_400_and_lone_surrogates_are_refused() {
        let text = r#"{"current_password":"x","new_password":"a sufficiently long password"}"#;
        let utf16le: Vec<u8> = text.encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let utf32le: Vec<u8> = text
            .chars()
            .flat_map(|c| (c as u32).to_le_bytes())
            .collect();
        let status_of = |raw: &[u8]| match password_body(raw, true) {
            Ok(_) => None,
            Err(response) => Some(response.status()),
        };
        // Odd UTF-16 length, truncated UTF-32, a BOM over UTF-16 text (detected as UTF-32), and a code point past U+10FFFF.
        assert_eq!(
            status_of(&[utf16le.clone(), vec![0x00]].concat()),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            status_of(&[utf32le.clone(), vec![0x00]].concat()),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            status_of(&[&[0xff, 0xfe, 0x00, 0x00][..], &utf16le[..]].concat()),
            Some(StatusCode::BAD_REQUEST)
        );
        assert_eq!(
            status_of(
                &[
                    &[0x00, 0x00, 0xfe, 0xff][..],
                    &[0x00, 0x11, 0x00, 0x00][..],
                    &text.as_bytes()[..4]
                ]
                .concat()
            ),
            Some(StatusCode::BAD_REQUEST)
        );
        // A raw lone surrogate code unit (not a JSON escape): refused as json_invalid, not parsed (Python answers 500).
        let mut units: Vec<u16> = r#"{"current_password":""#.encode_utf16().collect();
        units.push(0xD800);
        units.extend(r#""}"#.encode_utf16());
        let lone: Vec<u8> = units.iter().flat_map(|u| u.to_le_bytes()).collect();
        assert_eq!(status_of(&lone), Some(StatusCode::UNPROCESSABLE_ENTITY));
    }

    #[test]
    fn is_json_content_accepts_only_json_media_types() {
        for (value, json) in [
            ("application/json", true),
            ("application/json; charset=utf-8", true),
            ("Application/JSON", true),
            ("application/vnd.api+json", true),
            ("text/plain", false),
            ("text/json", false),
            ("application/x-www-form-urlencoded", false),
            ("multipart/form-data; boundary=x", false),
            ("", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
            assert_eq!(is_json_content(&headers), json, "{value}");
        }
        assert!(
            !is_json_content(&HeaderMap::new()),
            "no content type is not JSON"
        );
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

    fn headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("desk.test"));
        headers
    }

    fn body_json(response: Response) -> (StatusCode, Value, HeaderMap) {
        let status = response.status();
        let headers = response.headers().clone();
        let body = block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .expect("response body");
        (
            status,
            serde_json::from_slice(&body).expect("JSON response"),
            headers,
        )
    }

    fn body_text(response: Response) -> (StatusCode, String, HeaderMap) {
        let status = response.status();
        let headers = response.headers().clone();
        let body = block_on(axum::body::to_bytes(response.into_body(), usize::MAX))
            .expect("response body");
        (
            status,
            String::from_utf8(body.to_vec()).expect("text response"),
            headers,
        )
    }

    #[test]
    fn login_returns_safe_next_and_strict_session_cookie() {
        let backend: Arc<dyn AuthBackend> = Arc::new(FakeAuth::default());
        let mut headers = headers();
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://desk.test"));
        let response = block_on(login(
            Extension(backend),
            Extension(RequestScheme::Http),
            ConnectInfo("127.0.0.1:4000".parse().unwrap()),
            headers,
            Json(LoginBody {
                password: "correct".into(),
                next: Some("/dash/tasks".into()),
            }),
        ));
        let (status, body, headers) = body_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": true, "next": "/dash/tasks"}));
        let cookie = headers.get(header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(cookie.contains("vp_session=session-token"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=604800"));
    }

    #[test]
    fn https_origin_form_request_uses_configured_scheme() {
        let backend: Arc<dyn AuthBackend> = Arc::new(FakeAuth::default());
        let mut headers = headers();
        headers.insert(header::HOST, HeaderValue::from_static("desk.test:443"));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://desk.test:443"),
        );
        let response = block_on(login(
            Extension(backend),
            Extension(RequestScheme::Https),
            ConnectInfo("127.0.0.1:4000".parse().unwrap()),
            headers,
            Json(LoginBody {
                password: "correct".into(),
                next: Some("https://desk.test:443/dash/tasks".into()),
            }),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"ok": true, "next": "https://desk.test:443/dash/tasks"})
        );
    }

    #[test]
    fn login_rejects_backslash_safe_next() {
        let backend: Arc<dyn AuthBackend> = Arc::new(FakeAuth::default());
        let mut headers = headers();
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://desk.test"));
        let response = block_on(login(
            Extension(backend),
            Extension(RequestScheme::Http),
            ConnectInfo("127.0.0.1:4000".parse().unwrap()),
            headers,
            Json(LoginBody {
                password: "correct".into(),
                next: Some("/\\evil.example".into()),
            }),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": true, "next": "/dash/cos"}));
    }

    #[test]
    fn login_rejects_ascii_controls_in_safe_next() {
        for control in ['\t', '\n', '\r'] {
            let backend: Arc<dyn AuthBackend> = Arc::new(FakeAuth::default());
            let mut headers = headers();
            headers.insert(header::ORIGIN, HeaderValue::from_static("http://desk.test"));
            let response = block_on(login(
                Extension(backend),
                Extension(RequestScheme::Http),
                ConnectInfo("127.0.0.1:4000".parse().unwrap()),
                headers,
                Json(LoginBody {
                    password: "correct".into(),
                    next: Some(format!("/{control}/evil.example")),
                }),
            ));
            let (status, body, _) = body_json(response);
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body, json!({"ok": true, "next": "/dash/cos"}));
        }
    }

    #[test]
    fn login_rejects_cross_origin_with_python_error_shape() {
        let backend: Arc<dyn AuthBackend> = Arc::new(FakeAuth::default());
        let mut headers = headers();
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://other.test"),
        );
        let response = block_on(login(
            Extension(backend),
            Extension(RequestScheme::Http),
            ConnectInfo("127.0.0.1:4000".parse().unwrap()),
            headers,
            Json(LoginBody {
                password: "correct".into(),
                next: None,
            }),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "Cross-origin request refused"}));
    }

    #[test]
    fn login_internal_error_does_not_leak_backend_details() {
        let fake = Arc::new(FakeAuth {
            login_error: Mutex::new(Some(AuthError::Internal(
                "credential database path and secret".into(),
            ))),
            ..FakeAuth::default()
        });
        let backend: Arc<dyn AuthBackend> = fake;
        let mut headers = headers();
        headers.insert(header::ORIGIN, HeaderValue::from_static("http://desk.test"));
        let response = block_on(login(
            Extension(backend),
            Extension(RequestScheme::Http),
            ConnectInfo("127.0.0.1:4000".parse().unwrap()),
            headers,
            Json(LoginBody {
                password: "correct".into(),
                next: None,
            }),
        ));
        let (status, body, response_headers) = body_text(response);
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, "Internal Server Error");
        assert!(!body.contains("credential database path and secret"));
        assert_eq!(
            response_headers
                .get(header::CONTENT_TYPE)
                .unwrap()
                .to_str()
                .unwrap(),
            "text/plain; charset=utf-8"
        );
    }

    #[test]
    fn logout_requires_auth_and_deletes_session_cookie() {
        let unauthorized = Arc::new(FakeAuth {
            identity: Mutex::new(Err(AuthError::SignInRequired)),
            ..FakeAuth::default()
        });
        let backend: Arc<dyn AuthBackend> = unauthorized;
        let mut headers = headers();
        headers.insert(header::COOKIE, HeaderValue::from_static("vp_session=old"));
        let response = block_on(logout(
            Extension(backend),
            Extension(RequestScheme::Http),
            headers.clone(),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, json!({"detail": "Sign in required"}));

        let authorized = Arc::new(FakeAuth::default());
        let backend: Arc<dyn AuthBackend> = authorized.clone();
        let response = block_on(logout(
            Extension(backend),
            Extension(RequestScheme::Http),
            headers,
        ));
        let (status, body, response_headers) = body_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": true}));
        assert!(response_headers
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("Max-Age=0"));
        assert_eq!(
            *authorized.logged_out.lock().unwrap(),
            Some(Some("old".into()))
        );
    }

    #[test]
    fn password_change_is_owner_only_and_keeps_current_session() {
        let fake = Arc::new(FakeAuth::default());
        let backend: Arc<dyn AuthBackend> = fake.clone();
        let mut headers = headers();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("vp_session=owner-session"),
        );
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let response = block_on(change_password(
            Extension(backend),
            Extension(RequestScheme::Http),
            headers.clone(),
            Bytes::from_static(
                br#"{"current_password":"old","new_password":"a sufficiently long password"}"#,
            ),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"ok": true}));
        assert_eq!(
            *fake.changed_password.lock().unwrap(),
            Some((
                "old".into(),
                "a sufficiently long password".into(),
                Some("owner-session".into())
            ))
        );

        let api = Arc::new(FakeAuth {
            identity: Mutex::new(Ok(Identity::Api)),
            ..FakeAuth::default()
        });
        let backend: Arc<dyn AuthBackend> = api.clone();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let response = block_on(change_password(
            Extension(backend),
            Extension(RequestScheme::Http),
            headers,
            Bytes::from_static(
                br#"{"current_password":"old","new_password":"a sufficiently long password"}"#,
            ),
        ));
        let (status, body, _) = body_json(response);
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body, json!({"detail": "Owner sign-in required"}));
        assert!(api.changed_password.lock().unwrap().is_none());
    }
}
