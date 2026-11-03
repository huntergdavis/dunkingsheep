//! Concrete request-scoped turn-end text, delivery and metrics helpers.
use super::{outbox_runtime::FlockSnapshot, RequestRuntimeAdapter, TerminalCallAdapter};
use crate::flock::sends::{DirectSendOptions, DirectTextMetadata};
use crate::metrics::Event;
use serde_json::Value;

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)]
    pub(crate) fn helper_turndispatchenv_turn_end_text(&self, task: &Value) -> String {
        crate::dispatch::turn_end_text(task)
    }

    #[allow(dead_code)]
    pub(crate) fn helper_turndispatchenv_send_now(
        &self,
        pane: &str,
        text: &str,
    ) -> Result<Value, String> {
        let outbox = self
            .outbox
            .ok_or_else(|| "request outbox service is not attached".to_owned())?;
        let wait_s = self
            .terminal
            .flock
            .status_state()
            .map_err(|e| e.to_string())?
            .turn_end_wait_s;
        let mut env = TerminalCallAdapter {
            flock: self.terminal.flock,
            terminal: self.terminal.terminal,
            runtime: self.runtime,
        };
        outbox
            .flock
            .send_now(
                &Value::String(pane.to_owned()),
                text,
                DirectSendOptions {
                    wait_s,
                    ..DirectSendOptions::default()
                },
                DirectTextMetadata {
                    source: "turn_end",
                    ..DirectTextMetadata::default()
                },
                &mut env,
                outbox.pool.as_ref(),
                &FlockSnapshot,
                || None,
            )
            .map_err(|e| e.to_string())
    }

    #[allow(dead_code)]
    pub(crate) fn helper_turndispatchenv_record(
        &self,
        pane: &str,
        outcome: &str,
        reason: Option<&str>,
        bytes: Option<usize>,
    ) {
        if let Some(metrics) = self.terminal.flock.stores().metrics {
            metrics.record(&Event {
                kind: "turn.end.dispatch".into(),
                pane_id: Some(pane.into()),
                source: Some("turn.end".into()),
                outcome: Some(outcome.into()),
                reason: reason.map(str::to_owned),
                bytes: bytes.map(|n| i64::try_from(n).unwrap_or(i64::MAX)),
                ..Event::default()
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Env;
    use crate::flock::{panes::Stores, status_runtime::StatusState, Flock};
    use crate::inbox_store::Store;
    use crate::metrics::MetricsStore;
    use crate::server::RequestCtx;
    use crate::server_runtime::outbox_runtime::OutboxRuntime;
    use crate::terminal::{fakes::FakeTerminal, Terminal};
    use std::sync::{Arc, Mutex};

    #[test]
    fn production_env_turn_send_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        let inbox =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        let metrics = Arc::new(MetricsStore::open(dir.path().join("metrics.db")).unwrap());
        flock.attach_stores(Stores {
            inbox: Some(inbox),
            metrics: Some(Arc::clone(&metrics)),
            ..Stores::default()
        });
        flock
            .attach_status_state(StatusState::at_start(
                "test",
                dir.path().join("state"),
                dir.path().join("daemon"),
                Env::from_pairs(&[("DUNKINGSHEEP_TURN_END_WAIT_S", "0")]),
            ))
            .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
        )
        .unwrap();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let request = RequestCtx::default();
        let absent = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        assert!(absent
            .helper_turndispatchenv_send_now("w1:p1", "hello")
            .unwrap_err()
            .contains("not attached"));
        let other = Flock::new(
            dir.path().join("other"),
            dir.path().join("other-lanes"),
            false,
        );
        let mismatched = RequestRuntimeAdapter::new(&other, &request, runtime.handle(), &terminal);
        assert!(mismatched.with_outbox(&owner).is_err());
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal)
            .with_outbox(&owner)
            .unwrap();
        let task = serde_json::json!({"id":"t1","title":"example"});
        assert_eq!(
            adapter.helper_turndispatchenv_turn_end_text(&task),
            crate::dispatch::turn_end_text(&task)
        );
        assert!(adapter
            .helper_turndispatchenv_send_now("w1:p1", " ")
            .is_err());
        let lock = flock.send_lock_for("w1:p1");
        let guard = lock.lock().unwrap();
        let result = adapter
            .helper_turndispatchenv_send_now("w1:p1", "hello")
            .unwrap();
        assert_eq!(result["queued"], true);
        assert_eq!(result["source"], "turn_end");
        let rows = flock.outbox_snapshot();
        let row = &rows["w1:p1"][0];
        assert_eq!(row.id, result["message_id"].as_str().unwrap());
        assert_eq!(row.source, "turn_end");
        assert!(row.press_enter);
        drop(guard);
        owner.stop_and_join();
        assert!(adapter
            .helper_turndispatchenv_send_now("w1:p1", "again")
            .is_err());
        adapter.helper_turndispatchenv_record("w1:p1", "failed", Some("refused"), Some(5));
        let events = metrics
            .events_between(0.0, crate::clock::now_s() + 1.0)
            .unwrap();
        let event = events
            .iter()
            .find(|e| e.kind == "turn.end.dispatch")
            .unwrap();
        assert_eq!(event.source.as_deref(), Some("turn.end"));
        assert_eq!(event.outcome.as_deref(), Some("failed"));
        assert_eq!(event.reason.as_deref(), Some("refused"));
        assert_eq!(event.bytes, Some(5));
    }

    #[test]
    fn turn_send_wait_is_captured_and_invalid_values_refuse_startup() {
        let dir = tempfile::tempdir().unwrap();
        for (raw, expected) in [(None, 4.0), (Some(" 1_0 "), 10.0), (Some("0"), 0.0)] {
            let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
            let env = raw
                .map(|s| Env::from_pairs(&[("DUNKINGSHEEP_TURN_END_WAIT_S", s)]))
                .unwrap_or_default();
            flock
                .attach_status_state(StatusState::at_start(
                    "test",
                    dir.path().join("state"),
                    dir.path().join("daemon"),
                    env,
                ))
                .unwrap();
            assert_eq!(flock.status_state().unwrap().turn_end_wait_s, expected);
        }
        for raw in ["", "broken"] {
            let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
            let state = StatusState::at_start(
                "test",
                dir.path().join("state"),
                dir.path().join("daemon"),
                Env::from_pairs(&[("DUNKINGSHEEP_TURN_END_WAIT_S", raw)]),
            );
            assert!(flock.attach_status_state(state).is_err());
            assert!(flock.status_state().is_err());
        }
    }
}
