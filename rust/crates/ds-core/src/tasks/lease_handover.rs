//! Keeping a pane's task leases across a guarded respawn (t4473 slice D).
//!
//! A lease names the process that holds it (`caller_pid`, `caller_start`), and the reaper
//! requeues a lease whose process is gone. A respawn replaces that process on purpose, so a
//! lease kept across it is first DETACHED (no process, and a deadline at least `hold_s` away:
//! the reaper treats a lease with no process as unattributable, never as dead), then BOUND to
//! the new process once the respawn is verified. If the respawn fails, the detached lease
//! simply runs to its deadline, as an unheartbeated lease does today.

use rusqlite::params;

use super::Tasks;
use crate::db;
use crate::error::DunkError;

impl Tasks {
    /// Detach every live lease of `pane_id` from its process and keep it valid for at least
    /// `hold_s`. Returns the task ids, in order.
    pub async fn detach_leases_for_respawn(
        &self,
        pane_id: &str,
        now: f64,
        hold_s: f64,
    ) -> Result<Vec<String>, DunkError> {
        let pane = pane_id.to_string();
        self.writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                let ids: Vec<String> = {
                    let mut stmt = tx
                        .prepare(
                            "SELECT task_id FROM task_leases WHERE pane_id = ? AND expires_at > ? \
                             ORDER BY task_id",
                        )
                        .map_err(db::map_sqlite)?;
                    let rows = stmt
                        .query_map(params![pane, now], |row| row.get(0))
                        .map_err(db::map_sqlite)?;
                    rows.collect::<Result<Vec<_>, _>>()
                        .map_err(db::map_sqlite)?
                };
                for id in &ids {
                    tx.execute(
                        "UPDATE task_leases SET caller_pid = NULL, caller_start = NULL, \
                         expires_at = MAX(expires_at, ?) WHERE task_id = ? AND pane_id = ?",
                        params![now + hold_s, id, pane],
                    )
                    .map_err(db::map_sqlite)?;
                }
                tx.commit().map_err(db::map_sqlite)?;
                Ok(ids)
            })
            .await
    }

    /// Bind the listed leases of `pane_id` to the process that now runs there, as a heartbeat
    /// from it would. Returns how many leases were bound.
    pub async fn bind_leases_after_respawn(
        &self,
        pane_id: &str,
        task_ids: &[String],
        caller: (i64, String, Option<String>),
        now: f64,
        lease_s: f64,
    ) -> Result<usize, DunkError> {
        let (pane, ids) = (pane_id.to_string(), task_ids.to_vec());
        self.writer
            .call(move |conn| {
                let tx = conn
                    .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                    .map_err(db::map_sqlite)?;
                let (pid, start, name) = caller;
                let mut bound = 0;
                for id in &ids {
                    bound += tx
                        .execute(
                            "UPDATE task_leases SET caller_pid = ?, caller_start = ?, caller_name = ?, \
                             heartbeat_at = ?, expires_at = MAX(expires_at, ?) \
                             WHERE task_id = ? AND pane_id = ?",
                            params![pid, start, name, now, now + lease_s, id, pane],
                        )
                        .map_err(db::map_sqlite)?;
                }
                tx.commit().map_err(db::map_sqlite)?;
                Ok(bound)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::ClaimOptions;

    #[tokio::test]
    async fn a_lease_is_detached_then_bound_to_the_new_process_and_survives_the_reaper() {
        let dir = tempfile::tempdir().unwrap();
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let task = tasks.add("global", "held", "body").await.unwrap();
        tasks
            .claim_next("global", "%5", ClaimOptions::default())
            .await
            .unwrap();
        let now = crate::clock::now_s();

        let held = tasks
            .detach_leases_for_respawn("%5", now, 600.0)
            .await
            .unwrap();
        assert_eq!(held, std::slice::from_ref(&task.id));
        let lease = &tasks.leases_for_pane("%5", Some(now)).await.unwrap()[0];
        assert_eq!((lease.caller_pid, lease.caller_start.clone()), (None, None));
        assert!(lease.expires_at >= now + 600.0);
        // The old process is gone: a lease with no process is not "dead caller".
        let reaped = tasks
            .reap(None, Some(Box::new(|_, _| false)), Some(now + 1.0), None)
            .await
            .unwrap();
        assert!(reaped.is_empty(), "{reaped:?}");

        let bound = tasks
            .bind_leases_after_respawn(
                "%5",
                &held,
                (4242, "777".into(), Some("claude".into())),
                now,
                900.0,
            )
            .await
            .unwrap();
        assert_eq!(bound, 1);
        let lease = &tasks.leases_for_pane("%5", Some(now)).await.unwrap()[0];
        assert_eq!(
            (lease.caller_pid, lease.caller_start.as_deref()),
            (Some(4242), Some("777"))
        );
        // Another pane's lease and an unlisted id are left alone.
        assert_eq!(
            tasks
                .bind_leases_after_respawn("%6", &held, (1, "1".into(), None), now, 900.0)
                .await
                .unwrap(),
            0
        );
        assert!(tasks
            .detach_leases_for_respawn("%9", now, 600.0)
            .await
            .unwrap()
            .is_empty());
    }
}
