//! `flock/mailbox.rs` (T30, slice 3): `resolve_mailbox` and its helpers. Ports
//! `Flock.resolve_mailbox`, `_is_agent_type`, `_pane_refusal`, `_mailbox_for_pane`,
//! `_lane_pane`, `_registered_lane`, and `OWNER_MAILBOX`.
//!
//! The Python reaches the backend (`self.herdr`) and the role registry (`self.roles`);
//! here both sit behind [`MailboxEnv`], so the resolution rules are testable without
//! either, and the daemon wiring supplies the real one. Panes are JSON objects in the
//! shape `list_panes_grouped` returns (see `resolve.rs`).

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

use super::resolve::{self, describe};
use super::Flock;
use crate::error::DunkError;

/// `Flock.OWNER_MAILBOX`.
pub const OWNER_MAILBOX: &str = "owner";

/// `(mailbox key, label, pane id)`.
pub type Mailbox = (String, String, Option<String>);

/// What `resolve_mailbox` needs from outside the lane registry.
pub trait MailboxEnv {
    /// `herdr.list_panes()`: ONE backend call, no tab/workspace labels.
    fn list_panes(&self) -> Vec<Value>;
    /// `herdr.list_panes_grouped()`: annotated with `tab_label`/`workspace_label`.
    fn list_panes_grouped(&self) -> Vec<Value>;
    /// Fallible grouped listing for callers that must distinguish an empty terminal from a
    /// backend failure. Existing implementations keep their legacy behavior by default.
    fn try_list_panes_grouped(&self) -> Result<Vec<Value>, DunkError> {
        Ok(self.list_panes_grouped())
    }
    /// `_registered_pane`: `(registered, process_alive, pid)` from the role registry; no
    /// backend call.
    fn registered_pane(&self, pane_id: &str) -> (bool, bool, Option<String>);
}

fn re(cell: &'static OnceLock<Regex>, pattern: &str) -> &'static Regex {
    cell.get_or_init(|| Regex::new(pattern).expect("static regex"))
}

/// `_PANE_ID_RE`, `_TERM_ID_RE`, `_HERDR_ID_RE`: the id fast path.
pub(super) fn is_id(spec: &str) -> bool {
    static PANE: OnceLock<Regex> = OnceLock::new();
    static TERM: OnceLock<Regex> = OnceLock::new();
    static HERDR: OnceLock<Regex> = OnceLock::new();
    re(&PANE, r"^(?:[A-Za-z0-9_.-]+:p\d+|%\d+)$").is_match(spec)
        || re(&TERM, r"^term_[0-9A-Za-z]+$").is_match(spec)
        || re(&HERDR, r"^w\d+:p[A-Za-z0-9_.-]+$").is_match(spec)
}

/// `_EMBEDDED_PANE_RE`: the pane id inside `name (wR:p5)`.
fn embedded_pane(spec: &str) -> Option<String> {
    static EMBEDDED: OnceLock<Regex> = OnceLock::new();
    re(&EMBEDDED, r"\(\s*([A-Za-z0-9_.-]+:p\d+|%\d+)\s*\)\s*$")
        .captures(spec)
        .map(|c| c[1].to_string())
}

fn field(pane: &Value, key: &str) -> String {
    match pane.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None | Some(Value::Bool(false)) => String::new(),
        Some(other) => other.to_string(),
    }
}

fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    format!("{quote}{s}{quote}")
}

fn refused(text: String) -> DunkError {
    DunkError::Refused(text)
}

fn pane_matches_id(pane: &Value, id: &str) -> bool {
    field(pane, "pane_id") == id || field(pane, "terminal_id") == id
}

impl Flock {
    /// `_lane_pane`: `(live pane, recorded mailbox)` for a mailbox in the lane registry.
    pub fn lane_pane(
        &self,
        env: &dyn MailboxEnv,
        mailbox: &str,
    ) -> Result<Option<(Value, String)>, DunkError> {
        let needle = mailbox.trim().to_lowercase();
        if needle.is_empty() {
            return Ok(None);
        }
        let registry = self.lane_registry(false)?;
        let recorded: Vec<(&String, String)> = registry
            .iter()
            .filter_map(|(id, row)| {
                let name = Flock::row_identity(Some(row)).0.unwrap_or_default();
                (name.to_lowercase() == needle).then_some((id, name))
            })
            .collect();
        if recorded.is_empty() {
            return Ok(None);
        }
        let live = env.list_panes_grouped();
        let candidates: Vec<(Value, String)> = recorded
            .into_iter()
            .filter_map(|(id, name)| {
                live.iter()
                    .find(|p| field(p, "pane_id") == *id)
                    .map(|p| (p.clone(), name))
            })
            .collect();
        match candidates.len() {
            0 => Ok(None),
            1 => Ok(candidates.into_iter().next()),
            _ => {
                let ids: Vec<String> = candidates
                    .iter()
                    .map(|(p, _)| field(p, "pane_id"))
                    .collect();
                Err(DunkError::Ambiguous(format!(
                    "lane {} is DECLARED by more than one live pane ({}); delivery would pick one \
                     and split the lane's mail. Reset the duplicate with `lane set` before sending.",
                    py_repr(mailbox),
                    ids.join(", ")
                )))
            }
        }
    }

    /// `_registered_lane`: `(found, alive, dead_since)`.
    pub fn registered_lane(&self, name: &str) -> (bool, bool, Option<String>) {
        let needle = name.trim().to_lowercase();
        if needle.is_empty() {
            return (false, true, None);
        }
        let registry = self.lane_registry(false).unwrap_or_default();
        for value in registry.values() {
            let found = Flock::row_identity(Some(value)).0.unwrap_or_default();
            if found.to_lowercase() != needle {
                continue;
            }
            if let Value::Object(row) = value {
                // Python `bool(value.get("alive", True))`.
                let alive = match row.get("alive") {
                    None => true,
                    Some(Value::Null) | Some(Value::Bool(false)) => false,
                    Some(Value::String(s)) => !s.is_empty(),
                    Some(Value::Number(n)) => n.as_f64() != Some(0.0),
                    Some(_) => true,
                };
                let since = match row.get("dead_since") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(other) => Some(other.to_string()),
                };
                return (true, alive, since);
            }
            return (true, true, None); // a plain name row says nothing about life
        }
        (false, true, None)
    }

    /// `_is_agent_type`: `spec` names a kind of agent, not one of them.
    fn is_agent_type(env: &dyn MailboxEnv, spec: &str) -> bool {
        let low = spec.trim().to_lowercase();
        env.list_panes()
            .iter()
            .any(|p| field(p, "agent").to_lowercase() == low)
    }

    /// `_pane_refusal`: the send-time refusal for a pane id the backend does not list.
    fn pane_refusal(env: &dyn MailboxEnv, pane_id: &str, spec: Option<&str>) -> DunkError {
        let (registered, alive, pid) = env.registered_pane(pane_id);
        let target = match spec {
            Some(spec) => format!("target {} names pane {pane_id}", py_repr(spec)),
            None => format!("pane {}", py_repr(pane_id)),
        };
        if registered && alive {
            return refused(format!(
                "{target}, which is REGISTERED and whose recorded process is ALIVE (pid {}), but \
                 the terminal backend does not list that pane: no message was stored and nothing \
                 was typed. reason=backend_missing_pane. Restore the pane (respawn / lane \
                 register) or address the lane by its mailbox name, which keeps mail for a lane \
                 between panes.",
                pid.as_deref().unwrap_or("None")
            ));
        }
        let when = if registered && pid.is_some() {
            " (its registered root process is no longer running)"
        } else {
            ""
        };
        if spec.is_some() {
            return refused(format!(
                "{target}, which is not live{when}. A decorated address must name a live pane: \
                 use a current pane id, or a bare mailbox name without the parenthetical. \
                 reason=dead_lane."
            ));
        }
        refused(format!(
            "no live pane {}{when}; a dead pane id is not a mailbox, and a pane id is not a \
             name. If that lane still exists, address it by mailbox name; if it does not, the \
             send fails here instead of queueing into nothing. reason=dead_lane.",
            py_repr(pane_id)
        ))
    }

    /// `_mailbox_for_pane`: key = declared identity, else a unique label, else a unique tab
    /// label, else the pane id.
    pub fn mailbox_for_pane(&self, env: &dyn MailboxEnv, pane: &Value) -> Mailbox {
        let panes = env.list_panes_grouped();
        // Re-read the pane from the SAME listing `unique()` searches (t699).
        let wanted = {
            let id = field(pane, "pane_id");
            if id.is_empty() {
                field(pane, "terminal_id")
            } else {
                id
            }
        };
        let pane = panes
            .iter()
            .find(|p| field(p, "pane_id") == wanted || field(p, "terminal_id") == wanted)
            .unwrap_or(pane);
        let pane_id = field(pane, "pane_id");
        let unique = |key: &str| -> Option<String> {
            let value = field(pane, key);
            if value.is_empty() {
                return None;
            }
            (panes.iter().filter(|p| field(p, key) == value).count() == 1).then_some(value)
        };
        let registry = self.lane_registry(false).unwrap_or_default();
        let (declared_name, declared) = Flock::row_identity(registry.get(&pane_id));
        if let (true, Some(name)) = (declared, declared_name) {
            return (name.clone(), format!("{name} ({pane_id})"), Some(pane_id));
        }
        let key = unique("label")
            .or_else(|| unique("tab_label"))
            .unwrap_or_else(|| pane_id.clone());
        let mut label = [field(pane, "label"), field(pane, "tab_label")]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or_else(|| pane_id.clone());
        if label != pane_id {
            label = format!("{label} ({pane_id})");
        }
        (key, label, Some(pane_id))
    }

    /// `resolve_mailbox`: a target to `(mailbox key, label, pane id)`. `strict` is the
    /// SEND-time contract: a live pane or a registered lane, else a named refusal.
    pub fn resolve_mailbox(
        &self,
        env: &dyn MailboxEnv,
        spec: &str,
        self_pane_id: Option<&str>,
        strict: bool,
        server_error_hint: &str,
    ) -> Result<Mailbox, DunkError> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(refused("a target mailbox is required".to_string()));
        }
        if is_id(spec) {
            if let Some(pane) = env.list_panes().iter().find(|p| pane_matches_id(p, spec)) {
                return Ok(self.mailbox_for_pane(env, pane));
            }
            if !strict {
                // READS STAY PERMISSIVE: a dead pane id is a name mail can still be filed under.
                return Ok((spec.to_string(), spec.to_string(), None));
            }
            return Err(Flock::pane_refusal(env, spec, None));
        }
        let panes = env.list_panes_grouped();
        // 1b. A DECORATED address carries its own pane id, and that id decides.
        if let Some(hinted) = embedded_pane(spec) {
            if let Some(pane) = panes.iter().find(|p| pane_matches_id(p, &hinted)) {
                return Ok(self.mailbox_for_pane(env, pane));
            }
            if !strict {
                return Ok((spec.to_string(), spec.to_string(), None));
            }
            return Err(Flock::pane_refusal(env, &hinted, Some(spec)));
        }
        // 1b-bis. RESERVED MAILBOXES: the owner is a person's channel, not a pane.
        if spec.to_lowercase() == OWNER_MAILBOX {
            return Ok((OWNER_MAILBOX.to_string(), "owner".to_string(), None));
        }
        // 1c. The LANE REGISTRY, read backwards.
        if let Some((pane, name)) = self.lane_pane(env, spec)? {
            let id = field(&pane, "pane_id");
            return Ok((name.clone(), format!("{name} ({id})"), Some(id)));
        }
        // 2. An exact NAME must identify exactly one pane across every name field.
        let is_self = matches!(spec.to_lowercase().as_str(), "self" | "me" | "here");
        if !is_self {
            let needle = spec.to_lowercase();
            let exact: Vec<&Value> = panes
                .iter()
                .filter(|p| {
                    ["label", "tab_label", "workspace_label", "agent"]
                        .iter()
                        .any(|k| field(p, k).to_lowercase() == needle)
                })
                .collect();
            if exact.len() > 1 {
                return Err(DunkError::Ambiguous(format!(
                    "target mailbox {} is ambiguous: {}",
                    py_repr(spec),
                    describe(&exact)
                )));
            }
            if let [only] = exact.as_slice() {
                return Ok(self.mailbox_for_pane(env, only));
            }
        }
        match resolve::resolve_target(&panes, spec, self_pane_id, server_error_hint) {
            Ok(pane) => Ok(self.mailbox_for_pane(env, pane)),
            Err(error @ DunkError::Ambiguous(_)) => Err(error),
            Err(error) => {
                if is_self {
                    return Err(error);
                }
                if Flock::is_agent_type(env, spec) {
                    return Err(refused(format!(
                        "{} is an agent type, not one agent: it would be a mailbox several panes \
                         share. Use a pane id, a pane or tab label, or a name of your own.",
                        py_repr(spec)
                    )));
                }
                // FAIL CLOSED (VP ruling 2026-10-24, owner #1556).
                if !strict {
                    return Ok((spec.to_string(), spec.to_string(), None));
                }
                let (registered, alive, dead_since) = self.registered_lane(spec);
                if registered && !alive {
                    let when = dead_since
                        .filter(|s| !s.is_empty())
                        .map(|s| format!(" (dead since {s})"))
                        .unwrap_or_default();
                    return Err(refused(format!(
                        "lane {} is registered as DEAD{when}: nothing can be delivered to it. \
                         reason=dead_lane. Restore the lane first, or send to a lane that is live.",
                        py_repr(spec)
                    )));
                }
                if !registered {
                    return Err(refused(format!(
                        "target {} resolves to no live pane and to no registered lane; refusing \
                         rather than creating a mailbox that would queue in silence. \
                         reason=unknown_name. Use a live pane id, a pane/tab label, or a name \
                         recorded in the lane registry.",
                        py_repr(spec)
                    )));
                }
                Err(refused(format!(
                    "lane {} is registered but has no live pane on this backend; nothing can be \
                     delivered to it right now. reason=unhoused_lane. Existing mail for it can \
                     still be read and archived; start or restore the lane, or send to a lane \
                     that is live.",
                    py_repr(spec)
                )))
            }
        }
    }
}
