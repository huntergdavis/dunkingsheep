//! Mailbox lifecycle (t5014): a mailbox can be RETIRED as a whole.
//!
//! Retiring is logical. The messages stay in `messages`, so search, read-by-id, attachments
//! and recovery work unchanged; live list views skip a retired mailbox. Any new mail for a
//! retired mailbox reopens it in the same transaction that stores the mail, so nothing sent to
//! it is hidden or lost.
//!
//! "Retired" is a mailbox status. It is not the per-message state `archived`, which a reader
//! (or the reaper, for retired panes) sets on single messages.

use rusqlite::{params, Connection, TransactionBehavior};
use serde_json::{json, Value};

use crate::db::map_sqlite;
use crate::error::DunkError;

use super::Store;

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS mailbox_status (
    mailbox        TEXT PRIMARY KEY,
    status         TEXT NOT NULL CHECK (status IN ('active', 'retired')),
    changed_at     REAL NOT NULL,
    reason         TEXT,
    changed_by     TEXT,
    message_count  INTEGER,
    newest_at      REAL,
    reopen_count   INTEGER NOT NULL DEFAULT 0
)";

/// t5014: every retire decision and every reopen, append-only. Rows are never updated or
/// deleted: the triggers refuse it. An older daemon ignores this table (additive; rollback-safe).
pub const AUDIT_SCHEMA: &str = "CREATE TABLE IF NOT EXISTS mailbox_retire_audit (
    id       INTEGER PRIMARY KEY AUTOINCREMENT,
    at       REAL NOT NULL,
    actor    TEXT NOT NULL,
    mailbox  TEXT NOT NULL,
    class    TEXT,
    reason   TEXT,
    outcome  TEXT NOT NULL
);
CREATE TRIGGER IF NOT EXISTS mailbox_retire_audit_no_update BEFORE UPDATE ON mailbox_retire_audit
BEGIN SELECT RAISE(ABORT, 'mailbox_retire_audit is append-only'); END;
CREATE TRIGGER IF NOT EXISTS mailbox_retire_audit_no_delete BEFORE DELETE ON mailbox_retire_audit
BEGIN SELECT RAISE(ABORT, 'mailbox_retire_audit is append-only'); END;";

fn audit(
    connection: &Connection,
    at: f64,
    actor: &str,
    mailbox: &str,
    class: Option<&str>,
    reason: &str,
    outcome: &str,
) -> Result<(), DunkError> {
    connection
        .execute(
            "INSERT INTO mailbox_retire_audit (at, actor, mailbox, class, reason, outcome) \
             VALUES (?, ?, ?, ?, ?, ?)",
            params![at, actor, mailbox, class, reason, outcome],
        )
        .map_err(map_sqlite)?;
    Ok(())
}

/// The SQL condition that leaves out retired mailboxes, for queries over `messages`.
pub const NOT_RETIRED: &str =
    "mailbox NOT IN (SELECT mailbox FROM mailbox_status WHERE status = 'retired')";

/// A mailbox is retired only after this long without new mail.
pub const QUIET_S: f64 = 14.0 * 86_400.0;

/// Put a retired mailbox back to active because mail arrived for it. Called inside the
/// transaction that stores the mail; a mailbox that is not retired is left alone.
pub(crate) fn reopen_on_mail(
    connection: &Connection,
    mailbox: &str,
    now: f64,
) -> Result<(), DunkError> {
    let reopened = connection
        .execute(
            "UPDATE mailbox_status SET status = 'active', changed_at = ?, reason = 'late mail', \
             changed_by = 'inbox', reopen_count = reopen_count + 1 \
             WHERE mailbox = ? AND status = 'retired'",
            params![now, mailbox],
        )
        .map_err(map_sqlite)?;
    if reopened > 0 {
        audit(
            connection,
            now,
            "inbox",
            mailbox,
            None,
            "late mail",
            "reopened",
        )?;
    }
    Ok(())
}

/// One mailbox as the census sees it. No subject or body is read.
#[derive(Debug, Clone, PartialEq)]
pub struct CensusRow {
    pub mailbox: String,
    pub messages: i64,
    pub attachments: i64,
    /// Messages still waiting to be read (state `queued`).
    pub queued: i64,
    pub newest_at: Option<f64>,
    pub open_pr_waits: i64,
    /// `active` or `retired`; `None` when the mailbox has no status row (active by default).
    pub status: Option<String>,
}

/// What a retire call did with each mailbox it was given.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RetireOutcome {
    pub retired: Vec<String>,
    /// Mailboxes left active, with why.
    pub kept: Vec<(String, String)>,
}

impl Store {
    /// Every mailbox with its counts, read in one pass over the indexes. Read-only.
    pub fn mailbox_census(&self) -> Result<Vec<CensusRow>, DunkError> {
        self.read_pool().with(|connection: &Connection| {
            let mut statement = connection
                .prepare(
                    "SELECT m.mailbox, COUNT(*), SUM(m.state = 'queued'), MAX(m.created_at), \
                     (SELECT COUNT(*) FROM attachments a JOIN messages x ON x.id = a.message_id \
                        WHERE x.mailbox = m.mailbox), \
                     (SELECT COUNT(*) FROM pr_waits w WHERE w.mailbox = m.mailbox AND w.event IS NULL), \
                     (SELECT status FROM mailbox_status s WHERE s.mailbox = m.mailbox) \
                     FROM messages m GROUP BY m.mailbox ORDER BY m.mailbox",
                )
                .map_err(map_sqlite)?;
            let rows = statement
                .query_map([], |row| {
                    Ok(CensusRow {
                        mailbox: row.get(0)?,
                        messages: row.get(1)?,
                        queued: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                        newest_at: row.get(3)?,
                        attachments: row.get(4)?,
                        open_pr_waits: row.get(5)?,
                        status: row.get(6)?,
                    })
                })
                .map_err(map_sqlite)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
        })
    }

    /// The status rows, for reports.
    pub fn mailbox_statuses(&self) -> Result<Vec<Value>, DunkError> {
        self.read_pool().with(|connection: &Connection| {
            let mut statement = connection
                .prepare(
                    "SELECT mailbox, status, changed_at, reason, changed_by, message_count, \
                     newest_at, reopen_count FROM mailbox_status ORDER BY mailbox",
                )
                .map_err(map_sqlite)?;
            let rows = statement
                .query_map([], |row| {
                    Ok(json!({
                        "mailbox": row.get::<_, String>(0)?,
                        "status": row.get::<_, String>(1)?,
                        "changed_at": row.get::<_, f64>(2)?,
                        "reason": row.get::<_, Option<String>>(3)?,
                        "changed_by": row.get::<_, Option<String>>(4)?,
                        "message_count": row.get::<_, Option<i64>>(5)?,
                        "newest_at": row.get::<_, Option<f64>>(6)?,
                        "reopen_count": row.get::<_, i64>(7)?,
                    }))
                })
                .map_err(map_sqlite)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
        })
    }

    /// Retire mailboxes. Each one is checked again inside one IMMEDIATE transaction, so mail
    /// that arrives meanwhile either lands before the check (and keeps the mailbox active) or
    /// after the commit (and reopens it). Kept: a mailbox with queued mail, an open PR wait,
    /// mail newer than `QUIET_S`, or no messages at all. Nothing is deleted or moved.
    ///
    /// No command calls this yet: retiring on the real database needs a reviewed dry run and
    /// the coordinator's GO (t5014).
    pub fn retire_mailboxes(
        &self,
        mailboxes: &[String],
        reason: &str,
        by: &str,
        now: f64,
    ) -> Result<RetireOutcome, DunkError> {
        self.retire_mailboxes_audited(mailboxes, &[], reason, by, now)
    }

    /// `retire_mailboxes`, recording in the append-only audit, in the same transaction, every
    /// mailbox the caller refused before asking (`refused`: name and class at apply time) as
    /// well as each one retired or kept at the re-check.
    pub fn retire_mailboxes_audited(
        &self,
        mailboxes: &[String],
        refused: &[(String, String)],
        reason: &str,
        by: &str,
        now: f64,
    ) -> Result<RetireOutcome, DunkError> {
        let mailboxes = mailboxes.to_vec();
        let refused = refused.to_vec();
        let (reason, by) = (reason.to_string(), by.to_string());
        self.writer.call(move |connection| {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .map_err(map_sqlite)?;
            for (mailbox, class) in &refused {
                let outcome = if class == "not found" { "not found" } else { "refused" };
                audit(&transaction, now, &by, mailbox, Some(class), &reason, outcome)?;
            }
            let mut outcome = RetireOutcome::default();
            for mailbox in mailboxes {
                let (messages, queued, newest): (i64, i64, Option<f64>) = transaction
                    .query_row(
                        "SELECT COUNT(*), COALESCE(SUM(state = 'queued'), 0), MAX(created_at) \
                         FROM messages WHERE mailbox = ?",
                        [&mailbox],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .map_err(map_sqlite)?;
                let waits: i64 = transaction
                    .query_row(
                        "SELECT COUNT(*) FROM pr_waits WHERE mailbox = ? AND event IS NULL",
                        [&mailbox],
                        |row| row.get(0),
                    )
                    .map_err(map_sqlite)?;
                let keep = if messages == 0 {
                    Some("no messages".to_string())
                } else if queued > 0 {
                    Some(format!("{queued} unread message(s)"))
                } else if waits > 0 {
                    Some("an open PR wait".to_string())
                } else if newest.is_some_and(|newest| now - newest < QUIET_S) {
                    Some("mail in the last 14 days".to_string())
                } else {
                    None
                };
                if let Some(why) = keep {
                    audit(&transaction, now, &by, &mailbox, Some("retired"), &reason, &format!("kept at re-check: {why}"))?;
                    outcome.kept.push((mailbox, why));
                    continue;
                }
                transaction
                    .execute(
                        "INSERT INTO mailbox_status (mailbox, status, changed_at, reason, \
                         changed_by, message_count, newest_at) VALUES (?, 'retired', ?, ?, ?, ?, ?) \
                         ON CONFLICT(mailbox) DO UPDATE SET status = 'retired', \
                         changed_at = excluded.changed_at, reason = excluded.reason, \
                         changed_by = excluded.changed_by, message_count = excluded.message_count, \
                         newest_at = excluded.newest_at",
                        params![mailbox, now, reason, by, messages, newest],
                    )
                    .map_err(map_sqlite)?;
                audit(&transaction, now, &by, &mailbox, Some("retired"), &reason, "retired")?;
                outcome.retired.push(mailbox);
            }
            transaction.commit().map_err(map_sqlite)?;
            Ok(outcome)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_store::{DigestUpsert, Outgoing, Store};

    const DAY: f64 = 86_400.0;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap();
        (dir, store)
    }

    fn send(store: &Store, mailbox: &str) -> String {
        store
            .send(&Outgoing {
                mailbox,
                body: Some("synthetic"),
                sender: Some("test"),
                ..Default::default()
            })
            .unwrap()
    }

    /// Age every message of `mailbox` by `days` and mark it read.
    fn age(store: &Store, mailbox: &str, days: f64) {
        let mailbox = mailbox.to_string();
        store
            .writer
            .call(move |connection| {
                connection
                    .execute(
                        "UPDATE messages SET created_at = created_at - ?, state = 'read' WHERE mailbox = ?",
                        params![days * DAY, mailbox],
                    )
                    .map_err(map_sqlite)?;
                Ok(())
            })
            .unwrap();
    }

    fn status(store: &Store, mailbox: &str) -> Option<String> {
        store
            .mailbox_statuses()
            .unwrap()
            .into_iter()
            .find(|row| row["mailbox"] == mailbox)
            .map(|row| row["status"].as_str().unwrap().to_string())
    }

    #[test]
    fn a_quiet_read_mailbox_is_retired_and_every_other_kind_is_kept_with_the_reason() {
        let (_dir, store) = store();
        for mailbox in ["quiet", "recent", "unread", "waiting"] {
            send(&store, mailbox);
        }
        age(&store, "quiet", 15.0);
        age(&store, "recent", 13.0);
        age(&store, "waiting", 30.0);
        // "unread" keeps its queued message, also when old.
        let unread = "unread".to_string();
        store
            .writer
            .call(move |c| {
                c.execute(
                    "UPDATE messages SET created_at = created_at - ? WHERE mailbox = ?",
                    params![30.0 * DAY, unread],
                )
                .map_err(map_sqlite)?;
                c.execute(
                    "INSERT INTO pr_waits (repo, pr, mailbox, pane, created_at) VALUES ('o/r', 1, 'waiting', '%1', 0)",
                    [],
                )
                .map_err(map_sqlite)?;
                Ok(())
            })
            .unwrap();
        let names = |list: &[&str]| list.iter().map(|m| m.to_string()).collect::<Vec<_>>();
        let outcome = store
            .retire_mailboxes(
                &names(&["quiet", "recent", "unread", "waiting", "empty"]),
                "test",
                "%308",
                crate::clock::now_s(),
            )
            .unwrap();
        assert_eq!(outcome.retired, ["quiet"]);
        assert_eq!(
            outcome.kept,
            vec![
                ("recent".to_string(), "mail in the last 14 days".to_string()),
                ("unread".to_string(), "1 unread message(s)".to_string()),
                ("waiting".to_string(), "an open PR wait".to_string()),
                ("empty".to_string(), "no messages".to_string()),
            ]
        );
        assert_eq!(status(&store, "quiet").as_deref(), Some("retired"));
        assert_eq!(
            status(&store, "recent"),
            None,
            "a kept mailbox gets no status row"
        );
        // Retiring again is a no-op that keeps it retired.
        let again = store
            .retire_mailboxes(&names(&["quiet"]), "test", "%308", crate::clock::now_s())
            .unwrap();
        assert_eq!(again.retired, ["quiet"]);
    }

    /// t5014: every apply appends; nothing in the audit can be changed or removed.
    #[test]
    fn the_retire_audit_is_append_only_and_two_applies_leave_two_rows_each() {
        let (_dir, store) = store();
        let rows = |store: &Store| -> Vec<(String, String, String)> {
            store
                .writer
                .call(|connection| {
                    let mut statement = connection
                        .prepare(
                            "SELECT mailbox, outcome, actor FROM mailbox_retire_audit ORDER BY id",
                        )
                        .map_err(map_sqlite)?;
                    let found = statement
                        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                        .map_err(map_sqlite)?
                        .collect::<Result<Vec<_>, _>>()
                        .map_err(map_sqlite)?;
                    Ok(found)
                })
                .unwrap()
        };
        let refused = vec![
            ("poolrad-vp".to_string(), "vp".to_string()),
            ("ghost".to_string(), "not found".to_string()),
        ];
        let names = |list: &[&str]| list.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        store
            .retire_mailboxes_audited(&names(&["empty-one"]), &refused, "first", "%1", 10.0)
            .unwrap();
        store
            .retire_mailboxes_audited(&names(&["empty-one"]), &refused, "second", "%1", 20.0)
            .unwrap();
        let audit = rows(&store);
        assert_eq!(audit.len(), 6, "{audit:?}");
        assert_eq!(
            audit[0],
            ("poolrad-vp".into(), "refused".into(), "%1".into())
        );
        assert_eq!(audit[1].1, "not found");
        assert!(audit[2].1.starts_with("kept at re-check"), "{audit:?}");
        // Append-only: an update or a delete is refused by the database.
        let tamper = |sql: &'static str| {
            store
                .writer
                .call(move |connection| connection.execute(sql, []).map_err(map_sqlite))
                .unwrap_err()
                .to_string()
        };
        assert!(tamper("UPDATE mailbox_retire_audit SET outcome = 'x'").contains("append-only"));
        assert!(tamper("DELETE FROM mailbox_retire_audit").contains("append-only"));
        assert_eq!(rows(&store).len(), 6);
    }

    #[test]
    fn mail_for_a_retired_mailbox_reopens_it_and_nothing_is_lost() {
        let (_dir, store) = store();
        let old = send(&store, "gone");
        age(&store, "gone", 20.0);
        store
            .retire_mailboxes(&["gone".to_string()], "test", "%308", crate::clock::now_s())
            .unwrap();
        // Live views skip it; its mail is still there.
        let live = crate::inbox::stats(store.read_pool(), crate::clock::now_s()).unwrap();
        assert!(live.iter().all(|row| row.mailbox != "gone"), "{live:?}");
        // `read` returns waiting mail only; this one was read, so count the stored row itself.
        let stored: i64 = store
            .read_pool()
            .with(|c: &Connection| {
                c.query_row("SELECT COUNT(*) FROM messages WHERE id = ?", [&old], |r| {
                    r.get(0)
                })
                .map_err(map_sqlite)
            })
            .unwrap();
        assert_eq!(stored, 1, "the retired mailbox's mail is still stored");
        let census = store.mailbox_census().unwrap();
        let row = census.iter().find(|row| row.mailbox == "gone").unwrap();
        assert_eq!((row.messages, row.status.as_deref()), (1, Some("retired")));

        // Late mail: stored, and the mailbox is active again in the same transaction.
        let late = send(&store, "gone");
        assert_eq!(status(&store, "gone").as_deref(), Some("active"));
        let row = store
            .mailbox_statuses()
            .unwrap()
            .into_iter()
            .find(|row| row["mailbox"] == "gone")
            .unwrap();
        assert_eq!(
            (row["reason"].as_str(), row["reopen_count"].as_i64()),
            (Some("late mail"), Some(1))
        );
        let live = crate::inbox::stats(store.read_pool(), crate::clock::now_s()).unwrap();
        let gone = live.iter().find(|row| row.mailbox == "gone").unwrap();
        assert_eq!((gone.total, gone.unread), (2, 1));
        assert_ne!(old, late);

        // A digest is mail too.
        age(&store, "gone", 20.0);
        store
            .retire_mailboxes(&["gone".to_string()], "test", "%308", crate::clock::now_s())
            .unwrap();
        store
            .upsert_digest(DigestUpsert::new("gone", "digest-1", "synthetic digest"))
            .unwrap();
        assert_eq!(status(&store, "gone").as_deref(), Some("active"));
    }

    #[test]
    fn a_next_job_and_a_renamed_mailbox_reopen_a_retired_mailbox() {
        let (_dir, store) = store();
        for mailbox in ["worker", "old-name"] {
            send(&store, mailbox);
            age(&store, mailbox, 20.0);
        }
        store
            .retire_mailboxes(
                &["worker".to_string()],
                "test",
                "%308",
                crate::clock::now_s(),
            )
            .unwrap();
        store
            .ensure_next_job("nj-1", "worker", "%23", "next body", "Next task")
            .unwrap();
        assert_eq!(status(&store, "worker").as_deref(), Some("active"));
        // The same next job again stores nothing new, and does not count as a second reopen.
        store
            .retire_mailboxes(
                &["worker".to_string()],
                "test",
                "%308",
                crate::clock::now_s(),
            )
            .unwrap();
        assert_eq!(
            status(&store, "worker").as_deref(),
            Some("active"),
            "it holds queued mail now"
        );

        // Mail moved into a retired mailbox by a rename reopens it.
        send(&store, "renamed-to");
        age(&store, "renamed-to", 20.0);
        store
            .retire_mailboxes(
                &["renamed-to".to_string()],
                "test",
                "%308",
                crate::clock::now_s(),
            )
            .unwrap();
        assert_eq!(store.rename_mailbox("old-name", "renamed-to").unwrap(), 1);
        assert_eq!(status(&store, "renamed-to").as_deref(), Some("active"));
        // A rename that moves nothing changes nothing.
        store
            .retire_mailboxes(
                &["renamed-to".to_string()],
                "test",
                "%308",
                crate::clock::now_s(),
            )
            .unwrap();
        assert_eq!(store.rename_mailbox("nobody", "renamed-to").unwrap(), 0);
        assert_eq!(status(&store, "renamed-to").as_deref(), Some("retired"));
    }

    #[test]
    fn the_census_reads_counts_without_bodies() {
        let (_dir, store) = store();
        send(&store, "a");
        send(&store, "a");
        send(&store, "b");
        let census = store.mailbox_census().unwrap();
        assert_eq!(census.len(), 2);
        assert_eq!(
            (
                census[0].mailbox.as_str(),
                census[0].messages,
                census[0].queued
            ),
            ("a", 2, 2)
        );
        assert_eq!(
            (
                census[0].attachments,
                census[0].open_pr_waits,
                census[0].status.clone()
            ),
            (0, 0, None)
        );
        assert!(census[0].newest_at.is_some());
    }
}
