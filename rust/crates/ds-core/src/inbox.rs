//! `inbox` (T15, first slice): the schema and the PURE predicates.
//!
//! `dunk_inbox.py` is 888 lines and its test file 1,114 - a session, not a slice -
//! and the store half needs the write convention (`db::SyncWriter`, #187) to be
//! merged so every store after it inherits one shape. This slice is the part that
//! needs neither: the schema as the fixture records it, and the pure functions the
//! SQL is built from (`normalise_mailbox`, `mailbox_where`, the retention cutoff,
//! the counter arithmetic). Everything here is testable against rows and strings.

/// Read + acknowledged + cancelled mail is kept this long, then purged. Unread mail
/// is NEVER purged by age (`RETENTION_S`).
pub const RETENTION_S: f64 = 7.0 * 24.0 * 3600.0;

/// Per-attachment and total blob ceilings, so a runaway agent cannot fill the disk.
pub const MAX_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_BLOB_BYTES: u64 = 2 * 1024 * 1024 * 1024;
pub const MAX_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Message states. `queued` means sitting unread in the mailbox; the typing
/// pipeline's "delivering/sent" has no analogue here because there is no delivery
/// step to fail - the write either happened or it did not.
pub const STATES: &[&str] = &[
    "queued",
    "read",
    "acked",
    "cancelled",
    "archived",
    "undeliverable",
];

/// The state that is never purged by age.
pub const UNREAD: &str = "queued";

/// The inbox schema, VERBATIM from `rust/fixtures/state/schema.json` (T00's snapshot
/// of the live database). A test compares these strings to the fixture, so the port
/// cannot drift from the schema the daemon actually writes.
pub const SCHEMA: &[&str] = &[
    "CREATE TABLE attachments (
    id          TEXT PRIMARY KEY,
    message_id  TEXT NOT NULL,
    filename    TEXT,
    media_type  TEXT,
    size        INTEGER NOT NULL,
    sha256      TEXT NOT NULL,
    created_at  REAL NOT NULL
)",
    "CREATE INDEX attachments_by_message ON attachments (message_id)",
    "CREATE TABLE counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL)",
    "CREATE TABLE deliveries (
    mailbox       TEXT PRIMARY KEY,
    message_ids   TEXT NOT NULL,
    delivered_at  REAL NOT NULL
)",
    "CREATE TABLE messages (
    id            TEXT PRIMARY KEY,
    mailbox       TEXT NOT NULL,
    mailbox_label TEXT,
    pane_id       TEXT,
    sender        TEXT,
    subject       TEXT,
    body          TEXT NOT NULL,
    state         TEXT NOT NULL,
    created_at    REAL NOT NULL,
    read_at       REAL,
    acked_at      REAL,
    cancelled_at  REAL,
    reason        TEXT,
    notify_state  TEXT,
    notify_reason TEXT
, archived_at REAL, digest_id TEXT, digest_updated_at REAL, notify_at REAL, notify_transport_id TEXT, notify_attempt INTEGER, transport TEXT, reply_to TEXT, external_id TEXT, delivered_at REAL, delivery_transport TEXT)",
    "CREATE UNIQUE INDEX messages_by_digest ON messages (digest_id) WHERE digest_id IS NOT NULL",
    "CREATE UNIQUE INDEX messages_by_external ON messages (mailbox, external_id) WHERE external_id IS NOT NULL",
    "CREATE INDEX messages_by_mailbox
    ON messages (mailbox, state, created_at)",
    "CREATE INDEX messages_by_state
    ON messages (state, pane_id)",
    "CREATE INDEX messages_by_transport ON messages (mailbox, transport, created_at)",
];

/// The purge SELECTION, verbatim from `Inbox.purge`: read/acked/cancelled mail older
/// than the cutoff. `state != 'queued'` is the rule that unread mail is never purged,
/// and `COALESCE(...)` is the order in which a row's age is decided.
pub const PURGE_SELECTION: &str = "SELECT id FROM messages WHERE state != 'queued' AND \
    COALESCE(acked_at, cancelled_at, read_at, archived_at, created_at) < ?";

/// The counter upsert (`_next_id`): message ids are stable, never reused, and survive
/// restarts, because the counter lives in the database beside the messages.
pub const COUNTER_UPSERT: &str = "INSERT INTO counters (name, value) VALUES ('message', ?) \
    ON CONFLICT(name) DO UPDATE SET value = ?";

/// `_next_id`'s arithmetic: one more than the stored value, from zero when absent.
pub fn next_message_number(stored: Option<i64>) -> i64 {
    stored.unwrap_or(0) + 1
}

/// The retention cutoff: `now - retention` (seconds since the epoch).
pub fn retention_cutoff(now: f64, retention_s: f64) -> f64 {
    now - retention_s
}

/// `normalise_mailbox`: `'vp of agentry (w9:pH)'` and `'vp of agentry'` are ONE mailbox.
///
/// The table carries both spellings: a channel that resolves with its pane id appends
/// it, and the same mailbox addressed by name does not. Comparing the raw strings made
/// the ack guard refuse the VP's own mail - a guard that refuses valid writes teaches
/// people to work around it, which is worse than the bug it prevents. ONLY a trailing
/// pane-id parenthetical is stripped: a label that merely contains brackets stays intact.
pub fn normalise_mailbox(name: &str) -> String {
    let text = name.trim();
    match strip_pane_suffix(text) {
        Some(stripped) if !stripped.is_empty() => stripped.to_string(),
        _ => text.to_string(),
    }
}

/// `_PANE_SUFFIX`: `\s*\(\s*(?:[A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+|%\d+)\s*\)\s*$` - a
/// TRAILING parenthetical pane id, and nothing else.
///
/// The window half is NOT `w<digits>`: the live mailbox table spells panes as
/// `claude poolrad (wR:p1)`, `PM for Poolrad-decomp (wN:p1)` and `PM (par-a) (wW:p1)`,
/// and a `w\d+` rule left every one of those unnormalised - so a read addressed to the
/// suffixed key missed the rows filed under the plain name, which is the two-spelling
/// rule this function exists for failing in its own docstring (review t995, measured).
fn strip_pane_suffix(text: &str) -> Option<&str> {
    let trimmed = text.trim_end();
    if !trimmed.ends_with(')') {
        return None;
    }
    let open = trimmed.rfind('(')?;
    let inside = trimmed[open + 1..trimmed.len() - 1].trim();
    let is_pane = inside
        .strip_prefix('%')
        .map(|digits| !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
        || matches_pane_id(inside);
    if !is_pane {
        return None;
    }
    Some(trimmed[..open].trim_end())
}

/// `[A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+` - a backend pane id, in the Python's spelling.
fn matches_pane_id(text: &str) -> bool {
    let Some((window, pane)) = text.split_once(":p") else {
        return false;
    };
    let word = |part: &str| {
        !part.is_empty()
            && part
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
    };
    word(window) && word(pane)
}

/// `mailbox_where`: SQL that matches either spelling the table may hold for one mailbox.
///
/// The parameters are `[plain, plain + " (%"]` and the LIKE is escaped, so a mailbox
/// whose name contains `%` or `_` cannot widen its own match.
pub fn mailbox_where(mailbox: &str, column: &str) -> (String, Vec<String>) {
    let plain = normalise_mailbox(mailbox);
    let escaped = plain
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    (
        format!("({column} = ? OR {column} LIKE ? ESCAPE '\\')"),
        vec![plain, format!("{escaped} (%")],
    )
}

#[cfg(test)]
#[path = "inbox_tests.rs"]
mod inbox_tests;

// -- reads (the half that needs no write convention) ---------------------------
//
// `db::ReadPool` is on main (T05, #183): a pool of `query_only` connections that
// never wait for a writer. These functions are the Python's read surface - `count`,
// `list`, `get` - and they take the pool, so the store half that still waits is the
// WRITE one (`send`/`ack`/`purge` through `db::SyncWriter`, #187).

use rusqlite::Connection;

use crate::db::{map_sqlite, ReadPool};
use crate::error::DunkError;

/// A message as the Python's `_summary` renders it: no body, plus the counts a
/// board reads. Field names are the wire's, so a caller ported later keeps them.
#[derive(Debug, Clone, PartialEq)]
pub struct Summary {
    pub message_id: String,
    pub mailbox: String,
    pub destination: String,
    pub sender: Option<String>,
    pub subject: Option<String>,
    pub state: String,
    pub created_at: f64,
    pub waiting_s: Option<i64>,
    pub body_bytes: i64,
    pub attachments: i64,
    // (4) The read half used to DROP these, and the body never said so. `digest_id`
    // comes from the Python's `_summary`; `transport`/`reply_to`/`external_id` are the
    // owner-mailbox fields FLAT_FLEET 7.2 relies on, and `delivered_at` /
    // `delivery_transport` are how a caller sees that a message actually left. A read
    // API that silently narrows the row makes the caller believe the column is empty.
    pub digest_id: Option<String>,
    pub transport: Option<String>,
    pub reply_to: Option<String>,
    pub external_id: Option<String>,
    pub delivered_at: Option<f64>,
    pub delivery_transport: Option<String>,
}

/// `count()`: one mailbox's totals. `unread` is `state = 'queued'`, and cancelled
/// mail is excluded from every number, exactly as the Python excludes it.
#[derive(Debug, Clone, PartialEq)]
pub struct Counts {
    pub mailbox: String,
    pub unread: i64,
    pub total: i64,
    pub archived: i64,
    pub undeliverable: i64,
    pub oldest_unread_s: Option<i64>,
    /// Attachments sitting on queued mail (the Python's `attachments_unread`).
    pub attachments_unread: i64,
    /// Queued mail sent BY a lane, i.e. mail that bypassed the PM. A documented
    /// fleet number that should trend to zero, so a read API that omits it hides a
    /// regression rather than reporting one.
    pub from_lanes: i64,
}

/// `count(mailbox)`: the same arithmetic the Python's single SELECT does.
pub fn count(
    pool: &ReadPool,
    mailbox: &str,
    lane_panes: &[String],
    now: f64,
) -> Result<Counts, DunkError> {
    let (where_clause, params) = mailbox_where(mailbox, "mailbox");
    let sql = format!(
        "SELECT COUNT(*) AS total, SUM(state = 'queued') AS unread, \
         SUM(state = 'archived') AS archived, SUM(state = 'undeliverable') AS undeliverable, \
         MIN(CASE WHEN state = 'queued' THEN created_at END) AS oldest \
         FROM messages WHERE {where_clause} AND state != 'cancelled'"
    );
    pool.with(|connection| {
        let mut statement = connection.prepare(&sql).map_err(map_sqlite)?;
        let row = statement
            .query_row(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((
                    row.get::<_, i64>("total")?,
                    row.get::<_, Option<i64>>("unread")?,
                    row.get::<_, Option<i64>>("archived")?,
                    row.get::<_, Option<i64>>("undeliverable")?,
                    row.get::<_, Option<f64>>("oldest")?,
                ))
            })
            .map_err(map_sqlite)?;
        let (total, unread, archived, undeliverable, oldest) = row;
        // (4) attachments_unread: attachments on QUEUED mail, which is how a caller
        // sees that a message is carrying a file nobody has fetched.
        let blobs_sql = format!(
            "SELECT COUNT(*) FROM attachments a JOIN messages m ON m.id = a.message_id \
             WHERE m.mailbox IN (SELECT mailbox FROM messages WHERE {where_clause}) \
             AND m.state = 'queued'"
        );
        let attachments_unread: i64 = {
            let mut statement = connection.prepare(&blobs_sql).map_err(map_sqlite)?;
            statement
                .query_row(rusqlite::params_from_iter(params.iter()), |row| row.get(0))
                .map_err(map_sqlite)?
        };
        // (4) from_lanes: queued mail whose sender is one of the lane panes. The
        // Python takes `lane_panes=None` and reports 0 in that case; so does this.
        let from_lanes: i64 = if lane_panes.is_empty() {
            0
        } else {
            let marks = std::iter::repeat_n("?", lane_panes.len())
                .collect::<Vec<_>>()
                .join(",");
            let lanes_sql = format!(
                "SELECT COUNT(*) FROM messages WHERE {where_clause} AND state = 'queued' \
                 AND sender IN ({marks})"
            );
            let mut statement = connection.prepare(&lanes_sql).map_err(map_sqlite)?;
            let mut bound: Vec<String> = params.clone();
            bound.extend(lane_panes.iter().cloned());
            statement
                .query_row(rusqlite::params_from_iter(bound.iter()), |row| row.get(0))
                .map_err(map_sqlite)?
        };
        Ok(Counts {
            mailbox: mailbox.to_string(),
            unread: unread.unwrap_or(0),
            total,
            archived: archived.unwrap_or(0),
            undeliverable: undeliverable.unwrap_or(0),
            oldest_unread_s: oldest.map(|at| (now - at) as i64),
            attachments_unread,
            from_lanes,
        })
    })
}

/// The sweep's unread gate (t1053, `Inbox.gate_unread`): `fresh` unread mail may hold a
/// lane back from the sweep; `expired` mail is past the window and the sweep feeds anyway.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gate {
    pub fresh: i64,
    pub expired: i64,
}

/// The Python's `window_s=600.0` default.
pub const GATE_WINDOW_S: f64 = 600.0;

/// `gate_unread(mailbox, window_s, since)`: unread mail that may hold a lane back.
///
/// Daemon announcements (NULL or empty sender) never count: a lane does not read mail
/// until prompted, so counting them deadlocked the fleet. Only mail from another lane or
/// the owner counts, newer than `since` (the lane's last turn end, strictly) and no older
/// than `window_s` (inclusive); past that the sweep feeds anyway.
pub fn gate_unread(
    pool: &ReadPool,
    mailbox: &str,
    window_s: f64,
    since: Option<f64>,
    now: f64,
) -> Result<Gate, DunkError> {
    let (where_clause, params) = mailbox_where(mailbox, "mailbox");
    let cutoff = now - window_s;
    let sql = format!(
        "SELECT created_at FROM messages WHERE {where_clause} AND state = 'queued' \
         AND sender IS NOT NULL AND sender != ''"
    );
    pool.with(|connection| {
        let mut statement = connection.prepare(&sql).map_err(map_sqlite)?;
        let times = statement
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                row.get::<_, f64>(0)
            })
            .map_err(map_sqlite)?
            .collect::<Result<Vec<f64>, _>>()
            .map_err(map_sqlite)?;
        let times: Vec<f64> = times
            .into_iter()
            .filter(|at| since.is_none_or(|since| *at > since))
            .collect();
        let fresh = times.iter().filter(|at| **at >= cutoff).count() as i64;
        Ok(Gate {
            fresh,
            expired: times.len() as i64 - fresh,
        })
    })
}

/// `_gate_unread` (dunk_api.py): the count the sweep's `unread` callback returns, plus
/// whether this pane belongs in `unread_mail_expired` (expired mail and nothing fresh).
pub fn sweep_gate(
    pool: &ReadPool,
    pane_id: &str,
    since: Option<f64>,
    now: f64,
) -> Result<(i64, bool), DunkError> {
    let gate = gate_unread(pool, pane_id, GATE_WINDOW_S, since, now)?;
    Ok((gate.fresh, gate.expired > 0 && gate.fresh == 0))
}

/// `list(mailbox, unread_only, limit)`: summaries, oldest first, no bodies.
pub fn list(
    pool: &ReadPool,
    mailbox: &str,
    unread_only: bool,
    limit: usize,
    now: f64,
) -> Result<Vec<Summary>, DunkError> {
    let (condition, params) = mailbox_where(mailbox, "mailbox");
    let mut where_clause = format!("{condition} AND state != 'cancelled'");
    if unread_only {
        where_clause.push_str(" AND state = 'queued'");
    }
    let sql = format!(
        "SELECT id, mailbox, mailbox_label, sender, subject, state, created_at, \
         length(CAST(body AS BLOB)) AS body_bytes, digest_id, transport, reply_to, \
         external_id, delivered_at, delivery_transport, \
         (SELECT COUNT(*) FROM attachments a WHERE a.message_id = messages.id) AS attachments \
         FROM messages WHERE {where_clause} ORDER BY created_at LIMIT ?"
    );
    pool.with(|connection| {
        let mut statement = connection.prepare(&sql).map_err(map_sqlite)?;
        let mut bound: Vec<rusqlite::types::Value> = params
            .iter()
            .cloned()
            .map(rusqlite::types::Value::Text)
            .collect();
        // Bound as an INTEGER: the reviewer showed SQLite coerces the text form, but a
        // coercion is not a type and `LIMIT` is where a wrong type would be silent.
        bound.push(rusqlite::types::Value::Integer(limit as i64));
        let rows = statement
            .query_map(rusqlite::params_from_iter(bound.iter()), |row| {
                let state: String = row.get("state")?;
                let created_at: f64 = row.get("created_at")?;
                let mailbox_label: Option<String> = row.get("mailbox_label")?;
                let mailbox_name: String = row.get("mailbox")?;
                Ok(Summary {
                    message_id: row.get("id")?,
                    destination: mailbox_label.unwrap_or_else(|| mailbox_name.clone()),
                    mailbox: mailbox_name,
                    sender: row.get("sender")?,
                    subject: row.get("subject")?,
                    waiting_s: if state == UNREAD {
                        Some((now - created_at) as i64)
                    } else {
                        None
                    },
                    created_at,
                    body_bytes: row.get("body_bytes")?,
                    attachments: row.get("attachments")?,
                    digest_id: row.get("digest_id")?,
                    transport: row.get("transport")?,
                    reply_to: row.get("reply_to")?,
                    external_id: row.get("external_id")?,
                    delivered_at: row.get("delivered_at")?,
                    delivery_transport: row.get("delivery_transport")?,
                    state,
                })
            })
            .map_err(map_sqlite)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row.map_err(map_sqlite)?);
        }
        Ok(out)
    })
}

/// `get(id)`: one message with its body, or the Python's named refusal.
pub fn get(pool: &ReadPool, message_id: &str, now: f64) -> Result<(Summary, String), DunkError> {
    let sql = "SELECT id, mailbox, mailbox_label, sender, subject, state, created_at, \
               length(CAST(body AS BLOB)) AS body_bytes, digest_id, transport, reply_to, \
               external_id, delivered_at, delivery_transport, read_at, body, \
               (SELECT COUNT(*) FROM attachments a WHERE a.message_id = messages.id) \
               AS attachments \
               FROM messages WHERE id = ?";
    pool.with(|connection: &Connection| {
        let mut statement = connection.prepare(sql).map_err(map_sqlite)?;
        let mut rows = statement
            .query_map([message_id], |row| {
                let state: String = row.get("state")?;
                let created_at: f64 = row.get("created_at")?;
                let mailbox_label: Option<String> = row.get("mailbox_label")?;
                let mailbox_name: String = row.get("mailbox")?;
                let body: String = row.get("body")?;
                let summary = Summary {
                    message_id: row.get("id")?,
                    destination: mailbox_label.unwrap_or_else(|| mailbox_name.clone()),
                    mailbox: mailbox_name,
                    sender: row.get("sender")?,
                    subject: row.get("subject")?,
                    // (3) `waiting_s` is how long a queued message has waited, not a
                    // constant: the same row returned Some(10800) from list() and
                    // Some(0) from get(), so two readers of one row disagreed about
                    // the number the fleet uses to see a stuck queue.
                    waiting_s: if state == UNREAD {
                        Some((now - created_at).max(0.0) as i64)
                    } else {
                        None
                    },
                    created_at,
                    body_bytes: row.get("body_bytes")?,
                    attachments: row.get("attachments")?,
                    digest_id: row.get("digest_id")?,
                    transport: row.get("transport")?,
                    reply_to: row.get("reply_to")?,
                    external_id: row.get("external_id")?,
                    delivered_at: row.get("delivered_at")?,
                    delivery_transport: row.get("delivery_transport")?,
                    state,
                };
                Ok((summary, body))
            })
            .map_err(map_sqlite)?;
        match rows.next() {
            Some(row) => row.map_err(map_sqlite),
            None => Err(DunkError::Refused(format!(
                "no message with id {message_id:?}"
            ))),
        }
    })
}

// ---- T15a leftovers (t1113): the pure helpers main did not have, from PR #199 ----------
//
// Added to this module rather than as a second `inbox.rs`: main's SCHEMA (the fixture,
// byte for byte), `normalise_mailbox` and `mailbox_where` are the reviewed versions, and
// `is_expired` below is the purge rule `PURGE_SELECTION` states in SQL.

/// A question records where it arrived so the reply goes back the same way
/// (FLAT_FLEET.md 7.2); `None` is a lane message.
pub const TRANSPORTS: [&str; 3] = ["web", "telegram", "report"];

/// The PERMISSIVE pane token the suffix stripper accepts:
/// `[A-Za-z0-9_.-]+:p[A-Za-z0-9_.-]+` or `%<digits>`.
///
/// The Python has two different regular expressions here on purpose: `_PANE_SUFFIX`
/// allows letters after `:p` (a tab label can be `w9:pH`), while `PANE_MAILBOX_RE`
/// requires digits. Conflating them would either refuse to strip a real suffix or
/// accept a non-pane as a mailbox key.
pub fn is_pane_token(value: &str) -> bool {
    if let Some(rest) = value.strip_prefix('%') {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    let Some((workspace, pane)) = value.split_once(":p") else {
        return false;
    };
    let token_char = |c: char| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-');
    !workspace.is_empty()
        && workspace.chars().all(token_char)
        && !pane.is_empty()
        && pane.chars().all(token_char)
}

/// `w<N>:p<N>` or `%<N>`: a mailbox KEY that is itself a pane id (the strict form).
pub fn is_pane_id(value: &str) -> bool {
    if let Some(rest) = value.strip_prefix('%') {
        return !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit());
    }
    // w<digits>:p<digits>
    let Some((workspace, pane)) = value.split_once(":p") else {
        return false;
    };
    let Some(workspace_digits) = workspace.strip_prefix('w') else {
        return false;
    };
    !workspace_digits.is_empty()
        && workspace_digits.chars().all(|c| c.is_ascii_digit())
        && !pane.is_empty()
        && pane.chars().all(|c| c.is_ascii_digit())
}

/// A mailbox key that is ITSELF a pane id: older versions stored dead-pane mail this
/// way, with `pane_id` NULL.
pub fn is_pane_mailbox(value: &str) -> bool {
    // The Python's PANE_MAILBOX_RE is applied to the raw value, so surrounding space makes
    // it NOT a pane mailbox - the strict form is what a mailbox key must be, and a padded
    // key is a caller mistake rather than a pane.
    is_pane_id(value)
}

/// `None` (a lane message) or one of [`TRANSPORTS`]. Only `None` and exactly `""` mean
/// "lane message" (`transport in (None, "")`); a whitespace-only value is NOT empty, it
/// strips to `""` and is refused with `got ''`, like the Python.
pub fn validate_transport(transport: Option<&str>) -> Result<Option<String>, String> {
    let transport = match transport {
        None => return Ok(None),
        Some("") => return Ok(None),
        Some(text) => text.trim().to_lowercase(),
    };
    if TRANSPORTS.contains(&transport.as_str()) {
        Ok(Some(transport))
    } else {
        Err(format!(
            "transport must be one of {}; got '{}'",
            TRANSPORTS.join(", "),
            transport
        ))
    }
}

/// The purge rule: `state != 'queued' AND COALESCE(acked_at, cancelled_at, read_at,
/// archived_at, created_at) < cutoff`. Unread mail is NEVER purged by age, and a row's
/// age is when it was last acted on, not when it arrived.
#[allow(clippy::too_many_arguments)]
pub fn is_expired(
    state: &str,
    created_at: f64,
    read_at: Option<f64>,
    acked_at: Option<f64>,
    archived_at: Option<f64>,
    cancelled_at: Option<f64>,
    now: f64,
    retention_s: f64,
) -> bool {
    if state == "queued" {
        return false;
    }
    let age_from = acked_at
        .or(cancelled_at)
        .or(read_at)
        .or(archived_at)
        .unwrap_or(created_at);
    age_from < retention_cutoff(now, retention_s)
}

/// The size limits, as a named refusal rather than a silent truncation. The texts are
/// the Python's (`body is larger than ...`, the attachment line, the store-full line).
/// `blob_total_bytes` is the store's size AFTER adding this attachment.
pub fn check_sizes(
    body_bytes: usize,
    attachment_name: &str,
    attachment_bytes: u64,
    blob_total_bytes: u64,
) -> Result<(), String> {
    if body_bytes > MAX_BODY_BYTES {
        return Err(format!("body is larger than {MAX_BODY_BYTES} bytes"));
    }
    if attachment_bytes > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "{attachment_name} is {attachment_bytes} bytes, over the {MAX_ATTACHMENT_BYTES} byte attachment limit"
        ));
    }
    if blob_total_bytes > MAX_BLOB_BYTES {
        return Err("attachment store is full; delete some mail first".to_string());
    }
    Ok(())
}
