//! t1702 integer gate, slice 1: POST /api/brief against the Python desk on the served venv.
//!
//! The oracle is `fixtures/messages_int_oracle.json`, captured from the Python desk's brief route
//! (FastAPI 0.162.0 / Starlette 1.10.0 / Pydantic 2.14.2, the served release venv) on a real
//! on-disk messages.sqlite3. This test replays the brief cases in the same order against the
//! Rust `messages_write` router over the production `MessagesBackend`, and checks the status,
//! the body (exact for errors, key set for successes) and the message/attachment/answer counts
//! before and after every request. Python's 2**100 and 2**63 answers are 500s that roll back, and
//! so must these.
//!
//! NOT covered here: the send-now cases recorded in the same fixture (a later slice), the
//! production daemon, and the live desk on 127.0.0.1:8787.

#[allow(dead_code)]
#[path = "../src/messages_backend.rs"]
mod messages_backend;

// messages_backend.rs names `crate::roster` and `crate::routes`, so they are re-exported here.
pub use ds_web::{roster, routes};

use std::{collections::BTreeSet, sync::Arc, time::Duration};

use axum::{
    body::{to_bytes, Body},
    http::{header, Method, Request},
    Extension, Router,
};
use messages_backend::{
    ChannelConfig, MessagesBackend, MessagesBackendConfig, UnixDaemonConnector,
};
use routes::{
    auth::{AuthBackend, AuthError, AuthFuture, Identity, RequestScheme},
    messages_write,
};
use rusqlite::Connection;
use serde_json::{json, Value};
use tower::ServiceExt;

const OWNER: &str = "owner-token";
const API: &str = "api-token";
const ORACLE: &str = include_str!("fixtures/messages_int_oracle.json");

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

/// The counts the oracle records after each request, read straight from the database.
fn counts(connection: &Connection) -> Value {
    let one = |sql: &str| -> i64 { connection.query_row(sql, [], |row| row.get(0)).expect(sql) };
    json!({
        "messages": one("SELECT count(*) FROM messages"),
        "attachments": one("SELECT count(*) FROM attachments"),
        "answered": one("SELECT count(*) FROM messages WHERE answer_id IS NOT NULL"),
        "newest_id": one("SELECT coalesce(max(id), 0) FROM messages"),
    })
}

fn key_set(value: &Value) -> BTreeSet<String> {
    value
        .as_object()
        .expect("a JSON object")
        .keys()
        .cloned()
        .collect()
}

#[tokio::test]
async fn brief_matches_the_python_desk_on_every_recorded_case() {
    let oracle: Value = serde_json::from_str(ORACLE).expect("oracle fixture is JSON");
    let seed = &oracle["seed"];

    let directory = tempfile::tempdir().expect("temporary directory");
    let connector = Arc::new(UnixDaemonConnector::new(
        directory.path().join("unused.sock"),
        Some("service-token".to_owned()),
        Duration::from_secs(5),
        "brief integer parity",
    ));
    // The Python desk's channel registry (bridges/channels.example.json) for the channels used.
    let channels = vec![ChannelConfig {
        id: "cos".to_owned(),
        config: json!({"id": "cos", "name": "Chief of Staff", "match": "chief of staff", "kind": "active"}),
    }];
    let backend = MessagesBackend::open(MessagesBackendConfig {
        data_dir: directory.path().join("desk"),
        channels,
        daemon: connector,
    })
    .expect("open the production messages backend");

    // The same seed the oracle used: one owner question, which is the first row (id 1).
    let database = Connection::open(backend.database_path()).expect("open the database");
    database
        .execute(
            "INSERT INTO messages(channel, role, body) VALUES('cos', 'owner', 'question?')",
            [],
        )
        .expect("seed the owner question");
    assert_eq!(
        database.last_insert_rowid(),
        seed["owner_question_id"].as_i64().expect("seed id"),
        "the seed must land on the id the oracle used"
    );

    let auth: Arc<dyn AuthBackend> = Arc::new(TokenAuth);
    let (read, write, _search) = backend.trait_objects();
    let app = Router::new()
        .merge(messages_write::router(
            write,
            Arc::clone(&auth),
            RequestScheme::Http,
        ))
        .layer(Extension(auth))
        .layer(Extension(read));

    let mut failures = Vec::new();
    for case in oracle["cases"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("case name");
        if !name.starts_with("brief_") {
            continue; // send-now cases are recorded for a later slice
        }
        let token = match case["identity"].as_str().expect("identity") {
            "owner" => OWNER,
            _ => API,
        };
        let payload = case["payload"].clone();
        let before = counts(&database);
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/brief")
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload.to_string()))
                    .expect("request"),
            )
            .await
            .expect("the router answers every request");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body")
            .to_vec();
        let after = counts(&database);

        let want_status = case["status"].as_u64().expect("status") as u16;
        if status.as_u16() != want_status {
            failures.push(format!(
                "{name}: status {} != {want_status}",
                status.as_u16()
            ));
        }
        let want_body = &case["body"];
        if let Some(text) = want_body.get("text").and_then(Value::as_str) {
            let got = String::from_utf8_lossy(&bytes);
            if got != text {
                failures.push(format!("{name}: body {got:?} != {text:?}"));
            }
        } else if want_status == 200 {
            let got: Value = serde_json::from_slice(&bytes).expect("a JSON success body");
            if key_set(&got) != key_set(want_body) {
                failures.push(format!(
                    "{name}: success keys {:?} != {:?}",
                    key_set(&got),
                    key_set(want_body)
                ));
            }
        } else {
            let got: Value = serde_json::from_slice(&bytes).expect("a JSON error body");
            if got != *want_body {
                failures.push(format!("{name}: error body {got} != {want_body}"));
            }
        }
        if after != case["db_after"] {
            failures.push(format!("{name}: db after {after} != {}", case["db_after"]));
        }
        if before != case["db_before"] {
            failures.push(format!(
                "{name}: db before {before} != {}",
                case["db_before"]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "brief differs from the Python desk:\n{}",
        failures.join("\n")
    );
}
