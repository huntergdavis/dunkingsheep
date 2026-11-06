//! Age-archive mailboxes only after the owning Flock revalidates retirement while
//! holding the identity-file guards. The authorization callback is run twice on
//! the writer connection; its second result is authoritative inside IMMEDIATE.

use std::collections::BTreeSet;

use rusqlite::types::Value as SqlValue;
use rusqlite::{params_from_iter, Connection, TransactionBehavior};

use crate::clock::now_s;
use crate::db::map_sqlite;
use crate::error::DunkError;

use super::Store;

/// The source-compatible retention controls for `Inbox.archive_retired`.
#[derive(Debug, Clone, PartialEq)]
pub struct RetiredArchiveOptions {
    pub age_s: f64,
    pub now: Option<f64>,
    pub reason: Option<String>,
}

impl Default for RetiredArchiveOptions {
    fn default() -> Self {
        Self {
            age_s: 86_400.0,
            now: None,
            reason: None,
        }
    }
}

impl Store {
    /// Archive old queued/undeliverable rows after the caller verifies pane
    /// identity. `authorize` runs once for a preview and again after
    /// `BEGIN IMMEDIATE`; the preview can only reduce the final set.
    pub fn archive_retired<F>(
        &self,
        options: RetiredArchiveOptions,
        authorize: F,
    ) -> Result<usize, DunkError>
    where
        F: Fn(&Connection) -> Result<Vec<String>, DunkError> + Send + 'static,
    {
        let now = options.now.unwrap_or_else(now_s);
        let cutoff = now - options.age_s;
        let reason = options
            .reason
            .filter(|reason| !reason.is_empty())
            .unwrap_or_else(|| "mailbox of a retired pane".to_string());

        self.writer.call(move |connection| {
            archive_on_connection(connection, now, cutoff, &reason, &authorize)
        })
    }
}

fn archive_on_connection<F>(
    connection: &mut Connection,
    now: f64,
    cutoff: f64,
    reason: &str,
    authorize: &F,
) -> Result<usize, DunkError>
where
    F: Fn(&Connection) -> Result<Vec<String>, DunkError>,
{
    // Python refuses to commit or discard a transaction owned by another caller.
    if !connection.is_autocommit() {
        return Ok(0);
    }
    let preview: BTreeSet<String> = authorize(connection)?.into_iter().collect();
    if preview.is_empty() {
        return Ok(0);
    }

    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    let fresh: BTreeSet<String> = authorize(&transaction)?.into_iter().collect();
    let mailboxes: Vec<String> = preview.intersection(&fresh).cloned().collect();
    if mailboxes.is_empty() {
        transaction.rollback().map_err(map_sqlite)?;
        return Ok(0);
    }

    let placeholders = vec!["?"; mailboxes.len()].join(",");
    let sql = format!(
        "UPDATE messages SET state = 'archived', archived_at = ?, \
         read_at = COALESCE(read_at, ?), reason = COALESCE(reason, ?) \
         WHERE mailbox IN ({placeholders}) AND state IN ('queued','undeliverable') \
         AND created_at < ? AND archived_at IS NULL"
    );
    let mut values: Vec<SqlValue> = vec![now.into(), now.into(), reason.to_string().into()];
    values.extend(mailboxes.into_iter().map(Into::into));
    values.push(cutoff.into());
    let changed = transaction
        .execute(&sql, params_from_iter(values.iter()))
        .map_err(map_sqlite)?;
    transaction.commit().map_err(map_sqlite)?;
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_store::{Outgoing, Store};
    use rusqlite::params;
    use std::sync::{Arc, Mutex};

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().expect("tempdir");
        let store =
            Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).expect("store");
        (dir, store)
    }

    fn send(store: &Store, mailbox: &str, pane_id: Option<&str>) -> String {
        store
            .send(&Outgoing {
                mailbox,
                body: Some("test mail"),
                pane_id,
                ..Outgoing::default()
            })
            .expect("send")
    }

    fn eligible(
        mailbox: &'static str,
    ) -> impl Fn(&Connection) -> Result<Vec<String>, DunkError> + Send + 'static {
        move |_| Ok(vec![mailbox.to_string()])
    }

    fn fields(store: &Store, id: &str) -> (String, Option<f64>, Option<f64>, Option<String>) {
        let id = id.to_string();
        store
            .writer
            .call(move |connection| {
                connection
                    .query_row(
                        "SELECT state, archived_at, read_at, reason FROM messages WHERE id = ?",
                        [id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .map_err(crate::db::map_sqlite)
            })
            .expect("read row")
    }

    #[test]
    fn retired_archive_revalidates_inside_immediate_transaction() {
        let (_dir, store) = store();
        let id = send(&store, "old-lane", Some("w1:p1"));
        let calls = Arc::new(Mutex::new(0));
        let seen = Arc::clone(&calls);
        let count = store
            .archive_retired(
                RetiredArchiveOptions {
                    now: Some(172_800.0),
                    ..RetiredArchiveOptions::default()
                },
                move |_| {
                    let mut calls = seen.lock().expect("calls lock");
                    *calls += 1;
                    if *calls == 1 {
                        Ok(vec!["old-lane".to_string()])
                    } else {
                        Ok(Vec::new()) // a newly bound/live identity is protected
                    }
                },
            )
            .expect("archive");
        assert_eq!(0, count);
        assert_eq!(2, *calls.lock().expect("calls lock"));
        assert_eq!("queued", fields(&store, &id).0);
    }

    #[test]
    fn retired_archive_changes_only_eligible_old_rows_and_preserves_fields() {
        let (_dir, store) = store();
        let old = send(&store, "old-lane", Some("w1:p1"));
        let undeliverable = send(&store, "old-lane", Some("w1:p1"));
        let young = send(&store, "old-lane", Some("w1:p1"));
        let read = send(&store, "old-lane", Some("w1:p1"));
        store
            .writer
            .call({
                let old = old.clone();
                let undeliverable = undeliverable.clone();
                let young = young.clone();
                let read = read.clone();
                move |connection| {
                    connection
                        .execute(
                            "UPDATE messages SET created_at = ?, read_at = ?, reason = ? WHERE id = ?",
                            params![1.0, 7.0, "keep this reason", old],
                        )
                        .map_err(crate::db::map_sqlite)?;
                    connection
                        .execute(
                            "UPDATE messages SET created_at = ?, state = 'undeliverable' WHERE id = ?",
                            params![1.0, undeliverable],
                        )
                        .map_err(crate::db::map_sqlite)?;
                    connection
                        .execute("UPDATE messages SET created_at = ? WHERE id = ?", params![86_400.0, young])
                        .map_err(crate::db::map_sqlite)?;
                    connection
                        .execute(
                            "UPDATE messages SET created_at = ?, state = 'read' WHERE id = ?",
                            params![1.0, read],
                        )
                        .map_err(crate::db::map_sqlite)?;
                    Ok(())
                }
            })
            .expect("prepare rows");
        let changed = store
            .archive_retired(
                RetiredArchiveOptions {
                    now: Some(172_800.0),
                    ..RetiredArchiveOptions::default()
                },
                eligible("old-lane"),
            )
            .expect("archive");
        assert_eq!(2, changed);
        let old = fields(&store, &old);
        assert_eq!("archived", old.0);
        assert!(old.1.is_some());
        assert_eq!(Some(7.0), old.2);
        assert_eq!(Some("keep this reason".to_string()), old.3);
        let undeliverable = fields(&store, &undeliverable);
        assert_eq!("archived", undeliverable.0);
        assert_eq!(
            Some("mailbox of a retired pane".to_string()),
            undeliverable.3
        );
        let young = fields(&store, &young);
        assert_eq!("queued", young.0);
        let read = fields(&store, &read);
        assert_eq!("read", read.0);
        assert_eq!(
            0,
            store
                .archive_retired(
                    RetiredArchiveOptions {
                        now: Some(172_800.0),
                        ..RetiredArchiveOptions::default()
                    },
                    eligible("old-lane"),
                )
                .expect("repeat archive")
        );
    }

    #[test]
    fn retired_archive_authorization_error_rolls_back() {
        let (_dir, store) = store();
        let id = send(&store, "old-lane", Some("w1:p1"));
        let calls = Arc::new(Mutex::new(0));
        let seen = Arc::clone(&calls);
        let error = store.archive_retired(
            RetiredArchiveOptions {
                now: Some(172_800.0),
                ..RetiredArchiveOptions::default()
            },
            move |_| {
                let mut calls = seen.lock().expect("calls lock");
                *calls += 1;
                if *calls == 1 {
                    Ok(vec!["old-lane".to_string()])
                } else {
                    Err(DunkError::Refused("identity changed".into()))
                }
            },
        );
        assert!(error.is_err());
        assert_eq!("queued", fields(&store, &id).0);
    }

    #[test]
    fn retired_archive_uses_python_reason_fallback_and_preserves_existing_reason() {
        let (_dir, store) = store();
        let default_reason = send(&store, "default-reason", Some("w1:p1"));
        let empty_reason = send(&store, "empty-reason", Some("w1:p1"));
        let configured_reason = send(&store, "configured-reason", Some("w1:p1"));
        let whitespace_reason = send(&store, "whitespace-reason", Some("w1:p1"));
        let existing_reason = send(&store, "existing-reason", Some("w1:p1"));
        store
            .writer
            .call({
                let ids = vec![
                    default_reason.clone(),
                    empty_reason.clone(),
                    configured_reason.clone(),
                    whitespace_reason.clone(),
                    existing_reason.clone(),
                ];
                let existing_reason_id = existing_reason.clone();
                move |connection| {
                    for id in ids {
                        connection
                            .execute("UPDATE messages SET created_at = 1 WHERE id = ?", [id])
                            .map_err(crate::db::map_sqlite)?;
                    }
                    connection
                        .execute(
                            "UPDATE messages SET reason = 'keep existing reason' WHERE id = ?",
                            [existing_reason_id],
                        )
                        .map_err(crate::db::map_sqlite)?;
                    Ok(())
                }
            })
            .expect("age messages");

        let archive = |mailbox, reason| {
            store
                .archive_retired(
                    RetiredArchiveOptions {
                        now: Some(172_800.0),
                        reason,
                        ..RetiredArchiveOptions::default()
                    },
                    eligible(mailbox),
                )
                .expect("archive message")
        };
        assert_eq!(1, archive("default-reason", None));
        assert_eq!(1, archive("empty-reason", Some(String::new())));
        assert_eq!(1, archive("configured-reason", Some("configured".into())));
        assert_eq!(1, archive("whitespace-reason", Some(" ".into())));
        assert_eq!(1, archive("existing-reason", Some("replacement".into())));

        assert_eq!(
            Some("mailbox of a retired pane".to_string()),
            fields(&store, &default_reason).3
        );
        assert_eq!(
            Some("mailbox of a retired pane".to_string()),
            fields(&store, &empty_reason).3
        );
        assert_eq!(
            Some("configured".to_string()),
            fields(&store, &configured_reason).3
        );
        assert_eq!(Some(" ".to_string()), fields(&store, &whitespace_reason).3);
        assert_eq!(
            Some("keep existing reason".to_string()),
            fields(&store, &existing_reason).3
        );
    }

    #[test]
    fn retired_archive_does_not_commit_or_discard_a_foreign_transaction() {
        let mut connection = Connection::open_in_memory().expect("connection");
        connection.execute_batch("BEGIN").expect("foreign begin");
        let result =
            archive_on_connection(&mut connection, 172_800.0, 86_400.0, "retired", &|_| {
                panic!("foreign transaction must be refused before authorization")
            })
            .expect("refuse foreign transaction safely");
        assert_eq!(0, result);
        assert!(
            !connection.is_autocommit(),
            "the transaction remains caller-owned"
        );
        connection
            .execute_batch("ROLLBACK")
            .expect("caller rollback");
    }

    #[test]
    fn retired_archive_keeps_rows_younger_than_the_cutoff_queued() {
        let (_dir, store) = store();
        let older = send(&store, "old-lane", Some("w1:p1"));
        let younger = send(&store, "old-lane", Some("w1:p1"));
        store
            .writer
            .call({
                let older = older.clone();
                let younger = younger.clone();
                move |connection| {
                    connection
                        .execute(
                            "UPDATE messages SET created_at = ? WHERE id = ?",
                            params![86_399.0, older],
                        )
                        .map_err(crate::db::map_sqlite)?;
                    connection
                        .execute(
                            "UPDATE messages SET created_at = ? WHERE id = ?",
                            params![86_401.0, younger],
                        )
                        .map_err(crate::db::map_sqlite)?;
                    Ok(())
                }
            })
            .expect("age rows around the cutoff");
        let changed = store
            .archive_retired(
                RetiredArchiveOptions {
                    now: Some(172_800.0),
                    ..RetiredArchiveOptions::default()
                },
                eligible("old-lane"),
            )
            .expect("archive");
        assert_eq!(1, changed);
        assert_eq!("archived", fields(&store, &older).0);
        assert_eq!("queued", fields(&store, &younger).0);
    }

    #[test]
    fn retired_archive_holds_the_sqlite_writer_lock_during_revalidation() {
        let (_dir, store) = store();
        let id = send(&store, "old-lane", Some("w1:p1"));
        store
            .writer
            .call({
                let id = id.clone();
                move |connection| {
                    connection
                        .execute("UPDATE messages SET created_at = 1 WHERE id = ?", [id])
                        .map_err(crate::db::map_sqlite)?;
                    Ok(())
                }
            })
            .expect("age row");
        let path = store.path().to_path_buf();
        let calls = Arc::new(Mutex::new(0));
        let seen = Arc::clone(&calls);
        let observed = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&observed);
        let count = store
            .archive_retired(
                RetiredArchiveOptions {
                    now: Some(172_800.0),
                    ..RetiredArchiveOptions::default()
                },
                move |connection| {
                    let mut calls = seen.lock().expect("calls lock");
                    *calls += 1;
                    if *calls == 1 {
                        return Ok(vec!["old-lane".to_string()]);
                    }
                    // The fresh check runs inside BEGIN IMMEDIATE: a rival writer is refused.
                    let rival = Connection::open(&path).expect("rival connection");
                    let refused = rival
                        .execute("UPDATE messages SET subject = 'rival'", ())
                        .is_err();
                    record
                        .lock()
                        .expect("record lock")
                        .push((!connection.is_autocommit(), refused));
                    Ok(Vec::new())
                },
            )
            .expect("archive");
        assert_eq!(0, count);
        assert_eq!(vec![(true, true)], *observed.lock().expect("record lock"));
        assert_eq!("queued", fields(&store, &id).0);
    }
}
