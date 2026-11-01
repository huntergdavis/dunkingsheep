//! `flock/panes.rs` (T30, slice 4): `_pane_info`, `_role_pane_info` and the stores a `Flock`
//! owns. Ports `Flock._pane_info`, `_role_pane_info` (2 s cache, 180 s fail-open grace, t535
//! and the 2026-10-30 demotion incident) and the tasks/inbox attachment of `Flock.__init__`.
//!
//! NOT here: the real backend adapter (terminal::Pane -> the JSON pane shape needs
//! `_agent_kind` and `terminal_id`, which belong to the terminal layer), and the stores that
//! have no Rust owner yet (Superusers, MemoryStore, NextJobs, Allocator, metrics.db).

use std::collections::BTreeMap;
use std::sync::Arc;

use serde_json::Value;

use super::Flock;
use crate::inbox_store;
use crate::tasks::Tasks;

/// What `_pane_info` needs from the backend.
pub trait PaneInfoEnv {
    /// `herdr.list_panes_grouped()`: annotated with tab/workspace labels. May be empty.
    fn list_panes_grouped(&self) -> Vec<Value>;
    /// `herdr.get_pane(id)`.
    fn get_pane(&self, pane_id: &str) -> Option<Value>;
}

/// `_ROLE_PANE_TTL_S`: one tmux listing per request under load made every read cost 3-9 s.
pub const ROLE_PANE_TTL_S: f64 = 2.0;
/// A lane seen this recently is not "gone" on one empty answer.
pub const ROLE_PANE_GRACE_S: f64 = 180.0;
/// Entries kept before expired ones are swept.
const ROLE_PANE_CACHE_MAX: usize = 512;

/// The two role-pane caches: `_role_pane_cache` (expiry, info) and `_role_pane_last`.
#[derive(Debug, Default)]
pub struct RolePaneCache {
    cache: BTreeMap<String, (f64, Option<Value>)>,
    last: BTreeMap<String, (f64, Value)>,
}

/// The stores a `Flock` owns, attached after construction (their opening is async or
/// fallible, so `Flock::new` stays infallible).
#[derive(Default, Clone)]
pub struct Stores {
    pub tasks: Option<Arc<Tasks>>,
    pub inbox: Option<Arc<inbox_store::Store>>,
    /// `Flock.metrics` (`dunk_metrics.Metrics`): None until `wire_observability`.
    pub metrics: Option<Arc<crate::metrics::MetricsStore>>,
    /// `Flock.memory`'s read side: None when not persisting or the memory db is absent.
    pub memory: Option<Arc<crate::db::ReadPool>>,
}

impl std::fmt::Debug for Stores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stores")
            .field("tasks", &self.tasks.is_some())
            .field("inbox", &self.inbox.is_some())
            .field("metrics", &self.metrics.is_some())
            .field("memory", &self.memory.is_some())
            .finish()
    }
}

impl Flock {
    /// `_pane_info`: the pane with that id, preferring the grouped listing (it carries the
    /// tab/workspace labels the name-based checks need), else `get_pane`. Never raises.
    pub fn pane_info(env: &dyn PaneInfoEnv, pane_id: &str) -> Option<Value> {
        if pane_id.is_empty() {
            return None;
        }
        if let Some(found) = env
            .list_panes_grouped()
            .into_iter()
            .find(|p| p.get("pane_id").and_then(Value::as_str) == Some(pane_id))
        {
            return Some(found);
        }
        env.get_pane(pane_id)
    }

    /// `_role_pane_info`: `pane_info` cached for 2 s; a lane seen within the last 180 s is
    /// kept on one empty answer (fail open), a pane that really went away stops being served
    /// after the grace window. `now` is injected so the clocks are testable.
    pub fn role_pane_info(&self, env: &dyn PaneInfoEnv, pane_id: &str, now: f64) -> Option<Value> {
        if pane_id.is_empty() {
            return None;
        }
        let mut caches = self.role_panes.lock().expect("role pane lock");
        if let Some((expires, info)) = caches.cache.get(pane_id) {
            if *expires > now {
                return info.clone();
            }
        }
        let mut info = Flock::pane_info(env, pane_id);
        match &info {
            Some(found) => {
                caches
                    .last
                    .insert(pane_id.to_string(), (now, found.clone()));
            }
            None => {
                if let Some((seen, held)) = caches.last.get(pane_id) {
                    if now - seen < ROLE_PANE_GRACE_S {
                        eprintln!(
                            "pane lookup for {pane_id} returned nothing; using the record from \
                             {:.0} s ago (fail open for 180 s)",
                            now - seen
                        );
                        info = Some(held.clone());
                    }
                }
            }
        }
        caches
            .cache
            .insert(pane_id.to_string(), (now + ROLE_PANE_TTL_S, info.clone()));
        if caches.cache.len() > ROLE_PANE_CACHE_MAX {
            caches.cache.retain(|_, (expires, _)| *expires > now);
        }
        info
    }

    /// Attach the opened stores (`Flock.__init__`'s `self.tasks` / `self.inbox`).
    pub fn attach_stores(&self, stores: Stores) {
        *self.stores.lock().expect("stores lock") = stores;
    }

    /// `Flock.__init__`'s memory and metrics wiring. Persisting: metrics live beside the state
    /// file (`<state_dir>/metrics.db`, so a flock persisting into a temp dir keeps its events
    /// there) and the memory db is opened read-only when it exists. Not persisting: metrics are
    /// in memory and there is no memory store. Metrics are pruned on open. Missing or failing
    /// stores leave the field `None`; the flock still works (`Err` only for the metrics db).
    pub fn wire_observability(
        &self,
        state_dir: &std::path::Path,
        memory_db: Option<&std::path::Path>,
    ) -> Result<(), crate::error::DunkError> {
        let metrics = if self.persist_enabled {
            crate::metrics::MetricsStore::open(state_dir.join("metrics.db"))?
        } else {
            crate::metrics::MetricsStore::open(":memory:")?
        };
        metrics.prune(None, crate::metrics::MAX_EVENTS)?;
        let memory = match memory_db {
            Some(path) if self.persist_enabled && path.exists() => {
                crate::db::ReadPool::open(path, 2).ok().map(Arc::new)
            }
            _ => None,
        };
        let mut guard = self.stores.lock().expect("stores lock");
        guard.metrics = Some(Arc::new(metrics));
        guard.memory = memory;
        Ok(())
    }

    /// The attached stores.
    pub fn stores(&self) -> Stores {
        self.stores.lock().expect("stores lock").clone()
    }
}
