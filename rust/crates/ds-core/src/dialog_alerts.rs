//! Durable, per-sink deduplication for delivery-blocking dialog alerts.
//!
//! Ports `dunk_dialog_alerts.notify` and its `alerts` table. Sink implementations
//! are passed explicitly; this module owns identity, retry timing and persistence,
//! but does not wire a production owner or VP delivery path.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::db::map_sqlite;
use crate::error::DunkError;

pub const RETRY_S: f64 = 60.0;
pub const COOLDOWN_S: f64 = 1800.0;

static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock() -> MutexGuard<'static, ()> {
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkDelivery {
    Delivered,
    Undeliverable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkAttempt {
    pub sink: &'static str,
    pub delivered: bool,
    pub error: Option<String>,
}

fn default_path() -> Result<PathBuf, DunkError> {
    if let Some(directory) =
        std::env::var_os("DUNKINGSHEEP_DIR").filter(|directory| !directory.is_empty())
    {
        return Ok(PathBuf::from(directory).join("dialog-alerts.db"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| DunkError::Refused("HOME is not set for dialog alert storage".into()))?;
    Ok(PathBuf::from(home)
        .join(".config/dunkingsheep")
        .join("dialog-alerts.db"))
}

fn identity(pane_id: &str, generation: &str, signature: &str) -> String {
    let input = format!("{pane_id}\0{generation}\0{signature}");
    format!("{:x}", Sha256::digest(input.as_bytes()))
}

fn alert_body(pane_id: &str, signature: &str) -> String {
    let visible: String = signature.chars().take(500).collect();
    format!(
        "Dunking Sheep: a dialog in {pane_id} needs attention and prevents message delivery. \
Open that terminal to inspect the question. The VP may handle routine \
questions within existing authority; approvals needing the owner remain \
for the owner. No answer or key has been sent automatically.\n\n\
Visible dialog: {visible}"
    )
}

fn open(path: &Path) -> Result<Connection, DunkError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path).map_err(map_sqlite)?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(map_sqlite)?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS alerts (
                identity TEXT NOT NULL,
                sink TEXT NOT NULL,
                last_attempt REAL NOT NULL,
                delivered REAL,
                PRIMARY KEY(identity, sink)
            )",
        )
        .map_err(map_sqlite)?;
    Ok(connection)
}

/// Notify the owner and VP sinks independently for one pane/process/dialog identity.
///
/// Missing generation or signature skips without creating the database. Each sink's
/// `last_attempt` is committed before its callback runs. A callback returning
/// `Undeliverable` is treated as a failure; successful sinks are suppressed for 30m,
/// failed sinks retry after 60s. A crash after delivery but before the delivered
/// update may repeat an alert, preserving at-least-once delivery.
pub fn notify<OwnerSink, VpSink>(
    pane_id: &str,
    generation: Option<&str>,
    signature: Option<&str>,
    mut owner_sink: OwnerSink,
    mut vp_sink: VpSink,
    path: Option<&Path>,
    now: Option<f64>,
) -> Result<Vec<SinkAttempt>, DunkError>
where
    OwnerSink: FnMut(&str) -> Result<SinkDelivery, String>,
    VpSink: FnMut(&str) -> Result<SinkDelivery, String>,
{
    let Some(generation) = generation.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let Some(signature) = signature.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let identity = identity(pane_id, generation, signature);
    let body = alert_body(pane_id, signature);
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => default_path()?,
    };
    let now = now.unwrap_or_else(crate::clock::now_s);
    let _guard = lock();
    let mut db = open(&path)?;
    let mut results = Vec::new();

    for (name, sink) in [
        (
            "owner",
            &mut owner_sink as &mut dyn FnMut(&str) -> Result<SinkDelivery, String>,
        ),
        (
            "vp",
            &mut vp_sink as &mut dyn FnMut(&str) -> Result<SinkDelivery, String>,
        ),
    ] {
        let transaction = db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(map_sqlite)?;
        let prior: Option<(f64, Option<f64>)> = transaction
            .query_row(
                "SELECT last_attempt, delivered FROM alerts WHERE identity=?1 AND sink=?2",
                (&identity, name),
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(map_sqlite)?;
        let suppressed = prior.is_some_and(|(last_attempt, delivered)| {
            delivered.is_some_and(|delivered| now - delivered < COOLDOWN_S)
                || now - last_attempt < RETRY_S
        });
        if suppressed {
            transaction.commit().map_err(map_sqlite)?;
            continue;
        }
        transaction
            .execute(
                "INSERT INTO alerts(identity,sink,last_attempt,delivered) VALUES (?1,?2,?3,NULL) \
                 ON CONFLICT(identity,sink) DO UPDATE SET last_attempt=excluded.last_attempt, delivered=NULL",
                (&identity, name, now),
            )
            .map_err(map_sqlite)?;
        transaction.commit().map_err(map_sqlite)?;

        match sink(&body) {
            Ok(SinkDelivery::Delivered) => {
                db.execute(
                    "UPDATE alerts SET delivered=?1 WHERE identity=?2 AND sink=?3",
                    (now, &identity, name),
                )
                .map_err(map_sqlite)?;
                results.push(SinkAttempt {
                    sink: name,
                    delivered: true,
                    error: None,
                });
            }
            Ok(SinkDelivery::Undeliverable) => results.push(SinkAttempt {
                sink: name,
                delivered: false,
                error: Some("notification target is unavailable".into()),
            }),
            Err(error) => results.push(SinkAttempt {
                sink: name,
                delivered: false,
                error: Some(error),
            }),
        }
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn path() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/alerts.db");
        (directory, path)
    }

    fn delivered(sink: &'static str) -> SinkAttempt {
        SinkAttempt {
            sink,
            delivered: true,
            error: None,
        }
    }

    #[test]
    fn durable_dedupe_is_per_pane_generation_and_dialog() {
        let (_directory, path) = path();
        let owner_calls = Cell::new(0);
        let vp_calls = Cell::new(0);
        let mut notify = |pane, generation, signature, now| {
            super::notify(
                pane,
                Some(generation),
                Some(signature),
                |_| {
                    owner_calls.set(owner_calls.get() + 1);
                    Ok(SinkDelivery::Delivered)
                },
                |_| {
                    vp_calls.set(vp_calls.get() + 1);
                    Ok(SinkDelivery::Delivered)
                },
                Some(&path),
                Some(now),
            )
            .unwrap()
        };

        assert_eq!(
            2,
            notify("%2", "pid:1:start:2", "Allow command?", 100.0).len()
        );
        assert!(notify("%2", "pid:1:start:2", "Allow command?", 999.0).is_empty());
        assert_eq!(
            2,
            notify("%2", "pid:1:start:2", "Approve network?", 1000.0).len()
        );
        assert_eq!(
            2,
            notify("%2", "pid:2:start:3", "Allow command?", 1001.0).len()
        );
        assert_eq!(
            2,
            notify("%3", "pid:1:start:2", "Allow command?", 1002.0).len()
        );
        assert_eq!(4, owner_calls.get());
        assert_eq!(4, vp_calls.get());
        assert!(
            path.exists(),
            "notify creates the database parent directory"
        );
    }

    #[test]
    fn recurrent_identical_dialog_realerts_at_the_thirty_minute_boundary() {
        let (_directory, path) = path();
        let sink = |_| Ok(SinkDelivery::Delivered);
        assert_eq!(
            2,
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                sink,
                sink,
                Some(&path),
                Some(100.0)
            )
            .unwrap()
            .len()
        );
        let sink = |_| Ok(SinkDelivery::Delivered);
        assert!(notify(
            "%2",
            Some("g1"),
            Some("question"),
            sink,
            sink,
            Some(&path),
            Some(1899.0)
        )
        .unwrap()
        .is_empty());
        let sink = |_| Ok(SinkDelivery::Delivered);
        assert_eq!(
            2,
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                sink,
                sink,
                Some(&path),
                Some(1900.0)
            )
            .unwrap()
            .len()
        );
    }

    #[test]
    fn failed_owner_retries_after_sixty_seconds_without_repeating_successful_vp() {
        let (_directory, path) = path();
        let owner_calls = Cell::new(0);
        let vp_calls = Cell::new(0);
        let owner = |_| {
            owner_calls.set(owner_calls.get() + 1);
            if owner_calls.get() == 1 {
                Err("offline".into())
            } else {
                Ok(SinkDelivery::Delivered)
            }
        };
        let vp = |_| {
            vp_calls.set(vp_calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        let first = notify(
            "%2",
            Some("g1"),
            Some("question"),
            owner,
            vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(
            vec![
                SinkAttempt {
                    sink: "owner",
                    delivered: false,
                    error: Some("offline".into())
                },
                delivered("vp"),
            ],
            first
        );
        let owner = |_| {
            owner_calls.set(owner_calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        let vp = |_| {
            vp_calls.set(vp_calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        assert!(notify(
            "%2",
            Some("g1"),
            Some("question"),
            owner,
            vp,
            Some(&path),
            Some(159.0)
        )
        .unwrap()
        .is_empty());
        let owner = |_| {
            owner_calls.set(owner_calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        let vp = |_| {
            vp_calls.set(vp_calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        assert_eq!(
            vec![delivered("owner")],
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                owner,
                vp,
                Some(&path),
                Some(160.0)
            )
            .unwrap()
        );
        assert_eq!(2, owner_calls.get());
        assert_eq!(1, vp_calls.get());
    }

    #[test]
    fn undeliverable_sink_is_failed_and_retried() {
        let (_directory, path) = path();
        let vp_calls = Cell::new(0);
        let owner = |_| Ok(SinkDelivery::Delivered);
        let vp = |_| {
            vp_calls.set(vp_calls.get() + 1);
            if vp_calls.get() == 1 {
                Ok(SinkDelivery::Undeliverable)
            } else {
                Ok(SinkDelivery::Delivered)
            }
        };
        let first = notify(
            "%2",
            Some("g1"),
            Some("question"),
            owner,
            vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(
            Some("notification target is unavailable".to_string()),
            first[1].error
        );
        let owner = |_| Ok(SinkDelivery::Delivered);
        let vp = |_| {
            vp_calls.set(vp_calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        assert_eq!(
            vec![delivered("vp")],
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                owner,
                vp,
                Some(&path),
                Some(160.0)
            )
            .unwrap()
        );
        assert_eq!(2, vp_calls.get());
    }

    #[test]
    fn unidentified_dialog_skips_without_creating_storage_or_calling_sinks() {
        let (directory, path) = path();
        let calls = Cell::new(0);
        let sink = |_| {
            calls.set(calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        assert!(notify(
            "%2",
            None,
            Some("question"),
            sink,
            sink,
            Some(&path),
            Some(100.0)
        )
        .unwrap()
        .is_empty());
        assert!(notify(
            "%2",
            Some("g1"),
            Some(""),
            sink,
            sink,
            Some(&path),
            Some(100.0)
        )
        .unwrap()
        .is_empty());
        assert_eq!(0, calls.get());
        assert!(!directory.path().join("nested").exists());
    }

    #[test]
    fn last_attempt_is_committed_before_the_sink_runs() {
        let (_directory, path) = path();
        let path_for_sink = path.clone();
        let owner = |_| {
            let connection = Connection::open(&path_for_sink).unwrap();
            let (last_attempt, delivered): (f64, Option<f64>) = connection
                .query_row(
                    "SELECT last_attempt, delivered FROM alerts WHERE sink='owner'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(100.0, last_attempt);
            assert_eq!(None, delivered);
            Err("crash after reservation".into())
        };
        let vp = |_| Ok(SinkDelivery::Delivered);
        let report = notify(
            "%2",
            Some("g1"),
            Some("question"),
            owner,
            vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(
            "crash after reservation",
            report[0].error.as_deref().unwrap()
        );
    }
}
