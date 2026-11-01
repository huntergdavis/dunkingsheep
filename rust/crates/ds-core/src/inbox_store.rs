//! `inbox` write path (T15b): `Inbox.send` and `Inbox._store_blob`/`_next_id`, on
//! `db::SyncWriter`.
//!
//! Scope is exactly the task's: one message row per delivery, attachments content-addressed
//! by sha256 with the Python's dedupe, the `message` counter. NOT here: the read
//! aggregations (T15c), upsert_digest/ack/cancel/archive/purge, and the migrations of an
//! OLD database (`ALTER TABLE ... ADD COLUMN`): this opens a database that is either new
//! (the verbatim schema is applied) or already at the current schema.
//!
//! File names deviate from the task text on purpose: `inbox.rs` already exists as a single
//! file, so the store is `inbox_store.rs` beside it rather than a `inbox/` directory move.

use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::clock::now_s;
use crate::db::{map_sqlite, SyncWriter};
use crate::error::DunkError;
use crate::inbox::{
    check_sizes, mailbox_where, next_message_number, normalise_mailbox, receipt_of,
    retention_cutoff, validate_transport, Summary, COUNTER_UPSERT, MAX_BLOB_BYTES, PURGE_SELECTION,
    RETENTION_S, SCHEMA, STATES,
};

/// What `send` stores. Every field but `mailbox` and `body` is optional, as in the Python.
#[derive(Debug, Clone, Default)]
pub struct Outgoing<'a> {
    pub mailbox: &'a str,
    pub body: Option<&'a str>,
    pub subject: Option<&'a str>,
    pub sender: Option<&'a str>,
    pub mailbox_label: Option<&'a str>,
    pub pane_id: Option<&'a str>,
    pub attachments: &'a [PathBuf],
    pub transport: Option<&'a str>,
    pub reply_to: Option<&'a str>,
    pub external_id: Option<&'a str>,
}

/// One stored attachment: the content-addressed blob plus what the row records.
#[derive(Debug, Clone, PartialEq)]
pub struct Blob {
    pub sha256: String,
    pub filename: String,
    pub size: u64,
    pub media_type: String,
}

pub struct Store {
    writer: SyncWriter,
    blob_dir: PathBuf,
}

fn refuse(text: impl Into<String>) -> DunkError {
    DunkError::Refused(text.into())
}

impl Store {
    /// Open (creating) the database and the blob directory. A new database gets the
    /// verbatim schema; an existing one is left alone.
    pub fn open(path: impl AsRef<Path>, blob_dir: impl AsRef<Path>) -> Result<Self, DunkError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| refuse(format!("{}: {e}", parent.display())))?;
        }
        let blob_dir = blob_dir.as_ref().to_path_buf();
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&blob_dir)
            .map_err(|e| refuse(format!("{}: {e}", blob_dir.display())))?;
        let writer = SyncWriter::spawn(path)?;
        writer.call(|connection| {
            let exists: i64 = connection
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'messages'",
                    [],
                    |row| row.get(0),
                )
                .map_err(map_sqlite)?;
            if exists == 0 {
                for statement in SCHEMA {
                    connection.execute_batch(statement).map_err(map_sqlite)?;
                }
            }
            Ok(())
        })?;
        Ok(Store { writer, blob_dir })
    }

    pub fn path(&self) -> &Path {
        self.writer.path()
    }

    /// `send`: put a message in a mailbox. Returns its id (`i<N>`); read the row back with
    /// `inbox::get` on a `ReadPool` (a write here is visible to a fresh pool).
    ///
    /// Blobs are stored BEFORE the row, exactly as the Python does, so a refused
    /// attachment leaves no message behind; a blob stored for a send that then fails is an
    /// orphan the retention sweep collects.
    pub fn send(&self, outgoing: &Outgoing) -> Result<String, DunkError> {
        let mailbox = outgoing.mailbox.trim().to_string();
        if mailbox.is_empty() {
            return Err(refuse("a mailbox is required"));
        }
        let transport = validate_transport(outgoing.transport).map_err(refuse)?;
        let body = outgoing.body.unwrap_or("").to_string();
        check_sizes(body.len(), "", 0, 0).map_err(refuse)?;
        if body.trim().is_empty() && outgoing.attachments.is_empty() {
            return Err(refuse("a message needs a body or an attachment"));
        }
        let mut stored = Vec::new();
        for path in outgoing.attachments {
            stored.push(self.store_blob(path)?);
        }
        // Python: `mailbox_label or mailbox`, so an EMPTY label falls back too.
        let label = outgoing
            .mailbox_label
            .filter(|label| !label.is_empty())
            .unwrap_or(&mailbox)
            .to_string();
        let own = |value: Option<&str>| value.map(str::to_string);
        let (pane_id, sender, subject) = (
            own(outgoing.pane_id),
            own(outgoing.sender),
            own(outgoing.subject),
        );
        let reply_to = own(outgoing.reply_to);
        let external_id = outgoing
            .external_id
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        self.writer.call(move |connection| {
            let transaction = connection.transaction().map_err(map_sqlite)?;
            let message_id = next_message_id(&transaction)?;
            let created = now_s();
            transaction
                .execute(
                    "INSERT INTO messages (id, mailbox, mailbox_label, pane_id, sender, \
                     subject, body, state, created_at, transport, reply_to, external_id) \
                     VALUES (?,?,?,?,?,?,?,'queued',?,?,?,?)",
                    rusqlite::params![
                        message_id,
                        mailbox,
                        label,
                        pane_id,
                        sender,
                        subject,
                        body,
                        created,
                        transport,
                        reply_to,
                        external_id
                    ],
                )
                .map_err(map_sqlite)?;
            for (index, blob) in stored.iter().enumerate() {
                // The attachment ROW id is per message; the blob stays content-addressed
                // and shared (re-attaching one file to another message must not collide).
                transaction
                    .execute(
                        "INSERT INTO attachments (id, message_id, filename, media_type, \
                         size, sha256, created_at) VALUES (?,?,?,?,?,?,?)",
                        rusqlite::params![
                            format!("{message_id}-{}", index + 1),
                            message_id,
                            blob.filename,
                            blob.media_type,
                            blob.size as i64,
                            blob.sha256,
                            now_s()
                        ],
                    )
                    .map_err(map_sqlite)?;
            }
            transaction.commit().map_err(map_sqlite)?;
            Ok(message_id)
        })
    }

    /// `_store_blob`: hash the file, keep ONE copy per sha256 under the blob dir (mode
    /// 0600: t739's leak is the warning), refuse an oversize file or a full store.
    pub fn store_blob(&self, path: &Path) -> Result<Blob, DunkError> {
        let path = expand_user(path);
        if !path.is_file() {
            return Err(refuse(format!(
                "no such file to attach: {}",
                path.display()
            )));
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let size = fs::metadata(&path)
            .map_err(|e| refuse(format!("{}: {e}", path.display())))?
            .len();
        check_sizes(0, &name, size, 0).map_err(refuse)?;
        let bytes = fs::read(&path).map_err(|e| refuse(format!("{}: {e}", path.display())))?;
        let sha = Sha256::digest(&bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let blob_path = self.blob_dir.join(&sha);
        if !blob_path.exists() {
            check_sizes(0, "", 0, self.blob_bytes() + size).map_err(refuse)?;
            debug_assert!(MAX_BLOB_BYTES > 0);
            // Write beside the target and rename, so a crash never leaves a half blob
            // under its hash (a later send would then dedupe onto a truncated file).
            let partial = self.blob_dir.join(format!(".{sha}.partial"));
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&partial)
                .map_err(|e| refuse(format!("{}: {e}", partial.display())))?;
            file.write_all(&bytes)
                .and_then(|()| file.sync_all())
                .map_err(|e| refuse(format!("{}: {e}", partial.display())))?;
            fs::rename(&partial, &blob_path)
                .map_err(|e| refuse(format!("{}: {e}", blob_path.display())))?;
        }
        Ok(Blob {
            sha256: sha,
            filename: name,
            size,
            media_type: media_type(&path),
        })
    }

    /// `_blob_bytes`: the size of everything in the blob dir (unreadable entries count 0).
    pub fn blob_bytes(&self) -> u64 {
        fs::read_dir(&self.blob_dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    // a leftover `.<sha>.partial` is not a stored blob
                    .filter(|entry| !entry.file_name().to_string_lossy().ends_with(".partial"))
                    .filter_map(|entry| entry.metadata().ok())
                    .map(|meta| meta.len())
                    .sum()
            })
            .unwrap_or(0)
    }

    /// `upsert_digest`: ONE message per digest id, updated in place (a PM publishes a digest
    /// every window; it must not become a stream). An update replaces the body and makes
    /// the row unread again (read/ack/archive/cancel stamps and the notify state cleared);
    /// `created_at` and the id stay. Returns the message id.
    pub fn upsert_digest(
        &self,
        mailbox: &str,
        digest_id: &str,
        body: &str,
        subject: Option<&str>,
        sender: Option<&str>,
        mailbox_label: Option<&str>,
        pane_id: Option<&str>,
    ) -> Result<String, DunkError> {
        if body.trim().is_empty() {
            return Err(refuse("a digest needs a body"));
        }
        check_sizes(body.len(), "", 0, 0).map_err(refuse)?;
        let (mailbox, digest_id, body) =
            (mailbox.to_string(), digest_id.to_string(), body.to_string());
        let label = mailbox_label
            .filter(|l| !l.is_empty())
            .unwrap_or(&mailbox)
            .to_string();
        let own = |value: Option<&str>| value.map(str::to_string);
        let (subject, sender, pane_id) = (own(subject), own(sender), own(pane_id));
        self.writer.call(move |connection| {
            let transaction = connection.transaction().map_err(map_sqlite)?;
            let existing: Option<String> = transaction
                .query_row(
                    "SELECT id FROM messages WHERE digest_id = ?",
                    [&digest_id],
                    |row| row.get(0),
                )
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
                .map_err(map_sqlite)?;
            let now = now_s();
            let message_id = if let Some(id) = existing {
                transaction
                    .execute(
                        "UPDATE messages SET mailbox=?, mailbox_label=?, pane_id=?, sender=?, \
                         subject=?, body=?, state='queued', read_at=NULL, acked_at=NULL, \
                         archived_at=NULL, cancelled_at=NULL, reason=NULL, notify_state=NULL, \
                         notify_reason=NULL, digest_updated_at=? WHERE digest_id=?",
                        rusqlite::params![
                            mailbox, label, pane_id, sender, subject, body, now, digest_id
                        ],
                    )
                    .map_err(map_sqlite)?;
                id
            } else {
                let id = next_message_id(&transaction)?;
                transaction
                    .execute(
                        "INSERT INTO messages (id, mailbox, mailbox_label, pane_id, sender, \
                         subject, body, state, created_at, digest_id, digest_updated_at) \
                         VALUES (?,?,?,?,?,?,?,'queued',?,?,?)",
                        rusqlite::params![
                            id, mailbox, label, pane_id, sender, subject, body, now, digest_id, now
                        ],
                    )
                    .map_err(map_sqlite)?;
                id
            };
            transaction.commit().map_err(map_sqlite)?;
            Ok(message_id)
        })
    }

    /// `read(mailbox, limit, peek, mark)`: full messages in queue order. Marks them read
    /// unless `peek` or `!mark`; the select and the update share one writer call, as the
    /// Python shares one lock. `mark = false` lets a caller render first and commit with
    /// `mark_read` (VP t93: a reader that crashed mid-render had already consumed the batch).
    pub fn read(
        &self,
        mailbox: &str,
        limit: usize,
        peek: bool,
        mark: bool,
    ) -> Result<Vec<Message>, DunkError> {
        let mailbox = mailbox.to_string();
        self.writer.call(move |connection| {
            let now = now_s();
            let mut messages = {
                let mut statement = connection
                    .prepare(&format!(
                        "SELECT {FULL_COLUMNS} FROM messages WHERE mailbox = ? AND \
                         state = 'queued' ORDER BY created_at LIMIT ?"
                    ))
                    .map_err(map_sqlite)?;
                let rows = statement
                    .query_map(rusqlite::params![mailbox, limit as i64], |row| {
                        full_from_row(row, now)
                    })
                    .map_err(map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)?
            };
            for message in &mut messages {
                message.attachments = attachments_of(connection, &message.summary.message_id)?;
            }
            if !peek && mark && !messages.is_empty() {
                let tx = connection.transaction().map_err(map_sqlite)?;
                for message in &mut messages {
                    tx.execute(
                        "UPDATE messages SET state = 'read', read_at = ? \
                         WHERE id = ? AND state = 'queued'",
                        rusqlite::params![now, message.summary.message_id],
                    )
                    .map_err(map_sqlite)?;
                    message.summary.state = "read".to_string();
                    // waiting_s stays what the queued row computed (t1226): Python returns
                    // how long the message waited even though its state is now 'read'.
                    message.read_at = Some(now);
                }
                tx.commit().map_err(map_sqlite)?;
            }
            Ok(messages)
        })
    }

    /// `mark_read(ids, now)`: commit the read for ids already rendered. Idempotent and
    /// narrow: only rows still `queued` move. Returns how many rows changed.
    pub fn mark_read(&self, message_ids: &[String], now: Option<f64>) -> Result<usize, DunkError> {
        let ids: Vec<String> = message_ids.to_vec();
        if ids.is_empty() {
            return Ok(0);
        }
        let now = now.unwrap_or_else(now_s);
        self.writer.call(move |connection| {
            let tx = connection.transaction().map_err(map_sqlite)?;
            let mut changed = 0;
            for id in &ids {
                changed += tx
                    .execute(
                        "UPDATE messages SET state = 'read', read_at = ? \
                         WHERE id = ? AND state = 'queued'",
                        rusqlite::params![now, id],
                    )
                    .map_err(map_sqlite)?;
            }
            tx.commit().map_err(map_sqlite)?;
            Ok(changed)
        })
    }

    /// `record_delivery`: remember the latest delivered batch for a mailbox (ONE batch per
    /// mailbox; a newer read overwrites it). Blank ids are dropped; an empty batch records
    /// nothing and leaves an existing receipt alone. Returns the number of ids recorded.
    pub fn record_delivery(
        &self,
        mailbox: &str,
        message_ids: &[String],
        now: Option<f64>,
    ) -> Result<usize, DunkError> {
        let ids: Vec<String> = message_ids
            .iter()
            .filter(|id| !id.trim().is_empty())
            .cloned()
            .collect();
        if ids.is_empty() {
            return Ok(0);
        }
        let now = now.unwrap_or_else(now_s);
        let mailbox = mailbox.to_string();
        // Python's `json.dumps(list)` writes `["a", "b"]` (comma + space); serde's compact
        // form would differ byte-for-byte in the column, so match it.
        let encoded = json_list(&ids);
        let count = ids.len();
        self.writer.call(move |connection| {
            connection
                .execute(
                    "INSERT INTO deliveries (mailbox, message_ids, delivered_at) VALUES (?,?,?) \
                     ON CONFLICT(mailbox) DO UPDATE SET message_ids = excluded.message_ids, \
                     delivered_at = excluded.delivered_at",
                    rusqlite::params![mailbox, encoded, now],
                )
                .map_err(map_sqlite)?;
            Ok(count)
        })
    }

    /// `recover_delivery`: replay the receipt batch whatever its read state. Only ids that
    /// still exist come back, in queue order; vanished ids shrink the receipt, and a
    /// receipt with no survivors is dropped. No receipt answers empty, not an error.
    pub fn recover_delivery(&self, mailbox: &str) -> Result<Vec<Message>, DunkError> {
        let mailbox = mailbox.to_string();
        self.writer.call(move |connection| {
            let Some(receipt) = receipt_of(connection, &mailbox)? else {
                return Ok(Vec::new());
            };
            let now = now_s();
            let placeholders = vec!["?"; receipt.message_ids.len()].join(",");
            let mut messages = {
                let mut statement = connection
                    .prepare(&format!(
                        "SELECT {FULL_COLUMNS} FROM messages WHERE id IN ({placeholders}) \
                         ORDER BY created_at"
                    ))
                    .map_err(map_sqlite)?;
                let rows = statement
                    .query_map(
                        rusqlite::params_from_iter(receipt.message_ids.iter()),
                        |row| full_from_row(row, now),
                    )
                    .map_err(map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)?
            };
            for message in &mut messages {
                message.attachments = attachments_of(connection, &message.summary.message_id)?;
            }
            let survivors: Vec<String> = messages
                .iter()
                .map(|m| m.summary.message_id.clone())
                .collect();
            let wanted: std::collections::HashSet<&String> = receipt.message_ids.iter().collect();
            let have: std::collections::HashSet<&String> = survivors.iter().collect();
            if wanted != have {
                if survivors.is_empty() {
                    connection
                        .execute("DELETE FROM deliveries WHERE mailbox = ?", [&mailbox])
                        .map_err(map_sqlite)?;
                } else {
                    connection
                        .execute(
                            "UPDATE deliveries SET message_ids = ? WHERE mailbox = ?",
                            rusqlite::params![json_list(&survivors), mailbox],
                        )
                        .map_err(map_sqlite)?;
                }
            }
            Ok(messages)
        })
    }

    /// `note_acked`: shrink the receipt to ids that are still not acked. A fully acked
    /// receipt is deleted. Returns the ids still pending.
    pub fn note_acked(&self, mailbox: &str) -> Result<Vec<String>, DunkError> {
        let mailbox = mailbox.to_string();
        self.writer.call(move |connection| {
            let Some(receipt) = receipt_of(connection, &mailbox)? else {
                return Ok(Vec::new());
            };
            let placeholders = vec!["?"; receipt.message_ids.len()].join(",");
            let remaining: Vec<String> = {
                let mut statement = connection
                    .prepare(&format!(
                        "SELECT id, state FROM messages WHERE id IN ({placeholders})"
                    ))
                    .map_err(map_sqlite)?;
                let rows = statement
                    .query_map(
                        rusqlite::params_from_iter(receipt.message_ids.iter()),
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .map_err(map_sqlite)?;
                let mut out = Vec::new();
                for row in rows {
                    let (id, state) = row.map_err(map_sqlite)?;
                    if state != "acked" {
                        out.push(id);
                    }
                }
                out
            };
            if remaining.is_empty() {
                connection
                    .execute("DELETE FROM deliveries WHERE mailbox = ?", [&mailbox])
                    .map_err(map_sqlite)?;
            } else {
                connection
                    .execute(
                        "UPDATE deliveries SET message_ids = ? WHERE mailbox = ?",
                        rusqlite::params![json_list(&remaining), mailbox],
                    )
                    .map_err(map_sqlite)?;
            }
            Ok(remaining)
        })
    }

    /// `purge(older_than_s)`: drop read, acknowledged and cancelled mail past its
    /// retention (default `RETENTION_S`), then the blobs nothing references. Unread mail is
    /// never purged by age. Returns the number of messages purged.
    pub fn purge(&self, older_than_s: Option<f64>) -> Result<usize, DunkError> {
        let cutoff = retention_cutoff(now_s(), older_than_s.unwrap_or(RETENTION_S));
        let purged = self.writer.call(move |connection| {
            let ids: Vec<String> = {
                let mut statement = connection.prepare(PURGE_SELECTION).map_err(map_sqlite)?;
                let rows = statement
                    .query_map([cutoff], |row| row.get::<_, String>(0))
                    .map_err(map_sqlite)?;
                rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)?
            };
            if !ids.is_empty() {
                let tx = connection.transaction().map_err(map_sqlite)?;
                for id in &ids {
                    tx.execute("DELETE FROM attachments WHERE message_id = ?", [id])
                        .map_err(map_sqlite)?;
                    tx.execute("DELETE FROM messages WHERE id = ?", [id])
                        .map_err(map_sqlite)?;
                }
                tx.commit().map_err(map_sqlite)?;
            }
            Ok(ids.len())
        })?;
        if purged > 0 {
            self.collect_blobs()?;
        }
        Ok(purged)
    }

    /// `_collect_blobs`: drop blobs no attachment row references any more.
    fn collect_blobs(&self) -> Result<(), DunkError> {
        let keep: std::collections::HashSet<String> = self.writer.call(|connection| {
            let mut statement = connection
                .prepare("SELECT DISTINCT sha256 FROM attachments")
                .map_err(map_sqlite)?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(map_sqlite)?;
            rows.collect::<Result<_, _>>().map_err(map_sqlite)
        })?;
        if let Ok(entries) = fs::read_dir(&self.blob_dir) {
            for entry in entries.flatten() {
                if !keep.contains(entry.file_name().to_string_lossy().as_ref()) {
                    let _ = fs::remove_file(entry.path());
                }
            }
        }
        Ok(())
    }

    pub fn shutdown(self) {
        self.writer.shutdown();
    }
}

impl Store {
    /// `record_notify`: how the nudge for a message went. `transport_id` and `attempt` keep
    /// their old value when `None` (the Python's `COALESCE`).
    pub fn record_notify(
        &self,
        message_id: &str,
        state: &str,
        reason: Option<&str>,
        transport_id: Option<&str>,
        attempt: Option<i64>,
    ) -> Result<(), DunkError> {
        let (id, state, reason) = (
            message_id.to_string(),
            state.to_string(),
            reason.map(str::to_string),
        );
        let (transport_id, attempt, now) = (transport_id.map(str::to_string), attempt, now_s());
        self.writer.call(move |connection| {
            connection
                .execute(
                    "UPDATE messages SET notify_state = ?, notify_reason = ?, notify_at = ?, \
                     notify_transport_id = COALESCE(?, notify_transport_id), \
                     notify_attempt = COALESCE(?, notify_attempt) WHERE id = ?",
                    rusqlite::params![state, reason, now, transport_id, attempt, id],
                )
                .map_err(map_sqlite)?;
            Ok(())
        })
    }
}

/// What `ack`/`cancel` did: `_transition`'s `{state, changed, unchanged}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Transition {
    pub state: String,
    pub changed: Vec<String>,
    /// `(message_id, state it was already in)`.
    pub unchanged: Vec<(String, String)>,
}

/// `archive`'s `{mailbox, archived, states, before}`.
#[derive(Debug, Clone, PartialEq)]
pub struct Archived {
    pub mailbox: String,
    pub archived: usize,
    pub states: Vec<String>,
    pub before: Option<f64>,
}

/// `fetch`'s result.
#[derive(Debug, Clone, PartialEq)]
pub struct Fetched {
    pub attachment_id: String,
    pub path: PathBuf,
    pub filename: Option<String>,
    pub size: i64,
    pub media_type: Option<String>,
    pub message_id: String,
}

/// `[str(i).strip() for i in ids if str(i).strip()]`, and the empty refusal.
fn clean_ids(message_ids: &[String]) -> Result<Vec<String>, DunkError> {
    let ids: Vec<String> = message_ids
        .iter()
        .map(|i| i.trim().to_string())
        .filter(|i| !i.is_empty())
        .collect();
    if ids.is_empty() {
        return Err(refuse("at least one message id is required"));
    }
    Ok(ids)
}

/// Python `repr()` of a str, for the `{message_id!r}` texts.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    format!("{quote}{s}{quote}")
}

impl Store {
    /// `ack`: implies read (`read_at` is stamped so a board that counts NULLs as unread
    /// cannot report drained mail as waiting), and is scoped to the caller's mailbox: an id
    /// from another mailbox is refused, never silently marked read there.
    pub fn ack(
        &self,
        message_ids: &[String],
        mailbox: Option<&str>,
    ) -> Result<Transition, DunkError> {
        self.transition(
            message_ids,
            "acked",
            "acked_at",
            &["queued", "read"],
            true,
            mailbox,
        )
    }

    /// `cancel`.
    pub fn cancel(&self, message_ids: &[String]) -> Result<Transition, DunkError> {
        self.transition(
            message_ids,
            "cancelled",
            "cancelled_at",
            &["queued"],
            false,
            None,
        )
    }

    /// `_transition`. The whole batch is one transaction: a refusal part-way leaves nothing
    /// changed (the Python raises before its single commit).
    fn transition(
        &self,
        message_ids: &[String],
        state: &'static str,
        stamp: &'static str,
        allowed: &'static [&'static str],
        read_stamp: bool,
        mailbox: Option<&str>,
    ) -> Result<Transition, DunkError> {
        let ids = clean_ids(message_ids)?;
        let mailbox = mailbox.filter(|m| !m.is_empty()).map(str::to_string);
        self.writer.call(move |connection| {
            let tx = connection.transaction().map_err(map_sqlite)?;
            let (mut changed, mut unchanged) = (Vec::new(), Vec::new());
            for id in &ids {
                let row: Option<(String, String)> = tx
                    .query_row(
                        "SELECT state, mailbox FROM messages WHERE id = ?",
                        [id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map(Some)
                    .or_else(|e| match e {
                        rusqlite::Error::QueryReturnedNoRows => Ok(None),
                        other => Err(other),
                    })
                    .map_err(map_sqlite)?;
                let Some((current, owner)) = row else {
                    return Err(refuse(format!("no message with id {}", py_repr(id))));
                };
                if let Some(mailbox) = &mailbox {
                    if normalise_mailbox(&owner) != normalise_mailbox(mailbox) {
                        // A write not scoped to its owner is a blast radius: the VP acked a
                        // sent copy and the recipient lane lost its unread.
                        return Err(refuse(format!(
                            "message {id} is not in mailbox {}; an ack only applies to the \
                             caller's own mailbox",
                            py_repr(mailbox)
                        )));
                    }
                }
                if !allowed.contains(&current.as_str()) {
                    unchanged.push((id.clone(), current));
                    continue;
                }
                let now = now_s();
                if read_stamp {
                    tx.execute(
                        &format!(
                            "UPDATE messages SET state = ?, {stamp} = ?, \
                             read_at = COALESCE(read_at, ?) WHERE id = ?"
                        ),
                        rusqlite::params![state, now, now, id],
                    )
                } else {
                    tx.execute(
                        &format!("UPDATE messages SET state = ?, {stamp} = ? WHERE id = ?"),
                        rusqlite::params![state, now, id],
                    )
                }
                .map_err(map_sqlite)?;
                changed.push(id.clone());
            }
            tx.commit().map_err(map_sqlite)?;
            Ok(Transition {
                state: state.to_string(),
                changed,
                unchanged,
            })
        })
    }

    /// `archive`: bulk-retire a mailbox without deleting anything. Default states are queued
    /// and acked; `cancelled`, `archived` and `undeliverable` are never archivable.
    pub fn archive(
        &self,
        mailbox: &str,
        before: Option<f64>,
        states: Option<&[String]>,
    ) -> Result<Archived, DunkError> {
        let mailbox = mailbox.trim().to_string();
        if mailbox.is_empty() {
            return Err(refuse("a mailbox is required"));
        }
        let requested: Vec<String> = match states {
            None => vec!["queued".to_string(), "acked".to_string()],
            Some(list) => list.to_vec(),
        };
        let allowed: Vec<String> = requested
            .into_iter()
            .filter(|s| {
                STATES.contains(&s.as_str())
                    && !matches!(s.as_str(), "cancelled" | "archived" | "undeliverable")
            })
            .collect();
        if allowed.is_empty() {
            return Err(refuse("state must be one of queued, read, acked"));
        }
        let now = now_s();
        let placeholders = vec!["?"; allowed.len()].join(",");
        let (condition, box_params) = mailbox_where(&mailbox, "mailbox");
        let mut sql = format!(
            "UPDATE messages SET state = 'archived', archived_at = ?, \
             read_at = COALESCE(read_at, ?) WHERE {condition} AND state IN ({placeholders})"
        );
        let mut params: Vec<rusqlite::types::Value> = vec![now.into(), now.into()];
        params.extend(box_params.into_iter().map(Into::into));
        params.extend(allowed.iter().cloned().map(Into::into));
        if let Some(before) = before {
            sql.push_str(" AND created_at < ?");
            params.push(before.into());
        }
        let archived = self.writer.call(move |connection| {
            connection
                .execute(&sql, rusqlite::params_from_iter(params.iter()))
                .map_err(map_sqlite)
        })?;
        Ok(Archived {
            mailbox,
            archived,
            states: allowed,
            before,
        })
    }

    /// `delete`: hard-delete messages and their attachment rows, then collect orphan blobs.
    /// Every id is checked first, so an unknown id deletes nothing.
    pub fn delete(&self, message_ids: &[String]) -> Result<Vec<String>, DunkError> {
        let ids = clean_ids(message_ids)?;
        let doomed = ids.clone();
        self.writer.call(move |connection| {
            let tx = connection.transaction().map_err(map_sqlite)?;
            for id in &doomed {
                let found: i64 = tx
                    .query_row("SELECT COUNT(*) FROM messages WHERE id = ?", [id], |r| {
                        r.get(0)
                    })
                    .map_err(map_sqlite)?;
                if found == 0 {
                    return Err(refuse(format!("no message with id {}", py_repr(id))));
                }
            }
            for id in &doomed {
                tx.execute("DELETE FROM attachments WHERE message_id = ?", [id])
                    .map_err(map_sqlite)?;
                tx.execute("DELETE FROM messages WHERE id = ?", [id])
                    .map_err(map_sqlite)?;
            }
            tx.commit().map_err(map_sqlite)?;
            Ok(())
        })?;
        self.collect_blobs()?;
        Ok(ids)
    }

    /// `fetch`: copy an attachment out of the store into `dest_dir` (created if missing).
    pub fn fetch(&self, attachment_id: &str, dest_dir: &Path) -> Result<Fetched, DunkError> {
        let wanted = attachment_id.to_string();
        let row = self.writer.call(move |connection| {
            connection
                .query_row(
                    "SELECT sha256, filename, size, media_type, message_id \
                     FROM attachments WHERE id = ?",
                    [&wanted],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, Option<String>>(1)?,
                            row.get::<_, i64>(2)?,
                            row.get::<_, Option<String>>(3)?,
                            row.get::<_, String>(4)?,
                        ))
                    },
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    other => Err(other),
                })
                .map_err(map_sqlite)
        })?;
        let Some((sha, filename, size, media, message_id)) = row else {
            return Err(refuse(format!(
                "no attachment with id {}",
                py_repr(attachment_id)
            )));
        };
        let blob_path = self.blob_dir.join(&sha);
        if !blob_path.exists() {
            return Err(refuse(format!(
                "attachment {attachment_id} is missing from the store"
            )));
        }
        let dest = expand_user(dest_dir);
        fs::create_dir_all(&dest).map_err(|e| refuse(format!("{}: {e}", dest.display())))?;
        let name = filename.clone().filter(|n| !n.is_empty()).unwrap_or(sha);
        let target = dest.join(name);
        fs::copy(&blob_path, &target).map_err(|e| refuse(format!("{}: {e}", target.display())))?;
        Ok(Fetched {
            attachment_id: attachment_id.to_string(),
            path: target,
            filename,
            size,
            media_type: media,
            message_id,
        })
    }
}

/// A message as the Python's `_full` renders it: the summary plus body, `read_at` and the
/// attachment rows (the read half's `Summary` carries only their count).
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    pub summary: Summary,
    pub body: String,
    pub read_at: Option<f64>,
    pub attachments: Vec<AttachmentRow>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AttachmentRow {
    pub id: String,
    pub filename: String,
    pub media_type: String,
    pub size: i64,
}

const FULL_COLUMNS: &str = "id, mailbox, mailbox_label, sender, subject, state, created_at, \
    body, length(CAST(body AS BLOB)) AS body_bytes, digest_id, transport, reply_to, \
    external_id, delivered_at, delivery_transport, read_at, \
    (SELECT COUNT(*) FROM attachments a WHERE a.message_id = messages.id) AS n_attachments";

fn full_from_row(row: &rusqlite::Row, now: f64) -> rusqlite::Result<Message> {
    let state: String = row.get("state")?;
    let created_at: f64 = row.get("created_at")?;
    let mailbox: String = row.get("mailbox")?;
    let label: Option<String> = row.get("mailbox_label")?;
    Ok(Message {
        summary: Summary {
            message_id: row.get("id")?,
            destination: label
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| mailbox.clone()),
            mailbox,
            sender: row.get("sender")?,
            subject: row.get("subject")?,
            waiting_s: if state == "queued" {
                Some((now - created_at) as i64)
            } else {
                None
            },
            created_at,
            body_bytes: row.get("body_bytes")?,
            attachments: row.get("n_attachments")?,
            digest_id: row.get("digest_id")?,
            transport: row.get("transport")?,
            reply_to: row.get("reply_to")?,
            external_id: row.get("external_id")?,
            delivered_at: row.get("delivered_at")?,
            delivery_transport: row.get("delivery_transport")?,
            state,
        },
        body: row.get("body")?,
        read_at: row.get("read_at")?,
        attachments: Vec::new(),
    })
}

fn attachments_of(
    connection: &rusqlite::Connection,
    message_id: &str,
) -> Result<Vec<AttachmentRow>, DunkError> {
    let mut statement = connection
        .prepare(
            "SELECT id, filename, media_type, size FROM attachments \
             WHERE message_id = ? ORDER BY rowid",
        )
        .map_err(map_sqlite)?;
    let rows = statement
        .query_map([message_id], |row| {
            Ok(AttachmentRow {
                id: row.get(0)?,
                filename: row.get(1)?,
                media_type: row.get(2)?,
                size: row.get(3)?,
            })
        })
        .map_err(map_sqlite)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
}

/// `_next_id`: the counter lives beside the messages, so ids survive restarts.
fn next_message_id(transaction: &rusqlite::Transaction) -> Result<String, DunkError> {
    let stored_number: Option<i64> = transaction
        .query_row(
            "SELECT value FROM counters WHERE name = 'message'",
            [],
            |row| row.get(0),
        )
        .map(Some)
        .or_else(|error| match error {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other),
        })
        .map_err(map_sqlite)?;
    let number = next_message_number(stored_number);
    transaction
        .execute(COUNTER_UPSERT, rusqlite::params![number, number])
        .map_err(map_sqlite)?;
    Ok(format!("i{number}"))
}

/// `json.dumps(list_of_str)`: `["a", "b"]`.
fn json_list(ids: &[String]) -> String {
    let parts: Vec<String> = ids
        .iter()
        .map(|id| serde_json::to_string(id).unwrap_or_default())
        .collect();
    format!("[{}]", parts.join(", "))
}

/// `os.path.expanduser`: a leading `~` or `~/` only.
fn expand_user(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if text == "~" || text.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(text.trim_start_matches('~').trim_start_matches('/'));
        }
    }
    path.to_path_buf()
}

/// `mimetypes.guess_type` for the extensions mail actually carries; anything else is the
/// Python's fallback, `application/octet-stream`. Every entry was checked against Python 3
/// `mimetypes.guess_type` (`.log` and bare `.gz` are octet-stream there, so they are NOT
/// listed). The Python also reads the platform's mime tables (/etc/mime.types), so an
/// extension this table omits can differ on a host that maps it: named, not hidden.
pub fn media_type(path: &Path) -> String {
    // `.gz` alone is (None, 'gzip') in Python, i.e. octet-stream, but `.tar.gz`/`.tgz`
    // guess the TAR type: the compression is the encoding, not the type.
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return "application/x-tar".to_string();
    }
    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    match extension.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "txt" => "text/plain",
        "md" => "text/markdown",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "pdf" => "application/pdf",
        "zip" => "application/zip",
        "tar" => "application/x-tar",
        _ => "application/octet-stream",
    }
    .to_string()
}

#[cfg(test)]
#[path = "inbox_store_tests.rs"]
mod inbox_store_tests;
