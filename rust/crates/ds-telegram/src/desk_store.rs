//! Access to the legacy desk store, `messages.sqlite3`.
//!
//! The bridge reads watchdog owner pages and the forwarding cursor here. Telegram attachments are
//! written here too because the owner-mailbox API does not accept blobs.
//! A missing database is an empty desk, not an error, so a fresh data directory works.
//!
//! `/status` does NOT read this store: this bridge answers it from the owner mailbox, the store it
//! actually writes to (t3671/t3672). The desk store's `delivery` column is Python's source, and
//! that is the one deliberate difference between the two bridges.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior};

use crate::error::BridgeError;
use crate::policy::PAGE_MARKER;

/// How many legacy rows one tick reads for pages, as `_legacy_pages` does.
pub const PAGE_BATCH: i64 = 20;
static ATTACHMENT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedAttachment {
    pub message_id: i64,
    pub delivery: String,
}

fn open(path: &Path) -> Result<Option<Connection>, BridgeError> {
    if !path.exists() {
        return Ok(None);
    }
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map(Some)
    .map_err(|error| BridgeError::Runtime(format!("desk store: {error}")))
}

fn store_error(error: rusqlite::Error) -> BridgeError {
    BridgeError::Runtime(format!("desk store: {error}"))
}

fn attachment_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos() as u64)
        .unwrap_or(0);
    let sequence = ATTACHMENT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{nanos:016x}{sequence:016x}")
}

/// Python `Path(filename).name` and `re.sub(r"[^\\w. ()-]", "_", ...)[:180]`.
fn safe_filename(filename: &str) -> String {
    let basename = Path::new(filename)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("telegram-file");
    let safe: String = basename
        .chars()
        .map(|character| {
            if character.is_alphanumeric() || matches!(character, '_' | '.' | ' ' | '(' | ')' | '-')
            {
                character
            } else {
                '_'
            }
        })
        .take(180)
        .collect();
    if safe.is_empty() {
        "telegram-file".into()
    } else {
        safe
    }
}

/// Persist an inbound Telegram blob and its desk message, matching `_save_attachment` followed by
/// `desk.post_message`. The caller has already applied Telegram's 20 MB download cap.
pub fn post_telegram_attachment(
    database: &Path,
    uploads: &Path,
    channel: &str,
    body: &str,
    external_id: &str,
    filename: &str,
    mime: &str,
    content: &[u8],
) -> Result<PostedAttachment, BridgeError> {
    let filename = safe_filename(filename);
    fs::create_dir_all(uploads).map_err(|error| BridgeError::Os(error.to_string()))?;
    let id = attachment_id();
    let path: PathBuf = uploads.join(format!("{id}_{filename}"));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|error| BridgeError::Os(error.to_string()))?;
    if let Err(error) = file.write_all(content) {
        let _ = fs::remove_file(&path);
        return Err(BridgeError::Os(error.to_string()));
    }
    drop(file);

    let result = (|| {
        let mut conn = Connection::open(database).map_err(store_error)?;
        let tx = conn
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(store_error)?;
        tx.execute(
            "INSERT INTO attachments(id,filename,mime,size,path) VALUES(?,?,?,?,?)",
            rusqlite::params![
                id,
                filename,
                mime,
                content.len() as i64,
                path.to_string_lossy()
            ],
        )
        .map_err(store_error)?;
        tx.execute(
            "INSERT INTO messages(channel,role,body,delivery,external_id) VALUES(?,'owner',?,'posted',?)",
            rusqlite::params![channel, body.trim(), external_id],
        )
        .map_err(store_error)?;
        let message_id = tx.last_insert_rowid();
        tx.execute(
            "UPDATE attachments SET message_id=? WHERE id=?",
            rusqlite::params![message_id, id],
        )
        .map_err(store_error)?;
        tx.commit().map_err(store_error)?;
        Ok(PostedAttachment {
            message_id,
            delivery: "posted".into(),
        })
    })();
    if result.is_err() {
        let _ = fs::remove_file(&path);
    }
    result
}

/// `_latest_brief_id`: the newest agent row id on a channel, or 0.
pub fn latest_agent_row(path: &Path, channel: &str) -> Result<i64, BridgeError> {
    let Some(conn) = open(path)? else {
        return Ok(0);
    };
    conn.query_row(
        "SELECT COALESCE(MAX(id),0) FROM messages WHERE channel=? AND role='agent'",
        [channel],
        |row| row.get(0),
    )
    .map_err(store_error)
}

/// `_page_watermark`'s fallback: the newest row on a channel of any role, or 0.
pub fn newest_row(path: &Path, channel: &str) -> Result<i64, BridgeError> {
    let Some(conn) = open(path)? else {
        return Ok(0);
    };
    let newest: Option<i64> = conn
        .query_row(
            "SELECT COALESCE(MAX(id),0) AS newest FROM messages WHERE channel=?",
            [channel],
            |row| row.get(0),
        )
        .optional()
        .map_err(store_error)?;
    Ok(newest.unwrap_or(0))
}

/// `_legacy_pages`' SQL: agent rows after the watermark whose body carries the page marker, in
/// id order, at most `PAGE_BATCH`. The marker filter is in SQL, not after the LIMIT, so ordinary
/// rows cannot starve a page (t678). The caller still checks `is_page`.
pub fn page_candidates(
    path: &Path,
    channel: &str,
    watermark: i64,
) -> Result<Vec<(i64, String)>, BridgeError> {
    let Some(conn) = open(path)? else {
        return Ok(Vec::new());
    };
    let pattern = format!("%{}%", PAGE_MARKER.to_uppercase());
    let mut statement = conn
        .prepare(
            "SELECT id,body FROM messages WHERE channel=? AND role='agent' AND id>? \
             AND UPPER(body) LIKE ? ORDER BY id LIMIT ?",
        )
        .map_err(store_error)?;
    let rows = statement
        .query_map(
            rusqlite::params![channel, watermark, pattern, PAGE_BATCH],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                ))
            },
        )
        .map_err(store_error)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(store_error)
}
