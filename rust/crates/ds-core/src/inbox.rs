//! The message store, PURE half (T15a): schema, mailbox identity, the query
//! predicate, and the size/retention rules.
//!
//! From `dunk_inbox.py`. This slice deliberately contains no SQLite wiring: it is the
//! part that is a function of its arguments (the Python's `normalise_mailbox`,
//! `mailbox_where`, `validate_transport`, the pane-mailbox regex, and the limits), and it
//! is tested against values exactly as `metrics.rs` (#186) and `panes.rs` (#191) are.
//!
//! The schema is here verbatim because the write path (T15b) and the read path (T15c)
//! both install it - and the same text is what `rust/fixtures/state/schema.json` holds,
//! so a drift between this and the installed database is checkable.

/// 7 days: how long a message survives (`RETENTION_S`).
pub const RETENTION_S: f64 = 7.0 * 24.0 * 3600.0;
pub const MAX_ATTACHMENT_BYTES: i64 = 64 * 1024 * 1024;
pub const MAX_BLOB_BYTES: i64 = 2 * 1024 * 1024 * 1024;
pub const MAX_BODY_BYTES: i64 = 8 * 1024 * 1024;

/// A question records where it arrived so the reply goes back the same way
/// (FLAT_FLEET.md 7.2); `None` is a lane message.
pub const TRANSPORTS: [&str; 3] = ["web", "telegram", "report"];

/// Message states, in the order the Python lists them.
pub const STATES: [&str; 7] = [
    "queued",
    "read",
    "acked",
    "cancelled",
    "archived",
    "hold",
    "failed",
];

/// The schema, verbatim from `dunk_inbox.SCHEMA` up to the messages table.
///
/// Kept as one string because both later slices install it; the attachments,
/// counters and deliveries tables follow the same pattern and are added by T15b with
/// the write methods that need them (a schema this slice cannot exercise would be
/// untested text).
pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS messages (
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
    archived_at   REAL,
    cancelled_at  REAL,
    reason        TEXT,
    notify_state  TEXT,
    notify_reason TEXT,
    digest_id     TEXT,
    digest_updated_at REAL,
    transport     TEXT,
    reply_to      TEXT,
    external_id   TEXT,
    delivery_at   REAL,
    delivery_detail TEXT NOT NULL DEFAULT ''
);";

/// `'vp of agentry (w9:pH)'` and `'vp of agentry'` are ONE mailbox.
///
/// The table carries both spellings: a channel that resolves with its pane id appends
/// it, and the same mailbox addressed by name does not. Comparing the raw strings made
/// the ack guard refuse the VP's own mail - and a guard that refuses valid writes
/// teaches people to work around it, which is worse than the bug it prevents.
///
/// Only a TRAILING pane-id parenthetical is stripped: a label that merely contains
/// brackets stays intact.
pub fn normalise_mailbox(name: Option<&str>) -> Option<String> {
    let text = name?.trim().to_string();
    let stripped = strip_pane_suffix(&text);
    let stripped = stripped.trim();
    if stripped.is_empty() {
        Some(text)
    } else {
        Some(stripped.to_string())
    }
}

/// Strip one trailing `(...)` that holds a pane id (`w9:pH` or `%12`), with the
/// Python's own regular expression: optional space, `(`, the id, `)`, end of string.
fn strip_pane_suffix(text: &str) -> String {
    let trimmed = text.trim_end();
    if !trimmed.ends_with(')') {
        return text.to_string();
    }
    let open = match trimmed.rfind('(') {
        Some(index) => index,
        None => return text.to_string(),
    };
    let inner = trimmed[open + 1..trimmed.len() - 1].trim();
    if is_pane_token(inner) {
        trimmed[..open].trim_end().to_string()
    } else {
        text.to_string()
    }
}

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

/// SQL that matches either spelling the table may hold for one mailbox.
///
/// Returns the predicate and its parameters: `(mailbox = ? OR mailbox LIKE ? ESCAPE '\')`
/// with `[plain, escaped + " (%"]`. The `%`/`_`/`\` in the mailbox name are escaped, so a
/// mailbox called `a_b` does not match `axb`.
pub fn mailbox_where(mailbox: &str, column: &str) -> (String, Vec<String>) {
    let plain = normalise_mailbox(Some(mailbox)).unwrap_or_default();
    let escaped = plain
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    (
        format!("({column} = ? OR {column} LIKE ? ESCAPE '\\')"),
        vec![plain, format!("{escaped} (%")],
    )
}

/// `None` (a lane message) or one of [`TRANSPORTS`].
pub fn validate_transport(transport: Option<&str>) -> Result<Option<String>, String> {
    let transport = match transport {
        None => return Ok(None),
        Some(text) if text.trim().is_empty() => return Ok(None),
        Some(text) => text.trim().to_lowercase(),
    };
    if TRANSPORTS.contains(&transport.as_str()) {
        Ok(Some(transport))
    } else {
        Err(format!(
            "transport must be one of {}; got {:?}",
            TRANSPORTS.join(", "),
            transport
        ))
    }
}

/// A transport's own id for a row: the dedupe key for a redelivered update.
pub fn external_id(transport: &str, value: &str) -> String {
    format!("{transport}:{}", value.trim())
}

/// The retention cutoff for a given `now`: rows older than this are prunable.
pub fn retention_cutoff(now: f64, retention_s: f64) -> f64 {
    now - retention_s
}

/// Is this message old enough to prune?
pub fn is_expired(created_at: f64, now: f64, retention_s: f64) -> bool {
    created_at < retention_cutoff(now, retention_s)
}

/// The size limits, as a named refusal rather than a silent truncation.
pub fn check_sizes(
    body_bytes: i64,
    attachment_bytes: i64,
    blob_total_bytes: i64,
) -> Result<(), String> {
    if body_bytes > MAX_BODY_BYTES {
        return Err(format!(
            "body is {body_bytes} bytes; the limit is {MAX_BODY_BYTES}"
        ));
    }
    if attachment_bytes > MAX_ATTACHMENT_BYTES {
        return Err(format!("attachment is {attachment_bytes} bytes; the limit is                             {MAX_ATTACHMENT_BYTES}"));
    }
    if blob_total_bytes > MAX_BLOB_BYTES {
        return Err(format!("the attachment store would hold {blob_total_bytes} bytes; the                             limit is {MAX_BLOB_BYTES}"));
    }
    Ok(())
}
