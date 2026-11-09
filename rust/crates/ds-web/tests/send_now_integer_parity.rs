//! t1702 integer gate, slice 2: POST /api/messages/{id}/send-now against the Python desk.
//!
//! The oracle is `fixtures/messages_int_oracle.json` (captured on the served release venv, see
//! `fixtures/generate_messages_int_oracle.py`). This replays the three send-now cases against
//! the Rust `messages_write` router over the production `MessagesBackend`, and checks the status,
//! the body and the message/attachment/answer counts before and after each request. The 2**100
//! message id is Python's 500 (SQLite cannot bind it), and the Rust port must answer it the same
//! way, without narrowing the id to i64 or rejecting it at the path.
//!
//! NOT covered here: the brief cases (slice 1, brief_integer_parity.rs), the production daemon,
//! and the live desk on 127.0.0.1:8787.

#[allow(dead_code)]
#[path = "../src/messages_backend.rs"]
mod messages_backend;

// messages_backend.rs names `crate::roster` and `crate::routes`, so they are re-exported here.
pub use ds_web::{roster, routes};

use std::{sync::Arc, time::Duration};

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
use rusqlite::{types::Value as SqlValue, Connection};
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

/// The counts the oracle records, read straight from the database.
fn counts(connection: &Connection) -> Value {
    let one = |sql: &str| -> i64 { connection.query_row(sql, [], |row| row.get(0)).expect(sql) };
    json!({
        "messages": one("SELECT count(*) FROM messages"),
        "attachments": one("SELECT count(*) FROM attachments"),
        "answered": one("SELECT count(*) FROM messages WHERE answer_id IS NOT NULL"),
        "newest_id": one("SELECT coalesce(max(id), 0) FROM messages"),
    })
}

/// Capture every value, including blobs and the AUTOINCREMENT sequence, instead of only counts.
fn full_state(connection: &Connection) -> Vec<Vec<Vec<SqlValue>>> {
    [
        "SELECT * FROM messages ORDER BY rowid",
        "SELECT * FROM attachments ORDER BY rowid",
        "SELECT * FROM sqlite_sequence ORDER BY name",
    ]
    .iter()
    .map(|sql| {
        let mut statement = connection.prepare(sql).expect(sql);
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|index| row.get(index))
                    .collect::<rusqlite::Result<Vec<SqlValue>>>()
            })
            .expect(sql);
        rows.collect::<rusqlite::Result<Vec<_>>>().expect(sql)
    })
    .collect()
}

fn key_set(value: &Value) -> std::collections::BTreeSet<String> {
    value
        .as_object()
        .expect("a JSON object")
        .keys()
        .cloned()
        .collect()
}

#[tokio::test]
async fn send_now_matches_the_python_desk_on_every_recorded_case() {
    let oracle: Value = serde_json::from_str(ORACLE).expect("oracle fixture is JSON");
    let seed = &oracle["seed"];

    let directory = tempfile::tempdir().expect("temporary directory");
    let connector = Arc::new(UnixDaemonConnector::new(
        directory.path().join("unused.sock"),
        Some("service-token".to_owned()),
        Duration::from_secs(5),
        "send-now integer parity",
    ));
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
    let (mut brief_seen, mut send_now_seen, mut rollback_seen) = (0, 0, 0);
    // Every recorded case is replayed in order, because the database state each case sees
    // (its db_before) is the state the earlier brief cases left behind.
    for case in oracle["cases"].as_array().expect("cases") {
        let name = case["name"].as_str().expect("case name");
        let token = match case["identity"].as_str().expect("identity") {
            "owner" => OWNER,
            _ => API,
        };
        let (uri, body, json_body) = if name.starts_with("brief_") {
            brief_seen += 1;
            ("/api/brief".to_owned(), case["payload"].to_string(), true)
        } else {
            send_now_seen += 1;
            (
                case["url"].as_str().expect("url").to_owned(),
                String::new(),
                false,
            )
        };
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri(&uri)
            .header(header::AUTHORIZATION, format!("Bearer {token}"));
        if json_body {
            builder = builder.header(header::CONTENT_TYPE, "application/json");
        }
        let before = counts(&database);
        let check_rollback = matches!(
            name,
            "brief_answer_too_large" | "brief_answer_overflow_64bit" | "send_now_huge_id"
        );
        let full_before = check_rollback.then(|| full_state(&database));
        let response = app
            .clone()
            .oneshot(builder.body(Body::from(body)).expect("request"))
            .await
            .expect("the router answers every request");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body")
            .to_vec();
        let after = counts(&database);
        if let Some(full_before) = full_before {
            rollback_seen += 1;
            assert_eq!(
                full_state(&database),
                full_before,
                "{name}: complete rows or sqlite_sequence changed"
            );
        }

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
            // Successes carry ids and timestamps, so only the key set is compared.
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
        if before != case["db_before"] {
            failures.push(format!(
                "{name}: db before {before} != {}",
                case["db_before"]
            ));
        }
        if after != case["db_after"] {
            failures.push(format!("{name}: db after {after} != {}", case["db_after"]));
        }
    }
    assert_eq!(
        rollback_seen, 3,
        "all huge-integer failure states must be compared"
    );
    assert_eq!(brief_seen, 11, "every recorded brief case must be replayed");
    assert_eq!(
        send_now_seen, 3,
        "the three recorded send-now cases must all be replayed"
    );
    assert!(
        failures.is_empty(),
        "send-now differs from the Python desk:\n{}",
        failures.join("\n")
    );
}
