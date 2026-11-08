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

/// The two role-pane caches: `_role_pane_cache` (expiry, info) and `_role_pane_last`, and the
/// caller-binding root pids (t3368), which have no grace window.
#[derive(Debug, Default)]
pub struct RolePaneCache {
    cache: BTreeMap<String, (f64, Option<Value>)>,
    last: BTreeMap<String, (f64, Value)>,
    roots: BTreeMap<String, (f64, Option<i64>)>,
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
    /// Attach the one startup-supplied project policy to this canonical Flock.
    pub fn attach_project_runtime_policy(
        &self,
        policy: std::sync::Arc<crate::server_runtime::ProjectRuntimePolicy>,
    ) -> Result<(), crate::error::DunkError> {
        let mut slot = self
            .project_runtime_policy
            .policy
            .lock()
            .expect("project runtime policy lock");
        if let Some(current) = slot.as_ref() {
            if !std::sync::Arc::ptr_eq(current, &policy) {
                return Err(crate::error::DunkError::Refused(
                    "a different project runtime policy is already attached".into(),
                ));
            }
        } else {
            *slot = Some(policy);
        }
        Ok(())
    }

    /// Clone the canonical policy Arc and release the Flock slot lock before use.
    pub fn project_runtime_policy(
        &self,
    ) -> Option<std::sync::Arc<crate::server_runtime::ProjectRuntimePolicy>> {
        self.project_runtime_policy
            .policy
            .lock()
            .expect("project runtime policy lock")
            .clone()
    }

    /// Record why startup attached no policy, or only part of one. The reason is startup
    /// evidence only; it never grants approval.
    pub fn attach_project_runtime_refusal(&self, reason: String) {
        *self
            .project_runtime_policy
            .refusal
            .lock()
            .expect("project runtime refusal lock") = Some(reason);
    }

    /// The startup refusal recorded by `attach_project_runtime_refusal`, if any.
    pub fn project_runtime_policy_refusal(&self) -> Option<String> {
        self.project_runtime_policy
            .refusal
            .lock()
            .expect("project runtime refusal lock")
            .clone()
    }

    /// Attach the canonical memory service and retain its own pool as the legacy read alias.
    pub fn attach_memory_service(
        &self,
        service: Arc<crate::memory::service::MemoryService>,
    ) -> Result<(), crate::error::DunkError> {
        let mut slot = self.memory_service.0.lock().expect("memory service lock");
        service.while_accepting(|| {
            if let Some(current) = slot.as_ref() {
                if !Arc::ptr_eq(current, &service) {
                    return Err(crate::error::DunkError::Refused(
                        "a different memory service is already attached".into(),
                    ));
                }
            } else {
                *slot = Some(Arc::clone(&service));
            }
            let mut stores = self.stores.lock().expect("stores lock");
            stores.memory = Some(Arc::clone(service.read_pool()));
            Ok(())
        })?
    }

    /// Clone the canonical service Arc, releasing the slot lock before callers do work.
    pub fn memory_service(&self) -> Option<Arc<crate::memory::service::MemoryService>> {
        self.memory_service
            .0
            .lock()
            .expect("memory service lock")
            .clone()
    }

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

    /// The root pid of a pane for caller binding (t3368), read with `read`. Fail closed: a listing
    /// without the pane (`Ok(None)`) and a read that fails (`Err`) are returned as they are, and
    /// the caller refuses the claim. There is no grace window here: an earlier sighting never
    /// stands in for a missed listing. A found pane is cached for the same 2 s TTL as the role
    /// panes; a miss or an error is not cached, so the next request asks again.
    pub(crate) fn pane_root_pid(
        &self,
        pane_id: &str,
        now: f64,
        read: impl FnOnce() -> Result<Option<Option<i64>>, crate::error::DunkError>,
    ) -> Result<Option<Option<i64>>, crate::error::DunkError> {
        let cached = self
            .role_panes
            .lock()
            .expect("role pane lock")
            .roots
            .get(pane_id)
            .copied();
        if let Some((expires, pid)) = cached {
            if expires > now {
                return Ok(Some(pid));
            }
        }
        let answer = read()?;
        if let Some(pid) = answer {
            let mut caches = self.role_panes.lock().expect("role pane lock");
            caches
                .roots
                .insert(pane_id.to_string(), (now + ROLE_PANE_TTL_S, pid));
            if caches.roots.len() > ROLE_PANE_CACHE_MAX {
                caches.roots.retain(|_, (expires, _)| *expires > now);
            }
        }
        Ok(answer)
    }

    /// `self._role_pane_cache.pop(pane_id)`: a registration change must not be served stale.
    pub(crate) fn forget_role_pane(&self, pane_id: &str) {
        self.role_panes
            .lock()
            .expect("role pane lock")
            .cache
            .remove(pane_id);
    }

    /// Attach the opened stores (`Flock.__init__`'s `self.tasks` / `self.inbox`).
    pub fn attach_stores(&self, stores: Stores) {
        let mut stores = stores;
        let slot = self.memory_service.0.lock().expect("memory service lock");
        if let Some(service) = slot.as_ref() {
            stores.memory = Some(Arc::clone(service.read_pool()));
        }
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
        let slot = self.memory_service.0.lock().expect("memory service lock");
        let memory = if let Some(service) = slot.as_ref() {
            Some(Arc::clone(service.read_pool()))
        } else {
            match memory_db {
                Some(path) if self.persist_enabled && path.exists() => {
                    crate::db::ReadPool::open(path, 2).ok().map(Arc::new)
                }
                _ => None,
            }
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
