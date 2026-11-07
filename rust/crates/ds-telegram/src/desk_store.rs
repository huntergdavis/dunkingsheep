//! Read-only access to the legacy desk store, `messages.sqlite3`.
//!
//! Two things still live there and the bridge must read them: the watchdog's owner pages (rows
//! with the `PAGE OWNER` marker, in the desk's `messages` table) and the forwarding cursor's
//! channel tail. Writes to that store (attachments, delivery reconciliation) are not ported yet.
//! A missing database is an empty desk, not an error, so a fresh data directory works.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::error::BridgeError;
use crate::policy::PAGE_MARKER;

/// How many legacy rows one tick reads for pages, as `_legacy_pages` does.
pub const PAGE_BATCH: i64 = 20;

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
