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

use std::path::PathBuf;
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

// ---- the request path (T41 slice 3): FlockClient -------------------------------------------
//
// Ported from `dunk_client.FlockClient`: `service_token`, `call`, `_call_once`, `probe`. One JSON
// request line out, one JSON response line back, over the daemon's Unix socket. The socket is a
// `Transport`, so every rule below is tested with a fake and one real round trip.

use serde_json::{json, Map, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

/// A read may be retried ONCE on timeout (idempotent). A write never is: the command may already
/// have run.
pub const READ_RETRIES: usize = 1;

/// The verbs safe to resend after a timeout (t693/t705): the retry decision is IDEMPOTENCY, never
/// the role allowlist. Deliberately absent though `roles::READ_ONLY` lists them: `lane_register`
/// and `superuser_recover` (bootstrap WRITES: the first owner may register itself) and `dispatch`
/// (a dry run is a read, a real tick files tasks); `turn_start`/`turn_end` are writes (retrying
/// one can record the same turn twice).
pub const IDEMPOTENT_READS: [&str; 41] = [
    "status",
    "poll",
    "health",
    "help",
    "doctor",
    "whoami",
    "roster",
    "message_state",
    "reply_stats",
    "project_summary",
    "state_latest",
    "task_list",
    "task_read",
    "task_stats",
    "task_metrics",
    "task_followers",
    "metrics",
    "inbox_count",
    "inbox_stats",
    "inbox_status",
    "list_panes",
    "list_workspaces",
    "list_dunks",
    "get_dunk",
    "delivery",
    "delivery_capability",
    "lanes",
    "memory_recall",
    "memory_search",
    "memory_context",
    "memory_stats",
    "memory_status",
    "owner_stats",
    "superuser_status",
    "pm_digest",
    "pm_routing",
    "list_messages",
    "get_message",
    "review_list",
    "poolrad_progress",
    "owner_receipts",
];

/// Errnos that mean "nothing is listening" rather than "listening but stuck"
/// (`ABSENT_ERRNOS`): ENOENT, ECONNREFUSED, ENOTSOCK, EADDRNOTAVAIL.
fn absent_errno(code: i32) -> bool {
    use nix::errno::Errno;
    [
        Errno::ENOENT,
        Errno::ECONNREFUSED,
        Errno::ENOTSOCK,
        Errno::EADDRNOTAVAIL,
    ]
    .iter()
    .any(|errno| *errno as i32 == code)
}

fn is_timeout(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
    )
}

/// What can go wrong on one call. `Dunk` is the daemon (or the identity rule) answering NO: it
/// proves the daemon is alive. The rest are transport failures.
#[derive(Debug)]
pub enum ClientError {
    Dunk(DunkError),
    /// `DaemonUnavailable`: the daemon closed the connection without replying.
    Unavailable(String),
    Io(std::io::Error),
    /// The reply was not JSON (the Python's `ValueError`).
    Protocol(String),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClientError::Dunk(error) => write!(formatter, "{error}"),
            ClientError::Unavailable(text) | ClientError::Protocol(text) => {
                write!(formatter, "{text}")
            }
            ClientError::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl From<ClientError> for DunkError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Dunk(inner) => inner,
            ClientError::Unavailable(text) => DunkError::Unavailable(text),
            ClientError::Io(inner) => DunkError::from(inner),
            ClientError::Protocol(text) => DunkError::Refused(text),
        }
    }
}

/// One request line out, one reply line back. `Ok(None)` is "the daemon closed the connection
/// without replying".
pub trait Transport {
    fn exchange(&self, line: &str, timeout: Duration) -> Result<Option<String>, std::io::Error>;
}

/// The real transport: a Unix stream socket. Only the CONNECT is retried (a 20-agent burst can
/// overflow a short listen backlog): three attempts, `50 ms * n` apart. Once connected nothing is
/// retried here, because the command may have run.
pub struct UnixTransport {
    pub path: PathBuf,
}

impl Transport for UnixTransport {
    fn exchange(&self, line: &str, timeout: Duration) -> Result<Option<String>, std::io::Error> {
        let mut stream = None;
        for attempt in 0..3u32 {
            match UnixStream::connect(&self.path) {
                Ok(connected) => {
                    stream = Some(connected);
                    break;
                }
                Err(error) => {
                    let retryable = is_timeout(&error)
                        || matches!(
                            error.kind(),
                            std::io::ErrorKind::ConnectionRefused
                                | std::io::ErrorKind::ConnectionReset
                        );
                    if attempt == 2 || !retryable {
                        return Err(error);
                    }
                    std::thread::sleep(Duration::from_millis(50 * u64::from(attempt + 1)));
                }
            }
        }
        let stream = stream.expect("connected or returned");
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut writer = &stream;
        writer.write_all(format!("{line}\n").as_bytes())?;
        let mut reply = String::new();
        let read = BufReader::new(&stream).read_line(&mut reply)?;
        Ok((read > 0).then_some(reply))
    }
}

/// `service_token`: a token for a pane-less process (web desk, timers), from
/// `DUNKINGSHEEP_SERVICE_TOKEN` or the 0600 file `DUNKINGSHEEP_SERVICE_TOKEN_FILE`. An unreadable
/// or empty file is no token, never an error.
pub fn service_token(
    env: EnvLookup<'_>,
    read_file: &dyn Fn(&str) -> std::io::Result<String>,
) -> Option<String> {
    let direct = env("DUNKINGSHEEP_SERVICE_TOKEN")
        .map(|t| t.trim().to_string())
        .unwrap_or_default();
    if !direct.is_empty() {
        return Some(direct);
    }
    let path = env("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
        .map(|p| p.trim().to_string())
        .unwrap_or_default();
    if path.is_empty() {
        return None;
    }
    read_file(&path)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

/// The client: who is calling, how patient, and the wire.
pub struct FlockClient<T: Transport> {
    transport: T,
    pub self_pane_id: Option<String>,
    pub identity_note: String,
    pub timeout: Duration,
    token: Option<String>,
    next_id: u64,
}

impl<T: Transport> FlockClient<T> {
    pub fn new(
        transport: T,
        self_pane_id: Option<String>,
        identity_note: String,
        timeout: Duration,
        token: Option<String>,
    ) -> Self {
        FlockClient {
            transport,
            self_pane_id,
            identity_note,
            timeout,
            token,
            next_id: 1,
        }
    }

    /// `call`: one command, its result or the reason it failed. No pane and no token is enough for
    /// a READ and not for a write (t602); a SYSTEM SENDER carries no pane by definition (attaching
    /// one made the daemon refuse the claim and the watchdog went silent).
    pub fn call(&mut self, cmd: &str, args: Map<String, Value>) -> Result<Value, ClientError> {
        if let Some(refusal) = identity_refusal_for(
            cmd,
            self.self_pane_id.as_deref(),
            self.token.is_some(),
            Some(&self.identity_note),
        ) {
            return Err(ClientError::Dunk(DunkError::Refused(refusal)));
        }
        let system_sender = args.get("system_sender").is_some_and(|value| match value {
            Value::Bool(flag) => *flag,
            Value::Null => false,
            other => other != &json!(0) && other != &json!(""),
        });
        let pane = if system_sender {
            None
        } else {
            self.self_pane_id.clone()
        };
        let mut request = json!({
            "id": self.next_id,
            "cmd": cmd,
            "args": args,
            "self_pane_id": pane,
        });
        if let Some(token) = &self.token {
            request["service_token"] = json!(token);
        }
        self.next_id += 1;
        let line = request.to_string();
        let attempts = 1 + if IDEMPOTENT_READS.contains(&cmd) {
            READ_RETRIES
        } else {
            0
        };
        let mut attempt = 0;
        loop {
            attempt += 1;
            match self.call_once(&line) {
                Err(ClientError::Io(error)) if is_timeout(&error) && attempt < attempts => continue,
                other => return other,
            }
        }
    }

    fn call_once(&self, line: &str) -> Result<Value, ClientError> {
        let raw = self
            .transport
            .exchange(line, self.timeout)
            .map_err(ClientError::Io)?;
        let Some(raw) = raw else {
            return Err(ClientError::Unavailable(
                "daemon closed the connection without replying".to_string(),
            ));
        };
        let response: Value = serde_json::from_str(raw.trim_end())
            .map_err(|error| ClientError::Protocol(format!("invalid daemon reply: {error}")))?;
        if response.get("ok").and_then(Value::as_bool) != Some(true) {
            let text = response
                .get("error")
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
                .unwrap_or("unknown error");
            return Err(ClientError::Dunk(DunkError::Refused(text.to_string())));
        }
        Ok(response.get("result").cloned().unwrap_or(Value::Null))
    }

    /// `probe`: classify the daemon WITHOUT side effects. A refused command still proves the
    /// daemon answered, so a `Dunk` error counts as running; nothing listening (ENOENT,
    /// ECONNREFUSED...) is absent; a listener with no reply (full accept queue, reset, timeout, a
    /// reply that is not JSON) is BUSY: alive, and starting another one is wrong.
    pub fn probe(&mut self) -> Probe {
        match self.call("status", Map::new()) {
            Ok(_) | Err(ClientError::Dunk(_)) => Probe::Running,
            Err(ClientError::Unavailable(_)) | Err(ClientError::Protocol(_)) => Probe::Busy,
            Err(ClientError::Io(error)) => {
                if error.raw_os_error().is_some_and(absent_errno) {
                    Probe::Absent
                } else {
                    Probe::Busy
                }
            }
        }
    }

    pub fn is_running(&mut self) -> bool {
        self.probe() == Probe::Running
    }
}

// ---- start_daemon (T41 slice 4) --------------------------------------------------------------
//
// Ported from `dunk_client.FlockClient.start_daemon`: hand off to systemd when it supervises the
// socket, otherwise spawn a detached `dunkingsheep serve`. The DECISION is a pure function of
// injected facts; the spawn is one small helper.
//
// NOT ported, on purpose: the Python's `PR_SET_PDEATHSIG` in the child (test spawns only) needs a
// `pre_exec` hook, which is `unsafe`, and RUST_PORT.md forbids `unsafe`. The userspace
// `DUNKINGSHEEP_DIE_WITH_PARENT` watch (also test-only) IS passed through. The Python's
// `start_new_session=True` (setsid) becomes `process_group(0)`: a new process group that terminal
// signals do not reach, but not a new session.

/// What `start_daemon` decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartAction {
    /// systemd started (or already runs) the unit; nothing to spawn.
    HandedToSystemd,
    /// Spawn a detached daemon with this environment.
    Spawn(SpawnSpec),
}

/// The environment the spawned daemon gets, as changes to the caller's own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// `DUNKINGSHEEP_SOCKET` for the child.
    pub socket_path: String,
    /// `(name, value)` pairs to set in the child.
    pub set: Vec<(String, String)>,
    /// Names to remove from the child's environment (`HERDR_PANE_ID`: a daemon must not inherit
    /// the starter's pane).
    pub remove: Vec<String>,
}

/// The facts `start_decision` reads, gathered by the caller.
pub struct StartInputs<'a> {
    /// The socket this client talks to.
    pub socket_path: &'a str,
    /// The production socket (`DEFAULT_SOCKET`): it belongs to the unit, even when a parent
    /// service exported `DUNKINGSHEEP_SOCKET` to that same path.
    pub default_socket: &'a str,
    pub systemd_installed: bool,
    pub has_systemctl: bool,
    /// `systemctl --user start dunkingsheep.service`: Some(true) started, Some(false) failed,
    /// None could not run.
    pub systemctl_start: &'a dyn Fn() -> Option<bool>,
    /// The backend the DAEMON would use ('tmux' by default), from the one place that defines it.
    pub backend: &'a str,
    pub env: EnvLookup<'a>,
    /// The starter's pid, for a test spawn's die-with-parent watch.
    pub pid: u32,
}

/// `start_daemon`'s decision. The guards apply to the SPAWN path only, and that ordering is
/// deliberate: the historical failure was a client spawning a trial daemon with a guessed
/// environment. A unit started by systemd carries its own environment, so requiring the tmux
/// socket in the CALLER would refuse the handoff to a correctly configured daemon.
pub fn start_decision(inputs: &StartInputs<'_>) -> Result<StartAction, ConnectError> {
    let supervised = inputs.socket_path == inputs.default_socket;
    if supervised && inputs.systemd_installed && inputs.has_systemctl {
        if (inputs.systemctl_start)() == Some(true) {
            return Ok(StartAction::HandedToSystemd);
        }
        return Err(ConnectError::Unavailable(
            "the dunkingsheep.service unit is installed but could not be started; run \
             `systemctl --user start dunkingsheep.service`"
                .to_string(),
        ));
    }
    check_spawn_allowed(supervised, (inputs.env)(OWNER_START_ENV).as_deref())?;
    let tmux_socket = (inputs.env)("DUNKINGSHEEP_TMUX_SOCKET").unwrap_or_default();
    if inputs.backend == "tmux" && tmux_socket.is_empty() {
        return Err(ConnectError::Unavailable(
            "refusing to auto-start a tmux daemon without DUNKINGSHEEP_TMUX_SOCKET; start it \
             explicitly with the full backend environment"
                .to_string(),
        ));
    }
    let mut set = vec![(
        "DUNKINGSHEEP_SOCKET".to_string(),
        inputs.socket_path.to_string(),
    )];
    // A TEST-SPAWNED DAEMON MUST NOT OUTLIVE ITS PARENT (t212): the suite marks its spawns with
    // DUNKINGSHEEP_TEST_SPAWN=1 and the daemon watches the pid in DUNKINGSHEEP_DIE_WITH_PARENT. The
    // value is the SUITE's pid (setdefault: an exported one wins), not this call's, or the daemon
    // would die between the CLI calls of one test. A normal auto-start is unchanged.
    if (inputs.env)("DUNKINGSHEEP_TEST_SPAWN").as_deref() == Some("1")
        && (inputs.env)("DUNKINGSHEEP_DIE_WITH_PARENT").is_none()
    {
        set.push((
            "DUNKINGSHEEP_DIE_WITH_PARENT".to_string(),
            inputs.pid.to_string(),
        ));
    }
    Ok(StartAction::Spawn(SpawnSpec {
        socket_path: inputs.socket_path.to_string(),
        set,
        remove: vec!["HERDR_PANE_ID".to_string()],
    }))
}

/// Spawn `<exe> <args...>` detached: stdin from /dev/null, stdout and stderr appended to `log`, its
/// own process group, the caller's environment plus `spec`. Returns once the child is started; it
/// is NOT waited for (a lane's daemon is supposed to outlive the call that started it).
pub fn spawn_detached(
    exe: &std::path::Path,
    args: &[&str],
    spec: &SpawnSpec,
    log: &std::path::Path,
    cwd: &std::path::Path,
) -> std::io::Result<u32> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)?;
    let err = out.try_clone()?;
    let mut command = Command::new(exe);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err)
        .current_dir(cwd)
        .process_group(0);
    for name in &spec.remove {
        command.env_remove(name);
    }
    for (name, value) in &spec.set {
        command.env(name, value);
    }
    command.spawn().map(|child| child.id())
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
