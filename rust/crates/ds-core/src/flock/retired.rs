//! Retired-mailbox archive eligibility and identity locking for the task reaper.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use rusqlite::Connection;
use serde_json::Value;

use super::Flock;
use crate::error::DunkError;
use crate::inbox_store::RetiredArchiveOptions;
use crate::lock::{Guard, Mode};
use crate::unicode_casefold::full_casefold;
use crate::{config, inbox, panes};

const IDENTITY_LOCK_TIMEOUT: Duration = Duration::from_secs(5);

impl Flock {
    /// Revalidate retired pane identities and archive their old inbox rows.
    /// Missing or ambiguous pane identity is a safe no-op; storage and lock
    /// failures are returned so the reaper can contain and log them.
    pub fn archive_retired_inbox(&self, pane_list: &[Value]) -> Result<usize, DunkError> {
        let retired_path = panes::default_path(&config::Env::from_process())?;
        self.archive_retired_inbox_at(pane_list, &retired_path, RetiredArchiveOptions::default())
    }

    fn archive_retired_inbox_at(
        &self,
        pane_list: &[Value],
        retired_path: &Path,
        options: RetiredArchiveOptions,
    ) -> Result<usize, DunkError> {
        if pane_list.is_empty() {
            return Ok(0);
        }
        let mut live = Vec::with_capacity(pane_list.len());
        let mut protected_live = Vec::new();
        for pane in pane_list {
            let Some(pane_id) = pane.get("pane_id").and_then(Value::as_str) else {
                return Ok(0);
            };
            if pane_id.trim().is_empty() {
                return Ok(0);
            }
            live.push(pane_id.to_string());
            for field in ["pane_id", "label", "tab_label", "agent_name", "name"] {
                if let Some(value) = pane
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                {
                    protected_live.push(value.to_string());
                }
            }
        }

        let stores = self.stores();
        let Some(store) = stores.inbox else {
            return Err(DunkError::Refused("inbox store is unavailable".into()));
        };
        let lane_path = self.lane_registry_path.clone();
        let mut lock_paths = vec![
            appended_lock_path(retired_path),
            appended_lock_path(&lane_path),
        ];
        lock_paths.sort();
        lock_paths.dedup();
        let _guards = acquire_shared_guards(&lock_paths)?;

        let retired_path = retired_path.to_path_buf();
        let live_for_auth = live.clone();
        let protected_live_for_auth = protected_live.clone();
        let lane_path_for_auth = lane_path.clone();
        let authorize = move |connection: &Connection| {
            eligible_now(
                connection,
                &retired_path,
                &lane_path_for_auth,
                &live_for_auth,
                &protected_live_for_auth,
            )
        };
        store.archive_retired(options, authorize)
    }
}

fn appended_lock_path(path: &Path) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(".lock");
    PathBuf::from(value)
}

fn acquire_shared_guards(paths: &[PathBuf]) -> Result<Vec<Guard>, DunkError> {
    acquire_shared_guards_with_timeout(paths, IDENTITY_LOCK_TIMEOUT)
}

fn acquire_shared_guards_with_timeout(
    paths: &[PathBuf],
    timeout: Duration,
) -> Result<Vec<Guard>, DunkError> {
    let deadline = Instant::now() + timeout;
    let mut guards = Vec::with_capacity(paths.len());
    for path in paths {
        loop {
            match Guard::try_acquire(path, Mode::Shared)? {
                Some(guard) => {
                    guards.push(guard);
                    break;
                }
                None if Instant::now() >= deadline => {
                    return Err(DunkError::Retryable(format!(
                        "identity record is busy: {} stayed locked for {:?}",
                        path.display(),
                        timeout
                    )));
                }
                None => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }
    Ok(guards)
}

fn eligible_now(
    connection: &Connection,
    retired_path: &Path,
    lane_path: &Path,
    live: &[String],
    protected_live: &[String],
) -> Result<Vec<String>, DunkError> {
    let records = panes::valid_retirements(&panes::all_retired(retired_path));
    let registry = match read_lane_registry(lane_path) {
        Ok(registry) => registry,
        Err(_) => return Ok(Vec::new()),
    };
    if registry
        .values()
        .any(|row| Flock::row_identity(Some(row)).0.is_none())
    {
        return Ok(Vec::new());
    }

    let mut protected = protected_live.to_vec();
    let mut effective = Vec::new();
    for record in records {
        if let Some(row) = registry.get(&record.pane_id) {
            let (name, declared) = Flock::row_identity(Some(row));
            let declared_at = row.get("declared_at").and_then(Value::as_f64);
            let aliases: BTreeSet<String> = panes::retirement_aliases(&record)
                .into_iter()
                .map(|value| full_casefold(&inbox::normalise_mailbox(&value)))
                .collect();
            let matching_declaration = declared
                && name
                    .as_ref()
                    .map(|value| aliases.contains(&full_casefold(&inbox::normalise_mailbox(value))))
                    .unwrap_or(false)
                && declared_at
                    .map(|at| at.is_finite() && at > 0.0 && at < record.retired_at)
                    .unwrap_or(false);
            if !matching_declaration {
                protected.extend(panes::retirement_aliases(&record));
                continue;
            }
        }
        effective.push(record);
    }

    let retired_ids: BTreeSet<String> = effective.iter().map(|row| row.pane_id.clone()).collect();
    for (pane_id, row) in &registry {
        if !retired_ids.contains(pane_id) || live.iter().any(|value| value == pane_id) {
            protected.push(pane_id.clone());
            if let Some(name) = Flock::row_identity(Some(row)).0 {
                protected.push(name);
            }
        }
    }

    let candidates = {
        let mut statement = connection
            .prepare(
                "SELECT DISTINCT mailbox FROM messages WHERE state != 'cancelled' ORDER BY mailbox",
            )
            .map_err(crate::db::map_sqlite)?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(crate::db::map_sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(crate::db::map_sqlite)?
    };
    let bindings = {
        let mut statement = connection
            .prepare("SELECT DISTINCT mailbox, pane_id FROM messages WHERE pane_id IS NOT NULL")
            .map_err(crate::db::map_sqlite)?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(crate::db::map_sqlite)?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(crate::db::map_sqlite)?
    };
    for (mailbox, pane_id) in bindings {
        if !retired_ids.contains(&pane_id) || live.iter().any(|value| value == &pane_id) {
            protected.push(mailbox);
        }
    }

    // A mailbox claimed by multiple retirement records is ambiguous even if
    // neither pane is currently live. Protect the shared normalized spelling.
    let mut alias_owners: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for record in &effective {
        for alias in panes::retirement_aliases(record) {
            let normalized = full_casefold(&inbox::normalise_mailbox(&alias));
            alias_owners
                .entry(normalized)
                .or_default()
                .insert(record.pane_id.clone());
        }
    }
    protected.extend(
        alias_owners
            .into_iter()
            .filter(|(_, owners)| owners.len() > 1)
            .map(|(alias, _)| alias),
    );

    Ok(panes::retired_mailboxes(
        &candidates,
        &effective,
        live,
        &protected,
    ))
}

fn read_lane_registry(path: &Path) -> Result<serde_json::Map<String, Value>, DunkError> {
    let text = std::fs::read_to_string(path)
        .map_err(|error| DunkError::Refused(format!("{}: {error}", path.display())))?;
    match serde_json::from_str::<Value>(&text)
        .map_err(|error| DunkError::Refused(format!("{}: {error}", path.display())))?
    {
        Value::Object(registry) => Ok(registry),
        _ => Err(DunkError::Refused("lane registry must be an object".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_store::{Outgoing, Store};
    use serde_json::json;
    use std::sync::Arc;

    struct Fixture {
        _dir: tempfile::TempDir,
        flock: Flock,
        store: Arc<Store>,
        retired_path: PathBuf,
        lane_path: PathBuf,
    }

    fn new_fixture() -> Fixture {
        let dir = tempfile::tempdir().expect("tempdir");
        let retired_path = dir.path().join("retired-panes.json");
        let lane_path = dir.path().join("lane-registry.json");
        let store = Arc::new(
            Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).expect("store"),
        );
        let flock = Flock::new(dir.path().join("launches.json"), lane_path.clone(), false);
        flock.stores.lock().expect("stores lock").inbox = Some(Arc::clone(&store));
        Fixture {
            _dir: dir,
            flock,
            store,
            retired_path,
            lane_path,
        }
    }

    fn retirement(fixture: &Fixture, retired_at: f64) {
        panes::retire(
            "w1:p9",
            "replaced",
            None,
            &["old-lane"],
            &["old-lane"],
            &fixture.retired_path,
            Some(retired_at),
        )
        .expect("retire");
    }

    fn declared_lane(fixture: &Fixture, name: &str, declared_at: f64) {
        crate::atomic_json::write(
            &fixture.lane_path,
            &json!({
                "w1:p9": {"mailbox": name, "declared": true, "declared_at": declared_at}
            }),
        )
        .expect("write lane registry");
    }

    fn send_old(fixture: &Fixture, pane_id: &str) -> String {
        send_old_to(fixture, "old-lane", pane_id)
    }

    fn send_old_to(fixture: &Fixture, mailbox: &str, pane_id: &str) -> String {
        let id = fixture
            .store
            .send(&Outgoing {
                mailbox,
                body: Some("queued"),
                pane_id: Some(pane_id),
                ..Outgoing::default()
            })
            .expect("send");
        let connection = rusqlite::Connection::open(fixture.store.path()).expect("test writer");
        connection
            .execute("UPDATE messages SET created_at = 1 WHERE id = ?", [&id])
            .expect("age row");
        id
    }

    fn state(fixture: &Fixture, id: &str) -> String {
        crate::inbox::status(fixture.store.read_pool(), id, 172_800.0)
            .expect("read status")
            .state
    }

    fn run(fixture: &Fixture, live: Value) -> Result<usize, DunkError> {
        fixture.flock.archive_retired_inbox_at(
            live.as_array().expect("live array"),
            &fixture.retired_path,
            RetiredArchiveOptions {
                age_s: 86_400.0,
                now: Some(172_800.0),
                reason: None,
            },
        )
    }

    #[test]
    fn flock_archive_retired_revalidates_declared_identity_and_keeps_active_bindings() {
        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "old-lane", 99.0);
        let old = send_old(&fixture, "w1:p9");
        assert_eq!(1, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("archived", state(&fixture, &old));

        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "old-lane", 100.0); // declaration is not older than retirement
        let stale = send_old(&fixture, "w1:p9");
        assert_eq!(0, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("queued", state(&fixture, &stale));

        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "old-lane", 99.0);
        let active = send_old(&fixture, "w1:p1");
        assert_eq!(0, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("queued", state(&fixture, &active));
    }

    #[test]
    fn flock_archive_retired_fails_closed_for_live_and_unknown_lane_identity() {
        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "old-lane", 99.0);
        let live = send_old(&fixture, "w1:p9");
        assert_eq!(0, run(&fixture, json!([{"pane_id":"w1:p9"}])).unwrap());
        assert_eq!("queued", state(&fixture, &live));

        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        crate::atomic_json::write(&fixture.lane_path, &json!([])).expect("bad registry");
        let unknown = send_old(&fixture, "w1:p9");
        assert_eq!(0, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("queued", state(&fixture, &unknown));

        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        crate::atomic_json::write(
            &fixture.lane_path,
            &json!({"w1:p9":{"mailbox":"[inbox] notice", "declared":true, "declared_at":99.0}}),
        )
        .expect("unusable identity");
        let unknown = send_old(&fixture, "w1:p9");
        assert_eq!(0, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("queued", state(&fixture, &unknown));

        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        let unknown = send_old(&fixture, "w1:p9");
        assert_eq!(0, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("queued", state(&fixture, &unknown));

        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "old-lane", 99.0);
        let missing_id = send_old(&fixture, "w1:p9");
        assert_eq!(0, run(&fixture, json!([{"label":"unknown"}])).unwrap());
        assert_eq!("queued", state(&fixture, &missing_id));
    }

    #[test]
    fn flock_archive_retired_uses_python_truthiness_for_mailbox_fallback() {
        let fixture = new_fixture();
        panes::retire(
            "w1:p9",
            "replaced",
            None,
            &["0"],
            &["0"],
            &fixture.retired_path,
            Some(100.0),
        )
        .expect("retire numeric mailbox alias");
        crate::atomic_json::write(
            &fixture.lane_path,
            &json!({
                "w1:p9": {"mailbox": 0, "declared": true, "declared_at": 99.0}
            }),
        )
        .expect("write falsey mailbox registry row");
        let unknown = send_old_to(&fixture, "0", "w1:p9");
        assert_eq!(0, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("queued", state(&fixture, &unknown));

        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        crate::atomic_json::write(
            &fixture.lane_path,
            &json!({
                "w1:p9": {
                    "mailbox": 0,
                    "name": "old-lane",
                    "declared": true,
                    "declared_at": 99.0
                }
            }),
        )
        .expect("write fallback name registry row");
        let eligible = send_old(&fixture, "w1:p9");
        assert_eq!(1, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("archived", state(&fixture, &eligible));
    }

    #[test]
    fn flock_archive_retired_protects_shared_aliases() {
        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        panes::retire(
            "w1:p8",
            "replaced",
            None,
            &["old-lane"],
            &["old-lane"],
            &fixture.retired_path,
            Some(100.0),
        )
        .expect("second retire");
        crate::atomic_json::write(
            &fixture.lane_path,
            &json!({
                "w1:p9": {"mailbox":"old-lane", "declared":true, "declared_at":99.0},
                "w1:p8": {"mailbox":"old-lane", "declared":true, "declared_at":99.0}
            }),
        )
        .expect("write lanes");
        let old = send_old(&fixture, "w1:p9");
        assert_eq!(0, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("queued", state(&fixture, &old));
    }

    #[test]
    fn flock_archive_retired_casefolds_declared_aliases_like_python() {
        let fixture = new_fixture();
        panes::retire(
            "w1:p9",
            "replaced",
            None,
            &["STRASSE"],
            &["STRASSE"],
            &fixture.retired_path,
            Some(100.0),
        )
        .expect("retire");
        declared_lane(&fixture, "Straße", 99.0);
        let old = send_old_to(&fixture, "STRASSE", "w1:p9");
        assert_eq!(1, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("archived", state(&fixture, &old));
    }

    #[test]
    fn flock_archive_retired_casefolds_live_labels_before_protecting_mailbox() {
        let fixture = new_fixture();
        panes::retire(
            "w1:p9",
            "replaced",
            None,
            &["STRASSE"],
            &["STRASSE"],
            &fixture.retired_path,
            Some(100.0),
        )
        .expect("retire");
        declared_lane(&fixture, "STRASSE", 99.0);
        let old = send_old_to(&fixture, "STRASSE", "w1:p9");
        assert_eq!(
            0,
            run(&fixture, json!([{"pane_id":"w1:p1", "label":"Straße"}])).unwrap()
        );
        assert_eq!("queued", state(&fixture, &old));
    }

    #[test]
    fn flock_archive_retired_refuses_contended_identity_guard() {
        let fixture = new_fixture();
        let path = appended_lock_path(&fixture.retired_path);
        let held = Guard::try_acquire(&path, Mode::Exclusive)
            .unwrap()
            .expect("exclusive guard");
        let error = acquire_shared_guards_with_timeout(
            std::slice::from_ref(&path),
            Duration::from_millis(20),
        )
        .expect_err("contended guard refuses");
        assert!(error.to_string().contains("identity record is busy"));
        drop(held);
        assert!(acquire_shared_guards_with_timeout(
            std::slice::from_ref(&path),
            Duration::from_millis(20),
        )
        .is_ok());
    }

    /// Runs the sweep's own authorization with `change` between the preview and the
    /// fresh check inside the transaction. `change` runs on the writer thread after the
    /// preview is computed, so it may change files and rows but must not call the Store.
    fn run_with_boundary_change<C>(fixture: &Fixture, change: C) -> Result<usize, DunkError>
    where
        C: Fn(&Connection) + Send + 'static,
    {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let retired_path = fixture.retired_path.clone();
        let lane_path = fixture.lane_path.clone();
        let live = vec!["w1:p1".to_string()];
        let calls = AtomicUsize::new(0);
        fixture.store.archive_retired(
            RetiredArchiveOptions {
                age_s: 86_400.0,
                now: Some(172_800.0),
                reason: None,
            },
            move |connection: &Connection| {
                let authorized = eligible_now(connection, &retired_path, &lane_path, &live, &live)?;
                if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                    change(connection);
                }
                Ok(authorized)
            },
        )
    }

    #[test]
    fn flock_retired_archive_rechecks_a_binding_added_at_the_boundary() {
        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "old-lane", 99.0);
        let old = send_old(&fixture, "w1:p9");
        let rebound = old.clone();
        let count = run_with_boundary_change(&fixture, move |connection| {
            connection
                .execute(
                    "UPDATE messages SET pane_id = 'w1:p1' WHERE id = ?",
                    [&rebound],
                )
                .expect("bind the old row to a live pane");
        })
        .unwrap();
        assert_eq!(0, count);
        assert_eq!("queued", state(&fixture, &old));
    }

    #[test]
    fn flock_retired_archive_rechecks_a_retirement_revoked_at_the_boundary() {
        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "old-lane", 99.0);
        let old = send_old(&fixture, "w1:p9");
        let retired_path = fixture.retired_path.clone();
        let count = run_with_boundary_change(&fixture, move |_| {
            crate::atomic_json::write(&retired_path, &json!({})).expect("revoke the retirement");
        })
        .unwrap();
        assert_eq!(0, count);
        assert_eq!("queued", state(&fixture, &old));
    }

    #[test]
    fn flock_retired_archive_rechecks_a_registry_identity_changed_at_the_boundary() {
        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "old-lane", 99.0);
        let old = send_old(&fixture, "w1:p9");
        let lane_path = fixture.lane_path.clone();
        let count = run_with_boundary_change(&fixture, move |_| {
            // Re-declared at the retirement time: not strictly older, so no longer evidence.
            crate::atomic_json::write(
                &lane_path,
                &json!({"w1:p9": {"mailbox": "old-lane", "declared": true, "declared_at": 100.0}}),
            )
            .expect("re-declare the lane");
        })
        .unwrap();
        assert_eq!(0, count);
        assert_eq!("queued", state(&fixture, &old));
    }

    #[test]
    fn flock_retired_archive_protects_a_declared_name_that_no_longer_matches() {
        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "new-lane", 99.0);
        let old = send_old(&fixture, "w1:p9");
        assert_eq!(0, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("queued", state(&fixture, &old));
    }

    #[test]
    fn flock_retired_archive_is_idempotent_across_sweeps() {
        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "old-lane", 99.0);
        let old = send_old(&fixture, "w1:p9");
        assert_eq!(1, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("archived", state(&fixture, &old));
        assert_eq!(0, run(&fixture, json!([{"pane_id":"w1:p1"}])).unwrap());
        assert_eq!("archived", state(&fixture, &old));
    }

    #[test]
    fn flock_retired_archive_rechecks_a_retirement_time_changed_at_the_boundary() {
        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "old-lane", 99.0);
        let old = send_old(&fixture, "w1:p9");
        let retired_path = fixture.retired_path.clone();
        let count = run_with_boundary_change(&fixture, move |_| {
            // Retired at 99: the declaration at 99 is no longer strictly older.
            panes::retire(
                "w1:p9",
                "replaced",
                None,
                &["old-lane"],
                &["old-lane"],
                &retired_path,
                Some(99.0),
            )
            .expect("re-retire at the boundary");
        })
        .unwrap();
        assert_eq!(0, count);
        assert_eq!("queued", state(&fixture, &old));
    }

    #[test]
    fn flock_retired_archive_rechecks_a_retirement_alias_changed_at_the_boundary() {
        let fixture = new_fixture();
        retirement(&fixture, 100.0);
        declared_lane(&fixture, "old-lane", 99.0);
        let old = send_old(&fixture, "w1:p9");
        let retired_path = fixture.retired_path.clone();
        let count = run_with_boundary_change(&fixture, move |_| {
            // The retired pane now claims another mailbox, so the declared name no longer matches.
            panes::retire(
                "w1:p9",
                "replaced",
                None,
                &["other-lane"],
                &["other-lane"],
                &retired_path,
                Some(100.0),
            )
            .expect("re-retire with another alias");
        })
        .unwrap();
        assert_eq!(0, count);
        assert_eq!("queued", state(&fixture, &old));
    }
}
