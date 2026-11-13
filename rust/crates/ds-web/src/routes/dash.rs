//! Standalone dashboard routes from `web/app.py`.
//!
//! The private UI bundle is supplied through an injected backend. Session checks
//! reuse the web auth seam, and handlers preserve the legacy dashboard redirects.

use super::response_error::WebResponseError;

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Extension, Path, Request},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Router,
};

use super::auth::{AuthBackend, AuthError, RequestScheme};

const DASH_VIEWS: [&str; 5] = ["poolrad", "operations", "play", "cos", "fleet"];

/// Supplies the private built bundle. Missing index content corresponds to `DIST/index.html`
/// not existing; the runtime owns its actual path and file access.
pub trait DashBackend: Send + Sync + 'static {
    fn index_html(&self) -> Option<Vec<u8>>;
}

pub fn router(
    backend: Arc<dyn DashBackend>,
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
) -> Router {
    Router::new()
        .route("/dash", get(dash_root))
        .route("/dash/", get(dash_root))
        .route("/dash/{*path}", get(dash_route))
        .layer(Extension(scheme))
        .layer(Extension(auth))
        .layer(Extension(backend))
}

/// The bare dashboard path opens the landing view. Projects since t4642: Python sent this to
/// `/dash/cos`, and the owner moved the landing view. Front Desk stays at `/dash/cos`, and the
/// redirects of retired bookmarks (`web_server::legacy_view`) are unchanged.
const DASH_LANDING: &str = "/dash/projects";

async fn dash_root() -> Response {
    redirect(StatusCode::TEMPORARY_REDIRECT, DASH_LANDING)
}

async fn dash_route(
    Extension(backend): Extension<Arc<dyn DashBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    Path(path): Path<String>,
    request: Request,
) -> Response {
    let name = path.trim_matches('/');
    if name != path && DASH_VIEWS.contains(&name) {
        return redirect(StatusCode::TEMPORARY_REDIRECT, &format!("/dash/{name}"));
    }

    match session_valid(auth.as_ref(), request.headers(), scheme).await {
        Ok(true) => {}
        Ok(false) => {
            let path = decode_path(request.uri().path());
            let mut target = path;
            if let Some(query) = request.uri().query().filter(|query| !query.is_empty()) {
                target.push('?');
                target.push_str(query);
            }
            return redirect(
                StatusCode::FOUND,
                &format!("/?next={}", quote_component(&target)),
            );
        }
        Err(response) => return response.into_response(),
    }

    if name == "poolrad" {
        return redirect(StatusCode::TEMPORARY_REDIRECT, "/dash/operations#poolrad");
    }
    let Some(html) = backend.index_html() else {
        return detail_response(StatusCode::NOT_FOUND, "Desk UI is not built");
    };
    let length = html.len();
    let mut response = Response::new(Body::from(html));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(length) = HeaderValue::from_str(&length.to_string()) {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, length);
    }
    response
}

async fn session_valid(
    auth: &dyn AuthBackend,
    headers: &HeaderMap,
    _scheme: RequestScheme,
) -> Result<bool, WebResponseError> {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let cookie = cookie_value(headers, "vp_session");
    match auth.identity(bearer, cookie.as_deref()).await {
        Ok(_) => Ok(true),
        Err(
            AuthError::SignInRequired | AuthError::IncorrectPassword | AuthError::TooManyAttempts,
        ) => Ok(false),
        Err(AuthError::Internal(_)) => Err(internal_error().into()),
    }
}

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut cookie = None;
    for header in headers.get_all(header::COOKIE).iter() {
        let value = header
            .as_bytes()
            .iter()
            .map(|byte| char::from(*byte))
            .collect::<String>();
        for part in value.split(';') {
            let (key, value) = part.split_once('=').unwrap_or(("", part));
            if key.trim() == name {
                cookie = Some(unquote_cookie_value(value.trim()));
            }
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

fn decode_path(path: &str) -> String {
    let bytes = path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2])) {
                decoded.push(high * 16 + low);
                index += 3;
                continue;
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).into_owned()
}

fn quote_component(text: &str) -> String {
    text.bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect()
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn redirect(status: StatusCode, location: &str) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = status;
    if let Ok(location) = HeaderValue::from_str(location) {
        response.headers_mut().insert(header::LOCATION, location);
    }
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, HeaderValue::from_static("0"));
    response
}

fn detail_response(status: StatusCode, detail: impl Into<String>) -> Response {
    (
        status,
        axum::Json(serde_json::json!({"detail": detail.into()})),
    )
        .into_response()
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
        http::Request,
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
        ) -> super::super::auth::AuthFuture<'a, Result<super::super::auth::Identity, AuthError>>
        {
            let result = match bearer {
                Some("token") => Ok(super::super::auth::Identity::Api),
                Some("session") => Ok(super::super::auth::Identity::Owner),
                Some("broken") => Err(AuthError::Internal("auth db".into())),
                _ => match cookie {
                    Some("session") => Ok(super::super::auth::Identity::Owner),
                    Some("broken") => Err(AuthError::Internal("auth db".into())),
                    _ => Err(AuthError::SignInRequired),
                },
            };
            Box::pin(async move { result })
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
    struct FakeDash {
        html: Option<Vec<u8>>,
        reads: Mutex<usize>,
    }

    impl DashBackend for FakeDash {
        fn index_html(&self) -> Option<Vec<u8>> {
            *self.reads.lock().unwrap() += 1;
            self.html.clone()
        }
    }

    fn app(html: Option<&'static [u8]>) -> (Router, Arc<FakeDash>) {
        let backend = Arc::new(FakeDash {
            html: html.map(|bytes| bytes.to_vec()),
            ..FakeDash::default()
        });
        (
            router(backend.clone(), Arc::new(FakeAuth), RequestScheme::Http),
            backend,
        )
    }

    fn request(uri: &str, token: Option<&str>) -> Request<Body> {
        let mut request = Request::builder()
            .method("GET")
            .uri(uri)
            .header("host", "desk.test");
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        request.body(Body::empty()).unwrap()
    }

    fn redirect_response(response: Response) -> (StatusCode, String) {
        let status = response.status();
        let location = response
            .headers()
            .get(header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        (status, location)
    }

    #[test]
    fn dash_root_redirects_both_path_forms_to_the_projects_landing_view() {
        let (app, backend) = app(Some(b"index"));
        for uri in ["/dash", "/dash/"] {
            let response = block_on(app.clone().oneshot(request(uri, None))).unwrap();
            assert_eq!(
                redirect_response(response),
                (StatusCode::TEMPORARY_REDIRECT, "/dash/projects".into())
            );
        }
        assert_eq!(*backend.reads.lock().unwrap(), 0);
    }

    #[test]
    fn dash_route_canonicalizes_known_view_before_authentication() {
        let (app, backend) = app(Some(b"index"));
        let response = block_on(app.oneshot(request("/dash//operations/", None))).unwrap();
        assert_eq!(
            redirect_response(response),
            (StatusCode::TEMPORARY_REDIRECT, "/dash/operations".into())
        );
        assert_eq!(*backend.reads.lock().unwrap(), 0);
    }

    #[test]
    fn dash_route_redirects_missing_session_to_login_with_encoded_return_target() {
        let (app, backend) = app(Some(b"index"));
        let response = block_on(app.oneshot(request("/dash/cos?tab=two%20words", None))).unwrap();
        assert_eq!(
            redirect_response(response),
            (
                StatusCode::FOUND,
                "/?next=%2Fdash%2Fcos%3Ftab%3Dtwo%2520words".into()
            )
        );
        assert_eq!(*backend.reads.lock().unwrap(), 0);
    }

    #[test]
    fn authenticated_poolrad_alias_redirects_to_operations_anchor() {
        let (app, backend) = app(Some(b"index"));
        let response = block_on(app.oneshot(request("/dash/poolrad", Some("token")))).unwrap();
        assert_eq!(
            redirect_response(response),
            (
                StatusCode::TEMPORARY_REDIRECT,
                "/dash/operations#poolrad".into()
            )
        );
        assert_eq!(*backend.reads.lock().unwrap(), 0);
    }

    #[test]
    fn authenticated_dashboard_route_serves_index_with_private_headers() {
        let (app, backend) = app(Some(b"<html>desk</html>"));
        let response =
            block_on(app.oneshot(request("/dash/custom/path", Some("session")))).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/html; charset=utf-8"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "17");
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "<html>desk</html>"
        );
        assert_eq!(*backend.reads.lock().unwrap(), 1);
    }

    #[test]
    fn missing_bundle_is_404_and_auth_backend_failure_is_generic_500() {
        let (missing_app, _) = app(None);
        let response =
            block_on(missing_app.oneshot(request("/dash/cos", Some("session")))).unwrap();
        let status = response.status();
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        assert_eq!(status, StatusCode::NOT_FOUND);
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["detail"], "Desk UI is not built");

        let (app, backend) = app(Some(b"index"));
        let response = block_on(app.oneshot(request("/dash/cos", Some("broken")))).unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(*backend.reads.lock().unwrap(), 0);
    }

    #[test]
    fn cookie_value_matches_starlette_last_duplicate_and_quoted_values() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static(
                r#"vp_session=first; theme=dark; vp_session="session\-token""#,
            ),
        );

        assert_eq!(
            Some("session-token".to_owned()),
            cookie_value(&headers, "vp_session")
        );
    }

    #[test]
    fn cookie_value_matches_starlette_across_cookie_header_fields() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=first-header"),
        );
        headers.append(
            header::COOKIE,
            HeaderValue::from_static(r#"vp_session="second-header""#),
        );

        assert_eq!(
            Some("second-header".to_owned()),
            cookie_value(&headers, "vp_session")
        );
    }

    #[test]
    fn auth_uses_last_duplicate_cookie_without_bearer() {
        let (app, backend) = app(Some(b"index"));
        let mut request = request("/dash/custom/path", None);
        request.headers_mut().append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=session; vp_session=broken"),
        );
        let response = block_on(app.oneshot(request)).unwrap();

        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(*backend.reads.lock().unwrap(), 0);
    }

    #[test]
    fn auth_accepts_quoted_cookie_from_later_cookie_header() {
        let (app, backend) = app(Some(b"index"));
        let mut request = request("/dash/custom/path", None);
        request.headers_mut().append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=broken"),
        );
        request.headers_mut().append(
            header::COOKIE,
            HeaderValue::from_static(r#"vp_session="session""#),
        );
        let response = block_on(app.oneshot(request)).unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*backend.reads.lock().unwrap(), 1);
    }

    #[test]
    fn auth_bearer_precedes_cookie_value() {
        let (app, backend) = app(Some(b"index"));
        let mut request = request("/dash/custom/path", Some("token"));
        request.headers_mut().append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=broken"),
        );
        let response = block_on(app.oneshot(request)).unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*backend.reads.lock().unwrap(), 1);
    }

    #[test]
    fn auth_falls_back_to_session_cookie_after_invalid_bearer() {
        let (app, backend) = app(Some(b"index"));
        let mut request = request("/dash/custom/path", Some("expired-bearer"));
        request.headers_mut().append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=session"),
        );
        let response = block_on(app.oneshot(request)).unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*backend.reads.lock().unwrap(), 1);
    }
}
