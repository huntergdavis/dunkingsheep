//! t3721 (t1702 leaf): viewport, password, ops-board answer and ops-metrics refresh parity.
//!
//! The oracle is `fixtures/t3721_parity_oracle.json`, recorded by
//! `fixtures/generate_t3721_parity_oracle.py` against the Python desk's FastAPI routes (the
//! served venv's interpreter, FastAPI 0.162.0 / Starlette 1.10.0 / Pydantic 2.14.2) on scratch
//! data and config only. This test drives the real `Desk` router (the runtime `serve` mounts) in
//! the same order, with the same passwords, and compares status, content type and body for every
//! case. Probes check the side effects: the stored viewport (through `/api/channels`), the session
//! sweep after a password change, the rewritten owner-credentials file, and the login results.
//!
//! Known divergences are NOT hidden. Each one is listed in `KNOWN_GAPS` with the Python behaviour,
//! and the exact Rust response is pinned (status and body). The test fails if a gap answers
//! differently from its pin, if a gap starts to match the oracle, or if the set of divergent cases
//! changes. A gap is closed only by a scoped follow-up that changes the Rust route.
//!
//! The replay inventory is checked too: every case in the oracle must be replayed here, and no
//! other name may be recorded, so a regenerated oracle with new cases fails until the test names them.
//!
//! Owner decisions recorded here:
//! - i50091 (accepted): the ops-board answer is the atomic versioned answer, with an optional reply
//!   and option picks, an intentional compatibility difference. Its three cases are listed in
//!   `INTENTIONAL_DIFFERENCES`, and the stale-answer guard (a missing or invalid `version` is refused
//!   before any backend call) is covered by a unit test in `routes/ops_board.rs`.
//! - i50225 (decided): submitted passwords must not appear in validation error bodies. The pinned
//!   Python 422 bodies echo the submitted value as `input`. This port shows `[redacted]` there and
//!   keeps status, content type, `type`, `loc`, `msg` and `ctx` as Python sends them. Each redacted
//!   case is listed in `INTENTIONAL_DIFFERENCES`, and a unit test (`password_validation_errors_never_echo_a_submitted_password`)
//!   checks that scratch sentinel passwords never come back. i50225 also allows the decoder wording
//!   difference (`password_not_json`) and the NaN 422 (`password_nan_body`) as intentional differences,
//!   provided status, content type and envelope match. No scanner port and no copy of Python's 500.
//! - t3724 R1 (fixed): FastAPI decodes a body as JSON only for `application/json` or `application/*+json`.
//!   Any other content type, or none, is refused as a model error with the body redacted. The
//!   `ct_*` cases replay that matrix; `ct_empty_text_plain` matches Python's `missing`.
//! - t3724 R2 (fixed): the missing-field error redacts the whole submitted object (any key), not only
//!   the two named keys. The sentinel unit test covers unknown keys.
//! - t3724 R3 (owner i50493): six malformed-input cases (invalid UTF-8, a 5,000-digit integer, a lone
//!   surrogate, +/-Infinity, 2,000-deep nesting) are recorded intentional differences with the owner's
//!   provenance, each pinning the exact Rust response. The UTF-8 BOM was functional and is FIXED: Rust
//!   drops the BOM before parsing, as Python does, and the correct-password BOM body changes the password
//!   on scratch state (`password_bom_correct_password`, then the two login probes).
//! - t3725 / owner i50587 (FIXED): UTF-16 and UTF-32 bodies, with or without a BOM, decode as Python's
//!   `json.loads` does (`detect_encoding`, then `surrogatepass`). They parse (`enc_*` cases) and a
//!   correct-password chain of changes across the encodings runs last on scratch state (`chain_*`). Odd or
//!   truncated units and out-of-range code points are 400 "There was an error parsing the body", as in Python.
//!   Decoder-wording (`enc_utf16le_bom_only`) and redaction (`enc_text_plain_*`) differences are listed under
//!   owner i50225. The two lone-surrogate and CESU cases (Python 500, a defect) are intentional differences
//!   under owner decision i50662, each pinning Rust's clean 422 response. `PENDING_DISPOSITION` is empty.
//!
//! NOT covered here: the ops-board answer success path (it needs a live or scratch daemon with an
//! owner-issued token), the ops-metrics refresh owner path (it calls `gh` and reads host state),
//! the live desk on 127.0.0.1:8787, and any live owner database, token or config.

use std::{net::SocketAddr, path::Path, time::Duration};

use axum::{
    body::{to_bytes, Body},
    extract::ConnectInfo,
    http::{header, Method, Request},
    Router,
};
use ds_web::{
    routes::auth::RequestScheme,
    web_server::{Desk, DeskConfig},
};
use rusqlite::Connection;
use serde_json::{json, Value};
use tower::ServiceExt;

const PASSWORD: &str = "t3721-initial-scratch";
const NEW_PASSWORD: &str = "t3721-changed-scratch";
const ORACLE: &str = include_str!("fixtures/t3721_parity_oracle.json");

/// A divergence from the pinned Python oracle. The Rust response is pinned exactly, so a gap cannot
/// drift to a different wrong answer unnoticed. Each entry is an owner-scoped follow-up, not an
/// accepted difference.
struct Gap {
    name: &'static str,
    reason: &'static str,
    rust_status: u16,
    rust_body: &'static str,
}

/// Open gaps: none. Every difference from the pinned Python oracle is now an owner-decided
/// intentional difference (see `INTENTIONAL_DIFFERENCES`). The list stays so a new gap has a place.
const KNOWN_GAPS: &[Gap] = &[];

/// Intentional compatibility differences, accepted by the owner (decision i50091, relayed by the
/// coordinator in i50094): the Rust ops-board answer is the atomic versioned answer, with an
/// optional reply and option picks. It keeps the stale-answer guard: a request without a positive
/// `version` is refused before any backend call (see `owner_answer_refuses_a_missing_or_invalid_version`
/// in `routes/ops_board.rs`). Each entry pins the exact Rust response, so the accepted difference
/// cannot silently change.
const INTENTIONAL_DIFFERENCES: &[Gap] = &[
    Gap {
        name: "answer_missing_reply",
        reason: "owner decision i50091: the versioned answer makes the reply optional (option picks). This case has no version, so Rust refuses on the version before the reply is considered; Python refuses 'a reply is required'.",
        rust_status: 400,
        rust_body: r#"{"detail":"question version is required"}"#,
    },
    Gap {
        name: "answer_blank_reply",
        reason: "owner decision i50091: a blank reply is not refused by the versioned answer. This case has no version, so Rust refuses on the version first; with a version the daemon decides (not verified here). Python refuses 'a reply is required'.",
        rust_status: 400,
        rust_body: r#"{"detail":"question version is required"}"#,
    },
    Gap {
        name: "answer_no_daemon",
        reason: "owner decision i50091: the versioned answer reports a failure as 'Could not answer <id>: <kind>', not Python's 'Could not unblock <id>: <exception>'. This case has no version, so Rust refuses before the daemon. The Python exception in the oracle is the isolated client's RuntimeError; the production one depends on the host.",
        rust_status: 400,
        rust_body: r#"{"detail":"question version is required"}"#,
    },
    Gap {
        name: "password_not_json",
        reason: "owner decision i50225: the decoder wording and position may differ while status, content type and envelope match. Python's json_invalid ctx.error and loc come from the Python json decoder (observed in the t3722 commit and PR body); Rust's come from serde_json. This is not a scanner port.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"error":"expected ident at line 1 column 2"},"input":{},"loc":["body",0],"msg":"JSON decode error","type":"json_invalid"}]}"#,
    },
    Gap {
        name: "password_nan_body",
        reason: "owner decision i50225: Rust answers 422 json_invalid for NaN. Python answers 500 Internal Server Error (json.loads accepts NaN and the 422 cannot serialise it). A 500 is a Python defect; it is not copied.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"error":"expected value at line 1 column 22"},"input":{},"loc":["body",0],"msg":"JSON decode error","type":"json_invalid"}]}"#,
    },
    Gap {
        name: "password_short_new",
        reason: "owner decision i50225: submitted passwords are redacted from validation errors. Python echoes the submitted value as `input`; Rust shows '[redacted]'. status, content type, type, loc, msg and ctx match.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"min_length":12},"input":"[redacted]","loc":["body","new_password"],"msg":"String should have at least 12 characters","type":"string_too_short"}]}"#,
    },
    Gap {
        name: "password_wrong_current_short_new",
        reason: "owner decision i50225: submitted passwords are redacted from validation errors (`input` shows '[redacted]'). Everything else matches the pinned Python 422.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"min_length":12},"input":"[redacted]","loc":["body","new_password"],"msg":"String should have at least 12 characters","type":"string_too_short"}]}"#,
    },
    Gap {
        name: "password_missing_current",
        reason: "owner decision i50225 and t3724 R2: the missing-field error redacts the whole submitted object in `input` (Python echoes it, so a password under any key would show). Everything else matches the pinned Python 422.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"input":"[redacted]","loc":["body","current_password"],"msg":"Field required","type":"missing"}]}"#,
    },
    Gap {
        name: "ct_text_plain",
        reason: "t3724 R1 with owner decision i50225: a body that is not declared JSON is never parsed (Python 422 model_attributes_type); its `input` echoes the body, which Rust redacts. type, loc and msg match.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"input":"[redacted]","loc":["body"],"msg":"Input should be a valid dictionary or object to extract fields from","type":"model_attributes_type"}]}"#,
    },
    Gap {
        name: "ct_no_content_type",
        reason: "t3724 R1 with owner decision i50225: no content type means the body is not parsed (Python 422 model_attributes_type); `input` is redacted in Rust. type, loc and msg match.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"input":"[redacted]","loc":["body"],"msg":"Input should be a valid dictionary or object to extract fields from","type":"model_attributes_type"}]}"#,
    },
    Gap {
        name: "ct_form_urlencoded",
        reason: "t3724 R1 with owner decision i50225: a form content type is not parsed as JSON (Python 422 model_attributes_type); `input` is redacted in Rust. type, loc and msg match.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"input":"[redacted]","loc":["body"],"msg":"Input should be a valid dictionary or object to extract fields from","type":"model_attributes_type"}]}"#,
    },
    Gap {
        name: "ct_multipart",
        reason: "t3724 R1 with owner decision i50225: a multipart content type is not parsed as JSON (Python 422 model_attributes_type); `input` is redacted in Rust. type, loc and msg match.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"input":"[redacted]","loc":["body"],"msg":"Input should be a valid dictionary or object to extract fields from","type":"model_attributes_type"}]}"#,
    },
    Gap {
        name: "ct_text_json",
        reason: "t3724 R1 with owner decision i50225: `text/json` is not an application type, so the body is not parsed (Python 422 model_attributes_type); `input` is redacted in Rust. type, loc and msg match.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"input":"[redacted]","loc":["body"],"msg":"Input should be a valid dictionary or object to extract fields from","type":"model_attributes_type"}]}"#,
    },
    Gap {
        name: "password_current_not_string",
        reason: "owner decision i50225: the non-string password value is redacted from `input`. Everything else matches the pinned Python 422.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"input":"[redacted]","loc":["body","current_password"],"msg":"Input should be a valid string","type":"string_type"}]}"#,
    },
    Gap {
        name: "password_new_too_long",
        reason: "owner decision i50225: the over-long submitted password is redacted from `input`. Everything else matches the pinned Python 422.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"max_length":200},"input":"[redacted]","loc":["body","new_password"],"msg":"String should have at most 200 characters","type":"string_too_long"}]}"#,
    },
    Gap {
        name: "password_api_identity_invalid_body",
        reason: "owner decision i50225: the submitted password is redacted from `input` on the 422 that precedes the owner check. Everything else matches the pinned Python 422.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"min_length":12},"input":"[redacted]","loc":["body","new_password"],"msg":"String should have at least 12 characters","type":"string_too_short"}]}"#,
    },
    Gap {
        name: "password_list_body",
        reason: "owner decision i50225: a non-object body is redacted whole in `input`, because a string inside it could be a password. Python echoes the list. type, loc and msg match.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"input":"[redacted]","loc":["body"],"msg":"Input should be a valid dictionary or object to extract fields from","type":"model_attributes_type"}]}"#,
    },
    Gap {
        name: "rv_invalid_utf8",
        reason: "owner decision i50493 (t3724 R3, accepted as a recorded intentional difference): a body that is not valid UTF-8 is refused by Rust as json_invalid (422). Python answers 400 'There was an error parsing the body'. status, content type and envelope of the decoder case are not claimed to match.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"error":"invalid unicode code point at line 1 column 23"},"input":{},"loc":["body",0],"msg":"JSON decode error","type":"json_invalid"}]}"#,
    },
    Gap {
        name: "rv_huge_integer",
        reason: "owner decision i50493 (t3724 R3, accepted as a recorded intentional difference): a 5,000-digit integer is parsed by Rust and refused as string_type with the value redacted (422). Python answers 400 'There was an error parsing the body' (its digit limit).",
        rust_status: 422,
        rust_body: r#"{"detail":[{"input":"[redacted]","loc":["body","current_password"],"msg":"Input should be a valid string","type":"string_type"}]}"#,
    },
    Gap {
        name: "rv_lone_surrogate",
        reason: "owner decision i50493 (t3724 R3, accepted as a recorded intentional difference): a lone surrogate escape is refused by Rust as json_invalid (422). Python answers 500 Internal Server Error; a 500 is not copied.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"error":"unexpected end of hex escape at line 1 column 29"},"input":{},"loc":["body",0],"msg":"JSON decode error","type":"json_invalid"}]}"#,
    },
    Gap {
        name: "rv_infinity",
        reason: "owner decision i50493 (t3724 R3, accepted as a recorded intentional difference): Rust refuses Infinity as json_invalid (422), as it does NaN. Python answers 500 Internal Server Error; a 500 is not copied.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"error":"expected value at line 1 column 22"},"input":{},"loc":["body",0],"msg":"JSON decode error","type":"json_invalid"}]}"#,
    },
    Gap {
        name: "rv_neg_infinity",
        reason: "owner decision i50493 (t3724 R3, accepted as a recorded intentional difference): Rust refuses -Infinity as json_invalid (422). Python answers 500 Internal Server Error; a 500 is not copied.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"error":"invalid number at line 1 column 23"},"input":{},"loc":["body",0],"msg":"JSON decode error","type":"json_invalid"}]}"#,
    },
    Gap {
        name: "rv_deep_nesting",
        reason: "owner decision i50493 (t3724 R3, accepted as a recorded intentional difference): 2,000 levels of nesting are refused by Rust as json_invalid (422, serde's recursion limit). Python answers 500 Internal Server Error; a 500 is not copied.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"error":"recursion limit exceeded at line 1 column 636"},"input":{},"loc":["body",0],"msg":"JSON decode error","type":"json_invalid"}]}"#,
    },
    Gap {
        name: "enc_utf16le_bom_only",
        reason: "owner decision i50225 (decoder wording, status/content type/envelope match): a body that decodes to nothing (a UTF-16 BOM alone) is json_invalid 422 in both; Python's wording is 'Expecting value', serde's is 'EOF while parsing a value'.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"error":"EOF while parsing a value at line 1 column 0"},"input":{},"loc":["body",0],"msg":"JSON decode error","type":"json_invalid"}]}"#,
    },
    Gap {
        name: "enc_text_plain_utf16le",
        reason: "owner decision i50225 (redaction): a UTF-16 body sent as text/plain is refused as model_attributes_type (never parsed, the content type decides first); Python's input echoes the raw body, Rust shows '[redacted]'. type, loc and msg match.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"input":"[redacted]","loc":["body"],"msg":"Input should be a valid dictionary or object to extract fields from","type":"model_attributes_type"}]}"#,
    },
    Gap {
        name: "enc_text_plain_odd_utf16",
        reason: "owner decision i50225 (redaction): an odd-length UTF-16 body sent as text/plain is refused as model_attributes_type before any decoding (content type first, as Python); Python's input echoes the raw body, Rust shows '[redacted]'.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"input":"[redacted]","loc":["body"],"msg":"Input should be a valid dictionary or object to extract fields from","type":"model_attributes_type"}]}"#,
    },
    Gap {
        name: "enc_utf16le_lone_surrogate",
        reason: "owner decision i50662 (intentional difference): a UTF-16 body with a lone surrogate code unit. Python answers 500 Internal Server Error (its password comparison cannot encode the surrogate; the 500 is a defect). Rust answers a clean 422 application/json json_invalid (the surrogate is refused at decode). The 500 is not copied.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"error":"lone surrogate in the body"},"input":{},"loc":["body",0],"msg":"JSON decode error","type":"json_invalid"}]}"#,
    },
    Gap {
        name: "enc_cesu_surrogate_utf8",
        reason: "owner decision i50662 (intentional difference): CESU-style surrogate bytes (ED A0 80) in a UTF-8 body. Python answers 500 Internal Server Error (surrogatepass decode, then the same comparison failure; a defect). Rust answers a clean 422 application/json json_invalid (invalid UTF-8, the accepted invalid-UTF-8 family). The 500 is not copied.",
        rust_status: 422,
        rust_body: r#"{"detail":[{"ctx":{"error":"invalid unicode code point at line 1 column 23"},"input":{},"loc":["body",0],"msg":"JSON decode error","type":"json_invalid"}]}"#,
    },
];

/// Measured differences with NO owner decision (t3724 R3). Visible, not accepted, not waived. Each pins
/// the exact Rust response measured on the head, so a change in either direction fails the test. The
/// coordinator routes them to policy; none is implemented here (no scanner, no copy of Python's 500).
const PENDING_DISPOSITION: &[Gap] = &[];

fn open_operations_db(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(path)?;
    conn.execute_batch(include_str!("../src/ops_schema.sql"))?;
    Ok(conn)
}

fn channels() -> Vec<Value> {
    vec![json!({"id": "cos", "name": "Chief of staff", "match": "cos", "kind": "active"})]
}

/// A temporary desk: every path under `dir`, with the bundle the static mount expects. The daemon
/// socket is absent, as in the Python oracle (scratch config, no daemon).
fn config(dir: &Path) -> DeskConfig {
    std::fs::create_dir_all(dir.join("ui/dist")).unwrap();
    std::fs::write(
        dir.join("ui/dist/index.html"),
        "<!doctype html><title>desk</title>",
    )
    .unwrap();
    for sub in ["config", "home"] {
        std::fs::create_dir_all(dir.join(sub)).unwrap();
    }
    DeskConfig {
        data_dir: dir.join("desk"),
        config_dir: dir.join("config"),
        home: dir.join("home"),
        daemon_socket: dir.join("absent-daemon.sock"),
        service_token: None,
        client_timeout: Duration::from_secs(2),
        channels: channels(),
        ui_dist: dir.join("ui/dist"),
        dashboard_dir: dir.join("dashboard"),
        dashboard_repo: dir.join("poolrad"),
        poolrad_repo: dir.join("poolrad"),
        operations_db: dir.join("desk/operations.sqlite3"),
        host_watch_state: dir.join("desk/host-watch-state.json"),
        opencode_db: dir.join("home/opencode.db"),
        dunk_events_db: dir.join("config/metrics.db"),
        usage_config: dir.join("config/usage.json"),
        usage_refresh_s: 1800,
        usage_backoff_s: vec![300, 600, 1800],
        usage_max_age_s: 3600,
        board_interval_s: 3600,
        gh_program: dir.join("missing-gh"),
        tmux_program: dir.join("missing-tmux"),
        proc_root: dir.join("proc"),
        herdr_webui_url: None,
        herdr_webui_label: Some("Herdr web UI".to_owned()),
        initial_password: Some(PASSWORD.to_owned()),
        scheme: RequestScheme::Http,
        open_operations_db: open_operations_db as fn(&Path) -> rusqlite::Result<Connection>,
    }
}

/// A request to the desk, with the Host header the Python TestClient sends (`testserver`), so the
/// same-origin check compares the same origin.
fn request(method: Method, uri: &str) -> axum::http::request::Builder {
    Request::builder()
        .method(method)
        .uri(uri)
        .header(header::HOST, "testserver")
        .extension(ConnectInfo(
            "127.0.0.1:4000".parse::<SocketAddr>().expect("address"),
        ))
}

struct Reply {
    status: u16,
    content_type: Option<String>,
    body: Value,
    cookie: Option<String>,
}

async fn send(router: &Router, builder: axum::http::request::Builder, body: Body) -> Reply {
    let response = router
        .clone()
        .oneshot(builder.body(body).expect("request"))
        .await
        .expect("the router answers");
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let cookie = response
        .headers()
        .get_all(header::SET_COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .find(|value| value.starts_with("vp_session="))
        .and_then(|value| value.split(';').next())
        .map(str::to_owned);
    let bytes = to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let body = serde_json::from_slice::<Value>(&bytes)
        .unwrap_or_else(|_| json!({"text": String::from_utf8_lossy(&bytes)}));
    Reply {
        status,
        content_type,
        body,
        cookie,
    }
}

fn json_body(value: &Value) -> Body {
    Body::from(value.to_string())
}

async fn login(router: &Router, password: &str) -> Reply {
    let builder =
        request(Method::POST, "/api/login").header(header::CONTENT_TYPE, "application/json");
    send(
        router,
        builder,
        json_body(&json!({"password": password, "next": "/dash/cos"})),
    )
    .await
}

fn case<'a>(oracle: &'a Value, name: &str) -> &'a Value {
    oracle["cases"]
        .as_array()
        .expect("cases")
        .iter()
        .find(|case| case["name"] == name)
        .unwrap_or_else(|| panic!("oracle case {name}"))
}

/// Compares one HTTP case with the oracle. Returns the failure text when they differ. A case the
/// oracle recorded without a body (a status-only probe) is compared on status alone.
fn compare(name: &str, reply: &Reply, want: &Value) -> Option<String> {
    let want_status = want["status"].as_u64().expect("status") as u16;
    if want.get("body").is_none() {
        return (reply.status != want_status).then(|| {
            format!(
                "{name}: status {} but the Python oracle is {want_status}",
                reply.status
            )
        });
    }
    let want_type = want["content_type"].as_str().map(str::to_owned);
    if reply.status != want_status || reply.content_type != want_type || reply.body != want["body"]
    {
        Some(format!(
            "{name}: got {} {:?} {} but the Python oracle is {} {:?} {}",
            reply.status, reply.content_type, reply.body, want_status, want_type, want["body"]
        ))
    } else {
        None
    }
}

/// `Some(text)` when the Rust response differs from the pinned one (status or body).
fn pinned_mismatch(name: &str, reply: &Reply, status: u16, body: &str) -> Option<String> {
    let pinned: Value = serde_json::from_str(body).expect("pinned body is JSON");
    (reply.status != status || reply.body != pinned).then(|| {
        format!(
            "{name}: Rust answered {} {} but the pinned response is {status} {pinned}",
            reply.status, reply.body
        )
    })
}

fn viewport_payload() -> Value {
    json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})
}

#[test]
fn the_composed_desk_matches_the_python_desk_for_viewport_password_answer_and_refresh() {
    let oracle: Value = serde_json::from_str(ORACLE).expect("oracle fixture is JSON");
    let root = tempfile::TempDir::new().expect("temporary desk root");
    let data_dir = root.path().join("desk");

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    runtime.block_on(async {
        let desk = Desk::open(config(root.path())).unwrap_or_else(|error| panic!("the desk opens: {error}"));
        let router = desk.router();
        let mut failures = Vec::new();
        // Ad-hoc probe failures, merged into `failures` after the last `check` call.
        let mut extra: Vec<String> = Vec::new();
        let mut diverged: Vec<String> = Vec::new();
        // Every replayed case is recorded here; the inventory is checked against the oracle at the end.
        let mut replayed: Vec<String> = Vec::new();
        // Probe names recorded outside `check` (their side effects are checked by hand below).
        let mut probes: Vec<String> = Vec::new();
        // Owner-accepted differences, recorded separately from the open gaps.
        let mut accepted_diverged: Vec<String> = Vec::new();
        // Measured differences with no owner decision (t3724 R3): visible, pinned, not accepted.
        let mut pending_diverged: Vec<String> = Vec::new();
        let mut check = |name: &str, reply: &Reply| {
            replayed.push(name.to_owned());
            let differs = compare(name, reply, case(&oracle, name));
            if let Some(gap) = KNOWN_GAPS.iter().find(|gap| gap.name == name) {
                if differs.is_none() {
                    failures.push(format!("{name}: now matches the Python oracle; remove it from KNOWN_GAPS"));
                }
                if let Some(text) = pinned_mismatch(name, reply, gap.rust_status, gap.rust_body) {
                    failures.push(format!("{text} ({})", gap.reason));
                }
                diverged.push(name.to_owned());
            } else if let Some(accepted) = INTENTIONAL_DIFFERENCES.iter().find(|item| item.name == name) {
                if differs.is_none() {
                    failures.push(format!("{name}: now matches the Python oracle; remove it from INTENTIONAL_DIFFERENCES"));
                }
                if let Some(text) = pinned_mismatch(name, reply, accepted.rust_status, accepted.rust_body) {
                    failures.push(format!("{text} ({})", accepted.reason));
                }
                accepted_diverged.push(name.to_owned());
            } else if let Some(pending) = PENDING_DISPOSITION.iter().find(|item| item.name == name) {
                if differs.is_none() {
                    failures.push(format!("{name}: now matches the Python oracle; remove it from PENDING_DISPOSITION"));
                }
                if let Some(text) = pinned_mismatch(name, reply, pending.rust_status, pending.rust_body) {
                    failures.push(format!("{text} ({})", pending.reason));
                }
                pending_diverged.push(name.to_owned());
            } else if let Some(text) = differs {
                failures.push(text);
            }
        };

        // Owner session for the viewport cases.
        let owner_login = login(&router, PASSWORD).await;
        check("login_owner", &owner_login);
        // The login reply's Set-Cookie value already reads `vp_session=<token>`.
        let owner_cookie_header = owner_login.cookie.clone().expect("login sets vp_session");

        // Viewport cases, in the oracle's order.
        let viewport = |name: &str, body: Body, extra: &[(&str, &str)], cookie: Option<&str>| {
            let mut builder = request(Method::POST, "/api/viewport").header(header::CONTENT_TYPE, "application/json");
            for (key, value) in extra {
                builder = builder.header(*key, *value);
            }
            if let Some(cookie) = cookie {
                builder = builder.header(header::COOKIE, cookie);
            }
            let _ = name;
            (builder, body)
        };
        let run = |_name: &str, (builder, body): (axum::http::request::Builder, Body)| {
            let router = router.clone();
            async move { send(&router, builder, body).await }
        };
        let base = viewport_payload();

        let reply = run("viewport_ok", viewport("viewport_ok", json_body(&base), &[], Some(&owner_cookie_header))).await;
        check("viewport_ok", &reply);

        // Side-effect probe: the stored viewport is read through /api/channels.
        let probe = send(
            &router,
            request(Method::GET, "/api/channels").header(header::COOKIE, owner_cookie_header.as_str()),
            Body::empty(),
        )
        .await;
        probes.push("viewport_ok_probe_channels".to_owned());
        let want_probe = case(&oracle, "viewport_ok_probe_channels");
        let rows = probe.body["viewports"].as_array().cloned().unwrap_or_default();
        let stored: Vec<Value> = rows
            .iter()
            .map(|row| {
                let mut row = row.clone();
                row.as_object_mut().map(|object| object.remove("viewport_at"));
                row
            })
            .collect();
        let presence: Vec<bool> = rows.iter().map(|row| row.get("viewport_at").is_some()).collect();
        if probe.status != want_probe["status"].as_u64().expect("status") as u16
            || stored != want_probe["stored"].as_array().cloned().unwrap_or_default()
            || presence != want_probe["viewport_at_present"].as_array().unwrap().iter().map(|v| v.as_bool().unwrap()).collect::<Vec<_>>()
        {
            extra.push(format!("viewport_ok_probe_channels: stored {stored:?} presence {presence:?} status {}", probe.status));
        }

        let reply = run("viewport_same_origin", viewport("", json_body(&base), &[("origin", "http://testserver")], Some(&owner_cookie_header))).await;
        check("viewport_same_origin", &reply);
        let reply = run("viewport_cross_origin", viewport("", json_body(&base), &[("origin", "https://evil.example")], Some(&owner_cookie_header))).await;
        check("viewport_cross_origin", &reply);
        let reply = run("viewport_no_cookie", viewport("", json_body(&base), &[], None)).await;
        check("viewport_no_cookie", &reply);
        let reply = run("viewport_bogus_cookie", viewport("", json_body(&base), &[], Some("vp_session=bogus-session"))).await;
        check("viewport_bogus_cookie", &reply);

        let variants: Vec<(&str, Value)> = vec![
            ("viewport_extra_key", json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0, "extra": 1})),
            ("viewport_missing_key", json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "devicePixelRatio": 2.0})),
            ("viewport_string_int", json!({"innerWidth": "1280", "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})),
            ("viewport_bool_int", json!({"innerWidth": true, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})),
            ("viewport_float_int", json!({"innerWidth": 1280.5, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})),
            ("viewport_zero_width", json!({"innerWidth": 0, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})),
            ("viewport_width_over_max", json!({"innerWidth": 32769, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2.0})),
            ("viewport_dpr_zero", json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 0})),
            ("viewport_dpr_over_max", json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 17})),
            ("viewport_dpr_int", json!({"innerWidth": 1280, "innerHeight": 800, "screenWidth": 1920, "screenHeight": 1080, "devicePixelRatio": 2})),
        ];
        for (name, payload) in &variants {
            let reply = run(name, viewport(name, json_body(payload), &[], Some(&owner_cookie_header))).await;
            check(name, &reply);
        }
        let nan = Body::from(
            r#"{"innerWidth":1,"innerHeight":1,"screenWidth":1,"screenHeight":1,"devicePixelRatio":NaN}"#,
        );
        let reply = run("viewport_nan_dpr", viewport("", nan, &[], Some(&owner_cookie_header))).await;
        check("viewport_nan_dpr", &reply);
        let reply = run("viewport_not_json", viewport("", Body::from("not json"), &[], Some(&owner_cookie_header))).await;
        check("viewport_not_json", &reply);

        // Password cases.
        let change = |body: Value, cookie: Option<&str>, bearer: Option<&str>| {
            let mut builder = request(Method::POST, "/api/password").header(header::CONTENT_TYPE, "application/json");
            if let Some(cookie) = cookie {
                builder = builder.header(header::COOKIE, cookie);
            }
            if let Some(token) = bearer {
                builder = builder.header(header::AUTHORIZATION, format!("Bearer {token}"));
            }
            (builder, json_body(&body))
        };
        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": NEW_PASSWORD}), None, None);
        check("password_no_cookie", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": "not-the-password", "new_password": NEW_PASSWORD}), Some(&owner_cookie_header), None);
        check("password_wrong_current", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": "short"}), Some(&owner_cookie_header), None);
        check("password_short_new", &send(&router, b, body).await);
        let (b, body) = change(json!({"new_password": NEW_PASSWORD}), Some(&owner_cookie_header), None);
        check("password_missing_current", &send(&router, b, body).await);
        // Ordering and shape cases (t3722): none changes state.
        let (b, body) = change(json!({"new_password": "short"}), None, None);
        check("password_no_cookie_invalid_body", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": "not-the-password", "new_password": "short"}), Some(&owner_cookie_header), None);
        check("password_wrong_current_short_new", &send(&router, b, body).await);
        let (b, body) = change(json!({}), Some(&owner_cookie_header), None);
        check("password_missing_both", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": 12345678901234u64, "new_password": NEW_PASSWORD}), Some(&owner_cookie_header), None);
        check("password_current_not_string", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": "x".repeat(201)}), Some(&owner_cookie_header), None);
        check("password_new_too_long", &send(&router, b, body).await);
        let (b, _) = change(json!({}), Some(&owner_cookie_header), None);
        check("password_not_json", &send(&router, b, Body::from("not json")).await);
        let (b, body) = change(json!([1, 2]), Some(&owner_cookie_header), None);
        check("password_list_body", &send(&router, b, body).await);
        let (b, _) = change(json!({}), Some(&owner_cookie_header), None);
        check("password_empty_body", &send(&router, b, Body::empty()).await);
        let (b, _) = change(json!({}), Some(&owner_cookie_header), None);
        check(
            "password_nan_body",
            &send(&router, b, Body::from(r#"{"current_password": NaN, "new_password": "x"}"#)).await,
        );

        // Content-type and malformed-input matrix (t3722 R1 and R3). Each body carries a wrong
        // current password, so none can change state even where it is accepted.
        let raw_password = |content_type: Option<&str>, body: Vec<u8>| {
            let mut builder = request(Method::POST, "/api/password").header(header::COOKIE, owner_cookie_header.as_str());
            if let Some(content_type) = content_type {
                builder = builder.header(header::CONTENT_TYPE, content_type);
            }
            (builder, Body::from(body))
        };
        let wrong = json!({"current_password": "not-the-password", "new_password": NEW_PASSWORD}).to_string().into_bytes();
        let json_ct = Some("application/json");
        let (b, body) = raw_password(Some("text/plain"), wrong.clone());
        check("ct_text_plain", &send(&router, b, body).await);
        let (b, body) = raw_password(None, wrong.clone());
        check("ct_no_content_type", &send(&router, b, body).await);
        let (b, body) = raw_password(Some("application/x-www-form-urlencoded"), wrong.clone());
        check("ct_form_urlencoded", &send(&router, b, body).await);
        let (b, body) = raw_password(Some("multipart/form-data; boundary=x"), wrong.clone());
        check("ct_multipart", &send(&router, b, body).await);
        let (b, body) = raw_password(Some("application/json; charset=utf-8"), wrong.clone());
        check("ct_json_charset", &send(&router, b, body).await);
        let (b, body) = raw_password(Some("application/vnd.api+json"), wrong.clone());
        check("ct_vnd_json", &send(&router, b, body).await);
        let (b, body) = raw_password(Some("text/json"), wrong.clone());
        check("ct_text_json", &send(&router, b, body).await);
        let (b, body) = raw_password(Some("text/plain"), Vec::new());
        check("ct_empty_text_plain", &send(&router, b, body).await);

        let mut invalid_utf8 = b"{\"current_password\": \"".to_vec();
        invalid_utf8.push(0xff);
        invalid_utf8.extend_from_slice(format!("\", \"new_password\": \"{NEW_PASSWORD}\"}}").as_bytes());
        let huge_integer = format!("{{\"current_password\": {}, \"new_password\": \"{NEW_PASSWORD}\"}}", "1".repeat(5000));
        let mut bom = vec![0xef, 0xbb, 0xbf];
        bom.extend_from_slice(&wrong);
        let surrogate = format!("{{\"current_password\": \"\\ud800\", \"new_password\": \"{NEW_PASSWORD}\"}}");
        let infinity = format!("{{\"current_password\": Infinity, \"new_password\": \"{NEW_PASSWORD}\"}}");
        let neg_infinity = format!("{{\"current_password\": -Infinity, \"new_password\": \"{NEW_PASSWORD}\"}}");
        let deep = format!("{}1{}", "{\"a\":".repeat(2000), "}".repeat(2000));
        for (name, body) in [
            ("rv_invalid_utf8", invalid_utf8),
            ("rv_huge_integer", huge_integer.into_bytes()),
            ("rv_bom", bom),
            ("rv_lone_surrogate", surrogate.into_bytes()),
            ("rv_infinity", infinity.into_bytes()),
            ("rv_neg_infinity", neg_infinity.into_bytes()),
            ("rv_deep_nesting", deep.into_bytes()),
        ] {
            let (b, body) = raw_password(json_ct, body);
            check(name, &send(&router, b, body).await);
        }

        // UTF-16 and UTF-32 bodies (t3725 / owner i50587): a wrong current password throughout (no state change).
        let plain = r#"{"current_password": "not-the-password", "new_password": "t3721-changed-scratch"}"#;
        let u16le = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect() };
        let u16be = |s: &str| -> Vec<u8> { s.encode_utf16().flat_map(|u| u.to_be_bytes()).collect() };
        let u32le = |s: &str| -> Vec<u8> { s.chars().flat_map(|c| (c as u32).to_le_bytes()).collect() };
        let u32be = |s: &str| -> Vec<u8> { s.chars().flat_map(|c| (c as u32).to_be_bytes()).collect() };
        let emoji = r#"{"current_password": "not-the-password", "new_password": "t3721-changed-scratch😀"}"#;
        let mut lone_units: Vec<u16> = r#"{"current_password": ""#.encode_utf16().collect();
        lone_units.push(0xD800);
        lone_units.extend(r#"", "new_password": "t3721-changed-scratch"}"#.encode_utf16());
        let lone: Vec<u8> = lone_units.iter().flat_map(|u| u.to_le_bytes()).collect();
        let bom16le: [u8; 2] = [0xff, 0xfe];
        let bom16be: [u8; 2] = [0xfe, 0xff];
        let bom32le: [u8; 4] = [0xff, 0xfe, 0x00, 0x00];
        let bom32be: [u8; 4] = [0x00, 0x00, 0xfe, 0xff];
        let json_ct_name = "application/json";
        let enc_cases: Vec<(&str, Vec<u8>, &str)> = vec![
            ("enc_utf16le_bom", [&bom16le[..], &u16le(plain)].concat(), json_ct_name),
            ("enc_utf16be_bom", [&bom16be[..], &u16be(plain)].concat(), json_ct_name),
            ("enc_utf16le_nobom", u16le(plain), json_ct_name),
            ("enc_utf16be_nobom", u16be(plain), json_ct_name),
            ("enc_utf32le_bom", [&bom32le[..], &u32le(plain)].concat(), json_ct_name),
            ("enc_utf32be_bom", [&bom32be[..], &u32be(plain)].concat(), json_ct_name),
            ("enc_utf32le_nobom", u32le(plain), json_ct_name),
            ("enc_utf32be_nobom", u32be(plain), json_ct_name),
            ("enc_utf16le_odd_length", [u16le(plain), vec![0x00]].concat(), json_ct_name),
            ("enc_utf32le_truncated", [u32le(plain), vec![0x00]].concat(), json_ct_name),
            ("enc_utf32le_bom_over_utf16le", [&bom32le[..], &u16le(plain)].concat(), json_ct_name),
            (
                "enc_utf32be_bom_codepoint_out_of_range",
                [&bom32be[..], &[0x00, 0x11, 0x00, 0x00][..], &u32be(plain)].concat(),
                json_ct_name,
            ),
            ("enc_utf16le_bom_only", bom16le.to_vec(), json_ct_name),
            ("enc_utf16le_valid_surrogate_pair", u16le(emoji), json_ct_name),
            ("enc_text_plain_utf16le", u16le(plain), "text/plain"),
            ("enc_text_plain_odd_utf16", [u16le(plain), vec![0x00]].concat(), "text/plain"),
            ("enc_utf16le_lone_surrogate", lone, json_ct_name),
            (
                "enc_cesu_surrogate_utf8",
                b"{\"current_password\": \"\xed\xa0\x80\", \"new_password\": \"t3721-changed-scratch\"}".to_vec(),
                json_ct_name,
            ),
        ];
        for (name, body, content_type) in enc_cases {
            let (b, body) = raw_password(Some(content_type), body);
            check(name, &send(&router, b, body).await);
        }

        // Pinned Python HTTP accepts unpaired surrogates in ignored values and keys.
        for (kind, prefix, suffix) in [
            ("value", r#"{"current_password":"not-the-password","new_password":"t3721-changed-scratch","ignored":""#, r#""}"#),
            ("key", r#"{"current_password":"not-the-password","new_password":"t3721-changed-scratch",""#, r#"":"ok"}"#),
        ] {
            let units: Vec<u16> = prefix.encode_utf16().chain([0xD800]).chain(suffix.encode_utf16()).collect();
            let codes: Vec<u32> = prefix.chars().map(u32::from).chain([0xD800]).chain(suffix.chars().map(u32::from)).collect();
            for (encoding, raw) in [
                ("utf-16-le", units.iter().flat_map(|u| u.to_le_bytes()).collect()),
                ("utf-16-be", units.iter().flat_map(|u| u.to_be_bytes()).collect()),
                ("utf-32-le", codes.iter().flat_map(|u| u.to_le_bytes()).collect()),
                ("utf-32-be", codes.iter().flat_map(|u| u.to_be_bytes()).collect()),
            ] {
                let (b, body) = raw_password(Some(json_ct_name), raw);
                check(&format!("ignored_{kind}_{encoding}"), &send(&router, b, body).await);
            }
        }

        // The API token is the scratch record's token, read from the scratch data dir only.
        let auth: Value = serde_json::from_str(&std::fs::read_to_string(data_dir.join("auth.json")).expect("scratch auth record"))
            .expect("auth record JSON");
        let api_token = auth["token"].as_str().expect("token").to_owned();
        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": "short"}), None, Some(&api_token));
        check("password_api_identity_invalid_body", &send(&router, b, body).await);
        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": NEW_PASSWORD}), None, Some(&api_token));
        check("password_api_identity", &send(&router, b, body).await);

        // A second owner session, to observe the sweep the change performs.
        let other = login(&router, PASSWORD).await;
        check("login_second_session", &other);
        let other_cookie = other.cookie.clone().expect("second session cookie");

        let (b, body) = change(json!({"current_password": PASSWORD, "new_password": NEW_PASSWORD}), Some(&owner_cookie_header), None);
        check("password_ok", &send(&router, b, body).await);

        let credentials = std::fs::read_to_string(data_dir.join("owner-credentials")).expect("credentials file");
        probes.push("password_ok_credentials_file".to_owned());
        let want_creds = case(&oracle, "password_ok_credentials_file");
        let lines: Vec<&str> = credentials.lines().collect();
        let got_creds = (
            lines.first().copied() == Some(&format!("Password: {NEW_PASSWORD}")),
            lines.iter().any(|line| line.starts_with("API token: ")),
        );
        if got_creds != (want_creds["first_line_is_new_password"].as_bool().unwrap(), want_creds["has_api_token_line"].as_bool().unwrap()) {
            extra.push(format!("password_ok_credentials_file: got {got_creds:?}"));
        }

        let current = send(&router, request(Method::GET, "/api/channels").header(header::COOKIE, owner_cookie_header.as_str()), Body::empty()).await;
        check("password_ok_probe_current_session", &current);
        let other_probe = send(&router, request(Method::GET, "/api/channels").header(header::COOKIE, other_cookie.as_str()), Body::empty()).await;
        check("password_ok_probe_other_session", &other_probe);

        check("login_new_password", &login(&router, NEW_PASSWORD).await);
        check("login_old_password", &login(&router, PASSWORD).await);

        // Ops-board answer, from the owner session (the cookie kept by the password change).
        let answer = |body: Body, cookie: Option<&str>| {
            let mut builder = request(Method::POST, "/api/ops-board/answer").header(header::CONTENT_TYPE, "application/json");
            if let Some(cookie) = cookie {
                builder = builder.header(header::COOKIE, cookie);
            }
            (builder, body)
        };
        let (b, body) = answer(json_body(&json!({"task_id": "t1", "reply": "ok"})), None);
        check("answer_no_cookie", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!([1, 2])), Some(&owner_cookie_header));
        check("answer_list_payload", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!({})), Some(&owner_cookie_header));
        check("answer_empty_payload", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!({"task_id": "   ", "reply": "ok"})), Some(&owner_cookie_header));
        check("answer_blank_task", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!({"task_id": "t1"})), Some(&owner_cookie_header));
        check("answer_missing_reply", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!({"task_id": "t1", "reply": "   "})), Some(&owner_cookie_header));
        check("answer_blank_reply", &send(&router, b, body).await);
        let (b, body) = answer(json_body(&json!({"task_id": "t1", "reply": "ok"})), Some(&owner_cookie_header));
        check("answer_no_daemon", &send(&router, b, body).await);

        // Ops-metrics refresh: the auth gate only (the owner path calls gh and reads host state).
        let refresh = send(&router, request(Method::POST, "/api/ops-metrics/refresh"), Body::empty()).await;
        check("metrics_refresh_no_cookie", &refresh);

        // Correct-password BOM body (t3725, owner i50493). It changes the password, so it runs last; the
        // two logins after it show the change on scratch state.
        let bom_new = "t3721-bom-scratch";
        let mut bom_body = vec![0xef, 0xbb, 0xbf];
        bom_body.extend_from_slice(
            json!({"current_password": NEW_PASSWORD, "new_password": bom_new})
                .to_string()
                .as_bytes(),
        );
        let (b, _) = change(json!({}), Some(&owner_cookie_header), None);
        check("password_bom_correct_password", &send(&router, b, Body::from(bom_body)).await);
        check("password_bom_login_new", &login(&router, bom_new).await);
        check("password_bom_login_previous", &login(&router, NEW_PASSWORD).await);

        // Correct-password chain (t3725 / owner i50587): last, on scratch state. Each step changes the password
        // to the next value, in another encoding, so the new password of one step is the current one of the next.
        let mut current = bom_new.to_owned();
        for (name, bom, encoding, next) in [
            ("chain_utf16le_bom", bom16le.to_vec(), 0u8, "t3721-chain-1"),
            ("chain_utf16be_nobom", Vec::new(), 1u8, "t3721-chain-2"),
            ("chain_utf32le_bom", bom32le.to_vec(), 2u8, "t3721-chain-3"),
            ("chain_utf32be_nobom", Vec::new(), 3u8, "t3721-chain-4"),
        ] {
            let text = format!("{{\"current_password\": \"{current}\", \"new_password\": \"{next}\"}}");
            let encoded = match encoding {
                0 => u16le(&text),
                1 => u16be(&text),
                2 => u32le(&text),
                _ => u32be(&text),
            };
            let (b, body) = raw_password(Some(json_ct_name), [bom, encoded].concat());
            check(name, &send(&router, b, body).await);
            current = next.to_owned();
        }
        check("password_chain_login_final", &login(&router, &current).await);
        check("password_chain_login_previous", &login(&router, bom_new).await);

        // Known gaps must be exactly the divergent cases, no more and no fewer.
        let mut expected: Vec<&str> = KNOWN_GAPS.iter().map(|gap| gap.name).collect();
        expected.sort_unstable();
        diverged.sort();
        diverged.dedup();
        failures.extend(extra);
        // Replay inventory (reviewer L6): every oracle case is replayed exactly once, and nothing else.
        replayed.extend(probes);
        let mut replay_want: Vec<String> = oracle["cases"]
            .as_array()
            .expect("cases")
            .iter()
            .map(|case| case["name"].as_str().expect("case name").to_owned())
            .collect();
        replay_want.sort();
        replayed.sort();
        assert_eq!(replayed, replay_want, "the replayed cases must be exactly the oracle's cases");
        assert!(failures.is_empty(), "t3721 parity differs from the Python oracle:\n{}", failures.join("\n"));
        assert_eq!(
            diverged.iter().map(String::as_str).collect::<Vec<_>>(),
            expected,
            "the divergent cases changed; update KNOWN_GAPS only with an owner-scoped follow-up"
        );
        // Owner-accepted differences must be exactly the accepted set (decision i50091).
        let mut accepted_expected: Vec<&str> = INTENTIONAL_DIFFERENCES.iter().map(|item| item.name).collect();
        accepted_expected.sort_unstable();
        accepted_diverged.sort();
        assert_eq!(
            accepted_diverged.iter().map(String::as_str).collect::<Vec<_>>(),
            accepted_expected,
            "the owner-accepted differences changed; they need a new owner decision"
        );
        // The measured, undecided differences must be exactly the pending set (t3724 R3).
        let mut pending_expected: Vec<&str> = PENDING_DISPOSITION.iter().map(|item| item.name).collect();
        pending_expected.sort_unstable();
        pending_diverged.sort();
        assert_eq!(
            pending_diverged.iter().map(String::as_str).collect::<Vec<_>>(),
            pending_expected,
            "the undecided differences changed; route the change to the coordinator"
        );
    });
}
