//! Production messages backend for the web routes (t3210).
//!
//! Ports the message, delivery, search and attachment behaviour of `web/app.py`
//! (`messages`, `deliveries`, `message_context`, `_post_message`, `send_now`,
//! `post_brief`, `search`, `upload`, `download`) together with the helpers they
//! call (`_message`, `_prompt_for`, `_send_saved`, `_send_to_pane`, `_reconcile*`).
//!
//! Storage is the private application database `<data>/messages.sqlite3` plus the
//! files under `<data>/uploads`. The daemon is reached only through a
//! [`DaemonConnector`] supplied at startup, so tests can drive a fake daemon over a
//! Unix socket without any live service, token or process environment.
//!
//! Observable Python behaviour is preserved: check order, status codes and detail
//! strings, delivery-state transitions, transaction boundaries (a failed write rolls
//! back the message, its attachment links, its answers and the AUTOINCREMENT
//! sequence), and SQLite's failure for Python integers outside signed 64-bit range.
//!
//! Route composition, startup lifecycle and the shared exports are owned by a
//! separate task; this module is not yet declared in `lib.rs`.

use std::{
    collections::HashMap,
    fmt, fs,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, TryLockError},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::http::StatusCode;
use chrono::{DateTime, Utc};
use ds_core::client::{FlockClient, Probe, UnixTransport};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::{types::Value as SqlValue, types::ValueRef, Connection, TransactionBehavior};
use serde_json::{json, Map, Number, Value};

use crate::{
    roster,
    routes::{
        auth::Identity,
        messages_read::{
            DeliveryId, MessageContextFuture, MessagesReadBackend, MessagesReadError,
            MessagesReadFuture, PythonInt,
        },
        messages_write::{
            BriefIn, MessageIn, MessagesWriteBackend, MessagesWriteError, MessagesWriteFuture,
        },
        search_attachments::{
            DownloadFile, DownloadMetadata, SearchAttachmentsBackend, SearchAttachmentsError,
            UploadPart,
        },
    },
};

/// `MAX_UPLOAD` in web/app.py.
const MAX_UPLOAD: usize = 25 * 1024 * 1024;
/// `READ_STATUS_WINDOW_S` in web/app.py: read rows stay reconcilable for eight days.
const READ_STATUS_WINDOW_S: f64 = (8 * 24 * 3600) as f64;
/// `_CAP_TTL_S` in web/app.py: how long a delivery capability answer is reused.
const CAPABILITY_TTL: Duration = Duration::from_secs(20);
/// `sqlite3.connect(DB, timeout=10)` in web/app.py.
const SQLITE_TIMEOUT: Duration = Duration::from_secs(10);
/// `before or 2**63 - 1` in web/app.py: the unbounded default for `before`.
const UNBOUNDED_BEFORE: i64 = i64::MAX;
/// Python's message for binding an int outside SQLite's signed 64-bit range.
const INTEGER_OVERFLOW: &str = "Python int too large to convert to SQLite INTEGER";
/// The uncertain-delivery text written by `_post_message` and `send_now`.
const DELIVERY_UNCONFIRMED: &str =
    "Delivery unconfirmed. Check agent activity before sending again.";

// ---------------------------------------------------------------------------
// Python-compatible primitives
// ---------------------------------------------------------------------------

/// Python `str.isspace()` for one code point: Unicode whitespace plus the
/// information separators U+001C..U+001F, which Python also treats as whitespace.
fn py_isspace(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\u{1c}'..='\u{1f}')
}

/// Python `str.strip()` with no arguments.
fn py_strip(text: &str) -> &str {
    text.trim_matches(py_isspace)
}

/// Python `len(str)`: code points, not bytes.
fn py_len(text: &str) -> usize {
    text.chars().count()
}

/// Python `repr(float)` for the finite values this module formats (file mtimes).
fn py_float_repr(value: f64) -> String {
    format!("{value:?}")
}

/// Python `path.name` on POSIX: the last component after dropping empty and `.` parts.
fn path_name(text: &str) -> &str {
    text.rsplit('/')
        .find(|part| !part.is_empty() && *part != ".")
        .unwrap_or("")
}

/// `uuid.uuid4().hex`: 122 random bits with the RFC 4122 version and variant set.
fn uuid4_hex() -> Result<String, Fault> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| Fault::other("random source unavailable"))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// Python `time.time()`.
fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |since| since.as_secs_f64())
}

/// Python `os.stat(...).st_mtime`: whole seconds plus nanoseconds as one float.
fn stat_mtime(metadata: &fs::Metadata) -> Result<f64, Fault> {
    let since = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .map_err(|error| Fault::other(error.to_string()))?;
    Ok(since.as_secs() as f64 + f64::from(since.subsec_nanos()) * 1e-9)
}

/// `email.utils.formatdate(t, usegmt=True)`: an RFC 7231 IMF-fixdate.
fn http_date(epoch_seconds: f64) -> String {
    DateTime::<Utc>::from_timestamp(epoch_seconds.floor() as i64, 0)
        .map(|moment| moment.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
        .unwrap_or_default()
}

/// MD5 as lowercase hex (RFC 1321). It is used only to reproduce Starlette's
/// `FileResponse` ETag, which hashes `"<st_mtime>-<st_size>"`; it is not a security control.
fn md5_hex(input: &[u8]) -> String {
    const SHIFTS: [u32; 16] = [7, 12, 17, 22, 5, 9, 14, 20, 4, 11, 16, 23, 6, 10, 15, 21];
    const K: [u32; 64] = [
        0xd76a_a478,
        0xe8c7_b756,
        0x2420_70db,
        0xc1bd_ceee,
        0xf57c_0faf,
        0x4787_c62a,
        0xa830_4613,
        0xfd46_9501,
        0x6980_98d8,
        0x8b44_f7af,
        0xffff_5bb1,
        0x895c_d7be,
        0x6b90_1122,
        0xfd98_7193,
        0xa679_438e,
        0x49b4_0821,
        0xf61e_2562,
        0xc040_b340,
        0x265e_5a51,
        0xe9b6_c7aa,
        0xd62f_105d,
        0x0244_1453,
        0xd8a1_e681,
        0xe7d3_fbc8,
        0x21e1_cde6,
        0xc337_07d6,
        0xf4d5_0d87,
        0x455a_14ed,
        0xa9e3_e905,
        0xfcef_a3f8,
        0x676f_02d9,
        0x8d2a_4c8a,
        0xfffa_3942,
        0x8771_f681,
        0x6d9d_6122,
        0xfde5_380c,
        0xa4be_ea44,
        0x4bde_cfa9,
        0xf6bb_4b60,
        0xbebf_bc70,
        0x289b_7ec6,
        0xeaa1_27fa,
        0xd4ef_3085,
        0x0488_1d05,
        0xd9d4_d039,
        0xe6db_99e5,
        0x1fa2_7cf8,
        0xc4ac_5665,
        0xf429_2244,
        0x432a_ff97,
        0xab94_23a7,
        0xfc93_a039,
        0x655b_59c3,
        0x8f0c_cc92,
        0xffef_f47d,
        0x8584_5dd1,
        0x6fa8_7e4f,
        0xfe2c_e6e0,
        0xa301_4314,
        0x4e08_11a1,
        0xf753_7e82,
        0xbd3a_f235,
        0x2ad7_d2bb,
        0xeb86_d391,
    ];
    let mut state: [u32; 4] = [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    let mut message = input.to_vec();
    let bit_length = (input.len() as u64).wrapping_mul(8);
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&bit_length.to_le_bytes());
    for block in message.as_chunks::<64>().0 {
        let words: [u32; 16] = core::array::from_fn(|index| {
            u32::from_le_bytes([
                block[index * 4],
                block[index * 4 + 1],
                block[index * 4 + 2],
                block[index * 4 + 3],
            ])
        });
        let [mut a, mut b, mut c, mut d] = state;
        for round in 0..64usize {
            let (mixed, word_index) = match round / 16 {
                0 => ((b & c) | (!b & d), round),
                1 => ((d & b) | (!d & c), (5 * round + 1) % 16),
                2 => (b ^ c ^ d, (3 * round + 5) % 16),
                _ => (c ^ (b | !d), (7 * round) % 16),
            };
            let shift = SHIFTS[(round / 16) * 4 + round % 4];
            let rotated = a
                .wrapping_add(mixed)
                .wrapping_add(K[round])
                .wrapping_add(words[word_index])
                .rotate_left(shift);
            a = d;
            d = c;
            c = b;
            b = b.wrapping_add(rotated);
        }
        state = [
            state[0].wrapping_add(a),
            state[1].wrapping_add(b),
            state[2].wrapping_add(c),
            state[3].wrapping_add(d),
        ];
    }
    state
        .iter()
        .flat_map(|word| word.to_le_bytes())
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `re.fullmatch(r"pane-[a-z0-9-]+", channel)`.
fn is_pane_channel(channel: &str) -> bool {
    channel.strip_prefix("pane-").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    })
}

// ---------------------------------------------------------------------------
// Faults, daemon boundary and configuration
// ---------------------------------------------------------------------------

/// A Python exception that escapes one backend operation.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Fault {
    /// `raise HTTPException(status, detail)`. Its `str()` is `"{status}: {detail}"`.
    Http { status: u16, detail: String },
    /// Any other exception: the route answers 500, and `str()` is `text`.
    Other { text: String },
}

impl Fault {
    fn http(status: u16, detail: impl Into<String>) -> Self {
        Self::Http {
            status,
            detail: detail.into(),
        }
    }

    fn other(text: impl Into<String>) -> Self {
        Self::Other { text: text.into() }
    }

    /// Python `str(exc)` for the exception this fault models.
    fn text(&self) -> String {
        match self {
            Self::Http { status, detail } => format!("{status}: {detail}"),
            Self::Other { text } => text.clone(),
        }
    }

    /// The SQLite failure for a Python int that cannot be bound as INTEGER.
    fn integer_overflow() -> Self {
        Self::other(INTEGER_OVERFLOW)
    }
}

impl From<rusqlite::Error> for Fault {
    fn from(error: rusqlite::Error) -> Self {
        Self::other(error.to_string())
    }
}

impl From<std::io::Error> for Fault {
    fn from(error: std::io::Error) -> Self {
        Self::other(error.to_string())
    }
}

impl From<serde_json::Error> for Fault {
    fn from(error: serde_json::Error) -> Self {
        Self::other(error.to_string())
    }
}

/// A failed daemon connection or call. `Display` is Python's `str(exc)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonCallError {
    message: String,
}

impl DaemonCallError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for DaemonCallError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for DaemonCallError {}

/// One authenticated daemon session: Python `FlockClient.call(method, **kwargs)`.
pub trait DaemonClient: Send + Sync {
    /// Returns the decoded `result`, or the error the daemon or transport reported.
    fn call(&self, method: &str, params: Map<String, Value>) -> Result<Value, DaemonCallError>;
}

/// Opens daemon sessions. Python calls `FlockClient.connect_or_start()` for each
/// operation, so this is called for each operation too.
pub trait DaemonConnector: Send + Sync {
    fn connect(&self) -> Result<Arc<dyn DaemonClient>, DaemonCallError>;
}

/// One entry of the startup channel registry (`CHANNELS` in web/app.py).
#[derive(Clone, Debug, PartialEq)]
pub struct ChannelConfig {
    /// The channel id (`item["id"]`).
    pub id: String,
    /// The whole registry entry, which the roster's `match_pane` reads.
    pub config: Value,
}

/// Startup-supplied configuration. Nothing is read from the environment here.
pub struct MessagesBackendConfig {
    /// `DATA` in web/app.py: holds `messages.sqlite3` and `uploads/`.
    pub data_dir: PathBuf,
    /// The validated channel registry.
    pub channels: Vec<ChannelConfig>,
    /// The authenticated daemon client factory.
    pub daemon: Arc<dyn DaemonConnector>,
}

/// Failure to open or initialise the backend at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenError {
    message: String,
}

impl fmt::Display for OpenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for OpenError {}

impl From<Fault> for OpenError {
    fn from(fault: Fault) -> Self {
        Self {
            message: fault.text(),
        }
    }
}

impl From<std::io::Error> for OpenError {
    fn from(error: std::io::Error) -> Self {
        Self {
            message: error.to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// SQLite helpers
// ---------------------------------------------------------------------------

/// A row as a column-ordered JSON object: Python `dict(row)`.
type Row = Map<String, Value>;

fn text_of<'a>(row: &'a Row, key: &str) -> Option<&'a str> {
    row.get(key).and_then(Value::as_str)
}

fn int_of(row: &Row, key: &str) -> Result<i64, Fault> {
    row.get(key)
        .and_then(Value::as_i64)
        .ok_or_else(|| Fault::other(format!("{key} is not an integer")))
}

/// Python's `sqlite3` adapter for the JSON scalars this module binds. An int that
/// does not fit in signed 64 bits fails at bind time, exactly like `OverflowError`.
fn bind_json(value: &Value) -> Result<SqlValue, Fault> {
    Ok(match value {
        Value::Null => SqlValue::Null,
        Value::Bool(flag) => SqlValue::Integer(i64::from(*flag)),
        Value::Number(number) => {
            if let Some(integer) = number.as_i64() {
                SqlValue::Integer(integer)
            } else if number.is_f64() {
                SqlValue::Real(number.as_f64().unwrap_or(f64::NAN))
            } else {
                // An integer literal outside i64: Python raises OverflowError here.
                return Err(Fault::integer_overflow());
            }
        }
        Value::String(text) => SqlValue::Text(text.clone()),
        Value::Array(_) | Value::Object(_) => {
            return Err(Fault::other(
                "Error binding parameter 1 - probably unsupported type.",
            ))
        }
    })
}

fn sql_text(text: &str) -> SqlValue {
    SqlValue::Text(text.to_owned())
}

fn sql_int(integer: i64) -> SqlValue {
    SqlValue::Integer(integer)
}

/// Runs a query and returns each row as an ordered JSON object.
fn query_rows(connection: &Connection, sql: &str, params: &[SqlValue]) -> Result<Vec<Row>, Fault> {
    let mut statement = connection.prepare(sql)?;
    let columns: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let mut rows = statement.query(rusqlite::params_from_iter(params.iter()))?;
    let mut out = Vec::new();
    while let Some(row) = rows.next()? {
        let mut object = Row::new();
        for (index, name) in columns.iter().enumerate() {
            let value = match row.get_ref(index)? {
                ValueRef::Null => Value::Null,
                ValueRef::Integer(integer) => Value::from(integer),
                ValueRef::Real(real) => Number::from_f64(real).map_or(Value::Null, Value::Number),
                ValueRef::Text(bytes) => Value::String(String::from_utf8_lossy(bytes).into_owned()),
                ValueRef::Blob(_) => {
                    return Err(Fault::other(
                        "Object of type bytes is not JSON serializable",
                    ))
                }
            };
            object.insert(name.clone(), value);
        }
        out.push(object);
    }
    Ok(out)
}

fn first_row(
    connection: &Connection,
    sql: &str,
    params: &[SqlValue],
) -> Result<Option<Row>, Fault> {
    Ok(query_rows(connection, sql, params)?.into_iter().next())
}

/// Python `json.dumps` for a value stored in `direct_transport`.
fn dump_json(value: &Value) -> Result<String, Fault> {
    Ok(serde_json::to_string(value)?)
}

/// Schema from `_initialize()` in web/app.py, for the `messages` and `attachments`
/// tables only. The `sessions` table belongs to the auth backend.
fn initialize_schema(connection: &Connection) -> Result<(), Fault> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            channel TEXT NOT NULL,
            role TEXT NOT NULL,
            body TEXT NOT NULL,
            delivery TEXT NOT NULL DEFAULT 'posted',
            delivery_detail TEXT NOT NULL DEFAULT '',
            transport_id TEXT,
            transport_attempts INTEGER NOT NULL DEFAULT 0,
            transport_attempt_at REAL NOT NULL DEFAULT 0,
            external_id TEXT,
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
        );
        CREATE INDEX IF NOT EXISTS messages_channel_id ON messages(channel,id);
        CREATE TABLE IF NOT EXISTS attachments (
            id TEXT PRIMARY KEY,
            filename TEXT NOT NULL,
            mime TEXT NOT NULL,
            size INTEGER NOT NULL,
            path TEXT NOT NULL,
            message_id INTEGER REFERENCES messages(id),
            created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
        );
        CREATE INDEX IF NOT EXISTS attachments_message_id ON attachments(message_id);",
    )?;
    let existing: Vec<String> = query_rows(connection, "PRAGMA table_info(messages)", &[])?
        .iter()
        .filter_map(|row| text_of(row, "name").map(str::to_owned))
        .collect();
    for (name, definition) in [
        ("transport_id", "TEXT"),
        ("delivery_detail", "TEXT NOT NULL DEFAULT ''"),
        ("transport_attempts", "INTEGER NOT NULL DEFAULT 0"),
        ("transport_attempt_at", "REAL NOT NULL DEFAULT 0"),
        ("answer_id", "INTEGER"),
        ("answered_at", "TEXT"),
        ("direct_transport", "TEXT"),
        ("delivered_at", "REAL"),
        ("delivery_latency_s", "REAL"),
        ("external_id", "TEXT"),
    ] {
        if !existing.iter().any(|column| column == name) {
            connection.execute_batch(&format!(
                "ALTER TABLE messages ADD COLUMN {name} {definition}"
            ))?;
        }
    }
    connection.execute_batch(
        "CREATE UNIQUE INDEX IF NOT EXISTS messages_external_id ON messages(channel,external_id) WHERE external_id IS NOT NULL",
    )?;
    Ok(())
}

fn text_field<'a>(row: &'a Row, key: &str) -> Result<&'a str, Fault> {
    row.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| Fault::other(format!("{key} is not text")))
}

/// `_message(row, conn)`: the row's columns (without `direct_transport`), the parsed
/// receipt with its `outcome` when one was stored, and the attachment summaries.
fn message_item(connection: &Connection, row: &Row) -> Result<Row, Fault> {
    let mut item = Row::new();
    let mut receipt: Option<&str> = None;
    for (key, value) in row {
        if key == "direct_transport" {
            receipt = value.as_str().filter(|text| !text.is_empty());
            continue;
        }
        item.insert(key.clone(), value.clone());
    }
    if let Some(text) = receipt {
        let transport: Value = serde_json::from_str(text)?;
        let outcome = transport
            .as_object()
            .ok_or_else(|| Fault::other("transport receipt is not an object"))?
            .get("outcome")
            .cloned()
            .unwrap_or(Value::Null);
        item.insert("transport".to_owned(), transport);
        item.insert("outcome".to_owned(), outcome);
    }
    let id = int_of(row, "id")?;
    let attachments = query_rows(
        connection,
        "SELECT id,filename,mime,size FROM attachments WHERE message_id=? ORDER BY created_at",
        &[sql_int(id)],
    )?;
    item.insert(
        "attachments".to_owned(),
        Value::Array(attachments.into_iter().map(Value::Object).collect()),
    );
    Ok(item)
}

/// `_prompt_for(row, conn)`: the text an agent receives for a web message.
fn prompt_for(connection: &Connection, row: &Row) -> Result<String, Fault> {
    let id = int_of(row, "id")?;
    let channel = text_field(row, "channel")?;
    let mut prompt = py_strip(text_field(row, "body")?).to_owned();
    let attachments = query_rows(
        connection,
        "SELECT filename,path FROM attachments WHERE message_id=? ORDER BY created_at",
        &[sql_int(id)],
    )?;
    let mut references = Vec::with_capacity(attachments.len());
    for attachment in &attachments {
        references.push(format!(
            "- {}: {}",
            text_field(attachment, "filename")?,
            text_field(attachment, "path")?
        ));
    }
    if !references.is_empty() {
        prompt.push_str("\n\nAttached files on this host (read these paths directly):\n");
        prompt.push_str(&references.join("\n"));
    }
    prompt.push_str(&format!(
        "\n\n[VP web message #{id}; reply through vpctl brief --channel {channel} --answers {id} when a concise response is ready.]"
    ));
    Ok(prompt)
}

/// Shared state behind the three trait objects.
struct Shared {
    db_path: PathBuf,
    uploads_dir: PathBuf,
    channels: Vec<ChannelConfig>,
    channel_index: HashMap<String, usize>,
    daemon: Arc<dyn DaemonConnector>,
    /// `DELIVERY_LOCK`: held by `send_now` and by reconciliation passes.
    delivery_lock: Mutex<()>,
    /// `_CAP_CACHE`: delivery capability answers keyed by pane id.
    capability_cache: Mutex<HashMap<String, (Instant, Value)>>,
}

/// The messages backend. Clones share all state.
#[derive(Clone)]
pub struct MessagesBackend {
    shared: Arc<Shared>,
}

impl MessagesBackend {
    /// Creates the data directory, the uploads folder and the schema, as the
    /// import-time `_initialize()` and `UPLOADS.mkdir` do in web/app.py.
    pub fn open(config: MessagesBackendConfig) -> Result<Self, OpenError> {
        fs::create_dir_all(&config.data_dir)?;
        let data_dir = fs::canonicalize(&config.data_dir)?;
        let uploads_dir = data_dir.join("uploads");
        fs::create_dir_all(&uploads_dir)?;
        let mut channel_index = HashMap::new();
        for (position, channel) in config.channels.iter().enumerate() {
            if channel_index.insert(channel.id.clone(), position).is_some() {
                return Err(OpenError {
                    message: format!("duplicate channel id {:?}", channel.id),
                });
            }
        }
        let backend = Self {
            shared: Arc::new(Shared {
                db_path: data_dir.join("messages.sqlite3"),
                uploads_dir,
                channels: config.channels,
                channel_index,
                daemon: config.daemon,
                delivery_lock: Mutex::new(()),
                capability_cache: Mutex::new(HashMap::new()),
            }),
        };
        initialize_schema(&backend.connect()?)?;
        Ok(backend)
    }

    /// The messages database path (`DB` in web/app.py).
    pub fn database_path(&self) -> &Path {
        &self.shared.db_path
    }

    /// The three trait objects the route layer mounts, all backed by this state.
    pub fn trait_objects(
        &self,
    ) -> (
        Arc<dyn MessagesReadBackend>,
        Arc<dyn MessagesWriteBackend>,
        Arc<dyn SearchAttachmentsBackend>,
    ) {
        (
            Arc::new(self.clone()),
            Arc::new(self.clone()),
            Arc::new(self.clone()),
        )
    }

    /// A new SQLite connection with the pragmas `_db()` sets in web/app.py.
    fn connect(&self) -> Result<Connection, Fault> {
        let connection = Connection::open(&self.shared.db_path)?;
        connection.busy_timeout(SQLITE_TIMEOUT)?;
        connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        Ok(connection)
    }

    /// `_flock()`: a fresh authenticated daemon session, or HTTP 503 when unavailable.
    fn daemon_client(&self) -> Result<Arc<dyn DaemonClient>, Fault> {
        self.shared
            .daemon
            .connect()
            .map_err(|error| Fault::http(503, format!("Dunking Sheep unavailable: {error}")))
    }

    /// `_known_channel(channel)`.
    fn is_known_channel(&self, channel: &str) -> bool {
        self.shared.channel_index.contains_key(channel) || is_pane_channel(channel)
    }

    /// Takes `DELIVERY_LOCK` as a blocking `with` block would.
    fn lock_delivery(&self) -> std::sync::MutexGuard<'_, ()> {
        self.shared
            .delivery_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `messages(channel, before, limit)` in web/app.py.
    fn read_messages(&self, channel: &str, before: PythonInt, limit: i64) -> Result<Value, Fault> {
        self.reconcile(channel);
        let limit = limit.clamp(1, 100);
        let connection = self.connect()?;
        let before = match before {
            PythonInt::Value(0) => UNBOUNDED_BEFORE,
            PythonInt::Value(value) => value,
            PythonInt::OutOfRange { .. } => return Err(Fault::integer_overflow()),
        };
        let mut rows = query_rows(
            &connection,
            "SELECT * FROM messages WHERE channel=? AND id<? ORDER BY id DESC LIMIT ?",
            &[sql_text(channel), sql_int(before), sql_int(limit + 1)],
        )?;
        let more = rows.len() as i64 > limit;
        rows.truncate(limit as usize);
        let last_delivery = first_row(
            &connection,
            "SELECT id,delivery,delivered_at,delivery_latency_s FROM messages WHERE channel=? AND role='owner' AND delivery='delivered' AND delivered_at IS NOT NULL ORDER BY delivered_at DESC,id DESC LIMIT 1",
            &[sql_text(channel)],
        )?;
        let mut items = Vec::with_capacity(rows.len());
        for row in rows.iter().rev() {
            items.push(Value::Object(message_item(&connection, row)?));
        }
        let next_before = rows
            .last()
            .and_then(|row| row.get("id"))
            .cloned()
            .unwrap_or(Value::Null);
        let mut page = Map::new();
        page.insert("items".to_owned(), Value::Array(items));
        page.insert("has_more".to_owned(), Value::Bool(more));
        page.insert(
            "last_delivery".to_owned(),
            last_delivery.map_or(Value::Null, Value::Object),
        );
        page.insert("next_before".to_owned(), next_before);
        Ok(Value::Object(page))
    }

    /// `deliveries(channel, ids)` in web/app.py.
    fn read_deliveries(&self, channel: &str, ids: &[DeliveryId]) -> Result<Value, Fault> {
        self.reconcile(channel);
        if ids.is_empty() {
            return Ok(json!({"deliveries": {}}));
        }
        let mut params = vec![sql_text(channel)];
        for id in ids {
            match id {
                DeliveryId::Integer(value) => params.push(sql_int(*value)),
                DeliveryId::OutOfRange { .. } => return Err(Fault::integer_overflow()),
            }
        }
        let placeholders = vec!["?"; ids.len()].join(",");
        let sql = format!(
            "SELECT id,delivery,delivery_detail FROM messages WHERE channel=? AND id IN ({placeholders})"
        );
        let connection = self.connect()?;
        let mut deliveries = Map::new();
        for row in query_rows(&connection, &sql, &params)? {
            let id = int_of(&row, "id")?;
            let mut entry = Map::new();
            entry.insert(
                "delivery".to_owned(),
                row.get("delivery").cloned().unwrap_or(Value::Null),
            );
            entry.insert(
                "delivery_detail".to_owned(),
                row.get("delivery_detail").cloned().unwrap_or(Value::Null),
            );
            deliveries.insert(id.to_string(), Value::Object(entry));
        }
        Ok(json!({ "deliveries": deliveries }))
    }

    /// `message_context(message_id)` in web/app.py. `Ok(None)` is "Message not found".
    fn read_message_context(&self, message_id: PythonInt) -> Result<Option<Value>, Fault> {
        let id = match message_id {
            PythonInt::Value(value) => value,
            PythonInt::OutOfRange { .. } => return Err(Fault::integer_overflow()),
        };
        let connection = self.connect()?;
        let Some(anchor) = first_row(
            &connection,
            "SELECT * FROM messages WHERE id=?",
            &[sql_int(id)],
        )?
        else {
            return Ok(None);
        };
        let channel = text_field(&anchor, "channel")?.to_owned();
        self.reconcile(&channel);
        // web/app.py re-reads the anchor after reconciliation, on the same connection.
        let Some(anchor) = first_row(
            &connection,
            "SELECT * FROM messages WHERE id=?",
            &[sql_int(id)],
        )?
        else {
            return Ok(None);
        };
        let earlier = query_rows(
            &connection,
            "SELECT * FROM messages WHERE channel=? AND id<? ORDER BY id DESC LIMIT 25",
            &[sql_text(&channel), sql_int(id)],
        )?;
        let later = query_rows(
            &connection,
            "SELECT * FROM messages WHERE channel=? AND id>? ORDER BY id ASC LIMIT 25",
            &[sql_text(&channel), sql_int(id)],
        )?;
        let rows: Vec<Row> = earlier
            .into_iter()
            .rev()
            .chain(std::iter::once(anchor))
            .chain(later)
            .collect();
        let first_id = int_of(&rows[0], "id")?;
        let has_older = !query_rows(
            &connection,
            "SELECT 1 FROM messages WHERE channel=? AND id<? LIMIT 1",
            &[sql_text(&channel), sql_int(first_id)],
        )?
        .is_empty();
        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            items.push(Value::Object(message_item(&connection, row)?));
        }
        Ok(Some(json!({
            "channel": channel,
            "items": items,
            "has_more": has_older,
            "next_before": first_id,
        })))
    }

    /// `_capability(pane)`: the daemon's delivery capability, or `{}` when it cannot answer.
    fn capability(&self, pane: &str) -> Value {
        let answer = self.daemon_client().ok().and_then(|client| {
            client
                .call("delivery_capability", args([("target", json!(pane))]))
                .ok()
        });
        match answer {
            Some(payload @ Value::Object(_)) => {
                if let Ok(mut cache) = self.shared.capability_cache.lock() {
                    cache.insert(pane.to_owned(), (Instant::now(), payload.clone()));
                }
                payload
            }
            _ => Value::Object(Map::new()),
        }
    }

    /// `_can_message(pane)`: lanes are always messageable; other panes need `agent_alive`.
    fn can_message(&self, pane: Option<&Value>) -> Result<bool, Fault> {
        let Some(pane) = pane else { return Ok(false) };
        let Some(pane_id) = pane
            .get("pane_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        else {
            return Ok(false);
        };
        if roster::is_lane(Some(pane)).map_err(|error| Fault::other(error.to_string()))? {
            return Ok(true);
        }
        let cached = self
            .shared
            .capability_cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(pane_id).cloned())
            .filter(|(at, _)| at.elapsed() < CAPABILITY_TTL);
        let capability = match cached {
            Some((_, payload)) => payload,
            None => self.capability(pane_id),
        };
        Ok(capability.get("agent_alive") == Some(&Value::Bool(true)))
    }

    /// `_target(channel, panes)`: the pane a channel delivers to, or `None`.
    fn target(&self, channel: &str, panes: &[Value]) -> Result<Option<Value>, Fault> {
        if let Some(&position) = self.shared.channel_index.get(channel) {
            let config = &self.shared.channels[position].config;
            return roster::match_pane(config, panes)
                .map_err(|error| Fault::other(error.to_string()));
        }
        for pane in panes {
            let pane_id = pane.get("pane_id").and_then(Value::as_str).unwrap_or("");
            if roster::discovered_id(pane_id) == channel && self.can_message(Some(pane))? {
                return Ok(Some(pane.clone()));
            }
        }
        if self.is_known_channel(channel) {
            return Ok(None);
        }
        Err(Fault::http(404, "Unknown channel"))
    }

    /// `_send_to_pane(pane, text)`: delivers through the route the daemon declares.
    fn send_to_pane(&self, pane: &str, text: &str) -> Result<Value, Fault> {
        if !pane.starts_with('%') {
            return Err(Fault::http(400, "a tmux pane id looks like %9"));
        }
        if text.is_empty() {
            return Err(Fault::http(400, "nothing to send"));
        }
        if py_len(text) > 16000 {
            return Err(Fault::http(400, "keep it under 16000 characters"));
        }
        let capability = self.capability(pane);
        if capability.as_object().is_some_and(Map::is_empty) {
            return Err(Fault::http(502, "the daemon could not describe this lane"));
        }
        if !py_truthy(capability.get("alive")) {
            return Err(Fault::http(
                409,
                format!("{pane} is not alive: nothing to send to"),
            ));
        }
        let declared = capability.get("send").cloned().unwrap_or(Value::Null);
        let route = match declared.as_str() {
            Some(route @ ("owner_push" | "guarded")) => route,
            _ => {
                return Err(Fault::http(
                    409,
                    format!(
                        "{pane} declares no send route ({}); not guessing",
                        py_repr(&declared)
                    ),
                ))
            }
        };
        let client = self.daemon_client()?;
        let call = if route == "owner_push" {
            client.call(
                "owner_push",
                args([("target", json!(pane)), ("body", json!(text))]),
            )
        } else {
            client.call(
                "send_text",
                args([("target", json!(pane)), ("text", json!(text))]),
            )
        };
        let payload =
            call.map_err(|error| Fault::http(502, format!("delivery path unavailable: {error}")))?;
        let payload = if payload.is_object() {
            payload
        } else {
            Value::Object(Map::new())
        };
        let detail = excerpt(
            py_str(&truthy_or(
                payload.get("message"),
                truthy_or(payload.get("outcome"), Value::String(String::new())),
            ))
            .trim_matches(py_isspace),
            400,
        );

        if route == "owner_push" {
            let outcome = py_str(&truthy_or(payload.get("outcome"), json!("")))
                .trim()
                .to_lowercase();
            if outcome.is_empty() {
                return Err(Fault::http(
                    502,
                    format!(
                        "the daemon stored the message but returned no outcome field, so the desk will not invent one: {}",
                        excerpt(&detail, 160)
                    ),
                ));
            }
            return Ok(json!({
                "ok": true,
                "pane": pane,
                "route": route,
                "outcome": outcome,
                "push_id": payload.get("id").cloned().unwrap_or(Value::Null),
                "detail": detail,
                "note": "Stored for the lane to drain; nothing typed into the pane. 'stored' is not 'read' - it has not arrived until the lane takes it.",
            }));
        }

        let outcome = py_str(&truthy_or(payload.get("outcome"), json!("")))
            .trim()
            .to_lowercase();
        let message_id = payload.get("message_id").cloned().unwrap_or(Value::Null);
        match outcome.as_str() {
            "refused" => Ok(json!({
                "ok": false,
                "pane": pane,
                "route": route,
                "outcome": outcome.clone(),
                "message_id": message_id,
                "detail": truthy_or(payload.get("message"), json!(detail)),
            })),
            "" => Err(Fault::http(
                409,
                if detail.is_empty() {
                    "No daemon outcome".to_owned()
                } else {
                    detail
                },
            )),
            "queued" => Ok(json!({
                "ok": true,
                "pane": pane,
                "route": route,
                "outcome": "queued",
                "queued": true,
                "message_id": message_id,
                "detail": detail,
                "note": "Held by the typing guard or not yet delivered. It has NOT arrived - do not read this as delivered.",
            })),
            "unconfirmed" => Ok(json!({
                "ok": true,
                "pane": pane,
                "route": route,
                "outcome": "unconfirmed",
                "unconfirmed": true,
                "message_id": message_id,
                "detail": detail,
                "note": "Neither witness fired. It may well have arrived - do NOT resend blindly; ask the lane first.",
            })),
            "stored" => Ok(json!({
                "ok": true,
                "pane": pane,
                "route": route,
                "outcome": "stored",
                "message_id": message_id,
                "detail": detail,
                "note": "Stored in a durable queue the lane drains itself. That is NOT 'read' - it has not arrived until the lane takes it.",
            })),
            "ok" | "sent" => Ok(json!({
                "ok": true,
                "pane": pane,
                "route": route,
                "outcome": outcome.clone(),
                "message_id": message_id,
                "detail": detail,
                "note": "Confirmed by the box clearing or by the turn it started - that is delivery to the lane, not proof the agent acted on it.",
            })),
            _ => Err(Fault::http(
                502,
                format!(
                    "the sender returned no outcome field, so the desk will not guess: {}",
                    excerpt(&detail, 200)
                ),
            )),
        }
    }

    /// `_send_saved(row, client, pane, force)`: stores the delivery state the send produced.
    fn send_saved(
        &self,
        row: &Row,
        client: &dyn DaemonClient,
        pane: &Value,
        force: bool,
    ) -> Result<Value, Fault> {
        let id = int_of(row, "id")?;
        let pane_id = pane_id_of(pane)?;
        let (prompt, attachment_paths) = {
            let connection = self.connect()?;
            let prompt = prompt_for(&connection, row)?;
            let paths: Vec<Value> = query_rows(
                &connection,
                "SELECT path FROM attachments WHERE message_id=? ORDER BY created_at",
                &[sql_int(id)],
            )?
            .iter()
            .map(|item| item.get("path").cloned().unwrap_or(Value::Null))
            .collect();
            (prompt, paths)
        };
        // A generic model-name mailbox can be shared by unrelated panes, so the daemon
        // addresses the pane itself (web/app.py).
        let mailbox = if force {
            String::new()
        } else {
            py_str(&truthy_or(
                client
                    .call("inbox_count", args([("mailbox", json!(pane_id))]))
                    .map_err(|error| Fault::other(error.to_string()))?
                    .get("mailbox"),
                json!(""),
            ))
        };
        let use_inbox = !mailbox.is_empty()
            && !["codex", "claude", "opencode"].contains(&mailbox.to_lowercase().as_str());
        let (delivery, detail, transport_id, result) = if use_inbox {
            let result = client
                .call(
                    "inbox_send",
                    args([
                        ("to", json!(pane_id)),
                        ("body", json!(prompt)),
                        ("subject", json!(format!("VP web message #{id}"))),
                        ("attach", Value::Array(attachment_paths)),
                        ("notify", json!(true)),
                    ]),
                )
                .map_err(|error| Fault::other(error.to_string()))?;
            let message_id = result.get("message_id").cloned().unwrap_or(json!(""));
            if !message_id
                .as_str()
                .is_some_and(|text| text.starts_with('i'))
            {
                return Err(Fault::other("Inbox send returned no message ID"));
            }
            let delivery = match result.get("state").and_then(Value::as_str) {
                Some("queued") => "inbox",
                Some("read") => "read",
                Some("acked") => "acked",
                _ => "uncertain",
            };
            let detail = match delivery {
                "inbox" => "Stored in agent inbox",
                "read" => "Read by agent",
                "acked" => "Handled by agent",
                _ => "Inbox state unconfirmed",
            };
            (delivery, json!(detail), message_id, result)
        } else {
            let result = client
                .call(
                    "send_text",
                    args([
                        ("target", json!(pane_id)),
                        ("text", json!(prompt)),
                        ("hold_while_typing", json!(!force)),
                        ("wait_s", json!(0)),
                    ]),
                )
                .map_err(|error| Fault::other(error.to_string()))?;
            let delivery = if py_truthy(result.get("queued")) {
                "queued"
            } else if py_truthy(result.get("ok")) {
                "sent"
            } else {
                "uncertain"
            };
            let detail = result.get("message").cloned().unwrap_or_else(|| json!(""));
            let transport_id = result.get("message_id").cloned().unwrap_or(Value::Null);
            (delivery, detail, transport_id, result)
        };
        let connection = self.connect()?;
        connection.execute(
            "UPDATE messages SET delivery=?,delivery_detail=?,transport_id=?,transport_attempts=transport_attempts+1,transport_attempt_at=? WHERE id=?",
            rusqlite::params_from_iter([
                sql_text(delivery),
                bind_json(&detail)?,
                bind_json(&transport_id)?,
                SqlValue::Real(now_epoch()),
                sql_int(id),
            ]),
        )?;
        Ok(result)
    }

    /// Reconcile channels with outstanding receipts, as Python's delivery watcher does.
    /// A failed database or daemon read is retried on the next tick; reconciliation never resends.
    pub(crate) fn reconcile_pending(&self, stopping: &std::sync::atomic::AtomicBool) {
        let Ok(connection) = self.connect() else {
            return;
        };
        let read_cutoff = now_epoch() - READ_STATUS_WINDOW_S;
        let channels = match query_rows(
            &connection,
            "SELECT DISTINCT channel FROM messages WHERE delivery IN ('pending','queued','failed','uncertain','inbox') OR (delivery='read' AND transport_attempt_at>?)",
            &[SqlValue::Real(read_cutoff)],
        ) {
            Ok(rows) => rows.into_iter().filter_map(|row| text_of(&row, "channel").map(str::to_owned)).collect::<Vec<_>>(),
            Err(_) => return,
        };
        drop(connection);
        for channel in channels {
            if stopping.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            self.reconcile(&channel);
        }
    }

    /// `_reconcile(channel)`: refreshes delivery state from the daemon. A pass that finds
    /// the lock held is skipped. Every daemon or database failure ends the pass quietly,
    /// and nothing here ever resends a message.
    fn reconcile(&self, channel: &str) {
        let _guard = match self.shared.delivery_lock.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => return,
        };
        let _ = self.reconcile_passes(channel);
    }

    fn reconcile_passes(&self, channel: &str) -> Result<(), Fault> {
        self.reconcile_owner_pushes(channel)?;
        self.reconcile_direct_guarded(channel)?;
        let read_cutoff = now_epoch() - READ_STATUS_WINDOW_S;
        let connection = self.connect()?;
        let queued = query_rows(
            &connection,
            "SELECT * FROM messages WHERE channel=? AND (delivery IN ('pending','queued','failed','uncertain','inbox') OR (delivery='read' AND transport_attempt_at>?)) ORDER BY id",
            &[sql_text(channel), SqlValue::Real(read_cutoff)],
        )?;
        if queued.is_empty() {
            return Ok(());
        }
        let client = self.daemon_client()?;
        let legacy = queued.iter().any(|row| {
            row.get("direct_transport").is_some_and(Value::is_null) && !starts_with_i(row)
        });
        let recent = if legacy {
            Value::Object(
                client
                    .call("list_messages", Map::new())
                    .map_err(|error| Fault::other(error.to_string()))?
                    .as_object()
                    .cloned()
                    .ok_or_else(|| Fault::other("list_messages result is not an object"))?,
            )
        } else {
            json!({"queued": [], "recent": []})
        };
        let queued_by_id = index_by_message_id(&recent)?;
        let writes = connection;

        for row in &queued {
            if !row.get("direct_transport").is_some_and(Value::is_null) {
                continue; // The daemon receipt is preserved; legacy reconciliation cannot read it.
            }
            let id = int_of(row, "id")?;
            let mut transport_id = row.get("transport_id").cloned().unwrap_or(Value::Null);

            if let Some(text) = transport_id.as_str().filter(|text| !text.is_empty()) {
                if text.starts_with('i') {
                    let Ok(inbox) =
                        client.call("inbox_status", args([("message_id", json!(text))]))
                    else {
                        continue;
                    };
                    let Some(object) = inbox.as_object() else {
                        continue;
                    };
                    let delivery = match object.get("state").and_then(Value::as_str) {
                        Some("queued") => Some("inbox"),
                        Some("read") => Some("read"),
                        Some("acked") => Some("acked"),
                        Some("cancelled") => Some("cancelled"),
                        _ => None,
                    };
                    if let Some(delivery) = delivery {
                        let mut detail = match delivery {
                            "inbox" => "Stored in agent inbox",
                            "read" => "Read by agent",
                            "acked" => "Handled by agent",
                            _ => "Cancelled",
                        }
                        .to_owned();
                        if delivery == "inbox" {
                            let notify =
                                object.get("notify").filter(|value| py_truthy(Some(*value)));
                            let notify_state = match notify {
                                None => None,
                                Some(value) => value
                                    .as_object()
                                    .ok_or_else(|| Fault::other("notify is not an object"))?
                                    .get("state")
                                    .and_then(Value::as_str),
                            };
                            match notify_state {
                                Some("queued") => {
                                    detail.push_str("; wakeup waiting for agent input to clear")
                                }
                                Some("failed") => detail.push_str("; wakeup failed"),
                                Some("sent") => detail.push_str("; agent notified"),
                                _ => {}
                            }
                        }
                        writes.execute(
                            "UPDATE messages SET delivery=?,delivery_detail=? WHERE id=?",
                            rusqlite::params_from_iter([
                                sql_text(delivery),
                                sql_text(&detail),
                                sql_int(id),
                            ]),
                        )?;
                    }
                    continue;
                }
            }

            let mut record: Option<Value> = None;
            if let Some(text) = transport_id.as_str().filter(|text| !text.is_empty()) {
                match client.call("get_message", args([("message_id", json!(text))])) {
                    Ok(value) => record = Some(value),
                    Err(error) if !error.to_string().contains("no message with id") => continue,
                    Err(_) => {}
                }
            }

            if record.is_none() {
                let marker = format!("[VP web message #{id};");
                let mut matches = Vec::new();
                for item in list_items(&recent, "queued")?
                    .into_iter()
                    .chain(list_items(&recent, "recent")?)
                {
                    let object = item
                        .as_object()
                        .ok_or_else(|| Fault::other("message item is not an object"))?;
                    match object.get("text") {
                        None => {}
                        Some(Value::String(text)) => {
                            if text.contains(&marker) {
                                matches.push(item.clone());
                            }
                        }
                        Some(Value::Null) => {
                            return Err(Fault::other("argument of type 'NoneType' is not iterable"))
                        }
                        Some(_) => {}
                    }
                }
                if let Some(chosen) = matches
                    .iter()
                    .find(|item| item.get("status") == Some(&json!("sent")))
                    .or_else(|| matches.first())
                {
                    transport_id = chosen.get("message_id").cloned().unwrap_or(Value::Null);
                    record = Some(chosen.clone());
                }
            }

            if let Some(record) = record.as_ref().filter(|value| py_truthy(Some(*value))) {
                let object = record
                    .as_object()
                    .ok_or_else(|| Fault::other("message record is not an object"))?;
                let state = object.get("status").cloned().unwrap_or(Value::Null);
                if matches!(state.as_str(), Some("queued" | "sent" | "cancelled")) {
                    let mut detail = object.get("message").cloned().unwrap_or_else(|| json!(""));
                    if state == json!("queued") {
                        let blocker = queued_by_id
                            .iter()
                            .find(|(key, _)| *key == transport_id)
                            .and_then(|(_, item)| item.get("blocked_by").cloned());
                        if py_truthy(blocker.as_ref()) {
                            detail = json!(format!(
                                "Waiting: {}",
                                py_str(&blocker.unwrap_or(Value::Null))
                            ));
                        }
                    }
                    writes.execute(
                        "UPDATE messages SET delivery=?,delivery_detail=?,transport_id=? WHERE id=?",
                        rusqlite::params_from_iter([
                            bind_json(&state)?,
                            bind_json(&detail)?,
                            bind_json(&transport_id)?,
                            sql_int(id),
                        ]),
                    )?;
                    continue;
                }
            }

            // A failed or vanished in-memory entry may still have been submitted. Keep its ID
            // and let the owner inspect the pane, because automatic replay caused duplicates.
            writes.execute(
                "UPDATE messages SET delivery='uncertain',delivery_detail=? WHERE id=?",
                rusqlite::params_from_iter([sql_text(DELIVERY_UNCONFIRMED), sql_int(id)]),
            )?;
        }
        Ok(())
    }

    /// `_reconcile_owner_pushes(channel)`: matches durable pickup receipts. An absent
    /// receipt proves nothing.
    fn reconcile_owner_pushes(&self, channel: &str) -> Result<(), Fault> {
        let connection = self.connect()?;
        let rows = query_rows(
            &connection,
            "SELECT * FROM messages WHERE channel=? AND role='owner' AND delivery='stored' AND direct_transport IS NOT NULL AND delivered_at IS NULL ORDER BY id",
            &[sql_text(channel)],
        )?;
        let mut pending: Vec<(Row, Map<String, Value>)> = Vec::new();
        for row in rows {
            let Some(text) = row.get("direct_transport").and_then(Value::as_str) else {
                continue;
            };
            let Ok(Value::Object(transport)) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            let push_id = transport.get("push_id").and_then(Value::as_i64);
            let is_int = transport.get("push_id").is_some_and(|value| value.is_i64());
            let transport_id_matches = row
                .get("transport_id")
                .and_then(Value::as_str)
                .is_some_and(|stored| Some(stored.to_owned()) == push_id.map(|id| id.to_string()));
            if transport.get("route") != Some(&json!("owner_push"))
                || !is_int
                || push_id.is_none_or(|id| id < 1)
                || !transport_id_matches
                || !py_truthy(transport.get("pane"))
            {
                continue;
            }
            pending.push((row, transport));
        }
        if pending.is_empty() {
            return Ok(());
        }
        // Everything below is best-effort: any failure ends the daemon section quietly.
        let _ = self.apply_pickup_receipts(&pending);
        Ok(())
    }

    fn apply_pickup_receipts(&self, pending: &[(Row, Map<String, Value>)]) -> Result<(), Fault> {
        let client = self.daemon_client()?;
        for batch in pending.chunks(100) {
            let push_ids: Vec<Value> = batch
                .iter()
                .map(|(_, transport)| transport.get("push_id").cloned().unwrap_or(Value::Null))
                .collect();
            let response = client
                .call(
                    "owner_receipts",
                    args([("push_ids", Value::Array(push_ids))]),
                )
                .map_err(|error| Fault::other(error.to_string()))?;
            let Some(receipts) = response.get("receipts").and_then(Value::as_array) else {
                return Ok(());
            };
            for (row, transport) in batch {
                let push_id = transport.get("push_id").cloned().unwrap_or(Value::Null);
                let matches: Vec<&Value> = receipts
                    .iter()
                    .filter(|receipt| {
                        receipt.as_object().is_some_and(|object| {
                            object
                                .get("id")
                                .is_some_and(|id| id.is_i64() && *id == push_id)
                        })
                    })
                    .collect();
                if matches.len() != 1 {
                    continue;
                }
                let receipt = matches[0]
                    .as_object()
                    .ok_or_else(|| Fault::other("receipt"))?;
                if receipt.get("pane_id") != transport.get("pane") {
                    continue;
                }
                let (Some(created), Some(taken)) = (
                    positive_number(receipt.get("created_at")),
                    positive_number(receipt.get("taken_at")),
                ) else {
                    continue;
                };
                let started = match row.get("transport_attempt_at").and_then(Value::as_f64) {
                    Some(value) if value.is_finite() && value >= 0.0 => value,
                    _ => continue,
                };
                if taken > now_epoch() {
                    continue;
                }
                let latency = (started > 0.0 && started <= created && created <= taken)
                    .then_some(taken - started);
                let mut detail = "Picked up by agent harness".to_owned();
                if started == 0.0 {
                    detail.push_str("; send timestamp unavailable, elapsed time unavailable");
                } else if latency.is_none() {
                    detail.push_str("; wall clock moved, elapsed time unavailable");
                }
                let mut pickup = Map::new();
                for key in ["id", "pane_id", "created_at", "taken_at"] {
                    let value = receipt
                        .get(key)
                        .cloned()
                        .ok_or_else(|| Fault::other(format!("'{key}'")))?;
                    pickup.insert(key.to_owned(), value);
                }
                let mut updated = transport.clone();
                updated.insert("pickup_receipt".to_owned(), Value::Object(pickup));
                let id = int_of(row, "id")?;
                self.connect()?.execute(
                    "UPDATE messages SET delivery='delivered',delivery_detail=?,delivered_at=?,delivery_latency_s=?,direct_transport=? WHERE id=? AND delivery='stored' AND delivered_at IS NULL",
                    rusqlite::params_from_iter([
                        sql_text(&detail),
                        SqlValue::Real(taken),
                        latency.map_or(SqlValue::Null, SqlValue::Real),
                        sql_text(&dump_json(&Value::Object(updated))?),
                        sql_int(id),
                    ]),
                )?;
            }
        }
        Ok(())
    }

    /// `_reconcile_direct_guarded(channel)`: reconciles guarded direct sends against the
    /// daemon's record for the message id bound at send time. It moves a row only when the
    /// record matches the row's own identity, and it never replays an attempt.
    fn reconcile_direct_guarded(&self, channel: &str) -> Result<(), Fault> {
        let connection = self.connect()?;
        let rows = query_rows(
            &connection,
            "SELECT * FROM messages WHERE channel=? AND role='owner' AND delivery IN ('pending','queued','failed','uncertain') AND direct_transport IS NOT NULL ORDER BY id",
            &[sql_text(channel)],
        )?;
        let mut pending: Vec<(Row, Map<String, Value>, String)> = Vec::new();
        for row in rows {
            let Some(text) = row.get("direct_transport").and_then(Value::as_str) else {
                continue;
            };
            let Ok(Value::Object(transport)) = serde_json::from_str::<Value>(text) else {
                continue;
            };
            if transport.get("route") != Some(&json!("guarded")) {
                continue;
            }
            let bound = py_str(&truthy_or(transport.get("message_id"), json!("")));
            let stored = row
                .get("transport_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            if bound.is_empty() || bound != stored {
                continue;
            }
            pending.push((row, transport, bound));
        }
        if pending.is_empty() {
            return Ok(());
        }
        let Ok(client) = self.daemon_client() else {
            return Ok(());
        };
        for (row, transport, bound) in pending {
            let id = int_of(&row, "id")?;
            let record = match client.call("get_message", args([("message_id", json!(bound))])) {
                Ok(record) => record,
                Err(error) => {
                    if error.to_string().contains("no message with id") {
                        self.connect()?.execute(
                            "UPDATE messages SET delivery='uncertain',delivery_detail=? WHERE id=? AND delivery IN ('pending','queued','failed','uncertain')",
                            rusqlite::params_from_iter([sql_text(UNAVAILABLE_RECEIPT), sql_int(id)]),
                        )?;
                    }
                    continue;
                }
            };
            let Some(object) = record.as_object() else {
                continue;
            };
            if object.get("message_id") != Some(&json!(bound)) {
                continue;
            }
            if object.get("pane_id").unwrap_or(&Value::Null)
                != transport.get("pane").unwrap_or(&Value::Null)
            {
                continue;
            }
            let text = py_str(&truthy_or(object.get("text"), json!("")));
            if !text.contains(&format!("[VP web message #{id};")) {
                continue;
            }
            let Some(state) = object
                .get("status")
                .and_then(Value::as_str)
                .filter(|state| matches!(*state, "queued" | "sent" | "cancelled"))
            else {
                continue;
            };
            let detail = py_str(&truthy_or(object.get("message"), json!(state)));
            let mut moving = vec!["pending", "queued", "failed"];
            if state != "queued" {
                moving.push("uncertain");
            }
            let delivered_at = if state == "sent" {
                positive_number(object.get("finished_at"))
            } else {
                None
            };
            let placeholders = vec!["?"; moving.len()].join(",");
            let mut params = vec![
                sql_text(state),
                sql_text(&detail),
                sql_text(&bound),
                sql_int(id),
            ];
            params.extend(moving.iter().map(|status| sql_text(status)));
            let connection = self.connect()?;
            let changed = connection.execute(
                &format!("UPDATE messages SET delivery=?,delivery_detail=?,transport_id=? WHERE id=? AND delivery IN ({placeholders})"),
                rusqlite::params_from_iter(params.iter()),
            )?;
            if changed > 0 && state == "sent" {
                connection.execute(
                    "UPDATE messages SET delivered_at=?,delivery_latency_s=? WHERE id=? AND delivery='sent' AND delivered_at IS NULL",
                    rusqlite::params_from_iter([
                        delivered_at.map_or(SqlValue::Null, SqlValue::Real),
                        SqlValue::Null,
                        sql_int(id),
                    ]),
                )?;
            }
        }
        Ok(())
    }
}

/// Builds the keyword arguments of one daemon call.
fn args<const N: usize>(pairs: [(&str, Value); N]) -> Map<String, Value> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

/// Python `x or default` for a JSON value.
fn truthy_or(value: Option<&Value>, default: Value) -> Value {
    if py_truthy(value) {
        value.cloned().unwrap_or(default)
    } else {
        default
    }
}

/// Python `str(value)` truncated to `limit` code points, after the caller has stripped it.
fn excerpt(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn starts_with_i(row: &Row) -> bool {
    row.get("transport_id")
        .and_then(Value::as_str)
        .is_some_and(|text| text.starts_with('i'))
}

/// `item.get("queued", [])` style access: a missing list is empty, and a non-list fails.
fn list_items<'a>(recent: &'a Value, key: &str) -> Result<Vec<&'a Value>, Fault> {
    match recent.get(key) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => Ok(items.iter().collect()),
        Some(_) => Err(Fault::other(format!("{key} is not a list"))),
    }
}

/// `{item.get("message_id"): item for item in recent.get("queued", [])}`, last one wins.
fn index_by_message_id(recent: &Value) -> Result<Vec<(Value, Value)>, Fault> {
    let mut index: Vec<(Value, Value)> = Vec::new();
    for item in list_items(recent, "queued")? {
        let object = item
            .as_object()
            .ok_or_else(|| Fault::other("message item is not an object"))?;
        let key = object.get("message_id").cloned().unwrap_or(Value::Null);
        if matches!(key, Value::Array(_) | Value::Object(_)) {
            return Err(Fault::other("unhashable type"));
        }
        match index.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = item.clone(),
            None => index.push((key, item.clone())),
        }
    }
    Ok(index)
}

/// A finite, positive JSON number, as `type(t) in (int, float) and isfinite(t) and t > 0`.
fn positive_number(value: Option<&Value>) -> Option<f64> {
    value
        .and_then(Value::as_f64)
        .filter(|number| number.is_finite() && *number > 0.0)
}

/// Python truthiness of a JSON value; an absent key is falsy.
fn py_truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(flag)) => *flag,
        Some(Value::Number(number)) => number.as_f64().is_none_or(|real| real != 0.0),
        Some(Value::String(text)) => !text.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(object)) => !object.is_empty(),
    }
}

/// Python `str(value)` for a JSON value.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => py_repr(other),
    }
}

/// Python `repr(value)` for a JSON-decoded value.
fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::Number(number) => match number.as_f64() {
            Some(real) if number.is_f64() => py_float_repr(real),
            _ => number.to_string(),
        },
        Value::String(text) => py_str_repr(text),
        Value::Array(items) => format!(
            "[{}]",
            items.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(object) => format!(
            "{{{}}}",
            object
                .iter()
                .map(|(key, value)| format!("{}: {}", py_str_repr(key), py_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// Python `repr(str)`: single quotes unless the text contains only a single quote.
fn py_str_repr(text: &str) -> String {
    let quote = if text.contains('\'') && !text.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(text.len() + 2);
    out.push(quote);
    for character in text.chars() {
        match character {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            other if other == quote => {
                out.push('\\');
                out.push(other);
            }
            other if (other as u32) < 0x20 || other as u32 == 0x7f => {
                out.push_str(&format!("\\x{:02x}", other as u32));
            }
            other => out.push(other),
        }
    }
    out.push(quote);
    out
}

/// The text written when a guarded receipt is no longer held by the daemon.
const UNAVAILABLE_RECEIPT: &str = "Delivery unconfirmed; the transport record for this send is unavailable. Check agent activity before sending again.";

/// Production daemon connector: the `FlockClient` protocol over the daemon's Unix socket,
/// authenticated with a service token supplied at startup. Auto-start is not performed
/// here; a daemon that is absent surfaces as `503 Dunking Sheep unavailable`.
pub struct UnixDaemonConnector {
    socket_path: PathBuf,
    token: Option<String>,
    timeout: Duration,
    identity_note: String,
}

impl UnixDaemonConnector {
    pub fn new(
        socket_path: impl Into<PathBuf>,
        token: Option<String>,
        timeout: Duration,
        identity_note: impl Into<String>,
    ) -> Self {
        Self {
            socket_path: socket_path.into(),
            token,
            timeout,
            identity_note: identity_note.into(),
        }
    }
}

impl DaemonConnector for UnixDaemonConnector {
    fn connect(&self) -> Result<Arc<dyn DaemonClient>, DaemonCallError> {
        let mut session = FlockClient::new(
            UnixTransport {
                path: self.socket_path.clone(),
            },
            None,
            self.identity_note.clone(),
            self.timeout,
            self.token.clone(),
        );
        match session.probe() {
            Probe::Running => Ok(Arc::new(SocketSession {
                session: Mutex::new(session),
            })),
            Probe::Busy => Err(DaemonCallError::new(format!(
                "the dunkingsheep daemon on {} is running but did not answer",
                self.socket_path.display()
            ))),
            Probe::Absent => Err(DaemonCallError::new(format!(
                "the dunkingsheep daemon is not running on {}",
                self.socket_path.display()
            ))),
        }
    }
}

struct SocketSession {
    session: Mutex<FlockClient<UnixTransport>>,
}

impl DaemonClient for SocketSession {
    fn call(&self, method: &str, params: Map<String, Value>) -> Result<Value, DaemonCallError> {
        let mut session = self
            .session
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        session
            .call(method, params)
            .map_err(|error| DaemonCallError::new(error.to_string()))
    }
}

impl MessagesBackend {
    /// `_panes()`: the daemon's pane list.
    fn panes(&self) -> Result<Vec<Value>, Fault> {
        let client = self.daemon_client()?;
        let response = client
            .call("list_panes", Map::new())
            .map_err(|error| Fault::other(error.to_string()))?;
        response
            .get("panes")
            .and_then(Value::as_array)
            .cloned()
            .ok_or_else(|| Fault::other("'panes'"))
    }

    /// The message as the route returns it: `_message(SELECT ..., conn)`.
    fn message_json(&self, id: i64) -> Result<Value, Fault> {
        let connection = self.connect()?;
        let row = first_row(
            &connection,
            "SELECT * FROM messages WHERE id=?",
            &[sql_int(id)],
        )?
        .ok_or_else(|| Fault::other("message row vanished"))?;
        Ok(Value::Object(message_item(&connection, &row)?))
    }

    /// `_post_message(data, identity, direct=True)` in web/app.py.
    fn post_message_impl(&self, data: MessageIn) -> Result<Value, Fault> {
        if !self.is_known_channel(&data.channel) {
            return Err(Fault::http(404, "Unknown channel"));
        }
        if py_strip(&data.body).is_empty() && data.attachment_ids.is_empty() {
            return Err(Fault::http(422, "Message is empty"));
        }
        // Python truthiness: an empty external id is the same as no external id.
        let external_id = data.external_id.as_deref().filter(|id| !id.is_empty());
        if let Some(external_id) = external_id {
            let connection = self.connect()?;
            if let Some(existing) = first_row(
                &connection,
                "SELECT * FROM messages WHERE channel=? AND external_id=?",
                &[sql_text(&data.channel), sql_text(external_id)],
            )? {
                return Ok(Value::Object(message_item(&connection, &existing)?));
            }
        }
        let panes = self.panes()?;
        let pane = self.target(&data.channel, &panes)?;
        let messageable = self.can_message(pane.as_ref())?;
        let pane = match pane {
            Some(pane) if messageable => pane,
            _ => {
                return Err(Fault::http(
                    409,
                    "No active agent is available in this channel",
                ))
            }
        };

        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(external_id) = external_id {
            if let Some(existing) = first_row(
                &transaction,
                "SELECT * FROM messages WHERE channel=? AND external_id=?",
                &[sql_text(&data.channel), sql_text(external_id)],
            )? {
                return Ok(Value::Object(message_item(&transaction, &existing)?));
            }
        }
        let mut attachment_rows = Vec::with_capacity(data.attachment_ids.len());
        for attachment_id in &data.attachment_ids {
            match first_row(
                &transaction,
                "SELECT * FROM attachments WHERE id=? AND message_id IS NULL",
                &[sql_text(attachment_id)],
            )? {
                Some(row) => attachment_rows.push(row),
                None => {
                    return Err(Fault::http(
                        422,
                        format!("Attachment {attachment_id} is unavailable"),
                    ))
                }
            }
        }
        transaction.execute(
            "INSERT INTO messages(channel,role,body,delivery,transport_attempt_at,external_id) VALUES(?,?,?,?,?,?)",
            rusqlite::params_from_iter([
                sql_text(&data.channel),
                sql_text("owner"),
                sql_text(py_strip(&data.body)),
                sql_text("pending"),
                SqlValue::Real(now_epoch()),
                external_id.map_or(SqlValue::Null, sql_text),
            ]),
        )?;
        let message_id = transaction.last_insert_rowid();
        for attachment in &attachment_rows {
            transaction.execute(
                "UPDATE attachments SET message_id=? WHERE id=?",
                rusqlite::params_from_iter([
                    sql_int(message_id),
                    sql_text(text_field(attachment, "id")?),
                ]),
            )?;
        }
        transaction.execute(
            "UPDATE messages SET direct_transport='{}' WHERE id=?",
            rusqlite::params_from_iter([sql_int(message_id)]),
        )?;
        let row = first_row(
            &transaction,
            "SELECT * FROM messages WHERE id=?",
            &[sql_int(message_id)],
        )?
        .ok_or_else(|| Fault::other("message row vanished"))?;
        let prompt = prompt_for(&transaction, &row)?;
        transaction.commit()?;

        // The `try` in web/app.py: any failure in the send or its receipt becomes an
        // uncertain delivery, and its text is reported back in `transport.error`.
        let sent = (|| -> Result<Value, Fault> {
            let pane_id = pane_id_of(&pane)?;
            let result = self.send_to_pane(&pane_id, &prompt)?;
            self.store_direct_receipt(message_id, &result)?;
            Ok(result)
        })();
        let result = match sent {
            Ok(result) => result,
            Err(fault) => {
                self.connect()?.execute(
                    "UPDATE messages SET delivery='uncertain',delivery_detail=?,transport_attempts=transport_attempts+1,transport_attempt_at=? WHERE id=?",
                    rusqlite::params_from_iter([
                        sql_text(DELIVERY_UNCONFIRMED),
                        SqlValue::Real(now_epoch()),
                        sql_int(message_id),
                    ]),
                )?;
                json!({ "error": fault.text() })
            }
        };
        let connection = self.connect()?;
        let row = first_row(
            &connection,
            "SELECT * FROM messages WHERE id=?",
            &[sql_int(message_id)],
        )?
        .ok_or_else(|| Fault::other("message row vanished"))?;
        let mut item = message_item(&connection, &row)?;
        item.insert("transport".to_owned(), result);
        Ok(Value::Object(item))
    }

    /// The receipt `_post_message` stores after a direct send.
    fn store_direct_receipt(&self, message_id: i64, result: &Value) -> Result<(), Fault> {
        let outcome = result
            .get("outcome")
            .cloned()
            .ok_or_else(|| Fault::other("'outcome'"))?;
        let detail = result.get("detail").cloned().unwrap_or_else(|| json!(""));
        let transport_id = truthy_or(
            result.get("push_id"),
            result.get("message_id").cloned().unwrap_or(Value::Null),
        );
        self.connect()?.execute(
            "UPDATE messages SET delivery=?,delivery_detail=?,transport_id=?,direct_transport=?,transport_attempts=1 WHERE id=?",
            rusqlite::params_from_iter([
                bind_json(&outcome)?,
                bind_json(&detail)?,
                bind_json(&transport_id)?,
                sql_text(&dump_json(result)?),
                sql_int(message_id),
            ]),
        )?;
        Ok(())
    }

    /// `send_now(message_id)` in web/app.py. The owner check is in the route.
    fn send_now_impl(&self, message_id: Number) -> Result<Value, Fault> {
        let _delivery = self.lock_delivery();
        let id = message_id.as_i64().ok_or_else(Fault::integer_overflow)?;
        let connection = self.connect()?;
        let Some(row) = first_row(
            &connection,
            "SELECT * FROM messages WHERE id=? AND role='owner'",
            &[sql_int(id)],
        )?
        else {
            return Err(Fault::http(404, "Message not found"));
        };
        let delivery = row.get("delivery").and_then(Value::as_str).unwrap_or("");
        if delivery == "sent" {
            return Ok(Value::Object(message_item(&connection, &row)?));
        }
        let transport_id = row
            .get("transport_id")
            .and_then(Value::as_str)
            .filter(|text| !text.is_empty());
        let Some(transport_id) = transport_id.filter(|_| matches!(delivery, "queued" | "pending"))
        else {
            return Err(Fault::http(
                409,
                "Message is being recovered; try again shortly",
            ));
        };
        let client = self.daemon_client()?;
        let record = client
            .call("get_message", args([("message_id", json!(transport_id))]))
            .map_err(|_| Fault::http(409, "Transport state unavailable; try again shortly"))?;
        if record.get("status") == Some(&json!("sent")) {
            return self.mark_sent(&connection, id);
        }
        if record.get("status") != Some(&json!("queued")) {
            return Err(Fault::http(
                409,
                "Message is no longer queued; refresh its status",
            ));
        }
        let listing = client
            .call("list_messages", Map::new())
            .map_err(|error| Fault::other(error.to_string()))?;
        let mut blocked = Value::Null;
        for item in list_items(&listing, "queued")? {
            let object = item
                .as_object()
                .ok_or_else(|| Fault::other("message item is not an object"))?;
            if object.get("message_id") == Some(&json!(transport_id)) {
                blocked = object.get("blocked_by").cloned().unwrap_or(Value::Null);
                break;
            }
        }
        if !holds_typing_marker(&record)? && blocked != json!("input box contains a draft") {
            return Err(Fault::http(
                409,
                "This message is queued for transport, not held behind a draft",
            ));
        }
        if client
            .call(
                "cancel_message",
                args([("message_id", json!(transport_id))]),
            )
            .is_err()
        {
            // The worker may have sent it between inspection and cancellation.
            let latest = client
                .call("get_message", args([("message_id", json!(transport_id))]))
                .map_err(|error| Fault::other(error.to_string()))?;
            if latest.get("status") == Some(&json!("sent")) {
                return self.mark_sent(&connection, id);
            }
            return Err(Fault::http(
                409,
                "Message changed while sending; refresh its status",
            ));
        }
        let channel = text_field(&row, "channel")?.to_owned();
        let panes = self.panes()?;
        let pane = self.target(&channel, &panes)?;
        let messageable = self.can_message(pane.as_ref())?;
        let pane = match pane {
            Some(pane) if messageable => pane,
            _ => {
                connection.execute(
                    "UPDATE messages SET delivery='uncertain',transport_id=NULL,delivery_detail='Agent pane unavailable after cancellation' WHERE id=?",
                    rusqlite::params_from_iter([sql_int(id)]),
                )?;
                return Err(Fault::http(
                    409,
                    "Agent pane unavailable; delivery is unconfirmed",
                ));
            }
        };
        if self.send_saved(&row, client.as_ref(), &pane, true).is_err() {
            connection.execute(
                "UPDATE messages SET delivery='uncertain',transport_id=NULL,delivery_detail='Urgent send unconfirmed',transport_attempts=transport_attempts+1,transport_attempt_at=? WHERE id=?",
                rusqlite::params_from_iter([SqlValue::Real(now_epoch()), sql_int(id)]),
            )?;
            return Err(Fault::http(
                503,
                "Urgent send unconfirmed; check agent activity before resending",
            ));
        }
        self.message_json(id)
    }

    /// Records a confirmed send and returns the message.
    fn mark_sent(&self, connection: &Connection, id: i64) -> Result<Value, Fault> {
        connection.execute(
            "UPDATE messages SET delivery='sent',delivery_detail='sent' WHERE id=?",
            rusqlite::params_from_iter([sql_int(id)]),
        )?;
        let row = first_row(
            connection,
            "SELECT * FROM messages WHERE id=?",
            &[sql_int(id)],
        )?
        .ok_or_else(|| Fault::other("message row vanished"))?;
        Ok(Value::Object(message_item(connection, &row)?))
    }

    /// `post_brief(data)` in web/app.py. The API-identity check is in the route. The
    /// message, its attachment links and its answers commit together or not at all.
    fn post_brief_impl(&self, data: BriefIn) -> Result<Value, Fault> {
        if !self.is_known_channel(&data.channel) {
            return Err(Fault::http(404, "Unknown channel"));
        }
        let mut connection = self.connect()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for attachment_id in &data.attachment_ids {
            if first_row(
                &transaction,
                "SELECT id FROM attachments WHERE id=? AND message_id IS NULL",
                &[sql_text(attachment_id)],
            )?
            .is_none()
            {
                return Err(Fault::http(
                    422,
                    format!("Attachment {attachment_id} is unavailable"),
                ));
            }
        }
        transaction.execute(
            "INSERT INTO messages(channel,role,body) VALUES(?,?,?)",
            rusqlite::params_from_iter([
                sql_text(&data.channel),
                sql_text("agent"),
                sql_text(py_strip(&data.body)),
            ]),
        )?;
        let message_id = transaction.last_insert_rowid();
        for attachment_id in &data.attachment_ids {
            transaction.execute(
                "UPDATE attachments SET message_id=? WHERE id=?",
                rusqlite::params_from_iter([sql_int(message_id), sql_text(attachment_id)]),
            )?;
        }
        let row = first_row(
            &transaction,
            "SELECT * FROM messages WHERE id=?",
            &[sql_int(message_id)],
        )?
        .ok_or_else(|| Fault::other("message row vanished"))?;
        // First explicit answer wins: `set(data.answers)` with the overflow check at bind time.
        let mut answers = std::collections::BTreeSet::new();
        for answer in &data.answers {
            answers.insert(answer.as_i64().ok_or_else(Fault::integer_overflow)?);
        }
        let null = Value::Null;
        let created_at = bind_json(row.get("created_at").unwrap_or(&null))?;
        for answer in answers {
            transaction.execute(
                "UPDATE messages SET answered_at=?,answer_id=? WHERE id=? AND channel=? AND role='owner' AND answer_id IS NULL AND answered_at IS NULL",
                rusqlite::params_from_iter([
                    created_at.clone(),
                    sql_int(message_id),
                    sql_int(answer),
                    sql_text(&data.channel),
                ]),
            )?;
        }
        let item = message_item(&transaction, &row)?;
        transaction.commit()?;
        Ok(Value::Object(item))
    }

    /// `search(q, channel, before)` in web/app.py. The route supplies the LIKE pattern,
    /// the validated `before` and the row limit (51 for the 50-item page).
    fn search_impl(
        &self,
        pattern: &str,
        channel: Option<&str>,
        before: i64,
        limit: usize,
    ) -> Result<Vec<Value>, Fault> {
        let before = if before == 0 {
            UNBOUNDED_BEFORE
        } else {
            before
        };
        let mut params = vec![sql_text(pattern)];
        let clause = match channel.filter(|value| !value.is_empty()) {
            Some(channel) => {
                params.push(sql_text(channel));
                "AND channel=?"
            }
            None => "",
        };
        params.push(sql_int(before));
        params.push(sql_int(i64::try_from(limit).unwrap_or(i64::MAX)));
        let connection = self.connect()?;
        let rows = query_rows(
            &connection,
            &format!(
                "SELECT * FROM messages WHERE body LIKE ? {clause} AND id<? ORDER BY id DESC LIMIT ?"
            ),
            &params,
        )?;
        let mut items = Vec::with_capacity(rows.len());
        for row in &rows {
            items.push(Value::Object(message_item(&connection, row)?));
        }
        Ok(items)
    }

    /// `upload(file)` in web/app.py: stores the bytes privately and records the attachment.
    fn save_upload_impl(&self, upload: UploadPart) -> Result<Value, Fault> {
        if upload.bytes.len() > MAX_UPLOAD {
            return Err(Fault::http(413, "Files must be 25 MB or smaller"));
        }
        let original = match path_name(&upload.filename) {
            "" => "upload".to_owned(),
            name => name.to_owned(),
        };
        let attachment_id = uuid4_hex()?;
        let path = self
            .shared
            .uploads_dir
            .join(format!("{attachment_id}_{original}"));
        write_private_file(&path, &upload.bytes)?;
        let size = upload.bytes.len();
        self.connect()?.execute(
            "INSERT INTO attachments(id,filename,mime,size,path) VALUES(?,?,?,?,?)",
            rusqlite::params_from_iter([
                sql_text(&attachment_id),
                sql_text(&original),
                sql_text(&upload.storage_mime),
                sql_int(i64::try_from(size).unwrap_or(i64::MAX)),
                sql_text(&path.to_string_lossy()),
            ]),
        )?;
        Ok(json!({
            "id": attachment_id,
            "filename": original,
            "mime": upload.response_mime,
            "size": size,
        }))
    }

    /// `download(attachment_id)` in web/app.py: the stored bytes and the metadata of the
    /// same stat, as Starlette's `FileResponse` derives it.
    fn attachment_impl(&self, id: &str) -> Result<Option<DownloadFile>, Fault> {
        let connection = self.connect()?;
        let Some(row) = first_row(
            &connection,
            "SELECT * FROM attachments WHERE id=?",
            &[sql_text(id)],
        )?
        else {
            return Ok(None);
        };
        let mut file = File::open(text_field(&row, "path")?)?;
        let metadata = file.metadata()?;
        let mtime = stat_mtime(&metadata)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let etag_base = format!("{}-{}", py_float_repr(mtime), metadata.len());
        Ok(Some(DownloadFile {
            filename: text_field(&row, "filename")?.to_owned(),
            mime: text_field(&row, "mime")?.to_owned(),
            bytes,
            metadata: DownloadMetadata {
                content_length: metadata.len(),
                last_modified: http_date(mtime),
                etag: format!("\"{}\"", md5_hex(etag_base.as_bytes())),
            },
        }))
    }
}

/// Writes a new file readable only by its owner (`open(...,"wb")` then `chmod 0o600`).
fn write_private_file(path: &Path, bytes: &[u8]) -> Result<(), Fault> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn pane_id_of(pane: &Value) -> Result<String, Fault> {
    pane.get("pane_id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| Fault::other("'pane_id'"))
}

/// Python `"someone is typing" in record.get("message", "")`.
fn holds_typing_marker(record: &Value) -> Result<bool, Fault> {
    const MARKER: &str = "someone is typing";
    match record.get("message") {
        None => Ok(false),
        Some(Value::String(text)) => Ok(text.contains(MARKER)),
        Some(Value::Array(items)) => Ok(items.iter().any(|item| item.as_str() == Some(MARKER))),
        Some(Value::Object(object)) => Ok(object.contains_key(MARKER)),
        Some(_) => Err(Fault::other("argument is not iterable")),
    }
}

/// Maps a backend fault onto the write routes' error type.
fn write_error(fault: Fault) -> MessagesWriteError {
    match fault {
        Fault::Http { status, detail } => MessagesWriteError::Http {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            detail,
        },
        Fault::Other { .. } => MessagesWriteError::Failure,
    }
}

/// Maps a backend fault onto the search and attachment routes' error type.
fn search_error(fault: Fault) -> SearchAttachmentsError {
    match fault {
        Fault::Http { status, detail } => SearchAttachmentsError::Http {
            status: StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            detail,
        },
        Fault::Other { .. } => SearchAttachmentsError::Failure,
    }
}

/// Runs blocking storage and daemon work off the async executor.
async fn run_blocking<T, F>(work: F) -> Result<T, Fault>
where
    F: FnOnce() -> Result<T, Fault> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(work)
        .await
        .unwrap_or_else(|error| Err(Fault::other(error.to_string())))
}

impl MessagesReadBackend for MessagesBackend {
    fn known_channel(&self, channel: &str) -> bool {
        self.is_known_channel(channel)
    }

    fn messages<'a>(
        &'a self,
        channel: &'a str,
        before: PythonInt,
        limit: i64,
    ) -> MessagesReadFuture<'a> {
        let this = self.clone();
        let channel = channel.to_owned();
        Box::pin(async move {
            run_blocking(move || this.read_messages(&channel, before, limit))
                .await
                .map_err(|_| MessagesReadError)
        })
    }

    fn deliveries<'a>(&'a self, channel: &'a str, ids: &'a [DeliveryId]) -> MessagesReadFuture<'a> {
        let this = self.clone();
        let channel = channel.to_owned();
        let ids = ids.to_vec();
        Box::pin(async move {
            run_blocking(move || this.read_deliveries(&channel, &ids))
                .await
                .map_err(|_| MessagesReadError)
        })
    }

    fn message_context(&self, message_id: PythonInt) -> MessageContextFuture<'_> {
        let this = self.clone();
        Box::pin(async move {
            run_blocking(move || this.read_message_context(message_id))
                .await
                .map_err(|_| MessagesReadError)
        })
    }
}

impl MessagesWriteBackend for MessagesBackend {
    fn post_message<'a>(
        &'a self,
        data: MessageIn,
        _identity: Identity,
    ) -> MessagesWriteFuture<'a, Result<Value, MessagesWriteError>> {
        let this = self.clone();
        Box::pin(async move {
            run_blocking(move || this.post_message_impl(data))
                .await
                .map_err(write_error)
        })
    }

    fn send_now<'a>(
        &'a self,
        message_id: Number,
    ) -> MessagesWriteFuture<'a, Result<Value, MessagesWriteError>> {
        let this = self.clone();
        Box::pin(async move {
            run_blocking(move || this.send_now_impl(message_id))
                .await
                .map_err(write_error)
        })
    }

    fn post_brief<'a>(
        &'a self,
        data: BriefIn,
    ) -> MessagesWriteFuture<'a, Result<Value, MessagesWriteError>> {
        let this = self.clone();
        Box::pin(async move {
            run_blocking(move || this.post_brief_impl(data))
                .await
                .map_err(write_error)
        })
    }
}

impl SearchAttachmentsBackend for MessagesBackend {
    fn known_channel(&self, channel: &str) -> bool {
        self.is_known_channel(channel)
    }

    fn search_messages(
        &self,
        pattern: &str,
        channel: Option<&str>,
        before: i64,
        limit: usize,
    ) -> Result<Vec<Value>, SearchAttachmentsError> {
        self.search_impl(pattern, channel, before, limit)
            .map_err(search_error)
    }

    fn save_upload(&self, upload: UploadPart) -> Result<Value, SearchAttachmentsError> {
        self.save_upload_impl(upload).map_err(search_error)
    }

    fn attachment(&self, id: &str) -> Result<Option<DownloadFile>, SearchAttachmentsError> {
        self.attachment_impl(id).map_err(search_error)
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    /// Runs `python3 -c` and returns its stdout, failing the test if Python is absent.
    fn python(script: &str) -> String {
        let output = std::process::Command::new("python3")
            .arg("-I")
            .arg("-c")
            .arg(script)
            .output()
            .expect("python3 is required for the CPython oracle");
        assert!(
            output.status.success(),
            "python3 failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).expect("python3 printed UTF-8")
    }

    #[test]
    fn md5_matches_rfc_1321_vectors() {
        assert_eq!(md5_hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(
            md5_hex(b"The quick brown fox jumps over the lazy dog"),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn md5_agrees_with_hashlib_across_block_boundaries() {
        let lengths = [0usize, 1, 55, 56, 57, 63, 64, 65, 119, 120, 127, 128, 1000];
        let script = format!(
            "import hashlib\nfor n in {lengths:?}:\n    print(hashlib.md5(('x' * n).encode()).hexdigest())"
        );
        let expected = python(&script);
        let actual: Vec<String> = lengths
            .iter()
            .map(|n| md5_hex("x".repeat(*n).as_bytes()))
            .collect();
        assert_eq!(actual, expected.split_whitespace().collect::<Vec<_>>());
    }

    #[test]
    fn python_isspace_agrees_with_cpython_for_every_code_point() {
        let expected: Vec<u32> =
            python("print(' '.join(str(c) for c in range(0x110000) if chr(c).isspace()))")
                .split_whitespace()
                .map(|value| value.parse().expect("code point"))
                .collect();
        let actual: Vec<u32> = (0..0x110000u32)
            .filter(|&code| char::from_u32(code).is_some_and(py_isspace))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn python_strip_keeps_bom_and_zero_width_space() {
        assert_eq!(py_strip("\u{1c}x\u{1f}"), "x");
        assert_eq!(py_strip("\u{a0} x \u{3000}"), "x");
        assert_eq!(py_strip("\u{feff}x"), "\u{feff}x");
        assert_eq!(py_strip("\u{200b}x"), "\u{200b}x");
        assert_eq!(py_len("a\u{1f600}"), 2);
    }

    #[test]
    fn python_float_repr_matches_cpython_for_file_times() {
        assert_eq!(py_float_repr(1_762_367_112.0), "1762367112.0");
        assert_eq!(py_float_repr(1_762_367_112.5), "1762367112.5");
        assert_eq!(py_float_repr(0.1), "0.1");
    }

    #[test]
    fn path_name_follows_pathlib_on_posix() {
        for (input, expected) in [
            ("a.txt", "a.txt"),
            ("dir/a.txt", "a.txt"),
            ("dir/", "dir"),
            ("..", ".."),
            ("/", ""),
            ("", ""),
            ("x/./y", "y"),
        ] {
            assert_eq!(path_name(input), expected, "{input:?}");
        }
    }

    #[test]
    fn uuid4_hex_sets_version_and_variant() {
        let value = uuid4_hex().expect("random bytes");
        assert_eq!(value.len(), 32);
        assert_eq!(&value[12..13], "4");
        assert!(matches!(&value[16..17], "8" | "9" | "a" | "b"));
    }

    #[test]
    fn python_repr_and_truthiness_for_json_values() {
        assert_eq!(py_repr(&Value::Null), "None");
        assert_eq!(py_repr(&json!("guarded")), "'guarded'");
        assert_eq!(py_repr(&json!("it's")), "\"it's\"");
        assert_eq!(py_repr(&json!(true)), "True");
        assert_eq!(py_str(&json!(5)), "5");
        assert!(!py_truthy(None));
        assert!(!py_truthy(Some(&json!(""))));
        assert!(!py_truthy(Some(&json!(0))));
        assert!(py_truthy(Some(&json!("x"))));
        assert!(py_truthy(Some(&json!({"a": 1}))));
    }

    #[test]
    fn python_int_beyond_signed_64_bits_fails_to_bind() {
        let two_pow_100: Value = serde_json::from_str("1267650600228229401496703205376")
            .expect("arbitrary precision number");
        assert_eq!(bind_json(&two_pow_100), Err(Fault::integer_overflow()));
        let max: Value = serde_json::from_str("9223372036854775807").expect("i64::MAX");
        assert_eq!(bind_json(&max), Ok(SqlValue::Integer(i64::MAX)));
    }

    #[test]
    fn pane_channel_pattern_matches_python_fullmatch() {
        assert!(is_pane_channel("pane-7"));
        assert!(is_pane_channel("pane-a-b-9"));
        assert!(!is_pane_channel("pane-"));
        assert!(!is_pane_channel("pane-A"));
        assert!(!is_pane_channel("pane-7\n"));
        assert!(!is_pane_channel("xpane-7"));
    }
}
