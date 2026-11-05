//! Real sources for the owner and fleet web routes.
//!
//! All paths and channels are supplied by the runtime. The authenticated daemon
//! client is shared through the pane backend; this adapter never discovers
//! credentials or starts a daemon itself.

use crate::{
    ops_summary,
    roster::{discovered_id, is_lane, match_pane},
    routes::{
        auth::Identity,
        fleet::{FleetError, FleetFuture, FleetSources},
        messages_write::MessageIn,
        owner::{OwnerWebBackend, OwnerWebError, OwnerWebFuture},
        panes::{PanesWebBackend, PanesWebError},
    },
};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension, Row};
use serde_json::{json, Map, Value};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{Arc, Mutex, PoisonError},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const MAX_REGISTRY_BYTES: u64 = 2 * 1024 * 1024;
const MAX_SESSION_FILES: usize = 20_000;
const MAX_PROCESSES: usize = 10_000;
const MAX_INVENTORY_ROWS: usize = 500;
const MAX_CMDLINE_BYTES: usize = 16 * 1024;
const MAX_TMUX_OUTPUT_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct OwnerFleetConfig {
    /// The already initialized `web/app.py` messages database.
    pub messages_db: PathBuf,
    /// Existing operations metrics database populated by the metrics collector.
    pub operations_db: PathBuf,
    pub host_watch_state: PathBuf,
    pub opencode_db: PathBuf,
    pub lane_registry: PathBuf,
    pub session_roots: Vec<PathBuf>,
    pub proc_root: PathBuf,
    pub home: PathBuf,
    /// Executable path for `tmux list-panes -a -F ...`; explicit for fixtures.
    pub tmux_program: PathBuf,
    pub channels: HashMap<String, Value>,
    pub desk_channel: Option<Value>,
    pub summary_clock: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl OwnerFleetConfig {
    pub fn new(messages_db: impl Into<PathBuf>, operations_db: impl Into<PathBuf>) -> Self {
        Self {
            messages_db: messages_db.into(),
            operations_db: operations_db.into(),
            host_watch_state: PathBuf::new(),
            opencode_db: PathBuf::new(),
            lane_registry: PathBuf::new(),
            session_roots: Vec::new(),
            proc_root: PathBuf::new(),
            home: PathBuf::new(),
            tmux_program: PathBuf::new(),
            channels: HashMap::new(),
            desk_channel: None,
            summary_clock: Arc::new(Utc::now),
        }
    }

    pub fn with_local_sources(
        mut self,
        lane_registry: impl Into<PathBuf>,
        session_roots: Vec<PathBuf>,
        proc_root: impl Into<PathBuf>,
        home: impl Into<PathBuf>,
        tmux_program: impl Into<PathBuf>,
        channels: HashMap<String, Value>,
    ) -> Self {
        self.lane_registry = lane_registry.into();
        self.session_roots = session_roots;
        self.proc_root = proc_root.into();
        self.home = home.into();
        self.tmux_program = tmux_program.into();
        self.channels = channels;
        self
    }

    pub fn with_summary_sources(
        mut self,
        host_watch_state: impl Into<PathBuf>,
        opencode_db: impl Into<PathBuf>,
    ) -> Self {
        self.host_watch_state = host_watch_state.into();
        self.opencode_db = opencode_db.into();
        self
    }

    pub fn with_desk_channel(mut self, channel: Value) -> Self {
        self.desk_channel = Some(channel);
        self
    }

    pub fn with_summary_clock(
        mut self,
        clock: impl Fn() -> DateTime<Utc> + Send + Sync + 'static,
    ) -> Self {
        self.summary_clock = Arc::new(clock);
        self
    }
}

/// Both route interfaces share this adapter and the caller's pane backend.
/// The desk connection is opened once and retained for direct messages.
pub struct OwnerFleetBackend {
    rpc: Arc<dyn PanesWebBackend>,
    config: OwnerFleetConfig,
    messages: Mutex<Connection>,
    capability_cache: Mutex<HashMap<String, (f64, Value)>>,
}

impl OwnerFleetBackend {
    pub fn open(
        rpc: Arc<dyn PanesWebBackend>,
        config: OwnerFleetConfig,
    ) -> Result<Self, FleetError> {
        if [
            &config.messages_db,
            &config.operations_db,
            &config.host_watch_state,
            &config.opencode_db,
            &config.lane_registry,
            &config.proc_root,
            &config.home,
            &config.tmux_program,
        ]
        .iter()
        .any(|path| path.as_os_str().is_empty())
        {
            return Err(FleetError::Other(
                "owner/fleet backend requires explicit database and source paths".into(),
            ));
        }
        let messages =
            Connection::open_with_flags(&config.messages_db, OpenFlags::SQLITE_OPEN_READ_WRITE)
                .map_err(|error| FleetError::Database(sqlite_kind(&error).to_owned()))?;
        messages
            .busy_timeout(Duration::from_secs(10))
            .map_err(|error| FleetError::Database(sqlite_kind(&error).to_owned()))?;
        messages
            .execute_batch("PRAGMA foreign_keys=ON; PRAGMA journal_mode=WAL;")
            .map_err(|error| FleetError::Database(sqlite_kind(&error).to_owned()))?;
        ensure_message_schema(&messages)?;
        Ok(Self {
            rpc,
            config,
            messages: Mutex::new(messages),
            capability_cache: Mutex::new(HashMap::new()),
        })
    }

    fn now(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs_f64()
    }

    async fn capability(&self, pane: &str) -> Result<Value, PanesWebError> {
        let now = self.rpc.now_seconds();
        if let Some((seen, value)) = self
            .capability_cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(pane)
            .cloned()
        {
            if now - seen < 20.0 {
                return Ok(value);
            }
        }
        let value = self
            .rpc
            .rpc("delivery_capability", json!({"target": pane}))
            .await?;
        if value.is_object() {
            self.capability_cache
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(pane.to_owned(), (self.rpc.now_seconds(), value.clone()));
        }
        Ok(value)
    }

    async fn find_message_target(&self, channel: &str) -> Result<Value, FleetError> {
        if !known_channel(channel, &self.config.channels) {
            return Err(FleetError::Http(
                axum::http::StatusCode::NOT_FOUND,
                "Unknown channel".into(),
            ));
        }
        let listing = self
            .rpc
            .rpc("list_panes", json!({}))
            .await
            .map_err(|error| match error {
                PanesWebError::Http { status, detail } => FleetError::Http(status, detail),
                PanesWebError::Failure(detail) => FleetError::Http(
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    format!("Dunking Sheep unavailable: {detail}"),
                ),
            })?;
        let panes = listing
            .get("panes")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                FleetError::Other("pane roster response is missing its panes list".into())
            })?;
        if let Some(config) = self.config.channels.get(channel) {
            return match_pane(config, panes)
                .map_err(|error| FleetError::Other(error.to_string()))?
                .ok_or_else(|| {
                    FleetError::Http(
                        axum::http::StatusCode::CONFLICT,
                        "No active agent is available in this channel".into(),
                    )
                });
        }
        for pane in panes {
            let Some(pane_id) = pane.get("pane_id").and_then(Value::as_str) else {
                continue;
            };
            if discovered_id(pane_id) != channel {
                continue;
            }
            if is_lane(Some(pane)).unwrap_or(false)
                || self
                    .capability(pane_id)
                    .await
                    .ok()
                    .and_then(|c| c.get("agent_alive").cloned())
                    == Some(json!(true))
            {
                return Ok(pane.clone());
            }
        }
        Err(FleetError::Http(
            axum::http::StatusCode::CONFLICT,
            "No active agent is available in this channel".into(),
        ))
    }

    async fn send_direct(&self, pane: &str, body: &str) -> Result<Value, String> {
        let capability = self.capability(pane).await.map_err(|error| match error {
            PanesWebError::Http { detail, .. } | PanesWebError::Failure(detail) => detail,
        })?;
        let route = capability.get("send").and_then(Value::as_str).unwrap_or("");
        if !matches!(route, "owner_push" | "guarded") {
            return Err(format!(
                "{pane} declares no send route ({route:?}); not guessing"
            ));
        }
        let (method, args) = if route == "owner_push" {
            ("owner_push", json!({"target":pane,"body":body}))
        } else {
            ("send_text", json!({"target":pane,"text":body}))
        };
        let payload = self.rpc.rpc(method, args).await.map_err(|error| {
            let detail = match error {
                PanesWebError::Http { detail, .. } | PanesWebError::Failure(detail) => detail,
            };
            format!("delivery path unavailable: {detail}")
        })?;
        let result = send_result(pane, route, &payload);
        if let Some(error) = result.get("error").and_then(Value::as_str) {
            return Err(error.to_owned());
        }
        Ok(result)
    }
}

impl OwnerWebBackend for OwnerFleetBackend {
    fn rpc<'a>(
        &'a self,
        method: &'a str,
        args: Value,
    ) -> OwnerWebFuture<'a, Result<Value, OwnerWebError>> {
        Box::pin(async move {
            self.rpc
                .rpc(method, args)
                .await
                .map_err(|error| match error {
                    PanesWebError::Http { status, detail } => {
                        OwnerWebError::Http { status, detail }
                    }
                    PanesWebError::Failure(detail) => OwnerWebError::Failure(detail),
                })
        })
    }

    fn desk_channel(&self) -> Option<Value> {
        self.config.desk_channel.clone()
    }

    fn now_seconds(&self) -> f64 {
        self.rpc.now_seconds()
    }
}

impl FleetSources for OwnerFleetBackend {
    fn registry(&self) -> Result<Value, FleetError> {
        let metadata = match fs::metadata(&self.config.lane_registry) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(json!({})),
            Err(error) => return Err(FleetError::Other(error.to_string())),
        };
        if metadata.len() > MAX_REGISTRY_BYTES {
            return Ok(json!({}));
        }
        match fs::read(&self.config.lane_registry)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        {
            Some(value @ Value::Object(_)) => Ok(value),
            _ => Ok(json!({})),
        }
    }

    fn session_mtimes(&self) -> Result<HashMap<String, f64>, FleetError> {
        let mut output = HashMap::new();
        let mut seen = 0usize;
        for root in &self.config.session_roots {
            let entries = match fs::read_dir(root) {
                Ok(entries) => entries,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(_) => continue,
            };
            for directory in entries.flatten() {
                seen += 1;
                if seen >= MAX_SESSION_FILES {
                    return Ok(output);
                }
                if !directory
                    .file_type()
                    .map(|kind| kind.is_dir())
                    .unwrap_or(false)
                {
                    continue;
                }
                let files = match fs::read_dir(directory.path()) {
                    Ok(files) => files,
                    Err(_) => continue,
                };
                for entry in files.flatten() {
                    seen += 1;
                    if seen >= MAX_SESSION_FILES {
                        return Ok(output);
                    }
                    if entry
                        .path()
                        .extension()
                        .is_none_or(|extension| extension != "jsonl")
                    {
                        continue;
                    }
                    let entry_path = entry.path();
                    let Some(stem) = entry_path.file_stem().and_then(|stem| stem.to_str()) else {
                        continue;
                    };
                    let id = stem.rsplit('_').next().unwrap_or(stem);
                    if let Ok(modified) = entry.metadata().and_then(|metadata| metadata.modified())
                    {
                        if let Ok(seconds) = modified.duration_since(UNIX_EPOCH) {
                            output.insert(id.to_owned(), seconds.as_secs_f64());
                        }
                    }
                }
            }
        }
        Ok(output)
    }

    fn inventory(&self, routed: &[String]) -> Value {
        collect_inventory(
            &self.config.proc_root,
            &self.config.home,
            &self.config.tmux_program,
            routed,
        )
    }

    fn configured_channels(&self) -> HashMap<String, Value> {
        self.config.channels.clone()
    }

    fn post_direct_message<'a>(
        &'a self,
        data: MessageIn,
        identity: Identity,
    ) -> FleetFuture<'a, Result<Value, FleetError>> {
        Box::pin(async move {
            // The Python helper receives this authenticated identity unchanged; its
            // direct-message persistence intentionally does not branch on owner/API.
            let _authenticated_identity = identity;
            if !known_channel(&data.channel, &self.config.channels) {
                return Err(FleetError::Http(
                    axum::http::StatusCode::NOT_FOUND,
                    "Unknown channel".into(),
                ));
            }
            if data.body.trim().is_empty() && data.attachment_ids.is_empty() {
                return Err(FleetError::Http(
                    axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                    "Message is empty".into(),
                ));
            }
            if let Some(external_id) = data.external_id.as_deref() {
                let db = self.messages.lock().unwrap_or_else(PoisonError::into_inner);
                if let Some(row) = db
                    .query_row(
                        "SELECT * FROM messages WHERE channel=?1 AND external_id=?2",
                        params![data.channel, external_id],
                        message_json,
                    )
                    .optional()
                    .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?
                {
                    return decorate_message(&db, row);
                }
            }
            let pane = self.find_message_target(&data.channel).await?;
            if !pane_can_message(self, &pane).await {
                return Err(FleetError::Http(
                    axum::http::StatusCode::CONFLICT,
                    "No active agent is available in this channel".into(),
                ));
            }
            if data.body.chars().count() > 12_000 || data.attachment_ids.len() > 10 {
                return Err(FleetError::Other("invalid message model".into()));
            }
            let pane_id = pane["pane_id"].as_str().unwrap_or_default().to_owned();
            let (id, prompt) = match self.persist_pending(&data)? {
                PersistedMessage::Existing(item) => return Ok(item),
                PersistedMessage::Pending { id, prompt } => (id, prompt),
            };
            let transport = self.send_direct(&pane_id, &prompt).await;
            self.finish_direct(id, transport)
        })
    }

    fn summary<'a>(&'a self) -> FleetFuture<'a, Result<Value, FleetError>> {
        Box::pin(async move {
            let path = self.config.operations_db.clone();
            let host = self.config.host_watch_state.clone();
            let opencode = self.config.opencode_db.clone();
            let now = (self.config.summary_clock)();
            tokio::task::spawn_blocking(move || {
                let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)
                    .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?;
                ops_summary::summary(&db, now, &host, &opencode).map_err(|error| match error {
                    ops_summary::SummaryError::Sql(error) => {
                        FleetError::Database(sqlite_kind(&error).into())
                    }
                    error => FleetError::Other(error.to_string()),
                })
            })
            .await
            .map_err(|error| FleetError::Other(error.to_string()))?
        })
    }
}

impl OwnerFleetBackend {
    fn persist_pending(&self, data: &MessageIn) -> Result<PersistedMessage, FleetError> {
        let mut db = self.messages.lock().unwrap_or_else(PoisonError::into_inner);
        let tx = db
            .transaction()
            .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?;
        if let Some(external_id) = data.external_id.as_deref() {
            if let Some(row) = tx
                .query_row(
                    "SELECT * FROM messages WHERE channel=?1 AND external_id=?2",
                    params![data.channel, external_id],
                    message_json,
                )
                .optional()
                .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?
            {
                let item = decorate_message(&tx, row)?;
                tx.commit()
                    .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?;
                return Ok(PersistedMessage::Existing(item));
            }
        }
        let mut refs = Vec::new();
        for attachment_id in &data.attachment_ids {
            let value = tx
                .query_row(
                    "SELECT filename,path FROM attachments WHERE id=?1 AND message_id IS NULL",
                    [attachment_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                )
                .optional()
                .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?
                .ok_or_else(|| {
                    FleetError::Http(
                        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                        format!("Attachment {attachment_id} is unavailable"),
                    )
                })?;
            refs.push(value);
        }
        tx.execute("INSERT INTO messages(channel,role,body,delivery,transport_attempt_at,external_id,direct_transport) VALUES(?1,'owner',?2,'pending',?3,?4,'{}')",params![data.channel,data.body.trim(),self.now(),data.external_id])
            .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?;
        let id = tx.last_insert_rowid();
        for attachment in &data.attachment_ids {
            tx.execute(
                "UPDATE attachments SET message_id=?1 WHERE id=?2",
                params![id, attachment],
            )
            .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?;
        }
        let prompt = prompt_for(id, &data.channel, data.body.trim(), &refs);
        tx.commit()
            .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?;
        Ok(PersistedMessage::Pending { id, prompt })
    }

    fn finish_direct(
        &self,
        id: i64,
        transport: Result<Value, String>,
    ) -> Result<Value, FleetError> {
        let (delivery, detail, transport_id) = match &transport {
            Ok(value) => (
                value["outcome"].as_str().unwrap_or("uncertain").to_owned(),
                value["detail"].as_str().unwrap_or("").to_owned(),
                value
                    .get("push_id")
                    .filter(|id| truthy(id))
                    .or_else(|| value.get("message_id").filter(|id| truthy(id)))
                    .map(json_sql_value),
            ),
            Err(_) => (
                "uncertain".into(),
                "Delivery unconfirmed. Check agent activity before sending again.".into(),
                None,
            ),
        };
        let serialized = transport
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_else(|_| "{}".into()))
            .unwrap_or_else(|_| "{}".into());
        let db = self.messages.lock().unwrap_or_else(PoisonError::into_inner);
        db.execute("UPDATE messages SET delivery=?1,delivery_detail=?2,transport_id=?3,direct_transport=?4,transport_attempts=1,transport_attempt_at=?5 WHERE id=?6",params![delivery,detail,transport_id,serialized,self.now(),id]).map_err(|error|FleetError::Database(sqlite_kind(&error).into()))?;
        let row = db
            .query_row("SELECT * FROM messages WHERE id=?1", [id], message_json)
            .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?;
        let mut item = decorate_message(&db, row)?;
        item["transport"] = transport.unwrap_or_else(|error| json!({"error":error}));
        Ok(item)
    }
}

enum PersistedMessage {
    Existing(Value),
    Pending { id: i64, prompt: String },
}

fn json_sql_value(value: &Value) -> rusqlite::types::Value {
    match value {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(value) => rusqlite::types::Value::Integer(i64::from(*value)),
        Value::Number(value) if value.is_i64() => {
            rusqlite::types::Value::Integer(value.as_i64().unwrap_or_default())
        }
        Value::Number(value) => rusqlite::types::Value::Real(value.as_f64().unwrap_or_default()),
        Value::String(value) => rusqlite::types::Value::Text(value.clone()),
        _ => rusqlite::types::Value::Text(value.to_string()),
    }
}

async fn pane_can_message(backend: &OwnerFleetBackend, pane: &Value) -> bool {
    if is_lane(Some(pane)).unwrap_or(false) {
        return true;
    }
    let Some(id) = pane["pane_id"].as_str() else {
        return false;
    };
    async_capability_alive(backend, id).await
}

// Kept separate to make the capability read explicit at every discovered-pane
// boundary; callers never substitute the agent name for daemon authority.
async fn async_capability_alive(backend: &OwnerFleetBackend, pane: &str) -> bool {
    backend
        .capability(pane)
        .await
        .ok()
        .is_some_and(|value| value["agent_alive"] == true)
}

fn known_channel(channel: &str, channels: &HashMap<String, Value>) -> bool {
    channels.contains_key(channel)
        || (channel.starts_with("pane-")
            && channel.len() > 5
            && channel[5..]
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
}

fn send_result(pane: &str, route: &str, payload: &Value) -> Value {
    let detail = payload
        .get("message")
        .filter(|v| !v.is_null() && !v.as_str().unwrap_or("").is_empty())
        .or_else(|| payload.get("outcome"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .chars()
        .take(400)
        .collect::<String>();
    let outcome = payload["outcome"]
        .as_str()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if route == "owner_push" {
        if outcome.is_empty() {
            return json!({"error":format!("the daemon stored the message but returned no outcome field, so the desk will not invent one: {}",detail.chars().take(160).collect::<String>())});
        }
        return json!({"ok":true,"pane":pane,"route":route,"outcome":outcome,"push_id":payload["id"],"detail":detail,
            "note":"Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it."});
    }
    if outcome == "refused" {
        return json!({"ok":false,"pane":pane,"route":route,"outcome":outcome,"message_id":payload["message_id"],"detail":payload.get("message").cloned().unwrap_or(json!(detail))});
    }
    let (ok,note)=match outcome.as_str() {
        "queued" => (true,"Held by the typing guard or not yet delivered. It has NOT arrived - do not read this as delivered."),
        "unconfirmed" => (true,"Neither witness fired. It may well have arrived - do NOT resend blindly; ask the lane first."),
        "stored" => (true,"Stored in a durable queue the lane drains itself. That is NOT 'read' - it has not arrived until the lane takes it."),
        "ok"|"sent" => (true,"Confirmed by the box clearing or by the turn it started - that is delivery to the lane, not proof the agent acted on it."),
        _ => (false,""),
    };
    if outcome.is_empty() {
        return json!({"error":if detail.is_empty(){"No daemon outcome".into()}else{detail}});
    }
    if !matches!(
        outcome.as_str(),
        "queued" | "unconfirmed" | "stored" | "ok" | "sent"
    ) {
        return json!({"error":format!("the sender returned no outcome field, so the desk will not guess: {}",detail.chars().take(200).collect::<String>())});
    }
    json!({"ok":ok,"pane":pane,"route":route,"outcome":outcome,"message_id":payload["message_id"],"detail":detail,"note":note})
}

fn prompt_for(id: i64, channel: &str, body: &str, attachments: &[(String, String)]) -> String {
    let refs = attachments
        .iter()
        .map(|(name, path)| format!("- {name}: {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut prompt = body.to_owned();
    if !refs.is_empty() {
        prompt.push_str(&format!(
            "\n\nAttached files on this host (read these paths directly):\n{refs}"
        ));
    }
    prompt.push_str(&format!("\n\n[VP web message #{id}; reply through vpctl brief --channel {channel} --answers {id} when a concise response is ready.]"));
    prompt
}

fn message_json(row: &Row<'_>) -> rusqlite::Result<Value> {
    let mut object = Map::new();
    for name in [
        "id",
        "channel",
        "role",
        "body",
        "delivery",
        "delivery_detail",
        "transport_id",
        "transport_attempts",
        "transport_attempt_at",
        "external_id",
        "created_at",
        "answer_id",
        "answered_at",
        "direct_transport",
        "delivered_at",
        "delivery_latency_s",
    ] {
        let value: rusqlite::types::Value = row.get(name)?;
        let value = match value {
            rusqlite::types::Value::Null => Value::Null,
            rusqlite::types::Value::Integer(v) => json!(v),
            rusqlite::types::Value::Real(v) => json!(v),
            rusqlite::types::Value::Text(v) => json!(v),
            rusqlite::types::Value::Blob(v) => json!(v),
        };
        object.insert(name.into(), value);
    }
    Ok(Value::Object(object))
}

fn ensure_message_schema(db: &Connection) -> Result<(), FleetError> {
    for (table, required) in [
        (
            "messages",
            &[
                "id",
                "channel",
                "role",
                "body",
                "delivery",
                "delivery_detail",
                "transport_id",
                "transport_attempts",
                "transport_attempt_at",
                "external_id",
                "created_at",
                "answer_id",
                "answered_at",
                "direct_transport",
                "delivered_at",
                "delivery_latency_s",
            ][..],
        ),
        (
            "attachments",
            &[
                "id",
                "filename",
                "mime",
                "size",
                "path",
                "message_id",
                "created_at",
            ][..],
        ),
    ] {
        let mut statement = db
            .prepare(&format!("PRAGMA table_info({table})"))
            .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))
            .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?
            .collect::<Result<HashSet<_>, _>>()
            .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?;
        if required.iter().any(|name| !columns.contains(*name)) {
            return Err(FleetError::Other(format!(
                "initialized web database is missing required {table} columns"
            )));
        }
    }
    Ok(())
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn decorate_message(db: &Connection, mut item: Value) -> Result<Value, FleetError> {
    let id = item["id"].as_i64().unwrap_or_default();
    let direct = item["direct_transport"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    item.as_object_mut().unwrap().remove("direct_transport");
    if let Some(encoded) = direct {
        if let Ok(value) = serde_json::from_str::<Value>(&encoded) {
            item["outcome"] = value.get("outcome").cloned().unwrap_or(Value::Null);
            item["transport"] = value;
        }
    }
    let mut statement = db
        .prepare(
            "SELECT id,filename,mime,size FROM attachments WHERE message_id=?1 ORDER BY created_at",
        )
        .map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?;
    let rows=statement.query_map([id],|row|Ok(json!({"id":row.get::<_,String>(0)?,"filename":row.get::<_,String>(1)?,"mime":row.get::<_,String>(2)?,"size":row.get::<_,i64>(3)?})))
        .map_err(|error|FleetError::Database(sqlite_kind(&error).into()))?;
    let mut attachments = Vec::new();
    for row in rows {
        attachments.push(row.map_err(|error| FleetError::Database(sqlite_kind(&error).into()))?);
    }
    item["attachments"] = json!(attachments);
    Ok(item)
}

fn sqlite_kind(error: &rusqlite::Error) -> &'static str {
    match error {
        rusqlite::Error::SqliteFailure(inner, _) => match inner.code {
            rusqlite::ffi::ErrorCode::CannotOpen => "OperationalError",
            rusqlite::ffi::ErrorCode::DatabaseBusy | rusqlite::ffi::ErrorCode::DatabaseLocked => {
                "OperationalError"
            }
            rusqlite::ffi::ErrorCode::ConstraintViolation => "IntegrityError",
            _ => "DatabaseError",
        },
        _ => "DatabaseError",
    }
}

#[derive(Debug)]
struct ProcRow {
    pid: u32,
    ppid: u32,
    agent: Option<String>,
    cwd: Option<String>,
}

fn collect_inventory(
    proc_root: &Path,
    home: &Path,
    tmux_program: &Path,
    routed: &[String],
) -> Value {
    let started = Instant::now();
    let Some(pane_pids) = tmux_panes(tmux_program, started) else {
        return json!({"state":"unavailable","rows":[],"reason":"Process or pane inventory unavailable"});
    };
    let processes = match process_rows(proc_root) {
        Ok(rows) => rows,
        Err(_) => {
            return json!({"state":"unavailable","rows":[],"reason":"Process or pane inventory unavailable"})
        }
    };
    let routed: HashSet<&str> = routed.iter().map(String::as_str).collect();
    let mut output = Vec::new();
    for process in processes.values().filter(|row| row.agent.is_some()) {
        let (pane, nested) = ancestor_pane(process, &processes, &pane_pids);
        if pane.as_deref().is_some_and(|id| routed.contains(id)) && !nested {
            continue;
        }
        let reason = if nested {
            "Agent process has no independent pane route"
        } else if pane.is_some() {
            "Pane is absent from the desk's agent roster"
        } else {
            "No pane route found for this process"
        };
        let stamp = session_write(process, proc_root, home);
        let project = Path::new(process.cwd.as_deref().unwrap_or("unknown"))
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        output.push(json!({"pid":process.pid,"agent":process.agent,"cwd":process.cwd,"pane_id":pane,"reason":reason,"project":project,
            "last_session_write_s":stamp.map(|t|((SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()-t).max(0.0)) as i64),
            "session_age_reason":if stamp.is_some(){Value::Null}else{json!("No unambiguous open session file")}}));
        if output.len() >= MAX_INVENTORY_ROWS {
            break;
        }
    }
    output.sort_by(|a, b| {
        (a["project"].as_str(), a["pid"].as_i64()).cmp(&(b["project"].as_str(), b["pid"].as_i64()))
    });
    json!({"state":"ok","rows":output,"source":"local process metadata and tmux pane ancestry; session-file mtime only","note":"Detected process presence is not proof of active work; unrecognized harnesses are not counted."})
}

fn tmux_panes(program: &Path, started: Instant) -> Option<HashMap<u32, String>> {
    let mut child = Command::new(program)
        .args(["list-panes", "-a", "-F", "#{pane_id} #{pane_pid}"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let stdout = child.stdout.take()?;
    let reader = thread::spawn(move || {
        let mut output = Vec::new();
        let mut stdout = stdout.take((MAX_TMUX_OUTPUT_BYTES + 1) as u64);
        stdout.read_to_end(&mut output).ok()?;
        Some(output)
    });
    loop {
        if started.elapsed() > Duration::from_secs(5) {
            let _ = child.kill();
            let _ = child.wait();
            let _ = reader.join();
            return None;
        }
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => return None,
        }
    }
    let _ = child.wait().ok()?;
    let output = reader.join().ok()??;
    if output.len() > MAX_TMUX_OUTPUT_BYTES {
        return None;
    }
    let mut panes = HashMap::new();
    for line in String::from_utf8_lossy(&output).lines() {
        let mut parts = line.split_whitespace();
        let (Some(pane), Some(pid), None) = (parts.next(), parts.next(), parts.next()) else {
            return None;
        };
        let pid = pid.parse::<u32>().ok()?;
        panes.insert(pid, pane.to_owned());
    }
    Some(panes)
}

fn process_rows(root: &Path) -> io::Result<HashMap<u32, ProcRow>> {
    use std::os::unix::fs::MetadataExt;
    let uid = unsafe_uid();
    let mut rows = HashMap::new();
    for entry in fs::read_dir(root)?.take(MAX_PROCESSES) {
        let Ok(entry) = entry else {
            continue;
        };
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = name.parse::<u32>() else {
            continue;
        };
        let path = entry.path();
        if fs::metadata(&path).map(|m| m.uid() != uid).unwrap_or(true) {
            continue;
        }
        let stat = match fs::read_to_string(path.join("stat")) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let Some(after) = stat.rsplit_once(')').map(|(_, tail)| tail) else {
            continue;
        };
        let fields = after.split_whitespace().collect::<Vec<_>>();
        if fields.first() == Some(&"Z") {
            continue;
        }
        let Some(ppid) = fields.get(1).and_then(|v| v.parse().ok()) else {
            continue;
        };
        let args = match fs::read(path.join("cmdline")) {
            Ok(bytes) if bytes.len() <= MAX_CMDLINE_BYTES => bytes,
            Ok(_) => continue,
            Err(_) => continue,
        };
        let args = args
            .split(|b| *b == 0)
            .filter(|v| !v.is_empty())
            .map(|v| String::from_utf8_lossy(v).into_owned())
            .collect::<Vec<_>>();
        let exe = fs::read_link(path.join("exe"))
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_default();
        let agent = harness(&args, &exe);
        let cwd = if agent.is_some() {
            fs::read_link(path.join("cwd"))
                .ok()
                .map(|p| p.to_string_lossy().into_owned())
        } else {
            None
        };
        rows.insert(
            pid,
            ProcRow {
                pid,
                ppid,
                agent,
                cwd,
            },
        );
    }
    Ok(rows)
}

#[cfg(unix)]
fn unsafe_uid() -> u32 {
    use std::os::unix::fs::MetadataExt;
    fs::metadata(".").map(|m| m.uid()).unwrap_or(u32::MAX)
}

fn harness(args: &[String], exe: &str) -> Option<String> {
    let names = args
        .iter()
        .take(2)
        .map(|s| {
            Path::new(s)
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string()
        })
        .collect::<Vec<_>>();
    if names.first().is_some_and(|n| n == "codex")
        && names
            .get(1)
            .is_some_and(|n| n == "app-server" || n == "mcp-server")
    {
        return None;
    }
    for name in std::iter::once(
        Path::new(exe)
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string(),
    )
    .chain(names)
    {
        if ["codex", "claude", "pi", "opencode"].contains(&name.as_str()) {
            return Some(name);
        }
    }
    if args
        .get(1)
        .is_some_and(|arg| arg.contains("pi-coding-agent") && arg.ends_with("/cli.js"))
    {
        return Some("pi".into());
    }
    None
}

fn ancestor_pane(
    process: &ProcRow,
    processes: &HashMap<u32, ProcRow>,
    pane_pids: &HashMap<u32, String>,
) -> (Option<String>, bool) {
    let mut current = process.pid;
    let mut seen = HashSet::new();
    let mut nested = false;
    loop {
        if !seen.insert(current) {
            break;
        }
        if let Some(pane) = pane_pids.get(&current) {
            return (Some(pane.clone()), nested);
        }
        let Some(parent) = processes.get(&current) else {
            break;
        };
        if current != process.pid && parent.agent.is_some() {
            nested = true;
        }
        if parent.ppid <= 1 {
            break;
        }
        current = parent.ppid;
    }
    (None, nested)
}

fn session_write(process: &ProcRow, proc_root: &Path, home: &Path) -> Option<f64> {
    let agent = process.agent.as_deref()?;
    let root = match agent {
        "codex" => home.join(".codex/sessions"),
        "claude" => home.join(".claude/projects"),
        "pi" => home.join(".pi/agent/sessions"),
        _ => return None,
    };
    let entries = fs::read_dir(proc_root.join(process.pid.to_string()).join("fd")).ok()?;
    let mut paths = HashSet::new();
    for entry in entries.flatten() {
        let Ok(target) = fs::read_link(entry.path()) else {
            continue;
        };
        if target.extension().is_some_and(|ext| ext == "jsonl") && target.starts_with(&root) {
            paths.insert(target);
        }
    }
    if paths.len() != 1 {
        return None;
    }
    let modified = fs::metadata(paths.into_iter().next()?)
        .ok()?
        .modified()
        .ok()?;
    modified
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs_f64())
}
