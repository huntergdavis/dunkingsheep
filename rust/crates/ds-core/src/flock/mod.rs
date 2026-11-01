//! `flock/mod.rs` (T30, slice 1): the `Flock` struct, launch memory, the lane registry and
//! the per-request caller state. Ports `dunk_core.Flock.__init__` (the pieces whose
//! dependencies are already in the tree), `_load_launches`, `_save_launches`,
//! `_record_launch`, `_snapshot_launches`, `_lane_registry`, `_write_lane_registry`,
//! `_remember_lane`, `_row_identity`, `_plain_lane_name`, `_usable_declared_name`,
//! `set_caller`/`current_caller`/`clear_caller`, `set_socket_peer`/`clear_socket_peer`.
//!
//! NOT in this slice (tracked on t798): resolve_* (target/mailbox/tab/workspace/_pick),
//! `_pane_info`, and the construction of the stores that have no Rust owner yet
//! (Superusers, MemoryStore, NextJobs, Allocator, outbox/stuck state).

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::error::DunkError;
use crate::{atomic_json, clock, lock};

/// `_PANE_ID_RE`.
fn pane_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^(?:[A-Za-z0-9_.-]+:p\d+|%\d+)$").expect("static regex"))
}

/// `_HERDR_ID_RE`.
fn herdr_id_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^w\d+:p[A-Za-z0-9_.-]+$").expect("static regex"))
}

/// `_PLAIN_LANE_NAME`.
fn plain_lane_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._:-]*$").expect("static regex"))
}

/// `_NOT_A_NAME_MARKERS`.
const NOT_A_NAME_MARKERS: [&str; 5] = ["[inbox]", "read it with", "|", "\u{2502}", "\u{203a}"];

static NEXT_FLOCK_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    /// The Python's `threading.local()` (`dunk_core.py:727`): the caller state of the request
    /// running on THIS thread, per flock. It used to be one `Mutex<CallerState>` on the shared
    /// `Flock`, so a request with an owner token could be read by any concurrent request
    /// (privilege confusion, t1347/t1354).
    ///
    /// CONTRACT for the server (T40): a request runs its Flock calls on ONE thread, with no
    /// `.await` between `set_caller` and `clear_caller` (a sync handler on a blocking thread, or
    /// a plain synchronous block inside an async fn), and ends with `clear_caller`.
    static CALLERS: RefCell<BTreeMap<u64, CallerState>> = const { RefCell::new(BTreeMap::new()) };
}

/// Who is making the current request (`set_caller`): pane id plus the stable agent pid/start.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CallerState {
    pub value: Option<Value>,
    pub role: Option<String>,
    pub role_pane: Option<String>,
    pub service_token: Option<String>,
    pub identity_bound: Option<bool>,
    pub identity_note: Option<String>,
    /// `None` = never set (in-process trusted code); `Some(false)` = socket peer failed
    /// verification (fail closed).
    pub socket_peer_ok: Option<bool>,
}

/// The flock. Slice 1 holds launch memory, the lane registry path and caller state.
#[derive(Debug)]
pub struct Flock {
    pub launches_file: PathBuf,
    pub lane_registry_path: PathBuf,
    pub persist_enabled: bool,
    /// pane_id -> {agent, session, session_kind, cwd, argv, command, source, recorded_at}
    launches: Mutex<BTreeMap<String, Map<String, Value>>>,
    /// pane_id -> FIFO messages waiting for the pane's outbox worker.
    pub(crate) outbox: Mutex<BTreeMap<String, Vec<sends::Message>>>,
    /// Next direct-message identity, matching `_new_message`'s monotonic `mN` allocation.
    next_message_id: AtomicU64,
    /// Which flock this is, for the per-thread caller state below.
    id: u64,
    /// pane_id -> the scope it follows for the next task (`self.task_waiters`).
    pub(crate) task_waiters: Mutex<BTreeMap<String, String>>,
    /// `self._next_job_wake` (an Event): set on turn end, task completion and close; the push
    /// loop waits on it with the sweep interval as the timeout (`flock/loops_push.rs`).
    pub(crate) push_wake: Arc<tokio::sync::Notify>,
    role_panes: Mutex<panes::RolePaneCache>,
    turns: Mutex<BTreeMap<String, crate::activity::Turn>>,
    stores: Mutex<panes::Stores>,
    /// The role registry and the legacy allowlist (T31), attached after construction.
    pub(crate) role_stores: roles_gate::RoleStoresCell,
    /// pane_id -> its full turn (turn_id, caller, handoff; T36). `turns` above is the
    /// activity view `lane_activity` reads; `commands_turns` keeps the two in step.
    pub(crate) turn_states: Mutex<commands_turns::Turns>,
    /// Turn-end nudge cooldowns and the in-progress reservation (T36).
    pub(crate) turn_dispatch: Mutex<commands_turns::TurnDispatchState>,
    /// Shared status view cache for requests served by this flock.
    status_view: status::ViewCache,
}

impl Flock {
    /// `Flock.__init__` (the launches part): `_load_launches` runs when persisting.
    pub fn new(
        launches_file: impl Into<PathBuf>,
        lane_registry_path: impl Into<PathBuf>,
        persist: bool,
    ) -> Flock {
        let flock = Flock {
            launches_file: launches_file.into(),
            lane_registry_path: lane_registry_path.into(),
            persist_enabled: persist,
            launches: Mutex::new(BTreeMap::new()),
            outbox: Mutex::new(BTreeMap::new()),
            next_message_id: AtomicU64::new(1),
            id: NEXT_FLOCK_ID.fetch_add(1, Ordering::Relaxed),
            task_waiters: Mutex::new(BTreeMap::new()),
            push_wake: Arc::new(tokio::sync::Notify::new()),
            role_panes: Mutex::new(panes::RolePaneCache::default()),
            turns: Mutex::new(BTreeMap::new()),
            stores: Mutex::new(panes::Stores::default()),
            role_stores: Mutex::new(roles_gate::RoleStores::default()),
            turn_states: Mutex::new(commands_turns::Turns::new()),
            turn_dispatch: Mutex::new(commands_turns::TurnDispatchState::default()),
            status_view: status::ViewCache::new(),
        };
        flock.load_launches();
        flock
    }

    /// Returns this flock's shared status view cache.
    #[allow(dead_code)]
    pub(crate) fn view_cache(&self) -> &status::ViewCache {
        &self.status_view
    }

    /// Invalidates the cached status view after a state change.
    #[allow(dead_code)]
    pub(crate) fn invalidate_view(&self) {
        self.status_view.invalidate();
    }

    // -- launches.json --------------------------------------------------------

    /// `_load_launches`: a missing or unreadable file is an empty memory, never an error.
    fn load_launches(&self) {
        if !self.persist_enabled || !self.launches_file.exists() {
            return;
        }
        let payload: Value = match std::fs::read_to_string(&self.launches_file)
            .map_err(|e| e.to_string())
            .and_then(|text| serde_json::from_str(&text).map_err(|e| e.to_string()))
        {
            Ok(payload) => payload,
            Err(error) => {
                eprintln!("could not read {}: {error}", self.launches_file.display());
                return;
            }
        };
        if let Some(panes) = payload.get("panes").and_then(Value::as_object) {
            let mut launches = self.launches.lock().expect("launches lock");
            *launches = panes
                .iter()
                .filter_map(|(id, entry)| entry.as_object().map(|e| (id.clone(), e.clone())))
                .collect();
        }
    }

    /// `_save_launches`: `{"version": 1, "panes": ...}`, atomic, 0600; a failed write warns.
    pub fn save_launches(&self) {
        if !self.persist_enabled {
            return;
        }
        let panes = self.launches.lock().expect("launches lock").clone();
        let payload = json!({"version": 1, "panes": panes});
        if let Err(error) = atomic_json::write(&self.launches_file, &payload) {
            eprintln!("could not write {}: {error}", self.launches_file.display());
        }
    }

    /// `_record_launch`: merge the non-null fields and stamp `recorded_at`.
    pub fn record_launch(&self, pane_id: &str, fields: &[(&str, Value)]) {
        if pane_id.is_empty() {
            return;
        }
        {
            let mut launches = self.launches.lock().expect("launches lock");
            let entry = launches.entry(pane_id.to_string()).or_default();
            for (key, value) in fields {
                if !value.is_null() {
                    entry.insert((*key).to_string(), value.clone());
                }
            }
            entry.insert("recorded_at".to_string(), json!(clock::now_s()));
        }
        self.save_launches();
    }

    /// `_snapshot_launches`: keep launches.json current from the backend's agent_session
    /// metadata. A pane needs an agent and a session value; `source` is set to "herdr" only
    /// the first time a changed entry has none (the Python's `changed` flag is cumulative
    /// across panes, and that is kept).
    pub fn snapshot_launches(&self, panes: &[Value]) {
        if !self.persist_enabled {
            return;
        }
        let mut changed = false;
        {
            let mut launches = self.launches.lock().expect("launches lock");
            for pane in panes {
                let session = pane.get("agent_session").cloned().unwrap_or(Value::Null);
                let text = |v: &Value, k: &str| v.get(k).filter(|x| !x.is_null()).cloned();
                // Python `pane.get("agent") or session.get("agent")`: an EMPTY agent (tmux panes
                // report '') falls back to the session's.
                let agent = text(pane, "agent")
                    .filter(|v| v != &json!(""))
                    .or_else(|| text(&session, "agent"));
                let value = text(&session, "value");
                let (Some(agent), Some(value)) = (agent, value) else {
                    continue;
                };
                if agent == json!("") || value == json!("") {
                    continue;
                }
                let pane_id = pane
                    .get("pane_id")
                    .and_then(Value::as_str)
                    .unwrap_or("None")
                    .to_string();
                let entry = launches.entry(pane_id).or_default();
                let cwd = text(pane, "cwd")
                    .filter(|v| v != &json!(""))
                    .or_else(|| text(pane, "foreground_cwd"));
                let fields = [
                    ("agent", Some(agent)),
                    ("session", Some(value)),
                    ("session_kind", text(&session, "kind")),
                    ("cwd", cwd),
                    ("terminal_title", text(pane, "terminal_title_stripped")),
                ];
                for (key, field) in fields {
                    if let Some(field) = field {
                        if entry.get(key) != Some(&field) {
                            entry.insert(key.to_string(), field);
                            changed = true;
                        }
                    }
                }
                if changed && !entry.contains_key("source") {
                    entry.insert("source".to_string(), json!("herdr"));
                }
            }
        }
        if changed {
            self.save_launches();
        }
    }

    /// A copy of one pane's launch entry.
    pub fn launch(&self, pane_id: &str) -> Option<Map<String, Value>> {
        self.launches
            .lock()
            .expect("launches lock")
            .get(pane_id)
            .cloned()
    }

    // -- lane registry ---------------------------------------------------------

    /// `_lane_registry(strict)`: an object, else `{}`; strict raises instead.
    pub fn lane_registry(&self, strict: bool) -> Result<Map<String, Value>, DunkError> {
        let parsed = std::fs::read_to_string(&self.lane_registry_path)
            .map_err(|e| DunkError::Refused(e.to_string()))
            .and_then(|text| {
                serde_json::from_str::<Value>(&text).map_err(|e| DunkError::Refused(e.to_string()))
            });
        match parsed {
            Ok(Value::Object(map)) => Ok(map),
            Ok(_) if strict => Err(DunkError::Refused(
                "lane registry must be an object".to_string(),
            )),
            Err(error) if strict => Err(error),
            _ => Ok(Map::new()),
        }
    }

    /// `_write_lane_registry`: atomic replace under the identity lock; a failure warns and
    /// returns false.
    pub fn write_lane_registry(&self, data: &Map<String, Value>) -> bool {
        let path: &Path = &self.lane_registry_path;
        let guard = path.with_extension("json.lock");
        let result =
            lock::with_exclusive_timeout(&guard, std::time::Duration::from_secs(5), || {
                atomic_json::write(path, data)
            });
        match result {
            Ok(()) => true,
            Err(error) => {
                eprintln!("could not write the lane registry: {error}");
                false
            }
        }
    }

    /// `_plain_lane_name`: may this string become a lane's identity BY INFERENCE? One token
    /// (or a pane id); a title is written by anything that talks to the pane.
    pub fn plain_lane_name(name: &str) -> bool {
        let text = name.trim();
        if text.is_empty() || text.chars().count() > 64 {
            return false;
        }
        let lowered = text.to_lowercase();
        if NOT_A_NAME_MARKERS.iter().any(|m| lowered.contains(m)) {
            return false;
        }
        pane_id_re().is_match(text)
            || herdr_id_re().is_match(text)
            || plain_lane_re().is_match(text)
    }

    /// `_usable_declared_name`: looser than inference (spaces and parentheses are fine), but
    /// notice text or a pipe is corruption, not a name.
    pub fn usable_declared_name(name: &str) -> bool {
        let text = name.trim();
        if text.is_empty() || text.chars().count() > 64 {
            return false;
        }
        let lowered = text.to_lowercase();
        !NOT_A_NAME_MARKERS.iter().any(|m| lowered.contains(m))
    }

    /// `_row_identity`: (mailbox, declared) for a registry row, however spelled. A declared
    /// row that is not a usable name fails closed (and warns).
    pub fn row_identity(value: Option<&Value>) -> (Option<String>, bool) {
        let non_empty = |s: String| if s.is_empty() { None } else { Some(s) };
        let as_text = |v: Option<&Value>| match v {
            Some(Value::String(s)) => s.trim().to_string(),
            Some(Value::Null) | None => String::new(),
            Some(other) => other.to_string().trim().to_string(),
        };
        match value {
            Some(Value::Object(row)) => {
                let name = {
                    let mailbox = as_text(row.get("mailbox"));
                    if mailbox.is_empty() {
                        as_text(row.get("name"))
                    } else {
                        mailbox
                    }
                };
                let declared = matches!(row.get("declared"), Some(v) if truthy(v));
                if declared && !Flock::usable_declared_name(&name) {
                    eprintln!(
                        "lane registry row is not a usable name ({name:?}); ignoring it. \
                         Repair with: dunkingsheep lane set <pane> <name>"
                    );
                    return (None, false);
                }
                (non_empty(name), declared)
            }
            other => (non_empty(as_text(other)), false),
        }
    }

    /// `_remember_lane`: record pane id -> mailbox. A DECLARED identity is immutable from the
    /// inside: inferred names may fill an empty row and never overwrite a declared one.
    pub fn remember_lane(&self, pane_id: &str, mailbox: &str, declared: bool) -> bool {
        let mut data = self.lane_registry(false).unwrap_or_default();
        let existing = data.get(pane_id).cloned();
        let (existing_name, existing_declared) = Flock::row_identity(existing.as_ref());
        if existing_declared && !declared {
            return false;
        }
        if !declared && existing_name.is_some() {
            return false;
        }
        if declared {
            let mut row = match existing {
                Some(Value::Object(row)) => row,
                _ => Map::new(),
            };
            row.insert("mailbox".into(), json!(mailbox.trim()));
            row.insert("declared".into(), json!(true));
            row.entry("declared_at").or_insert(json!(clock::now_s()));
            row.entry("alive").or_insert(json!(true));
            data.insert(pane_id.to_string(), Value::Object(row));
        } else {
            if existing_name.as_deref() == Some(mailbox) {
                return false;
            }
            data.insert(pane_id.to_string(), json!(mailbox.trim()));
        }
        self.write_lane_registry(&data);
        true
    }

    // -- caller state ------------------------------------------------------------

    /// Run `work` on this thread's caller state for this flock. `work` must not call back into
    /// another caller-state method (the cell is borrowed for its duration).
    pub(crate) fn with_caller<R>(&self, work: impl FnOnce(&mut CallerState) -> R) -> R {
        CALLERS.with(|callers| work(callers.borrow_mut().entry(self.id).or_default()))
    }

    /// `set_caller`.
    pub fn set_caller(&self, caller: Option<Value>) {
        self.with_caller(|state| state.value = caller);
    }

    /// `current_caller`.
    pub fn current_caller(&self) -> Option<Value> {
        self.with_caller(|state| state.value.clone())
    }

    /// `clear_caller`: every per-request field except `socket_peer_ok`, which only
    /// `clear_socket_peer` resets (as in the Python).
    pub fn clear_caller(&self) {
        self.with_caller(|state| {
            let peer = state.socket_peer_ok;
            *state = CallerState::default();
            state.socket_peer_ok = peer;
        });
    }

    /// `set_socket_peer` (t423): true = verifiable peer credentials, false = failed (fail
    /// closed), `None` = in-process trusted code.
    pub fn set_socket_peer(&self, ok: Option<bool>) {
        self.with_caller(|state| state.socket_peer_ok = ok);
    }

    /// `clear_socket_peer`.
    pub fn clear_socket_peer(&self) {
        self.set_socket_peer(None);
    }

    /// The tri-state the completion binding reads.
    pub fn socket_peer_ok(&self) -> Option<bool> {
        self.with_caller(|state| state.socket_peer_ok)
    }

    /// A snapshot of the caller state.
    pub fn caller_state(&self) -> CallerState {
        self.with_caller(|state| state.clone())
    }
}

/// Python truthiness for the JSON values a registry row can hold.
fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().map(|f| f != 0.0).unwrap_or(true),
        Value::String(s) => !s.is_empty(),
        Value::Array(a) => !a.is_empty(),
        Value::Object(o) => !o.is_empty(),
    }
}

pub mod agent_verify;
pub mod commands_inbox;
pub mod commands_layout;
pub mod commands_owner_views;
pub mod commands_task_complete;
pub mod commands_tasks;
pub mod commands_turns;
pub mod delivery;
pub mod dunks;
pub mod lane_registry_rows;
pub mod lanes;
pub mod loops;
pub mod loops_push;
pub mod loops_stuck;
pub mod mailbox;
pub mod panes;
pub mod project_summary;
pub mod resolve;
pub mod retire;
pub mod roles_gate;
pub mod roster;
pub mod send_keys;
pub mod sends;
pub mod status;
pub mod views;

#[cfg(test)]
mod commands_inbox_tests;
#[cfg(test)]
mod commands_owner_views_tests;
#[cfg(test)]
mod commands_turns_tests;
#[cfg(test)]
mod dunks_tests;
#[cfg(test)]
mod lanes_tests;
#[cfg(test)]
mod loops_push_tests;
#[cfg(test)]
mod loops_stuck_tests;
#[cfg(test)]
mod loops_tests;
#[cfg(test)]
mod mailbox_tests;
#[cfg(test)]
mod panes_tests;
#[cfg(test)]
mod resolve_tests;
#[cfg(test)]
mod roles_gate_tests;
#[cfg(test)]
mod sends_tests;
#[cfg(test)]
mod status_tests;
#[cfg(test)]
mod tests;
