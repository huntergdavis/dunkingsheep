//! Search and attachment routes ported from `web/app.py`.
//!
//! Database and upload-root access remain behind the injected backend. This module
//! owns query/form parsing, authentication, response headers, and pagination.

use std::{
    collections::hash_map::RandomState,
    hash::{BuildHasher, Hasher},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

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
    /// Required values from the same filesystem stat used to read `bytes`.
    /// Runtime adapters must not synthesize defaults when stat metadata is absent.
    pub metadata: DownloadMetadata,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadMetadata {
    pub content_length: u64,
    /// RFC 7231 HTTP-date formatted from the file's stat mtime.
    pub last_modified: String,
    /// Strong ETag generated from the file's stat metadata.
    pub etag: String,
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
        .route(
            "/api/uploads/{attachment_id}",
            get(download).head(download_head_not_allowed),
        )
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
    download_response(file, &disposition, request.headers())
}

async fn download_head_not_allowed() -> Response {
    let mut response = detail_response(StatusCode::METHOD_NOT_ALLOWED, "Method Not Allowed");
    response
        .headers_mut()
        .insert(header::ALLOW, HeaderValue::from_static("GET"));
    response
}

fn download_response(
    file: DownloadFile,
    disposition: &str,
    request_headers: &HeaderMap,
) -> Response {
    let Ok(content_length) = usize::try_from(file.metadata.content_length) else {
        return internal_error();
    };
    if content_length != file.bytes.len() {
        return internal_error();
    }

    let content_type = file_response_content_type(&file.mime);
    let range = request_headers
        .get(header::RANGE)
        .map(|value| value.to_str().unwrap_or_default());
    let if_range = request_headers
        .get(header::IF_RANGE)
        .map(|value| value.to_str().unwrap_or_default());
    let use_range = range.is_some()
        && if_range.is_none_or(|value| {
            !value.starts_with("W/")
                && (value == file.metadata.last_modified || value == file.metadata.etag)
        });

    let mut response = Response::new(Body::empty());
    let headers = response.headers_mut();
    let content_length_header = file.metadata.content_length.to_string();
    let metadata_headers = [
        (header::CONTENT_TYPE, content_type.as_str()),
        (header::CONTENT_DISPOSITION, disposition),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::ACCEPT_RANGES, "bytes"),
        (header::CONTENT_LENGTH, content_length_header.as_str()),
        (header::LAST_MODIFIED, file.metadata.last_modified.as_str()),
        (header::ETAG, file.metadata.etag.as_str()),
    ];
    for (name, value) in metadata_headers {
        let Ok(value) = HeaderValue::from_str(value) else {
            return internal_error();
        };
        headers.insert(name, value);
    }

    if !use_range {
        *response.status_mut() = StatusCode::OK;
        *response.body_mut() = Body::from(file.bytes);
        return response;
    }

    let parsed = match parse_byte_ranges(range.unwrap_or_default(), content_length) {
        Ok(ranges) => ranges,
        Err(ByteRangeError::Malformed(message)) => {
            return range_error(StatusCode::BAD_REQUEST, &message, None)
        }
        Err(ByteRangeError::Unsatisfiable) => {
            return range_error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "",
                Some(format!("bytes */{}", file.metadata.content_length)),
            )
        }
    };
    match parsed.as_slice() {
        [] => range_error(
            StatusCode::BAD_REQUEST,
            "Range header: range must be requested",
            None,
        ),
        [range] => single_range_response(response, file, range),
        ranges => multiple_ranges_response(response, file, ranges, &content_type),
    }
}

fn file_response_content_type(mime: &str) -> String {
    if mime.to_ascii_lowercase().starts_with("text/")
        && !mime.to_ascii_lowercase().contains("charset=")
    {
        format!("{mime}; charset=utf-8")
    } else {
        mime.to_owned()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ByteRange {
    start: usize,
    end: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ByteRangeError {
    Malformed(String),
    Unsatisfiable,
}

fn parse_byte_ranges(value: &str, file_size: usize) -> Result<Vec<ByteRange>, ByteRangeError> {
    let Some((units, value)) = value.split_once('=') else {
        return Err(ByteRangeError::Malformed(
            "Range header: range must be requested".into(),
        ));
    };
    if !units.trim().eq_ignore_ascii_case("bytes") {
        return Err(ByteRangeError::Malformed("Only support bytes range".into()));
    }
    if value.matches(',').count() + 1 > 100 {
        return Ok(Vec::new());
    }

    let mut ranges = Vec::new();
    for part in value.split(',') {
        let part = part.trim();
        if part.is_empty() || part == "-" {
            continue;
        }
        let Some((start_text, end_text)) = part.split_once('-') else {
            continue;
        };
        let start_text = start_text.trim();
        let end_text = end_text.trim();
        let parsed = if start_text.is_empty() {
            let Some(suffix) = parse_decimal(end_text) else {
                continue;
            };
            let start = file_size.saturating_sub(suffix);
            (start, file_size)
        } else {
            let Some(start) = parse_decimal(start_text) else {
                continue;
            };
            let end = if end_text.is_empty() {
                file_size
            } else {
                let Some(end_inclusive) = parse_decimal(end_text) else {
                    continue;
                };
                if end_inclusive < file_size {
                    end_inclusive.saturating_add(1)
                } else {
                    file_size
                }
            };
            (start, end)
        };
        if parsed.0 >= file_size {
            return Err(ByteRangeError::Unsatisfiable);
        }
        if parsed.0 >= parsed.1 {
            return Err(ByteRangeError::Malformed(
                "Range header: start must be less than end".into(),
            ));
        }
        ranges.push(ByteRange {
            start: parsed.0,
            end: parsed.1,
        });
    }
    if ranges.is_empty() {
        return Err(ByteRangeError::Malformed(
            "Range header: range must be requested".into(),
        ));
    }
    ranges.sort_by_key(|range| (range.start, range.end));
    let mut merged = Vec::<ByteRange>::with_capacity(ranges.len());
    for range in ranges {
        if let Some(last) = merged.last_mut() {
            if range.start <= last.end {
                last.end = last.end.max(range.end);
                continue;
            }
        }
        merged.push(range);
    }
    Ok(merged)
}

fn parse_decimal(value: &str) -> Option<usize> {
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    Some(value.bytes().fold(0usize, |number, digit| {
        number
            .saturating_mul(10)
            .saturating_add(usize::from(digit - b'0'))
    }))
}

fn range_error(status: StatusCode, message: &str, content_range: Option<String>) -> Response {
    let body = Body::from(message.to_owned());
    let mut response = Response::new(body);
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    let Ok(content_length) = HeaderValue::from_str(&message.len().to_string()) else {
        return internal_error();
    };
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, content_length);
    if let Some(value) = content_range {
        let Ok(value) = HeaderValue::from_str(&value) else {
            return internal_error();
        };
        response.headers_mut().insert(header::CONTENT_RANGE, value);
    }
    response
}

fn single_range_response(
    mut response: Response,
    file: DownloadFile,
    range: &ByteRange,
) -> Response {
    let content_range = format!(
        "bytes {}-{}/{}",
        range.start,
        range.end - 1,
        file.metadata.content_length
    );
    *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    let Ok(content_range) = HeaderValue::from_str(&content_range) else {
        return internal_error();
    };
    let Ok(content_length) = HeaderValue::from_str(&(range.end - range.start).to_string()) else {
        return internal_error();
    };
    response
        .headers_mut()
        .insert(header::CONTENT_RANGE, content_range);
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, content_length);
    *response.body_mut() = Body::from(file.bytes[range.start..range.end].to_vec());
    response
}

fn multiple_ranges_response(
    mut response: Response,
    file: DownloadFile,
    ranges: &[ByteRange],
    content_type: &str,
) -> Response {
    let boundary = range_boundary();
    let mut body = Vec::new();
    for range in ranges {
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Type: {content_type}\r\nContent-Range: bytes {}-{}/{}\r\n\r\n",
                range.start,
                range.end - 1,
                file.metadata.content_length
            )
            .as_bytes(),
        );
        body.extend_from_slice(&file.bytes[range.start..range.end]);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--").as_bytes());
    *response.status_mut() = StatusCode::PARTIAL_CONTENT;
    let Ok(content_type) =
        HeaderValue::from_str(&format!("multipart/byteranges; boundary={boundary}"))
    else {
        return internal_error();
    };
    let Ok(content_length) = HeaderValue::from_str(&body.len().to_string()) else {
        return internal_error();
    };
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, content_type);
    response
        .headers_mut()
        .insert(header::CONTENT_LENGTH, content_length);
    *response.body_mut() = Body::from(body);
    response
}

fn range_boundary() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let counter = NEXT.fetch_add(1, Ordering::Relaxed);
    let first = RandomState::new();
    let mut first_hasher = first.build_hasher();
    first_hasher.write_u128(now);
    first_hasher.write_u64(counter);
    let second = RandomState::new();
    let mut second_hasher = second.build_hasher();
    second_hasher.write_u128(now);
    second_hasher.write_u64(counter);
    format!(
        "{:016x}{:016x}",
        first_hasher.finish(),
        second_hasher.finish()
    )
    .chars()
    .take(26)
    .collect()
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
    if !body.starts_with(&delimiter) {
        return None;
    }
    let mut marker = b"\r\n".to_vec();
    marker.extend_from_slice(&delimiter);
    let mut part_start = 0;
    loop {
        if !body.get(part_start..)?.starts_with(&delimiter) {
            return None;
        }
        let mut header_start = part_start.checked_add(delimiter.len())?;
        let after_delimiter = body.get(header_start..)?;
        if after_delimiter.starts_with(b"--") {
            return None;
        }
        if !after_delimiter.starts_with(b"\r\n") {
            return None;
        }
        header_start = header_start.checked_add(2)?;
        let header_end_relative = body
            .get(header_start..)?
            .windows(4)
            .position(|window| window == b"\r\n\r\n")?;
        let header_end = header_start.checked_add(header_end_relative)?;
        if header_end > MAX_MULTIPART_OVERHEAD {
            return None;
        }
        let headers = std::str::from_utf8(body.get(header_start..header_end)?).ok()?;
        let mut field_name = None;
        let mut filename = None;
        let mut mime = None;
        for line in headers.split("\r\n") {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            if name.trim().eq_ignore_ascii_case("content-disposition") {
                let (field, part_filename) = disposition_parameters(value)?;
                field_name = Some(field);
                filename = part_filename;
            } else if name.trim().eq_ignore_ascii_case("content-type") {
                mime = Some(value.trim().to_owned());
            }
        }
        let data_start = header_end.checked_add(4)?;
        let tail = body.get(data_start..)?;
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
        if field_name.as_deref() == Some("file") {
            let filename = filename?;
            return Some((filename, mime, tail[..data_end].to_vec()));
        }
        let next_delimiter = data_start.checked_add(data_end)?.checked_add(2)?;
        let suffix = body.get(next_delimiter.checked_add(delimiter.len())?..)?;
        if suffix.starts_with(b"--") {
            return None;
        }
        part_start = next_delimiter;
    }
}

fn disposition_parameters(value: &str) -> Option<(String, Option<String>)> {
    let mut parts = Vec::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, character) in value.char_indices() {
        if escaped {
            escaped = false;
        } else if quoted && character == '\\' {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == ';' && !quoted {
            parts.push(value[start..index].trim());
            start = index + character.len_utf8();
        }
    }
    parts.push(value[start..].trim());
    if !parts.first()?.eq_ignore_ascii_case("form-data") {
        return None;
    }
    let mut field_name = None;
    let mut filename = None;
    for part in parts.into_iter().skip(1) {
        let Some((name, raw_value)) = part.split_once('=') else {
            continue;
        };
        let raw_value = raw_value.trim();
        let parsed_value = raw_value
            .strip_prefix('"')
            .and_then(|value| value.strip_suffix('"'))
            .map(|value| value.replace("\\\"", "\"").replace("\\\\", "\\"))
            .unwrap_or_else(|| raw_value.to_owned());
        if name.trim().eq_ignore_ascii_case("name") {
            field_name = Some(parsed_value);
        } else if name.trim().eq_ignore_ascii_case("filename") {
            filename = Some(parsed_value);
        }
    }
    Some((field_name?, filename))
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
    fn upload_selects_file_after_normal_form_field() {
        let backend = Arc::new(FakeBackend::default());
        let multipart = b"--b\r\nContent-Disposition: form-data; name=\"description\"\r\n\r\nnotes\r\n--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\nContent-Type: text/plain\r\n\r\nfile bytes\r\n--b--\r\n";
        let request = Request::builder()
            .method("POST")
            .uri("/api/uploads")
            .header("host", "desk.test")
            .header("origin", "http://desk.test")
            .header("authorization", "Bearer owner-token")
            .header("content-type", "multipart/form-data; boundary=b")
            .body(Body::from(multipart.as_slice()))
            .unwrap();
        let response = block_on(app(backend.clone()).oneshot(request)).unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["filename"], "a.txt");
        assert_eq!(body["size"], 10);
        assert_eq!(backend.uploads.lock().unwrap()[0].bytes, b"file bytes");
    }

    #[test]
    fn upload_matches_mixed_case_part_header_names() {
        let backend = Arc::new(FakeBackend::default());
        let multipart = b"--b\r\ncOnTeNt-dIsPoSiTiOn: form-data; name=\"file\"; filename=\"a.txt\"\r\ncOnTeNt-tYpE: text/plain\r\n\r\nabc\r\n--b--\r\n";
        let request = Request::builder()
            .method("POST")
            .uri("/api/uploads")
            .header("host", "desk.test")
            .header("origin", "http://desk.test")
            .header("authorization", "Bearer owner-token")
            .header("content-type", "multipart/form-data; boundary=b")
            .body(Body::from(multipart.as_slice()))
            .unwrap();
        let response = block_on(app(backend.clone()).oneshot(request)).unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["mime"], "text/plain");
        assert_eq!(backend.uploads.lock().unwrap()[0].bytes, b"abc");
    }

    #[test]
    fn upload_preserves_quoted_semicolon_in_filename_before_sanitizing() {
        let backend = Arc::new(FakeBackend::default());
        let multipart = b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"foo;bar.txt\"\r\n\r\nabc\r\n--b--\r\n";
        let request = Request::builder()
            .method("POST")
            .uri("/api/uploads")
            .header("host", "desk.test")
            .header("origin", "http://desk.test")
            .header("authorization", "Bearer owner-token")
            .header("content-type", "multipart/form-data; boundary=b")
            .body(Body::from(multipart.as_slice()))
            .unwrap();
        let response = block_on(app(backend.clone()).oneshot(request)).unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["filename"], "foo_bar.txt");
        assert_eq!(backend.uploads.lock().unwrap()[0].bytes, b"abc");
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
                metadata: DownloadMetadata {
                    content_length: 3,
                    last_modified: "Tue, 14 Nov 2023 22:13:20 GMT".into(),
                    etag: "\"fixture-three-byte-file\"".into(),
                },
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
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "3");
        assert_eq!(
            response.headers()[header::LAST_MODIFIED],
            "Tue, 14 Nov 2023 22:13:20 GMT"
        );
        assert_eq!(
            response.headers()[header::ETAG],
            "\"fixture-three-byte-file\""
        );
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "png"
        );
    }

    fn range_fixture() -> Arc<FakeBackend> {
        Arc::new(FakeBackend {
            file: Some(DownloadFile {
                filename: "report.txt".into(),
                mime: "text/plain".into(),
                bytes: b"abcdef".to_vec(),
                metadata: DownloadMetadata {
                    content_length: 6,
                    last_modified: "Tue, 14 Nov 2023 22:13:20 GMT".into(),
                    etag: "\"7a7d5b4bd1e88dca4470ebf19361f0c1\"".into(),
                },
            }),
            ..FakeBackend::default()
        })
    }

    #[test]
    fn download_matches_file_response_base_metadata_headers() {
        let response = block_on(app(range_fixture()).oneshot(request(
            "GET",
            "/api/uploads/a1",
            Body::empty(),
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers()[header::CONTENT_TYPE],
            "text/plain; charset=utf-8"
        );
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "6");
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        assert_eq!(
            response.headers()[header::LAST_MODIFIED],
            "Tue, 14 Nov 2023 22:13:20 GMT"
        );
        assert_eq!(
            response.headers()[header::ETAG],
            "\"7a7d5b4bd1e88dca4470ebf19361f0c1\""
        );
        assert_eq!(
            response.headers()[header::X_CONTENT_TYPE_OPTIONS],
            "nosniff"
        );
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "abcdef"
        );
    }

    #[test]
    fn download_serves_single_range_and_honors_if_range() {
        let mut request = request("GET", "/api/uploads/a1", Body::empty());
        request
            .headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=1-3"));
        request.headers_mut().insert(
            header::IF_RANGE,
            HeaderValue::from_static("\"7a7d5b4bd1e88dca4470ebf19361f0c1\""),
        );
        let response = block_on(app(range_fixture()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 1-3/6");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "3");
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "bcd"
        );

        let mut request = request("GET", "/api/uploads/a1", Body::empty());
        request
            .headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=1-3"));
        request.headers_mut().insert(
            header::IF_RANGE,
            HeaderValue::from_static("Tue, 14 Nov 2023 22:13:20 GMT"),
        );
        let response = block_on(app(range_fixture()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 1-3/6");
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "bcd"
        );

        let mut request = request("GET", "/api/uploads/a1", Body::empty());
        request
            .headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=1-3"));
        request
            .headers_mut()
            .insert(header::IF_RANGE, HeaderValue::from_static("\"stale\""));
        let response = block_on(app(range_fixture()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::CONTENT_RANGE));
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "abcdef"
        );
    }

    #[test]
    fn download_rejects_malformed_and_unsatisfiable_ranges() {
        let mut request = request("GET", "/api/uploads/a1", Body::empty());
        request
            .headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=not-a-range"));
        let response = block_on(app(range_fixture()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "Range header: range must be requested"
        );

        let mut request = request("GET", "/api/uploads/a1", Body::empty());
        request
            .headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=99-"));
        let response = block_on(app(range_fixture()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */6");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert!(block_on(to_bytes(response.into_body(), usize::MAX))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn download_serves_multipart_ranges() {
        let mut request = request("GET", "/api/uploads/a1", Body::empty());
        request
            .headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=0-1,4-5"));
        let response = block_on(app(range_fixture()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        let content_type = response.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .to_owned();
        let boundary = content_type
            .strip_prefix("multipart/byteranges; boundary=")
            .unwrap();
        assert_eq!(response.headers()[header::ACCEPT_RANGES], "bytes");
        let body = block_on(to_bytes(response.into_body(), usize::MAX)).unwrap();
        let body = String::from_utf8(body.to_vec()).unwrap();
        assert!(body.contains(&format!(
            "--{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Range: bytes 0-1/6\r\n\r\nab\r\n"
        )));
        assert!(body.contains(&format!(
            "--{boundary}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Range: bytes 4-5/6\r\n\r\nef\r\n"
        )));
        assert!(body.ends_with(&format!("--{boundary}--")));

        let mut request = request("GET", "/api/uploads/a1", Body::empty());
        request
            .headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=0-2,2-4"));
        let response = block_on(app(range_fixture()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 0-4/6");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "5");
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "abcde"
        );
    }

    #[test]
    fn download_head_matches_get_only_route() {
        let response = block_on(app(range_fixture()).oneshot(request(
            "HEAD",
            "/api/uploads/a1",
            Body::empty(),
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(response.headers()[header::ALLOW], "GET");
    }

    #[test]
    fn download_keeps_non_image_as_attachment_and_missing_is_404() {
        let backend = Arc::new(FakeBackend {
            file: Some(DownloadFile {
                filename: "note.txt".into(),
                mime: "text/plain".into(),
                bytes: b"note".to_vec(),
                metadata: DownloadMetadata {
                    content_length: 4,
                    last_modified: "Tue, 14 Nov 2023 22:13:20 GMT".into(),
                    etag: "\"fixture-four-byte-file\"".into(),
                },
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
