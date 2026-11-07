//! The daemon side of the bridge: the owner mailbox verbs over the unix socket.
//!
//! The bridge is a pane-less system sender. It proves its role with the service token that the
//! unit points at (`DUNKINGSHEEP_SERVICE_TOKEN_FILE`), exactly as the Python `FlockClient()` does,
//! and it never starts the daemon: a missing daemon is a RuntimeError the loop retries.

use std::path::PathBuf;

use ds_core::client::{default_timeout, service_token, FlockClient, Transport, UnixTransport};
use ds_core::config::{socket_path, Env};
use serde_json::{Map, Value};

use crate::error::BridgeError;

/// One request, one answer: the subset of the client the bridge calls.
pub trait Daemon {
    fn call(&mut self, cmd: &str, args: Map<String, Value>) -> Result<Value, BridgeError>;
}

impl<T: Transport> Daemon for FlockClient<T> {
    fn call(&mut self, cmd: &str, args: Map<String, Value>) -> Result<Value, BridgeError> {
        FlockClient::call(self, cmd, args).map_err(|error| BridgeError::Runtime(error.to_string()))
    }
}

/// The socket the bridge talks to: `DUNKINGSHEEP_SOCKET`, else the config directory's socket.
pub fn socket_for(env: &Env) -> Result<PathBuf, BridgeError> {
    if let Some(explicit) = env
        .get("DUNKINGSHEEP_SOCKET")
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return Ok(PathBuf::from(explicit));
    }
    socket_path(env).map_err(|error| BridgeError::Runtime(error.to_string()))
}

/// The production client: pane-less, service-token authenticated, never starts the daemon.
pub fn system_client() -> Result<FlockClient<UnixTransport>, BridgeError> {
    let env = Env::from_process();
    let path = socket_for(&env)?;
    let process_env = |name: &str| std::env::var(name).ok();
    let timeout = default_timeout(&process_env);
    Ok(FlockClient::new_with_lazy_token(
        UnixTransport { path },
        None,
        "telegram bridge system sender".to_string(),
        timeout,
        || {
            let process_env = |name: &str| std::env::var(name).ok();
            service_token(&process_env, &|path| std::fs::read_to_string(path))
        },
    ))
}
