//! Production services for the Flock task reaper loop.

use std::collections::BTreeSet;

use serde_json::Value;

use crate::error::DunkError;
use crate::flock::lanes::pane_to_json;
use crate::flock::loop_env::{with_sync_store, LoopFuture};
use crate::server_runtime::LoopRuntimeAdapter;

impl LoopRuntimeAdapter {
    #[allow(dead_code)] // The complete ReaperEnv implementation is assembled by t1924.
    pub(crate) fn helper_reaperenv_list_panes(
        &self,
    ) -> LoopFuture<'_, Result<Vec<Value>, DunkError>> {
        Box::pin(async move {
            self.with_loop_terminal(|_, terminal| {
                terminal.with_terminal(|backend| {
                    backend
                        .try_list_panes_with_status()
                        .map(|panes| {
                            panes
                                .iter()
                                .map(|(pane, status)| pane_to_json(pane, status.as_deref()))
                                .collect()
                        })
                        .map_err(|error| DunkError::Refused(error.to_string()))
                })
            })
            .await
            .map_err(|error| {
                DunkError::Refused(format!("reaper terminal worker failed: {error}"))
            })?
        })
    }

    #[allow(dead_code)] // The complete ReaperEnv implementation is assembled by t1924.
    pub(crate) fn helper_reaperenv_is_live(&self) -> bool {
        // LoopRuntimeAdapter is constructed from configured_loop_terminal_factory, which
        // refuses fixture and retired backends. Runtime ReaperEnv instances therefore always
        // represent a live backend; fixture behavior is exercised through the loop test seam.
        true
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
        let path = self
            .flock
            .stores()
            .memory
            .map(|pool| pool.path().to_path_buf());
        Box::pin(async move {
            let Some(path) = path else {
                return;
            };
            let result = with_sync_store(move || {
                let mut connection = crate::db::open_writer(&path)?;
                let recovered = crate::memory::ingest::recover_leases(&mut connection)?;
                let partial = crate::memory::ingest::requeue_partial(&mut connection)?;
                Ok::<_, DunkError>((recovered, partial))
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
    use crate::db::{open_writer, ReadPool};
    use crate::flock::panes::Stores;
    use crate::flock::Flock;
    use crate::memory::ingest::{apply_full_schema, claim_next, enqueue, finish, Outcome};
    use crate::terminal::fakes::FakeTerminal;
    use crate::terminal::Terminal;

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
            memory: Some(Arc::new(ReadPool::open(&memory_path, 1).unwrap())),
            ..Stores::default()
        });
        let adapter = LoopRuntimeAdapter::new(flock, fake_terminal_factory());

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let panes = runtime
            .block_on(adapter.helper_reaperenv_list_panes())
            .unwrap();

        assert!(!panes.is_empty());
        assert!(panes
            .iter()
            .all(|pane| pane.get("pane_id").and_then(Value::as_str).is_some()));
        assert!(adapter.helper_reaperenv_is_live());
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
    async fn production_env_reaper_stores_uses_concrete_services() {
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
        flock.attach_stores(Stores {
            memory: Some(Arc::new(ReadPool::open(&db_path, 1).unwrap())),
            ..Stores::default()
        });
        let adapter = LoopRuntimeAdapter::new(flock, fake_terminal_factory());
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(adapter.helper_reaperenv_memory_maintenance());

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
