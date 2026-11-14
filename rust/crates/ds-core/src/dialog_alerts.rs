//! Durable, per-sink deduplication for delivery-blocking dialog alerts.
//!
//! Ports `dunk_dialog_alerts.notify` and its `alerts` table. Sink implementations
//! are passed explicitly; this module owns identity, retry timing and persistence,
//! and wires the owner and VP delivery paths through injectable boundaries.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::db::map_sqlite;
use crate::error::DunkError;
use crate::flock::commands_inbox::{InboxEnv, SendArgs};
use crate::flock::Flock;

/// The subject of every stuck-dialog alert row.
pub const ALERT_SUBJECT: &str = "Dialog blocks queued delivery";

/// One stuck-dialog alert as an inbox row from the watchdog, through the daemon's own inbox
/// store. `wake_quiet` notifies a recipient whose mailboxes are configured silent (the owner
/// lane's are); it is set by the daemon only and is not a wire argument.
fn post_inbox(
    flock: &Flock,
    env: &dyn InboxEnv,
    to: &str,
    body: &str,
    wake_quiet: bool,
) -> Result<SinkDelivery, String> {
    let args = SendArgs {
        to,
        body,
        subject: Some(ALERT_SUBJECT),
        attach: &[],
        notify: true,
        self_pane_id: None,
        system_sender: Some("watchdog"),
        agent_only: false,
        role_sender: None,
        wake_quiet,
    };
    let sent = flock
        .inbox_send(env, &args)
        .map_err(|error| error.to_string())?;
    if sent.outcome.as_deref() == Some("undeliverable") {
        Ok(SinkDelivery::Undeliverable)
    } else {
        Ok(SinkDelivery::Delivered)
    }
}

/// t4678: the owner sink. The alert is a row in the reserved owner mailbox (the owner's
/// conversation in the desk). No subprocess and no channel that can be retired underneath it.
pub fn post_owner_mailbox(
    flock: &Flock,
    env: &dyn InboxEnv,
    body: &str,
) -> Result<SinkDelivery, String> {
    post_inbox(
        flock,
        env,
        crate::flock::mailbox::OWNER_MAILBOX,
        body,
        false,
    )
}

/// t4678: the lane sink: one inbox notice to a pane that can act on the stuck lane (its project
/// coordinator, else the owner lane). A target that is gone is `Undeliverable`.
pub fn post_lane(
    flock: &Flock,
    env: &dyn InboxEnv,
    body: &str,
    target: &str,
    wake_quiet: bool,
) -> Result<SinkDelivery, String> {
    post_inbox(flock, env, target, body, wake_quiet)
}

/// The ledger name of the lane sink (the project coordinator, else the owner lane).
pub const LANE_SINK: &str = "coordinator";
pub const RETRY_S: f64 = 60.0;
/// After this many consecutive failures a sink rests for [`COOLDOWN_S`] before trying again.
pub const MAX_FAILURES: i64 = 5;
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
    /// Consecutive failures of this sink for this dialog, counting this attempt; 0 when
    /// delivered. The caller logs the first one as an error and stays quiet after that.
    pub failures: i64,
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

fn alert_body(pane_id: &str, signature: &str, stuck_s: Option<f64>) -> String {
    let visible: String = signature.chars().take(500).collect();
    let age = match stuck_s.filter(|seconds| seconds.is_finite() && *seconds >= 0.0) {
        Some(seconds) if seconds >= 120.0 => {
            format!(
                " It has been there for about {} minutes.",
                (seconds / 60.0) as i64
            )
        }
        Some(seconds) => format!(" It has been there for about {} seconds.", seconds as i64),
        None => String::new(),
    };
    format!(
        "Dunking Sheep: a dialog in {pane_id} needs attention and prevents message delivery.{age} \
Open that terminal to inspect the question. The lane's project coordinator may handle routine \
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
    // t4678: a ledger written before the failure bound existed gains the column in place.
    let has_failures: bool = connection
        .prepare("SELECT 1 FROM pragma_table_info('alerts') WHERE name='failures'")
        .and_then(|mut statement| statement.exists([]))
        .map_err(map_sqlite)?;
    if !has_failures {
        connection
            .execute(
                "ALTER TABLE alerts ADD COLUMN failures INTEGER NOT NULL DEFAULT 0",
                [],
            )
            .map_err(map_sqlite)?;
    }
    Ok(connection)
}

fn attempt_sink(
    db: &mut Connection,
    identity: &str,
    name: &'static str,
    now: f64,
    body: &str,
    sink: &mut dyn FnMut(&str) -> Result<SinkDelivery, String>,
) -> Result<Option<SinkAttempt>, DunkError> {
    let transaction = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    let prior: Option<(f64, Option<f64>, i64)> = transaction
        .query_row(
            "SELECT last_attempt, delivered, failures FROM alerts WHERE identity=?1 AND sink=?2",
            (identity, name),
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(map_sqlite)?;
    let suppressed = prior.is_some_and(|(last_attempt, delivered, failures)| {
        delivered.is_some_and(|delivered| now - delivered < COOLDOWN_S)
            || now - last_attempt < RETRY_S
            // A sink that keeps failing is not hammered every minute for ever: after
            // MAX_FAILURES in a row it rests for the cooldown, then starts a new round.
            || (failures >= MAX_FAILURES && now - last_attempt < COOLDOWN_S)
    });
    if suppressed {
        transaction.commit().map_err(map_sqlite)?;
        return Ok(None);
    }
    let earlier_failures = match prior {
        Some((_, None, failures)) if failures < MAX_FAILURES => failures,
        _ => 0,
    };
    transaction
        .execute(
            "INSERT INTO alerts(identity,sink,last_attempt,delivered) VALUES (?1,?2,?3,NULL) \
             ON CONFLICT(identity,sink) DO UPDATE SET last_attempt=excluded.last_attempt, delivered=NULL",
            (identity, name, now),
        )
        .map_err(map_sqlite)?;
    transaction.commit().map_err(map_sqlite)?;

    let outcome = sink(body);
    let delivered = matches!(outcome, Ok(SinkDelivery::Delivered));
    let failures = if delivered { 0 } else { earlier_failures + 1 };
    if delivered {
        db.execute(
            "UPDATE alerts SET delivered=?1, failures=0 WHERE identity=?2 AND sink=?3",
            (now, identity, name),
        )
        .map_err(map_sqlite)?;
    } else {
        db.execute(
            "UPDATE alerts SET failures=?1 WHERE identity=?2 AND sink=?3",
            (failures, identity, name),
        )
        .map_err(map_sqlite)?;
    }
    let error = match outcome {
        Ok(SinkDelivery::Delivered) => None,
        Ok(SinkDelivery::Undeliverable) => Some("notification target is unavailable".into()),
        Err(error) => Some(error),
    };
    Ok(Some(SinkAttempt {
        sink: name,
        delivered,
        error,
        failures,
    }))
}

/// Notify the owner and lane sinks independently for one pane/process/dialog identity.
///
/// Missing generation or signature skips without creating the database. Each sink's
/// `last_attempt` is committed before its callback runs. A callback returning
/// `Undeliverable` is treated as a failure; successful sinks are suppressed for 30m,
/// failed sinks retry after 60s. A crash after delivery but before the delivered
/// update may repeat an alert, preserving at-least-once delivery.
pub fn notify(
    pane_id: &str,
    generation: Option<&str>,
    signature: Option<&str>,
    owner_sink: &mut dyn FnMut(&str) -> Result<SinkDelivery, String>,
    lane_sink: &mut dyn FnMut(&str) -> Result<SinkDelivery, String>,
    path: Option<&Path>,
    now: Option<f64>,
) -> Result<Vec<SinkAttempt>, DunkError> {
    notify_stuck_for(
        pane_id, generation, signature, None, owner_sink, lane_sink, path, now,
    )
}

/// [`notify`] with how long the dialog has been stuck, which the alert text then states.
#[allow(clippy::too_many_arguments)]
pub fn notify_stuck_for(
    pane_id: &str,
    generation: Option<&str>,
    signature: Option<&str>,
    stuck_s: Option<f64>,
    owner_sink: &mut dyn FnMut(&str) -> Result<SinkDelivery, String>,
    lane_sink: &mut dyn FnMut(&str) -> Result<SinkDelivery, String>,
    path: Option<&Path>,
    now: Option<f64>,
) -> Result<Vec<SinkAttempt>, DunkError> {
    let Some(generation) = generation.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let Some(signature) = signature.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let identity = identity(pane_id, generation, signature);
    let body = alert_body(pane_id, signature, stuck_s);
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => default_path()?,
    };
    let now = now.unwrap_or_else(crate::clock::now_s);
    let _guard = lock();
    let mut db = open(&path)?;
    let mut results = Vec::new();

    if let Some(result) = attempt_sink(&mut db, &identity, "owner", now, &body, owner_sink)? {
        results.push(result);
    }
    if let Some(result) = attempt_sink(&mut db, &identity, LANE_SINK, now, &body, lane_sink)? {
        results.push(result);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ReadPool;
    use crate::flock::commands_inbox::{Nudge, RouteEntry};
    use crate::flock::mailbox::MailboxEnv;
    use crate::inbox_store::Store;
    use serde_json::{json, Value};
    use std::cell::Cell;

    struct InboxFixture {
        pool: ReadPool,
        store: Store,
        dir: tempfile::TempDir,
        nudges: Mutex<Vec<String>>,
        panes: Vec<Value>,
    }

    impl InboxFixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap();
            let pool = ReadPool::open(store.path(), 1).unwrap();
            Self {
                pool,
                store,
                dir,
                nudges: Mutex::new(Vec::new()),
                panes: vec![json!({
                    "pane_id": "w9:pH",
                    "terminal_id": "term_vp",
                    "label": "VP",
                    "tab_label": "VP",
                    "workspace_label": "fixture",
                    "agent": "codex"
                })],
            }
        }
    }

    impl MailboxEnv for InboxFixture {
        fn list_panes(&self) -> Vec<Value> {
            self.panes.clone()
        }
        fn list_panes_grouped(&self) -> Vec<Value> {
            self.panes.clone()
        }
        fn registered_pane(&self, _: &str) -> (bool, bool, Option<String>) {
            (false, false, None)
        }
    }

    impl InboxEnv for InboxFixture {
        fn caller_role(&self, _: &str) -> Result<String, DunkError> {
            Ok("system".into())
        }
        fn mailbox_override(&self) -> Option<String> {
            None
        }
        fn backend_name(&self) -> Result<String, DunkError> {
            Ok("fixture".into())
        }
        fn lane_panes(&self) -> Vec<String> {
            Vec::new()
        }
        fn pool(&self) -> &ReadPool {
            &self.pool
        }
        fn store(&self) -> &Store {
            &self.store
        }
        fn now(&self) -> f64 {
            100.0
        }
        fn record_metric(
            &self,
            _: &str,
            _: Option<&str>,
            _: &str,
            _: Option<&str>,
            _: Option<i64>,
            _: Option<i64>,
        ) {
        }
        fn pm_routing(&self) -> Vec<RouteEntry> {
            Vec::new()
        }
        fn fold_into_queued_notice(&self, _: &str, _: &str, _: &str) -> Option<String> {
            None
        }
        fn send_now(&self, _: &str, text: &str, _: &str) -> Result<Nudge, DunkError> {
            self.nudges.lock().unwrap().push(text.to_string());
            Ok(Nudge {
                queued: false,
                message: Some("fixture nudge".into()),
            })
        }
        fn record_sent(&self, _: Option<&str>, _: &str, _: Option<&str>, _: usize, _: usize) {}
        fn tasks_for_pane(&self, _: &str) -> Result<Value, DunkError> {
            Ok(json!([]))
        }
    }

    fn fixture_flock(fixture: &InboxFixture) -> Flock {
        Flock::new(
            fixture.dir.path().join("launches.json"),
            fixture.dir.path().join("lanes.json"),
            false,
        )
    }

    fn latest_message_id(fixture: &InboxFixture) -> String {
        fixture
            .pool
            .with(|connection| {
                connection
                    .query_row(
                        "SELECT id FROM messages ORDER BY rowid DESC LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(crate::db::map_sqlite)
            })
            .unwrap()
    }

    #[test]
    fn lane_sink_stores_watchdog_message_with_exact_fields_and_fake_nudge() {
        let fixture = InboxFixture::new();
        let flock = fixture_flock(&fixture);
        let body = "Dialog needs attention — λ";
        assert_eq!(
            SinkDelivery::Delivered,
            post_lane(&flock, &fixture, body, "w9:pH", false).unwrap()
        );
        assert_eq!(1, fixture.nudges.lock().unwrap().len());
        let message_id = latest_message_id(&fixture);
        let (summary, stored_body) =
            crate::inbox::get(&fixture.pool, &message_id, fixture.now()).unwrap();
        assert_eq!(body, stored_body);
        assert_eq!(Some("watchdog"), summary.sender.as_deref());
        assert_eq!(
            Some("Dialog blocks queued delivery"),
            summary.subject.as_deref()
        );
        assert_eq!("VP", summary.mailbox);
    }

    #[test]
    fn lane_sink_maps_undeliverable_and_propagates_store_errors() {
        let fixture = InboxFixture::new();
        let flock = fixture_flock(&fixture);
        assert_eq!(
            SinkDelivery::Undeliverable,
            post_lane(&flock, &fixture, "body", "missing-vp-mailbox", false).unwrap()
        );
        assert!(post_lane(&flock, &fixture, "body", "", false).is_err());
        let too_large = "x".repeat(crate::inbox::MAX_BODY_BYTES + 1);
        assert!(post_lane(&flock, &fixture, &too_large, "w9:pH", false).is_err());
    }

    #[test]
    fn real_vp_store_retries_independently_of_failed_owner_sink() {
        let fixture = InboxFixture::new();
        let flock = fixture_flock(&fixture);
        let (_directory, path) = path();
        let owner_calls = Cell::new(0);
        let mut owner = |_: &str| {
            owner_calls.set(owner_calls.get() + 1);
            if owner_calls.get() == 1 {
                Err("owner process failed".into())
            } else {
                Ok(SinkDelivery::Delivered)
            }
        };
        let mut vp = |body: &str| post_lane(&flock, &fixture, body, "w9:pH", false);
        let first = notify(
            "w1:p1",
            Some("pid:1:start:1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(2, first.len());
        assert!(!first[0].delivered);
        assert!(first[1].delivered);
        assert!(notify(
            "w1:p1",
            Some("pid:1:start:1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(159.0),
        )
        .unwrap()
        .is_empty());
        let retry = notify(
            "w1:p1",
            Some("pid:1:start:1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(160.0),
        )
        .unwrap();
        assert_eq!(
            vec!["owner"],
            retry.iter().map(|attempt| attempt.sink).collect::<Vec<_>>()
        );
        assert_eq!(2, owner_calls.get());
        let counts = crate::inbox::count(&fixture.pool, "VP", &[], fixture.now()).unwrap();
        assert_eq!(
            1, counts.unread,
            "successful VP mail was not repeated during owner retry"
        );
    }

    #[test]
    fn real_undeliverable_vp_retries_without_repeating_successful_owner() {
        let fixture = InboxFixture::new();
        let flock = fixture_flock(&fixture);
        let (_directory, path) = path();
        let owner_calls = Cell::new(0);
        let mut owner = |_: &str| {
            owner_calls.set(owner_calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        let mut vp = |body: &str| post_lane(&flock, &fixture, body, "missing-vp-mailbox", false);
        let first = notify(
            "w1:p1",
            Some("pid:1:start:2"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(2, first.len());
        assert!(first[0].delivered);
        assert_eq!(
            Some("notification target is unavailable"),
            first[1].error.as_deref()
        );
        assert!(notify(
            "w1:p1",
            Some("pid:1:start:2"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(159.0),
        )
        .unwrap()
        .is_empty());
        let retry = notify(
            "w1:p1",
            Some("pid:1:start:2"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(160.0),
        )
        .unwrap();
        assert_eq!(
            vec![LANE_SINK],
            retry.iter().map(|attempt| attempt.sink).collect::<Vec<_>>()
        );
        assert_eq!(1, owner_calls.get());
    }

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
            failures: 0,
        }
    }

    fn successful_sink(_: &str) -> Result<SinkDelivery, String> {
        Ok(SinkDelivery::Delivered)
    }

    #[test]
    fn durable_dedupe_is_per_pane_generation_and_dialog() {
        let (_directory, path) = path();
        let owner_calls = Cell::new(0);
        let vp_calls = Cell::new(0);
        let notify = |pane, generation, signature, now| {
            let mut owner = |_: &str| {
                owner_calls.set(owner_calls.get() + 1);
                Ok(SinkDelivery::Delivered)
            };
            let mut vp = |_: &str| {
                vp_calls.set(vp_calls.get() + 1);
                Ok(SinkDelivery::Delivered)
            };
            super::notify(
                pane,
                Some(generation),
                Some(signature),
                &mut owner,
                &mut vp,
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
        let mut owner = successful_sink;
        let mut vp = successful_sink;
        assert_eq!(
            2,
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                &mut owner,
                &mut vp,
                Some(&path),
                Some(100.0)
            )
            .unwrap()
            .len()
        );
        assert!(notify(
            "%2",
            Some("g1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(1899.0)
        )
        .unwrap()
        .is_empty());
        assert_eq!(
            2,
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                &mut owner,
                &mut vp,
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
        let mut owner = |_: &str| {
            owner_calls.set(owner_calls.get() + 1);
            if owner_calls.get() == 1 {
                Err("offline".into())
            } else {
                Ok(SinkDelivery::Delivered)
            }
        };
        let mut vp = |_: &str| {
            vp_calls.set(vp_calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        let first = notify(
            "%2",
            Some("g1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(
            vec![
                SinkAttempt {
                    sink: "owner",
                    delivered: false,
                    error: Some("offline".into()),
                    failures: 1,
                },
                delivered(LANE_SINK),
            ],
            first
        );
        assert!(notify(
            "%2",
            Some("g1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(159.0)
        )
        .unwrap()
        .is_empty());
        assert_eq!(
            vec![delivered("owner")],
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                &mut owner,
                &mut vp,
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
        let mut owner = successful_sink;
        let mut vp = |_: &str| {
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
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(
            Some("notification target is unavailable".to_string()),
            first[1].error
        );
        assert_eq!(
            vec![delivered(LANE_SINK)],
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                &mut owner,
                &mut vp,
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
        let mut owner = |_: &str| {
            calls.set(calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        let mut vp = |_: &str| {
            calls.set(calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        assert!(notify(
            "%2",
            None,
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0)
        )
        .unwrap()
        .is_empty());
        assert!(notify(
            "%2",
            Some("g1"),
            Some(""),
            &mut owner,
            &mut vp,
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
        let mut owner = |_: &str| {
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
        let mut vp = successful_sink;
        let report = notify(
            "%2",
            Some("g1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(
            "crash after reservation",
            report[0].error.as_deref().unwrap()
        );
    }

    /// t4678: the owner sink is a row in the reserved owner mailbox, written through the inbox
    /// store: no subprocess, and no pane is typed into.
    #[test]
    fn owner_sink_stores_one_watchdog_row_in_the_owner_mailbox() {
        let fixture = InboxFixture::new();
        let flock = fixture_flock(&fixture);
        assert_eq!(
            SinkDelivery::Delivered,
            post_owner_mailbox(&flock, &fixture, "a dialog in %304 needs attention").unwrap()
        );
        let message_id = latest_message_id(&fixture);
        let (summary, body) = crate::inbox::get(&fixture.pool, &message_id, fixture.now()).unwrap();
        assert_eq!(crate::flock::mailbox::OWNER_MAILBOX, summary.mailbox);
        assert_eq!(Some("watchdog"), summary.sender.as_deref());
        assert_eq!(Some(ALERT_SUBJECT), summary.subject.as_deref());
        assert_eq!("a dialog in %304 needs attention", body);
        assert!(
            fixture.nudges.lock().unwrap().is_empty(),
            "the owner mailbox has no pane to type into"
        );
    }

    /// t4678: the alert says which pane and for how long, and no longer names the VP.
    #[test]
    fn alert_text_names_the_pane_and_how_long_it_has_been_stuck() {
        let minutes = alert_body("%304", "Rate limit reached. Continue?", Some(1500.0));
        assert!(
            minutes.contains("a dialog in %304 needs attention"),
            "{minutes}"
        );
        assert!(
            minutes.contains("It has been there for about 25 minutes."),
            "{minutes}"
        );
        assert!(minutes.ends_with("Visible dialog: Rate limit reached. Continue?"));
        assert!(!minutes.contains("VP"), "{minutes}");
        let seconds = alert_body("%304", "q", Some(45.0));
        assert!(seconds.contains("for about 45 seconds."), "{seconds}");
        assert!(!alert_body("%304", "q", None).contains("It has been there"));
    }

    /// t4678: a sink that keeps failing is retried a bounded number of times, then rests for
    /// the cooldown; each attempt carries its failure count so only the first is logged.
    #[test]
    fn a_failing_sink_is_bounded_and_counts_its_failures() {
        let (_directory, path) = path();
        let lane_calls = Cell::new(0);
        let mut owner = successful_sink;
        let mut lane = |_: &str| {
            lane_calls.set(lane_calls.get() + 1);
            Ok(SinkDelivery::Undeliverable)
        };
        let mut counts = Vec::new();
        let mut now = 100.0;
        // One attempt a minute for an hour: far more ticks than the bound allows attempts.
        for _ in 0..60 {
            for attempt in notify(
                "w1:p1",
                Some("pid:1:start:1"),
                Some("question"),
                &mut owner,
                &mut lane,
                Some(&path),
                Some(now),
            )
            .unwrap()
            {
                if attempt.sink == LANE_SINK {
                    counts.push(attempt.failures);
                }
            }
            now += RETRY_S;
        }
        // Five in a row, a 30-minute rest, then a new round that starts counting at one again.
        assert_eq!(vec![1, 2, 3, 4, 5, 1, 2, 3, 4, 5], counts);
        assert_eq!(10, lane_calls.get());
    }

    /// t4678: a ledger written by an earlier daemon (no `failures` column) is upgraded in place
    /// and its rows keep their meaning.
    #[test]
    fn an_older_ledger_gains_the_failure_column_without_losing_rows() {
        let (_directory, path) = path();
        {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let old = Connection::open(&path).unwrap();
            old.execute_batch(
                "CREATE TABLE alerts (identity TEXT NOT NULL, sink TEXT NOT NULL, \
                 last_attempt REAL NOT NULL, delivered REAL, PRIMARY KEY(identity, sink));",
            )
            .unwrap();
            old.execute(
                "INSERT INTO alerts VALUES (?1, 'owner', 100.0, 100.0)",
                [identity("w1:p1", "pid:1:start:1", "question")],
            )
            .unwrap();
        }
        let mut owner = successful_sink;
        let mut lane = successful_sink;
        let attempts = notify(
            "w1:p1",
            Some("pid:1:start:1"),
            Some("question"),
            &mut owner,
            &mut lane,
            Some(&path),
            Some(160.0),
        )
        .unwrap();
        // The owner row delivered at 100 is still inside its cooldown; only the lane sink runs.
        assert_eq!(vec![delivered(LANE_SINK)], attempts);
    }
}
