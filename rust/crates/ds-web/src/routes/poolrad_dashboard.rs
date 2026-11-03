//! Poolrad dashboard refresh and static report routes from `web/app.py`.
//!
//! The renderer and dashboard filesystem stay behind an injected backend. The
//! route layer enforces authentication, the report allowlist, and response headers.

use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Extension, Path, Request},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::json;

use super::auth::{AuthBackend, AuthError, RequestScheme};

const DASHBOARD_CSP: &str = "sandbox allow-scripts; default-src 'none'; style-src 'unsafe-inline'; script-src 'unsafe-inline'; frame-ancestors 'self'";
const DASHBOARD_REPORTS: [&str; 2] = [
    "SLOT_A_FIRST_EGA_ROW64.md",
    "SLOT_A_ROW6_COL38_PRINTER_PAGE.md",
];

/// Filesystem and renderer seam for these three read/refresh endpoints.
pub trait PoolradDashboardBackend: Send + Sync + 'static {
    fn dashboard_available(&self) -> bool;
    /// Return the published checkpoint OID, or `None` when publication was unavailable.
    fn refresh(&self) -> Result<Option<String>, String>;
    fn dashboard_html(&self) -> Option<Vec<u8>>;
    fn report(&self, filename: &str) -> Option<Vec<u8>>;
}

pub fn router(
    backend: Arc<dyn PoolradDashboardBackend>,
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
) -> Router {
    Router::new()
        .route(
            "/api/poolrad-dashboard/refresh",
            post(refresh_poolrad_dashboard),
        )
        .route(
            "/api/poolrad-dashboard/dashboard.html",
            get(poolrad_dashboard_page),
        )
        .route(
            "/api/poolrad-dashboard/state/reports/{filename}",
            get(poolrad_dashboard_report),
        )
        .layer(Extension(scheme))
        .layer(Extension(auth))
        .layer(Extension(backend))
}

async fn refresh_poolrad_dashboard(
    Extension(backend): Extension<Arc<dyn PoolradDashboardBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    if !backend.dashboard_available() {
        return detail_response(StatusCode::NOT_FOUND, "Poolrad dashboard unavailable");
    }
    match backend.refresh() {
        Ok(published_checkpoint) => Json(json!({
            "refreshed": true,
            "published_checkpoint": published_checkpoint
        }))
        .into_response(),
        Err(_) => detail_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "Poolrad dashboard refresh failed",
        ),
    }
}

async fn poolrad_dashboard_page(
    Extension(backend): Extension<Arc<dyn PoolradDashboardBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let Some(html) = backend.dashboard_html() else {
        return detail_response(StatusCode::NOT_FOUND, "Poolrad dashboard unavailable");
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
    response.headers_mut().insert(
        header::HeaderName::from_static("content-security-policy"),
        HeaderValue::from_static(DASHBOARD_CSP),
    );
    if let Ok(length) = HeaderValue::from_str(&length.to_string()) {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, length);
    }
    response
}

async fn poolrad_dashboard_report(
    Extension(backend): Extension<Arc<dyn PoolradDashboardBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    Path(filename): Path<String>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    if !DASHBOARD_REPORTS.contains(&filename.as_str()) {
        return detail_response(StatusCode::NOT_FOUND, "Report unavailable");
    }
    let Some(contents) = backend.report(&filename) else {
        return detail_response(StatusCode::NOT_FOUND, "Report unavailable");
    };
    let length = contents.len();
    let mut response = Response::new(Body::from(contents));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
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

async fn authenticate(
    auth: &dyn AuthBackend,
    headers: &HeaderMap,
    scheme: RequestScheme,
) -> Result<(), Response> {
    if let Some(value) = headers.get(header::ORIGIN) {
        let origin = value.to_str().map_err(|_| cross_origin())?;
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let expected = match scheme {
            RequestScheme::Http => "http",
            RequestScheme::Https => "https",
        };
        if !origin.is_empty() && origin != format!("{expected}://{host}") {
            return Err(cross_origin());
        }
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let cookie = cookie_value(headers, "vp_session");
    auth.identity(bearer, cookie.as_deref())
        .await
        .map(|_| ())
        .map_err(auth_error_response)
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

fn cross_origin() -> Response {
    detail_response(StatusCode::FORBIDDEN, "Cross-origin request refused")
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
    use std::{
        collections::HashMap,
        future::Future,
        pin::pin,
        sync::Mutex,
        task::{Context, Poll, Waker},
    };

    use axum::{
        body::{to_bytes, Body},
        http::Request,
    };
    use serde_json::Value;
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
            let valid = bearer.or(cookie) == Some("owner-token");
            Box::pin(async move {
                if valid {
                    Ok(super::super::auth::Identity::Owner)
                } else {
                    Err(AuthError::SignInRequired)
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

    struct FakeDashboard {
        available: bool,
        refresh_result: Result<Option<String>, String>,
        html: Option<Vec<u8>>,
        reports: HashMap<String, Vec<u8>>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeDashboard {
        fn ready() -> Self {
            Self {
                available: true,
                refresh_result: Ok(Some("a".repeat(40))),
                html: Some(b"<html>dashboard</html>".to_vec()),
                reports: HashMap::from([("SLOT_A_FIRST_EGA_ROW64.md".into(), b"report".to_vec())]),
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl PoolradDashboardBackend for FakeDashboard {
        fn dashboard_available(&self) -> bool {
            self.available
        }
        fn refresh(&self) -> Result<Option<String>, String> {
            self.calls.lock().unwrap().push("refresh".into());
            self.refresh_result.clone()
        }
        fn dashboard_html(&self) -> Option<Vec<u8>> {
            self.calls.lock().unwrap().push("html".into());
            self.html.clone()
        }
        fn report(&self, filename: &str) -> Option<Vec<u8>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("report:{filename}"));
            self.reports.get(filename).cloned()
        }
    }

    fn app(backend: Arc<FakeDashboard>) -> Router {
        router(backend, Arc::new(FakeAuth), RequestScheme::Http)
    }

    fn request(method: &str, uri: &str, authenticated: bool) -> Request<Body> {
        let mut request = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "desk.test")
            .header("origin", "http://desk.test");
        if authenticated {
            request = request.header("authorization", "Bearer owner-token");
        }
        request.body(Body::empty()).unwrap()
    }

    fn json_response(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let bytes = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[test]
    fn refresh_returns_published_checkpoint_and_maps_unavailable_cases() {
        let backend = Arc::new(FakeDashboard::ready());
        let response = block_on(app(backend.clone()).oneshot(request(
            "POST",
            "/api/poolrad-dashboard/refresh",
            true,
        )))
        .unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::OK,
                json!({"refreshed":true,"published_checkpoint":"a".repeat(40)})
            )
        );
        assert_eq!(*backend.calls.lock().unwrap(), vec!["refresh"]);

        let missing = Arc::new(FakeDashboard {
            available: false,
            ..FakeDashboard::ready()
        });
        let response =
            block_on(app(missing).oneshot(request("POST", "/api/poolrad-dashboard/refresh", true)))
                .unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::NOT_FOUND,
                json!({"detail":"Poolrad dashboard unavailable"})
            )
        );
        let failed = Arc::new(FakeDashboard {
            refresh_result: Err("renderer error".into()),
            ..FakeDashboard::ready()
        });
        let response =
            block_on(app(failed).oneshot(request("POST", "/api/poolrad-dashboard/refresh", true)))
                .unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"detail":"Poolrad dashboard refresh failed"})
            )
        );
    }

    #[test]
    fn dashboard_page_has_exact_content_security_and_no_store_headers() {
        let backend = Arc::new(FakeDashboard::ready());
        let response = block_on(app(backend).oneshot(request(
            "GET",
            "/api/poolrad-dashboard/dashboard.html",
            true,
        )))
        .unwrap();
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
        assert_eq!(response.headers()["content-security-policy"], DASHBOARD_CSP);
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "22");
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "<html>dashboard</html>"
        );
    }

    #[test]
    fn dashboard_page_missing_is_not_found() {
        let backend = Arc::new(FakeDashboard {
            html: None,
            ..FakeDashboard::ready()
        });
        let response = block_on(app(backend).oneshot(request(
            "GET",
            "/api/poolrad-dashboard/dashboard.html",
            true,
        )))
        .unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::NOT_FOUND,
                json!({"detail":"Poolrad dashboard unavailable"})
            )
        );
    }

    #[test]
    fn report_allowlist_controls_reads_and_reports_are_no_store_text() {
        let backend = Arc::new(FakeDashboard::ready());
        let response = block_on(app(backend.clone()).oneshot(request(
            "GET",
            "/api/poolrad-dashboard/state/reports/SLOT_A_FIRST_EGA_ROW64.md",
            true,
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "6");
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "report"
        );
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["report:SLOT_A_FIRST_EGA_ROW64.md"]
        );

        let response = block_on(app(backend.clone()).oneshot(request(
            "GET",
            "/api/poolrad-dashboard/state/reports/secret.txt",
            true,
        )))
        .unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::NOT_FOUND,
                json!({"detail":"Report unavailable"})
            )
        );
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn absent_allowlisted_report_and_unauthenticated_requests_are_refused() {
        let backend = Arc::new(FakeDashboard::ready());
        let response = block_on(app(backend.clone()).oneshot(request(
            "GET",
            "/api/poolrad-dashboard/state/reports/SLOT_A_ROW6_COL38_PRINTER_PAGE.md",
            true,
        )))
        .unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::NOT_FOUND,
                json!({"detail":"Report unavailable"})
            )
        );
        let response = block_on(app(backend.clone()).oneshot(request(
            "GET",
            "/api/poolrad-dashboard/dashboard.html",
            false,
        )))
        .unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::UNAUTHORIZED,
                json!({"detail":"Sign in required"})
            )
        );
        assert_eq!(
            *backend.calls.lock().unwrap(),
            vec!["report:SLOT_A_ROW6_COL38_PRINTER_PAGE.md"]
        );
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
        let backend = Arc::new(FakeDashboard::ready());
        let mut request = request("GET", "/api/poolrad-dashboard/dashboard.html", false);
        request.headers_mut().append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=owner-token; vp_session=invalid"),
        );
        let response = block_on(app(backend.clone()).oneshot(request)).unwrap();

        assert_eq!(
            json_response(response),
            (
                StatusCode::UNAUTHORIZED,
                json!({"detail":"Sign in required"})
            )
        );
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn auth_accepts_quoted_cookie_from_later_cookie_header() {
        let backend = Arc::new(FakeDashboard::ready());
        let mut request = request("GET", "/api/poolrad-dashboard/dashboard.html", false);
        request.headers_mut().append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=invalid"),
        );
        request.headers_mut().append(
            header::COOKIE,
            HeaderValue::from_static(r#"vp_session="owner-token""#),
        );
        let response = block_on(app(backend.clone()).oneshot(request)).unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }
    #[test]
    fn auth_bearer_precedes_cookie_value() {
        let backend = Arc::new(FakeDashboard::ready());
        let mut request = request("GET", "/api/poolrad-dashboard/dashboard.html", true);
        request.headers_mut().append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=invalid"),
        );
        let response = block_on(app(backend.clone()).oneshot(request)).unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
    }
}
