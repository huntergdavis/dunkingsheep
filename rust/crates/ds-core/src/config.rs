//! Configuration: the `DUNKINGSHEEP_*` switches and the config directory (T02).
//!
//! `CONFIG_DIR` is the one thing every store agrees on: `dunk_core`, `dunk_inbox`,
//! `dunk_tasks` and `dunk_roles` all build their paths from it, and `dunk_server` derives
//! the socket and the log from it. The Python reads `DUNKINGSHEEP_DIR` for it; that is the
//! variable a unit sets, and it is NOT the install directory (t735: pointing it at a
//! release tree sends the socket, the role registry and inbox.db into the release).

use std::path::{Path, PathBuf};

use crate::error::DunkError;

/// The environment variable that moves the whole config directory.
pub const DIR_ENV: &str = "DUNKINGSHEEP_DIR";
/// The socket override.
pub const SOCKET_ENV: &str = "DUNKINGSHEEP_SOCKET";
/// The service token for a pane-less process (a unit, the web desk).
pub const SERVICE_TOKEN_ENV: &str = "DUNKINGSHEEP_SERVICE_TOKEN";
/// The file the service token is read from (0600).
pub const SERVICE_TOKEN_FILE_ENV: &str = "DUNKINGSHEEP_SERVICE_TOKEN_FILE";

/// Every `DUNKINGSHEEP_*` variable the DAEMON reads, as a constant.
///
/// T02's acceptance is that this table matches `grep -o 'DUNKINGSHEEP_[A-Z_]*'` over the
/// tree. The owner's rule (2026-10-30) is that only variables the daemon reads become
/// public consts; the one test-only variable lives in `test_only` below and the delta is
/// documented there, so the sweep can account for every name either way.
///
/// A name in this list that no module reads is a lie the sweep cannot see, so this block
/// is generated from the tree and a test re-checks it against `os.environ` sites.
pub mod names {
    pub const DUNKINGSHEEP_TTY_IDENTITY: &str = "DUNKINGSHEEP_TTY_IDENTITY";
    pub const DUNKINGSHEEP_MIN_POLL_S: &str = "DUNKINGSHEEP_MIN_POLL_S";
    pub const DUNKINGSHEEP_LEADER_PANE: &str = "DUNKINGSHEEP_LEADER_PANE";
    pub const DUNKINGSHEEP_FRESH: &str = "DUNKINGSHEEP_FRESH";
    pub const DUNKINGSHEEP_AGENT_VERIFY_S: &str = "DUNKINGSHEEP_AGENT_VERIFY_S";
    pub const DUNKINGSHEEP_ALLOW_EMPTY_BACKEND: &str = "DUNKINGSHEEP_ALLOW_EMPTY_BACKEND";
    pub const DUNKINGSHEEP_API_HOST: &str = "DUNKINGSHEEP_API_HOST";
    pub const DUNKINGSHEEP_API_PORT: &str = "DUNKINGSHEEP_API_PORT";
    pub const DUNKINGSHEEP_API_VERBOSE: &str = "DUNKINGSHEEP_API_VERBOSE";
    pub const DUNKINGSHEEP_APP_DB: &str = "DUNKINGSHEEP_APP_DB";
    pub const DUNKINGSHEEP_BIN: &str = "DUNKINGSHEEP_BIN";
    pub const DUNKINGSHEEP_BIND_IDENTITY: &str = "DUNKINGSHEEP_BIND_IDENTITY";
    pub const DUNKINGSHEEP_CARRIED_FAILURE_MAX_S: &str = "DUNKINGSHEEP_CARRIED_FAILURE_MAX_S";
    pub const DUNKINGSHEEP_CLAUDE_BIN: &str = "DUNKINGSHEEP_CLAUDE_BIN";
    pub const DUNKINGSHEEP_CLAUDE_BUSY_TTL_S: &str = "DUNKINGSHEEP_CLAUDE_BUSY_TTL_S";
    pub const DUNKINGSHEEP_CLAUDE_END_WAIT_S: &str = "DUNKINGSHEEP_CLAUDE_END_WAIT_S";
    pub const DUNKINGSHEEP_CLAUDE_STATE_DIR: &str = "DUNKINGSHEEP_CLAUDE_STATE_DIR";
    pub const DUNKINGSHEEP_CLAUDE_STATUS: &str = "DUNKINGSHEEP_CLAUDE_STATUS";
    pub const DUNKINGSHEEP_CLAUDE_STATUS_TTL_S: &str = "DUNKINGSHEEP_CLAUDE_STATUS_TTL_S";
    pub const DUNKINGSHEEP_CLAUDE_TURN_TTL_S: &str = "DUNKINGSHEEP_CLAUDE_TURN_TTL_S";
    pub const DUNKINGSHEEP_CLI: &str = "DUNKINGSHEEP_CLI";
    pub const DUNKINGSHEEP_CODEX_END_WAIT_S: &str = "DUNKINGSHEEP_CODEX_END_WAIT_S";
    pub const DUNKINGSHEEP_CODEX_STATE_DIR: &str = "DUNKINGSHEEP_CODEX_STATE_DIR";
    pub const DUNKINGSHEEP_CODEX_TURN_TTL_S: &str = "DUNKINGSHEEP_CODEX_TURN_TTL_S";
    pub const DUNKINGSHEEP_CONTEXT_LEDGER: &str = "DUNKINGSHEEP_CONTEXT_LEDGER";
    pub const DUNKINGSHEEP_DESK_FILE: &str = "DUNKINGSHEEP_DESK_FILE";
    pub const DUNKINGSHEEP_DIE_WITH_PARENT: &str = "DUNKINGSHEEP_DIE_WITH_PARENT";
    pub const DUNKINGSHEEP_DIR: &str = "DUNKINGSHEEP_DIR";
    pub const DUNKINGSHEEP_DISPATCH_LANES: &str = "DUNKINGSHEEP_DISPATCH_LANES";
    pub const DUNKINGSHEEP_FEDERATED_INPUT: &str = "DUNKINGSHEEP_FEDERATED_INPUT";
    pub const DUNKINGSHEEP_GITHUB_API: &str = "DUNKINGSHEEP_GITHUB_API";
    pub const DUNKINGSHEEP_GITHUB_TOKENS_FILE: &str = "DUNKINGSHEEP_GITHUB_TOKENS_FILE";
    pub const DUNKINGSHEEP_GUARD: &str = "DUNKINGSHEEP_GUARD";
    pub const DUNKINGSHEEP_HERDR_BIN: &str = "DUNKINGSHEEP_HERDR_BIN";
    pub const DUNKINGSHEEP_IDLE_CLAIM_S: &str = "DUNKINGSHEEP_IDLE_CLAIM_S";
    pub const DUNKINGSHEEP_LEASE_S: &str = "DUNKINGSHEEP_LEASE_S";
    pub const DUNKINGSHEEP_MAILBOX: &str = "DUNKINGSHEEP_MAILBOX";
    pub const DUNKINGSHEEP_MAILBOX_CACHE_S: &str = "DUNKINGSHEEP_MAILBOX_CACHE_S";
    pub const DUNKINGSHEEP_OWNER_NAME: &str = "DUNKINGSHEEP_OWNER_NAME";
    pub const DUNKINGSHEEP_POOLRAD_REPO: &str = "DUNKINGSHEEP_POOLRAD_REPO";
    pub const DUNKINGSHEEP_PUSH_S: &str = "DUNKINGSHEEP_PUSH_S";
    pub const DUNKINGSHEEP_REAP_S: &str = "DUNKINGSHEEP_REAP_S";
    pub const DUNKINGSHEEP_RECOVERY_BINDINGS: &str = "DUNKINGSHEEP_RECOVERY_BINDINGS";
    pub const DUNKINGSHEEP_REPO: &str = "DUNKINGSHEEP_REPO";
    pub const DUNKINGSHEEP_ROLES_FILE: &str = "DUNKINGSHEEP_ROLES_FILE";
    pub const DUNKINGSHEEP_SANDBOX_WORKSPACE: &str = "DUNKINGSHEEP_SANDBOX_WORKSPACE";
    pub const DUNKINGSHEEP_SELF: &str = "DUNKINGSHEEP_SELF";
    pub const DUNKINGSHEEP_SERVICE_TOKEN: &str = "DUNKINGSHEEP_SERVICE_TOKEN";
    pub const DUNKINGSHEEP_SERVICE_TOKEN_FILE: &str = "DUNKINGSHEEP_SERVICE_TOKEN_FILE";
    pub const DUNKINGSHEEP_SOCKET: &str = "DUNKINGSHEEP_SOCKET";
    pub const DUNKINGSHEEP_STUCK_CONFIG: &str = "DUNKINGSHEEP_STUCK_CONFIG";
    pub const DUNKINGSHEEP_SUPERVISED: &str = "DUNKINGSHEEP_SUPERVISED";
    pub const DUNKINGSHEEP_TERMINAL_BACKEND: &str = "DUNKINGSHEEP_TERMINAL_BACKEND";
    pub const DUNKINGSHEEP_TEST_SPAWN: &str = "DUNKINGSHEEP_TEST_SPAWN";
    pub const DUNKINGSHEEP_TMUX_BIN: &str = "DUNKINGSHEEP_TMUX_BIN";
    pub const DUNKINGSHEEP_TMUX_REGISTRY: &str = "DUNKINGSHEEP_TMUX_REGISTRY";
    pub const DUNKINGSHEEP_TMUX_SOCKET: &str = "DUNKINGSHEEP_TMUX_SOCKET";
    pub const DUNKINGSHEEP_TOKENS_FILE: &str = "DUNKINGSHEEP_TOKENS_FILE";
    pub const DUNKINGSHEEP_TURN_END_MIN_GAP_S: &str = "DUNKINGSHEEP_TURN_END_MIN_GAP_S";
    pub const DUNKINGSHEEP_TURN_END_TASK_COOLDOWN_S: &str = "DUNKINGSHEEP_TURN_END_TASK_COOLDOWN_S";
    pub const DUNKINGSHEEP_TURN_END_WAIT_S: &str = "DUNKINGSHEEP_TURN_END_WAIT_S";
    pub const DUNKINGSHEEP_TURN_TTL_S: &str = "DUNKINGSHEEP_TURN_TTL_S";
    pub const DUNKINGSHEEP_TYPING_LOCK_LOUD_S: &str = "DUNKINGSHEEP_TYPING_LOCK_LOUD_S";
    pub const DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S: &str = "DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S";
    pub const DUNKINGSHEEP_USAGE_MAX_AGE_S: &str = "DUNKINGSHEEP_USAGE_MAX_AGE_S";
    pub const DUNKINGSHEEP_VERIFY_SELF: &str = "DUNKINGSHEEP_VERIFY_SELF";
    pub const DUNKINGSHEEP_VIEW_CACHE_S: &str = "DUNKINGSHEEP_VIEW_CACHE_S";
    pub const DUNKINGSHEEP_VPCTL: &str = "DUNKINGSHEEP_VPCTL";
    pub const DUNKINGSHEEP_VP_NAME: &str = "DUNKINGSHEEP_VP_NAME";
    pub const DUNKINGSHEEP_VP_PANE: &str = "DUNKINGSHEEP_VP_PANE";
}

/// Variables read ONLY by the test suite: not part of the daemon's contract.
pub mod test_only {
    /// Read by `tests/_bootstrap.py` to prove the suite owns its state directory
    /// (t247). It never reaches the daemon, so it is not in `names`.
    pub const DUNKINGSHEEP_TESTS_OWN_ENV: &str = "DUNKINGSHEEP_TESTS_OWN_ENV";
}

/// The config directory: `$DUNKINGSHEEP_DIR`, else `~/.config/dunkingsheep`.
pub fn config_dir() -> Result<PathBuf, DunkError> {
    if let Some(dir) = std::env::var_os(DIR_ENV) {
        let path = PathBuf::from(dir);
        if !path.as_os_str().is_empty() {
            return Ok(path);
        }
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| DunkError::Store(format!("{DIR_ENV} and HOME are both unset")))?;
    Ok(PathBuf::from(home).join(".config").join("dunkingsheep"))
}

/// A file inside the config directory.
pub fn config_path(name: &str) -> Result<PathBuf, DunkError> {
    Ok(config_dir()?.join(name))
}

/// The daemon socket: `$DUNKINGSHEEP_SOCKET`, else `<config>/dunkingsheep.sock`.
pub fn socket_path() -> Result<PathBuf, DunkError> {
    if let Some(path) = std::env::var_os(SOCKET_ENV) {
        let path = PathBuf::from(path);
        if !path.as_os_str().is_empty() {
            return Ok(path);
        }
    }
    config_path("dunkingsheep.sock")
}

/// The service token for a pane-less caller: the env value, else the 0600 file.
///
/// A no-pane request without one is `anonymous`, always: "no TTY means system" would be a
/// privilege grant any lane reaches with `setsid -f` (docs/FLAT_FLEET.md 4.2).
pub fn service_token() -> Result<Option<String>, DunkError> {
    if let Ok(token) = std::env::var(SERVICE_TOKEN_ENV) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(Some(token));
        }
    }
    let Some(path) = std::env::var_os(SERVICE_TOKEN_FILE_ENV) else {
        return Ok(None);
    };
    let path = Path::new(&path);
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let token = text.trim().to_string();
            Ok(if token.is_empty() { None } else { Some(token) })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(DunkError::Store(format!(
            "cannot read {}: {error}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path as StdPath;

    /// The sweep T02 asks for: every `os.environ`-read `DUNKINGSHEEP_*` name in the
    /// Python tree is in `names` (daemon) or `test_only`, and nothing else is.
    #[test]
    fn the_env_table_matches_the_tree() {
        let root = StdPath::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let mut found: Vec<String> = Vec::new();
        for entry in walk(&root) {
            let text = std::fs::read_to_string(&entry).unwrap_or_default();
            if !text.contains("os.environ") {
                continue;
            }
            for line in text.lines() {
                let mut rest = line;
                while let Some(at) = rest.find("DUNKINGSHEEP_") {
                    let tail = &rest[at..];
                    let name: String = tail
                        .chars()
                        .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
                        .collect();
                    if name.len() > "DUNKINGSHEEP_".len() && line.contains("os.environ") {
                        found.push(name);
                    }
                    rest = &tail[1..];
                }
            }
        }
        found.sort();
        found.dedup();
        let mut known: Vec<String> = TABLE_NAMES.iter().map(|s| s.to_string()).collect();
        known.push(test_only::DUNKINGSHEEP_TESTS_OWN_ENV.to_string());
        known.sort();
        let missing: Vec<&String> = found.iter().filter(|n| !known.contains(n)).collect();
        assert!(
            missing.is_empty(),
            "env names read but not in the table: {missing:?}"
        );
        let extra: Vec<&String> = known.iter().filter(|n| !found.contains(n)).collect();
        assert!(extra.is_empty(), "table names no module reads: {extra:?}");
    }

    /// The const values ARE the names, so the table cannot drift from itself.
    const TABLE_NAMES: &[&str] = &[
        names::DUNKINGSHEEP_TTY_IDENTITY,
        names::DUNKINGSHEEP_MIN_POLL_S,
        names::DUNKINGSHEEP_LEADER_PANE,
        names::DUNKINGSHEEP_FRESH,
        names::DUNKINGSHEEP_AGENT_VERIFY_S,
        names::DUNKINGSHEEP_ALLOW_EMPTY_BACKEND,
        names::DUNKINGSHEEP_API_HOST,
        names::DUNKINGSHEEP_API_PORT,
        names::DUNKINGSHEEP_API_VERBOSE,
        names::DUNKINGSHEEP_APP_DB,
        names::DUNKINGSHEEP_BIN,
        names::DUNKINGSHEEP_BIND_IDENTITY,
        names::DUNKINGSHEEP_CARRIED_FAILURE_MAX_S,
        names::DUNKINGSHEEP_CLAUDE_BIN,
        names::DUNKINGSHEEP_CLAUDE_BUSY_TTL_S,
        names::DUNKINGSHEEP_CLAUDE_END_WAIT_S,
        names::DUNKINGSHEEP_CLAUDE_STATE_DIR,
        names::DUNKINGSHEEP_CLAUDE_STATUS,
        names::DUNKINGSHEEP_CLAUDE_STATUS_TTL_S,
        names::DUNKINGSHEEP_CLAUDE_TURN_TTL_S,
        names::DUNKINGSHEEP_CLI,
        names::DUNKINGSHEEP_CODEX_END_WAIT_S,
        names::DUNKINGSHEEP_CODEX_STATE_DIR,
        names::DUNKINGSHEEP_CODEX_TURN_TTL_S,
        names::DUNKINGSHEEP_CONTEXT_LEDGER,
        names::DUNKINGSHEEP_DESK_FILE,
        names::DUNKINGSHEEP_DIE_WITH_PARENT,
        names::DUNKINGSHEEP_DIR,
        names::DUNKINGSHEEP_DISPATCH_LANES,
        names::DUNKINGSHEEP_FEDERATED_INPUT,
        names::DUNKINGSHEEP_GITHUB_API,
        names::DUNKINGSHEEP_GITHUB_TOKENS_FILE,
        names::DUNKINGSHEEP_GUARD,
        names::DUNKINGSHEEP_HERDR_BIN,
        names::DUNKINGSHEEP_IDLE_CLAIM_S,
        names::DUNKINGSHEEP_LEASE_S,
        names::DUNKINGSHEEP_MAILBOX,
        names::DUNKINGSHEEP_MAILBOX_CACHE_S,
        names::DUNKINGSHEEP_OWNER_NAME,
        names::DUNKINGSHEEP_POOLRAD_REPO,
        names::DUNKINGSHEEP_PUSH_S,
        names::DUNKINGSHEEP_REAP_S,
        names::DUNKINGSHEEP_RECOVERY_BINDINGS,
        names::DUNKINGSHEEP_REPO,
        names::DUNKINGSHEEP_ROLES_FILE,
        names::DUNKINGSHEEP_SANDBOX_WORKSPACE,
        names::DUNKINGSHEEP_SELF,
        names::DUNKINGSHEEP_SERVICE_TOKEN,
        names::DUNKINGSHEEP_SERVICE_TOKEN_FILE,
        names::DUNKINGSHEEP_SOCKET,
        names::DUNKINGSHEEP_STUCK_CONFIG,
        names::DUNKINGSHEEP_SUPERVISED,
        names::DUNKINGSHEEP_TERMINAL_BACKEND,
        names::DUNKINGSHEEP_TEST_SPAWN,
        names::DUNKINGSHEEP_TMUX_BIN,
        names::DUNKINGSHEEP_TMUX_REGISTRY,
        names::DUNKINGSHEEP_TMUX_SOCKET,
        names::DUNKINGSHEEP_TOKENS_FILE,
        names::DUNKINGSHEEP_TURN_END_MIN_GAP_S,
        names::DUNKINGSHEEP_TURN_END_TASK_COOLDOWN_S,
        names::DUNKINGSHEEP_TURN_END_WAIT_S,
        names::DUNKINGSHEEP_TURN_TTL_S,
        names::DUNKINGSHEEP_TYPING_LOCK_LOUD_S,
        names::DUNKINGSHEEP_TYPING_LOCK_TIMEOUT_S,
        names::DUNKINGSHEEP_USAGE_MAX_AGE_S,
        names::DUNKINGSHEEP_VERIFY_SELF,
        names::DUNKINGSHEEP_VIEW_CACHE_S,
        names::DUNKINGSHEEP_VPCTL,
        names::DUNKINGSHEEP_VP_NAME,
        names::DUNKINGSHEEP_VP_PANE,
    ];
    use super::*;

    fn walk(dir: &StdPath) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let Ok(entries) = std::fs::read_dir(dir) else {
            return out;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                if name == "target" || name == ".git" || name == "rust" {
                    continue;
                }
                out.extend(walk(&path));
            } else if path.extension().map(|e| e == "py").unwrap_or(false) {
                out.push(path);
            }
        }
        out
    }

    #[test]
    fn the_config_dir_follows_the_environment_variable() {
        // t735: DUNKINGSHEEP_DIR is the CONFIG dir, not the install dir.
        std::env::set_var(DIR_ENV, "/tmp/ds-config-test");
        assert_eq!(PathBuf::from("/tmp/ds-config-test"), config_dir().unwrap());
        std::env::remove_var(DIR_ENV);
    }

    #[test]
    fn the_socket_lives_inside_the_config_dir() {
        std::env::set_var(DIR_ENV, "/tmp/ds-config-test");
        std::env::remove_var(SOCKET_ENV);
        assert_eq!(
            PathBuf::from("/tmp/ds-config-test/dunkingsheep.sock"),
            socket_path().unwrap()
        );
        std::env::remove_var(DIR_ENV);
    }

    #[test]
    fn a_missing_token_file_is_none_not_an_error() {
        std::env::remove_var(SERVICE_TOKEN_ENV);
        std::env::set_var(SERVICE_TOKEN_FILE_ENV, "/tmp/ds-no-such-token-file");
        assert_eq!(None, service_token().unwrap());
        std::env::remove_var(SERVICE_TOKEN_FILE_ENV);
    }
}
