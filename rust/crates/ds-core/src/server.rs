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
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{json, Value};

use crate::config::Env;
use crate::error::{refusal, DunkError};
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
    /// Transport provenance and peer-credential result (t423). `None` is trusted in-process
    /// code; `Some(false)` is a socket request without usable peer credentials; `Some(true)` is
    /// a socket request with peer credentials.
    pub socket_peer: Option<bool>,
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
    /// The release the daemon logs on bind (`dunk_core.VERSION` in the Python line).
    version: String,
    listener: Option<UnixListener>,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<ConnectionRegistry>>,
    connections_changed: Arc<std::sync::Condvar>,
    dispatcher_closed: bool,
    shutdown_failure: Option<Arc<ShutdownFailure>>,
}

struct ConnectionWorker {
    wake: UnixStream,
    join: JoinHandle<()>,
}

struct ConnectionRegistry {
    admitting: bool,
    workers: BTreeMap<u64, ConnectionWorker>,
}

impl ConnectionRegistry {
    fn new() -> Self {
        Self {
            admitting: true,
            workers: BTreeMap::new(),
        }
    }
}

impl FlockServer {
    pub fn new(dispatcher: Arc<dyn Dispatcher>, socket_path: impl Into<PathBuf>) -> Self {
        Self::new_with_stop(dispatcher, socket_path, Arc::new(AtomicBool::new(false)))
    }

    pub fn new_with_stop(
        dispatcher: Arc<dyn Dispatcher>,
        socket_path: impl Into<PathBuf>,
        stop: Arc<AtomicBool>,
    ) -> Self {
        Self {
            dispatcher,
            socket_path: socket_path.into(),
            version: "unknown".into(),
            listener: None,
            stop,
            connections: Arc::new(Mutex::new(ConnectionRegistry::new())),
            connections_changed: Arc::new(std::sync::Condvar::new()),
            dispatcher_closed: false,
            shutdown_failure: None,
        }
    }

    /// The version named in the "listening on" log line.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
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
        let setup =
            std::fs::set_permissions(&self.socket_path, std::fs::Permissions::from_mode(0o600))
                .and_then(|()| listener.set_nonblocking(true));
        if let Err(error) = setup {
            drop(listener);
            if let Err(cleanup) = std::fs::remove_file(&self.socket_path) {
                if cleanup.kind() != std::io::ErrorKind::NotFound {
                    return Err(BindError::SetupAndCleanup {
                        setup: error,
                        cleanup,
                    });
                }
            }
            return Err(BindError::Io(error));
        }
        self.listener = Some(listener);
        tracing::info!(
            "dunkingsheep {} listening on {} (pid {})",
            self.version,
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
            if let Err(error) = reap_finished_workers(&self.connections, &self.connections_changed)
            {
                failure = Some(error);
                break;
            }
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
                    if let Err(error) = admit_connection(dispatcher, conn, id, stop, connections) {
                        failure = Some(error);
                        break;
                    }
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
        let close_result = self.close();
        match (failure, close_result) {
            (Some(error), Ok(())) => Err(error),
            (Some(serve), Err(shutdown)) => Err(BindError::ServeAndShutdown {
                serve: Box::new(serve),
                shutdown: Box::new(shutdown),
            }),
            (None, result) => result,
        }
    }

    pub fn close(&mut self) -> Result<(), BindError> {
        if let Some(failure) = &self.shutdown_failure {
            return Err(BindError::Shutdown(Arc::clone(failure)));
        }
        self.stop.store(true, Ordering::SeqCst);
        self.listener = None;
        let (panicked_workers, wake_errors, registry_poisoned) =
            drain_connections(&self.connections, &self.connections_changed);
        let dispatcher_panicked = if !self.dispatcher_closed {
            let result = catch_unwind(AssertUnwindSafe(|| self.dispatcher.close())).is_err();
            self.dispatcher_closed = true;
            result
        } else {
            false
        };
        let socket_error = match std::fs::remove_file(&self.socket_path) {
            Ok(()) => None,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => Some(error),
        };
        tracing::info!("daemon stopped");
        let failure = ShutdownFailure {
            worker_panics: panicked_workers,
            worker_wake_errors: wake_errors
                .into_iter()
                .map(|(id, error)| (id, error.to_string()))
                .collect(),
            dispatcher_panicked,
            registry_poisoned,
            socket_error: socket_error.map(|error| error.to_string()),
        };
        if failure.has_errors() {
            let failure = Arc::new(failure);
            self.shutdown_failure = Some(Arc::clone(&failure));
            return Err(BindError::Shutdown(failure));
        }
        Ok(())
    }

    /// Return (response, stop_after). The socket peer is the short-lived CLI; the caller
    /// identity is its nearest agent ancestor (or session leader), resolved by `proc` and
    /// attached to task leases for `/proc` liveness.
    pub fn handle_line(&self, line: &str, peer_pid: Option<i64>) -> (Response, bool) {
        handle_line(self.dispatcher.as_ref(), line, peer_pid)
    }
}

/// Terminal cleanup errors are cached so a repeated close cannot hide a failed shutdown.
#[derive(Debug, Clone)]
pub struct ShutdownFailure {
    pub worker_panics: Vec<u64>,
    pub worker_wake_errors: Vec<(u64, String)>,
    pub dispatcher_panicked: bool,
    pub registry_poisoned: bool,
    pub socket_error: Option<String>,
}

impl ShutdownFailure {
    fn has_errors(&self) -> bool {
        !self.worker_panics.is_empty()
            || !self.worker_wake_errors.is_empty()
            || self.dispatcher_panicked
            || self.registry_poisoned
            || self.socket_error.is_some()
    }
}

#[derive(Debug)]
pub enum BindError {
    AlreadyRunning(AlreadyRunning),
    Io(std::io::Error),
    SetupAndCleanup {
        setup: std::io::Error,
        cleanup: std::io::Error,
    },
    RegistryPoisoned,
    WorkerPanicked(Vec<u64>),
    Shutdown(Arc<ShutdownFailure>),
    ServeAndShutdown {
        serve: Box<BindError>,
        shutdown: Box<BindError>,
    },
}

impl std::fmt::Display for BindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BindError::AlreadyRunning(error) => error.fmt(f),
            BindError::Io(error) => error.fmt(f),
            BindError::SetupAndCleanup { setup, cleanup } => {
                write!(f, "socket setup failed ({setup}); socket rollback failed ({cleanup})")
            }
            BindError::RegistryPoisoned => f.write_str("connection registry lock was poisoned"),
            BindError::WorkerPanicked(ids) => write!(f, "connection workers panicked: {ids:?}"),
            BindError::Shutdown(failure) => write!(
                f,
                "shutdown incomplete: worker panics={:?}, worker wake errors={:?}, dispatcher panicked={}, registry poisoned={}, socket cleanup error={:?}",
                failure.worker_panics,
                failure.worker_wake_errors,
                failure.dispatcher_panicked,
                failure.registry_poisoned,
                failure.socket_error
            ),
            BindError::ServeAndShutdown { serve, shutdown } => {
                write!(f, "serve failed ({serve}); shutdown also failed ({shutdown})")
            }
        }
    }
}

impl std::error::Error for BindError {}

fn lock_registry(
    registry: &Mutex<ConnectionRegistry>,
) -> Result<MutexGuard<'_, ConnectionRegistry>, BindError> {
    registry.lock().map_err(|_| BindError::RegistryPoisoned)
}

fn take_workers(registry: &mut ConnectionRegistry) -> BTreeMap<u64, ConnectionWorker> {
    std::mem::take(&mut registry.workers)
}

fn reap_finished_workers(
    registry: &Mutex<ConnectionRegistry>,
    changed: &std::sync::Condvar,
) -> Result<(), BindError> {
    let finished = {
        let mut registry = lock_registry(registry)?;
        let ids: Vec<_> = registry
            .workers
            .iter()
            .filter_map(|(id, worker)| worker.join.is_finished().then_some(*id))
            .collect();
        let finished = ids
            .into_iter()
            .filter_map(|id| registry.workers.remove(&id).map(|worker| (id, worker)))
            .collect::<Vec<_>>();
        if !finished.is_empty() {
            changed.notify_all();
        }
        finished
    };
    let panicked: Vec<_> = finished
        .into_iter()
        .filter_map(|(id, worker)| worker.join.join().is_err().then_some(id))
        .collect();
    if panicked.is_empty() {
        Ok(())
    } else {
        Err(BindError::WorkerPanicked(panicked))
    }
}

fn drain_connections(
    registry: &Mutex<ConnectionRegistry>,
    changed: &std::sync::Condvar,
) -> (Vec<u64>, Vec<(u64, std::io::Error)>, bool) {
    let (workers, poisoned) = match registry.lock() {
        Ok(mut registry) => {
            registry.admitting = false;
            (take_workers(&mut registry), false)
        }
        Err(poisoned) => {
            let mut registry = poisoned.into_inner();
            registry.admitting = false;
            (take_workers(&mut registry), true)
        }
    };
    changed.notify_all();
    let mut wake_errors = Vec::new();
    for (id, worker) in &workers {
        // Wake only blocked reads. Active handlers may still need this connection's write
        // half to deliver their response before their worker can be joined.
        if let Err(error) = worker.wake.shutdown(std::net::Shutdown::Read) {
            wake_errors.push((*id, error));
        }
    }
    let mut panicked_workers = Vec::new();
    for (id, worker) in workers {
        if worker.join.join().is_err() {
            panicked_workers.push(id);
        }
    }
    (panicked_workers, wake_errors, poisoned)
}

fn admit_connection(
    dispatcher: Arc<dyn Dispatcher>,
    conn: UnixStream,
    id: u64,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<ConnectionRegistry>>,
) -> Result<(), BindError> {
    let mut registry = lock_registry(&connections)?;
    if !registry.admitting || stop.load(Ordering::SeqCst) {
        return Ok(());
    }
    let wake = conn.try_clone().map_err(BindError::Io)?;
    let _ = conn.set_nonblocking(false);
    let log = crate::daemon_log::ThreadLog::capture();
    let join = std::thread::Builder::new()
        .name(format!("dunk-connection-{id}"))
        .spawn(move || {
            log.adopt();
            serve_connection(dispatcher, conn, stop)
        })
        .map_err(BindError::Io)?;
    registry.workers.insert(id, ConnectionWorker { wake, join });
    Ok(())
}

fn serve_connection(dispatcher: Arc<dyn Dispatcher>, conn: UnixStream, stop: Arc<AtomicBool>) {
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
    let request: Value = match crate::wire_json::from_str(line) {
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
        socket_peer: Some(peer_pid.is_some()),
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
        // A backend that cannot answer does not contradict the claim. This is the deliberate
        // no-backend trial (DUNKINGSHEEP_ALLOW_EMPTY_BACKEND), which the serve test pins. Only a
        // listing that answers without the pane refuses (t3368), see the Ok(None) arm above.
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

/// Python-compatible daemon gate that preserves fallible backend enumeration.
/// The caller runs the supervised default-socket check first, before constructing
/// stores or otherwise producing startup side effects.
pub fn startup_refusal_fallible(
    env: &Env,
    backend: &dyn Terminal,
    backend_name: &str,
) -> Result<Option<(i32, String)>, DunkError> {
    if env.get("DUNKINGSHEEP_ALLOW_EMPTY_BACKEND") == Some("1") {
        return Ok(None);
    }
    let panes = match backend.try_list_panes() {
        Ok(panes) => panes,
        Err(error) => {
            return Ok(Some((
                4,
                format!("could not list backend {backend_name:?} panes: {error}"),
            )));
        }
    };
    if panes.is_empty() {
        return Ok(Some((
            4,
            format!(
                "refusing to start: backend {backend_name:?} sees zero panes. This daemon delivers \
                 by reading and typing into panes; with none it would accept every message and \
                 fail every delivery. Start it in an environment where the backend has panes, or \
                 set DUNKINGSHEEP_ALLOW_EMPTY_BACKEND=1 for a deliberate trial."
            ),
        )));
    }
    Ok(None)
}

/// Gate the supervised default socket without constructing or probing the backend.
pub fn supervised_default_socket_refusal(
    env: &Env,
    socket_path: &Path,
    default_socket: &Path,
    unit_exists: bool,
) -> Option<(i32, String)> {
    (socket_path == default_socket
        && env.get("DUNKINGSHEEP_SUPERVISED") != Some("1")
        && unit_exists)
        .then(|| {
            (
                3,
                "dunkingsheep.service supervises this daemon; refusing an unsupervised start. Run: \
                 systemctl --user start dunkingsheep.service"
                    .into(),
            )
        })
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

/// Watch one startup-selected parent, retaining a cancellable join for normal daemon close.
/// When the parent actually disappears, preserve Python's hard process exit behavior.
pub struct ParentWatch {
    stop: Arc<(Mutex<bool>, std::sync::Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}

pub fn spawn_parent_watch(pid: i64, env: Env) -> Result<ParentWatch, DunkError> {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    let stop = Arc::new((Mutex::new(false), std::sync::Condvar::new()));
    let worker_stop = Arc::clone(&stop);
    let log = crate::daemon_log::ThreadLog::capture();
    let worker = std::thread::Builder::new()
        .name("die-with-parent".into())
        .spawn(move || {
            log.adopt();
            loop {
                let (lock, changed) = &*worker_stop;
                let stopping = lock.lock().unwrap_or_else(|error| error.into_inner());
                let (stopping, _) = changed
                    .wait_timeout_while(stopping, Duration::from_secs(2), |stopping| !*stopping)
                    .unwrap_or_else(|error| error.into_inner());
                if *stopping {
                    return;
                }
                drop(stopping);
                if kill(Pid::from_raw(pid as i32), None).is_err() {
                    tracing::warn!("parent pid {pid} is gone; a test-spawned daemon exits with it");
                    kill_own_process_group(&env);
                    std::process::exit(0);
                }
            }
        })
        .map_err(|error| refusal(format!("cannot start parent watcher: {error}")))?;
    Ok(ParentWatch {
        stop,
        worker: Some(worker),
    })
}

impl ParentWatch {
    pub fn join(mut self) -> Result<(), DunkError> {
        self.stop_and_join()
    }

    fn stop_and_join(&mut self) -> Result<(), DunkError> {
        if let Some(worker) = self.worker.take() {
            let (lock, changed) = &*self.stop;
            *lock.lock().unwrap_or_else(|error| error.into_inner()) = true;
            changed.notify_all();
            worker
                .join()
                .map_err(|_| refusal("parent watcher panicked"))?;
        }
        Ok(())
    }
}

impl Drop for ParentWatch {
    fn drop(&mut self) {
        if let Err(error) = self.stop_and_join() {
            tracing::error!("{error}");
        }
    }
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
    let log = crate::daemon_log::ThreadLog::capture();
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            log.adopt();
            if let Ok(signal) = set.wait() {
                tracing::info!("received signal {}, shutting down", signal as i32);
                kill_own_process_group(&env);
                stop.store(true, Ordering::SeqCst);
            }
        })
        .map(|_| ())
        .map_err(|e| DunkError::Refused(format!("cannot start the signal thread: {e}")))
}

/// Stoppable signal waiter used by the owning daemon. SIGURG is reserved as its private
/// cancellation wake; SIGTERM/SIGINT still request the server's ordinary drain path.
pub struct SignalStop {
    stop: Arc<AtomicBool>,
    waiter: Option<std::thread::JoinHandle<Result<(), String>>>,
    previous_mask: nix::sys::signal::SigSet,
}

impl SignalStop {
    pub fn install(env: Env) -> Result<Self, DunkError> {
        use nix::sys::signal::{SigSet, SigmaskHow, Signal};
        let mut set = SigSet::empty();
        set.add(Signal::SIGTERM);
        set.add(Signal::SIGINT);
        set.add(Signal::SIGURG);
        let mut previous_mask = SigSet::empty();
        nix::sys::signal::pthread_sigmask(
            SigmaskHow::SIG_BLOCK,
            Some(&set),
            Some(&mut previous_mask),
        )
        .map_err(|error| refusal(format!("cannot block daemon signals: {error}")))?;
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let log = crate::daemon_log::ThreadLog::capture();
        let waiter = std::thread::Builder::new()
            .name("daemon-signals".into())
            .spawn(move || {
                log.adopt();
                loop {
                    match set.wait() {
                        Ok(Signal::SIGURG) => return Ok(()),
                        Ok(signal) => {
                            tracing::info!("received signal {}, shutting down", signal as i32);
                            kill_own_process_group(&env);
                            worker_stop.store(true, Ordering::SeqCst);
                        }
                        Err(error) => return Err(error.to_string()),
                    }
                }
            });
        let waiter = match waiter {
            Ok(waiter) => waiter,
            Err(error) => {
                nix::sys::signal::pthread_sigmask(
                    SigmaskHow::SIG_SETMASK,
                    Some(&previous_mask),
                    None,
                )
                .map_err(|restore| {
                    refusal(format!(
                        "cannot start daemon signal waiter ({error}); cannot restore signal mask: {restore}"
                    ))
                })?;
                return Err(refusal(format!(
                    "cannot start daemon signal waiter: {error}"
                )));
            }
        };
        Ok(Self {
            stop,
            waiter: Some(waiter),
            previous_mask,
        })
    }

    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    pub fn join(mut self) -> Result<(), DunkError> {
        self.cancel_and_join()
    }

    fn cancel_and_join(&mut self) -> Result<(), DunkError> {
        if let Some(waiter) = self.waiter.take() {
            use nix::sys::signal::{kill, SigmaskHow, Signal};
            use nix::unistd::Pid;
            let mut failures = Vec::new();
            if let Err(error) = kill(Pid::this(), Signal::SIGURG) {
                failures.push(format!("cannot stop daemon signal waiter: {error}"));
            }
            match waiter.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failures.push(format!("daemon signal waiter failed: {error}")),
                Err(_) => failures.push("daemon signal waiter panicked".to_owned()),
            }
            if let Err(error) = nix::sys::signal::pthread_sigmask(
                SigmaskHow::SIG_SETMASK,
                Some(&self.previous_mask),
                None,
            ) {
                failures.push(format!("cannot restore daemon signal mask: {error}"));
            }
            if !failures.is_empty() {
                return Err(refusal(failures.join("; ")));
            }
        }
        Ok(())
    }
}

impl Drop for SignalStop {
    fn drop(&mut self) {
        if let Err(error) = self.cancel_and_join() {
            tracing::error!("{error}");
        }
    }
}

#[cfg(test)]
#[path = "server_tests.rs"]
mod server_tests;
