//! Concrete inbox and archive-path sources for retiring a pane.

use std::path::PathBuf;

use super::RequestRuntimeAdapter;

impl RequestRuntimeAdapter<'_> {
    pub(crate) fn helper_retireenv_mailboxes_of_pane(&self, pane_id: &str) -> Vec<String> {
        self.terminal
            .flock
            .stores()
            .inbox
            .and_then(|store| store.mailboxes_for_pane(pane_id).ok())
            .unwrap_or_default()
    }

    pub(crate) fn helper_retireenv_unread(
        &self,
        pane_id: &str,
    ) -> Result<Option<i64>, crate::error::DunkError> {
        match self.terminal.flock.stores().inbox {
            Some(store) => store.unread_for_pane(pane_id).map(Some),
            None => Ok(None),
        }
    }

    pub(crate) fn helper_retireenv_retired_path(&self) -> Result<PathBuf, crate::error::DunkError> {
        crate::api::CommandEnv::retired_panes_path(self)
    }
}

impl crate::flock::retire::RetireEnv for RequestRuntimeAdapter<'_> {
    fn panes(&self) -> Vec<serde_json::Value> {
        self.helper_retireenv_panes()
    }
    fn server_error_hint(&self) -> String {
        self.helper_retireenv_server_error_hint()
    }
    fn require_superuser(
        &self,
        pane: Option<&str>,
        action: &str,
    ) -> Result<(), crate::error::DunkError> {
        self.helper_retireenv_require_superuser(pane, action)
    }
    fn mailboxes_of_pane(&self, pane: &str) -> Vec<String> {
        self.helper_retireenv_mailboxes_of_pane(pane)
    }
    fn unread(&self, pane: &str) -> Result<Option<i64>, crate::error::DunkError> {
        self.helper_retireenv_unread(pane)
    }
    fn retired_path(&self) -> Result<PathBuf, crate::error::DunkError> {
        self.helper_retireenv_retired_path()
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
        assert_eq!(adapter.helper_retireenv_unread("%4").unwrap(), Some(1));
        assert_eq!(
            adapter.helper_retireenv_retired_path().unwrap(),
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
        assert_eq!(adapter.helper_retireenv_unread("%404").unwrap(), None);
    }

    #[test]
    fn retire_inbox_unread_errors_propagate() {
        let dir = tempfile::tempdir().unwrap();
        let inbox_path = dir.path().join("inbox.db");
        let inbox = Arc::new(Store::open(&inbox_path, dir.path().join("blobs")).unwrap());
        rusqlite::Connection::open(&inbox_path)
            .unwrap()
            .execute("DROP TABLE messages", [])
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

        assert!(adapter.helper_retireenv_unread("%404").is_err());
    }
}

#[cfg(test)]
mod trait_tests {
    use super::*;
    use crate::{
        config::Env,
        flock::{
            panes::Stores,
            retire::{h_retire_pane, RetireEnv},
            Flock,
        },
        inbox_store::{Outgoing, Store},
        roles::Roles,
        server::RequestCtx,
        terminal::{fakes::FakeTerminal, Terminal},
    };
    use std::sync::{Arc, Mutex};

    #[test]
    fn production_env_retire_trait_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let flock = Flock::new(dir.path().join("launches"), dir.path().join("lanes"), false);
        let inbox =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        inbox
            .send(&Outgoing {
                mailbox: "w1:p1",
                body: Some("test"),
                pane_id: Some("w1:p1"),
                ..Default::default()
            })
            .unwrap();
        flock.attach_stores(Stores {
            inbox: Some(inbox),
            ..Stores::default()
        });
        let mut roles = Roles::new(
            &Env::default(),
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register("w1:p1", "worker", None, None, None, None, None, None)
            .unwrap();
        flock.attach_roles(roles, None);
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let request = RequestCtx::default();
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);
        let env: &dyn RetireEnv = &adapter;
        assert!(env.panes().iter().any(|pane| pane["pane_id"] == "w1:p1"));
        assert!(!env.server_error_hint().is_empty());
        assert!(env.require_superuser(Some("w1:p1"), "retire_pane").is_err());
        assert_eq!(env.mailboxes_of_pane("w1:p1"), ["w1:p1"]);
        assert_eq!(env.unread("w1:p1").unwrap(), Some(1));
        assert_eq!(
            env.retired_path().unwrap(),
            crate::panes::default_path(&Env::from_process()).unwrap()
        );
        let err = h_retire_pane(env, &serde_json::json!({"action":"bad"}), None, None).unwrap_err();
        assert!(err.to_string().contains("retire action"));
        assert!(crate::panes::default_path(&Env::default()).is_err());
    }
}
