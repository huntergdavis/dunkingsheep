//! The roster cache (t4706 slice 2). Building the roster costs seconds (median 5 s on the live
//! daemon, 9,748 calls a day, mostly desk polls), so the last raw roster is reused while:
//!
//! * it is at most `MAX_AGE_S` old, and
//! * nothing it is built from changed: the pane table (ids, root pids, foreground programs,
//!   labels), the lane registry file, the role registry file, or the launch records.
//!
//! Only the RAW roster is cached. Every caller's own filter (such as a project) is applied after,
//! per call, so a cached answer never crosses one caller's scope into another's. The answer says
//! when it was built (`cached_at`) and whether this call reused it (`cache`).

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::sync::Mutex;

use serde_json::{json, Value};

/// A cached roster is reused for at most this long.
pub const MAX_AGE_S: f64 = 5.0;

/// The fingerprint of everything the roster is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Key(u64);

/// `(modified nanoseconds, length)` of a file, or zeros when it cannot be read (absent).
fn file_stamp(path: &Path) -> (i128, u64) {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).map_or((0, 0), |meta| {
        (
            i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
            meta.size(),
        )
    })
}

/// Build the key from its four inputs: each pane as `(id, root pid, command, label)`, the lane
/// and role registry paths, and the launch records' serialized text.
pub fn key(
    panes: &[(String, Option<i64>, String, String)],
    lane_registry: &Path,
    roles: Option<&Path>,
    launches: &str,
) -> Key {
    let mut hasher = DefaultHasher::new();
    panes.hash(&mut hasher);
    file_stamp(lane_registry).hash(&mut hasher);
    roles.map(file_stamp).hash(&mut hasher);
    launches.hash(&mut hasher);
    Key(hasher.finish())
}

#[derive(Debug, Default)]
pub struct RosterCache {
    entry: Mutex<Option<(Key, f64, Value)>>,
}

impl RosterCache {
    /// The cached raw roster for `key` if it is younger than `MAX_AGE_S`, else `build()`'s
    /// answer, which is cached. Each answer carries `cached_at` and `cache` (hit or miss).
    pub fn get_or_build(
        &self,
        key: Key,
        now: f64,
        build: impl FnOnce() -> Result<Value, String>,
    ) -> Result<Value, String> {
        if let Some((cached_key, at, value)) = self
            .entry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            if *cached_key == key && now >= *at && now - *at <= MAX_AGE_S {
                let mut value = value.clone();
                value["cached_at"] = json!(at);
                value["cache"] = json!("hit");
                return Ok(value);
            }
        }
        let built = build()?;
        *self.entry.lock().unwrap_or_else(|e| e.into_inner()) = Some((key, now, built.clone()));
        let mut value = built;
        value["cached_at"] = json!(now);
        value["cache"] = json!("miss");
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panes(pid: i64, command: &str, label: &str) -> Vec<(String, Option<i64>, String, String)> {
        vec![("%1".into(), Some(pid), command.into(), label.into())]
    }

    /// Builds counted: a hit must not build.
    fn get(cache: &RosterCache, key: Key, now: f64, builds: &std::cell::Cell<u32>) -> Value {
        cache
            .get_or_build(key, now, || {
                builds.set(builds.get() + 1);
                Ok(json!({"lanes": [{"pane_id": "%1"}]}))
            })
            .unwrap()
    }

    #[test]
    fn a_hit_within_the_age_reuses_the_roster_and_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let lanes = dir.path().join("lanes.json");
        let k = key(&panes(10, "codex", "a"), &lanes, None, "{}");
        let cache = RosterCache::default();
        let builds = std::cell::Cell::new(0);
        let first = get(&cache, k, 100.0, &builds);
        assert_eq!(
            (first["cache"].as_str(), first["cached_at"].as_f64()),
            (Some("miss"), Some(100.0))
        );
        let second = get(&cache, k, 104.9, &builds);
        assert_eq!(
            (second["cache"].as_str(), second["cached_at"].as_f64()),
            (Some("hit"), Some(100.0))
        );
        assert_eq!(builds.get(), 1);
        // Past the maximum age: rebuilt.
        let old = get(&cache, k, 105.1, &builds);
        assert_eq!(old["cache"], "miss");
        assert_eq!(builds.get(), 2);
        // A clock that went backwards is not a hit.
        assert_eq!(get(&cache, k, 50.0, &builds)["cache"], "miss");
    }

    #[test]
    fn every_input_change_invalidates() {
        let dir = tempfile::tempdir().unwrap();
        let lanes = dir.path().join("lanes.json");
        let roles = dir.path().join("roles.json");
        std::fs::write(&lanes, "{}").unwrap();
        std::fs::write(&roles, "{}").unwrap();
        let base = key(&panes(10, "codex", "a"), &lanes, Some(&roles), "{}");
        assert_eq!(
            base,
            key(&panes(10, "codex", "a"), &lanes, Some(&roles), "{}"),
            "stable"
        );
        // Pane table: a new root pid (relaunch), another foreground program, another label, a
        // pane added.
        assert_ne!(
            base,
            key(&panes(11, "codex", "a"), &lanes, Some(&roles), "{}")
        );
        assert_ne!(base, key(&panes(10, "sh", "a"), &lanes, Some(&roles), "{}"));
        assert_ne!(
            base,
            key(&panes(10, "codex", "b"), &lanes, Some(&roles), "{}")
        );
        let mut two = panes(10, "codex", "a");
        two.push(("%2".into(), Some(20), "claude".into(), "c".into()));
        assert_ne!(base, key(&two, &lanes, Some(&roles), "{}"));
        // Launch records.
        assert_ne!(
            base,
            key(
                &panes(10, "codex", "a"),
                &lanes,
                Some(&roles),
                "{\"%1\":{}}"
            )
        );
        // Lane registry and role registry files (content length and modification time).
        std::fs::write(&lanes, "{\"%1\": \"writer\"}").unwrap();
        let after_lanes = key(&panes(10, "codex", "a"), &lanes, Some(&roles), "{}");
        assert_ne!(base, after_lanes);
        std::fs::write(&roles, "{\"lanes\": {}}").unwrap();
        assert_ne!(
            after_lanes,
            key(&panes(10, "codex", "a"), &lanes, Some(&roles), "{}")
        );
        // A cache entry under the old key is not served for the new one.
        let cache = RosterCache::default();
        let builds = std::cell::Cell::new(0);
        get(&cache, base, 100.0, &builds);
        assert_eq!(get(&cache, after_lanes, 101.0, &builds)["cache"], "miss");
        assert_eq!(builds.get(), 2);
    }
}
