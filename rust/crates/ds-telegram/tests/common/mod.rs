//! Shared fakes for the bridge tests: a Telegram that records sends, a daemon that holds an outbox,
//! and a desk store built in a temp directory. No test here opens a socket or holds a real token.

#![allow(dead_code)]

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use ds_telegram::{Bot, Bridge, BridgeError, Config, Daemon, Paths};
use serde_json::{json, Map, Value};
use tempfile::TempDir;

/// The pinned wall clock for every test (seconds).
pub const NOW: f64 = 10_000.0;

#[derive(Default)]
pub struct BotState {
    /// `(chat_id, text)` for every sendMessage that went out.
    pub sent: Vec<(Value, String)>,
    /// A sendMessage whose text contains this fails with the Python 502 text.
    pub fail_on: Option<String>,
    /// Queued answers for the next getUpdates.
    pub updates: Vec<Value>,
    /// Scripted getUpdates answers, one batch per call (the oracle replay uses these).
    pub batches: std::collections::VecDeque<Vec<Value>>,
    /// Every method called, in order.
    pub calls: Vec<String>,
    /// Every call with its payload, in order: the oracle replay compares these.
    pub log: Vec<(String, Map<String, Value>)>,
    /// The `payload` of every getUpdates call.
    pub poll_payloads: Vec<Map<String, Value>>,
    pub webhook_url: String,
    pub get_me: Value,
    /// Scripted Bot API file metadata and UTF-8 fixture contents.
    pub files: std::collections::BTreeMap<String, Value>,
}

#[derive(Clone, Default)]
pub struct FakeBot(pub Rc<RefCell<BotState>>);

impl FakeBot {
    pub fn texts(&self) -> Vec<String> {
        self.0
            .borrow()
            .sent
            .iter()
            .map(|(_, text)| text.clone())
            .collect()
    }
    pub fn sent_count(&self) -> usize {
        self.0.borrow().sent.len()
    }
}

impl Bot for FakeBot {
    fn call(&self, method: &str, payload: Map<String, Value>) -> Result<Value, BridgeError> {
        let mut state = self.0.borrow_mut();
        state.calls.push(method.to_string());
        state.log.push((method.to_string(), payload.clone()));
        match method {
            "sendMessage" => {
                let text = payload
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                if let Some(needle) = state.fail_on.clone() {
                    if text.contains(&needle) {
                        return Err(BridgeError::Runtime("Telegram sendMessage: 502".into()));
                    }
                }
                let chat = payload.get("chat_id").cloned().unwrap_or(Value::Null);
                state.sent.push((chat, text));
                Ok(json!({}))
            }
            "getUpdates" => {
                state.poll_payloads.push(payload);
                if let Some(batch) = state.batches.pop_front() {
                    return Ok(Value::Array(batch));
                }
                Ok(Value::Array(std::mem::take(&mut state.updates)))
            }
            "getWebhookInfo" => Ok(json!({"url": state.webhook_url.clone()})),
            "getMe" => Ok(state.get_me.clone()),
            "getFile" => {
                let file_id = payload.get("file_id").and_then(Value::as_str).unwrap_or("");
                let Some(file) = state.files.get(file_id) else {
                    return Err(BridgeError::Runtime(
                        "Telegram getFile: file not found".into(),
                    ));
                };
                Ok(json!({
                    "file_path": file["file_path"],
                    "file_size": file.get("file_size").cloned().unwrap_or(Value::Null),
                }))
            }
            _ => Ok(json!({})),
        }
    }

    fn download(&self, file_id: &str) -> Result<Vec<u8>, BridgeError> {
        let mut payload = Map::new();
        payload.insert("file_id".into(), json!(file_id));
        let info = self.call("getFile", payload)?;
        if info
            .get("file_size")
            .and_then(Value::as_u64)
            .is_some_and(|size| size > ds_telegram::bot::DOWNLOAD_LIMIT as u64)
        {
            return Err(BridgeError::Value(
                "Telegram Bot API cannot download files over 20 MB; use the VP web desk.".into(),
            ));
        }
        let state = self.0.borrow();
        let file = state
            .files
            .get(file_id)
            .ok_or_else(|| BridgeError::Runtime("Telegram file download failed".into()))?;
        let content = file
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| BridgeError::Runtime("Telegram file download failed".into()))?
            .as_bytes();
        if content.len() > ds_telegram::bot::DOWNLOAD_LIMIT {
            return Err(BridgeError::Value(
                "Telegram Bot API cannot download files over 20 MB; use the VP web desk.".into(),
            ));
        }
        Ok(content.to_vec())
    }
}

#[derive(Default)]
pub struct DaemonState {
    /// Every daemon call: `(command, args)`.
    pub calls: Vec<(String, Map<String, Value>)>,
    /// What `owner_outbox` returns as `messages`.
    pub outbox: Vec<Value>,
    /// What `owner_ingest` returns.
    pub ingest: Value,
    /// What `owner_conversation` returns as `messages` (the owner mailbox, oldest first).
    pub conversation: Vec<Value>,
}

#[derive(Clone, Default)]
pub struct FakeDaemon(pub Rc<RefCell<DaemonState>>);

impl FakeDaemon {
    pub fn commands(&self) -> Vec<String> {
        self.0
            .borrow()
            .calls
            .iter()
            .map(|(cmd, _)| cmd.clone())
            .collect()
    }
    pub fn args_of(&self, command: &str) -> Vec<Map<String, Value>> {
        self.0
            .borrow()
            .calls
            .iter()
            .filter(|(cmd, _)| cmd == command)
            .map(|(_, args)| args.clone())
            .collect()
    }
    pub fn receipts(&self) -> Vec<String> {
        self.args_of("owner_delivered")
            .iter()
            .map(|args| args["message_id"].as_str().unwrap_or("").to_string())
            .collect()
    }
}

impl Daemon for FakeDaemon {
    fn call(&mut self, cmd: &str, args: Map<String, Value>) -> Result<Value, BridgeError> {
        let mut state = self.0.borrow_mut();
        state.calls.push((cmd.to_string(), args.clone()));
        match cmd {
            "owner_outbox" => Ok(json!({"messages": state.outbox.clone()})),
            "owner_ingest" => Ok(state.ingest.clone()),
            "owner_conversation" => {
                // The daemon's own contract: `created_at < before`, newest `limit` of those,
                // returned oldest first (ds-core inbox_store::conversation_rows).
                let limit = args
                    .get("limit")
                    .and_then(Value::as_f64)
                    .map(|value| value as usize)
                    .unwrap_or(50);
                let before = args.get("before").and_then(Value::as_f64);
                // The daemon orders by `created_at`; ties keep insertion order, which is as
                // well defined as SQLite's own tie order and lets a test place a row inside a
                // cluster.
                let mut rows: Vec<Value> = state.conversation.clone();
                rows.sort_by(|left, right| {
                    let key = |row: &Value| {
                        row.get("created_at")
                            .and_then(Value::as_f64)
                            .unwrap_or(f64::NEG_INFINITY)
                    };
                    key(left).total_cmp(&key(right))
                });
                rows.retain(
                    |row| match (before, row.get("created_at").and_then(Value::as_f64)) {
                        (Some(cursor), Some(created)) => created < cursor,
                        _ => true,
                    },
                );
                if rows.len() > limit {
                    rows.drain(..rows.len() - limit);
                }
                let count = rows.len();
                Ok(json!({"messages": rows, "count": count}))
            }
            _ => Ok(json!({})),
        }
    }
}

/// A temp data directory with a channel registry, plus the fakes wired to a bridge.
pub struct Fixture {
    pub dir: TempDir,
    pub paths: Paths,
    pub registry: PathBuf,
    pub bot: FakeBot,
    pub daemon: FakeDaemon,
}

impl Fixture {
    pub fn new() -> Self {
        let dir = TempDir::new().expect("temp dir");
        let data = dir.path().join("desk");
        std::fs::create_dir_all(&data).expect("data dir");
        let registry = dir.path().join("channels.json");
        std::fs::write(
            &registry,
            r#"[{"id":"vp","name":"VP"},{"id":"cos","name":"Chief of Staff"}]"#,
        )
        .expect("registry");
        Fixture {
            dir,
            paths: Paths::new(data),
            registry,
            bot: FakeBot::default(),
            daemon: FakeDaemon::default(),
        }
    }

    pub fn bridge(&self) -> Bridge<FakeDaemon, FakeBot> {
        Bridge {
            paths: self.paths.clone(),
            registry_path: self.registry.clone(),
            env_channel: None,
            daemon: self.daemon.clone(),
            bot: self.bot.clone(),
            clock: Box::new(|| NOW),
        }
    }

    /// Write `telegram.json` exactly as the Python bridge would hold it.
    pub fn write_config(&self, config: &Value) {
        std::fs::write(
            self.paths.config(),
            serde_json::to_string_pretty(config).unwrap(),
        )
        .expect("config");
    }

    pub fn read_config(&self) -> Config {
        let text = std::fs::read_to_string(self.paths.config()).expect("config file");
        match serde_json::from_str::<Value>(&text).expect("json") {
            Value::Object(map) => map,
            _ => panic!("config is not an object"),
        }
    }

    pub fn write_routing(&self, body: &str) {
        std::fs::write(self.paths.routing(), body).expect("routing");
    }

    /// A legacy desk store with a `messages` table and the given `(id, channel, role, body)` rows.
    pub fn desk_rows(&self, rows: &[(i64, &str, &str, &str)]) {
        let conn = rusqlite::Connection::open(self.paths.desk_db()).expect("desk db");
        conn.execute_batch(DESK_SCHEMA).expect("schema");
        for (id, channel, role, body) in rows {
            conn.execute(
                "INSERT INTO messages (id, channel, role, body) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, channel, role, body],
            )
            .expect("row");
        }
    }

    /// Append one row of any role carrying a Telegram `external_id`, as the desk stores a
    /// Telegram message it recorded itself. Used to prove `/status` does not read this store.
    pub fn desk_row_with_telegram_id(&self, id: i64, channel: &str, role: &str, telegram: &str) {
        let conn = rusqlite::Connection::open(self.paths.desk_db()).expect("desk db");
        conn.execute_batch(DESK_SCHEMA).expect("schema");
        conn.execute(
            "INSERT INTO messages (id, channel, role, body, external_id) VALUES (?1, ?2, ?3, '', ?4)",
            rusqlite::params![id, channel, role, telegram],
        )
        .expect("row");
    }
}

/// The desk's `messages` table, with the two columns `/status` reads. The real desk stores
/// `delivery NOT NULL DEFAULT 'posted'`; the fixtures pass it explicitly.
const DESK_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS messages (\
     id INTEGER PRIMARY KEY, channel TEXT, role TEXT, body TEXT, \
     delivery TEXT NOT NULL DEFAULT 'posted', external_id TEXT);\
     CREATE TABLE IF NOT EXISTS attachments (id TEXT PRIMARY KEY, filename TEXT NOT NULL, \
     mime TEXT NOT NULL, size INTEGER NOT NULL, path TEXT NOT NULL, message_id INTEGER);";

/// A paired private-chat text update.
pub fn text_update(update_id: i64, chat_id: i64, text: &str) -> Value {
    json!({
        "update_id": update_id,
        "message": {"chat": {"id": chat_id, "type": "private"}, "text": text}
    })
}

/// A mailbox row the daemon returns in the Telegram outbox.
pub fn outbox_row(
    message_id: &str,
    sender: Option<&str>,
    body: &str,
    reply_to: Option<&str>,
) -> Value {
    json!({
        "message_id": message_id,
        "sender": sender,
        "transport": "telegram",
        "reply_to": reply_to,
        "body": body,
    })
}

/// An owner-mailbox row as the daemon returns it in `owner_conversation` (oldest first).
///
/// `created_at` is what the paging cursor is built from; give rows distinct values so a page
/// boundary is well defined.
pub fn mailbox_row_at(
    id: &str,
    transport: &str,
    state: &str,
    external_id: Option<&str>,
    reply_to: Option<&str>,
    created_at: f64,
) -> Value {
    json!({
        "id": id,
        "transport": transport,
        "state": state,
        "external_id": external_id,
        "reply_to": reply_to,
        "created_at": created_at,
        "answered": reply_to.is_none() && state == "acked",
    })
}

/// `mailbox_row_at` with a `created_at` of 0, for tests that never cross a page boundary.
pub fn mailbox_row(
    id: &str,
    transport: &str,
    state: &str,
    external_id: Option<&str>,
    reply_to: Option<&str>,
) -> Value {
    mailbox_row_at(id, transport, state, external_id, reply_to, 0.0)
}

/// A config for a paired bridge with no prior cursor.
pub fn paired_config(chat_id: i64) -> Value {
    json!({
        "bot_token": "123:TEST-TOKEN-NOT-REAL",
        "pair_code": "PAIRCODE",
        "chat_id": chat_id,
        "update_offset": null,
        "last_brief_id": null,
        "brief_channel": "cos"
    })
}
