//! Search and attachment routes ported from `web/app.py`.
//!
//! Database and upload-root access remain behind the injected backend. This module
//! owns query/form parsing, authentication, response headers, and pagination.

use std::sync::Arc;

use axum::{
    body::{to_bytes, Body},
    extract::{Extension, Path, Request},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use super::auth::{AuthBackend, AuthError, RequestScheme};

const MAX_UPLOAD: usize = 25 * 1024 * 1024;
const MAX_MULTIPART_OVERHEAD: usize = 64 * 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SearchAttachmentsError {
    Http { status: StatusCode, detail: String },
    Failure,
}

impl SearchAttachmentsError {
    fn into_response(self) -> Response {
        match self {
            Self::Http { status, detail } => detail_response(status, detail),
            Self::Failure => internal_error(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UploadPart {
    pub filename: String,
    /// `None` is retained for the response, while storage uses the fallback MIME type.
    pub response_mime: Option<String>,
    pub storage_mime: String,
    pub bytes: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadFile {
    pub filename: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

/// Search and attachment storage operations supplied by the application runtime.
pub trait SearchAttachmentsBackend: Send + Sync + 'static {
    fn known_channel(&self, channel: &str) -> bool;
    /// Return up to `limit` message objects, already expanded with `_message` attachments.
    fn search_messages(
        &self,
        pattern: &str,
        channel: Option<&str>,
        before: i64,
        limit: usize,
    ) -> Result<Vec<Value>, SearchAttachmentsError>;
    fn save_upload(&self, upload: UploadPart) -> Result<Value, SearchAttachmentsError>;
    fn attachment(&self, id: &str) -> Result<Option<DownloadFile>, SearchAttachmentsError>;
}

pub fn router(
    backend: Arc<dyn SearchAttachmentsBackend>,
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
) -> Router {
    Router::new()
        .route("/api/search", get(search))
        .route("/api/uploads", post(upload))
        .route("/api/uploads/{attachment_id}", get(download))
        .layer(Extension(scheme))
        .layer(Extension(auth))
        .layer(Extension(backend))
}

async fn search(
    Extension(backend): Extension<Arc<dyn SearchAttachmentsBackend>>,
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
    let Some(text) = query.get("q") else {
        return validation_error(json!({
            "type": "missing", "loc": ["query", "q"], "msg": "Field required", "input": null
        }));
    };
    let text = text.trim();
    let before = match query.get("before") {
        Some(value) => match value.parse::<i64>() {
            Ok(before) if before != 0 => before,
            Ok(_) => i64::MAX,
            Err(_) => return query_integer_error("before", value),
        },
        _ => i64::MAX,
    };
    if text.chars().count() < 2 {
        return Json(json!({"items": [], "has_more": false})).into_response();
    }
    let channel = query
        .get("channel")
        .map(String::as_str)
        .filter(|value| !value.is_empty());
    if let Some(channel) = channel {
        if !backend.known_channel(channel) {
            return detail_response(StatusCode::NOT_FOUND, "Unknown channel");
        }
    }
    let rows = match backend.search_messages(&format!("%{text}%"), channel, before, 51) {
        Ok(rows) => rows,
        Err(error) => return error.into_response(),
    };
    let has_more = rows.len() > 50;
    let next_before = if rows.len() >= 50 {
        rows.get(49)
            .and_then(|row| row.get("id"))
            .cloned()
            .unwrap_or(Value::Null)
    } else {
        Value::Null
    };
    Json(json!({
        "items": rows.into_iter().take(50).collect::<Vec<_>>(),
        "has_more": has_more,
        "next_before": next_before
    }))
    .into_response()
}

async fn upload(
    Extension(backend): Extension<Arc<dyn SearchAttachmentsBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    let headers = request.headers().clone();
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    let boundary = multipart_boundary(content_type);
    if content_type.split(';').next().is_some_and(|media_type| {
        media_type
            .trim()
            .eq_ignore_ascii_case("multipart/form-data")
    }) && boundary.is_none()
    {
        return detail_response(StatusCode::BAD_REQUEST, "Invalid multipart request");
    }
    let Some(boundary) = boundary else {
        return invalid_upload();
    };
    let bytes = match to_bytes(request.into_body(), MAX_UPLOAD + MAX_MULTIPART_OVERHEAD).await {
        Ok(bytes) => bytes,
        Err(_) => return too_large(),
    };
    let Some((filename, mime, file_bytes)) = multipart_file(&bytes, &boundary) else {
        return invalid_upload();
    };
    if let Err(response) = authenticate(auth.as_ref(), &headers, scheme).await {
        return response;
    }
    if file_bytes.len() > MAX_UPLOAD {
        return too_large();
    }
    let filename = sanitize_filename(&filename);
    let upload = UploadPart {
        filename,
        storage_mime: mime
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_owned()),
        response_mime: mime,
        bytes: file_bytes,
    };
    match backend.save_upload(upload) {
        Ok(value) => Json(value).into_response(),
        Err(error) => error.into_response(),
    }
}

async fn download(
    Extension(backend): Extension<Arc<dyn SearchAttachmentsBackend>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    Path(id): Path<String>,
    request: Request,
) -> Response {
    if let Err(response) = authenticate(auth.as_ref(), request.headers(), scheme).await {
        return response;
    }
    let query = match parse_query(request.uri().query()) {
        Ok(query) => query,
        Err(()) => return detail_response(StatusCode::BAD_REQUEST, "invalid query"),
    };
    let preview = match query.get("preview") {
        None => false,
        Some(value) => match parse_bool(value) {
            Some(value) => value,
            None => return query_bool_error("preview", value),
        },
    };
    let file = match backend.attachment(&id) {
        Ok(Some(file)) => file,
        Ok(None) => return detail_response(StatusCode::NOT_FOUND, "Attachment not found"),
        Err(error) => return error.into_response(),
    };
    let inline = preview
        && matches!(
            file.mime.as_str(),
            "image/png" | "image/jpeg" | "image/gif" | "image/webp"
        );
    let disposition = content_disposition(&file.filename, inline);
    let content_length = file.bytes.len();
    let mut response = Response::new(Body::from(file.bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&file.mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition)
            .unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if let Ok(length) = HeaderValue::from_str(&content_length.to_string()) {
        response
            .headers_mut()
            .insert(header::CONTENT_LENGTH, length);
    }
    response
}

fn multipart_boundary(content_type: &str) -> Option<String> {
    let (media_type, params) = content_type.split_once(';')?;
    if !media_type
        .trim()
        .eq_ignore_ascii_case("multipart/form-data")
    {
        return None;
    }
    params.split(';').find_map(|param| {
        let (name, value) = param.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("boundary") {
            return None;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        (!value.is_empty()).then(|| value.to_owned())
    })
}

fn multipart_file(body: &[u8], boundary: &str) -> Option<(String, Option<String>, Vec<u8>)> {
    let delimiter = format!("--{boundary}").into_bytes();
    let header_end = body.windows(4).position(|window| window == b"\r\n\r\n")?;
    if !body.starts_with(&delimiter) || header_end > MAX_MULTIPART_OVERHEAD {
        return None;
    }
    let headers = std::str::from_utf8(&body[delimiter.len()..header_end]).ok()?;
    let mut filename = None;
    let mut mime = None;
    for line in headers.trim_start_matches("\r\n").split("\r\n") {
        if let Some(value) = line
            .strip_prefix("Content-Disposition:")
            .or_else(|| line.strip_prefix("content-disposition:"))
        {
            if !value.to_ascii_lowercase().contains("form-data") || !value.contains("name=\"file\"")
            {
                return None;
            }
            filename = disposition_filename(value);
        } else if let Some(value) = line
            .strip_prefix("Content-Type:")
            .or_else(|| line.strip_prefix("content-type:"))
        {
            mime = Some(value.trim().to_owned());
        }
    }
    let filename = filename?;
    let data_start = header_end.checked_add(4)?;
    let tail = &body[data_start..];
    let mut marker = b"\r\n".to_vec();
    marker.extend_from_slice(&delimiter);
    let data_end = tail
        .windows(marker.len())
        .enumerate()
        .find_map(|(at, window)| {
            if window != marker.as_slice() {
                return None;
            }
            let suffix = &tail[at + marker.len()..];
            (suffix.starts_with(b"--") || suffix.starts_with(b"\r\n")).then_some(at)
        })?;
    Some((filename, mime, tail[..data_end].to_vec()))
}

fn disposition_filename(value: &str) -> Option<String> {
    value.split(';').find_map(|part| {
        let (name, value) = part.trim().split_once('=')?;
        if !name.trim().eq_ignore_ascii_case("filename") {
            return None;
        }
        let value = value.trim();
        let value = value
            .strip_prefix('"')
            .and_then(|v| v.strip_suffix('"'))
            .unwrap_or(value);
        Some(value.replace("\\\"", "\"").replace("\\\\", "\\"))
    })
}

fn sanitize_filename(filename: &str) -> String {
    let basename = filename.rsplit('/').next().unwrap_or("upload");
    let cleaned: String = basename
        .chars()
        .map(|ch| {
            if ch.is_alphanumeric() || "_. ()-".contains(ch) {
                ch
            } else {
                '_'
            }
        })
        .take(180)
        .collect();
    if cleaned.is_empty() {
        "upload".to_owned()
    } else {
        cleaned
    }
}

fn content_disposition(filename: &str, inline: bool) -> String {
    let kind = if inline { "inline" } else { "attachment" };
    if filename.is_ascii() {
        let escaped = filename.replace('\\', "\\\\").replace('"', "\\\"");
        return format!("{kind}; filename=\"{escaped}\"");
    }
    let encoded = filename
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"!#$&+-.^_`|~".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    format!("{kind}; filename*=utf-8''{encoded}")
}

async fn authenticate(
    auth: &dyn AuthBackend,
    headers: &HeaderMap,
    scheme: RequestScheme,
) -> Result<(), Response> {
    if let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
    {
        let host = headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        let expected = match scheme {
            RequestScheme::Http => "http",
            RequestScheme::Https => "https",
        };
        if !origin.is_empty() && origin != format!("{expected}://{host}") {
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
        .map(|_| ())
        .map_err(auth_error_response)
}

fn parse_query(raw: Option<&str>) -> Result<std::collections::BTreeMap<String, String>, ()> {
    fn decode(part: &str) -> Result<String, ()> {
        let bytes = part.as_bytes();
        let mut out = Vec::with_capacity(bytes.len());
        let mut at = 0;
        while at < bytes.len() {
            match bytes[at] {
                b'+' => {
                    out.push(b' ');
                    at += 1;
                }
                b'%' if at + 2 < bytes.len() => {
                    let hex = |byte: u8| match byte {
                        b'0'..=b'9' => Some(byte - b'0'),
                        b'a'..=b'f' => Some(byte - b'a' + 10),
                        b'A'..=b'F' => Some(byte - b'A' + 10),
                        _ => None,
                    };
                    out.push(hex(bytes[at + 1]).ok_or(())? * 16 + hex(bytes[at + 2]).ok_or(())?);
                    at += 3;
                }
                b'%' => return Err(()),
                byte => {
                    out.push(byte);
                    at += 1;
                }
            }
        }
        String::from_utf8(out).map_err(|_| ())
    }
    let mut values = std::collections::BTreeMap::new();
    for part in raw.unwrap_or("").split('&').filter(|part| !part.is_empty()) {
        let (key, value) = part.split_once('=').unwrap_or((part, ""));
        values.insert(decode(key)?, decode(value)?);
    }
    Ok(values)
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "true" | "t" | "yes" | "y" | "on" => Some(true),
        "0" | "false" | "f" | "no" | "n" | "off" => Some(false),
        _ => None,
    }
}

fn query_integer_error(name: &str, value: &str) -> Response {
    validation_error(json!({
        "type": "int_parsing", "loc": ["query", name],
        "msg": "Input should be a valid integer, unable to parse string as an integer", "input": value
    }))
}

fn query_bool_error(name: &str, value: &str) -> Response {
    validation_error(json!({
        "type": "bool_parsing", "loc": ["query", name],
        "msg": "Input should be a valid boolean, unable to interpret input", "input": value
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

fn invalid_upload() -> Response {
    validation_error(
        json!({"type": "missing", "loc": ["body", "file"], "msg": "Field required", "input": null}),
    )
}

fn too_large() -> Response {
    detail_response(
        StatusCode::PAYLOAD_TOO_LARGE,
        "Files must be 25 MB or smaller",
    )
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
            Box::pin(async move {
                if bearer.or(cookie) == Some("owner-token") {
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

    #[derive(Default)]
    struct FakeBackend {
        search: Mutex<Vec<(String, Option<String>, i64, usize)>>,
        uploads: Mutex<Vec<UploadPart>>,
        rows: Mutex<Vec<Value>>,
        file: Option<DownloadFile>,
        error: Option<SearchAttachmentsError>,
    }

    impl SearchAttachmentsBackend for FakeBackend {
        fn known_channel(&self, channel: &str) -> bool {
            channel == "workers"
        }
        fn search_messages(
            &self,
            pattern: &str,
            channel: Option<&str>,
            before: i64,
            limit: usize,
        ) -> Result<Vec<Value>, SearchAttachmentsError> {
            self.search.lock().unwrap().push((
                pattern.into(),
                channel.map(str::to_owned),
                before,
                limit,
            ));
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            Ok(self.rows.lock().unwrap().clone())
        }
        fn save_upload(&self, upload: UploadPart) -> Result<Value, SearchAttachmentsError> {
            self.uploads.lock().unwrap().push(upload.clone());
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            Ok(
                json!({"id":"a1","filename":upload.filename,"mime":upload.response_mime,"size":upload.bytes.len()}),
            )
        }
        fn attachment(&self, id: &str) -> Result<Option<DownloadFile>, SearchAttachmentsError> {
            if let Some(error) = &self.error {
                return Err(error.clone());
            }
            Ok((id == "a1").then(|| self.file.clone()).flatten())
        }
    }

    fn app(backend: Arc<FakeBackend>) -> Router {
        router(backend, Arc::new(FakeAuth), RequestScheme::Http)
    }

    fn request(method: &str, uri: &str, body: Body) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header("host", "desk.test")
            .header("origin", "http://desk.test")
            .header("authorization", "Bearer owner-token")
            .body(body)
            .unwrap()
    }

    fn json_response(response: Response) -> (StatusCode, Value) {
        let status = response.status();
        let body = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    #[test]
    fn search_trims_query_and_returns_fifty_rows_with_cursor() {
        let rows = (1..=51).rev().map(|id| json!({"id": id})).collect();
        let backend = Arc::new(FakeBackend {
            rows: Mutex::new(rows),
            ..FakeBackend::default()
        });
        let response = block_on(app(backend.clone()).oneshot(request(
            "GET",
            "/api/search?q=%20sheep%20&channel=workers&before=88",
            Body::empty(),
        )))
        .unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["items"].as_array().unwrap().len(), 50);
        assert_eq!(body["has_more"], true);
        assert_eq!(body["next_before"], 2);
        assert_eq!(
            *backend.search.lock().unwrap(),
            vec![("%sheep%".into(), Some("workers".into()), 88, 51)]
        );
    }

    #[test]
    fn short_search_skips_storage_and_unknown_channel_is_not_found() {
        let backend = Arc::new(FakeBackend::default());
        let short = block_on(app(backend.clone()).oneshot(request(
            "GET",
            "/api/search?q=%20x",
            Body::empty(),
        )))
        .unwrap();
        assert_eq!(
            json_response(short).1,
            json!({"items": [], "has_more": false})
        );
        assert!(backend.search.lock().unwrap().is_empty());
        let unknown = block_on(app(backend.clone()).oneshot(request(
            "GET",
            "/api/search?q=xx&channel=unknown",
            Body::empty(),
        )))
        .unwrap();
        let (status, body) = json_response(unknown);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({"detail":"Unknown channel"}));
    }

    #[test]
    fn search_rejects_empty_before_as_invalid_integer() {
        let backend = Arc::new(FakeBackend::default());
        let response = block_on(app(backend.clone()).oneshot(request(
            "GET",
            "/api/search?q=hi&before=",
            Body::empty(),
        )))
        .unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["detail"][0]["loc"], json!(["query", "before"]));
        assert!(backend.search.lock().unwrap().is_empty());
    }

    #[test]
    fn search_requires_authentication_and_maps_storage_failures() {
        let backend = Arc::new(FakeBackend {
            error: Some(SearchAttachmentsError::Failure),
            ..FakeBackend::default()
        });
        let unauthenticated = Request::builder()
            .method("GET")
            .uri("/api/search?q=hi")
            .header("host", "desk.test")
            .body(Body::empty())
            .unwrap();
        let response = block_on(app(backend.clone()).oneshot(unauthenticated)).unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::UNAUTHORIZED,
                json!({"detail":"Sign in required"})
            )
        );
        let response =
            block_on(app(backend).oneshot(request("GET", "/api/search?q=hi", Body::empty())))
                .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn upload_parses_single_file_part_sanitizes_filename_and_preserves_bytes() {
        let backend = Arc::new(FakeBackend::default());
        let multipart = "--xYz\r\nContent-Disposition: form-data; name=\"file\"; filename=\"../my sheep!.txt\"\r\nContent-Type: text/plain\r\n\r\nhello\r\n--xYz--\r\n";
        let request = Request::builder()
            .method("POST")
            .uri("/api/uploads")
            .header("host", "desk.test")
            .header("origin", "http://desk.test")
            .header("authorization", "Bearer owner-token")
            .header("content-type", "multipart/form-data; boundary=xYz")
            .body(Body::from(multipart))
            .unwrap();
        let response = block_on(app(backend.clone()).oneshot(request)).unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body,
            json!({"id":"a1","filename":"my sheep_.txt","mime":"text/plain","size":5})
        );
        assert_eq!(backend.uploads.lock().unwrap()[0].bytes, b"hello");
        assert_eq!(
            backend.uploads.lock().unwrap()[0].storage_mime,
            "text/plain"
        );
    }

    #[test]
    fn upload_preserves_boundary_like_bytes_with_invalid_suffix() {
        let backend = Arc::new(FakeBackend::default());
        let payload = b"first\r\n--bXsecond";
        let mut multipart =
            b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\n"
                .to_vec();
        multipart.extend_from_slice(payload);
        multipart.extend_from_slice(b"\r\n--b--\r\n");
        let request = Request::builder()
            .method("POST")
            .uri("/api/uploads")
            .header("host", "desk.test")
            .header("origin", "http://desk.test")
            .header("authorization", "Bearer owner-token")
            .header("content-type", "multipart/form-data; boundary=b")
            .body(Body::from(multipart))
            .unwrap();
        let response = block_on(app(backend.clone()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(backend.uploads.lock().unwrap()[0].bytes, payload);
    }

    #[test]
    fn upload_matches_python_multipart_closing_prefix_junk_truncation() {
        for payload in [
            b"alpha\r\n--b--junk\r\nomega".as_slice(),
            b"alpha\r\n--b-- \tjunk\r\nomega".as_slice(),
        ] {
            let backend = Arc::new(FakeBackend::default());
            let mut multipart =
                b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\n"
                    .to_vec();
            multipart.extend_from_slice(payload);
            multipart.extend_from_slice(b"\r\n--b--\r\n");
            let request = Request::builder()
                .method("POST")
                .uri("/api/uploads")
                .header("host", "desk.test")
                .header("origin", "http://desk.test")
                .header("authorization", "Bearer owner-token")
                .header("content-type", "multipart/form-data; boundary=b")
                .body(Body::from(multipart))
                .unwrap();
            let response = block_on(app(backend.clone()).oneshot(request)).unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(backend.uploads.lock().unwrap()[0].bytes, b"alpha");
        }
    }

    #[test]
    fn upload_parses_malformed_multipart_before_authentication() {
        let backend = Arc::new(FakeBackend::default());
        let request = Request::builder()
            .method("POST")
            .uri("/api/uploads")
            .header("host", "desk.test")
            .header("content-type", "multipart/form-data")
            .body(Body::from("ignored"))
            .unwrap();
        let response = block_on(app(backend.clone()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert!(backend.uploads.lock().unwrap().is_empty());
    }

    #[test]
    fn upload_requires_multipart_file_and_maps_storage_error() {
        let missing = Arc::new(FakeBackend::default());
        let request = Request::builder()
            .method("POST")
            .uri("/api/uploads")
            .header("host", "desk.test")
            .header("origin", "http://desk.test")
            .header("authorization", "Bearer owner-token")
            .header("content-type", "application/json")
            .body(Body::from("{}"))
            .unwrap();
        let response = block_on(app(missing.clone()).oneshot(request)).unwrap();
        assert_eq!(json_response(response).0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(missing.uploads.lock().unwrap().is_empty());

        let failed = Arc::new(FakeBackend {
            error: Some(SearchAttachmentsError::Http {
                status: StatusCode::INSUFFICIENT_STORAGE,
                detail: "upload store full".into(),
            }),
            ..FakeBackend::default()
        });
        let multipart = "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nx\r\n--b--\r\n";
        let request = Request::builder()
            .method("POST")
            .uri("/api/uploads")
            .header("host", "desk.test")
            .header("origin", "http://desk.test")
            .header("authorization", "Bearer owner-token")
            .header("content-type", "multipart/form-data; boundary=b")
            .body(Body::from(multipart))
            .unwrap();
        let response = block_on(app(failed).oneshot(request)).unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE);
        assert_eq!(body, json!({"detail":"upload store full"}));
    }

    #[test]
    fn download_sets_inline_policy_nosniff_and_content_disposition() {
        let backend = Arc::new(FakeBackend {
            file: Some(DownloadFile {
                filename: "sheep.png".into(),
                mime: "image/png".into(),
                bytes: b"png".to_vec(),
            }),
            ..FakeBackend::default()
        });
        let response = block_on(app(backend).oneshot(request(
            "GET",
            "/api/uploads/a1?preview=true",
            Body::empty(),
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "image/png");
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "inline; filename=\"sheep.png\""
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "png"
        );
    }

    #[test]
    fn download_keeps_non_image_as_attachment_and_missing_is_404() {
        let backend = Arc::new(FakeBackend {
            file: Some(DownloadFile {
                filename: "note.txt".into(),
                mime: "text/plain".into(),
                bytes: b"note".to_vec(),
            }),
            ..FakeBackend::default()
        });
        let response = block_on(app(backend.clone()).oneshot(request(
            "GET",
            "/api/uploads/a1?preview=true",
            Body::empty(),
        )))
        .unwrap();
        assert_eq!(
            response.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"note.txt\""
        );
        let missing =
            block_on(app(backend).oneshot(request("GET", "/api/uploads/nope", Body::empty())))
                .unwrap();
        let (status, body) = json_response(missing);
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body, json!({"detail":"Attachment not found"}));
    }
}
