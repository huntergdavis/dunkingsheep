//! `Flock._role_pane_info`: the pane record used for role binding, cached 2 s, with the
//! fail-open grace map (PR #208, t1112).
//!
//! The terminal backend answers `None` both for a pane that is gone AND for a listing that
//! timed out or came back empty under load (measured 2026-10-30: every registered lane was
//! demoted to anonymous for 40 minutes, the sweep fed nobody and the fleet idled). A lane
//! seen within the last 3 minutes is not "gone" on one empty answer: its last good record
//! is kept. A pane that really went away stops being served after the grace window.
//!
//! `Flock` (T30) is not in Rust yet, so this is the cache as a value the Flock port will
//! own: time and the backend lookup are passed in, and the warning goes to a sink.

use std::collections::HashMap;

/// How long one empty answer is tolerated (`180.0` in the Python).
pub const FAIL_OPEN_S: f64 = 180.0;
/// The per-pane cache TTL (`now + 2.0`), unchanged by #208.
pub const CACHE_TTL_S: f64 = 2.0;
/// The Python prunes the TTL cache once it holds more than this many panes.
pub const CACHE_PRUNE_AT: usize = 512;

/// The Python's warning text, byte for byte (the watchdog and tests grep it).
pub fn fail_open_message(pane_id: &str, age_s: f64) -> String {
    format!(
        "pane lookup for {pane_id} returned nothing; using the record from {age_s:.0} s ago \
         (fail open for 180 s)"
    )
}

#[derive(Debug, Clone, Default)]
pub struct RolePaneCache<T: Clone> {
    /// `_role_pane_cache`: pane id -> (expires_at, record or None).
    cache: HashMap<String, (f64, Option<T>)>,
    /// `_role_pane_last`: pane id -> (time, last good record).
    pub role_pane_last: HashMap<String, (f64, T)>,
}

impl<T: Clone> RolePaneCache<T> {
    pub fn new() -> Self {
        RolePaneCache {
            cache: HashMap::new(),
            role_pane_last: HashMap::new(),
        }
    }

    /// `_role_pane_info(pane_id)`. `fetch` is `_pane_info` (None = the backend knows no
    /// such pane, or could not say); `warn` receives the fail-open message.
    pub fn role_pane_info(
        &mut self,
        pane_id: &str,
        now: f64,
        fetch: impl FnOnce() -> Option<T>,
        mut warn: impl FnMut(&str),
    ) -> Option<T> {
        if pane_id.is_empty() {
            return None;
        }
        if let Some((expires, record)) = self.cache.get(pane_id) {
            if *expires > now {
                return record.clone();
            }
        }
        let mut info = fetch();
        match &info {
            Some(record) => {
                self.role_pane_last
                    .insert(pane_id.to_string(), (now, record.clone()));
            }
            None => {
                if let Some((at, record)) = self.role_pane_last.get(pane_id) {
                    if now - at < FAIL_OPEN_S {
                        warn(&fail_open_message(pane_id, now - at));
                        info = Some(record.clone());
                    }
                }
            }
        }
        self.cache
            .insert(pane_id.to_string(), (now + CACHE_TTL_S, info.clone()));
        if self.cache.len() > CACHE_PRUNE_AT {
            self.cache.retain(|_, (expires, _)| *expires > now);
        }
        info
    }

    /// Drop the 2 s cache (the Python tests call `_role_pane_cache.clear()`); the grace
    /// map is kept, as it is there.
    pub fn clear_cache(&mut self) {
        self.cache.clear();
    }
}

#[cfg(test)]
#[path = "pane_record_tests.rs"]
mod pane_record_tests;
