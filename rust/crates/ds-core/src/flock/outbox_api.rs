//! Python's list/get/cancel operations on the canonical outbox and history.
use super::panes::PaneInfoEnv;
use super::sends::{outbox_lock, read_screen, SendEnv, SnapshotSaveFailure, SnapshotStore};
use super::Flock;
use crate::api::python_value_repr;
use crate::error::DunkError;
use serde_json::{json, Value};
use std::collections::HashMap;

impl Flock {
    pub(crate) fn list_messages<E: SendEnv + PaneInfoEnv>(&self, env: &E) -> Value {
        let (mut queued, recent) = {
            let outbox = outbox_lock(&self.outbox);
            let history = self
                .message_history
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let queued: Vec<_> = outbox
                .values()
                .flatten()
                .filter(|m| m.status == "queued")
                .map(|m| m.to_dict())
                .collect();
            let recent: Vec<_> = history.iter().rev().map(|m| m.to_dict()).collect();
            (queued, recent)
        };
        queued.sort_by(|a, b| {
            a["created_at"]
                .as_f64()
                .unwrap_or_default()
                .partial_cmp(&b["created_at"].as_f64().unwrap_or_default())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut boxes = HashMap::new();
        for message in &mut queued {
            let pane = message["pane_id"].as_str().unwrap_or_default().to_owned();
            let content = boxes.entry(pane.clone()).or_insert_with(|| {
                read_screen(env, &pane, None, true)
                    .and_then(|screen| self.recovery_box_text(env, &pane, &screen))
                    .unwrap_or_default()
            });
            message["blocked_by"] = if content.is_empty() {
                Value::Null
            } else if self.looks_like_ours(&pane, content) {
                json!("guarded send pending")
            } else {
                json!("input box contains a draft")
            };
        }
        json!({"queued": queued, "recent": recent})
    }

    pub(crate) fn get_message(&self, id: &Value) -> Result<Value, DunkError> {
        let outbox = outbox_lock(&self.outbox);
        for message in outbox.values().flatten() {
            if id.as_str() == Some(message.id.as_str()) {
                return Ok(message.to_dict());
            }
        }
        let history = self
            .message_history
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for message in history.iter() {
            if id.as_str() == Some(message.id.as_str()) {
                return Ok(message.to_dict());
            }
        }
        Err(DunkError::Refused(format!(
            "no message with id {}",
            python_value_repr(id)
        )))
    }

    pub(crate) fn cancel_message(
        &self,
        id: &Value,
        store: &dyn SnapshotStore,
    ) -> Result<Value, DunkError> {
        let message = {
            let mut outbox = outbox_lock(&self.outbox);
            let found = outbox.values_mut().find_map(|queue| {
                queue
                    .iter()
                    .position(|m| id.as_str() == Some(m.id.as_str()))
                    .map(|index| (queue, index))
            });
            if let Some((queue, index)) = found {
                let message = &mut queue[index];
                if message.status != "queued" {
                    return Err(DunkError::Refused(format!(
                        "message {} already {}",
                        message.id, message.status
                    )));
                }
                message.cancel_at(crate::clock::now_s());
                let message = queue.remove(index);
                self.remember(&message);
                // Python cancellation releases the Event, but does not record delivery receipts.
                // A callback which already captured this lifetime may still run to completion.
                message.done.settle();
                message
            } else {
                let history = self
                    .message_history
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(message) = history.iter().find(|m| id.as_str() == Some(m.id.as_str())) {
                    return Err(DunkError::Refused(format!(
                        "message {} already {}",
                        message.id, message.status
                    )));
                }
                return Err(DunkError::Refused(format!(
                    "no queued message with id {}",
                    python_value_repr(id)
                )));
            }
        };
        match store.save_classified(self) {
            Ok(()) | Err(SnapshotSaveFailure::Io(_)) => Ok(message.to_dict()),
            Err(SnapshotSaveFailure::Other(error)) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flock::sends::Message;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct SaveProbe {
        io: bool,
        calls: AtomicUsize,
    }
    impl SnapshotStore for SaveProbe {
        fn save(&self, _: &Flock) -> Result<(), DunkError> {
            unreachable!()
        }
        fn save_classified(&self, flock: &Flock) -> Result<(), SnapshotSaveFailure> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            assert!(flock.outbox_snapshot()["p"].is_empty());
            assert_eq!(
                flock.get_message(&json!("m")).unwrap()["status"],
                "cancelled"
            );
            assert!(flock.message_history.lock().unwrap()[0].done.is_set());
            let error = DunkError::Refused("sentinel save failure".into());
            Err(if self.io {
                SnapshotSaveFailure::Io(error)
            } else {
                SnapshotSaveFailure::Other(error)
            })
        }
    }

    #[test]
    fn outbox_cancel_ignores_only_classified_filesystem_failure_after_transition() {
        for io in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let flock = Flock::new(
                dir.path().join("launches.json"),
                dir.path().join("lanes.json"),
                false,
            );
            let message = Message::new("m", "p", "target", "body", "send_text");
            flock
                .outbox
                .lock()
                .unwrap()
                .insert("p".into(), vec![message.clone()]);
            let store = SaveProbe {
                io,
                calls: AtomicUsize::new(0),
            };
            let result = flock.cancel_message(&json!("m"), &store);
            if io {
                assert_eq!(result.unwrap()["status"], "cancelled");
            } else {
                assert_eq!(result.unwrap_err().to_string(), "sentinel save failure");
            }
            assert!(message.done.is_set());
            assert_eq!(store.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                flock
                    .cancel_message(&json!("m"), &store)
                    .unwrap_err()
                    .to_string(),
                "message m already cancelled"
            );
            assert_eq!(store.calls.load(Ordering::SeqCst), 1);
        }
    }
}
