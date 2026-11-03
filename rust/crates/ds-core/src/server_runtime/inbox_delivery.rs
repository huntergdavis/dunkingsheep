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
        message.notify_for.push(id.to_owned());
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
    use crate::flock::{panes::Stores, sends::Message, Flock};
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
        let owner = OutboxRuntime::new(
            Arc::clone(&flock),
            runtime.handle().clone(),
            Arc::new(|| Box::new(FakeTerminal::new())),
        );
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        assert!(
            InboxRuntimeView::new(adapter, Arc::clone(&flock), Arc::clone(&owner.pool)).is_err()
        );
        let store =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        flock.attach_stores(Stores {
            inbox: Some(store),
            ..Stores::default()
        });
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        let view =
            InboxRuntimeView::new(adapter, Arc::clone(&flock), Arc::clone(&owner.pool)).unwrap();
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
}
