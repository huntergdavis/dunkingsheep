//! Production services for the Flock task reaper loop.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::error::DunkError;
use crate::flock::lanes::pane_to_json;
use crate::flock::loop_env::{with_sync_store, LoopFuture};
use crate::server_runtime::LoopRuntimeAdapter;

impl LoopRuntimeAdapter {
    pub(crate) fn helper_reaperenv_pane_snapshot(
        &self,
    ) -> LoopFuture<'_, (bool, Result<Vec<Value>, DunkError>)> {
        Box::pin(async move {
            match self
                .with_loop_terminal(|_, terminal| {
                    terminal.with_terminal(|backend| {
                        let live = backend.is_live_backend();
                        let panes = backend
                            .try_list_panes_with_status()
                            .map(|panes| {
                                panes
                                    .iter()
                                    .map(|(pane, status)| pane_to_json(pane, status.as_deref()))
                                    .collect()
                            })
                            .map_err(|error| DunkError::Refused(error.to_string()));
                        (live, panes)
                    })
                })
                .await
            {
                Ok(snapshot) => snapshot,
                Err(error) => (
                    false,
                    Err(DunkError::Refused(format!(
                        "reaper terminal worker failed: {error}"
                    ))),
                ),
            }
        })
    }

    #[allow(dead_code)] // The complete ReaperEnv implementation is assembled by t1924.
    pub(crate) fn helper_reaperenv_inbox_maintenance<'a>(
        &'a self,
        panes: &'a BTreeSet<String>,
        pane_list: &'a [Value],
    ) -> LoopFuture<'a, ()> {
        let flock = std::sync::Arc::clone(&self.flock);
        let panes = panes.clone();
        let pane_list = pane_list.to_vec();
        let stores = flock.stores();
        Box::pin(async move {
            let Some(inbox) = stores.inbox else {
                eprintln!("inbox maintenance failed: inbox store is not attached");
                return;
            };
            let metrics = stores.metrics;
            let result = with_sync_store(move || {
                match inbox.mark_undeliverable(&panes) {
                    Ok(marked) if marked.undeliverable != 0 => eprintln!(
                        "inbox: {} queued message(s) marked undeliverable for dead panes {}",
                        marked.undeliverable,
                        marked
                            .panes
                            .iter()
                            .take(5)
                            .map(String::as_str)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    Ok(_) => {}
                    Err(error) => eprintln!("inbox undeliverable sweep failed: {error}"),
                }

                let archived = match inbox.auto_archive_undeliverable(None, None) {
                    Ok(count) => count,
                    Err(error) => {
                        eprintln!("inbox auto-archive failed: {error}");
                        return;
                    }
                };

                match flock.archive_retired_inbox(&pane_list) {
                    Ok(count) if count != 0 => {
                        if let Some(metrics) = &metrics {
                            metrics.record(&crate::metrics::Event {
                                kind: "inbox.archived".into(),
                                source: Some("inbox".into()),
                                outcome: Some("ok".into()),
                                reason: Some("retired_mailbox_24h".into()),
                                ..Default::default()
                            });
                        }
                        eprintln!(
                            "inbox: auto-archived {count} retired-mailbox message(s) older than 24h"
                        );
                    }
                    Ok(_) => {}
                    Err(error) => eprintln!("retired-mailbox archive failed: {error}"),
                }

                if archived != 0 {
                    if let Some(metrics) = &metrics {
                        metrics.record(&crate::metrics::Event {
                            kind: "inbox.archived".into(),
                            source: Some("inbox".into()),
                            outcome: Some("ok".into()),
                            reason: Some("undeliverable_24h".into()),
                            ..Default::default()
                        });
                    }
                    eprintln!(
                        "inbox: auto-archived {archived} undeliverable message(s) older than 24h"
                    );
                }
            })
            .await;
            if let Err(error) = result {
                eprintln!("inbox maintenance worker failed: {error}");
            }
        })
    }

    #[allow(dead_code)] // The complete ReaperEnv implementation is assembled by t1924.
    pub(crate) fn helper_reaperenv_clear_historical_errors<'a>(
        &'a self,
        panes: &'a BTreeSet<String>,
    ) -> LoopFuture<'a, ()> {
        let flock = std::sync::Arc::clone(&self.flock);
        let panes = panes.clone();
        Box::pin(async move {
            match with_sync_store(move || flock.clear_historical_errors(&panes)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("historical dunk error cleanup failed: {error}"),
                Err(error) => eprintln!("historical dunk error worker failed: {error}"),
            }
        })
    }

    #[allow(dead_code)] // The complete ReaperEnv implementation is assembled by t1924.
    pub(crate) fn helper_reaperenv_memory_maintenance(&self) -> LoopFuture<'_, ()> {
        let service = self.flock.memory_service();
        Box::pin(async move {
            let Some(service) = service else {
                return;
            };
            let result = with_sync_store(move || {
                // spawn_blocking retains a Tokio handle. SyncWriter owns a runtime, so
                // execute on an ordinary thread while only the blocking pool waits.
                std::thread::Builder::new()
                    .name("memory-reaper".into())
                    .spawn(move || {
                        let recovered = service.recover_leases()?;
                        let partial = service.requeue_partial()?;
                        Ok::<_, DunkError>((recovered, partial))
                    })
                    .map_err(|error| {
                        DunkError::Refused(format!("memory maintenance thread: {error}"))
                    })?
                    .join()
                    .map_err(|_| DunkError::Refused("memory maintenance thread panicked".into()))?
            })
            .await;
            match result {
                Ok(Ok((recovered, partial))) => {
                    if recovered != 0 || partial != 0 {
                        eprintln!(
                            "memory maintenance recovered {recovered} lease(s) and requeued {partial} partial source(s)"
                        );
                    }
                }
                Ok(Err(error)) => eprintln!("memory maintenance failed: {error}"),
                Err(error) => eprintln!("memory maintenance worker failed: {error}"),
            }
        })
    }

    #[allow(dead_code)] // The complete ReaperEnv implementation is assembled by t1924.
    pub(crate) fn helper_reaperenv_set_last_message(&self, message: &str) {
        self.flock.set_last_message(message);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::json;

    use super::*;
    use crate::db::open_writer;
    use crate::flock::panes::Stores;
    use crate::flock::Flock;
    use crate::memory::ingest::{apply_full_schema, claim_next, enqueue, finish, Outcome};
    use crate::memory::service::MemoryService;
    use crate::terminal::fakes::FakeTerminal;
    use crate::terminal::Terminal;
    use std::time::Duration;

    fn flock(dir: &std::path::Path) -> Arc<Flock> {
        Arc::new(Flock::new(
            dir.join("launches.json"),
            dir.join("lanes.json"),
            false,
        ))
    }

    fn fake_terminal_factory() -> Arc<dyn Fn() -> Box<dyn Terminal> + Send + Sync> {
        Arc::new(|| Box::new(FakeTerminal::new()))
    }

    #[test]
    fn production_env_reaper_adapter_calls_every_helper() {
        let dir = tempfile::tempdir().unwrap();
        let flock = flock(dir.path());
        let for_assert = Arc::clone(&flock);
        let inbox = Arc::new(
            crate::inbox_store::Store::open(dir.path().join("inbox.db"), dir.path().join("blobs"))
                .unwrap(),
        );
        let memory_path = dir.path().join("memory.db");
        let memory_writer = open_writer(&memory_path).unwrap();
        apply_full_schema(&memory_writer).unwrap();
        drop(memory_writer);
        flock.attach_stores(Stores {
            inbox: Some(inbox),
            ..Stores::default()
        });
        flock
            .attach_memory_service(Arc::new(
                MemoryService::open(&memory_path, 1, Duration::from_millis(500), Duration::ZERO)
                    .unwrap(),
            ))
            .unwrap();
        let adapter = LoopRuntimeAdapter::new(flock, fake_terminal_factory());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let panes = runtime.block_on(adapter.helper_reaperenv_pane_snapshot());
        let (live, panes) = panes;
        let panes = panes.unwrap();

        assert!(!panes.is_empty());
        assert!(panes
            .iter()
            .all(|pane| pane.get("pane_id").and_then(Value::as_str).is_some()));
        assert!(!live);
        runtime.block_on(async {
            adapter
                .helper_reaperenv_clear_historical_errors(&BTreeSet::new())
                .await;
            adapter
                .helper_reaperenv_inbox_maintenance(&BTreeSet::new(), &[])
                .await;
            adapter.helper_reaperenv_memory_maintenance().await;
        });
        adapter.helper_reaperenv_set_last_message("reaper status");
        assert_eq!("reaper status", for_assert.last_message());
    }

    #[tokio::test]
    async fn reaper_runtime_clears_only_historical_errors_for_exact_live_panes() {
        let dir = tempfile::tempdir().unwrap();
        let flock = flock(dir.path());
        let payload = json!({"next_id": 3, "dunkers": [
            {"id":"d1", "running":true, "target_pane_id":"%1", "last_error":"target pane not found"},
            {"id":"d2", "running":true, "target_pane_id":"%2", "last_error":"target pane not found"}
        ]});
        flock._load_dunks(payload.as_object().unwrap());
        let adapter = LoopRuntimeAdapter::new(Arc::clone(&flock), fake_terminal_factory());
        adapter
            .helper_reaperenv_clear_historical_errors(&BTreeSet::from(["%1".to_string()]))
            .await;
        let rows = flock.dunks.list();
        let error =
            |id: &str| rows.iter().find(|row| row["id"] == id).unwrap()["last_error"].clone();
        assert_eq!(error("d1"), Value::Null);
        assert_eq!(error("d2"), json!("target pane not found"));
    }

    #[tokio::test]
    async fn reaper_runtime_uses_backend_provenance_from_the_listing_worker() {
        use crate::tasks::{ClaimOptions, Tasks};
        use crate::terminal::tmux::TmuxBackend;

        let dir = tempfile::tempdir().unwrap();
        let flock = flock(dir.path());
        let tasks = Arc::new(Tasks::open(dir.path().join("tasks.db")).await.unwrap());
        flock.attach_stores(Stores {
            tasks: Some(Arc::clone(&tasks)),
            ..Stores::default()
        });
        let task = tasks.add("global", "held", "").await.unwrap();
        tasks
            .claim_next(
                "global",
                "%1",
                ClaimOptions {
                    caller: Some((i32::MAX as i64, Some("1".into()), Some("pi".into()))),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        let fixture = LoopRuntimeAdapter::new(Arc::clone(&flock), fake_terminal_factory());
        let tick = flock.reaper_tick(&fixture).await;
        assert!(tick.skipped_not_live);
        assert_eq!(tasks.get(&task.id).await.unwrap().status, "in_progress");

        // A real tmux backend remains live even when its command cannot list panes.
        let unavailable = dir.path().join("no-such-tmux");
        let live = LoopRuntimeAdapter::new(
            Arc::clone(&flock),
            Arc::new(move || Box::new(TmuxBackend::new(unavailable.clone()))),
        );
        let tick = flock.reaper_tick(&live).await;
        assert!(!tick.skipped_not_live);
        assert_eq!(tick.panes, None);
        assert_eq!(tick.requeued.len(), 1);
        assert_eq!(tasks.get(&task.id).await.unwrap().status, "open");
    }

    #[test]
    fn production_env_reaper_stores_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("memory.db");
        let first_path = dir.path().join("expired.jsonl");
        let second_path = dir.path().join("partial.jsonl");
        std::fs::write(&first_path, "{}\n").unwrap();
        std::fs::write(&second_path, "{}\n").unwrap();
        let mut writer = open_writer(&db_path).unwrap();
        apply_full_schema(&writer).unwrap();
        enqueue(
            &mut writer,
            first_path.to_str().unwrap(),
            Some("generic"),
            Some("demo"),
        )
        .unwrap();
        claim_next(&mut writer, -1.0).unwrap().unwrap();
        enqueue(
            &mut writer,
            second_path.to_str().unwrap(),
            Some("generic"),
            Some("demo"),
        )
        .unwrap();
        let partial = claim_next(&mut writer, 60.0).unwrap().unwrap();
        finish(
            &mut writer,
            &partial,
            Outcome::Skipped("stopped by operator"),
            &json!({}),
        )
        .unwrap();
        drop(writer);

        let flock = flock(dir.path());
        let service = Arc::new(
            MemoryService::open(&db_path, 1, Duration::from_millis(500), Duration::ZERO).unwrap(),
        );
        flock.attach_memory_service(Arc::clone(&service)).unwrap();
        let adapter = LoopRuntimeAdapter::new(flock, fake_terminal_factory());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(adapter.helper_reaperenv_memory_maintenance());

        let connection = open_writer(&db_path).unwrap();
        let queued: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM ingest_queue WHERE state='queued'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(2, queued, "expired leases and stopped partials requeue");
    }

    #[test]
    fn production_env_reaper_maintenance_uses_concrete_services() {
        use crate::inbox_store::{Outgoing, Store};

        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        let dead = store
            .send(&Outgoing {
                mailbox: "%dead",
                pane_id: Some("%dead"),
                body: Some("must be retired"),
                ..Outgoing::default()
            })
            .unwrap();
        let live = store
            .send(&Outgoing {
                mailbox: "%live",
                pane_id: Some("%live"),
                body: Some("must remain queued"),
                ..Outgoing::default()
            })
            .unwrap();
        let writer = open_writer(store.path()).unwrap();
        writer
            .execute(
                "UPDATE messages SET created_at = 0 WHERE id IN (?1, ?2)",
                rusqlite::params![dead, live],
            )
            .unwrap();
        drop(writer);

        let flock = flock(dir.path());
        flock.attach_stores(Stores {
            inbox: Some(Arc::clone(&store)),
            ..Stores::default()
        });
        let adapter = LoopRuntimeAdapter::new(flock, fake_terminal_factory());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(
                adapter.helper_reaperenv_inbox_maintenance(
                    &BTreeSet::from(["%live".to_string()]),
                    &[],
                ),
            );

        let connection = crate::db::open_reader(store.path()).unwrap();
        let state = |id: &str| {
            connection
                .query_row("SELECT state FROM messages WHERE id = ?1", [id], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap()
        };
        assert_eq!("archived", state(&dead));
        assert_eq!("queued", state(&live));
    }
}
