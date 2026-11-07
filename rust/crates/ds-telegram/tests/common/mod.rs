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
    /// Every method called, in order.
    pub calls: Vec<String>,
    /// The `payload` of every getUpdates call.
    pub poll_payloads: Vec<Map<String, Value>>,
    pub webhook_url: String,
    pub get_me: Value,
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
                Ok(Value::Array(std::mem::take(&mut state.updates)))
            }
            "getWebhookInfo" => Ok(json!({"url": state.webhook_url.clone()})),
            "getMe" => Ok(state.get_me.clone()),
            _ => Ok(json!({})),
        }
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
        state.calls.push((cmd.to_string(), args));
        match cmd {
            "owner_outbox" => Ok(json!({"messages": state.outbox.clone()})),
            "owner_ingest" => Ok(state.ingest.clone()),
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
        conn.execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, channel TEXT, role TEXT, body TEXT);",
        )
        .expect("schema");
        for (id, channel, role, body) in rows {
            conn.execute(
                "INSERT INTO messages (id, channel, role, body) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, channel, role, body],
            )
            .expect("row");
        }
    }
}

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
