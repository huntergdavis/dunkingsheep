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

/// `_PANE_SUFFIX`: `'\s*(?:\(\s*(?:w\d+:p[\w.-]+|%\d+)\s*\))\s*$'` - a TRAILING
/// parenthetical pane id, and nothing else.
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

/// `w\d+:p[\w.-]+` - a backend pane id.
fn matches_pane_id(text: &str) -> bool {
    let Some((window, pane)) = text.split_once(":p") else {
        return false;
    };
    let Some(digits) = window.strip_prefix('w') else {
        return false;
    };
    !digits.is_empty()
        && digits.chars().all(|c| c.is_ascii_digit())
        && !pane.is_empty()
        && pane
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
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
