//! Search and attachment routes ported from `web/app.py`.
//!
//! Database and upload-root access remain behind the injected backend. This module
//! owns query/form parsing, authentication, response headers, and pagination.

use super::response_error::WebResponseError;

use std::{
    collections::hash_map::RandomState,
    future::poll_fn,
    hash::{BuildHasher, Hasher},
    pin::Pin,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{Body, HttpBody as _},
    extract::{Extension, Path, Request},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde_json::{json, Value};

use super::auth::{AuthBackend, AuthError, RequestScheme};

const MAX_UPLOAD: usize = 25 * 1024 * 1024;
const MAX_MULTIPART_BOUNDARY_BYTES: usize = 256;
const MAX_MULTIPART_HEADER_COUNT: usize = 8;
const MAX_MULTIPART_HEADER_LINE_BYTES: usize = 4224;
const MAX_MULTIPART_HEADER_BLOCK_BYTES: usize =
    MAX_MULTIPART_HEADER_COUNT * (MAX_MULTIPART_HEADER_LINE_BYTES + 2) + 2;
const MAX_NON_FILE_PART_SIZE: usize = 1024 * 1024;
const MAX_MULTIPART_FILES: usize = 1000;
const MAX_MULTIPART_FIELDS: usize = 1000;
const MULTIPART_FEED_SIZE: usize = 64 * 1024;

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
        return response.into_response();
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
    let mut before_overflow = false;
    let before = match query.get("before") {
        Some(value) => match parse_python_query_integer(value) {
            Ok(before) if before != 0 => before,
            Ok(_) => i64::MAX,
            Err(QueryIntegerError::Invalid) => return query_integer_error("before", value),
            Err(QueryIntegerError::Overflow) => {
                before_overflow = true;
                i64::MAX
            }
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
    if before_overflow {
        return internal_error();
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
        return detail_response(StatusCode::BAD_REQUEST, "Missing boundary in multipart.");
    }
    let Some(boundary) = boundary else {
        return invalid_upload();
    };
    let parsed = match multipart_file(request.into_body(), &boundary).await {
        Ok(Some(ParsedFormField::Upload(file))) => file,
        Ok(Some(ParsedFormField::Scalar(value))) => {
            return invalid_upload_scalar(value);
        }
        Ok(None) => return invalid_upload(),
        Err(MultipartFileError::BadRequest(detail)) => {
            return detail_response(StatusCode::BAD_REQUEST, detail);
        }
        Err(MultipartFileError::BodyFailure) => return too_large(),
    };
    if let Err(response) = authenticate(auth.as_ref(), &headers, scheme).await {
        return response.into_response();
    }
    if parsed.bytes.len() > MAX_UPLOAD {
        return too_large();
    }
    let filename = sanitize_filename(&parsed.filename);
    let upload = UploadPart {
        filename,
        storage_mime: parsed
            .mime
            .clone()
            .unwrap_or_else(|| "application/octet-stream".to_owned()),
        response_mime: parsed.mime,
        bytes: parsed.bytes,
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
        return response.into_response();
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
        return response.into_response();
    }

    let parsed = match parse_byte_ranges(range.unwrap_or_default(), content_length) {
        Ok(ranges) => ranges,
        Err(ByteRangeError::Malformed(message)) => {
            return range_error(StatusCode::BAD_REQUEST, &message, None);
        }
        Err(ByteRangeError::Unsatisfiable) => {
            return range_error(
                StatusCode::RANGE_NOT_SATISFIABLE,
                "",
                Some(format!("bytes */{}", file.metadata.content_length)),
            );
        }
    };
    match parsed.as_slice() {
        [] => {
            *response.status_mut() = StatusCode::OK;
            *response.body_mut() = Body::from(file.bytes);
            response
        }
        [range] => single_range_response(response, file, range),
        ranges => multiple_ranges_response(response, file, ranges, &content_type),
    }
}

fn file_response_content_type(mime: &str) -> String {
    if mime.starts_with("text/") && !mime.to_ascii_lowercase().contains("charset=") {
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
            let Some(suffix) = parse_python_range_integer(end_text) else {
                continue;
            };
            let start = if suffix.negative {
                file_size.saturating_add(suffix.magnitude)
            } else {
                file_size.saturating_sub(suffix.magnitude)
            };
            (start, file_size)
        } else {
            let Some(start) = parse_python_range_integer(start_text) else {
                continue;
            };
            if start.negative {
                return Err(ByteRangeError::Unsatisfiable);
            }
            let end = if end_text.is_empty() {
                file_size
            } else {
                let Some(end_inclusive) = parse_python_range_integer(end_text) else {
                    continue;
                };
                if end_inclusive.negative {
                    if end_inclusive.magnitude == 0 {
                        1
                    } else {
                        0
                    }
                } else if end_inclusive.magnitude < file_size {
                    end_inclusive.magnitude.saturating_add(1)
                } else {
                    file_size
                }
            };
            (start.magnitude, end)
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PythonRangeInteger {
    negative: bool,
    magnitude: usize,
}

fn parse_python_range_integer(value: &str) -> Option<PythonRangeInteger> {
    let value = value.trim();
    let (negative, digits) = if let Some(value) = value.strip_prefix('-') {
        (true, value)
    } else if let Some(value) = value.strip_prefix('+') {
        (false, value)
    } else {
        (false, value)
    };
    if digits.is_empty() {
        return None;
    }
    let bytes = digits.as_bytes();
    if !bytes[0].is_ascii_digit() || !bytes[bytes.len() - 1].is_ascii_digit() {
        return None;
    }
    let mut magnitude = 0usize;
    let mut previous_was_digit = false;
    for &byte in bytes {
        match byte {
            b'0'..=b'9' => {
                magnitude = magnitude
                    .saturating_mul(10)
                    .saturating_add(usize::from(byte - b'0'));
                previous_was_digit = true;
            }
            b'_' if previous_was_digit => previous_was_digit = false,
            _ => return None,
        }
    }
    if !previous_was_digit {
        return None;
    }
    Some(PythonRangeInteger {
        negative: negative && magnitude != 0,
        magnitude,
    })
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MultipartFileError {
    BadRequest(&'static str),
    BodyFailure,
}

#[derive(Debug)]
struct ParsedUpload {
    filename: String,
    mime: Option<String>,
    bytes: Vec<u8>,
}

enum ParsedFormField {
    Upload(ParsedUpload),
    Scalar(String),
}

struct MultipartPart {
    selected: bool,
    is_file: bool,
    size: usize,
    upload: Option<ParsedUpload>,
    scalar: Vec<u8>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum MultipartEnd {
    Next,
    Closing,
}

fn find_multipart_end(buffer: &[u8], marker: &[u8]) -> Option<(usize, MultipartEnd)> {
    buffer
        .windows(marker.len())
        .enumerate()
        .find_map(|(at, window)| {
            if window != marker {
                return None;
            }
            let suffix = buffer.get(at + marker.len()..)?;
            if suffix.starts_with(b"--") {
                Some((at, MultipartEnd::Closing))
            } else if suffix.starts_with(b"\r\n") {
                Some((at, MultipartEnd::Next))
            } else {
                None
            }
        })
}

fn feed_part_data(
    part: &mut Option<MultipartPart>,
    bytes: &[u8],
    part_too_large: &'static str,
) -> Result<(), MultipartFileError> {
    let Some(part) = part.as_mut() else {
        return Ok(());
    };
    part.size = part.size.saturating_add(bytes.len());
    if !part.is_file && part.size > MAX_NON_FILE_PART_SIZE {
        return Err(MultipartFileError::BadRequest(part_too_large));
    }
    if part.selected {
        if let Some(file) = part.upload.as_mut() {
            let remaining = MAX_UPLOAD
                .saturating_add(1)
                .saturating_sub(file.bytes.len());
            file.bytes
                .extend_from_slice(&bytes[..bytes.len().min(remaining)]);
        } else if !part.is_file {
            part.scalar.extend_from_slice(bytes);
        }
    }
    Ok(())
}

fn finish_multipart_part(part: Option<MultipartPart>, selected: &mut Option<ParsedFormField>) {
    let Some(part) = part.filter(|part| part.selected) else {
        return;
    };
    *selected = Some(match part.upload {
        Some(upload) => ParsedFormField::Upload(upload),
        None => ParsedFormField::Scalar(decode_form_field(&part.scalar)),
    });
}

fn latin1_text(bytes: &[u8]) -> String {
    bytes.iter().copied().map(char::from).collect()
}

fn decode_latin1_parameter(value: String) -> String {
    let bytes = value
        .chars()
        .map(|character| character as u32 as u8)
        .collect::<Vec<_>>();
    String::from_utf8(bytes.clone()).unwrap_or_else(|_| latin1_text(&bytes))
}

fn decode_form_field(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap_or_else(|_| latin1_text(bytes))
}

async fn multipart_file(
    mut body: Body,
    boundary: &str,
) -> Result<Option<ParsedFormField>, MultipartFileError> {
    const MISSING_PART_NAME: &str =
        "The Content-Disposition header field \"name\" must be provided.";
    const PART_TOO_LARGE: &str = "Part exceeded maximum size of 1024KB.";
    const INVALID_MULTIPART: &str = "Invalid multipart data.";
    if boundary.len() > MAX_MULTIPART_BOUNDARY_BYTES {
        return Err(MultipartFileError::BadRequest(INVALID_MULTIPART));
    }
    let delimiter = format!("--{boundary}").into_bytes();
    let mut marker = b"\r\n".to_vec();
    marker.extend_from_slice(&delimiter);
    let mut buffer = Vec::new();
    let mut selected: Option<ParsedFormField> = None;
    let mut files = 0usize;
    let mut fields = 0usize;
    let mut part: Option<MultipartPart> = None;
    let mut started = false;
    loop {
        let frame = poll_fn(|cx| Pin::new(&mut body).poll_frame(cx))
            .await
            .transpose()
            .map_err(|_| MultipartFileError::BodyFailure)?;
        let Some(frame) = frame else {
            return Ok(selected);
        };
        let Ok(data) = frame.into_data() else {
            continue;
        };
        for chunk in data.chunks(MULTIPART_FEED_SIZE) {
            buffer.extend_from_slice(chunk);
            loop {
                if !started {
                    if buffer.len() < delimiter.len() + 2 {
                        break;
                    }
                    if !buffer.starts_with(&delimiter) {
                        return Ok(None);
                    }
                    let suffix = &buffer[delimiter.len()..];
                    if suffix.starts_with(b"--") {
                        return Ok(selected);
                    }
                    if !suffix.starts_with(b"\r\n") {
                        return Ok(None);
                    }
                    buffer.drain(..delimiter.len() + 2);
                    started = true;
                    continue;
                }

                if part.is_none() {
                    let Some(header_end) = buffer.windows(4).position(|w| w == b"\r\n\r\n") else {
                        if buffer.len() > MAX_MULTIPART_HEADER_BLOCK_BYTES {
                            return Err(MultipartFileError::BadRequest(INVALID_MULTIPART));
                        }
                        break;
                    };
                    if header_end > MAX_MULTIPART_HEADER_BLOCK_BYTES {
                        return Err(MultipartFileError::BadRequest(INVALID_MULTIPART));
                    }
                    let raw_headers = &buffer[..header_end];
                    let header_lines = raw_headers
                        .split(|byte| *byte == b'\n')
                        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
                        .collect::<Vec<_>>();
                    if header_lines.len() > MAX_MULTIPART_HEADER_COUNT
                        || header_lines
                            .iter()
                            .any(|line| line.len() > MAX_MULTIPART_HEADER_LINE_BYTES)
                    {
                        return Err(MultipartFileError::BadRequest(INVALID_MULTIPART));
                    }
                    let headers = latin1_text(raw_headers);
                    let mut field_name = None;
                    let mut filename = None;
                    let mut mime = None;
                    for line in headers.split("\r\n") {
                        let Some((name, value)) = line.split_once(':') else {
                            continue;
                        };
                        if name.trim().eq_ignore_ascii_case("content-disposition") {
                            let Some((field, part_filename)) = disposition_parameters(value) else {
                                return Err(MultipartFileError::BadRequest(MISSING_PART_NAME));
                            };
                            field_name = Some(field);
                            filename = part_filename;
                        } else if name.trim().eq_ignore_ascii_case("content-type") {
                            mime = Some(value.trim().to_owned());
                        }
                    }
                    let Some(field_name) = field_name else {
                        return Err(MultipartFileError::BadRequest(MISSING_PART_NAME));
                    };
                    let field_name = decode_latin1_parameter(field_name);
                    let filename = filename.map(decode_latin1_parameter);
                    let is_file = filename.is_some();
                    let selected_part = field_name == "file";
                    if is_file {
                        files += 1;
                        if files > MAX_MULTIPART_FILES {
                            return Err(MultipartFileError::BadRequest(
                                "Too many files. Maximum number of files is 1000.",
                            ));
                        }
                    } else {
                        fields += 1;
                        if fields > MAX_MULTIPART_FIELDS {
                            return Err(MultipartFileError::BadRequest(
                                "Too many fields. Maximum number of fields is 1000.",
                            ));
                        }
                    }
                    let upload = if selected_part {
                        filename.map(|filename| ParsedUpload {
                            filename,
                            mime,
                            bytes: Vec::new(),
                        })
                    } else {
                        None
                    };
                    buffer.drain(..header_end + 4);
                    part = Some(MultipartPart {
                        selected: selected_part,
                        is_file,
                        size: 0,
                        upload,
                        scalar: Vec::new(),
                    });
                    continue;
                }

                let Some((at, end_kind)) = find_multipart_end(&buffer, &marker) else {
                    let keep = marker.len() + 2;
                    if buffer.len() <= keep {
                        break;
                    }
                    let emit = buffer.len() - keep;
                    feed_part_data(&mut part, &buffer[..emit], PART_TOO_LARGE)?;
                    buffer.drain(..emit);
                    break;
                };
                feed_part_data(&mut part, &buffer[..at], PART_TOO_LARGE)?;
                let consumed = at + marker.len();
                buffer.drain(..consumed);
                finish_multipart_part(part.take(), &mut selected);
                if end_kind == MultipartEnd::Closing {
                    return Ok(selected);
                }
                buffer.drain(..2);
            }
        }
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
    let encoded = filename
        .bytes()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || b"_.-~/".contains(&byte) {
                (byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    if encoded == filename {
        format!("{kind}; filename=\"{filename}\"")
    } else {
        format!("{kind}; filename*=utf-8''{encoded}")
    }
}

async fn authenticate(
    auth: &dyn AuthBackend,
    headers: &HeaderMap,
    scheme: RequestScheme,
) -> Result<(), WebResponseError> {
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
            return Err(
                detail_response(StatusCode::FORBIDDEN, "Cross-origin request refused").into(),
            );
        }
    }
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let cookie = session_cookie(headers);
    auth.identity(bearer, cookie.as_deref())
        .await
        .map(|_| ())
        .map_err(auth_error_response)
        .map_err(WebResponseError::from)
}

fn session_cookie(headers: &HeaderMap) -> Option<String> {
    let mut session = None;
    for header_value in headers.get_all(header::COOKIE).iter() {
        let cookie_header = latin1_text(header_value.as_bytes());
        for chunk in cookie_header.split(';') {
            let (name, value) = chunk.split_once('=').unwrap_or(("", chunk));
            let name = name.trim();
            let value = value.trim();
            if (!name.is_empty() || !value.is_empty()) && name == "vp_session" {
                session = Some(unquote_cookie_value(value));
            }
        }
    }
    session
}

fn unquote_cookie_value(value: &str) -> String {
    if value.len() < 2 || !value.starts_with('"') || !value.ends_with('"') {
        return value.to_owned();
    }
    let characters = value[1..value.len() - 1].chars().collect::<Vec<_>>();
    let mut output = String::with_capacity(value.len() - 2);
    let mut at = 0;
    while at < characters.len() {
        if characters[at] == '\\' && at + 1 < characters.len() {
            if at + 3 < characters.len()
                && ('0'..='3').contains(&characters[at + 1])
                && ('0'..='7').contains(&characters[at + 2])
                && ('0'..='7').contains(&characters[at + 3])
            {
                let value = (characters[at + 1] as u32 - '0' as u32) * 64
                    + (characters[at + 2] as u32 - '0' as u32) * 8
                    + (characters[at + 3] as u32 - '0' as u32);
                if let Some(character) = char::from_u32(value) {
                    output.push(character);
                }
                at += 4;
            } else {
                output.push(characters[at + 1]);
                at += 2;
            }
        } else {
            output.push(characters[at]);
            at += 1;
        }
    }
    output
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
                    if let (Some(high), Some(low)) = (hex(bytes[at + 1]), hex(bytes[at + 2])) {
                        out.push(high * 16 + low);
                        at += 3;
                    } else {
                        out.push(b'%');
                        at += 1;
                    }
                }
                b'%' => {
                    out.push(b'%');
                    at += 1;
                }
                byte => {
                    out.push(byte);
                    at += 1;
                }
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum QueryIntegerError {
    Invalid,
    Overflow,
}

fn parse_python_query_integer(value: &str) -> Result<i64, QueryIntegerError> {
    fn integer_digits(value: &str) -> Option<String> {
        let bytes = value.as_bytes();
        if bytes.is_empty() || !bytes.first()?.is_ascii_digit() || !bytes.last()?.is_ascii_digit() {
            return None;
        }
        let mut digits = String::with_capacity(value.len());
        let mut previous_digit = false;
        for &byte in bytes {
            match byte {
                b'0'..=b'9' => {
                    digits.push(char::from(byte));
                    previous_digit = true;
                }
                b'_' if previous_digit => previous_digit = false,
                _ => return None,
            }
        }
        previous_digit.then_some(digits)
    }

    let value = value.trim();
    let (sign, unsigned) = match value.as_bytes().first().copied() {
        Some(b'+') => ("", &value[1..]),
        Some(b'-') => ("-", &value[1..]),
        _ => ("", value),
    };
    let integer = match unsigned.split_once('.') {
        Some((integer, fraction)) => {
            if fraction.is_empty()
                || !fraction.bytes().all(|byte| byte.is_ascii_digit())
                || !fraction.bytes().all(|byte| byte == b'0')
            {
                return Err(QueryIntegerError::Invalid);
            }
            integer
        }
        None => unsigned,
    };
    let digits = integer_digits(integer).ok_or(QueryIntegerError::Invalid)?;
    format!("{sign}{digits}")
        .parse::<i64>()
        .map_err(|error| match error.kind() {
            std::num::IntErrorKind::PosOverflow | std::num::IntErrorKind::NegOverflow => {
                QueryIntegerError::Overflow
            }
            _ => QueryIntegerError::Invalid,
        })
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

fn invalid_upload_scalar(value: String) -> Response {
    validation_error(json!({
        "type": "value_error",
        "loc": ["body", "file"],
        "msg": "Value error, Expected UploadFile, received: <class 'str'>",
        "input": value,
        "ctx": {"error": {}}
    }))
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

    fn multipart_request(body: Body) -> Request<Body> {
        multipart_request_with_boundary(body, "b")
    }

    fn multipart_request_with_boundary(body: Body, boundary: &str) -> Request<Body> {
        let mut request = request("POST", "/api/uploads", body);
        request.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_str(&format!("multipart/form-data; boundary={boundary}")).unwrap(),
        );
        request
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
    fn search_uses_python_query_decoding_for_malformed_percent_and_utf8() {
        for (query, expected) in [("a%", "a%"), ("a%2", "a%2"), ("a%FF", "a\u{fffd}")] {
            let backend = Arc::new(FakeBackend::default());
            let response = block_on(app(backend.clone()).oneshot(request(
                "GET",
                &format!("/api/search?q={query}"),
                Body::empty(),
            )))
            .unwrap();
            assert_eq!(json_response(response).0, StatusCode::OK, "{query:?}");
            assert_eq!(
                *backend.search.lock().unwrap(),
                vec![(format!("%{expected}%"), None, i64::MAX, 51)],
                "{query:?}"
            );
        }
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
    fn search_matches_python_integer_query_coercion_and_overflow_order() {
        for (before, expected) in [("+1", 1), ("1_0", 10), ("1.0", 1), ("%201%20", 1)] {
            let backend = Arc::new(FakeBackend::default());
            let response = block_on(app(backend.clone()).oneshot(request(
                "GET",
                &format!("/api/search?q=hi&before={before}"),
                Body::empty(),
            )))
            .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "before={before}");
            assert_eq!(backend.search.lock().unwrap()[0].2, expected);
        }

        let backend = Arc::new(FakeBackend::default());
        let response = block_on(app(backend.clone()).oneshot(request(
            "GET",
            "/api/search?q=hi&before=1.5",
            Body::empty(),
        )))
        .unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(body["detail"][0]["input"], "1.5");
        assert!(backend.search.lock().unwrap().is_empty());

        let huge = "999999999999999999999999999999999999";
        let backend = Arc::new(FakeBackend::default());
        let response = block_on(app(backend.clone()).oneshot(request(
            "GET",
            &format!("/api/search?q=hi&before={huge}"),
            Body::empty(),
        )))
        .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert!(backend.search.lock().unwrap().is_empty());

        let short = block_on(app(Arc::new(FakeBackend::default())).oneshot(request(
            "GET",
            &format!("/api/search?q=x&before={huge}"),
            Body::empty(),
        )))
        .unwrap();
        assert_eq!(
            json_response(short).1,
            json!({"items": [], "has_more": false})
        );
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
    fn authenticate_uses_starlette_last_unquoted_cookie_across_headers() {
        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=first; vp_session=second"),
        );
        assert_eq!(session_cookie(&headers).as_deref(), Some("second"));

        let mut headers = HeaderMap::new();
        headers.append(header::COOKIE, HeaderValue::from_static("vp_session=first"));
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=\"owner-token\""),
        );
        assert_eq!(session_cookie(&headers).as_deref(), Some("owner-token"));

        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=\"tok\\\"en\""),
        );
        assert_eq!(session_cookie(&headers).as_deref(), Some("tok\"en"));

        let mut headers = HeaderMap::new();
        headers.append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=\"\\101\""),
        );
        assert_eq!(session_cookie(&headers).as_deref(), Some("A"));

        let backend = Arc::new(FakeBackend::default());
        let mut request = request("GET", "/api/search?q=hi", Body::empty());
        request.headers_mut().remove(header::AUTHORIZATION);
        request
            .headers_mut()
            .append(header::COOKIE, HeaderValue::from_static("vp_session=bad"));
        request.headers_mut().append(
            header::COOKIE,
            HeaderValue::from_static("vp_session=\"owner-token\""),
        );
        let response = block_on(app(backend.clone()).oneshot(request)).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(backend.search.lock().unwrap().len(), 1);
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
    fn upload_parses_all_parts_and_uses_last_file_value() {
        let backend = Arc::new(FakeBackend::default());
        let multipart = b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"first.txt\"\r\n\r\none\r\n--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"last.txt\"\r\n\r\nlast\r\n--b--\r\n";
        let response = block_on(
            app(backend.clone()).oneshot(multipart_request(Body::from(multipart.as_slice()))),
        )
        .unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["filename"], "last.txt");
        assert_eq!(body["size"], 4);
        assert_eq!(backend.uploads.lock().unwrap().len(), 1);
        assert_eq!(backend.uploads.lock().unwrap()[0].bytes, b"last");

        let backend = Arc::new(FakeBackend::default());
        let malformed = b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"first.txt\"\r\n\r\none\r\n--b\r\nContent-Disposition: garbage\r\n\r\nignored\r\n--b--\r\n";
        let response = block_on(
            app(backend.clone()).oneshot(multipart_request(Body::from(malformed.as_slice()))),
        )
        .unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({"detail":"The Content-Disposition header field \"name\" must be provided."})
        );
        assert!(backend.uploads.lock().unwrap().is_empty());
    }

    #[test]
    fn upload_uses_python_nonfile_part_size_limit() {
        let backend = Arc::new(FakeBackend::default());
        let accepted_body = format!(
            "--b\r\nContent-Disposition: form-data; name=\"description\"\r\n\r\n{}\r\n--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nfile\r\n--b--\r\n",
            "x".repeat(80 * 1024)
        );
        let response =
            block_on(app(backend.clone()).oneshot(multipart_request(Body::from(accepted_body))))
                .unwrap();
        assert_eq!(json_response(response).0, StatusCode::OK);
        assert_eq!(backend.uploads.lock().unwrap().len(), 1);

        let backend = Arc::new(FakeBackend::default());
        let too_large_body = format!(
            "--b\r\nContent-Disposition: form-data; name=\"description\"\r\n\r\n{}\r\n--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nfile\r\n--b--\r\n",
            "x".repeat(MAX_NON_FILE_PART_SIZE + 1)
        );
        let response =
            block_on(app(backend.clone()).oneshot(multipart_request(Body::from(too_large_body))))
                .unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            body,
            json!({"detail":"Part exceeded maximum size of 1024KB."})
        );
        assert!(backend.uploads.lock().unwrap().is_empty());
    }

    #[test]
    fn upload_accepts_python_multipart_aggregate_overhead_without_aggregate_cap() {
        let backend = Arc::new(FakeBackend::default());
        let mut body = Vec::with_capacity(28 * 1024 * 1024);
        for index in 0..27 {
            body.extend_from_slice(
                format!("--b\r\nContent-Disposition: form-data; name=\"field{index}\"\r\n\r\n")
                    .as_bytes(),
            );
            body.resize(body.len() + MAX_NON_FILE_PART_SIZE, b'x');
            body.extend_from_slice(b"\r\n");
        }
        body.extend_from_slice(
            b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"tiny.txt\"\r\n\r\nx\r\n--b--\r\n",
        );

        let response =
            block_on(app(backend.clone()).oneshot(multipart_request(Body::from(body)))).unwrap();
        let (status, response_body) = json_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response_body["size"], 1);
        assert_eq!(backend.uploads.lock().unwrap()[0].bytes, b"x");
    }

    #[test]
    fn upload_enforces_python_multipart_field_count_limit() {
        let backend = Arc::new(FakeBackend::default());
        let mut body = Vec::new();
        for index in 0..1001 {
            body.extend_from_slice(
                format!(
                    "--b\r\nContent-Disposition: form-data; name=\"field{index}\"\r\n\r\nx\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(b"--b--\r\n");
        let response =
            block_on(app(backend.clone()).oneshot(multipart_request(Body::from(body)))).unwrap();
        let (status, response_body) = json_response(response);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            response_body["detail"],
            "Too many fields. Maximum number of fields is 1000."
        );
        assert!(backend.uploads.lock().unwrap().is_empty());
    }

    #[test]
    fn upload_enforces_python_multipart_file_count_limit() {
        let backend = Arc::new(FakeBackend::default());
        let mut body = Vec::new();
        for index in 0..1001 {
            body.extend_from_slice(
                format!(
                    "--b\r\nContent-Disposition: form-data; name=\"other{index}\"; filename=\"f{index}.txt\"\r\n\r\nx\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(b"--b--\r\n");
        let response =
            block_on(app(backend.clone()).oneshot(multipart_request(Body::from(body)))).unwrap();
        let (status, response_body) = json_response(response);
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(
            response_body["detail"],
            "Too many files. Maximum number of files is 1000."
        );
        assert!(backend.uploads.lock().unwrap().is_empty());
    }

    #[test]
    fn upload_matches_python_boundary_and_header_limits() {
        let max_boundary = "b".repeat(MAX_MULTIPART_BOUNDARY_BYTES);
        let multipart = format!(
            "--{max_boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nx\r\n--{max_boundary}--\r\n"
        );
        let backend = Arc::new(FakeBackend::default());
        let response = block_on(
            app(backend.clone()).oneshot(multipart_request_with_boundary(
                Body::from(multipart),
                &max_boundary,
            )),
        )
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(backend.uploads.lock().unwrap()[0].bytes, b"x");

        let oversized_boundary = "b".repeat(MAX_MULTIPART_BOUNDARY_BYTES + 1);
        let body = format!("--{oversized_boundary}--\r\n");
        let response = block_on(app(Arc::new(FakeBackend::default())).oneshot(
            multipart_request_with_boundary(Body::from(body), &oversized_boundary),
        ))
        .unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::BAD_REQUEST,
                json!({"detail":"Invalid multipart data."})
            )
        );

        let mut eight_headers =
            b"Content-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n".to_vec();
        for index in 0..7 {
            eight_headers.extend_from_slice(format!("X-{index}: v\r\n").as_bytes());
        }
        let mut body = b"--b\r\n".to_vec();
        body.extend_from_slice(&eight_headers);
        body.extend_from_slice(b"\r\nx\r\n--b--\r\n");
        let backend = Arc::new(FakeBackend::default());
        let response =
            block_on(app(backend.clone()).oneshot(multipart_request(Body::from(body)))).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(backend.uploads.lock().unwrap()[0].bytes, b"x");

        let mut nine_headers =
            b"Content-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n".to_vec();
        for index in 0..8 {
            nine_headers.extend_from_slice(format!("X-{index}: v\r\n").as_bytes());
        }
        let mut body = b"--b\r\n".to_vec();
        body.extend_from_slice(&nine_headers);
        body.extend_from_slice(b"\r\n\r\nx\r\n--b--\r\n");
        let response = block_on(
            app(Arc::new(FakeBackend::default())).oneshot(multipart_request(Body::from(body))),
        )
        .unwrap();
        assert_eq!(
            json_response(response),
            (
                StatusCode::BAD_REQUEST,
                json!({"detail":"Invalid multipart data."})
            )
        );

        for (line_size, expected) in [
            (MAX_MULTIPART_HEADER_LINE_BYTES, StatusCode::OK),
            (MAX_MULTIPART_HEADER_LINE_BYTES + 1, StatusCode::BAD_REQUEST),
        ] {
            let mut headers =
                b"Content-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n".to_vec();
            headers.extend_from_slice(b"X: ");
            headers.resize(headers.len() + line_size - 3, b'x');
            let mut body = b"--b\r\n".to_vec();
            body.extend_from_slice(&headers);
            body.extend_from_slice(b"\r\n\r\nx\r\n--b--\r\n");
            let backend = Arc::new(FakeBackend::default());
            let response =
                block_on(app(backend.clone()).oneshot(multipart_request(Body::from(body))))
                    .unwrap();
            assert_eq!(response.status(), expected, "header size {line_size}");
            if expected == StatusCode::OK {
                assert_eq!(backend.uploads.lock().unwrap()[0].bytes, b"x");
            } else {
                assert_eq!(
                    json_response(response).1,
                    json!({"detail":"Invalid multipart data."})
                );
            }
        }
    }

    #[test]
    fn upload_decodes_utf8_and_latin1_filenames_like_starlette() {
        for (encoded, expected) in [
            (b"caf\xe9.txt".as_slice(), "café.txt"),
            (b"caf\xc3\xa9.txt".as_slice(), "café.txt"),
        ] {
            let mut body =
                b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"".to_vec();
            body.extend_from_slice(encoded);
            body.extend_from_slice(b"\"\r\n\r\nx\r\n--b--\r\n");
            let backend = Arc::new(FakeBackend::default());
            let response =
                block_on(app(backend.clone()).oneshot(multipart_request(Body::from(body))))
                    .unwrap();
            let (status, body) = json_response(response);
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["filename"], expected);
            assert_eq!(backend.uploads.lock().unwrap()[0].filename, expected);
        }
    }

    #[test]
    fn upload_keeps_last_completed_file_when_next_part_is_incomplete() {
        let backend = Arc::new(FakeBackend::default());
        let body = b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\ncomplete\r\n--b\r\nContent-Disposition: form-data; name=\"next\"\r\n\r\nincomplete";
        let response =
            block_on(app(backend.clone()).oneshot(multipart_request(Body::from(body.as_slice()))))
                .unwrap();
        let (status, response_body) = json_response(response);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(response_body["filename"], "a.txt");
        assert_eq!(backend.uploads.lock().unwrap()[0].bytes, b"complete");
    }

    #[test]
    fn upload_reports_python_error_for_last_duplicate_scalar_value() {
        let backend = Arc::new(FakeBackend::default());
        let body = b"--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.txt\"\r\n\r\nfirst\r\n--b\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\ntext\r\n--b--\r\n";
        let response =
            block_on(app(backend.clone()).oneshot(multipart_request(Body::from(body.as_slice()))))
                .unwrap();
        let (status, body) = json_response(response);
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            body,
            json!({"detail":[{
                "type":"value_error",
                "loc":["body","file"],
                "msg":"Value error, Expected UploadFile, received: <class 'str'>",
                "input":"text",
                "ctx":{"error":{}}
            }]})
        );
        assert!(backend.uploads.lock().unwrap().is_empty());
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
        assert_eq!(
            json_response(response),
            (
                StatusCode::BAD_REQUEST,
                json!({"detail":"Missing boundary in multipart."})
            )
        );
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

    #[test]
    fn content_disposition_matches_starlette_quoting() {
        assert_eq!(
            content_disposition("my sheep_.txt", false),
            "attachment; filename*=utf-8''my%20sheep_.txt"
        );
        assert_eq!(
            content_disposition("a\"b.txt", false),
            "attachment; filename*=utf-8''a%22b.txt"
        );
        assert_eq!(
            content_disposition("café.txt", false),
            "attachment; filename*=utf-8''caf%C3%A9.txt"
        );
        assert_eq!(
            content_disposition("a/b.txt", false),
            "attachment; filename=\"a/b.txt\""
        );
        assert_eq!(
            content_disposition("sheep.png", true),
            "inline; filename=\"sheep.png\""
        );
    }

    #[test]
    fn file_response_content_type_checks_prefix_and_charset_case_sensitively() {
        assert_eq!(
            file_response_content_type("text/plain"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(file_response_content_type("TEXT/PLAIN"), "TEXT/PLAIN");
        assert_eq!(
            file_response_content_type("text/PLAIN"),
            "text/PLAIN; charset=utf-8"
        );
        assert_eq!(
            file_response_content_type("text/plain; CHARSET=x"),
            "text/plain; CHARSET=x"
        );
        assert_eq!(
            file_response_content_type("text/plain; Charset=x"),
            "text/plain; Charset=x"
        );
        assert_eq!(
            file_response_content_type("text/plain; charset=x"),
            "text/plain; charset=x"
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
        let mut req = request("GET", "/api/uploads/a1", Body::empty());
        req.headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=1-3"));
        req.headers_mut().insert(
            header::IF_RANGE,
            HeaderValue::from_static("\"7a7d5b4bd1e88dca4470ebf19361f0c1\""),
        );
        let response = block_on(app(range_fixture()).oneshot(req)).unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 1-3/6");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "3");
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "bcd"
        );

        let mut req = request("GET", "/api/uploads/a1", Body::empty());
        req.headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=1-3"));
        req.headers_mut().insert(
            header::IF_RANGE,
            HeaderValue::from_static("Tue, 14 Nov 2023 22:13:20 GMT"),
        );
        let response = block_on(app(range_fixture()).oneshot(req)).unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 1-3/6");
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "bcd"
        );

        let mut req = request("GET", "/api/uploads/a1", Body::empty());
        req.headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=1-3"));
        req.headers_mut()
            .insert(header::IF_RANGE, HeaderValue::from_static("\"stale\""));
        let response = block_on(app(range_fixture()).oneshot(req)).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(!response.headers().contains_key(header::CONTENT_RANGE));
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "abcdef"
        );
    }

    #[test]
    fn download_matches_python_range_integer_lexemes_and_range_limit() {
        let mut req = request("GET", "/api/uploads/a1", Body::empty());
        req.headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=+1-+2"));
        let response = block_on(app(range_fixture()).oneshot(req)).unwrap();
        assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 1-2/6");
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "bc"
        );
        assert_eq!(
            parse_byte_ranges("bytes=1_0-1_1", 20).unwrap(),
            vec![ByteRange { start: 10, end: 12 }]
        );

        let ranges = (0..101).map(|_| "0-0").collect::<Vec<_>>().join(",");
        let mut req = request("GET", "/api/uploads/a1", Body::empty());
        req.headers_mut().insert(
            header::RANGE,
            HeaderValue::from_str(&format!("bytes={ranges}")).unwrap(),
        );
        let response = block_on(app(range_fixture()).oneshot(req)).unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "abcdef"
        );
    }

    #[test]
    fn download_rejects_malformed_and_unsatisfiable_ranges() {
        let mut req = request("GET", "/api/uploads/a1", Body::empty());
        req.headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=not-a-range"));
        let response = block_on(app(range_fixture()).oneshot(req)).unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            block_on(to_bytes(response.into_body(), usize::MAX)).unwrap(),
            "Range header: range must be requested"
        );

        let mut req = request("GET", "/api/uploads/a1", Body::empty());
        req.headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=99-"));
        let response = block_on(app(range_fixture()).oneshot(req)).unwrap();
        assert_eq!(response.status(), StatusCode::RANGE_NOT_SATISFIABLE);
        assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */6");
        assert_eq!(response.headers()[header::CONTENT_LENGTH], "0");
        assert!(block_on(to_bytes(response.into_body(), usize::MAX))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn download_serves_multipart_ranges() {
        let mut req = request("GET", "/api/uploads/a1", Body::empty());
        req.headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=0-1,4-5"));
        let response = block_on(app(range_fixture()).oneshot(req)).unwrap();
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

        let mut req = request("GET", "/api/uploads/a1", Body::empty());
        req.headers_mut()
            .insert(header::RANGE, HeaderValue::from_static("bytes=0-2,2-4"));
        let response = block_on(app(range_fixture()).oneshot(req)).unwrap();
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
