//! Attached inbox store and request clock sources for the runtime adapter.

use super::RequestRuntimeAdapter;
use crate::db::ReadPool;
use crate::inbox_store::Store;

impl RequestRuntimeAdapter<'_> {
    #[allow(dead_code)] // The complete InboxEnv assembly is owned by its dependent leaf.
    pub(crate) fn helper_inboxenv_pool(&self) -> &ReadPool {
        self.inbox
            .as_deref()
            .expect("InboxEnv requires an attached inbox store")
            .read_pool()
    }

    #[allow(dead_code)] // The complete InboxEnv assembly is owned by its dependent leaf.
    pub(crate) fn helper_inboxenv_store(&self) -> &Store {
        self.inbox
            .as_deref()
            .expect("InboxEnv requires an attached inbox store")
    }

    #[allow(dead_code)] // The complete InboxEnv assembly is owned by its dependent leaf.
    pub(crate) fn helper_inboxenv_now(&self) -> f64 {
        crate::clock::now_s()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::flock::{panes::Stores, Flock};
    use crate::server::RequestCtx;
    use crate::terminal::{fakes::FakeTerminal, Terminal};

    #[test]
    fn production_env_inbox_stores_uses_concrete_services() {
        let dir = tempfile::tempdir().unwrap();
        let inbox =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        let flock = Flock::new(
            dir.path().join("launches.json"),
            dir.path().join("lanes.json"),
            false,
        );
        flock.attach_stores(Stores {
            inbox: Some(Arc::clone(&inbox)),
            ..Stores::default()
        });
        let request = RequestCtx::default();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::new()));
        let adapter = RequestRuntimeAdapter::new(&flock, &request, runtime.handle(), &terminal);

        assert!(std::ptr::eq(
            adapter.helper_inboxenv_store(),
            inbox.as_ref()
        ));
        assert!(std::ptr::eq(
            adapter.helper_inboxenv_pool(),
            inbox.read_pool()
        ));
        let before = crate::clock::now_s();
        let request_time = adapter.helper_inboxenv_now();
        let after = crate::clock::now_s();
        assert!((before..=after).contains(&request_time));
    }
}
