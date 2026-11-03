//! Actual queued-notice folding and direct send services.
use super::{inbox_data::InboxRuntimeView, TerminalCallAdapter};
use crate::error::DunkError;
use crate::flock::commands_inbox::Nudge;
use crate::flock::sends::{outbox_lock, DirectSendOptions, DirectTextMetadata, SnapshotStore};
use serde_json::Value;

impl InboxRuntimeView<'_> {
    pub(super) fn fold_notice(&self, pane: &str, text: &str, id: &str) -> Option<String> {
        let mut outbox = outbox_lock(&self.flock.outbox);
        let message = outbox
            .get_mut(pane)?
            .iter_mut()
            .find(|m| m.status == "queued" && m.is_notice)?;
        message.text = text.to_owned();
        if !message.append_notify_for(id) {
            return None;
        }
        Some(message.id.clone())
    }
    pub(super) fn send_notice(
        &self,
        pane: &str,
        text: &str,
        id: &str,
        snapshot: &dyn SnapshotStore,
    ) -> Result<Nudge, DunkError> {
        let mut env = TerminalCallAdapter {
            flock: &self.flock,
            terminal: self.adapter.terminal.terminal,
            runtime: self.adapter.runtime,
        };
        let result = self.flock.send_now(
            &Value::String(pane.into()),
            text,
            DirectSendOptions::default(),
            DirectTextMetadata {
                is_notice: true,
                notify_for: Some(id),
                ..DirectTextMetadata::default()
            },
            &mut env,
            self.pool.as_ref(),
            snapshot,
            || None,
        )?;
        let queued = result
            .get("queued")
            .and_then(Value::as_bool)
            .ok_or_else(|| DunkError::Refused("invalid outbox queued result".into()))?;
        let message = result
            .get("message")
            .and_then(Value::as_str)
            .ok_or_else(|| DunkError::Refused("invalid outbox message result".into()))?;
        Ok(Nudge {
            queued,
            message: Some(message.to_owned()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flock::{commands_inbox::InboxEnv, panes::Stores, sends::Message, Flock};
    use crate::inbox_store::Store;
    use crate::server::RequestCtx;
    use crate::server_runtime::{outbox_runtime::OutboxRuntime, RequestRuntimeAdapter};
    use crate::terminal::{fakes::FakeTerminal, Terminal};
    use std::sync::{Arc, Mutex};

    #[test]
    fn inbox_view_requires_store_and_folds_first_queued_notice() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        assert!(OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new()))
        )
        .is_err());
        let store =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        flock.attach_stores(Stores {
            inbox: Some(store),
            ..Stores::default()
        });
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
        )
        .unwrap();
        assert!(
            InboxRuntimeView::new(adapter, Arc::clone(&flock), Arc::clone(&owner.pool)).is_err()
        );
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        let view =
            InboxRuntimeView::new(adapter, Arc::clone(&flock), Arc::clone(&owner.pool)).unwrap();
        assert!(view.caller_role("%1").is_err());
        assert!(view.backend_name().is_ok());
        assert!(view.tasks_for_pane("%1").is_err());
        assert!(std::ptr::eq(view.pool(), view.store().read_pool()));
        let normal = Message::new("m1", "%1", "lane", "normal", "test");
        let mut notice = Message::new("m2", "%1", "lane", "old", "test");
        notice.is_notice = true;
        notice.notify_for.push("i1".into());
        outbox_lock(&flock.outbox).insert("%1".into(), vec![normal, notice]);
        assert_eq!(view.fold_notice("%1", "new", "i2").as_deref(), Some("m2"));
        let outbox = outbox_lock(&flock.outbox);
        assert_eq!(outbox["%1"][0].text, "normal");
        assert_eq!(outbox["%1"][1].text, "new");
        assert_eq!(outbox["%1"][1].notify_for, ["i1", "i2"]);
    }
    #[test]
    fn concurrent_fold_settles_both_inbox_receipts() {
        use crate::inbox_store::Outgoing;
        use std::time::{Duration, Instant};

        let dir = tempfile::tempdir().unwrap();
        let store =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        let outgoing = Outgoing {
            mailbox: "w1:p1",
            body: Some("body"),
            subject: None,
            sender: None,
            mailbox_label: None,
            pane_id: Some("w1:p1"),
            attachments: &[],
            transport: None,
            reply_to: None,
            external_id: None,
        };
        let first = store.send(&outgoing).unwrap();
        let second = store.send(&outgoing).unwrap();
        let flock = Arc::new(Flock::new(
            dir.path().join("launches"),
            dir.path().join("lanes"),
            false,
        ));
        flock.attach_stores(Stores {
            inbox: Some(Arc::clone(&store)),
            ..Stores::default()
        });
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
        let request = RequestCtx::default();
        let local_terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter =
            RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &local_terminal);
        let view =
            InboxRuntimeView::new(adapter, Arc::clone(&flock), Arc::clone(&owner.pool)).unwrap();
        let mut message = Message::new("m1", "w1:p1", "lane", "notice", "test");
        message.is_notice = true;
        message.press_enter = false;
        message.hold_while_typing = false;
        message.notify_for.push(first.clone());
        outbox_lock(&flock.outbox).insert("w1:p1".into(), vec![message]);
        let send_lock = flock.send_lock_for("w1:p1");
        let guard = send_lock.lock().unwrap();
        std::thread::scope(|scope| {
            let flock = Arc::clone(&flock);
            let handle = runtime.handle().clone();
            let worker = scope.spawn(move || {
                let mut fake = FakeTerminal::new();
                fake.panes[0].pane_pid = Some(i64::from(std::process::id()));
                let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(fake));
                let mut env = TerminalCallAdapter {
                    flock: &flock,
                    terminal: &terminal,
                    runtime: &handle,
                };
                flock.deliver_message("m1", crate::clock::now_s(), &mut env, || None)
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let captured = outbox_lock(&view.flock.outbox)
                    .get("w1:p1")
                    .and_then(|rows| rows.first())
                    .and_then(|row| row.attempt_generation.clone());
                if captured.is_some() {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "delivery did not capture its generation"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(
                view.fold_notice("w1:p1", "updated", &second).as_deref(),
                Some("m1")
            );
            drop(guard);
            worker.join().unwrap();
        });
        assert_eq!(
            flock.sent_text_by_pane.lock().unwrap()["w1:p1"]
                .last()
                .map(String::as_str),
            Some("updated"),
            "the in-flight delivery must type the latest folded notice text"
        );
        let first_status =
            crate::inbox::status(store.read_pool(), &first, crate::clock::now_s()).unwrap();
        let second_status =
            crate::inbox::status(store.read_pool(), &second, crate::clock::now_s()).unwrap();
        assert!(first_status.notify_state.is_some());
        assert_eq!(first_status.notify_state, second_status.notify_state);
    }
}
