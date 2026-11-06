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

mod outbox_api;

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use regex::Regex;
use serde_json::{json, Map, Value};

use crate::error::DunkError;
use crate::{atomic_json, clock, lock};

/// Holds the one canonical memory service without exposing its internals via `Flock: Debug`.
#[derive(Default)]
pub(crate) struct MemoryServiceSlot(Mutex<Option<Arc<crate::memory::service::MemoryService>>>);

impl std::fmt::Debug for MemoryServiceSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("MemoryServiceSlot")
            .field(&self.0.lock().expect("memory service lock").is_some())
            .finish()
    }
}

/// Holds the one explicitly attached project policy for this canonical Flock.
/// The attached project policy plus the startup reason any part of it was refused (t1692).
#[derive(Default)]
pub(crate) struct ProjectRuntimePolicySlot {
    policy: Mutex<Option<Arc<crate::server_runtime::ProjectRuntimePolicy>>>,
    refusal: Mutex<Option<String>>,
}

impl std::fmt::Debug for ProjectRuntimePolicySlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProjectRuntimePolicySlot")
            .field(
                "attached",
                &self
                    .policy
                    .lock()
                    .expect("project runtime policy lock")
                    .is_some(),
            )
            .finish()
    }
}

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

// Canonical launch insertion order matches Python dict; persistence remains sorted JSON.
type LaunchMap =
    indexmap::IndexMap<String, Map<String, Value>, std::collections::hash_map::RandomState>;

/// The flock. Slice 1 holds launch memory, the lane registry path and caller state.
#[derive(Debug)]
pub struct Flock {
    pub launches_file: PathBuf,
    pub lane_registry_path: PathBuf,
    pub persist_enabled: bool,
    /// pane_id -> {agent, session, session_kind, cwd, argv, command, source, recorded_at}
    launches: Mutex<LaunchMap>,
    /// The in-memory dunk list and next_id slot. Whole-state persistence is owned by the
    /// later snapshot leaf; this leaf only loads/extracts dunk fields and invokes its seam.
    #[allow(dead_code)]
    // Snapshot and runtime dunk CRUD callers are assigned to dependent leaves.
    pub(crate) dunks: dunks::DunkList,
    /// pane_id -> FIFO messages waiting for the pane's outbox worker.
    pub(crate) outbox: Mutex<sends::Outbox>,
    /// pane_id -> the latest delivery intent used by the stuck observer.
    #[allow(dead_code)] // The stuck-recovery integration caller is owned by a later leaf.
    pub(crate) stuck_candidates: Mutex<BTreeMap<String, sends::StuckCandidate>>,
    /// Bounded recently decided messages, used by the message-list view.
    #[allow(dead_code)] // The list consumer is owned by a dependent T33 integration slice.
    pub(crate) message_history: Mutex<VecDeque<sends::Message>>,
    /// Bounded id-keyed tombstones preventing decided messages from being restored as queued.
    #[allow(dead_code)] // Snapshot restore is owned by a dependent T33 integration slice.
    pub(crate) decided: Mutex<sends::DecidedMessages>,
    /// Stuck-observer state is persisted with the same whole-Flock snapshot.
    pub(crate) stuck_tracker: Mutex<crate::stuck::Tracker>,
    /// Recovery policy persisted alongside the attempt ledger.
    pub(crate) stuck_config: Mutex<crate::stuck::Config>,
    /// Volatile per-pane observation throttling; the spent ledger is persisted separately.
    pub(crate) stuck_last_check: Mutex<BTreeMap<String, f64>>,
    /// pane_id -> the bounded recent text this flock typed into its composer.
    #[allow(dead_code)] // The send integration callers are assigned to dependent T33 leaves.
    pub(crate) sent_text_by_pane: Mutex<BTreeMap<String, Vec<serde_json::Value>>>,
    /// Next direct-message identity, matching `_new_message`'s monotonic `mN` allocation.
    next_message_id: AtomicU64,
    /// One whole-send lock per pane (`Flock._lock_for`). The `Arc` lets a caller keep the
    /// selected lock alive while holding its mutex guard.
    pub(crate) pane_send_locks: Mutex<BTreeMap<String, Arc<Mutex<()>>>>,
    /// The reentrant, fleet-wide keystroke-burst lock (`Flock._typing`).
    pub(crate) typing_lock: sends::TypingLock,
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
    memory_service: MemoryServiceSlot,
    /// Explicit project/provider policy attached by the startup owner; absent is fail-closed.
    project_runtime_policy: ProjectRuntimePolicySlot,
    /// The role registry and the legacy allowlist (T31), attached after construction.
    pub(crate) role_stores: roles_gate::RoleStoresCell,
    /// pane_id -> its full turn (turn_id, caller, handoff; T36). `turns` above is the
    /// activity view `lane_activity` reads; `commands_turns` keeps the two in step.
    pub(crate) turn_states: Mutex<commands_turns::Turns>,
    /// Turn-end nudge cooldowns and the in-progress reservation (T36).
    pub(crate) turn_dispatch: Mutex<commands_turns::TurnDispatchState>,
    /// Shared status view cache for requests served by this flock.
    status_view: status::ViewCache,
    /// Serializes snapshot selection through atomic replacement so an older snapshot cannot win.
    snapshot_lock: Mutex<()>,
    status_runtime: OnceLock<status_runtime::StatusState>,
    last_message: Mutex<String>,
    /// Per-pane successful retry key, matching Python Flock._submit_key.
    pub(crate) learned_submit_keys: Mutex<BTreeMap<String, String>>,
    /// Pane -> last author-App token mint time, matching Python Flock._gh_token_mints.
    pub(crate) gh_token_mints: Mutex<BTreeMap<String, f64>>,
    quarantined_mailboxes: Mutex<Vec<Value>>,
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
            launches: Mutex::new(LaunchMap::default()),
            dunks: dunks::DunkList::new(),
            outbox: Mutex::new(sends::Outbox::default()),
            stuck_candidates: Mutex::new(BTreeMap::new()),
            message_history: Mutex::new(VecDeque::new()),
            decided: Mutex::new(sends::DecidedMessages::default()),
            stuck_tracker: Mutex::new(crate::stuck::Tracker::default()),
            stuck_config: Mutex::new(crate::stuck::Config::default()),
            stuck_last_check: Mutex::new(BTreeMap::new()),
            sent_text_by_pane: Mutex::new(BTreeMap::new()),
            next_message_id: AtomicU64::new(1),
            pane_send_locks: Mutex::new(BTreeMap::new()),
            typing_lock: sends::TypingLock::default(),
            id: NEXT_FLOCK_ID.fetch_add(1, Ordering::Relaxed),
            task_waiters: Mutex::new(BTreeMap::new()),
            push_wake: Arc::new(tokio::sync::Notify::new()),
            role_panes: Mutex::new(panes::RolePaneCache::default()),
            turns: Mutex::new(BTreeMap::new()),
            stores: Mutex::new(panes::Stores::default()),
            memory_service: MemoryServiceSlot::default(),
            project_runtime_policy: ProjectRuntimePolicySlot::default(),
            role_stores: Mutex::new(roles_gate::RoleStores::default()),
            turn_states: Mutex::new(commands_turns::Turns::new()),
            turn_dispatch: Mutex::new(commands_turns::TurnDispatchState::default()),
            status_view: status::ViewCache::new(),
            snapshot_lock: Mutex::new(()),
            status_runtime: OnceLock::new(),
            last_message: Mutex::new(String::new()),
            learned_submit_keys: Mutex::new(BTreeMap::new()),
            gh_token_mints: Mutex::new(BTreeMap::new()),
            quarantined_mailboxes: Mutex::new(Vec::new()),
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

    /// Capture every durable field owned by this Flock in one consistent JSON value.
    #[allow(dead_code)] // Used by the shared whole-Flock save and restore seam.
    pub(crate) fn snapshot(&self) -> Value {
        let (next_id, dunk_rows) = self.dunks.snapshot();
        let dunkers = dunk_rows
            .into_iter()
            .map(|pairs| {
                Value::Object(
                    pairs
                        .into_iter()
                        .map(|(key, value)| (key.to_string(), value))
                        .collect(),
                )
            })
            .collect::<Vec<_>>();
        let next_message_id = self.next_message_id.load(Ordering::Relaxed);
        let outbox = sends::outbox_lock(&self.outbox)
            .values()
            .flatten()
            .filter(|message| message.status == "queued")
            .map(sends::message_snapshot)
            .collect::<Vec<_>>();
        let stuck_candidates = self
            .stuck_candidates
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(pane, candidate)| (pane.clone(), sends::candidate_snapshot(candidate)))
            .collect::<Map<String, Value>>();
        let stuck = self
            .stuck_tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .to_state();
        let config = *self
            .stuck_config
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Rows stay in decided-order (the Map keeps insertion order), which is the order the Python
        // file lists them in. A separate order list would be an extra key the Python never writes.
        let decided = {
            let decided = self
                .decided
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            decided
                .order
                .iter()
                .filter_map(|id| {
                    decided.by_id.get(id).map(|entry| {
                        (
                            id.clone(),
                            json!({"status": entry.status, "created_at": entry.created_at}),
                        )
                    })
                })
                .collect::<Map<String, Value>>()
        };
        json!({
            "version": 1,
            "next_id": next_id,
            "next_message_id": next_message_id,
            "dunkers": dunkers,
            "stuck_candidates": stuck_candidates,
            "stuck": stuck,
            "stuck_config": {
                "enabled": config.enabled,
                "threshold_s": config.threshold_s,
                "max_attempts": config.max_attempts,
                "cooldown_s": config.cooldown_s,
                "observation_interval_s": config.observation_interval_s,
            },
            "outbox": outbox,
            "decided": decided,
        })
    }

    /// Atomically persist one whole-Flock snapshot. The serialization guard spans snapshot
    /// selection and replacement, so an older snapshot cannot become the final file.
    #[allow(dead_code)] // Send/outbox callers share this persistence edge.
    pub(crate) fn save(&self) -> Result<(), DunkError> {
        self.save_classified()
            .map_err(sends::SnapshotSaveFailure::into_dunk_error)
    }

    /// Preserve the OS-vs-serialization boundary for the small set of legacy dunk
    /// operations whose Python callers intentionally ignore only OSError.
    pub(crate) fn save_classified(&self) -> Result<(), sends::SnapshotSaveFailure> {
        self.invalidate_view();
        if !self.persist_enabled {
            return Ok(());
        }
        let _serialized = self
            .snapshot_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let path = self
            .status_state()
            .map_err(sends::SnapshotSaveFailure::Other)?
            .state_file
            .clone();
        let payload = self.snapshot();
        crate::atomic_json::write_python_state_classified(&path, &payload).map_err(|error| {
            match error {
                atomic_json::ClassifiedWriteError::Io(error) => {
                    sends::SnapshotSaveFailure::Io(error)
                }
                atomic_json::ClassifiedWriteError::Other(error) => {
                    sends::SnapshotSaveFailure::Other(error)
                }
            }
        })
    }

    /// Rebuild queued messages from snapshot rows. Inbox read errors keep the original row.
    #[allow(dead_code)] // Startup restore invokes this as part of whole-state restoration.
    pub(crate) fn restore_outbox(&self, rows: &[Value]) {
        let inbox = self.stores().inbox;
        let decided = self
            .decided
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .by_id
            .clone();
        let mut restored = sends::Outbox::default();
        for row in rows {
            let Some(message) = sends::message_from_snapshot(row) else {
                continue;
            };
            if decided.get(&message.id).is_some_and(|entry| {
                sends::decided_tombstone_matches_row(
                    entry,
                    row.get("created_at").and_then(Value::as_f64),
                )
            }) {
                continue;
            }
            if !message.notify_for.is_empty() {
                if let Some(inbox) = inbox.as_ref() {
                    let statuses = message
                        .notify_for
                        .iter()
                        .map(|id| {
                            crate::inbox::status(inbox.read_pool(), id, crate::clock::now_s())
                        })
                        .collect::<Result<Vec<_>, _>>();
                    if let Ok(statuses) = statuses {
                        let states = statuses
                            .iter()
                            .map(|status| status.notify_state.as_deref())
                            .collect::<Vec<_>>();
                        let all_arrived = !states.is_empty()
                            && states
                                .iter()
                                .all(|state| matches!(*state, Some("sent" | "cancelled")));
                        let no_longer_waiting = !states.is_empty()
                            && states.iter().all(|state| {
                                matches!(*state, Some("sent" | "failed" | "cancelled"))
                            })
                            && statuses.iter().all(|status| status.state != "queued");
                        if all_arrived || no_longer_waiting {
                            continue;
                        }
                    }
                }
            }
            eprintln!(
                "restored queued message {} for {}",
                message.id, message.pane_id
            );
            restored
                .entry(message.pane_id.clone())
                .or_default()
                .push(message);
        }
        *sends::outbox_lock(&self.outbox) = restored;
    }

    /// Restore every field owned by the snapshot leaf after the file is decoded.
    #[allow(dead_code)] // Startup restore wiring is in the dependent runtime task.
    pub(crate) fn restore_snapshot(&self, payload: &Value) {
        let Some(root) = payload.as_object() else {
            return;
        };
        self.dunks.load(root);
        if let Some(next) = root.get("next_message_id").and_then(Value::as_u64) {
            self.next_message_id.store(next.max(1), Ordering::Relaxed);
        }
        if let Some(rows) = root.get("stuck_candidates").and_then(Value::as_object) {
            let candidates = rows
                .iter()
                .filter_map(|(pane, value)| {
                    sends::candidate_from_snapshot(value).map(|candidate| (pane.clone(), candidate))
                })
                .collect();
            *self
                .stuck_candidates
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = candidates;
        }
        if let Some(state) = root.get("stuck") {
            if let Ok(tracker) = crate::stuck::Tracker::from_state(state) {
                *self
                    .stuck_tracker
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = tracker;
            }
        }
        if let Some(config) = root.get("stuck_config").and_then(Value::as_object) {
            let raw = crate::stuck::RawConfig {
                enabled: config.get("enabled").and_then(Value::as_bool),
                threshold_s: config.get("threshold_s").and_then(Value::as_f64),
                max_attempts: config.get("max_attempts").and_then(Value::as_i64),
                cooldown_s: config.get("cooldown_s").and_then(Value::as_f64),
                observation_interval_s: config
                    .get("observation_interval_s")
                    .and_then(Value::as_f64),
            };
            if let Ok(config) = crate::stuck::Config::from_raw(&raw) {
                *self
                    .stuck_config
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = config;
            }
        }
        if let Some(decided) = root.get("decided").and_then(Value::as_object) {
            let mut state = sends::DecidedMessages::default();
            // The order the file lists the ids in (preserve_order keeps it): the Python writes
            // them in decided order, so that is the order a restart must keep.
            let mut file_order: Vec<String> = Vec::with_capacity(decided.len());
            for (id, entry) in decided {
                let (status, created_at) = match entry {
                    Value::Object(row) => (
                        row.get("status")
                            .and_then(Value::as_str)
                            .unwrap_or("unknown")
                            .to_string(),
                        row.get("created_at").and_then(Value::as_f64),
                    ),
                    Value::String(status) => (status.clone(), None),
                    _ => continue,
                };
                state
                    .by_id
                    .insert(id.clone(), sends::DecidedTombstone { status, created_at });
                file_order.push(id.clone());
            }
            // Files written before the Python-compatible writer carry a separate `decided_order`
            // (the Rust writer sorted keys then); it still wins for those files.
            if let Some(order) = root.get("decided_order").and_then(Value::as_array) {
                for id in order.iter().filter_map(Value::as_str) {
                    if state.by_id.contains_key(id) && !state.order.iter().any(|seen| seen == id) {
                        state.order.push_back(id.to_string());
                    }
                }
            }
            for id in file_order {
                if !state.order.iter().any(|seen| *seen == id) {
                    state.order.push_back(id);
                }
            }
            while state.order.len() > sends::DECIDED_KEPT {
                if let Some(id) = state.order.pop_front() {
                    state.by_id.remove(&id);
                }
            }
            *self
                .decided
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = state;
        }
        if let Some(rows) = root.get("outbox").and_then(Value::as_array) {
            self.restore_outbox(rows);
        }
    }

    /// Remember a successful author-App token mint for review attribution.
    pub(crate) fn record_gh_token_mint(&self, pane_id: &str, at: f64) {
        if !pane_id.is_empty() {
            self.gh_token_mints
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(pane_id.to_owned(), at);
        }
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
        let panes: BTreeMap<_, _> = self
            .launches
            .lock()
            .expect("launches lock")
            .clone()
            .into_iter()
            .collect();
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
                let name = as_text(
                    row.get("mailbox")
                        .filter(|value| truthy(value))
                        .or_else(|| row.get("name").filter(|value| truthy(value))),
                );
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
pub(crate) mod allocator;
pub mod clear_box;
pub mod commands_inbox;
pub mod commands_layout;
pub mod commands_owner_views;
pub mod commands_review;
pub mod commands_task_complete;
pub mod commands_tasks;
pub mod commands_turns;
pub mod delivery;
pub mod dunks;
pub mod guarded_submit;
pub mod lane_registry_rows;
pub mod lanes;
pub mod loop_env;
pub mod loops;
pub mod loops_push;
pub mod loops_stuck;
pub mod mailbox;
pub mod panes;
pub mod project_summary;
pub mod recovery_commands;
pub mod resolve;
pub mod retire;
mod retired;
pub mod roles_gate;
pub mod roster;
pub mod send_keys;
pub mod sends;
pub mod status;
pub mod status_runtime;
pub mod task_wire;
pub mod views;

#[cfg(test)]
mod clear_box_tests;
#[cfg(test)]
mod commands_inbox_tests;
#[cfg(test)]
mod commands_owner_views_tests;
#[cfg(test)]
mod commands_turns_tests;
#[cfg(test)]
mod dunks_state_file_tests;
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
mod snapshot_tests;
#[cfg(test)]
mod status_tests;
#[cfg(test)]
mod tests;

pub mod respawn;

pub mod context_mutations;
