//! `client` (T41, first slice): the connect-or-start DECISION, ported from
//! `dunk_client.FlockClient.connect_or_start` / `start_daemon` / `_service_owns_socket`
//! (t696, Python commit d71848b, PR #216).
//!
//! Only the part that is a function of injected facts is here: the probe states, the
//! busy-guard, the bounded backoff and the owner-only production spawn. The socket
//! probe, the real sleep, `systemctl` and the process spawn are the caller's closures,
//! so the rules are testable without a daemon. Identity resolution, the request path
//! and the real `start_daemon` are later T41 slices.
//!
//! The rule t696 added: a refused connect on a socket that EXISTS (or an active unit)
//! means the daemon is saturated, not absent. Starting a `serve` from a worktree CLI
//! then only deepens the accept-queue pressure (six strays at once were measured).

use std::time::Duration;

use crate::error::DunkError;

/// What a probe of the socket found (`PROBE_*`). "Busy" is the state the old code
/// collapsed into "not running".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    Running,
    Busy,
    Absent,
}

/// Bounded backoff for a busy daemon: three retries, ~7.5 s total, no loop
/// (`BUSY_RETRY_DELAYS`).
pub const BUSY_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_millis(2000),
    Duration::from_millis(5000),
];

/// Environment variable that lets the OWNER spawn a daemon on the production socket.
pub const OWNER_START_ENV: &str = "DUNKINGSHEEP_OWNER_START";

/// `DaemonBusy` vs `DaemonUnavailable`: the daemon is UP but silent, or it is absent.
/// The texts are the Python's.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ConnectError {
    #[error("{0}")]
    Busy(String),
    #[error("{0}")]
    Unavailable(String),
}

impl From<ConnectError> for DunkError {
    fn from(error: ConnectError) -> Self {
        DunkError::Unavailable(error.to_string())
    }
}

/// `_service_owns_socket`: true when the production service owns this socket.
///
/// `is_default_socket`: the path is the production socket (anything else is a test or
/// private daemon and is never "owned"). `socket_exists` and `unit_active` are the two
/// facts the Python reads (`os.path.exists`, `systemctl --user is-active --quiet`).
pub fn service_owns_socket(
    is_default_socket: bool,
    socket_exists: bool,
    unit_active: impl FnOnce() -> bool,
) -> bool {
    if !is_default_socket {
        return false;
    }
    socket_exists || unit_active()
}

/// The refusal `start_daemon` raises on the production socket: only the owner
/// (`DUNKINGSHEEP_OWNER_START=1`) or systemd may create it. `supervised` is "this is
/// the production socket / systemd supervises it" (the Python's `supervised`).
pub fn check_spawn_allowed(
    supervised: bool,
    owner_start: Option<&str>,
) -> Result<(), ConnectError> {
    if supervised && owner_start != Some("1") {
        return Err(ConnectError::Unavailable(
            "refusing to spawn a daemon on the production socket: only the owner \
             (DUNKINGSHEEP_OWNER_START=1) or systemd may create it"
                .to_string(),
        ));
    }
    Ok(())
}

/// `connect_or_start` with every edge injected. Returns Ok(()) when the daemon answers.
///
/// A BUSY daemon is never started: it is retried with the bounded backoff, because
/// the retry storm is what fills its accept queue in the first place. An ABSENT probe
/// on a socket the service owns is treated as BUSY (t696). Only a truly absent daemon
/// is started, and `start` is not called at all when `start_allowed` is false.
#[allow(clippy::too_many_arguments)]
pub fn connect_or_start(
    socket_path: &str,
    mut probe: impl FnMut() -> Probe,
    owns_socket: impl FnOnce() -> bool,
    sleep: &mut impl FnMut(Duration),
    start_allowed: bool,
    mut start: impl FnMut() -> Result<(), ConnectError>,
    start_attempts: usize,
    log_file: &str,
) -> Result<(), ConnectError> {
    let mut state = probe();
    if state == Probe::Running {
        return Ok(());
    }
    if state == Probe::Absent && owns_socket() {
        state = Probe::Busy;
    }
    if state == Probe::Busy {
        for delay in BUSY_RETRY_DELAYS {
            sleep(delay);
            if probe() == Probe::Running {
                return Ok(());
            }
        }
        return Err(ConnectError::Busy(format!(
            "the dunkingsheep daemon on {socket_path} is running but did not answer \
             (listen queue full or server saturated). Do NOT restart it or start a second \
             one; retry in ~10s. See {log_file}"
        )));
    }
    if !start_allowed {
        return Err(ConnectError::Unavailable(format!(
            "no dunkingsheep daemon on {socket_path}"
        )));
    }
    start()?;
    let mut last = Probe::Absent;
    for _ in 0..start_attempts {
        last = probe();
        if last == Probe::Running {
            return Ok(());
        }
        sleep(Duration::from_millis(100));
    }
    if last == Probe::Busy {
        return Err(ConnectError::Busy(format!(
            "the dunkingsheep daemon on {socket_path} was started but is saturated and did \
             not answer; it is alive, so do NOT start another one. Retry in ~10s. See {log_file}"
        )));
    }
    Err(ConnectError::Unavailable(format!(
        "daemon did not come up on {socket_path}; see {log_file}"
    )))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
