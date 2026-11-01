//! The daemon's socket server: ported from `dunk_server.py` (T40 slice 1, t1364).
//!
//! Newline-delimited JSON over a unix socket, any number of requests per connection
//! (see [`crate::protocol`]). This slice is the SKELETON: listener, per-connection read
//! loop, `handle_line`, peer pid from `SO_PEERCRED`, identity binding, the startup refusals,
//! signals and die-with-parent. The commands themselves plug in through [`Dispatcher`]
//! (T39); nothing here knows a command name except `shutdown`.
//!
//! PER-REQUEST CONTEXT: the Python keeps the caller, service token and bound identity in
//! `threading.local` on the Flock. Here they are a [`RequestCtx`] built per request and handed
//! to the dispatcher, so one connection can never see another connection's state.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Env;
use crate::error::DunkError;
use crate::proc::{caller_from_peer, caller_pane_auth_failure, Caller};
use crate::protocol::{encode_line, Response};
use crate::terminal::Terminal;

/// The caller state of ONE request (the Python's `Flock._caller_state` thread-local).
#[derive(Debug, Clone, Default)]
pub struct RequestCtx {
    /// The resolved caller behind the socket peer; `None` without peer credentials.
    pub caller: Option<Caller>,
    pub service_token: Option<String>,
    /// The pane the request acts as AFTER identity binding (`None` = anonymous).
    pub self_pane_id: Option<String>,
    /// `Some(true)` bound to the pane process, `Some(false)` refused, `None` not checked.
    pub identity_bound: Option<bool>,
    pub identity_note: Option<String>,
    /// Transport provenance (t423): a socket request whose peer credentials failed must not
    /// degrade into the absent-caller state that trusted in-process code uses.
    pub socket_peer: bool,
}

/// What the server needs to know about a pane to bind an identity to it.
#[derive(Debug, Clone, Default)]
pub struct PaneInfo {
    pub pane_pid: Option<i64>,
}

/// The command layer behind the socket (T39 implements it on the Flock).
pub trait Dispatcher: Send + Sync {
    /// Run one command. `Err` becomes `{ok:false,error}` (plus `retryable` when it is).
    fn dispatch(&self, cmd: &str, args: &Value, ctx: &RequestCtx) -> Result<Value, DunkError>;

    /// `Flock._role_pane_info`: `Ok(None)` = the backend answered and knows no such pane;
    /// `Err` = the backend could not answer (the claim then stands).
    fn pane_info(&self, _pane_id: &str) -> Result<Option<PaneInfo>, DunkError> {
        Ok(Some(PaneInfo::default()))
    }

    /// `flock.metrics.record("identity.mismatch", ...)`; failure must never fail a request.
    fn record_identity_mismatch(&self, _pane_id: &str, _reason: &str) {}

    /// `flock.close()`, called once when the server closes.
    fn close(&self) {}
}

/// Another daemon already answers on the socket.
#[derive(Debug)]
pub struct AlreadyRunning(pub String);

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub struct FlockServer {
    dispatcher: Arc<dyn Dispatcher>,
    pub socket_path: PathBuf,
    listener: Option<UnixListener>,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<BTreeMap<u64, UnixStream>>>,
}

impl FlockServer {
    pub fn new(dispatcher: Arc<dyn Dispatcher>, socket_path: impl Into<PathBuf>) -> Self {
        Self {
            dispatcher,
            socket_path: socket_path.into(),
            listener: None,
            stop: Arc::new(AtomicBool::new(false)),
            connections: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    pub fn is_bound(&self) -> bool {
        self.listener.is_some()
    }

    /// A handle that stops the accept loop from another thread (`shutdown`).
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    pub fn shutdown(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn bind(&mut self) -> Result<(), BindError> {
        if let Some(parent) = self.socket_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(BindError::Io)?;
            }
        }
        let listener = match UnixListener::bind(&self.socket_path) {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                if socket_alive(&self.socket_path) {
                    return Err(BindError::AlreadyRunning(AlreadyRunning(format!(
                        "a daemon is already listening on {}",
                        self.socket_path.display()
                    ))));
                }
                // Stale socket file left by a daemon that died; reclaim it.
                std::fs::remove_file(&self.socket_path).map_err(BindError::Io)?;
                UnixListener::bind(&self.socket_path).map_err(BindError::Io)?
            }
            Err(error) => return Err(BindError::Io(error)),
        };
        std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))
            .map_err(BindError::Io)?;
        listener.set_nonblocking(true).map_err(BindError::Io)?;
        self.listener = Some(listener);
        tracing::info!(
            "dunkingsheep listening on {} (pid {})",
            self.socket_path.display(),
            std::process::id()
        );
        Ok(())
    }

    /// Accept until [`shutdown`](Self::shutdown); one thread per connection.
    pub fn serve_forever(&mut self) -> Result<(), BindError> {
        if self.listener.is_none() {
            self.bind()?;
        }
        let mut next_id = 0u64;
        let mut failure = None;
        while !self.stop.load(Ordering::SeqCst) {
            let accepted = match self.listener.as_ref() {
                Some(listener) => listener.accept(),
                None => break,
            };
            match accepted {
                Ok((conn, _addr)) => {
                    next_id += 1;
                    let id = next_id;
                    let dispatcher = Arc::clone(&self.dispatcher);
                    let stop = Arc::clone(&self.stop);
                    let connections = Arc::clone(&self.connections);
                    std::thread::spawn(move || {
                        serve_connection(dispatcher, conn, id, stop, connections)
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(error) => {
                    if !self.stop.load(Ordering::SeqCst) {
                        failure = Some(BindError::Io(error));
                    }
                    break;
                }
            }
        }
        self.close();
        failure.map_or(Ok(()), Err)
    }

    pub fn close(&mut self) {
        self.dispatcher.close();
        self.listener = None;
        if let Ok(connections) = self.connections.lock() {
            for conn in connections.values() {
                let _ = conn.shutdown(std::net::Shutdown::Both);
            }
        }
        if self.socket_path.exists() {
            let _ = std::fs::remove_file(&self.socket_path);
        }
        tracing::info!("daemon stopped");
    }

    /// Return (response, stop_after). The socket peer is the short-lived CLI; the caller
    /// identity is its nearest agent ancestor (or session leader), resolved by `proc` and
    /// attached to task leases for `/proc` liveness.
    pub fn handle_line(&self, line: &str, peer_pid: Option<i64>) -> (Response, bool) {
        handle_line(self.dispatcher.as_ref(), line, peer_pid)
    }
}

#[derive(Debug)]
pub enum BindError {
    AlreadyRunning(AlreadyRunning),
    Io(std::io::Error),
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::AlreadyRunning(error) => error.fmt(f),
            BindError::Io(error) => error.fmt(f),
        }
    }
}

fn serve_connection(
    dispatcher: Arc<dyn Dispatcher>,
    conn: UnixStream,
    id: u64,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<BTreeMap<u64, UnixStream>>>,
) {
    if let (Ok(mut map), Ok(clone)) = (connections.lock(), conn.try_clone()) {
        map.insert(id, clone);
    }
    let _ = conn.set_nonblocking(false);
    let peer_pid = peer_pid(&conn);
    if let Ok(reader) = conn.try_clone() {
        let mut writer = conn;
        for raw in BufReader::new(reader).split(b'\n') {
            let Ok(raw) = raw else { break };
            let text = String::from_utf8_lossy(&raw);
            let line = text.trim();
            if line.is_empty() {
                continue;
            }
            let (response, stop_after) = handle_line(dispatcher.as_ref(), line, peer_pid);
            if writer.write_all(encode_line(&response).as_bytes()).is_err() {
                break;
            }
            if stop_after {
                stop.store(true, Ordering::SeqCst);
                break;
            }
        }
    }
    if let Ok(mut map) = connections.lock() {
        map.remove(&id);
    }
}

/// The connecting process, from `SO_PEERCRED` (Linux); `None` when it cannot be read or is 0.
pub fn peer_pid(conn: &UnixStream) -> Option<i64> {
    use nix::sys::socket::{getsockopt, sockopt::PeerCredentials};
    let creds = getsockopt(conn, PeerCredentials).ok()?;
    let pid = i64::from(creds.pid());
    (pid != 0).then_some(pid)
}

/// True if something accepts connections on the unix socket at `path`.
pub fn socket_alive(path: &Path) -> bool {
    UnixStream::connect(path).is_ok()
}

/// The Python's `repr`-free truthiness for `request.get("args") or {}`.
fn falsy(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(flag) => !flag,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(map) => map.is_empty(),
    }
}

/// One request line to one response. `Dispatcher` panics are answered, never propagated:
/// one bad request must not kill the connection thread.
pub fn handle_line(
    dispatcher: &dyn Dispatcher,
    line: &str,
    peer_pid: Option<i64>,
) -> (Response, bool) {
    let request: Value = match serde_json::from_str(line) {
        Ok(value) => value,
        Err(_) => return (Response::invalid_json(), false),
    };
    let Value::Object(request) = request else {
        return (bad(Value::Null, "request must be an object"), false);
    };
    let req_id = request.get("id").cloned().unwrap_or(Value::Null);
    let name = request.get("cmd").cloned().unwrap_or(Value::Null);
    let args = match request.get("args") {
        Some(value) if !falsy(value) => value.clone(),
        _ => json!({}),
    };
    let claimed = request
        .get("self_pane_id")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_string);
    if !args.is_object() {
        return (bad(req_id, "args must be an object"), false);
    }
    let caller = peer_pid.map(|pid| caller_from_peer(Some(pid), claimed.clone()));
    let mut ctx = RequestCtx {
        caller,
        service_token: request
            .get("service_token")
            .and_then(Value::as_str)
            .map(str::to_string),
        socket_peer: peer_pid.is_some(),
        ..RequestCtx::default()
    };
    ctx.self_pane_id = bind_identity(dispatcher, &mut ctx, claimed, &name);
    // Python's `dispatch(name)` formats a non-string name through repr; the dispatcher sees
    // the string form (NOT verified for a missing/non-string cmd: the empty string stands in).
    let command = name.as_str().unwrap_or("");
    let outcome = catch_unwind(AssertUnwindSafe(|| {
        dispatcher.dispatch(command, &args, &ctx)
    }));
    match outcome {
        Ok(Ok(result)) => (Response::ok(req_id, result), command == "shutdown"),
        Ok(Err(error)) => (Response::refused(req_id, &error), false),
        Err(panic) => {
            let text = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_default();
            tracing::error!("command {command} failed");
            (bad(req_id, &format!("internal error: {text}")), false)
        }
    }
}

fn bad(id: Value, text: &str) -> Response {
    Response {
        id,
        ok: false,
        result: None,
        error: Some(text.to_string()),
        retryable: None,
    }
}

/// The pane a request acts as is the pane its process runs in, or none.
///
/// `self_pane_id` is a claim from the client (docs/FLAT_FLEET.md 4.1). Where the kernel can
/// check it - the pane record carries a root pid and the peer's agent ancestor is in that pid's
/// ancestry - a disagreeing claim is replaced by nothing: the request runs as anonymous, the
/// mismatch is counted, and the caller sees it in `whoami`. Where it cannot be checked (a backend
/// that reports no pane pid, no /proc), the claim stands, as before.
/// `DUNKINGSHEEP_BIND_IDENTITY=0` is the rollback switch.
pub fn bind_identity(
    dispatcher: &dyn Dispatcher,
    ctx: &mut RequestCtx,
    claimed: Option<String>,
    command: &Value,
) -> Option<String> {
    ctx.identity_bound = None;
    ctx.identity_note = None;
    let claimed = claimed?;
    // No peer credentials (or no claim): nothing to check the claim against, so it stands.
    if ctx.caller.is_none() {
        return Some(claimed);
    }
    if std::env::var("DUNKINGSHEEP_BIND_IDENTITY").as_deref() == Ok("0") {
        ctx.identity_note = Some("binding disabled by DUNKINGSHEEP_BIND_IDENTITY=0".into());
        return Some(claimed);
    }
    if !Path::new("/proc").is_dir() {
        ctx.identity_note = Some("no /proc; claim stands".into());
        return Some(claimed);
    }
    let caller_pid = ctx.caller.as_ref().and_then(|c| c.pid);
    let pane = match dispatcher.pane_info(&claimed) {
        Ok(Some(pane)) => pane,
        Ok(None) => {
            // The backend answered and knows no such pane: a made-up id (a test run's 'w1:p2'
            // claimed live review tasks on 2026-10-29) is nobody.
            ctx.identity_bound = Some(false);
            ctx.identity_note = Some(format!(
                "claim {claimed} refused: no such pane; running anonymous"
            ));
            tracing::warn!(
                "identity mismatch cmd={command} claimed={claimed:?} reason=no_such_pane caller_pid={caller_pid:?}"
            );
            return None;
        }
        // A backend that cannot answer does not contradict the claim.
        Err(_) => PaneInfo::default(),
    };
    let Some(pane_pid) = pane.pane_pid else {
        ctx.identity_note = Some("pane has no root pid; claim stands".into());
        return Some(claimed);
    };
    if !Path::new(&format!("/proc/{pane_pid}")).is_dir() {
        // A pane whose root process is gone cannot contradict the claim.
        ctx.identity_note = Some(format!(
            "pane root pid {pane_pid} not running; claim stands"
        ));
        return Some(claimed);
    }
    match caller_pane_auth_failure(ctx.caller.as_ref(), Some(pane_pid)) {
        None => {
            ctx.identity_bound = Some(true);
            ctx.identity_note = Some("bound to pane process".into());
            Some(claimed)
        }
        Some(reason) => {
            ctx.identity_bound = Some(false);
            ctx.identity_note = Some(format!(
                "claim {claimed} refused: {reason}; running anonymous"
            ));
            tracing::warn!(
                "identity mismatch cmd={command} claimed={claimed:?} reason={reason} caller_pid={caller_pid:?} caller_source={:?} pane_pid={pane_pid:?}",
                ctx.caller.as_ref().map(|c| c.source)
            );
            dispatcher.record_identity_mismatch(&claimed, reason);
            None
        }
    }
}

/// Why the daemon will not start, as `(exit code, log text)`, or `None` to go ahead.
///
/// 3: systemd owns the default socket (a direct spawn would steal it from the unit and put
/// it in a restart loop). 4 (GATE 5, 2026-10-24): a backend that sees NO panes is a wrong
/// environment, not a degraded mode - it would accept every message and fail every delivery.
pub fn startup_refusal(
    env: &Env,
    socket_path: &Path,
    default_socket: &Path,
    unit_exists: bool,
    backend: &dyn Terminal,
    backend_name: &str,
) -> Option<(i32, String)> {
    if socket_path == default_socket
        && env.get("DUNKINGSHEEP_SUPERVISED") != Some("1")
        && unit_exists
    {
        return Some((
            3,
            "dunkingsheep.service supervises this daemon; refusing an unsupervised start. Run: \
             systemctl --user start dunkingsheep.service"
                .into(),
        ));
    }
    if env.get("DUNKINGSHEEP_ALLOW_EMPTY_BACKEND") != Some("1") && backend.list_panes().is_empty() {
        return Some((
            4,
            format!(
                "refusing to start: backend {backend_name:?} sees zero panes. This daemon delivers \
                 by reading and typing into panes; with none it would accept every message and \
                 fail every delivery. Start it in an environment where the backend has panes, or \
                 set DUNKINGSHEEP_ALLOW_EMPTY_BACKEND=1 for a deliberate trial."
            ),
        ));
    }
    None
}

/// SIGTERM the daemon's whole process group (t212). A no-op unless `DUNKINGSHEEP_TEST_SPAWN=1`:
/// a lane's auto-started daemon must outlive the call that started it.
pub fn kill_own_process_group(env: &Env) -> bool {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::getpgid;
    if env.get("DUNKINGSHEEP_TEST_SPAWN") != Some("1") {
        return false;
    }
    getpgid(None)
        .and_then(|group| killpg(group, Signal::SIGTERM))
        .is_ok()
}

/// DIE WITH THE PARENT (t212): the pid to watch, or `None`. A test-spawned daemon exits when
/// the pid that spawned it goes away (121 orphans measured at load1 65.7 on 2026-10-27).
pub fn parent_to_watch(env: &Env, own_ppid: i64) -> Option<i64> {
    let explicit = env
        .get("DUNKINGSHEEP_DIE_WITH_PARENT")
        .map(str::trim)
        .unwrap_or("");
    let text = if explicit.is_empty() && env.get("DUNKINGSHEEP_TEST_SPAWN") == Some("1") {
        own_ppid.to_string()
    } else {
        explicit.to_string()
    };
    (!text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse().ok())
        .flatten()
}

/// Watch `pid` every 2 s and exit HARD when it is gone (group kill is the bound; calling a
/// graceful shutdown from here deadlocked the Python daemon, measured 12 s later still alive).
pub fn spawn_parent_watch(pid: i64, env: Env) {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    std::thread::Builder::new()
        .name("die-with-parent".into())
        .spawn(move || loop {
            std::thread::sleep(Duration::from_secs(2));
            if kill(Pid::from_raw(pid as i32), None).is_err() {
                tracing::warn!("parent pid {pid} is gone; a test-spawned daemon exits with it");
                kill_own_process_group(&env);
                std::process::exit(0);
            }
        })
        .ok();
}

/// SIGTERM/SIGINT: block them in the calling thread (call BEFORE spawning others so all inherit
/// the mask) and wait for one on a dedicated thread, which sets `stop`. No `unsafe` needed.
pub fn install_signal_stop(stop: Arc<AtomicBool>, env: Env) -> Result<(), DunkError> {
    use nix::sys::signal::{SigSet, SigmaskHow, Signal};
    let mut set = SigSet::empty();
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGINT);
    nix::sys::signal::pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&set), None)
        .map_err(|e| DunkError::Refused(format!("cannot block signals: {e}")))?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            if let Ok(signal) = set.wait() {
                tracing::info!("received signal {}, shutting down", signal as i32);
                kill_own_process_group(&env);
                stop.store(true, Ordering::SeqCst);
            }
        })
        .map(|_| ())
        .map_err(|e| DunkError::Refused(format!("cannot start the signal thread: {e}")))
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod server_tests;
