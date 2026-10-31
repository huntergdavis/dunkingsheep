//! The `Roles` class from `dunk_roles.py` (T31a): the lane registry plus the service-token
//! table. Pure file store (`roles.json`, `service-tokens.json`) over `atomic_json`; no
//! `Flock`. Both files are re-read when they change, so a registration or a revocation
//! takes effect on the next request without a daemon restart.
//!
//! NOT PORTED here: `Flock.caller_role` (the live pane record, the t1248-style grace
//! window) lives in T31 (`flock/roles_gate.rs`); `role_of` takes the pane record as an
//! argument exactly like the Python.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::atomic_json;
use crate::config::{self, Env};
use crate::error::DunkError;

pub const ROLES: [&str; 8] = [
    "owner",
    "system",
    "web",
    "desk",
    "planner",
    "worker",
    "reviewer",
    "anonymous",
];
pub const LANE_ROLES: [&str; 5] = ["desk", "planner", "worker", "reviewer", "owner"];
pub const TOKEN_ROLES: [&str; 3] = ["web", "system", "owner"];
pub const DEFAULT_ROLE: &str = "anonymous";
/// With no registry file at all (a fresh install, the test suite) a bound pane is a
/// worker; the first registration writes the file with default "anonymous" (FLAT_FLEET 4.2).
pub const UNCONFIGURED_DEFAULT: &str = "worker";
pub const DEFAULT_REVIEWER_PROVIDERS: [&str; 2] = ["claude", "codex"];

/// A named refusal (`RoleError`).
fn role_error(text: impl Into<String>) -> DunkError {
    DunkError::Refused(text.into())
}

/// `_hash`: sha256 hex of the token.
pub fn hash_token(token: &str) -> String {
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `secrets.token_urlsafe(32)`: 32 random bytes, base64url without padding. Read from the
/// kernel's CSPRNG; there is no `rand` crate in the tree and the port may add none.
fn token_urlsafe() -> Result<String, DunkError> {
    let mut bytes = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(43);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        let symbols = chunk.len() + 1;
        for i in 0..symbols {
            out.push(ALPHABET[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    Ok(out)
}

/// `_read_json`: a missing or unreadable file is `{}`; invalid JSON is a named refusal; a
/// non-object is `{}`.
fn read_json(path: &Path) -> Result<Map<String, Value>, DunkError> {
    // Python maps only OSError to `{}`; bytes that are not UTF-8 raise UnicodeDecodeError (a
    // ValueError), which is the "not valid JSON" refusal - NOT an empty registry, because the
    // next register() would overwrite the corrupt file and destroy the registrations.
    let invalid = || {
        role_error(format!(
            "{} is not valid JSON; fix it or remove it",
            path.display()
        ))
    };
    let Ok(bytes) = std::fs::read(path) else {
        return Ok(Map::new());
    };
    let text = String::from_utf8(bytes).map_err(|_| invalid())?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Ok(Map::new()),
        Err(_) => Err(invalid()),
    }
}

/// `_stamp`: (mtime ns, size), or None when the file is not there.
fn stamp(path: &Path) -> Option<(u128, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let ns = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_nanos();
    Some((ns, meta.len()))
}

fn now_rounded() -> f64 {
    (crate::clock::now_s() * 1000.0).round() / 1000.0
}

fn str_in(value: Option<&Value>, set: &[&str]) -> bool {
    value
        .and_then(Value::as_str)
        .is_some_and(|s| set.contains(&s))
}

/// The lane registry plus the service-token table.
#[derive(Debug)]
pub struct Roles {
    pub path: PathBuf,
    pub tokens_path: PathBuf,
    default_override: Option<String>,
    pub lanes: BTreeMap<String, Map<String, Value>>,
    pub reviewer_providers: Vec<String>,
    pub default: String,
    tokens: BTreeMap<String, Map<String, Value>>,
    stamp: Option<(u128, u64)>,
    tokens_stamp: Option<(u128, u64)>,
}

impl Roles {
    /// `Roles.__init__`. Paths fall back to `DUNKINGSHEEP_ROLES_FILE` / `_TOKENS_FILE`, then
    /// the config directory.
    pub fn new(
        env: &Env,
        path: Option<PathBuf>,
        tokens_path: Option<PathBuf>,
        default: Option<&str>,
    ) -> Result<Roles, DunkError> {
        let path = match path.or_else(|| env.get("DUNKINGSHEEP_ROLES_FILE").map(PathBuf::from)) {
            Some(path) => path,
            None => config::config_dir(env)?.join("roles.json"),
        };
        let tokens_path =
            match tokens_path.or_else(|| env.get("DUNKINGSHEEP_TOKENS_FILE").map(PathBuf::from)) {
                Some(path) => path,
                None => config::config_dir(env)?.join("service-tokens.json"),
            };
        let mut roles = Roles {
            path,
            tokens_path,
            default_override: default.filter(|d| !d.is_empty()).map(String::from),
            lanes: BTreeMap::new(),
            reviewer_providers: DEFAULT_REVIEWER_PROVIDERS
                .iter()
                .map(|s| s.to_string())
                .collect(),
            default: default
                .filter(|d| !d.is_empty())
                .unwrap_or(DEFAULT_ROLE)
                .to_string(),
            tokens: BTreeMap::new(),
            stamp: None,
            tokens_stamp: None,
        };
        roles.refresh(true)?;
        Ok(roles)
    }

    // -- persistence ----------------------------------------------------------

    /// `refresh`: re-read a file when its stamp changed (or when forced).
    pub fn refresh(&mut self, force: bool) -> Result<(), DunkError> {
        let current = stamp(&self.path);
        if force || current != self.stamp {
            let data = read_json(&self.path)?;
            self.lanes = data
                .get("lanes")
                .and_then(Value::as_object)
                .map(|lanes| {
                    lanes
                        .iter()
                        .filter_map(|(k, v)| {
                            let entry = v.as_object()?;
                            str_in(entry.get("role"), &ROLES).then(|| (k.clone(), entry.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.reviewer_providers = match data.get("reviewer_providers") {
                Some(Value::Array(items)) => items
                    .iter()
                    .map(|v| {
                        v.as_str()
                            .map(String::from)
                            .unwrap_or_else(|| v.to_string())
                    })
                    .collect(),
                _ => DEFAULT_REVIEWER_PROVIDERS
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
            };
            let configured = data
                .get("default")
                .and_then(Value::as_str)
                .filter(|d| ROLES.contains(d));
            self.default = if let Some(forced) = &self.default_override {
                forced.clone()
            } else if let Some(configured) = configured {
                configured.to_string()
            } else if data.is_empty() {
                UNCONFIGURED_DEFAULT.to_string()
            } else {
                DEFAULT_ROLE.to_string()
            };
            self.stamp = current;
        }
        let tstamp = stamp(&self.tokens_path);
        if force || tstamp != self.tokens_stamp {
            let data = read_json(&self.tokens_path)?;
            self.tokens = data
                .get("tokens")
                .and_then(Value::as_object)
                .map(|tokens| {
                    tokens
                        .iter()
                        .filter_map(|(k, v)| {
                            let entry = v.as_object()?;
                            str_in(entry.get("role"), &TOKEN_ROLES)
                                .then(|| (k.clone(), entry.clone()))
                        })
                        .collect()
                })
                .unwrap_or_default();
            self.tokens_stamp = tstamp;
        }
        Ok(())
    }

    /// `save`.
    pub fn save(&mut self) -> Result<(), DunkError> {
        let payload = json!({
            "lanes": self.lanes,
            "reviewer_providers": self.reviewer_providers,
            "default": self.default,
            "updated_at": now_rounded(),
        });
        atomic_json::write(&self.path, &payload)?;
        self.stamp = stamp(&self.path);
        Ok(())
    }

    fn save_tokens(&mut self) -> Result<(), DunkError> {
        atomic_json::write(
            &self.tokens_path,
            &json!({ "tokens": self.tokens, "updated_at": now_rounded() }),
        )?;
        self.tokens_stamp = stamp(&self.tokens_path);
        Ok(())
    }

    // -- registry -------------------------------------------------------------

    /// `register`: bind a pane id to a role. The first registration ends bootstrap.
    #[allow(clippy::too_many_arguments)]
    pub fn register(
        &mut self,
        pane_id: &str,
        role: &str,
        pane_pid: Option<i64>,
        start: Option<&str>,
        provider: Option<&str>,
        model: Option<&str>,
        by: Option<&str>,
        label: Option<&str>,
    ) -> Result<Map<String, Value>, DunkError> {
        let pane_id = pane_id.trim();
        if pane_id.is_empty() {
            return Err(role_error("a registration needs a pane id"));
        }
        if !LANE_ROLES.contains(&role) {
            return Err(role_error(format!(
                "role must be one of {}; got {role:?}",
                LANE_ROLES.join(", ")
            )));
        }
        self.refresh(false)?;
        if self.default_override.is_none() && stamp(&self.path).is_none() {
            self.default = DEFAULT_ROLE.to_string(); // the first registration ends bootstrap
        }
        let mut entry = Map::new();
        entry.insert("role".into(), json!(role));
        entry.insert("registered_at".into(), json!(now_rounded()));
        for (key, value) in [
            ("start", start),
            ("provider", provider),
            ("model", model),
            ("by", by),
            ("label", label),
        ] {
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                entry.insert(key.into(), json!(value));
            }
        }
        if let Some(pid) = pane_pid {
            entry.insert("pane_pid".into(), json!(pid));
        }
        self.lanes.insert(pane_id.to_string(), entry.clone());
        self.save()?;
        entry.insert("pane_id".into(), json!(pane_id));
        Ok(entry)
    }

    /// `unregister`: the removed entry, if there was one.
    pub fn unregister(&mut self, pane_id: &str) -> Result<Option<Map<String, Value>>, DunkError> {
        self.refresh(false)?;
        let removed = self.lanes.remove(pane_id);
        if removed.is_some() {
            self.save()?;
        }
        Ok(removed)
    }

    pub fn entry(&mut self, pane_id: &str) -> Result<Option<Map<String, Value>>, DunkError> {
        self.refresh(false)?;
        Ok(self.lanes.get(pane_id).cloned())
    }

    pub fn owners(&mut self) -> Result<Vec<String>, DunkError> {
        self.refresh(false)?;
        Ok(self
            .lanes
            .iter()
            .filter(|(_, e)| e.get("role").and_then(Value::as_str) == Some("owner"))
            .map(|(p, _)| p.clone())
            .collect())
    }

    pub fn has_owner(&mut self) -> Result<bool, DunkError> {
        Ok(!self.owners()?.is_empty()
            || self
                .tokens
                .values()
                .any(|t| t.get("role").and_then(Value::as_str) == Some("owner")))
    }

    /// `role_of`: the registered role for a pane, or the default. When the registration
    /// recorded a pane pid and the live record carries one, they must match: a reused `%N`
    /// after a tmux restart is a different pane and gets the default role until
    /// re-registered. A registration that recorded a pid is unverifiable without a live pid.
    pub fn role_of(
        &mut self,
        pane_id: Option<&str>,
        pane_info: Option<&Value>,
    ) -> Result<String, DunkError> {
        self.refresh(false)?;
        let Some(pane_id) = pane_id.filter(|p| !p.is_empty()) else {
            return Ok(DEFAULT_ROLE.to_string());
        };
        let Some(entry) = self.lanes.get(pane_id) else {
            return Ok(self.default.clone());
        };
        if let Some(want) = entry.get("pane_pid").filter(|v| !v.is_null()) {
            let live = pane_info.and_then(|info| info.get("pane_pid"));
            let live_pid = match live {
                None | Some(Value::Null) => return Ok(self.default.clone()),
                Some(Value::String(s)) if s.is_empty() || s == "None" => {
                    return Ok(self.default.clone())
                }
                Some(v) => v,
            };
            let as_int = |v: &Value| -> Option<i64> {
                v.as_i64()
                    .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            };
            match (as_int(live_pid), as_int(want)) {
                (Some(live), Some(want)) if live == want => {}
                _ => return Ok(self.default.clone()),
            }
        }
        Ok(entry
            .get("role")
            .and_then(Value::as_str)
            .unwrap_or(DEFAULT_ROLE)
            .to_string())
    }

    pub fn provider_of(&mut self, pane_id: &str) -> Result<Option<String>, DunkError> {
        let entry = self.entry(pane_id)?.unwrap_or_default();
        let provider = entry
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_lowercase();
        Ok((!provider.is_empty()).then_some(provider))
    }

    /// `may_review`: a reviewer role AND a provider on the reviewer list (free models never
    /// review). `role` is the caller's already-resolved role; resolving here without one
    /// would demote a registered pane to the default (2026-10-29).
    pub fn may_review(&mut self, pane_id: &str, role: Option<&str>) -> Result<bool, DunkError> {
        let resolved = match role.filter(|r| !r.is_empty()) {
            Some(role) => role.to_string(),
            None => self.role_of(Some(pane_id), None)?,
        };
        if resolved != "reviewer" {
            return Ok(false);
        }
        let allowed: Vec<String> = self
            .reviewer_providers
            .iter()
            .map(|p| p.to_lowercase())
            .collect();
        Ok(self
            .provider_of(pane_id)?
            .is_some_and(|p| allowed.contains(&p)))
    }

    // -- service tokens -------------------------------------------------------

    /// `issue_token`: the plaintext token, returned once; only its hash is stored.
    pub fn issue_token(
        &mut self,
        role: &str,
        label: Option<&str>,
        by: Option<&str>,
    ) -> Result<String, DunkError> {
        if !TOKEN_ROLES.contains(&role) {
            return Err(role_error(format!(
                "a token role must be one of {}",
                TOKEN_ROLES.join(", ")
            )));
        }
        self.refresh(false)?;
        let token = token_urlsafe()?;
        let mut entry = Map::new();
        entry.insert("role".into(), json!(role));
        entry.insert(
            "label".into(),
            json!(label.filter(|l| !l.is_empty()).unwrap_or(role)),
        );
        entry.insert("issued_at".into(), json!(now_rounded()));
        entry.insert("by".into(), json!(by.unwrap_or("")));
        self.tokens.insert(hash_token(&token), entry);
        self.save_tokens()?;
        Ok(token)
    }

    /// `revoke_token`: how many tokens carried that label.
    pub fn revoke_token(&mut self, label: &str) -> Result<usize, DunkError> {
        self.refresh(false)?;
        let before = self.tokens.len();
        self.tokens
            .retain(|_, t| t.get("label").and_then(Value::as_str) != Some(label));
        if self.tokens.len() != before {
            self.save_tokens()?;
        }
        Ok(before - self.tokens.len())
    }

    pub fn token_role(&mut self, token: &str) -> Result<Option<String>, DunkError> {
        if token.is_empty() {
            return Ok(None);
        }
        self.refresh(false)?;
        Ok(self
            .tokens
            .get(&hash_token(token))
            .and_then(|e| e.get("role"))
            .and_then(Value::as_str)
            .map(String::from))
    }

    /// `tokens`: label, role and issue time for each token (never the hash).
    pub fn tokens(&mut self) -> Result<Vec<Value>, DunkError> {
        self.refresh(false)?;
        Ok(self
            .tokens
            .values()
            .map(|t| json!({ "label": t.get("label"), "role": t.get("role"), "issued_at": t.get("issued_at") }))
            .collect())
    }

    pub fn describe(&mut self) -> Result<Value, DunkError> {
        self.refresh(false)?;
        let tokens = self.tokens()?;
        Ok(json!({
            "path": self.path.display().to_string(),
            "tokens_path": self.tokens_path.display().to_string(),
            "lanes": self.lanes,
            "reviewer_providers": self.reviewer_providers,
            "default": self.default,
            "tokens": tokens,
        }))
    }
}
