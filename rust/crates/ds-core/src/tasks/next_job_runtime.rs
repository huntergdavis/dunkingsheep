//! Runtime state and per-pane serialization for next-job operations.
//!
//! The lock map mirrors `dunk_next_job.NextJobs._pane_lock`: a pane always maps to
//! the same async mutex, while different panes can progress independently. The
//! standard mutex protects only the map and is never held across an await.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio::sync::OwnedMutexGuard;

use crate::flock::Flock;

pub struct NextJobs {
    // Later runtime leaves use the shared Flock for dispatch, tasks, and inbox work.
    #[allow(dead_code)]
    flock: Arc<Flock>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl NextJobs {
    pub fn new(flock: Arc<Flock>) -> Self {
        Self {
            flock,
            locks: Mutex::new(HashMap::new()),
        }
    }

    fn pane_lock(&self, pane: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self
            .locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(
            locks
                .entry(pane.to_owned())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))),
        )
    }

    #[allow(dead_code)] // Used by the next-job methods added in following runtime leaves.
    async fn lock_pane(&self, pane: &str) -> OwnedMutexGuard<()> {
        self.pane_lock(pane).lock_owned().await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn next_jobs() -> NextJobs {
        NextJobs::new(Arc::new(Flock::new(PathBuf::new(), PathBuf::new(), false)))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_reuses_one_lock_per_pane() {
        let jobs = Arc::new(next_jobs());
        let first = jobs.pane_lock("%12");
        let repeated = jobs.pane_lock("%12");
        assert!(Arc::ptr_eq(&first, &repeated));

        let held = first
            .clone()
            .try_lock_owned()
            .expect("first lock available");
        assert!(repeated.try_lock_owned().is_err());
        drop(held);

        let owned_guard = jobs.lock_pane("%12").await;
        drop(owned_guard);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn next_jobs_locks_are_independent_across_panes() {
        let jobs = next_jobs();
        let first = jobs.pane_lock("%12");
        let second = jobs.pane_lock("%13");
        assert!(!Arc::ptr_eq(&first, &second));

        let first_guard = first.try_lock_owned().expect("first pane lock available");
        let second_guard = second.try_lock_owned().expect("second pane lock available");
        drop((first_guard, second_guard));
    }
}
