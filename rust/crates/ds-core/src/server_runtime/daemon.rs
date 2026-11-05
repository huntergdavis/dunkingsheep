//! Canonical process assembly for the Rust daemon.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use std::{fs::OpenOptions, io::Write};

use crate::config::{CapturedPythonHome, Env};
use crate::error::{refusal, DunkError};
use crate::flock::loop_env::{configured_loop_terminal_factory_from, LoopTerminalFactory};
use crate::flock::loops::{LoopIntervals, LoopSet};
use crate::flock::panes::Stores;
use crate::roles::Roles;
use crate::server::{startup_refusal_fallible, supervised_default_socket_refusal, FlockServer};
use crate::server_runtime::{
    RuntimeMemoryConfig, RuntimeObservability, RuntimeServiceInputs, RuntimeServices,
};
use crate::superusers::Superusers;
use crate::tasks::Tasks;

/// Explicit per-invocation overrides. `None` follows the Python daemon defaults.
#[derive(Debug, Clone, Default)]
pub struct DaemonOptions {
    pub socket_path: Option<PathBuf>,
    pub state_file: Option<PathBuf>,
    pub no_log_file: bool,
}

/// Captured startup values that can be supplied by isolated fixtures without mutating
/// process-global environment variables. Production uses [`run_daemon`].
pub struct DaemonStartup {
    pub env: Env,
    pub home: CapturedPythonHome,
    pub executable: PathBuf,
    pub release_version: String,
    pub terminal_factory: Option<LoopTerminalFactory>,
    pub loop_intervals: Option<LoopIntervals>,
    /// Isolated unit fixtures disable process signal registration and supply stop through
    /// a controlled child harness. Production always enables the real signal waiter.
    pub install_signals: bool,
    /// Test-only startup synchronization; production leaves it unset.
    pub ready: Option<std::sync::mpsc::SyncSender<()>>,
    #[cfg(test)]
    pub loop_test_gate: Option<crate::server_runtime::LoopTestGate>,
    #[cfg(test)]
    pub stop_test_handle: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// Build and run the one production daemon using one captured process environment.
pub fn run_daemon(options: DaemonOptions) -> Result<i32, DunkError> {
    let env = Env::try_from_process()?;
    let home = CapturedPythonHome::capture(&env);
    let executable = std::env::current_exe().map_err(|error| {
        refusal(format!(
            "could not resolve daemon executable identity: {error}"
        ))
    })?;
    let release_version = env
        .get("DUNKINGSHEEP_VERSION")
        .or(option_env!("CARGO_PKG_VERSION"))
        .unwrap_or("unknown")
        .to_owned();
    run_daemon_captured(
        options,
        DaemonStartup {
            env,
            home,
            executable,
            release_version,
            terminal_factory: None,
            loop_intervals: None,
            install_signals: true,
            ready: None,
            #[cfg(test)]
            loop_test_gate: None,
            #[cfg(test)]
            stop_test_handle: None,
        },
    )
}

/// The fixture-friendly entry shares the production assembly after capture.
pub fn run_daemon_captured(
    options: DaemonOptions,
    startup: DaemonStartup,
) -> Result<i32, DunkError> {
    let env = &startup.env;
    let config_dir = common_config_dir(env, &startup.home)?;
    let mut log = DaemonLog::configure(&config_dir, options.no_log_file);
    let default_socket = config_dir.join("dunkingsheep.sock");
    let socket_path = options
        .socket_path
        .filter(|path| !path.as_os_str().is_empty())
        .or_else(|| {
            env.get("DUNKINGSHEEP_SOCKET")
                .filter(|s| !s.is_empty())
                .map(PathBuf::from)
        })
        .unwrap_or_else(|| default_socket.clone());

    let unit_root = startup
        .home
        .expanduser_home()
        .map_err(|error| refusal(format!("could not expand systemd user unit path: {error}")))?
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("~"));
    let unit_path = unit_root.join(".config/systemd/user/dunkingsheep.service");
    if let Some((code, message)) =
        supervised_default_socket_refusal(env, &socket_path, &default_socket, unit_path.exists())
    {
        log.error(&message);
        return Ok(code);
    }

    let terminal_factory = match startup.terminal_factory {
        Some(factory) => factory,
        None => configured_loop_terminal_factory_from(env)?,
    };
    let loop_intervals = match startup.loop_intervals {
        Some(intervals) => intervals,
        None => captured_loop_intervals(env)?,
    };

    // Backend enumeration is after the default-socket refusal and before every store open.
    let backend = terminal_factory();
    if let Some((code, message)) = startup_refusal_fallible(
        env,
        backend.as_ref(),
        crate::server_runtime::DEFAULT_TERMINAL_BACKEND,
    )? {
        log.error(&message);
        return Ok(code);
    }
    drop(backend);

    // Store parents are created only after the supervised and backend refusal gates.
    std::fs::create_dir_all(&config_dir)
        .map_err(|error| refusal(format!("could not create daemon config directory: {error}")))?;

    let state_file = options
        .state_file
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| config_dir.join("dunks.json"));
    let state_parent = state_file.parent().unwrap_or(Path::new("."));
    let state_dir = if state_parent.is_absolute() {
        state_parent.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| refusal(format!("could not resolve state directory: {error}")))?
            .join(state_parent)
    };
    let roles_paths = roles_paths(env, &startup.home)?;
    let superusers_path = config_dir.join("superusers.json");
    let owner_path = owner_path(env, &startup.home)?;

    // Signal installation must precede the runtime and the synchronous store writers,
    // because their threads inherit the process signal mask.
    let signal_stop = startup
        .install_signals
        .then(|| crate::server::SignalStop::install(env.clone()))
        .transpose()?;
    let stop = signal_stop
        .as_ref()
        .map(crate::server::SignalStop::stop_handle);
    #[cfg(test)]
    let fallback_stop = startup
        .stop_test_handle
        .clone()
        .unwrap_or(Arc::new(std::sync::atomic::AtomicBool::new(false)));
    #[cfg(not(test))]
    let fallback_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stop = stop.unwrap_or(fallback_stop);

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| refusal(format!("could not create daemon runtime: {error}")))?;
    let tasks = Arc::new(runtime.block_on(Tasks::open(config_dir.join("tasks.db")))?);
    // AllocationStore.__init__ creates the canonical tables on this same task-store actor.
    runtime.block_on(tasks.allocation_init())?;
    let inbox = Arc::new(crate::inbox_store::Store::open(
        config_dir.join("inbox.db"),
        config_dir.join("attachments"),
    )?);
    let roles = Roles::new(env, Some(roles_paths.0), Some(roles_paths.1), None)?;
    let superusers = Superusers::new(superusers_path, true)?;
    let snapshot = read_snapshot(&state_file);
    let memory_path = config_dir.join("memory.db");
    let services = RuntimeServices::new_with_memory_and_snapshot(
        RuntimeServiceInputs {
            launches_file: config_dir.join("launches.json"),
            lane_registry_file: config_dir.join("lanes.json"),
            persist_enabled: true,
            stores: Stores {
                tasks: Some(tasks),
                inbox: Some(inbox),
                ..Stores::default()
            },
            roles,
            legacy_superusers: Some(superusers),
            release_version: startup.release_version,
            state_file,
            executable: startup.executable,
            env: env.clone(),
            observability: Some(RuntimeObservability {
                state_dir,
                memory_db: Some(memory_path),
            }),
            runtime,
            terminal_factory,
        },
        Some(RuntimeMemoryConfig::captured(
            config_dir.join("memory.db"),
            env,
        )?),
        snapshot,
    )?;
    services.attach_owner_push_path(owner_path)?;

    let mut loops = LoopSet::default();
    {
        let _entered = services.runtime_handle().enter();
        let loop_env = services.push_loop_adapter()?;
        #[cfg(test)]
        let loop_env = match startup.loop_test_gate.clone() {
            Some(gate) => loop_env.with_test_gate(gate),
            None => loop_env,
        };
        let loop_env = Arc::new(loop_env);
        if !loops.start(Arc::clone(services.flock()), loop_env, loop_intervals) {
            let mut failures = vec!["daemon loops could not be started exactly once".to_owned()];
            if let Err(error) = services.runtime_handle().block_on(loops.stop()) {
                failures.push(format!("loop rollback failed: {error:?}"));
            }
            if let Err(error) = services.shutdown_checked() {
                failures.push(format!("service rollback failed: {error}"));
            }
            return Err(refusal(failures.join("; ")));
        }
    }
    let parent_watch =
        match crate::server::parent_to_watch(env, i64::from(nix::unistd::getppid().as_raw())) {
            Some(pid) => match crate::server::spawn_parent_watch(pid, env.clone()) {
                Ok(watcher) => Some(watcher),
                Err(error) => {
                    let loop_result = services.runtime_handle().block_on(loops.stop());
                    let shutdown = services.shutdown_checked();
                    let mut failures = vec![format!("could not start parent watcher: {error}")];
                    if let Err(error) = loop_result {
                        failures.push(format!("loop rollback failed: {error:?}"));
                    }
                    if let Err(error) = shutdown {
                        failures.push(format!("service rollback failed: {error}"));
                    }
                    return Err(refusal(failures.join("; ")));
                }
            },
            None => None,
        };
    let mut server =
        FlockServer::new_with_stop(services.production_dispatcher(), socket_path, stop);
    if let Err(error) = server.bind() {
        let loop_result = services.runtime_handle().block_on(loops.stop());
        let shutdown = services.shutdown_checked();
        let already_running = matches!(&error, crate::server::BindError::AlreadyRunning(_));
        let mut failures = if already_running {
            Vec::new()
        } else {
            vec![format!("could not bind daemon socket: {error}")]
        };
        if let Err(join_errors) = loop_result {
            failures.push(format!(
                "daemon loop startup cleanup failed: {join_errors:?}"
            ));
        }
        if let Err(error) = shutdown {
            failures.push(format!("runtime startup rollback failed: {error}"));
        }
        if let Some(parent_watch) = parent_watch {
            if let Err(error) = parent_watch.join() {
                failures.push(format!("parent watcher rollback failed: {error}"));
            }
        }
        if already_running && failures.is_empty() {
            log.error(&error.to_string());
            return Ok(3);
        }
        return Err(refusal(failures.join("; ")));
    }
    if let Some(ready) = startup.ready {
        if ready.send(()).is_err() {
            server.shutdown();
            let server_close = server.close();
            let loop_result = services.runtime_handle().block_on(loops.stop());
            let shutdown = services.shutdown_checked();
            let mut failures = vec!["daemon startup observer disconnected".to_owned()];
            if let Err(error) = loop_result {
                failures.push(format!("loop rollback failed: {error:?}"));
            }
            if let Err(error) = shutdown {
                failures.push(format!("service rollback failed: {error}"));
            }
            if let Err(error) = server_close {
                failures.push(format!("server rollback failed: {error}"));
            }
            if let Some(parent_watch) = parent_watch {
                if let Err(error) = parent_watch.join() {
                    failures.push(format!("parent watcher rollback failed: {error}"));
                }
            }
            return Err(refusal(failures.join("; ")));
        }
    }
    let serve = server.serve_forever();
    let loop_result = services.runtime_handle().block_on(loops.stop());
    let shutdown = services.shutdown_checked();
    let mut failures = Vec::new();
    if let Err(join_errors) = loop_result {
        failures.push(format!("daemon loop shutdown failed: {join_errors:?}"));
    }
    if let Err(error) = shutdown {
        failures.push(format!("runtime shutdown failed: {error}"));
    }
    if let Err(error) = serve {
        failures.push(format!("daemon serve failed: {error}"));
    }
    if let Some(signal_stop) = signal_stop {
        if let Err(error) = signal_stop.join() {
            failures.push(format!("signal waiter shutdown failed: {error}"));
        }
    }
    if let Some(parent_watch) = parent_watch {
        if let Err(error) = parent_watch.join() {
            failures.push(format!("parent watcher shutdown failed: {error}"));
        }
    }
    if !failures.is_empty() {
        return Err(refusal(failures.join("; ")));
    }
    Ok(0)
}

struct DaemonLog(Option<std::fs::File>);

impl DaemonLog {
    fn configure(config_dir: &Path, no_log_file: bool) -> Self {
        if no_log_file {
            return Self(None);
        }
        use std::os::unix::fs::OpenOptionsExt;
        let path = config_dir.join("daemon.log");
        let file = std::fs::create_dir_all(config_dir)
            .and_then(|()| {
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .mode(0o600)
                    .open(&path)
            })
            .ok();
        Self(file)
    }

    fn error(&mut self, message: &str) {
        if let Some(file) = &mut self.0 {
            let _ = writeln!(file, "ERROR {message}");
        } else {
            eprintln!("{message}");
        }
    }
}

fn common_config_dir(env: &Env, home: &CapturedPythonHome) -> Result<PathBuf, DunkError> {
    if let Some(path) = env.get("DUNKINGSHEEP_DIR").filter(|path| !path.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let default = match home
        .expanduser_home()
        .map_err(|error| refusal(format!("cannot expand config directory: {error}")))?
    {
        Some(home) => home.join(".config/dunkingsheep"),
        None => PathBuf::from("~/.config/dunkingsheep"),
    };
    Ok(default)
}

fn roles_paths(env: &Env, home: &CapturedPythonHome) -> Result<(PathBuf, PathBuf), DunkError> {
    // dunk_roles preserves a present empty DUNKINGSHEEP_DIR as Path(""); its two explicit
    // file overrides remain truthy-only. Keep that consumer-specific behavior intact.
    let directory = env.get("DUNKINGSHEEP_DIR").map(PathBuf::from).unwrap_or(
        home.path()
            .map_err(|error| refusal(format!("cannot resolve roles config directory: {error}")))?
            .join(".config/dunkingsheep"),
    );
    let roles = env
        .get("DUNKINGSHEEP_ROLES_FILE")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| directory.join("roles.json"));
    let tokens = env
        .get("DUNKINGSHEEP_TOKENS_FILE")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| directory.join("service-tokens.json"));
    Ok((roles, tokens))
}

fn owner_path(env: &Env, home: &CapturedPythonHome) -> Result<PathBuf, DunkError> {
    let directory = match env.get("DUNKINGSHEEP_DIR") {
        Some(config_dir) => PathBuf::from(config_dir),
        None => match home
            .expanduser_home()
            .map_err(|error| refusal(format!("cannot expand owner config directory: {error}")))?
        {
            Some(home) => home.join(".config/dunkingsheep"),
            None => PathBuf::from("~/.config/dunkingsheep"),
        },
    };
    Ok(directory.join("owner-push.db"))
}

fn read_snapshot(path: &Path) -> Option<serde_json::Value> {
    match std::fs::read(path) {
        Ok(bytes) => match serde_json::from_slice(&bytes) {
            Ok(value) => Some(value),
            Err(error) => {
                tracing::error!("could not restore {}: {error}", path.display());
                None
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            tracing::error!("could not restore {}: {error}", path.display());
            None
        }
    }
}

fn captured_loop_intervals(env: &Env) -> Result<LoopIntervals, DunkError> {
    let duration = |name: &str, fallback: f64| -> Result<Duration, DunkError> {
        let seconds = match env.get(name) {
            Some(raw) => crate::github::parse_python_float(raw)
                .ok_or_else(|| refusal(format!("invalid loop interval in {name}")))?,
            None => fallback,
        };
        if !seconds.is_finite() || seconds <= 0.0 {
            return Err(refusal(format!("loop interval {name} must be positive")));
        }
        Ok(Duration::from_secs_f64(seconds))
    };
    Ok(LoopIntervals {
        push: duration("DUNKINGSHEEP_PUSH_S", 10.0)?,
        stuck: Duration::from_secs_f64(crate::flock::loops_stuck::STUCK_OBSERVE_TICK_S),
        reaper: duration("DUNKINGSHEEP_REAP_S", 15.0)?,
    })
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::Shutdown;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::mpsc;

    use super::*;

    fn daemon_call(
        socket: &Path,
        id: u64,
        command: &str,
        args: serde_json::Value,
        service_token: Option<&str>,
    ) -> serde_json::Value {
        daemon_call_with_identity(socket, id, command, args, service_token, None)
    }

    fn daemon_call_with_identity(
        socket: &Path,
        id: u64,
        command: &str,
        args: serde_json::Value,
        service_token: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> serde_json::Value {
        let (response, mut reader) = daemon_call_with_identity_response(
            socket,
            id,
            command,
            args,
            service_token,
            self_pane_id,
        );
        let mut remainder = Vec::new();
        reader
            .read_to_end(&mut remainder)
            .expect("response connection reaches EOF");
        assert!(remainder.is_empty(), "unexpected extra response bytes");
        response
    }

    fn daemon_call_with_identity_response(
        socket: &Path,
        id: u64,
        command: &str,
        args: serde_json::Value,
        service_token: Option<&str>,
        self_pane_id: Option<&str>,
    ) -> (serde_json::Value, BufReader<UnixStream>) {
        let mut request = serde_json::json!({
            "id": id,
            "cmd": command,
            "args": args,
        });
        if let Some(token) = service_token {
            request["service_token"] = serde_json::Value::String(token.to_owned());
        }
        if let Some(pane_id) = self_pane_id {
            request["self_pane_id"] = serde_json::Value::String(pane_id.to_owned());
        }
        let mut stream = UnixStream::connect(socket).expect("connect production socket");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("set request timeout");
        writeln!(stream, "{request}").expect("write request");
        stream
            .shutdown(Shutdown::Write)
            .expect("finish request input");
        let mut response = String::new();
        let mut reader = BufReader::new(stream);
        reader
            .read_line(&mut response)
            .expect("read production response");
        (
            serde_json::from_str(&response).expect("decode production response"),
            reader,
        )
    }

    fn request_shutdown_for_cleanup(socket: &Path, token: &str) -> bool {
        let Ok(mut stream) = UnixStream::connect(socket) else {
            return false;
        };
        if stream
            .set_read_timeout(Some(Duration::from_secs(1)))
            .is_err()
        {
            return false;
        }
        let request = serde_json::json!({
            "id": "cleanup-stop",
            "cmd": "shutdown",
            "args": {},
            "service_token": token,
        });
        if writeln!(stream, "{request}").is_err() || stream.shutdown(Shutdown::Write).is_err() {
            return false;
        }
        let mut response = String::new();
        BufReader::new(stream)
            .read_line(&mut response)
            .ok()
            .filter(|count| *count > 0)
            .and_then(|_| serde_json::from_str::<serde_json::Value>(&response).ok())
            .is_some_and(|response| response["ok"] == true)
    }

    struct DaemonCleanupGuard {
        handle: Option<std::thread::JoinHandle<Result<i32, DunkError>>>,
        active_request: Option<std::thread::JoinHandle<serde_json::Value>>,
        finished: mpsc::Receiver<()>,
        socket: PathBuf,
        token: String,
        release_request: Option<mpsc::Sender<()>>,
        release_loop: Option<mpsc::Sender<()>>,
        stop: Arc<std::sync::atomic::AtomicBool>,
    }

    impl Drop for DaemonCleanupGuard {
        fn drop(&mut self) {
            if self.handle.is_none() {
                return;
            }
            if let Some(release) = self.release_request.take() {
                let _ = release.send(());
            }
            if let Some(release) = self.release_loop.take() {
                let _ = release.send(());
            }
            if !request_shutdown_for_cleanup(&self.socket, &self.token) {
                self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if self.finished.recv_timeout(Duration::from_secs(2)).is_err() {
                self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
                let _ = UnixStream::connect(&self.socket);
                let _ = self.finished.recv_timeout(Duration::from_secs(2));
            }
            if let Some(active_request) = self.active_request.take() {
                let _ = active_request.join();
            }
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[test]
    fn run_daemon_starts_workers_serves_and_joins_before_close() {
        let dir = crate::testsupport::dir("run-daemon-lifecycle");
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).expect("config directory for token fixture");
        let socket = dir.path().join("daemon.sock");
        let shutdown_token = "daemon-test-owner-token";
        let token_hash = crate::roles::hash_token(shutdown_token);
        let mut tokens = serde_json::Map::new();
        tokens.insert(
            token_hash,
            serde_json::json!({"role": "owner", "label": "daemon-test"}),
        );
        std::fs::write(
            config.join("service-tokens.json"),
            serde_json::json!({"tokens": tokens}).to_string(),
        )
        .expect("owner token fixture");
        let config_text = config.to_str().expect("UTF-8 fixture path");
        let home_text = dir.path().to_str().expect("UTF-8 fixture home");
        let env = Env::from_pairs(&[
            ("DUNKINGSHEEP_DIR", config_text),
            ("DUNKINGSHEEP_SOCKET", socket.to_str().unwrap()),
            ("DUNKINGSHEEP_SUPERVISED", "1"),
            ("HOME", home_text),
        ]);
        let home = CapturedPythonHome::capture_with(&env, || Ok(None));
        let request_block = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (request_entered_tx, request_entered_rx) = mpsc::channel();
        let (request_release_tx, request_release_rx) = mpsc::channel();
        let request_release_rx = Arc::new(std::sync::Mutex::new(request_release_rx));
        let factory_request_block = Arc::clone(&request_block);
        let factory_request_entered = request_entered_tx;
        let factory_request_release = Arc::clone(&request_release_rx);
        let caller =
            crate::proc::caller_from_peer(Some(std::process::id() as i64), Some("w1:p1".into()));
        assert!(
            crate::proc::caller_alive(Some(&caller)),
            "test caller is live"
        );
        let caller_root_pid = caller.pid.expect("resolved caller root pid");
        let terminal_factory: LoopTerminalFactory = Arc::new(move || {
            let is_connection = std::thread::current()
                .name()
                .is_some_and(|name| name.starts_with("dunk-connection-"));
            if is_connection
                && factory_request_block.swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                factory_request_entered
                    .send(())
                    .expect("request gate observer remains connected");
                factory_request_release
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .recv()
                    .expect("active request gate release");
            }
            let mut terminal = crate::terminal::fakes::FakeTerminal::new();
            for pane in &mut terminal.panes {
                pane.pane_pid = Some(caller_root_pid);
            }
            Box::new(terminal)
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel(0);
        let (loop_entered_tx, loop_entered_rx) = mpsc::channel();
        let (loop_release_tx, loop_release_rx) = mpsc::channel();
        let (loop_completed_tx, loop_completed_rx) = mpsc::channel();
        let (loop_panes_tx, loop_panes_rx) = mpsc::channel();
        let loop_test_gate = Arc::new(std::sync::Mutex::new(Some((
            loop_entered_tx,
            loop_release_rx,
            loop_completed_tx,
            loop_panes_tx,
        ))));
        let stop_test_handle = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let startup = DaemonStartup {
            env,
            home,
            executable: std::env::current_exe().expect("test executable"),
            release_version: "test".into(),
            terminal_factory: Some(terminal_factory),
            loop_intervals: Some(LoopIntervals {
                push: Duration::from_millis(5),
                stuck: Duration::from_secs(60),
                reaper: Duration::from_secs(60),
            }),
            install_signals: false,
            ready: Some(ready_tx),
            loop_test_gate: Some(loop_test_gate),
            stop_test_handle: Some(Arc::clone(&stop_test_handle)),
        };
        let daemon_socket = socket.clone();
        let state_file = dir.path().join("dunks.json");
        let (finished_tx, finished_rx) = mpsc::channel();
        let daemon = std::thread::spawn(move || {
            let result = run_daemon_captured(
                DaemonOptions {
                    socket_path: Some(daemon_socket),
                    state_file: Some(state_file),
                    no_log_file: true,
                },
                startup,
            );
            let _ = finished_tx.send(());
            result
        });
        let mut daemon = DaemonCleanupGuard {
            handle: Some(daemon),
            active_request: None,
            finished: finished_rx,
            socket: socket.clone(),
            token: shutdown_token.to_owned(),
            release_request: Some(request_release_tx),
            release_loop: Some(loop_release_tx),
            stop: stop_test_handle,
        };
        ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("daemon bound and ready");
        loop_entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("real background loop entered its terminal work barrier");

        let status = daemon_call(
            &socket,
            1,
            "status",
            serde_json::json!({}),
            Some(shutdown_token),
        );
        assert_eq!(true, status["ok"], "{status}");
        assert!(status["result"].is_object(), "{status}");

        let created = daemon_call(
            &socket,
            2,
            "task_add",
            serde_json::json!({
                "title": "runtime wire task",
                "scope": "global",
                "body": "created through the production dispatcher",
            }),
            Some(shutdown_token),
        );
        assert_eq!(true, created["ok"], "{created}");
        let task_id = created["result"]["id"].as_str().expect("created task id");

        let listed = daemon_call(
            &socket,
            3,
            "task_list",
            serde_json::json!({"scope": "global"}),
            Some(shutdown_token),
        );
        assert_eq!(true, listed["ok"], "{listed}");
        assert!(
            listed["result"]["tasks"]
                .as_array()
                .is_some_and(|tasks| tasks.iter().any(|task| task["id"] == task_id)),
            "{listed}"
        );

        let read = daemon_call(
            &socket,
            4,
            "task_read",
            serde_json::json!({"id": task_id}),
            Some(shutdown_token),
        );
        assert_eq!(true, read["ok"], "{read}");
        assert_eq!(
            "created through the production dispatcher",
            read["result"]["body"]
        );

        let identity = daemon_call_with_identity(
            &socket,
            9,
            "whoami",
            serde_json::json!({}),
            Some(shutdown_token),
            Some("w1:p1"),
        );
        assert_eq!(true, identity["ok"], "{identity}");
        assert_eq!("w1:p1", identity["result"]["pane_id"]);
        assert_eq!(true, identity["result"]["bound"], "{identity}");

        let private = daemon_call_with_identity(
            &socket,
            5,
            "task_add",
            serde_json::json!({
                "title": "private runtime task",
                "scope": "w1:p1",
                "body": "must not be visible anonymously",
            }),
            Some(shutdown_token),
            Some("w1:p1"),
        );
        assert_eq!(true, private["ok"], "{private}");
        let private_id = private["result"]["id"].as_str().expect("private task id");

        // Python intentionally allows a caller with no pane to read task rows.
        let anonymous_read = daemon_call(
            &socket,
            6,
            "task_read",
            serde_json::json!({"id": private_id}),
            None,
        );
        assert_eq!(true, anonymous_read["ok"], "{anonymous_read}");

        let other_pane_identity = daemon_call_with_identity(
            &socket,
            10,
            "whoami",
            serde_json::json!({}),
            Some(shutdown_token),
            Some("w2:p2"),
        );
        assert_eq!(true, other_pane_identity["ok"], "{other_pane_identity}");
        assert_eq!("w2:p2", other_pane_identity["result"]["pane_id"]);
        assert_eq!(
            true, other_pane_identity["result"]["bound"],
            "{other_pane_identity}"
        );
        let private_read = daemon_call_with_identity(
            &socket,
            11,
            "task_read",
            serde_json::json!({"id": private_id}),
            None,
            Some("w2:p2"),
        );
        assert_eq!(false, private_read["ok"], "{private_read}");
        assert!(
            private_read["error"]
                .as_str()
                .is_some_and(|error| error.contains("private queue")),
            "{private_read}"
        );

        request_block.store(true, std::sync::atomic::Ordering::SeqCst);
        let active_request_socket = socket.clone();
        let active_request_token = shutdown_token.to_owned();
        daemon.active_request = Some(std::thread::spawn(move || {
            daemon_call(
                &active_request_socket,
                7,
                "status",
                serde_json::json!({}),
                Some(&active_request_token),
            )
        }));
        request_entered_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("accepted status request paused in its worker-local terminal factory");

        let (response, mut shutdown_reader) = daemon_call_with_identity_response(
            &socket,
            8,
            "shutdown",
            serde_json::json!({}),
            Some(shutdown_token),
            None,
        );
        assert_eq!(true, response["ok"]);
        shutdown_reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("set bounded pre-drain EOF probe timeout");
        let mut pre_drain_byte = [0_u8; 1];
        match shutdown_reader.read(&mut pre_drain_byte) {
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                ) => {}
            Ok(0) => panic!("shutdown connection reached EOF before accepted work drained"),
            Ok(count) => panic!("unexpected {count} byte(s) after shutdown response"),
            Err(error) => panic!("pre-drain read failed unexpectedly: {error}"),
        }
        shutdown_reader
            .get_mut()
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("restore post-drain EOF timeout");
        assert_eq!(
            Err(mpsc::RecvTimeoutError::Timeout),
            daemon.finished.recv_timeout(Duration::from_millis(100)),
            "daemon teardown waits for both the active request and loop work"
        );
        daemon
            .release_loop
            .take()
            .expect("loop release guard")
            .send(())
            .expect("release loop work");
        let loop_pane_count = loop_panes_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("background push loop reports its real pane-list result")
            .expect("background push loop lists panes successfully");
        assert!(loop_pane_count > 0, "the backend fixture has live panes");
        loop_completed_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("real background loop operation completes after release");
        assert_eq!(
            Err(mpsc::RecvTimeoutError::Timeout),
            daemon.finished.recv_timeout(Duration::from_millis(100)),
            "daemon teardown waits for the active accepted request after loop work completes"
        );
        daemon
            .release_request
            .take()
            .expect("request release guard")
            .send(())
            .expect("release active request");
        let active_response = daemon
            .active_request
            .take()
            .expect("active request join guard")
            .join()
            .expect("active request thread");
        assert_eq!(true, active_response["ok"], "{active_response}");
        let mut shutdown_remainder = Vec::new();
        shutdown_reader
            .read_to_end(&mut shutdown_remainder)
            .expect("shutdown connection reaches EOF after all requests drain");
        assert!(
            shutdown_remainder.is_empty(),
            "shutdown response has no extra bytes"
        );
        assert_eq!(
            0,
            daemon
                .handle
                .take()
                .expect("daemon thread guard")
                .join()
                .expect("daemon thread")
                .expect("clean daemon exit")
        );
        assert!(!socket.exists(), "socket is removed after joined shutdown");
        assert!(
            !config.join("daemon.log").exists(),
            "--no-log-file leaves no daemon log"
        );

        let allocation_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("allocation status runtime");
        let allocation_tasks = allocation_runtime
            .block_on(Tasks::open(config.join("tasks.db")))
            .expect("reopen the daemon task store");
        let allocation_status = allocation_runtime
            .block_on(allocation_tasks.allocation_status())
            .expect("first-run allocator schema supports status after loop startup");
        assert_eq!(serde_json::json!([]), allocation_status["workers"]);
        assert_eq!(serde_json::json!([]), allocation_status["offers"]);
    }

    #[test]
    fn loop_intervals_use_the_captured_environment() {
        let env = Env::from_pairs(&[("DUNKINGSHEEP_PUSH_S", "2.5"), ("DUNKINGSHEEP_REAP_S", "4")]);
        let intervals = captured_loop_intervals(&env).expect("valid captured cadence");
        assert_eq!(Duration::from_millis(2500), intervals.push);
        assert_eq!(Duration::from_secs(4), intervals.reaper);
    }

    #[test]
    fn supervised_default_socket_refusal_precedes_backend_and_store_work() {
        let dir = crate::testsupport::dir("daemon-supervised-refusal");
        let config = dir.path().join("config");
        let home_path = dir.path().join("home");
        let unit = home_path.join(".config/systemd/user/dunkingsheep.service");
        std::fs::create_dir_all(unit.parent().unwrap()).expect("unit directory");
        std::fs::write(&unit, "fixture").expect("unit file");
        let env = Env::from_pairs(&[
            ("DUNKINGSHEEP_DIR", config.to_str().unwrap()),
            ("HOME", home_path.to_str().unwrap()),
        ]);
        let startup = DaemonStartup {
            env: env.clone(),
            home: CapturedPythonHome::capture_with(&env, || Ok(None)),
            executable: std::env::current_exe().expect("test executable"),
            release_version: "test".into(),
            terminal_factory: Some(Arc::new(|| panic!("backend must not be constructed"))),
            loop_intervals: None,
            install_signals: false,
            ready: None,
            loop_test_gate: None,
            stop_test_handle: None,
        };

        assert_eq!(
            3,
            run_daemon_captured(DaemonOptions::default(), startup).unwrap()
        );
        assert!(!config.join("tasks.db").exists());
        assert!(!config.join("inbox.db").exists());
    }

    #[test]
    fn empty_backend_refuses_before_opening_stores() {
        let dir = crate::testsupport::dir("daemon-empty-backend-refusal");
        let config = dir.path().join("config");
        let env = Env::from_pairs(&[("DUNKINGSHEEP_DIR", config.to_str().unwrap())]);
        let factory: LoopTerminalFactory = Arc::new(|| {
            let mut terminal = crate::terminal::fakes::FakeTerminal::new();
            terminal.panes.clear();
            Box::new(terminal)
        });
        let startup = DaemonStartup {
            env: env.clone(),
            home: CapturedPythonHome::capture_with(&env, || Ok(None)),
            executable: std::env::current_exe().expect("test executable"),
            release_version: "test".into(),
            terminal_factory: Some(factory),
            loop_intervals: None,
            install_signals: false,
            ready: None,
            loop_test_gate: None,
            stop_test_handle: None,
        };

        assert_eq!(
            4,
            run_daemon_captured(DaemonOptions::default(), startup).unwrap()
        );
        assert!(!config.join("tasks.db").exists());
        assert!(!config.join("inbox.db").exists());
    }

    #[test]
    fn backend_list_error_refuses_with_code_four_before_store_open() {
        let dir = crate::testsupport::dir("daemon-backend-list-error");
        let config = dir.path().join("config");
        let absent_tmux = dir.path().join("no-such-tmux-binary");
        let env = Env::from_pairs(&[
            ("DUNKINGSHEEP_DIR", config.to_str().unwrap()),
            ("DUNKINGSHEEP_TMUX_BIN", absent_tmux.to_str().unwrap()),
        ]);
        let startup = DaemonStartup {
            env: env.clone(),
            home: CapturedPythonHome::capture_with(&env, || Ok(None)),
            executable: std::env::current_exe().expect("test executable"),
            release_version: "test".into(),
            terminal_factory: None,
            loop_intervals: Some(LoopIntervals {
                push: Duration::from_secs(60),
                stuck: Duration::from_secs(60),
                reaper: Duration::from_secs(60),
            }),
            install_signals: false,
            ready: None,
            loop_test_gate: None,
            stop_test_handle: None,
        };

        assert_eq!(
            4,
            run_daemon_captured(DaemonOptions::default(), startup).unwrap()
        );
        assert!(!config.join("tasks.db").exists());
        assert!(!config.join("inbox.db").exists());
    }

    #[test]
    fn already_running_returns_three_and_preserves_the_existing_socket() {
        let dir = crate::testsupport::dir("daemon-already-running");
        let config = dir.path().join("config");
        let socket = dir.path().join("already-running.sock");
        let listener = UnixListener::bind(&socket).expect("existing daemon socket");
        let env = Env::from_pairs(&[
            ("DUNKINGSHEEP_DIR", config.to_str().unwrap()),
            ("HOME", dir.path().to_str().unwrap()),
        ]);
        let startup = DaemonStartup {
            env: env.clone(),
            home: CapturedPythonHome::capture_with(&env, || Ok(None)),
            executable: std::env::current_exe().expect("test executable"),
            release_version: "test".into(),
            terminal_factory: Some(Arc::new(|| {
                Box::new(crate::terminal::fakes::FakeTerminal::new())
            })),
            loop_intervals: Some(LoopIntervals {
                push: Duration::from_secs(60),
                stuck: Duration::from_secs(60),
                reaper: Duration::from_secs(60),
            }),
            install_signals: false,
            ready: None,
            loop_test_gate: None,
            stop_test_handle: None,
        };

        assert_eq!(
            3,
            run_daemon_captured(
                DaemonOptions {
                    socket_path: Some(socket.clone()),
                    no_log_file: true,
                    ..DaemonOptions::default()
                },
                startup,
            )
            .expect("AlreadyRunning is a normal code-3 refusal")
        );
        assert!(config.is_dir(), "first-run config directory is created");
        assert!(config.join("tasks.db").exists());
        assert!(
            socket.exists(),
            "rollback must not unlink another daemon's socket"
        );
        let connection = UnixStream::connect(&socket).expect("existing listener remains usable");
        let _ = listener.accept().expect("accept probe");
        drop(connection);
    }

    #[test]
    fn role_store_startup_failure_leaves_no_daemon_socket() {
        let dir = crate::testsupport::dir("daemon-role-startup-rollback");
        let config = dir.path().join("config");
        std::fs::create_dir_all(&config).expect("config directory");
        let socket = dir.path().join("never-bound.sock");
        let bad_roles = dir.path().join("bad-roles.json");
        std::fs::write(&bad_roles, "{bad").expect("invalid roles fixture");
        let env = Env::from_pairs(&[
            ("DUNKINGSHEEP_DIR", config.to_str().unwrap()),
            ("DUNKINGSHEEP_ROLES_FILE", bad_roles.to_str().unwrap()),
        ]);
        let startup = DaemonStartup {
            env: env.clone(),
            home: CapturedPythonHome::capture_with(&env, || Ok(None)),
            executable: std::env::current_exe().expect("test executable"),
            release_version: "test".into(),
            terminal_factory: Some(Arc::new(|| {
                Box::new(crate::terminal::fakes::FakeTerminal::new())
            })),
            loop_intervals: Some(LoopIntervals {
                push: Duration::from_secs(60),
                stuck: Duration::from_secs(60),
                reaper: Duration::from_secs(60),
            }),
            install_signals: false,
            ready: None,
            loop_test_gate: None,
            stop_test_handle: None,
        };

        assert!(run_daemon_captured(
            DaemonOptions {
                socket_path: Some(socket.clone()),
                no_log_file: true,
                ..DaemonOptions::default()
            },
            startup,
        )
        .is_err());
        assert!(
            !socket.exists(),
            "failed role loading never binds the socket"
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("reopen probe runtime");
        runtime
            .block_on(Tasks::open(config.join("tasks.db")))
            .expect("task writer was released on startup failure");
    }
}
