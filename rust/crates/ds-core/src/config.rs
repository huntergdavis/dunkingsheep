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
    use super::*;

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
