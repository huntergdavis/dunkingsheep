//! Concrete inbox and archive-path sources for retiring a pane.

use std::path::PathBuf;

use super::RequestRuntimeAdapter;

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete RetireEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_retireenv_mailboxes_of_pane(&self, pane_id: &str) -> Vec<String> {
        self.terminal
            .flock
            .stores()
            .inbox
            .and_then(|store| store.mailboxes_for_pane(pane_id).ok())
            .unwrap_or_default()
    }

    #[allow(dead_code)] // The complete RetireEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_retireenv_unread(&self, pane_id: &str) -> Option<i64> {
        self.terminal
            .flock
            .stores()
            .inbox
            .and_then(|store| store.unread_for_pane(pane_id).ok())
    }

    #[allow(dead_code)] // The complete RetireEnv implementation belongs to its assembly leaf.
    pub(crate) fn helper_retireenv_retired_path(&self) -> PathBuf {
        crate::api::CommandEnv::retired_panes_path(self)
            .expect("retired pane archive path must be configured")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::{
        config::Env,
        flock::{panes::Stores, Flock},
        inbox_store::{Outgoing, Store},
        server::RequestCtx,
        terminal::{fakes::FakeTerminal, Terminal},
    };

    use super::*;

    #[test]
    fn production_env_retire_inbox_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let inbox =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        inbox
            .send(&Outgoing {
                mailbox: "%4",
                body: Some("pane mailbox"),
                pane_id: Some("%4"),
                ..Default::default()
            })
            .unwrap();
        inbox
            .send(&Outgoing {
                mailbox: "writer-mailbox",
                body: Some("named mailbox"),
                pane_id: Some("%4"),
                ..Default::default()
            })
            .unwrap();

        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        flock.attach_stores(Stores {
            inbox: Some(inbox),
            ..Stores::default()
        });
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        assert_eq!(
            adapter.helper_retireenv_mailboxes_of_pane("%4"),
            vec!["%4", "writer-mailbox"]
        );
        assert_eq!(adapter.helper_retireenv_unread("%4"), Some(1));
        assert_eq!(
            adapter.helper_retireenv_retired_path(),
            crate::panes::default_path(&Env::from_process()).unwrap()
        );
    }

    #[test]
    fn retire_inbox_helpers_keep_the_absent_store_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        assert!(adapter
            .helper_retireenv_mailboxes_of_pane("%404")
            .is_empty());
        assert_eq!(adapter.helper_retireenv_unread("%404"), None);
    }
}
