//! `flock/roles_gate.rs` (T31): who the caller is and what that allows, ported from
//! `dunk_core.Flock`: `caller_role`, `set_service_token`, `is_superuser`, `require_superuser`,
//! `whoami`, `lane_register` / `lane_unregister`, `token_issue` / `token_revoke` and the legacy
//! `superuser_*` spellings. The registry itself is `roles::Roles` (T31a); the legacy allowlist
//! is `superusers::Superusers`; the pane record comes through `PaneInfoEnv`.
//!
//! The stores are attached after construction (`attach_roles`), like the tasks/inbox stores:
//! a gate call on a flock with no registry attached is a named refusal, never a guess.
//!
//! Successful registration publishes its status message on the Flock owner.

use std::sync::Mutex;

use serde_json::{json, Map, Value};

use super::panes::PaneInfoEnv;
use super::Flock;
use crate::error::DunkError;
use crate::roles::{Roles, DEFAULT_ROLE};
use crate::superusers::Superusers;

/// The role stores a `Flock` owns.
#[derive(Debug, Default)]
pub struct RoleStores {
    pub roles: Option<Roles>,
    /// `Flock._legacy_superusers`: the pre-registry allowlist, still honoured for `owner`.
    pub legacy: Option<Superusers>,
}

/// The `Flock` field type.
pub type RoleStoresCell = Mutex<RoleStores>;

/// One ACCEPTED owner-token call from a caller outside every pane (t4553). The gate still
/// accepts these ("legacy detached owner"); this record is the evidence of who they are: it is
/// logged and counted on every accept, and never changes the decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DetachedOwnerCall {
    pub pid: i64,
    /// Kernel name of the caller's parent, if it has one in the chain.
    pub parent: String,
    /// The top of the caller's process chain: the ancestor whose parent is init.
    pub root: (i64, String),
    pub cwd: String,
    /// `first`, `same_root` or `new_root`, against the previous accepted detached call.
    pub root_seen: &'static str,
}

impl DetachedOwnerCall {
    pub(crate) fn new(
        chain: &[crate::proc::ProcStat],
        cwd: Option<String>,
        previous: Option<&DetachedOwnerCall>,
    ) -> Option<Self> {
        let (caller, top) = (chain.first()?, chain.last()?);
        let root = (top.pid, top.start.clone());
        let root_seen = match previous {
            None => "first",
            Some(previous) if previous.root == root => "same_root",
            Some(_) => "new_root",
        };
        Some(Self {
            pid: caller.pid,
            parent: chain.get(1).map(|p| p.comm.clone()).unwrap_or_default(),
            root,
            cwd: cwd.unwrap_or_default(),
            root_seen,
        })
    }

    /// The journal line. Free text is escaped and bounded; no token or argument is included.
    pub(crate) fn line(&self) -> String {
        let short = |text: &str| text.chars().take(120).collect::<String>();
        format!(
            "owner token accepted from a detached caller: pid={} parent={:?} root={}:{} cwd={:?} \
             root_seen={}",
            self.pid,
            short(&self.parent),
            self.root.0,
            short(&self.root.1),
            short(&self.cwd),
            self.root_seen
        )
    }
}

fn refuse(text: impl Into<String>) -> DunkError {
    DunkError::Refused(text.into())
}

fn non_empty(text: Option<&str>) -> Option<&str> {
    text.filter(|t| !t.is_empty())
}

/// t4782: where a system-token caller's process was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SystemPeer {
    /// A `dunkingsheep-*.service` unit, named.
    Unit(String),
    /// Inside these listed panes.
    Panes(Vec<String>),
    /// Outside every listed pane and not a dunkingsheep service unit.
    Detached,
    /// The place could not be established.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SystemTokenVerdict {
    Accept,
    /// Accepted for this release; the line is logged.
    Warn(String),
    /// Refused; the line is logged.
    Refuse(String),
}

/// The systemd unit a process runs in: the last path component of its unified cgroup
/// (`0::/user.slice/.../dunkingsheep-watchdog.service`), when that component is a unit name.
pub(crate) fn unit_of_cgroup(cgroup: &str) -> Option<String> {
    let path = cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))?
        .trim();
    let unit = path.rsplit('/').next()?;
    (unit.ends_with(".service") || unit.ends_with(".scope")).then(|| unit.to_string())
}

/// The daemon's own timer and helper services. The daemon itself (`dunkingsheep.service`) does
/// not call its own socket with a token.
pub(crate) fn is_system_unit(unit: &str) -> bool {
    unit.strip_prefix("dunkingsheep-")
        .and_then(|rest| rest.strip_suffix(".service"))
        .is_some_and(|name| {
            !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '@')
        })
}

/// The decision for a system-token caller. Every line names the token's label and the place,
/// never the token.
pub(crate) fn system_token_verdict(
    label: &str,
    peer: &SystemPeer,
    allowed_panes: &[String],
) -> SystemTokenVerdict {
    match peer {
        SystemPeer::Unit(_) => SystemTokenVerdict::Accept,
        SystemPeer::Detached => SystemTokenVerdict::Refuse(format!(
            "system token '{label}' refused: the caller is outside every pane and is not a dunkingsheep-*.service unit"
        )),
        SystemPeer::Panes(panes)
            if !panes.is_empty() && panes.iter().all(|pane| allowed_panes.contains(pane)) =>
        {
            SystemTokenVerdict::Accept
        }
        SystemPeer::Panes(panes) => SystemTokenVerdict::Warn(format!(
            "WARNING system token '{label}' used from pane {}, which the owner has not listed for it; accepted for this release, to be refused in the next",
            panes.join(", ")
        )),
        SystemPeer::Unknown => SystemTokenVerdict::Warn(format!(
            "WARNING system token '{label}' used by a caller whose place could not be established; accepted for this release, to be refused in the next"
        )),
    }
}

/// A socket caller's verified process ancestry and the listed panes it sits inside.
struct PeerPlace {
    first: crate::proc::ProcStat,
    start: String,
    chain: Vec<crate::proc::ProcStat>,
    matched: Vec<(String, i64)>,
}

impl Flock {
    pub(crate) fn owner_token_audit_actor(
        &self,
        token: Option<&str>,
    ) -> Result<Option<String>, DunkError> {
        match token.filter(|token| !token.is_empty()) {
            Some(token) => self.with_roles(|roles, _| roles.owner_token_audit_label(token)),
            None => Ok(None),
        }
    }
    /// Attach the registry (and the optional legacy allowlist).
    pub fn attach_roles(&self, roles: Roles, legacy: Option<Superusers>) {
        *self.role_stores.lock().expect("role stores lock") = RoleStores {
            roles: Some(roles),
            legacy,
        };
    }

    pub(crate) fn with_roles<T>(
        &self,
        work: impl FnOnce(&mut Roles, Option<&Superusers>) -> Result<T, DunkError>,
    ) -> Result<T, DunkError> {
        let mut guard = self.role_stores.lock().expect("role stores lock");
        let RoleStores { roles, legacy } = &mut *guard;
        let roles = roles
            .as_mut()
            .ok_or_else(|| refuse("no role registry is attached to this flock"))?;
        work(roles, legacy.as_ref())
    }

    /// `set_service_token`.
    pub fn set_service_token(&self, token: Option<&str>) {
        self.with_caller(|state| {
            state.service_token = non_empty(token).map(String::from);
            state.role = None;
        });
    }

    /// `caller_role`: the token first, then the registry (with the live pane record for a
    /// registration that recorded a root pid). Cached per request inside a socket request only,
    /// so the allowlist check and the handlers agree on one answer; in-process callers and
    /// tests see every registry change at once.
    pub fn caller_role(
        &self,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<String, DunkError> {
        let pane = non_empty(self_pane_id);
        let cached = self.with_caller(|state| {
            let in_request = state.socket_peer_ok.is_some();
            (in_request && state.role_pane.as_deref() == self_pane_id)
                .then(|| state.role.clone())
                .flatten()
        });
        if let Some(cached) = cached {
            return Ok(cached);
        }
        let token = self.with_caller(|state| state.service_token.clone());
        let mut role: Option<String> = None;
        let mut owner_token = false;
        // The non-secret label of a system token, when this request carries one.
        let mut system_label: Option<String> = None;
        if let Some(token) = non_empty(token.as_deref()) {
            role = self.with_roles(|roles, _| roles.token_role(token))?;
            // The owner token is also used by a legitimate detached owner app.
            // Deny its direct use from a non-owner pane without trusting the
            // client-supplied pane id. Detached callers remain a compatibility
            // exception; this is not an adversarial same-UID isolation boundary.
            owner_token = role.as_deref() == Some("owner");
            if role.as_deref() == Some("system") {
                system_label = Some(
                    self.with_roles(|roles, _| roles.token_label(token))?
                        .unwrap_or_else(|| "system".to_string()),
                );
            }
            if role.is_none() {
                eprintln!("service token refused (unknown token)");
            }
        }
        if role.is_none() {
            role = Some(match pane {
                None => DEFAULT_ROLE.to_string(),
                Some(pane) => {
                    // Only a registration that recorded a root pid needs the live pane record;
                    // an unregistered pane costs no terminal read at all.
                    let wants_info = self.with_roles(|roles, _| {
                        Ok(roles
                            .entry(pane)?
                            .is_some_and(|e| e.get("pane_pid").is_some_and(|p| !p.is_null())))
                    })?;
                    let info = if wants_info {
                        self.role_pane_info(env, pane, crate::clock::now_s())
                    } else {
                        None
                    };
                    self.with_roles(|roles, legacy| {
                        let role = roles.role_of(Some(pane), info.as_ref())?;
                        let legacy_owner = legacy.is_some_and(|l| l.panes.contains(pane));
                        Ok(if role == roles.default && legacy_owner {
                            "owner".to_string()
                        } else {
                            role
                        })
                    })?
                }
            });
        }
        let role = role.expect("set above");
        if role == "owner" && self.socket_peer_ok().is_some() {
            let uid = self.with_caller(|state| state.socket_peer_uid);
            if self.socket_peer_ok() != Some(true) || uid != Some(nix::unistd::geteuid().as_raw()) {
                return Err(refuse("owner role requires the daemon Unix principal"));
            }
            if owner_token {
                self.owner_token_peer_check(env)?;
            }
        }
        if let Some(label) = system_label.as_deref().filter(|_| role == "system") {
            // In-process callers (no socket request) are trusted code, as for the owner role.
            if self.socket_peer_ok().is_some() {
                self.system_token_peer_check(env, label)?;
            }
        }
        self.with_caller(|state| {
            state.role = Some(role.clone());
            state.role_pane = self_pane_id.map(String::from);
        });
        Ok(role)
    }

    /// Log and count an accepted detached owner-token call. Never fails the request.
    fn audit_detached_owner(&self, chain: &[crate::proc::ProcStat]) {
        let cwd = chain.first().and_then(|caller| {
            std::fs::read_link(format!("/proc/{}/cwd", caller.pid))
                .ok()
                .map(|path| path.to_string_lossy().into_owned())
        });
        let mut last = self
            .detached_owner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(call) = DetachedOwnerCall::new(chain, cwd, last.as_ref()) else {
            return;
        };
        eprintln!("{}", call.line());
        if let Some(metrics) = self.stores().metrics {
            metrics.record(&crate::metrics::Event {
                at: crate::clock::now_s(),
                kind: "owner.detached_accept".into(),
                source: Some(format!("{}:{}", call.root.0, call.root.1)),
                reason: Some(call.parent.chars().take(120).collect()),
                outcome: Some(call.root_seen.into()),
                ..crate::metrics::Event::default()
            });
        }
        *last = Some(call);
    }

    /// Where a socket caller's process sits relative to the listed panes: its verified
    /// ancestry chain and every listed pane whose root process is in that chain. Fails with
    /// `refuse_peer()` whenever the answer cannot be established: no verified socket peer, a
    /// dead or changing process, an unreadable pane root, an empty pane listing, or a tmux
    /// server in the ancestry that the configured listing does not account for.
    fn peer_place(
        &self,
        env: &dyn PaneInfoEnv,
        refuse_peer: &dyn Fn() -> DunkError,
    ) -> Result<PeerPlace, DunkError> {
        if self.socket_peer_ok() != Some(true) {
            return Err(refuse_peer());
        }
        let caller = self.current_caller().ok_or_else(refuse_peer)?;
        let mut pid = caller
            .get("pid")
            .and_then(Value::as_i64)
            .filter(|p| *p > 1)
            .ok_or_else(refuse_peer)?;
        let start = caller
            .get("start")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(refuse_peer)?;
        let first = crate::proc::stat(pid).ok_or_else(refuse_peer)?;
        if first.start != start || crate::proc::DEAD_STATES.contains(&first.state.as_str()) {
            return Err(refuse_peer());
        }
        let mut ancestry = std::collections::HashSet::new();
        let mut chain = Vec::new();
        let mut reached_root = false;
        for _ in 0..crate::proc::MAX_DEPTH {
            if !ancestry.insert(pid) {
                return Err(refuse_peer());
            }
            let process = crate::proc::stat(pid).ok_or_else(refuse_peer)?;
            if crate::proc::DEAD_STATES.contains(&process.state.as_str()) {
                return Err(refuse_peer());
            }
            chain.push(process.clone());
            if process.ppid <= 1 {
                reached_root = true;
                break;
            }
            pid = process.ppid;
        }
        if !reached_root {
            return Err(refuse_peer());
        }
        let panes = env.try_authority_panes()?;
        if panes.is_empty() {
            return Err(refuse_peer());
        }
        let mut matched = Vec::new();
        for pane in &panes {
            let id = pane
                .get("pane_id")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(refuse_peer)?;
            let root = pane
                .get("pane_pid")
                .and_then(|v| {
                    v.as_i64()
                        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                })
                .filter(|p| *p > 1)
                .ok_or_else(refuse_peer)?;
            // A listed pane with an unreadable/dead root makes it impossible to
            // establish that a caller is truly outside every visible pane.
            let root_stat = crate::proc::stat(root).ok_or_else(refuse_peer)?;
            if crate::proc::DEAD_STATES.contains(&root_stat.state.as_str()) {
                return Err(refuse_peer());
            }
            if ancestry.contains(&root) {
                matched.push((id.to_string(), root));
            }
        }
        // A private or omitted tmux server must not be mistaken for a detached
        // caller merely because the configured server did not list its pane.
        for pair in chain.windows(2) {
            if pair[1].comm == "tmux: server"
                && !matched.iter().any(|(_, root)| *root == pair[0].pid)
            {
                return Err(refuse_peer());
            }
        }
        // Detect process exit, PID reuse or reparenting during the lookup.
        for prior in &chain {
            let current = crate::proc::stat(prior.pid).ok_or_else(refuse_peer)?;
            if current.start != prior.start
                || current.ppid != prior.ppid
                || crate::proc::DEAD_STATES.contains(&current.state.as_str())
            {
                return Err(refuse_peer());
            }
        }
        Ok(PeerPlace {
            first,
            start: start.to_string(),
            chain,
            matched,
        })
    }

    /// t4782: where a system-token caller connects from. The system token is a file readable by
    /// the daemon's Unix user, so the token alone does not show that the caller is one of the
    /// daemon's own services. Like the owner-token check, this is not a boundary against a
    /// determined same-user process; it stops the token being used casually from a lane.
    ///
    /// * a `dunkingsheep-*.service` unit (the timers): accepted, without needing the pane
    ///   listing, so the watchdog still delivers when the terminal backend is down;
    /// * outside every listed pane and not such a unit: REFUSED;
    /// * inside a pane the owner listed for this token label: accepted;
    /// * inside any other pane, or a place that cannot be established: accepted for this
    ///   release with a warning that names the label and the pane, never the token.
    fn system_token_peer_check(&self, env: &dyn PaneInfoEnv, label: &str) -> Result<(), DunkError> {
        let unknown = || refuse("system token refused: peer identity unavailable");
        let verified_unit = || -> Option<String> {
            if self.socket_peer_ok() != Some(true) {
                return None;
            }
            let caller = self.current_caller()?;
            let pid = caller
                .get("pid")
                .and_then(Value::as_i64)
                .filter(|p| *p > 1)?;
            let start = caller.get("start").and_then(Value::as_str)?;
            if !crate::proc::process_alive(pid, Some(start)) {
                return None;
            }
            let unit =
                unit_of_cgroup(&std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?)?;
            // The pid must still be the same process after the read.
            crate::proc::process_alive(pid, Some(start)).then_some(unit)
        };
        let peer = match verified_unit().filter(|unit| is_system_unit(unit)) {
            Some(unit) => SystemPeer::Unit(unit),
            None => match self.peer_place(env, &unknown) {
                Ok(place) if place.matched.is_empty() => SystemPeer::Detached,
                Ok(place) => {
                    SystemPeer::Panes(place.matched.into_iter().map(|(pane, _)| pane).collect())
                }
                Err(_) => SystemPeer::Unknown,
            },
        };
        let allowed = self
            .with_roles(|roles, _| Ok(roles.system_token_panes(label)))
            .unwrap_or_default();
        match system_token_verdict(label, &peer, &allowed) {
            SystemTokenVerdict::Accept => Ok(()),
            SystemTokenVerdict::Warn(line) => {
                eprintln!("{line}");
                Ok(())
            }
            SystemTokenVerdict::Refuse(line) => {
                eprintln!("{line}");
                Err(refuse(
                    "system token refused: the caller is not a dunkingsheep service unit",
                ))
            }
        }
    }

    fn owner_token_peer_check(&self, env: &dyn PaneInfoEnv) -> Result<(), DunkError> {
        let refuse_peer =
            || refuse("owner token refused: peer identity or pane authority unavailable");
        let PeerPlace {
            first,
            start,
            chain,
            matched,
        } = self.peer_place(env, &refuse_peer)?;
        let start = start.as_str();
        if matched.is_empty() {
            // Legacy detached owner compatibility: still accepted, now on the record.
            self.audit_detached_owner(&chain);
            return Ok(());
        }
        if matched.len() != 1 {
            return Err(refuse_peer());
        }
        let (pane, root) = (matched[0].0.as_str(), matched[0].1);
        let entry = self
            .with_roles(|roles, _| roles.entry(pane))?
            .ok_or_else(refuse_peer)?;
        let stored_pid = entry.get("pane_pid").and_then(|v| {
            v.as_i64()
                .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        });
        let stored_start = entry
            .get("start")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(refuse_peer)?;
        if entry.get("role").and_then(Value::as_str) != Some("owner")
            || stored_pid != Some(root)
            || !crate::proc::process_alive(root, Some(stored_start))
            || !crate::proc::process_alive(first.pid, Some(start))
        {
            return Err(refuse_peer());
        }
        Ok(())
    }

    /// `is_superuser`: the owner role.
    pub fn is_superuser(
        &self,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<bool, DunkError> {
        Ok(self.caller_role(self_pane_id, env)? == "owner")
    }

    /// `require_superuser`: fail closed; only the owner role may do destructive things.
    pub fn require_superuser(
        &self,
        self_pane_id: Option<&str>,
        action: &str,
        env: &dyn PaneInfoEnv,
    ) -> Result<(), DunkError> {
        let any_owner = self.with_roles(|roles, legacy| {
            Ok(roles.has_owner()? || legacy.is_some_and(|l| !l.panes.is_empty()))
        })?;
        if !any_owner {
            return Err(refuse(
                "no owner is registered; run `dunkingsheep lane register --role owner` from the owner's pane (bootstrap), then retry",
            ));
        }
        if !self.is_superuser(self_pane_id, env)? {
            let who = non_empty(self_pane_id).unwrap_or("this caller");
            let role = self.caller_role(self_pane_id, env)?;
            return Err(refuse(format!(
                "{action} requires a superuser (the owner role); {who} is {role}. Non-owners use the inbox and task queue."
            )));
        }
        Ok(())
    }

    /// `whoami`.
    pub fn whoami(
        &self,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        let role = self.caller_role(self_pane_id, env)?;
        let entry = match non_empty(self_pane_id) {
            Some(pane) => self.with_roles(|roles, _| roles.entry(pane))?,
            None => None,
        };
        let caller = self.current_caller().unwrap_or(Value::Null);
        let mut shown = Map::new();
        for key in ["pid", "start", "name", "source"] {
            shown.insert(key.into(), caller.get(key).cloned().unwrap_or(Value::Null));
        }
        let (via_token, bound, binding_note) = self.with_caller(|state| {
            (
                state.service_token.is_some(),
                state.identity_bound,
                state.identity_note.clone(),
            )
        });
        Ok(json!({
            "pane_id": self_pane_id, "role": role,
            "registered": entry,
            "via_token": via_token,
            "caller": shown,
            "bound": bound,
            "binding_note": binding_note,
        }))
    }

    /// `lane_register`: bind a role to a pane's root process. Owner/system only, except the
    /// bootstrap: with no owner registered, a bound caller may register ITSELF as owner.
    #[allow(clippy::too_many_arguments)]
    pub fn lane_register(
        &self,
        pane: Option<&str>,
        role: Option<&str>,
        provider: Option<&str>,
        model: Option<&str>,
        label: Option<&str>,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        let role = role.unwrap_or("").trim().to_lowercase();
        let mut target = non_empty(pane.map(str::trim))
            .or(non_empty(self_pane_id))
            .map(String::from);
        if target.is_none() {
            return Err(refuse(
                "lane register needs a pane (or a caller pane for 'self')",
            ));
        }
        if target.as_deref() == Some("self") {
            target = non_empty(self_pane_id).map(String::from);
        }
        let mine = self.caller_role(self_pane_id, env)?;
        if mine == "system" && role == "owner" {
            return Err(refuse(
                "the system token cannot create owners; only the owner's pane does",
            ));
        }
        if mine != "owner" && mine != "system" {
            let has_owner = self.with_roles(|roles, _| roles.has_owner())?;
            let bootstrap =
                role == "owner" && !has_owner && target.as_deref() == non_empty(self_pane_id);
            if !bootstrap {
                return Err(refuse(format!(
                    "lane register requires the owner role (caller is {mine}); the one exception is registering yourself as the first owner"
                )));
            }
        }
        let Some(target) = target else {
            return Err(refuse(
                "lane register needs a pane (or a caller pane for 'self')",
            ));
        };
        let info = Flock::pane_info(env, &target).unwrap_or(Value::Null);
        let pane_pid = info.get("pane_pid").and_then(|p| {
            p.as_i64()
                .or_else(|| p.as_str().and_then(|s| s.parse().ok()))
        });
        let start = pane_pid.and_then(crate::proc::process_start);
        let label_value = non_empty(label)
            .map(String::from)
            .or_else(|| {
                info.get("label")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            })
            .or_else(|| {
                info.get("tab_label")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
            });
        let by = non_empty(self_pane_id).unwrap_or("token");
        let entry = self.with_roles(|roles, _| {
            roles.register(
                &target,
                &role,
                pane_pid,
                start.as_deref(),
                provider,
                model,
                Some(by),
                label_value.as_deref(),
            )
        })?;
        self.forget_role_pane(&target);
        self.with_caller(|state| state.role = None);
        self.set_last_message(format!("Registered {target} as {role}"));
        Ok(Value::Object(entry))
    }

    /// `lane_unregister`.
    pub fn lane_unregister(
        &self,
        pane: &str,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        self.require_superuser(self_pane_id, "lane unregister", env)?;
        let removed = self.with_roles(|roles, _| roles.unregister(pane))?;
        self.forget_role_pane(pane);
        Ok(json!({ "pane_id": pane, "removed": removed }))
    }

    /// `token_issue`: mint a service token for a pane-less process (web desk, timers).
    pub fn token_issue(
        &self,
        role: &str,
        label: Option<&str>,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        if self.with_roles(|roles, _| roles.has_owner())? {
            self.require_superuser(self_pane_id, "token issue", env)?;
        } else if role != "owner" {
            return Err(refuse(
                "no owner is registered yet; the first token must be an owner token (bootstrap), issued from the owner's terminal",
            ));
        }
        let token = self.with_roles(|roles, _| roles.issue_token(role, label, self_pane_id))?;
        Ok(json!({
            "token": token, "role": role, "label": non_empty(label).unwrap_or(role),
            "note": "shown once; store it in the service's 0600 token file",
        }))
    }

    /// `token_revoke`.
    pub fn token_revoke(
        &self,
        label: &str,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        self.require_superuser(self_pane_id, "token revoke", env)?;
        let revoked = self.with_roles(|roles, _| roles.revoke_token(label))?;
        Ok(json!({ "label": label, "revoked": revoked }))
    }

    fn superuser_status_fields(
        &self,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Map<String, Value>, DunkError> {
        let mut out = Map::new();
        out.insert(
            "superuser".into(),
            json!(self.is_superuser(self_pane_id, env)?),
        );
        out.insert("pane_id".into(), json!(self_pane_id));
        out.insert("role".into(), json!(self.caller_role(self_pane_id, env)?));
        if let Value::Object(described) = self.with_roles(|roles, _| roles.describe())? {
            out.extend(described);
        }
        Ok(out)
    }

    /// `superuser_status`.
    pub fn superuser_status(
        &self,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        Ok(Value::Object(
            self.superuser_status_fields(self_pane_id, env)?,
        ))
    }

    /// `superuser_add`: the legacy spelling of `lane register --role owner <pane>`; names refused.
    pub fn superuser_add(
        &self,
        entry: &str,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        let entry = entry.trim();
        if !entry.starts_with('%') && !entry.contains(':') {
            return Err(refuse(
                "superuser add takes a pane id; names are no longer matched (a label is one `tmux select-pane -T` away for any lane)",
            ));
        }
        let result = self.lane_register(
            Some(entry),
            Some("owner"),
            None,
            None,
            None,
            self_pane_id,
            env,
        )?;
        let mut out = Map::new();
        out.insert("added".into(), json!(entry));
        out.extend(self.superuser_status_fields(self_pane_id, env)?);
        out.insert("entry".into(), result);
        Ok(Value::Object(out))
    }

    /// `superuser_remove`.
    pub fn superuser_remove(
        &self,
        entry: &str,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        self.require_superuser(self_pane_id, "superuser remove", env)?;
        let owners = self.with_roles(|roles, _| roles.owners())?;
        if owners.len() <= 1 && owners.iter().any(|o| o == entry) {
            return Err(refuse("refusing to remove the last owner"));
        }
        let removed = self.with_roles(|roles, _| roles.unregister(entry))?;
        let mut out = Map::new();
        out.insert("removed".into(), json!(removed.is_some()));
        out.insert("entry".into(), json!(entry));
        out.extend(self.superuser_status_fields(self_pane_id, env)?);
        Ok(Value::Object(out))
    }

    /// `superuser_recover`: bootstrap - register the caller's pane as owner when none exists.
    pub fn superuser_recover(
        &self,
        self_pane_id: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        let Some(pane) = non_empty(self_pane_id) else {
            return Err(refuse("superuser recover requires a known caller pane"));
        };
        let result = self.lane_register(
            Some(pane),
            Some("owner"),
            None,
            None,
            None,
            self_pane_id,
            env,
        )?;
        let mut out = Map::new();
        out.insert("recovered".into(), json!(true));
        out.insert("pane_id".into(), json!(pane));
        out.insert("entry".into(), result);
        out.extend(self.superuser_status_fields(self_pane_id, env)?);
        Ok(Value::Object(out))
    }
}
