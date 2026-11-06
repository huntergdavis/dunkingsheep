//! Integration tests for the production messages backend (t3210).
//!
//! The implementation under test is `src/messages_backend.rs`, included by path so that
//! `lib.rs`, which the shared-exports task owns, is not edited. The tests drive the real
//! routers over the real backend: a temporary on-disk SQLite database, a temporary uploads
//! folder, and a fake daemon speaking the newline-delimited JSON protocol over a Unix
//! socket. No live service, token file or process environment is involved.

pub use ds_web::{roster, routes};

#[allow(dead_code)]
#[path = "../src/messages_backend.rs"]
mod messages_backend;

use std::{
    io::{BufRead, BufReader, Write},
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
    process::Command,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::{to_bytes, Body},
    http::{header, HeaderMap, Method, Request, StatusCode},
    Extension, Router,
};
use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use tower::ServiceExt;

use messages_backend::{MessagesBackend, MessagesBackendConfig, UnixDaemonConnector};
use routes::{
    auth::{AuthBackend, AuthError, AuthFuture, Identity, RequestScheme},
    messages_read, messages_write, search_attachments,
    search_attachments::{SearchAttachmentsBackend, UploadPart},
};

const OWNER: &str = "owner-token";
const API: &str = "api-token";
const HUGE: &str = "1267650600228229401496703205376"; // 2**100

/// Bearer tokens map to identities; anything else is signed out.
struct TokenAuth;

impl AuthBackend for TokenAuth {
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
                Some(API) => Ok(Identity::Api),
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

type Handler = Arc<dyn Fn(&str, &Map<String, Value>) -> Result<Value, String> + Send + Sync>;

/// A daemon that answers each request on its own connection, as `FlockClient` expects.
struct FakeDaemon {
    socket: PathBuf,
    calls: Arc<Mutex<Vec<String>>>,
}

fn spawn_daemon(directory: &Path, handler: Handler) -> FakeDaemon {
    let socket = directory.join("dunkingsheep.sock");
    let listener = UnixListener::bind(&socket).expect("bind the fake daemon socket");
    let calls = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&calls);
    thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let Ok(reader) = stream.try_clone() else {
                continue;
            };
            let mut line = String::new();
            if BufReader::new(reader).read_line(&mut line).unwrap_or(0) == 0 {
                continue;
            }
            let request: Value = serde_json::from_str(&line).expect("request is JSON");
            let cmd = request["cmd"].as_str().unwrap_or_default().to_owned();
            let args = request["args"].as_object().cloned().unwrap_or_default();
            log.lock().expect("call log").push(cmd.clone());
            let outcome = if cmd == "status" {
                Ok(json!({}))
            } else {
                handler(&cmd, &args)
            };
            let reply = match outcome {
                Ok(result) => json!({"id": request["id"], "ok": true, "result": result}),
                Err(error) => json!({"id": request["id"], "ok": false, "error": error}),
            };
            writeln!(stream, "{reply}").expect("write the reply");
        }
    });
    FakeDaemon { socket, calls }
}

/// One lane pane that is messageable without a capability probe.
fn lane_command(cmd: &str) -> Option<Result<Value, String>> {
    match cmd {
        "list_panes" => Some(Ok(json!({"panes": [{
            "pane_id": "%7",
            "agent": "codex",
            "agent_status": "idle",
            "tab_label": "lane",
            "workspace_label": "desk",
            "cwd": "/work/lane",
        }]}))),
        "delivery_capability" => Some(Ok(json!({
            "alive": true,
            "send": "guarded",
            "agent_alive": true,
        }))),
        _ => None,
    }
}

/// A guarded lane whose sends are confirmed at once.
fn sent_handler(cmd: &str, _args: &Map<String, Value>) -> Result<Value, String> {
    lane_command(cmd).unwrap_or_else(|| match cmd {
        "send_text" => {
            Ok(json!({"ok": true, "outcome": "sent", "message_id": "m-1", "message": ""}))
        }
        other => Err(format!("unexpected daemon command {other}")),
    })
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64()
}

struct Harness {
    _directory: tempfile::TempDir,
    app: Router,
    backend: MessagesBackend,
    daemon: FakeDaemon,
}

impl Harness {
    fn new(
        handler: impl Fn(&str, &Map<String, Value>) -> Result<Value, String> + Send + Sync + 'static,
    ) -> Self {
        let directory = tempfile::tempdir().expect("temporary directory");
        let daemon = spawn_daemon(directory.path(), Arc::new(handler));
        let connector = Arc::new(UnixDaemonConnector::new(
            daemon.socket.clone(),
            Some("service-token".to_owned()),
            Duration::from_secs(5),
            "messages backend test",
        ));
        let backend = MessagesBackend::open(MessagesBackendConfig {
            data_dir: directory.path().join("desk"),
            channels: Vec::new(),
            daemon: connector,
        })
        .expect("open the backend");
        let (read, write, search) = backend.trait_objects();
        let auth: Arc<dyn AuthBackend> = Arc::new(TokenAuth);
        let app = Router::new()
            .merge(messages_read::router())
            .merge(messages_write::router(
                write,
                Arc::clone(&auth),
                RequestScheme::Http,
            ))
            .merge(search_attachments::router(
                search,
                Arc::clone(&auth),
                RequestScheme::Http,
            ))
            .layer(Extension(auth))
            .layer(Extension(read));
        Self {
            _directory: directory,
            app,
            backend,
            daemon,
        }
    }

    fn database(&self) -> Connection {
        Connection::open(self.backend.database_path()).expect("open the database")
    }

    fn calls(&self, cmd: &str) -> usize {
        self.daemon
            .calls
            .lock()
            .expect("call log")
            .iter()
            .filter(|call| call.as_str() == cmd)
            .count()
    }

    async fn send(
        &self,
        method: Method,
        uri: &str,
        token: Option<&str>,
        content_type: Option<&str>,
        range: Option<&str>,
        body: Vec<u8>,
    ) -> Reply {
        let mut builder = Request::builder().method(method).uri(uri);
        if let Some(token) = token {
            builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
        }
        if let Some(content_type) = content_type {
            builder = builder.header(header::CONTENT_TYPE, content_type);
        }
        if let Some(range) = range {
            builder = builder.header(header::RANGE, range);
        }
        let request = builder.body(Body::from(body)).expect("request");
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("the router answers every request");
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body")
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    /// A GET with explicit request headers, for conditional and range requests.
    async fn send_with_headers(&self, uri: &str, headers: &[(header::HeaderName, &str)]) -> Reply {
        let mut builder = Request::builder()
            .method(Method::GET)
            .uri(uri)
            .header(header::AUTHORIZATION, format!("Bearer {OWNER}"));
        for (name, value) in headers {
            builder = builder.header(name.clone(), *value);
        }
        let request = builder.body(Body::empty()).expect("request");
        let response = self
            .app
            .clone()
            .oneshot(request)
            .await
            .expect("the router answers every request");
        let status = response.status();
        let headers = response.headers().clone();
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body")
            .to_vec();
        Reply {
            status,
            headers,
            body,
        }
    }

    async fn get(&self, uri: &str, token: &str) -> Reply {
        self.send(Method::GET, uri, Some(token), None, None, Vec::new())
            .await
    }

    async fn post(&self, uri: &str, token: &str, value: &Value) -> Reply {
        self.send(
            Method::POST,
            uri,
            Some(token),
            Some("application/json"),
            None,
            value.to_string().into_bytes(),
        )
        .await
    }
}

struct Reply {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

fn multipart(filename: &str, mime: &str, bytes: &[u8]) -> (String, Vec<u8>) {
    let boundary = "----claude-messages-backend";
    let mut body = Vec::new();
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    (format!("multipart/form-data; boundary={boundary}"), body)
}

/// Runs `python3 -c` and returns its stdout, failing the test if Python cannot run.
fn python(script: &str, arguments: &[&str]) -> String {
    let output = Command::new("python3")
        .arg("-I")
        .arg("-c")
        .arg(script)
        .args(arguments)
        .output()
        .expect("python3 is required for the CPython oracle");
    assert!(
        output.status.success(),
        "python3 failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).expect("UTF-8 output")
}

/// The state a rolled-back write must leave untouched.
fn snapshot(database: &Connection, question_id: i64) -> (i64, i64, Option<String>) {
    let count = database
        .query_row("SELECT COUNT(*) FROM messages", [], |row| row.get(0))
        .expect("count");
    let sequence = database
        .query_row(
            "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name='messages'), 0)",
            [],
            |row| row.get(0),
        )
        .expect("sequence");
    let answered = database
        .query_row(
            "SELECT answered_at FROM messages WHERE id=?1",
            params![question_id],
            |row| row.get(0),
        )
        .expect("answered_at");
    (count, sequence, answered)
}

#[tokio::test]
async fn reads_have_python_shapes_on_an_empty_database() {
    let harness = Harness::new(sent_handler);
    let page = harness.get("/api/messages?channel=pane-7", OWNER).await;
    assert_eq!(page.status, StatusCode::OK);
    assert_eq!(
        page.json(),
        json!({"items": [], "has_more": false, "last_delivery": null, "next_before": null})
    );
    let unknown = harness.get("/api/messages?channel=nope", OWNER).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.json(), json!({"detail": "Unknown channel"}));
    let deliveries = harness
        .get("/api/deliveries?channel=pane-7&ids=1,2", OWNER)
        .await;
    assert_eq!(deliveries.json(), json!({"deliveries": {}}));
    let context = harness.get("/api/messages/9/context", OWNER).await;
    assert_eq!(context.status, StatusCode::NOT_FOUND);
    assert_eq!(context.json(), json!({"detail": "Message not found"}));
}

#[tokio::test]
async fn post_then_read_keeps_the_direct_receipt() {
    let harness = Harness::new(sent_handler);
    let posted = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": "  hello  "}),
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.text());
    let item = posted.json();
    assert_eq!(item["body"], json!("hello"));
    assert_eq!(item["delivery"], json!("sent"));
    assert_eq!(item["outcome"], json!("sent"));
    assert_eq!(item["transport"]["route"], json!("guarded"));
    assert_eq!(item["transport"]["message_id"], json!("m-1"));
    assert_eq!(item["attachments"], json!([]));
    let page = harness
        .get("/api/messages?channel=pane-7", OWNER)
        .await
        .json();
    assert_eq!(page["items"][0]["id"], item["id"]);
    assert_eq!(page["items"][0]["delivery"], json!("sent"));
    assert_eq!(harness.calls("send_text"), 1);
}

#[tokio::test]
async fn post_checks_run_in_python_order() {
    let harness = Harness::new(|cmd, _args| {
        lane_command(cmd).unwrap_or_else(|| Err(format!("unexpected daemon command {cmd}")))
    });
    // U+001C is whitespace to Python's str.strip(), so this body is empty.
    let empty = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": " \u{1c} "}),
        )
        .await;
    assert_eq!(empty.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(empty.json(), json!({"detail": "Message is empty"}));
    let unknown = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "zzz", "body": ""}),
        )
        .await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert_eq!(unknown.json(), json!({"detail": "Unknown channel"}));
    let missing = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": "x", "attachment_ids": ["nope"]}),
        )
        .await;
    assert_eq!(missing.status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(
        missing.json(),
        json!({"detail": "Attachment nope is unavailable"})
    );
    assert_eq!(harness.calls("send_text"), 0);
}

#[tokio::test]
async fn external_id_replays_the_stored_message_without_resending() {
    let harness = Harness::new(sent_handler);
    let body = json!({"channel": "pane-7", "body": "once", "external_id": "ext-1"});
    let first = harness.post("/api/messages", OWNER, &body).await.json();
    let second = harness.post("/api/messages", OWNER, &body).await.json();
    assert_eq!(second["id"], first["id"]);
    assert_eq!(harness.calls("send_text"), 1);
}

#[tokio::test]
async fn failed_direct_send_is_uncertain_and_keeps_the_error_text() {
    let harness = Harness::new(|cmd, _args| {
        lane_command(cmd).unwrap_or_else(|| match cmd {
            "send_text" => Err("socket reset".to_owned()),
            other => Err(format!("unexpected daemon command {other}")),
        })
    });
    let posted = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": "lost?"}),
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.text());
    let item = posted.json();
    assert_eq!(item["delivery"], json!("uncertain"));
    assert_eq!(
        item["delivery_detail"],
        json!("Delivery unconfirmed. Check agent activity before sending again.")
    );
    let error = item["transport"]["error"]
        .as_str()
        .expect("transport error");
    assert!(
        error.starts_with("502: delivery path unavailable: "),
        "{error}"
    );
    assert!(error.ends_with("socket reset"), "{error}");
    assert_eq!(item["transport_attempts"], json!(1));
}

#[tokio::test]
async fn brief_overflow_rolls_back_the_message_attachment_answers_and_sequence() {
    let harness = Harness::new(sent_handler);
    let (content_type, body) = multipart("note.txt", "text/plain", b"hello");
    let upload = harness
        .send(
            Method::POST,
            "/api/uploads",
            Some(OWNER),
            Some(&content_type),
            None,
            body,
        )
        .await;
    assert_eq!(upload.status, StatusCode::OK, "{}", upload.text());
    let attachment = upload.json()["id"]
        .as_str()
        .expect("attachment id")
        .to_owned();
    let question = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": "question?"}),
        )
        .await
        .json();
    let question_id = question["id"].as_i64().expect("question id");
    let before = snapshot(&harness.database(), question_id);

    // The 2**100 answer fails at SQLite binding, after the valid answer was applied.
    let overflow = format!(
        r#"{{"channel":"pane-7","body":"done","answers":[{question_id},{HUGE}],"attachment_ids":["{attachment}"]}}"#
    );
    let refused = harness
        .send(
            Method::POST,
            "/api/brief",
            Some(API),
            Some("application/json"),
            None,
            overflow.into_bytes(),
        )
        .await;
    assert_eq!(refused.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(refused.text(), "Internal Server Error");
    let database = harness.database();
    assert_eq!(
        snapshot(&database, question_id),
        before,
        "nothing may persist"
    );
    let linked: Option<i64> = database
        .query_row(
            "SELECT message_id FROM attachments WHERE id=?1",
            params![attachment],
            |row| row.get(0),
        )
        .expect("attachment row");
    assert_eq!(linked, None);
    drop(database);

    // A valid brief commits the message, the attachment link and the answer together.
    let valid = format!(
        r#"{{"channel":"pane-7","body":"done","answers":[{question_id}],"attachment_ids":["{attachment}"]}}"#
    );
    let brief = harness
        .send(
            Method::POST,
            "/api/brief",
            Some(API),
            Some("application/json"),
            None,
            valid.into_bytes(),
        )
        .await;
    assert_eq!(brief.status, StatusCode::OK, "{}", brief.text());
    let brief = brief.json();
    assert_eq!(brief["attachments"][0]["id"], json!(attachment));
    let database = harness.database();
    let (answer_id, answered_at): (Option<i64>, Option<String>) = database
        .query_row(
            "SELECT answer_id, answered_at FROM messages WHERE id=?1",
            params![question_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("answered question");
    assert_eq!(answer_id, brief["id"].as_i64());
    assert_eq!(answered_at.as_deref(), brief["created_at"].as_str());
    let linked: Option<i64> = database
        .query_row(
            "SELECT message_id FROM attachments WHERE id=?1",
            params![attachment],
            |row| row.get(0),
        )
        .expect("attachment row");
    assert_eq!(linked, brief["id"].as_i64());
}

#[tokio::test]
async fn out_of_range_integers_fail_closed_on_reads() {
    let harness = Harness::new(sent_handler);
    let page = harness
        .get(
            &format!("/api/messages?channel=pane-7&before={HUGE}"),
            OWNER,
        )
        .await;
    assert_eq!(page.status, StatusCode::INTERNAL_SERVER_ERROR);
    let deliveries = harness
        .get(&format!("/api/deliveries?channel=pane-7&ids={HUGE}"), OWNER)
        .await;
    assert_eq!(deliveries.status, StatusCode::INTERNAL_SERVER_ERROR);
}

#[tokio::test]
async fn upload_is_private_and_download_serves_ranges_with_stat_headers() {
    let harness = Harness::new(sent_handler);
    let (content_type, body) = multipart("notes (1).txt", "text/plain", b"hello world");
    let upload = harness
        .send(
            Method::POST,
            "/api/uploads",
            Some(OWNER),
            Some(&content_type),
            None,
            body,
        )
        .await;
    assert_eq!(upload.status, StatusCode::OK, "{}", upload.text());
    let meta = upload.json();
    assert_eq!(meta["filename"], json!("notes (1).txt"));
    assert_eq!(meta["mime"], json!("text/plain"));
    assert_eq!(meta["size"], json!(11));
    let id = meta["id"].as_str().expect("attachment id").to_owned();
    let path: String = harness
        .database()
        .query_row(
            "SELECT path FROM attachments WHERE id=?1",
            params![id],
            |row| row.get(0),
        )
        .expect("stored path");
    let mode = std::fs::metadata(&path)
        .expect("stored file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "uploads are private to the owner");

    let file = harness.get(&format!("/api/uploads/{id}"), OWNER).await;
    assert_eq!(file.status, StatusCode::OK);
    assert_eq!(file.body, b"hello world".to_vec());
    assert_eq!(
        file.headers
            .get(header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()),
        Some("11")
    );
    assert!(file.headers.contains_key(header::LAST_MODIFIED));
    let etag = file
        .headers
        .get(header::ETAG)
        .and_then(|v| v.to_str().ok())
        .expect("etag");
    assert!(
        etag.starts_with('"') && etag.ends_with('"') && etag.len() == 34,
        "{etag}"
    );

    let ranged = harness
        .send(
            Method::GET,
            &format!("/api/uploads/{id}"),
            Some(OWNER),
            None,
            Some("bytes=0-4"),
            Vec::new(),
        )
        .await;
    assert_eq!(ranged.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(ranged.body, b"hello".to_vec());

    let head = harness
        .send(
            Method::HEAD,
            &format!("/api/uploads/{id}"),
            Some(OWNER),
            None,
            None,
            Vec::new(),
        )
        .await;
    assert_eq!(head.status, StatusCode::METHOD_NOT_ALLOWED);

    let missing = harness.get("/api/uploads/nope", OWNER).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    assert_eq!(missing.json(), json!({"detail": "Attachment not found"}));
}

#[tokio::test]
async fn download_range_contract_holds_on_the_stored_file() {
    let harness = Harness::new(sent_handler);
    let saved = harness
        .backend
        .save_upload(UploadPart {
            filename: "range.txt".to_owned(),
            response_mime: Some("text/plain".to_owned()),
            storage_mime: "text/plain".to_owned(),
            bytes: b"hello world".to_vec(),
        })
        .expect("save the upload");
    let id = saved["id"].as_str().expect("attachment id").to_owned();
    let uri = format!("/api/uploads/{id}");
    let stored = harness
        .backend
        .attachment(&id)
        .expect("attachment lookup")
        .expect("attachment present");

    let partial = harness
        .send(
            Method::GET,
            &uri,
            Some(OWNER),
            None,
            Some("bytes=6-10"),
            Vec::new(),
        )
        .await;
    assert_eq!(partial.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(partial.body, b"world".to_vec());

    let unsatisfiable = harness
        .send(
            Method::GET,
            &uri,
            Some(OWNER),
            None,
            Some("bytes=100-200"),
            Vec::new(),
        )
        .await;
    assert_eq!(unsatisfiable.status, StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(
        unsatisfiable
            .headers
            .get(header::CONTENT_RANGE)
            .and_then(|value| value.to_str().ok()),
        Some("bytes */11")
    );

    let malformed = harness
        .send(
            Method::GET,
            &uri,
            Some(OWNER),
            None,
            Some("pages=0-1"),
            Vec::new(),
        )
        .await;
    assert_eq!(malformed.status, StatusCode::BAD_REQUEST);

    // If-Range: the stored validator keeps the range; a stale validator returns the whole file.
    let etag = stored.metadata.etag.clone();
    let matched = harness
        .send_with_headers(
            &uri,
            &[
                (header::RANGE, "bytes=0-4"),
                (header::IF_RANGE, etag.as_str()),
            ],
        )
        .await;
    assert_eq!(matched.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(matched.body, b"hello".to_vec());
    let stale = harness
        .send_with_headers(
            &uri,
            &[
                (header::RANGE, "bytes=0-4"),
                (header::IF_RANGE, "\"stale\""),
            ],
        )
        .await;
    assert_eq!(stale.status, StatusCode::OK);
    assert_eq!(stale.body, b"hello world".to_vec());

    let multipart = harness
        .send(
            Method::GET,
            &uri,
            Some(OWNER),
            None,
            Some("bytes=0-1,6-7"),
            Vec::new(),
        )
        .await;
    assert_eq!(multipart.status, StatusCode::PARTIAL_CONTENT);
    assert!(multipart
        .headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with("multipart/byteranges")));
}

#[tokio::test]
async fn send_now_delivers_a_held_message_once_its_draft_clears() {
    let sends = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&sends);
    let harness = Harness::new(move |cmd, _args| {
        lane_command(cmd).unwrap_or_else(|| match cmd {
            "send_text" => {
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    Ok(json!({"ok": true, "queued": true, "outcome": "queued", "message_id": "q-9", "message": ""}))
                } else {
                    Ok(json!({"ok": true, "outcome": "sent", "message": ""}))
                }
            }
            "get_message" => Ok(json!({"message_id": "q-9", "status": "queued", "message": "", "pane_id": "%7"})),
            "list_messages" => Ok(json!({"queued": [{"message_id": "q-9", "blocked_by": "input box contains a draft"}], "recent": []})),
            "cancel_message" => Ok(json!({"message_id": "q-9", "status": "cancelled"})),
            other => Err(format!("unexpected daemon command {other}")),
        })
    });
    let posted = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": "hold this"}),
        )
        .await
        .json();
    assert_eq!(posted["delivery"], json!("queued"));
    let id = posted["id"].as_i64().expect("message id");
    let api = harness
        .send(
            Method::POST,
            &format!("/api/messages/{id}/send-now"),
            Some(API),
            None,
            None,
            Vec::new(),
        )
        .await;
    assert_eq!(api.status, StatusCode::FORBIDDEN);
    let sent = harness
        .send(
            Method::POST,
            &format!("/api/messages/{id}/send-now"),
            Some(OWNER),
            None,
            None,
            Vec::new(),
        )
        .await;
    assert_eq!(sent.status, StatusCode::OK, "{}", sent.text());
    assert_eq!(sent.json()["delivery"], json!("sent"));
    assert_eq!(harness.calls("cancel_message"), 1);
}

#[tokio::test]
async fn pickup_receipt_reconciles_an_owner_push_on_the_next_read() {
    // Overrides come first: lane_command would otherwise answer delivery_capability as guarded.
    let harness = Harness::new(|cmd, _args| match cmd {
        "delivery_capability" => {
            Ok(json!({"alive": true, "send": "owner_push", "agent_alive": true}))
        }
        "owner_push" => Ok(
            json!({"id": 42, "pane_id": "%7", "at": 1.0, "outcome": "stored", "expect": "pickup"}),
        ),
        "owner_receipts" => {
            let now = now_seconds();
            Ok(
                json!({"receipts": [{"id": 42, "pane_id": "%7", "created_at": now, "taken_at": now}]}),
            )
        }
        other => {
            lane_command(other).unwrap_or_else(|| Err(format!("unexpected daemon command {other}")))
        }
    });
    let posted = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": "push me"}),
        )
        .await
        .json();
    assert_eq!(posted["delivery"], json!("stored"));
    assert_eq!(posted["transport"]["route"], json!("owner_push"));
    let page = harness
        .get("/api/messages?channel=pane-7", OWNER)
        .await
        .json();
    let item = &page["items"][0];
    assert_eq!(item["delivery"], json!("delivered"));
    assert_eq!(item["delivery_detail"], json!("Picked up by agent harness"));
    assert_eq!(page["last_delivery"]["id"], item["id"]);
}

#[tokio::test]
async fn search_matches_like_and_honours_the_channel_filter() {
    let harness = Harness::new(sent_handler);
    for body in ["Alpha beta", "gamma"] {
        harness
            .post(
                "/api/messages",
                OWNER,
                &json!({"channel": "pane-7", "body": body}),
            )
            .await;
    }
    let hit = harness.get("/api/search?q=ALPHA", OWNER).await.json();
    assert_eq!(hit["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(hit["items"][0]["body"], json!("Alpha beta"));
    assert_eq!(hit["has_more"], json!(false));
    assert_eq!(hit["next_before"], Value::Null);
    let short = harness.get("/api/search?q=a", OWNER).await.json();
    assert_eq!(short, json!({"items": [], "has_more": false}));
    let scoped = harness
        .get("/api/search?q=beta&channel=pane-7", OWNER)
        .await
        .json();
    assert_eq!(scoped["items"].as_array().map(Vec::len), Some(1));
    let unknown = harness.get("/api/search?q=beta&channel=nope", OWNER).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn attachment_metadata_matches_python_stat_formulas() {
    let harness = Harness::new(sent_handler);
    let saved = harness
        .backend
        .save_upload(UploadPart {
            filename: "report.pdf".to_owned(),
            response_mime: Some("application/pdf".to_owned()),
            storage_mime: "application/pdf".to_owned(),
            bytes: b"%PDF-1.4 test".to_vec(),
        })
        .expect("save the upload");
    let id = saved["id"].as_str().expect("attachment id").to_owned();
    let file = harness
        .backend
        .attachment(&id)
        .expect("attachment lookup")
        .expect("attachment present");
    let path: String = harness
        .database()
        .query_row(
            "SELECT path FROM attachments WHERE id=?1",
            params![id],
            |row| row.get(0),
        )
        .expect("stored path");
    // Starlette's FileResponse: ETag is md5("<st_mtime>-<st_size>") and Last-Modified is
    // email.utils.formatdate(st_mtime, usegmt=True). Both are computed by CPython here.
    let output = python(
        "import email.utils, hashlib, os, sys\n\
         st = os.stat(sys.argv[1])\n\
         print(hashlib.md5(f'{st.st_mtime}-{st.st_size}'.encode()).hexdigest())\n\
         print(email.utils.formatdate(st.st_mtime, usegmt=True))\n\
         print(st.st_size)",
        &[&path],
    );
    let lines: Vec<&str> = output.lines().collect();
    assert_eq!(file.metadata.etag, format!("\"{}\"", lines[0]));
    assert_eq!(file.metadata.last_modified, lines[1]);
    assert_eq!(file.metadata.content_length.to_string(), lines[2]);
    assert_eq!(file.bytes, b"%PDF-1.4 test".to_vec());
}

/// What the fake daemon answers for the delivery calls. Tests change it between requests.
#[derive(Clone)]
struct Plan {
    send_text: Result<Value, String>,
    get_message: Result<Value, String>,
    list_messages: Value,
    cancel: Result<Value, String>,
    lane_alive: bool,
}

impl Default for Plan {
    fn default() -> Self {
        Self {
            send_text: Ok(
                json!({"ok": true, "queued": true, "outcome": "queued", "message_id": "q-1", "message": ""}),
            ),
            get_message: Ok(
                json!({"message_id": "q-1", "pane_id": "%7", "text": "[VP web message #1; held]", "status": "queued", "message": "held"}),
            ),
            list_messages: json!({"queued": [{"message_id": "q-1", "blocked_by": "input box contains a draft"}], "recent": []}),
            cancel: Ok(json!({"message_id": "q-1", "status": "cancelled"})),
            lane_alive: true,
        }
    }
}

/// A harness whose daemon answers from `plan`, so a test can change the answers between requests.
fn planned_harness(plan: Arc<Mutex<Plan>>) -> Harness {
    Harness::new(move |cmd, _args| {
        let plan: Plan = plan.lock().expect("plan").clone();
        match cmd {
            "send_text" => plan.send_text,
            "get_message" => plan.get_message,
            "list_messages" => Ok(plan.list_messages),
            "cancel_message" => plan.cancel,
            "list_panes" => {
                let mut pane = json!({"pane_id": "%7", "tab_label": "lane", "workspace_label": "desk", "cwd": "/work/lane"});
                if plan.lane_alive {
                    pane["agent"] = json!("codex");
                    pane["agent_status"] = json!("idle");
                }
                Ok(json!({"panes": [pane]}))
            }
            "delivery_capability" => {
                Ok(json!({"alive": true, "send": "guarded", "agent_alive": plan.lane_alive}))
            }
            other => Err(format!("unexpected daemon command {other}")),
        }
    })
}

/// Posts message 1 as a guarded queued send and returns its harness and plan.
async fn posted_queued() -> (Harness, Arc<Mutex<Plan>>) {
    let plan = Arc::new(Mutex::new(Plan::default()));
    let harness = planned_harness(Arc::clone(&plan));
    let posted = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": "held"}),
        )
        .await
        .json();
    assert_eq!(posted["id"], json!(1));
    assert_eq!(posted["delivery"], json!("queued"));
    (harness, plan)
}

async fn send_now_one(harness: &Harness) -> Reply {
    harness
        .send(
            Method::POST,
            "/api/messages/1/send-now",
            Some(OWNER),
            None,
            None,
            Vec::new(),
        )
        .await
}

fn assert_refusal(reply: Reply, status: StatusCode, detail: &str) {
    assert_eq!(reply.status, status, "{}", reply.text());
    assert_eq!(reply.json(), json!({"detail": detail}));
}

/// Posts message 1, answers the daemon's record for it with `record`, and returns the item.
async fn reconciled_with(record: Result<Value, String>) -> Value {
    let (harness, plan) = posted_queued().await;
    plan.lock().expect("plan").get_message = record;
    harness
        .get("/api/messages?channel=pane-7", OWNER)
        .await
        .json()["items"][0]
        .clone()
}

#[tokio::test]
async fn empty_external_id_is_absent_on_every_post() {
    let harness = Harness::new(sent_handler);
    let first = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": "one", "external_id": ""}),
        )
        .await;
    let second = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": "two", "external_id": ""}),
        )
        .await;
    assert_eq!(first.status, StatusCode::OK, "{}", first.text());
    assert_eq!(second.status, StatusCode::OK, "{}", second.text());
    assert_ne!(first.json()["id"], second.json()["id"]);
    assert_eq!(harness.calls("send_text"), 2);
}

#[tokio::test]
async fn guarded_receipts_reconcile_to_each_daemon_state() {
    let text = "[VP web message #1; held]";
    let sent = reconciled_with(Ok(json!({
        "message_id": "q-1", "pane_id": "%7", "text": text,
        "status": "sent", "message": "delivered", "finished_at": 1_700_000_000.5
    })))
    .await;
    assert_eq!(sent["delivery"], json!("sent"));
    assert_eq!(sent["delivery_detail"], json!("delivered"));
    assert_eq!(sent["delivered_at"], json!(1_700_000_000.5));

    let queued = reconciled_with(Ok(json!({
        "message_id": "q-1", "pane_id": "%7", "text": text,
        "status": "queued", "message": "waiting for the lane"
    })))
    .await;
    assert_eq!(queued["delivery"], json!("queued"));
    assert_eq!(queued["delivery_detail"], json!("waiting for the lane"));
    assert_eq!(queued["delivered_at"], Value::Null);

    let cancelled = reconciled_with(Ok(json!({
        "message_id": "q-1", "pane_id": "%7", "text": text,
        "status": "cancelled", "message": "cancelled by the owner"
    })))
    .await;
    assert_eq!(cancelled["delivery"], json!("cancelled"));
    assert_eq!(
        cancelled["delivery_detail"],
        json!("cancelled by the owner")
    );

    let uncertain = reconciled_with(Err("no message with id 'q-1'".to_owned())).await;
    assert_eq!(uncertain["delivery"], json!("uncertain"));
    assert_eq!(
        uncertain["delivery_detail"],
        json!("Delivery unconfirmed; the transport record for this send is unavailable. Check agent activity before sending again.")
    );
}

#[tokio::test]
async fn send_now_refusals_use_the_python_states_and_details() {
    // No typing and no draft: the route will not force the held message out.
    let (harness, plan) = posted_queued().await;
    {
        let mut plan = plan.lock().expect("plan");
        plan.get_message = Ok(json!({"message_id": "q-1", "status": "queued", "message": "plain"}));
        plan.list_messages =
            json!({"queued": [{"message_id": "q-1", "blocked_by": null}], "recent": []});
    }
    assert_refusal(
        send_now_one(&harness).await,
        StatusCode::CONFLICT,
        "This message is queued for transport, not held behind a draft",
    );

    // The transport record cannot be read.
    let (harness, plan) = posted_queued().await;
    plan.lock().expect("plan").get_message = Err("socket closed".to_owned());
    assert_refusal(
        send_now_one(&harness).await,
        StatusCode::CONFLICT,
        "Transport state unavailable; try again shortly",
    );

    // The daemon says the message already left the queue.
    let (harness, plan) = posted_queued().await;
    plan.lock().expect("plan").get_message =
        Ok(json!({"message_id": "q-1", "status": "cancelled", "message": ""}));
    assert_refusal(
        send_now_one(&harness).await,
        StatusCode::CONFLICT,
        "Message is no longer queued; refresh its status",
    );

    // The cancel races a send: the record is still queued afterwards.
    let (harness, plan) = posted_queued().await;
    {
        let mut plan = plan.lock().expect("plan");
        plan.get_message =
            Ok(json!({"message_id": "q-1", "status": "queued", "message": "someone is typing"}));
        plan.cancel = Err("already gone".to_owned());
    }
    assert_refusal(
        send_now_one(&harness).await,
        StatusCode::CONFLICT,
        "Message changed while sending; refresh its status",
    );

    // The message is recovering, so its delivery is no longer queued or pending.
    let (harness, _plan) = posted_queued().await;
    harness
        .database()
        .execute("UPDATE messages SET delivery='failed' WHERE id=1", [])
        .expect("mark the message failed");
    assert_refusal(
        send_now_one(&harness).await,
        StatusCode::CONFLICT,
        "Message is being recovered; try again shortly",
    );
}

#[tokio::test]
async fn send_now_pane_loss_and_urgent_failure_leave_the_message_uncertain() {
    // The lane stops being an agent after the message was held.
    let (harness, plan) = posted_queued().await;
    plan.lock().expect("plan").lane_alive = false;
    assert_refusal(
        send_now_one(&harness).await,
        StatusCode::CONFLICT,
        "Agent pane unavailable; delivery is unconfirmed",
    );
    let state: String = harness
        .database()
        .query_row(
            "SELECT delivery || '|' || COALESCE(transport_id, 'null') FROM messages WHERE id=1",
            [],
            |row| row.get(0),
        )
        .expect("the message row");
    assert_eq!(state, "uncertain|null");

    // The urgent send itself fails after the cancel succeeded.
    let (harness, plan) = posted_queued().await;
    plan.lock().expect("plan").send_text = Err("boom".to_owned());
    assert_refusal(
        send_now_one(&harness).await,
        StatusCode::SERVICE_UNAVAILABLE,
        "Urgent send unconfirmed; check agent activity before resending",
    );
    let delivery: String = harness
        .database()
        .query_row("SELECT delivery FROM messages WHERE id=1", [], |row| {
            row.get(0)
        })
        .expect("the message row");
    assert_eq!(delivery, "uncertain");
}

#[tokio::test]
async fn post_refuses_when_no_agent_is_available_in_the_channel() {
    let plan = Arc::new(Mutex::new(Plan {
        lane_alive: false,
        ..Plan::default()
    }));
    let harness = planned_harness(plan);
    let refused = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": "nobody"}),
        )
        .await;
    assert_refusal(
        refused,
        StatusCode::CONFLICT,
        "No active agent is available in this channel",
    );
    assert_eq!(harness.calls("send_text"), 0);
}

#[tokio::test]
async fn prompt_over_16000_characters_is_refused_before_the_daemon() {
    let harness = Harness::new(sent_handler);
    let mut attachments = Vec::new();
    for index in 0..10usize {
        // 176 digits and ".txt" make a 180-character filename, the route's maximum.
        let name = format!("{index:0>176}.txt");
        let (content_type, body) = multipart(&name, "text/plain", b"hello");
        let upload = harness
            .send(
                Method::POST,
                "/api/uploads",
                Some(OWNER),
                Some(&content_type),
                None,
                body,
            )
            .await;
        assert_eq!(upload.status, StatusCode::OK, "{}", upload.text());
        attachments.push(upload.json()["id"].clone());
    }
    // The body alone is at its 12000-character limit; the attachment references push the prompt past 16000.
    let body = "x".repeat(12_000);
    let posted = harness
        .post(
            "/api/messages",
            OWNER,
            &json!({"channel": "pane-7", "body": body, "attachment_ids": attachments}),
        )
        .await;
    assert_eq!(posted.status, StatusCode::OK, "{}", posted.text());
    let item = posted.json();
    assert_eq!(item["delivery"], json!("uncertain"));
    assert_eq!(
        item["transport"]["error"],
        json!("400: keep it under 16000 characters")
    );
    assert_eq!(harness.calls("send_text"), 0);
    assert_eq!(harness.calls("delivery_capability"), 0);
}

#[tokio::test]
async fn upload_over_25_mb_is_refused_by_the_backend() {
    let harness = Harness::new(sent_handler);
    let refused = harness.backend.save_upload(UploadPart {
        filename: "large.bin".to_owned(),
        response_mime: None,
        storage_mime: "application/octet-stream".to_owned(),
        bytes: vec![0u8; 25 * 1024 * 1024 + 1],
    });
    assert_eq!(
        refused,
        Err(routes::search_attachments::SearchAttachmentsError::Http {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            detail: "Files must be 25 MB or smaller".to_owned(),
        })
    );
}

/// Whitespace-insensitive form of a SQL or message literal, with Python's JSON escapes
/// (`\n`, `\"`) read as the characters they stand for.
fn normalize(text: &str) -> String {
    text.replace("\\n", " ")
        .replace("\\\"", "\"")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn messages_section_strings_are_present_in_the_module() {
    // Every SQL statement and user-visible message literal in web/app.py's messages
    // section (its handlers and helpers) must appear in the Rust module.
    let app = concat!(env!("CARGO_MANIFEST_DIR"), "/../../../web/app.py");
    let output = python(
        r#"import ast, json, sys
tree = ast.parse(open(sys.argv[1], encoding="utf-8").read())
wanted = {
    "messages", "deliveries", "message_context", "post_message", "_post_message",
    "send_now", "post_brief", "search", "upload", "download", "_message",
    "_prompt_for", "_send_saved", "_send_to_pane", "_capability", "_target",
    "_can_message", "_known_channel", "_reconcile", "_reconcile_direct_guarded",
    "_reconcile_owner_pushes",
}
found = set()
for node in ast.walk(tree):
    if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef)) and node.name in wanted:
        body = node.body
        first = body[0] if body else None
        if isinstance(first, ast.Expr) and isinstance(first.value, ast.Constant) and isinstance(first.value.value, str):
            body = body[1:]
        for statement in body:
            for sub in ast.walk(statement):
                if isinstance(sub, ast.Constant) and isinstance(sub.value, str):
                    found.add(sub.value)
keep = sorted(s for s in found if len(s.strip()) >= 4 and "%s" not in s)
print(json.dumps([json.dumps(s)[1:-1] for s in keep]))
"#,
        &[app],
    );
    let constants: Vec<String> = serde_json::from_str(&output).expect("constant list");
    // Route-owned literals: the HTTP layer supplies these, not the backend.
    let route_owned = [
        "application/octet-stream",
        "Attachment not found",
        "attachment",
        "inline",
        "nosniff",
        "X-Content-Type-Options",
        "image/png",
        "image/jpeg",
        "image/gif",
        "image/webp",
        // Request validation and authorization are the route layer's (messages_read.rs,
        // messages_write.rs, search_attachments.rs), not the backend's.
        "API token required",
        "Owner sign-in required",
        "Invalid message IDs",
        "Too many or invalid message IDs",
        // Upload filename sanitization is the route's `sanitize_filename`.
        "[^\\\\w. ()-]",
    ];
    let module = normalize(
        &std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/messages_backend.rs"
        ))
        .expect("module source"),
    );
    let missing: Vec<&String> = constants
        .iter()
        .filter(|literal| !route_owned.contains(&literal.as_str()))
        .filter(|literal| !literal.ends_with("LIMIT 51"))
        .filter(|literal| !module.contains(&normalize(literal)))
        .collect();
    assert!(
        missing.is_empty(),
        "web/app.py literals missing from the module: {missing:#?}"
    );
}
