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

// ---- identity (T41 slice 2): who is calling, and when to refuse ---------------------------
//
// Ported from `dunk_client.py`: `default_timeout`, `controlling_tty`, `pane_for_tty`,
// `exported_pane_id`, `resolve_identity`, `_terminal_exists`, `identity_refusal_for`.
// Everything that touches the system (the environment, /proc, tmux, the backend) is a closure
// argument, so the rules are tested with fakes and the real wiring is one small caller.

/// The environment as a lookup (`os.environ.get`).
pub type EnvLookup<'a> = &'a dyn Fn(&str) -> Option<String>;

/// `(pane id, why)`: which pane owns the terminal, or why we cannot say (`pane_for_tty`).
pub type PaneAnswer = (Option<String>, String);

/// The client's patience (t693): `DUNKINGSHEEP_CLIENT_TIMEOUT` or 45 s, the measured p90 with
/// headroom (a 15 s deadline turned 3 of 8 cheap reads into failures, and a failed read is what
/// marks a live lane dead). An unparseable or empty value is the default, like the Python.
pub fn default_timeout(env: EnvLookup<'_>) -> Duration {
    env("DUNKINGSHEEP_CLIENT_TIMEOUT")
        .filter(|text| !text.is_empty())
        .and_then(|text| text.trim().parse::<f64>().ok())
        .filter(|seconds| seconds.is_finite() && *seconds >= 0.0)
        .map_or(Duration::from_secs(45), Duration::from_secs_f64)
}

/// `exported_pane_id`: TMUX_PANE wins over HERDR_PANE_ID on purpose. A tmux pane brought up
/// for a herdr lane inherits that lane's environment, so HERDR_PANE_ID keeps naming the pane the
/// lane used to be in (measured 2026-10-24: every tmux pane carried its dead herdr twin's id).
pub fn exported_pane_id(env: EnvLookup<'_>) -> Option<String> {
    ["TMUX_PANE", "HERDR_PANE_ID"]
        .iter()
        .filter_map(|name| env(name))
        .find(|value| !value.is_empty())
}

/// Decode a `tty_nr` (Linux `dev_t`) into `/dev/pts/N`, or None when it is not a pty master/slave
/// (majors 136-143, as the Python checks).
fn pts_from_tty_nr(tty_nr: u64) -> Option<String> {
    let major = (tty_nr >> 8) & 0xfff;
    let minor = (tty_nr & 0xff) | ((tty_nr >> 12) & 0xffff_ff00);
    (136..=143)
        .contains(&major)
        .then(|| format!("/dev/pts/{minor}"))
}

/// `controlling_tty`: the `/dev/pts/N` of the nearest process in this parent chain that has one.
/// An agent's command runner often has NO controlling terminal (the harness gives it a pipe)
/// while the agent above it owns the pane's pty, so this walks up the parents instead of reading
/// fd 0. `read_stat(pid)` returns the raw `/proc/<pid>/stat` text.
pub fn controlling_tty(
    start_pid: u32,
    max_depth: usize,
    read_stat: &dyn Fn(u32) -> Option<String>,
) -> Option<String> {
    let mut pid = start_pid;
    for _ in 0..max_depth {
        let stat = read_stat(pid)?;
        // the command name is parenthesised and may contain spaces or parens: split after the LAST ")"
        let after = stat.rsplit_once(')')?.1;
        let fields: Vec<&str> = after.split_whitespace().collect();
        let ppid: u32 = fields.get(1)?.parse().ok()?;
        let tty_nr: u64 = fields.get(4)?.parse().ok()?;
        if tty_nr != 0 {
            return pts_from_tty_nr(tty_nr);
        }
        if ppid <= 1 {
            return None;
        }
        pid = ppid;
    }
    None
}

/// `pane_for_tty`: which tmux pane owns this terminal. Refuses on zero matches and on more than
/// one: a tty that names two panes is a question, not an identity. `list_panes` returns the
/// output of `tmux list-panes -a -F '#{pane_id} #{pane_tty}'`, or the reason it could not run.
pub fn pane_for_tty(
    tty: Option<&str>,
    list_panes: &dyn Fn() -> Result<String, String>,
) -> PaneAnswer {
    let Some(tty) = tty.filter(|value| !value.is_empty()) else {
        return (
            None,
            "no controlling terminal found in this process chain".to_string(),
        );
    };
    let listing = match list_panes() {
        Ok(text) => text,
        Err(error) => return (None, error),
    };
    let matches: Vec<&str> = listing
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let (pane, owner) = (parts.next()?, parts.next()?);
            (parts.next().is_none() && owner == tty).then_some(pane)
        })
        .collect();
    match matches.as_slice() {
        [one] => (Some((*one).to_string()), format!("resolved by {tty}")),
        [] => (None, format!("no tmux pane owns {tty}")),
        many => (
            None,
            format!(
                "{tty} is claimed by {} panes ({})",
                many.len(),
                many.join(", ")
            ),
        ),
    }
}

/// `_terminal_exists`: true when the lookup's reason says a TERMINAL was found but did not
/// resolve ('<tty> is claimed by N panes' / 'no tmux pane owns <tty>'); 'no controlling terminal
/// found...' means there is none, and only the first class is a refusal.
fn terminal_exists(note: &str) -> bool {
    note.starts_with("/dev/") || note.contains("owns /dev/") || note.contains("claimed by")
}

/// `resolve_identity`: `(pane id, note)` for this caller, refusing only a LYING environment.
///
/// The terminal we are IN beats a claim inherited from somewhere else, and a claim that disagrees
/// with it is refused rather than preferred. An ABSENT identity is not a lying one: a systemd
/// unit has no pane and no pty, so it is ANONYMOUS (refusing there took every unit down, VP
/// i24050). A refusal is for a terminal that EXISTS and either disagrees with the claim or does
/// not resolve to exactly one pane (t602: `allow_unresolved` lets a read-only verb through as
/// anonymous). The tty fallback confers identity ONLY: no superuser, no capability.
///
/// `detect` stands for the backend's own "which pane am I" (herdr `pane current`, retired under
/// tmux): pass `&|| None` where there is none.
pub fn resolve_identity(
    explicit: Option<&str>,
    env: EnvLookup<'_>,
    tty_lookup: &dyn Fn() -> PaneAnswer,
    detect: &dyn Fn() -> Option<String>,
    allow_unresolved: bool,
) -> Result<(Option<String>, String), DunkError> {
    if let Some(pane) = explicit {
        return Ok((Some(pane.to_string()), "explicit".to_string()));
    }
    if env("DUNKINGSHEEP_VERIFY_SELF").as_deref() == Some("1") {
        if let Some(found) = detect() {
            return Ok((
                Some(found),
                "verified by the backend (VERIFY_SELF)".to_string(),
            ));
        }
    }
    let trimmed = |name: &str| env(name).map(|v| v.trim().to_string()).unwrap_or_default();
    let tmux_claim = trimmed("TMUX_PANE");
    let claim = if tmux_claim.is_empty() {
        trimmed("HERDR_PANE_ID")
    } else {
        tmux_claim
    };
    if env("DUNKINGSHEEP_TTY_IDENTITY").as_deref().unwrap_or("1") == "0" {
        let found = if claim.is_empty() {
            detect()
        } else {
            Some(claim)
        };
        return Ok((found, "env only (tty check off)".to_string()));
    }
    let (pane, note) = tty_lookup();
    if !claim.is_empty() {
        if let Some(owned) = &pane {
            if *owned != claim {
                return Err(DunkError::Identity(format!(
                    "TMUX_PANE/HERDR_PANE_ID says {claim:?} but this terminal is pane {owned:?} \
                     ({note}). The terminal you are in wins, and a claim that disagrees with it \
                     is not corrected silently - if you inherited it from another lane, unset it \
                     or set it to yours."
                )));
            }
        }
        let why = if pane.is_some() {
            format!("env, confirmed by {note}")
        } else {
            format!("env only ({note})")
        };
        return Ok((Some(claim), why));
    }
    if let Some(owned) = pane {
        return Ok((Some(owned), format!("resolved by terminal ({note})")));
    }
    if terminal_exists(&note) {
        if allow_unresolved {
            return Ok((
                None,
                format!("anonymous: the terminal resolved to no single pane ({note})"),
            ));
        }
        return Err(DunkError::Identity(format!(
            "the terminal exists but resolved to no single pane ({note}). Set TMUX_PANE, or run \
             inside a tmux pane."
        )));
    }
    if let Some(found) = detect() {
        return Ok((
            Some(found),
            "detected by the backend (no terminal in this process chain)".to_string(),
        ));
    }
    Ok((
        None,
        format!("anonymous: no pane in the environment and no terminal ({note})"),
    ))
}

/// `identity_refusal_for` (t602): why a pane-less caller may not run `cmd`, or None when it may.
/// A terminal that resolves to no pane is anonymous, not broken: the read-only verbs must work
/// from there. The rule is the daemon's own allowlist (`roles::READ_ONLY`), so client and daemon
/// cannot drift. The refusal names the exit, because a lane that cannot run any command cannot
/// discover one.
pub fn identity_refusal_for(
    cmd: &str,
    self_pane_id: Option<&str>,
    has_token: bool,
    note: Option<&str>,
) -> Option<String> {
    if self_pane_id.is_some() || has_token || crate::roles::READ_ONLY.contains(&cmd) {
        return None;
    }
    let where_ = note.map(|text| format!(" ({text})")).unwrap_or_default();
    Some(format!(
        "identity refused: {cmd} needs a lane and this terminal resolved to no pane{where_}. \
         Read-only commands work here. For a write, run it inside a tmux pane, or issue a \
         labelled, revocable token on a pane that has one: dunkingsheep token issue --role owner \
         --label <why>  (then set DUNKINGSHEEP_SERVICE_TOKEN to it)."
    ))
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
