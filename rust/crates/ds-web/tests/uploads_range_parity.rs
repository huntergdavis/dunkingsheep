//! t1702 route-parity leaf: GET and HEAD /api/uploads/{id} against the Python desk.
//!
//! The oracle is `fixtures/uploads_range_oracle.json`, captured from the Python desk's
//! `download` route on the pinned FastAPI/Starlette (see `fixtures/generate_uploads_range_oracle.py`).
//! This test drives the Rust `search_attachments` router over the production
//! `MessagesBackend` (a temporary on-disk uploads folder), with the stored file's mtime
//! pinned to the fixture's, and compares status, the kept headers and the body of every
//! recorded case. Multipart boundaries are random on both sides, so they are normalised.
//!
//! NOT covered here: the production daemon, the desk's auth, and the live desk on
//! 127.0.0.1:8787. Those are separate gates.

// messages_backend.rs is included by path and names `crate::roster` / `crate::routes`, which
// messages_backend.rs (the test that includes it too) re-exports at the crate root.
pub use ds_web::{roster, routes};

#[allow(dead_code)]
#[path = "../src/messages_backend.rs"]
mod messages_backend;

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, UNIX_EPOCH},
};

use axum::{
    body::{to_bytes, Body},
    http::{header, Method, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::STANDARD, Engine as _};
use ds_web::routes::{
    auth::{AuthBackend, AuthError, AuthFuture, Identity, RequestScheme},
    search_attachments::{self, SearchAttachmentsBackend, UploadPart},
};
use messages_backend::{MessagesBackend, MessagesBackendConfig, UnixDaemonConnector};
use serde_json::Value;
use tower::ServiceExt;

const OWNER: &str = "owner-token";
const ORACLE: &str = include_str!("fixtures/uploads_range_oracle.json");
/// The headers the oracle records. Anything else the server sends is not compared.
const KEPT: [&str; 10] = [
    "content-type",
    "content-length",
    "content-range",
    "accept-ranges",
    "etag",
    "last-modified",
    "content-disposition",
    "x-content-type-options",
    "allow",
    "cache-control",
];

/// Bearer tokens map to identities; anything else is signed out. Only the owner token is used.
struct OwnerAuth;

impl AuthBackend for OwnerAuth {
    fn login<'a>(
        &'a self,
        _address: &'a str,
        _password: &'a str,
    ) -> AuthFuture<'a, Result<String, AuthError>> {
        Box::pin(async { Err(AuthError::IncorrectPassword) })
    }

    fn identity<'a>(
        &'a self,
        bearer: Option<&'a str>,
        _cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<Identity, AuthError>> {
        Box::pin(async move {
            match bearer {
                Some(OWNER) => Ok(Identity::Owner),
                _ => Err(AuthError::SignInRequired),
            }
        })
    }

    fn logout<'a>(&'a self, _cookie: Option<&'a str>) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async { Ok(()) })
    }

    fn change_password<'a>(
        &'a self,
        _current: &'a str,
        _new: &'a str,
        _keep_cookie: Option<&'a str>,
    ) -> AuthFuture<'a, Result<(), AuthError>> {
        Box::pin(async { Err(AuthError::IncorrectPassword) })
    }
}

/// One response, normalised the way the fixture records it.
struct Normalised {
    status: u16,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
    multipart: bool,
}

fn normalise(status: StatusCode, headers: &axum::http::HeaderMap, body: Vec<u8>) -> Normalised {
    let mut kept = BTreeMap::new();
    for name in KEPT {
        if let Some(value) = headers.get(name) {
            kept.insert(
                name.to_owned(),
                value.to_str().expect("ascii header").to_owned(),
            );
        }
    }
    let mut body = body;
    let mut multipart = false;
    if let Some(content_type) = kept.get("content-type").cloned() {
        if content_type.starts_with("multipart/byteranges") {
            multipart = true;
            let boundary = content_type
                .split("boundary=")
                .nth(1)
                .expect("multipart boundary")
                .to_owned();
            kept.insert(
                "content-type".to_owned(),
                content_type.replace(&boundary, "BOUNDARY"),
            );
            body = replace_bytes(&body, boundary.as_bytes(), b"BOUNDARY");
        }
    }
    Normalised {
        status: status.as_u16(),
        headers: kept,
        body,
        multipart,
    }
}

fn replace_bytes(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut index = 0;
    while index < haystack.len() {
        if haystack[index..].starts_with(needle) {
            out.extend_from_slice(with);
            index += needle.len();
        } else {
            out.push(haystack[index]);
            index += 1;
        }
    }
    out
}

#[tokio::test]
async fn upload_download_matches_the_python_desk_on_every_recorded_case() {
    let oracle: Value = serde_json::from_str(ORACLE).expect("oracle fixture is JSON");
    let file = &oracle["file"];
    let content = STANDARD
        .decode(file["content_base64"].as_str().expect("content"))
        .expect("content is base64");
    let mtime = file["mtime"].as_u64().expect("mtime");
    let filename = file["filename"].as_str().expect("filename").to_owned();
    let mime = file["mime"].as_str().expect("mime").to_owned();

    let directory = tempfile::tempdir().expect("temporary directory");
    // The download path never calls the daemon, so the socket is not bound.
    let connector = Arc::new(UnixDaemonConnector::new(
        directory.path().join("unused.sock"),
        Some("service-token".to_owned()),
        Duration::from_secs(5),
        "uploads range parity",
    ));
    let backend = MessagesBackend::open(MessagesBackendConfig {
        data_dir: directory.path().join("desk"),
        channels: Vec::new(),
        daemon: connector,
    })
    .expect("open the production messages backend");

    let saved = backend
        .save_upload(UploadPart {
            filename: filename.clone(),
            response_mime: Some(mime.clone()),
            storage_mime: mime.clone(),
            bytes: content.clone(),
        })
        .expect("save the upload");
    let id = saved["id"].as_str().expect("attachment id").to_owned();

    // Pin the stored file's mtime to the fixture's, so Last-Modified and the ETag (both
    // from the same stat) are the ones the Python desk served.
    let uploads: PathBuf = directory.path().join("desk").join("uploads");
    let stored_path = std::fs::read_dir(&uploads)
        .expect("uploads folder")
        .map(|entry| entry.expect("entry").path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&id))
        })
        .expect("the stored upload file");
    std::fs::OpenOptions::new()
        .write(true)
        .open(&stored_path)
        .expect("open the stored upload")
        .set_modified(UNIX_EPOCH + Duration::from_secs(mtime))
        .expect("pin the stored mtime");

    let auth: Arc<dyn AuthBackend> = Arc::new(OwnerAuth);
    let (_read, _write, search) = backend.trait_objects();
    let app = Router::new().merge(search_attachments::router(
        search,
        auth,
        RequestScheme::Http,
    ));

    let cases = oracle["cases"].as_array().expect("cases");
    assert!(!cases.is_empty(), "the oracle records cases");
    let mut failures = Vec::new();
    for case in cases {
        let name = case["name"].as_str().expect("case name");
        let method = Method::from_bytes(case["method"].as_str().expect("method").as_bytes())
            .expect("method");
        let uri = format!("/api/uploads/{id}");
        let mut builder = Request::builder()
            .method(method)
            .uri(&uri)
            .header(header::AUTHORIZATION, format!("Bearer {OWNER}"));
        for (key, value) in case["request_headers"]
            .as_object()
            .expect("request headers")
        {
            builder = builder.header(key.as_str(), value.as_str().expect("header value"));
        }
        let response = app
            .clone()
            .oneshot(builder.body(Body::empty()).expect("request"))
            .await
            .expect("the router answers every request");
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body")
            .to_vec();
        let got = normalise(status, &headers, body);

        let want_status = case["status"].as_u64().expect("status") as u16;
        let want_headers: BTreeMap<String, String> = case["headers"]
            .as_object()
            .expect("headers")
            .iter()
            .map(|(key, value)| (key.clone(), value.as_str().expect("header").to_owned()))
            .collect();
        let want_body = STANDARD
            .decode(case["body_base64"].as_str().expect("body"))
            .expect("body is base64");
        let want_multipart = case["multipart"].as_bool().expect("multipart flag");

        if got.status != want_status {
            failures.push(format!("{name}: status {} != {want_status}", got.status));
        }
        if got.headers != want_headers {
            failures.push(format!(
                "{name}: headers {:?} != {:?}",
                got.headers, want_headers
            ));
        }
        if got.multipart != want_multipart {
            failures.push(format!(
                "{name}: multipart {} != {want_multipart}",
                got.multipart
            ));
        }
        if got.body != want_body {
            failures.push(format!(
                "{name}: body differs ({} bytes vs {} bytes)",
                got.body.len(),
                want_body.len()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "upload download differs from the Python desk:\n{}",
        failures.join("\n")
    );
}
