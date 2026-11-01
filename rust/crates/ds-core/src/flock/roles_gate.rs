//! `flock/roles_gate.rs` (T31): who the caller is and what that allows, ported from
//! `dunk_core.Flock`: `caller_role`, `set_service_token`, `is_superuser`, `require_superuser`,
//! `whoami`, `lane_register` / `lane_unregister`, `token_issue` / `token_revoke` and the legacy
//! `superuser_*` spellings. The registry itself is `roles::Roles` (T31a); the legacy allowlist
//! is `superusers::Superusers`; the pane record comes through `PaneInfoEnv`.
//!
//! The stores are attached after construction (`attach_roles`), like the tasks/inbox stores:
//! a gate call on a flock with no registry attached is a named refusal, never a guess.
//!
//! NOT ported here: `self.last_message = "Registered ..."` (the Rust `Flock` has no
//! `last_message` yet), and the dispatch-time use of these gates (`api.rs`, T39).

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

fn refuse(text: impl Into<String>) -> DunkError {
    DunkError::Refused(text.into())
}

fn non_empty(text: Option<&str>) -> Option<&str> {
    text.filter(|t| !t.is_empty())
}

impl Flock {
    /// Attach the registry (and the optional legacy allowlist).
    pub fn attach_roles(&self, roles: Roles, legacy: Option<Superusers>) {
        *self.role_stores.lock().expect("role stores lock") = RoleStores {
            roles: Some(roles),
            legacy,
        };
    }

    fn with_roles<T>(
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
        let mut state = self.caller.lock().expect("caller lock");
        state.service_token = non_empty(token).map(String::from);
        state.role = None;
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
        {
            let state = self.caller.lock().expect("caller lock");
            let in_request = state.socket_peer_ok.is_some();
            if in_request && state.role_pane.as_deref() == self_pane_id {
                if let Some(cached) = &state.role {
                    return Ok(cached.clone());
                }
            }
        }
        let token = self
            .caller
            .lock()
            .expect("caller lock")
            .service_token
            .clone();
        let mut role: Option<String> = None;
        if let Some(token) = non_empty(token.as_deref()) {
            role = self.with_roles(|roles, _| roles.token_role(token))?;
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
        let mut state = self.caller.lock().expect("caller lock");
        state.role = Some(role.clone());
        state.role_pane = self_pane_id.map(String::from);
        Ok(role)
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
        let state = self.caller.lock().expect("caller lock");
        Ok(json!({
            "pane_id": self_pane_id, "role": role,
            "registered": entry,
            "via_token": state.service_token.is_some(),
            "caller": shown,
            "bound": state.identity_bound,
            "binding_note": state.identity_note,
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
        self.caller.lock().expect("caller lock").role = None;
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
