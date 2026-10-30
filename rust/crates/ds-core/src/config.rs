//! Paths and environment: one place for every `DUNKINGSHEEP_*` switch.
//!
//! Ported from `dunk_core.CONFIG_DIR`, `dunk_server.SOCKET_PATH/LOG_FILE` and the
//! `os.environ.get("DUNKINGSHEEP_...")` calls scattered through the Python tree.
//! The Python builds these from `DUNKINGSHEEP_DIR` (the CONFIG directory, not the
//! install: dunk_inbox/dunk_tasks/dunk_roles/dunk_server all derive from it).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::DunkError;

/// Every `DUNKINGSHEEP_*` name the Python tree reads, sorted.
///
/// Generated from `grep -rho 'DUNKINGSHEEP_[A-Z_]*' --include=*.py --include=dunkingsheep`
/// and kept in step by `tools/check_env_table.py` (T02 acceptance: the table in
/// `config.rs` matches that grep). `DUNKINGSHEEP__` is deliberately absent: grep
/// produces it from a `f"DUNKINGSHEEP_{suffix}"` format string, which is a prefix,
/// not a variable - a real variable has letters after the second underscore.
pub const ENV_VARS: &[&str] = &[
    "DUNKINGSHEEP_AGENT_VERIFY_S",
    "DUNKINGSHEEP_ALLOW_EMPTY_BACKEND",
    "DUNKINGSHEEP_API_HOST",
    "DUNKINGSHEEP_API_PORT",
    "DUNKINGSHEEP_API_VERBOSE",
    "DUNKINGSHEEP_APP_DB",
    "DUNKINGSHEEP_BATCH_TIMEOUT_S",
    "DUNKINGSHEEP_BIN",
    "DUNKINGSHEEP_BIND_IDENTITY",
    "DUNKINGSHEEP_CARRIED_FAILURE_MAX_S",
    "DUNKINGSHEEP_CLAUDE_BIN",
    "DUNKINGSHEEP_CLAUDE_BUSY_TTL_S",
    "DUNKINGSHEEP_CLAUDE_END_WAIT_S",
    "DUNKINGSHEEP_CLAUDE_STATE_DIR",
    "DUNKINGSHEEP_CLAUDE_STATUS",
    "DUNKINGSHEEP_CLAUDE_STATUS_TTL_S",
    "DUNKINGSHEEP_CLAUDE_TURN_TTL_S",
    "DUNKINGSHEEP_CLI",
    "DUNKINGSHEEP_CLIENT_TIMEOUT",
    "DUNKINGSHEEP_CODEX_END_WAIT_S",
    "DUNKINGSHEEP_CODEX_STATE_DIR",
    "DUNKINGSHEEP_CODEX_TURN_TTL_S",
    "DUNKINGSHEEP_CONTEXT_LEDGER",
    "DUNKINGSHEEP_DESK_FILE",
    "DUNKINGSHEEP_DIE_WITH_PARENT",
    "DUNKINGSHEEP_DIR",
    "DUNKINGSHEEP_DISPATCH_LANES",
    "DUNKINGSHEEP_FEDERATED_INPUT",
    "DUNKINGSHEEP_FRESH",
    "DUNKINGSHEEP_GITHUB_API",
    "DUNKINGSHEEP_GITHUB_TOKENS_FILE",
    "DUNKINGSHEEP_GUARD",
    "DUNKINGSHEEP_HERDR_BIN",
    "DUNKINGSHEEP_IDLE_CLAIM_S",
    "DUNKINGSHEEP_LEADER_PANE",
    "DUNKINGSHEEP_LEASE_S",
    "DUNKINGSHEEP_LOAD_STATE",
    "DUNKINGSHEEP_MAILBOX",
    "DUNKINGSHEEP_MAILBOX_CACHE_S",
    "DUNKINGSHEEP_MEMORY_THROTTLE_S",
    "DUNKINGSHEEP_MIN_POLL_S",
    "DUNKINGSHEEP_NICE",
    "DUNKINGSHEEP_NO_START_DAEMON",
    "DUNKINGSHEEP_OWNER_NAME",
    "DUNKINGSHEEP_OWNER_PANE",
    "DUNKINGSHEEP_PM_ROUTING",
    "DUNKINGSHEEP_POOLRAD_REPO",
    "DUNKINGSHEEP_PUSH_S",
    "DUNKINGSHEEP_REAP_S",
    "DUNKINGSHEEP_RECOVERY_BINDINGS",
    "DUNKINGSHEEP_REPO",
    "DUNKINGSHEEP_ROLES_FILE",
    "DUNKINGSHEEP_SANDBOX_WORKSPACE",
    "DUNKINGSHEEP_SELF",
    "DUNKINGSHEEP_SEND_TIMEOUT_S",
    "DUNKINGSHEEP_SERVICE_TOKEN",
    "DUNKINGSHEEP_SERVICE_TOKEN_FILE",
    "DUNKINGSHEEP_SOCKET",
    "DUNKINGSHEEP_STUCK_CONFIG",
    "DUNKINGSHEEP_SUPERUSER",
    "DUNKINGSHEEP_SUPERVISED",
    "DUNKINGSHEEP_TERMINAL_BACKEND",
    "DUNKINGSHEEP_TESTS_OWN_ENV",
    "DUNKINGSHEEP_TEST_SPAWN",
    "DUNKINGSHEEP_TMUX_BIN",
    "DUNKINGSHEEP_TMUX_GUARD_DIR",
    "DUNKINGSHEEP_TMUX_REGISTRY",
    "DUNKINGSHEEP_TMUX_SOCKET",
    "DUNKINGSHEEP_TOKENS_FILE",
    "DUNKINGSHEEP_TTY_IDENTITY",
    "DUNKINGSHEEP_TURN_END_DISPATCH",
    "DUNKINGSHEEP_TURN_END_MIN_GAP_S",
    "DUNKINGSHEEP_TURN_END_TASK_COOLDOWN_S",
    "DUNKINGSHEEP_TURN_END_WAIT_S",
    "DUNKINGSHEEP_TURN_TTL_S",
    "DUNKINGSHEEP_TYPING_LOCK_LOUD_S",
    "DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S",
    "DUNKINGSHEEP_USAGE_MAX_AGE_S",
    "DUNKINGSHEEP_VERIFY_SELF",
    "DUNKINGSHEEP_VIEW_CACHE_S",
    "DUNKINGSHEEP_VPCTL",
    "DUNKINGSHEEP_VP_NAME",
    "DUNKINGSHEEP_VP_PANE",
];

/// The environment, so tests can hand in a table instead of the process's.
#[derive(Debug, Clone, Default)]
pub struct Env(BTreeMap<String, String>);

impl Env {
    /// The real process environment.
    pub fn from_process() -> Self {
        Env(std::env::vars().collect())
    }

    /// A fixed table (tests, and the differential harness's recorded requests).
    pub fn from_pairs(pairs: &[(&str, &str)]) -> Self {
        Env(pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect())
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.get(name).map(String::as_str)
    }

    /// An integer switch, defaulting rather than failing - the Python uses
    /// `int(os.environ.get(name, default))` and a bad value must not crash a unit.
    pub fn int(&self, name: &str, default: i64) -> i64 {
        self.get(name)
            .and_then(|v| v.trim().parse::<i64>().ok())
            .unwrap_or(default)
    }

    /// A float switch, same rule.
    pub fn float(&self, name: &str, default: f64) -> f64 {
        self.get(name)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(default)
    }

    /// A boolean switch: the Python treats "1"/"true"/"yes" as true.
    pub fn flag(&self, name: &str, default: bool) -> bool {
        match self.get(name).map(|v| v.trim().to_ascii_lowercase()) {
            Some(value) => matches!(value.as_str(), "1" | "true" | "yes" | "on"),
            None => default,
        }
    }
}

/// The config directory: `DUNKINGSHEEP_DIR`, else `~/.config/dunkingsheep`.
///
/// NOT the install directory. Setting this to a release path was a live defect
/// (the telegram unit did exactly that): every store, the socket and the log are
/// derived from here, so pointing it at the install sends the daemon looking for
/// `inbox.db` under the release tree.
pub fn config_dir(env: &Env) -> Result<PathBuf, DunkError> {
    if let Some(explicit) = env
        .get("DUNKINGSHEEP_DIR")
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return Ok(PathBuf::from(explicit));
    }
    let home = env
        .get("HOME")
        .or_else(|| env.get("USERPROFILE"))
        .ok_or_else(|| {
            DunkError::Refused(
                "neither DUNKINGSHEEP_DIR nor HOME is set; cannot find the config directory".into(),
            )
        })?;
    Ok(Path::new(home).join(".config").join("dunkingsheep"))
}

/// `dunkingsheep.sock` under the config directory (mode 0600 at creation).
pub fn socket_path(env: &Env) -> Result<PathBuf, DunkError> {
    Ok(config_dir(env)?.join("dunkingsheep.sock"))
}

/// `daemon.log` under the config directory.
pub fn log_path(env: &Env) -> Result<PathBuf, DunkError> {
    Ok(config_dir(env)?.join("daemon.log"))
}

/// One named store file under the config directory (the 33 in the manifest).
pub fn store_path(env: &Env, name: &str) -> Result<PathBuf, DunkError> {
    Ok(config_dir(env)?.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_env_table_is_sorted_and_has_no_prefix_artefact() {
        let mut sorted = ENV_VARS.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, ENV_VARS.to_vec(), "ENV_VARS must stay sorted");
        assert!(
            !ENV_VARS.contains(&"DUNKINGSHEEP__"),
            "the bare prefix is a grep artefact, not a variable"
        );
        for name in ENV_VARS {
            assert!(name.starts_with("DUNKINGSHEEP_"), "{name}");
            assert!(
                name.trim_start_matches("DUNKINGSHEEP_")
                    .chars()
                    .any(|c| c.is_ascii_uppercase()),
                "{name} has no name after the prefix"
            );
        }
    }

    #[test]
    fn config_dir_prefers_the_variable_over_home() {
        let env = Env::from_pairs(&[("DUNKINGSHEEP_DIR", "/tmp/ds"), ("HOME", "/home/x")]);
        assert_eq!(PathBuf::from("/tmp/ds"), config_dir(&env).unwrap());
    }

    #[test]
    fn config_dir_falls_back_to_home_dot_config() {
        let env = Env::from_pairs(&[("HOME", "/home/x")]);
        assert_eq!(
            PathBuf::from("/home/x/.config/dunkingsheep"),
            config_dir(&env).unwrap()
        );
        assert_eq!(
            PathBuf::from("/home/x/.config/dunkingsheep/dunkingsheep.sock"),
            socket_path(&env).unwrap()
        );
        assert_eq!(
            PathBuf::from("/home/x/.config/dunkingsheep/daemon.log"),
            log_path(&env).unwrap()
        );
        assert_eq!(
            PathBuf::from("/home/x/.config/dunkingsheep/tasks.db"),
            store_path(&env, "tasks.db").unwrap()
        );
    }

    #[test]
    fn an_empty_variable_is_not_a_directory() {
        let env = Env::from_pairs(&[("DUNKINGSHEEP_DIR", "  "), ("HOME", "/home/x")]);
        assert_eq!(
            PathBuf::from("/home/x/.config/dunkingsheep"),
            config_dir(&env).unwrap()
        );
    }

    #[test]
    fn no_home_and_no_variable_is_a_named_refusal() {
        let error = config_dir(&Env::from_pairs(&[])).unwrap_err();
        assert!(error
            .to_string()
            .contains("cannot find the config directory"));
        assert_eq!(2, error.exit_code());
    }

    #[test]
    fn switches_parse_like_the_python() {
        let env = Env::from_pairs(&[
            ("DUNKINGSHEEP_LEASE_S", "900"),
            ("DUNKINGSHEEP_GITHUB_API", "nope"),
            ("DUNKINGSHEEP_NO_START_DAEMON", "yes"),
        ]);
        assert_eq!(900, env.int("DUNKINGSHEEP_LEASE_S", 1800));
        assert_eq!(12, env.int("DUNKINGSHEEP_MISSING", 12));
        assert_eq!(1.5, env.float("DUNKINGSHEEP_MISSING", 1.5));
        assert!(env.flag("DUNKINGSHEEP_NO_START_DAEMON", false));
        assert!(!env.flag("DUNKINGSHEEP_MISSING", false));
    }
}
