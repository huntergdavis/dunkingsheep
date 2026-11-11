//! Process-local ownership of control clients, fenced by connection generation.
use std::{
    collections::BTreeMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ControlIdentity {
    pub generation: u64,
    pub pid: i64,
    pub start: String,
}

#[derive(Default)]
pub struct ControlRegistry {
    next: AtomicU64,
    entries: Mutex<BTreeMap<String, Vec<ControlIdentity>>>,
}

pub struct ControlLease {
    registry: Arc<ControlRegistry>,
    pane: String,
    generation: u64,
}

/// Keeps a resize fenced until its blocking worker has actually returned,
/// including after the HTTP caller has timed out.
#[derive(Default)]
pub struct ResizeSlots {
    next: AtomicU64,
    entries: Mutex<BTreeMap<u64, (String, String)>>,
}

pub struct ResizeLease {
    registry: Arc<ResizeSlots>,
    generation: u64,
}

impl ResizeSlots {
    pub fn acquire(self: &Arc<Self>, name: &str, pane: &str) -> Option<ResizeLease> {
        let mut entries = self.entries.lock().ok()?;
        if entries
            .values()
            .any(|(old_name, old_pane)| old_name == name || old_pane == pane)
        {
            return None;
        }
        let generation = self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
            .ok()?;
        entries.insert(generation, (name.to_owned(), pane.to_owned()));
        Some(ResizeLease {
            registry: self.clone(),
            generation,
        })
    }

    pub fn busy_name(&self, name: &str) -> Option<bool> {
        Some(
            self.entries
                .lock()
                .ok()?
                .values()
                .any(|(active, _)| active == name),
        )
    }

    pub fn busy_target(&self, name: &str, pane: &str) -> Option<bool> {
        Some(
            self.entries
                .lock()
                .ok()?
                .values()
                .any(|(active_name, active_pane)| active_name == name || active_pane == pane),
        )
    }
}

impl Drop for ResizeLease {
    fn drop(&mut self) {
        // A panic may have occurred after dispatching tmux. Its child could
        // still act, so retain the fence until the router is replaced.
        if std::thread::panicking() {
            return;
        }
        if let Ok(mut entries) = self.registry.entries.lock() {
            entries.remove(&self.generation);
        }
    }
}

impl ControlRegistry {
    pub fn register(self: &Arc<Self>, pane: &str, pid: i64, start: String) -> Option<ControlLease> {
        if pid <= 0 || start.is_empty() {
            return None;
        }
        let generation = self
            .next
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_add(1))
            .ok()?;
        let identity = ControlIdentity {
            generation,
            pid,
            start,
        };
        self.entries
            .lock()
            .ok()?
            .entry(pane.to_owned())
            .or_default()
            .push(identity);
        Some(ControlLease {
            registry: self.clone(),
            pane: pane.to_owned(),
            generation,
        })
    }
    pub fn unique(&self, pane: &str) -> Option<ControlIdentity> {
        let entries = self.entries.lock().ok()?;
        let [client] = entries.get(pane)?.as_slice() else {
            return None;
        };
        Some(client.clone())
    }
}

impl Drop for ControlLease {
    fn drop(&mut self) {
        if let Ok(mut entries) = self.registry.entries.lock() {
            if let Some(clients) = entries.get_mut(&self.pane) {
                clients.retain(|entry| entry.generation != self.generation);
                if clients.is_empty() {
                    entries.remove(&self.pane);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ControlRegistry, ResizeSlots};
    use std::sync::Arc;
    #[test]
    fn unique_generation_survives_other_connection_cleanup() {
        let registry = Arc::new(ControlRegistry::default());
        let first = registry.register("%1", 11, "a".into()).unwrap();
        let second = registry.register("%1", 12, "b".into()).unwrap();
        assert!(registry.unique("%1").is_none());
        drop(first);
        assert_eq!(registry.unique("%1").unwrap().pid, 12);
        drop(second);
        assert!(registry.unique("%1").is_none());
    }

    #[test]
    fn resize_lease_blocks_same_name_and_pane_until_worker_returns() {
        let slots = Arc::new(ResizeSlots::default());
        let lease = slots.acquire("worker", "%1").unwrap();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            let _lease = lease;
            release_rx.recv().unwrap();
        });
        assert_eq!(slots.busy_name("worker"), Some(true));
        assert!(slots.acquire("worker", "%2").is_none());
        assert!(slots.acquire("renamed", "%1").is_none());
        assert_eq!(slots.busy_target("renamed", "%1"), Some(true));
        release_tx.send(()).unwrap();
        worker.join().unwrap();
        assert_eq!(slots.busy_name("worker"), Some(false));
        assert!(slots.acquire("worker", "%1").is_some());
    }

    #[test]
    fn resize_lease_remains_fenced_on_worker_panic() {
        let slots = Arc::new(ResizeSlots::default());
        let lease = slots.acquire("worker", "%1").unwrap();
        assert!(std::thread::spawn(move || {
            let _lease = lease;
            panic!("private fixture panic")
        })
        .join()
        .is_err());
        assert_eq!(slots.busy_name("worker"), Some(true));
        assert!(slots.acquire("renamed", "%1").is_none());
    }
}
