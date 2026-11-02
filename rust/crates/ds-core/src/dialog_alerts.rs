//! Durable, per-sink deduplication for delivery-blocking dialog alerts.
//!
//! Ports `dunk_dialog_alerts.notify` and its `alerts` table. Sink implementations
//! are passed explicitly; this module owns identity, retry timing and persistence,
//! and wires the owner and VP delivery paths through injectable boundaries.

use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use sha2::{Digest, Sha256};

use crate::db::map_sqlite;
use crate::error::DunkError;
use crate::flock::commands_inbox::{InboxEnv, SendArgs};
use crate::flock::Flock;

const OWNER_POST_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_VP_PANE: &str = "w9:pH";

#[cfg(unix)]
const PYTHON_DEFAULT_SEARCH_PATH: &str = "/bin:/usr/bin";
#[cfg(windows)]
const PYTHON_DEFAULT_SEARCH_PATH: &str = r".;C:\Windows\System32";
#[cfg(not(any(unix, windows)))]
const PYTHON_DEFAULT_SEARCH_PATH: &str = "/bin:/usr/bin";

/// Inputs used to resolve the `vpctl` executable. `Some("")` mirrors Python's empty
/// `DUNKINGSHEEP_VPCTL`: it falls through to PATH and then the home-directory fallback.
#[derive(Debug, Clone)]
pub struct OwnerPostConfig {
    pub override_command: Option<OsString>,
    pub path: Option<OsString>,
    pub home: Option<PathBuf>,
}

impl OwnerPostConfig {
    /// Read the three process-environment values used by the Python owner sink.
    pub fn from_environment() -> Self {
        Self {
            override_command: std::env::var_os("DUNKINGSHEEP_VPCTL"),
            path: std::env::var_os("PATH"),
            home: python_home(
                std::env::var_os("HOME").map(PathBuf::from),
                account_home_directory,
            ),
        }
    }
}

fn python_home(
    configured_home: Option<PathBuf>,
    account_home: impl FnOnce() -> Option<PathBuf>,
) -> Option<PathBuf> {
    configured_home.or_else(account_home)
}

/// `Path.home()` uses the account database if HOME is absent. Rust's equivalent is
/// deprecated for general application use, but preserves that specific platform fallback
/// without adding a dependency to this port.
#[allow(deprecated)]
fn account_home_directory() -> Option<PathBuf> {
    std::env::home_dir()
}

/// Resolve the command using the same override, PATH, home fallback order as Python.
pub fn resolve_owner_command(config: &OwnerPostConfig) -> Result<PathBuf, String> {
    resolve_owner_command_with_default_path(config, OsStr::new(PYTHON_DEFAULT_SEARCH_PATH))
}

fn resolve_owner_command_with_default_path(
    config: &OwnerPostConfig,
    default_search_path: &OsStr,
) -> Result<PathBuf, String> {
    if let Some(command) = config
        .override_command
        .as_deref()
        .filter(|command| !command.is_empty())
    {
        return Ok(PathBuf::from(command));
    }
    let search_path = config.path.as_deref().unwrap_or(default_search_path);
    for directory in std::env::split_paths(search_path) {
        let candidate = directory.join("vpctl");
        if is_executable(&candidate) {
            return Ok(candidate);
        }
    }
    config
        .home
        .as_ref()
        .map(|home| home.join("workspace/vp-of-agentry/vpctl"))
        .ok_or_else(|| "HOME is not set and vpctl was not found on PATH".to_string())
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerProcessOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Injectable process boundary for the owner post. Production uses [`StdOwnerProcess`]; tests
/// can assert the exact invocation without executing any real owner tooling.
pub trait OwnerProcess {
    fn run(
        &mut self,
        command: &OsStr,
        args: &[OsString],
        body: &str,
        timeout: Duration,
    ) -> Result<OwnerProcessOutput, String>;
}

#[derive(Debug, Default)]
pub struct StdOwnerProcess;

impl OwnerProcess for StdOwnerProcess {
    fn run(
        &mut self,
        command: &OsStr,
        args: &[OsString],
        body: &str,
        timeout: Duration,
    ) -> Result<OwnerProcessOutput, String> {
        let mut process = Command::new(command);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            process.process_group(0);
        }
        let mut child = process
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| format!("could not start vpctl: {error}"))?;
        let mut stdin = child.stdin.take().expect("piped child stdin");
        let input = body.as_bytes().to_vec();
        let writer = thread::spawn(move || stdin.write_all(&input));
        let mut stdout = child.stdout.take().expect("piped child stdout");
        let stdout_reader = thread::spawn(move || {
            let mut output = Vec::new();
            stdout.read_to_end(&mut output).map(|_| output)
        });
        let mut stderr = child.stderr.take().expect("piped child stderr");
        let stderr_reader = thread::spawn(move || {
            let mut output = Vec::new();
            stderr.read_to_end(&mut output).map(|_| output)
        });

        let start = Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) if start.elapsed() < timeout => thread::sleep(Duration::from_millis(10)),
                Ok(None) => {
                    terminate_owner_process(&mut child);
                    break None;
                }
                Err(error) => {
                    terminate_owner_process(&mut child);
                    return Err(format!("vpctl wait failed: {error}"));
                }
            }
        };
        let _ = writer
            .join()
            .map_err(|_| "vpctl stdin writer panicked".to_string())?;
        let stdout = stdout_reader
            .join()
            .map_err(|_| "vpctl stdout reader panicked".to_string())?
            .map_err(|error| format!("could not read vpctl stdout: {error}"))?;
        let stderr = stderr_reader
            .join()
            .map_err(|_| "vpctl stderr reader panicked".to_string())?
            .map_err(|error| format!("could not read vpctl stderr: {error}"))?;
        let output = OwnerProcessOutput {
            success: status.is_some_and(|status| status.success()),
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::from_utf8_lossy(&stderr).into_owned(),
        };
        if status.is_none() {
            return Err(format!(
                "vpctl timed out after {} seconds",
                timeout.as_secs()
            ));
        }
        Ok(output)
    }
}

fn terminate_owner_process(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let group = format!("-{}", child.id());
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", group.as_str()])
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Send one dialog body to the owner using `vpctl brief --channel cos`.
pub fn post_owner(
    body: &str,
    config: &OwnerPostConfig,
    process: &mut dyn OwnerProcess,
) -> Result<SinkDelivery, String> {
    let command = resolve_owner_command(config)?;
    let args = [
        OsString::from("brief"),
        OsString::from("--channel"),
        OsString::from("cos"),
    ];
    let output = process.run(command.as_os_str(), &args, body, OWNER_POST_TIMEOUT)?;
    if !output.success {
        // Keep captured output inside the process boundary. Like Python's notify wrapper,
        // the durable sink result exposes the exception class, never arbitrary child text.
        return Err("CalledProcessError".into());
    }
    Ok(SinkDelivery::Delivered)
}

/// Production owner sink using process environment and the bounded std process runner.
pub fn post_owner_from_environment(body: &str) -> Result<SinkDelivery, String> {
    post_owner(
        body,
        &OwnerPostConfig::from_environment(),
        &mut StdOwnerProcess,
    )
}

/// Deliver a dialog alert to the VP through the real inbox storage/send path.
pub fn post_vp(
    flock: &Flock,
    env: &dyn InboxEnv,
    body: &str,
    vp_pane: Option<&str>,
) -> Result<SinkDelivery, String> {
    let pane = match vp_pane {
        Some(pane) => pane.to_string(),
        None => resolve_vp_pane(std::env::var("DUNKINGSHEEP_VP_PANE").ok().as_deref()),
    };
    let args = SendArgs {
        to: &pane,
        body,
        subject: Some("Dialog blocks queued delivery"),
        attach: &[],
        notify: true,
        self_pane_id: None,
        system_sender: Some("watchdog"),
    };
    let sent = flock
        .inbox_send(env, &args)
        .map_err(|error| error.to_string())?;
    if sent.outcome.as_deref() == Some("undeliverable") {
        Ok(SinkDelivery::Undeliverable)
    } else {
        Ok(SinkDelivery::Delivered)
    }
}

fn resolve_vp_pane(configured: Option<&str>) -> String {
    configured.unwrap_or(DEFAULT_VP_PANE).to_string()
}

pub const RETRY_S: f64 = 60.0;
pub const COOLDOWN_S: f64 = 1800.0;

static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock() -> MutexGuard<'static, ()> {
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkDelivery {
    Delivered,
    Undeliverable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SinkAttempt {
    pub sink: &'static str,
    pub delivered: bool,
    pub error: Option<String>,
}

fn default_path() -> Result<PathBuf, DunkError> {
    if let Some(directory) =
        std::env::var_os("DUNKINGSHEEP_DIR").filter(|directory| !directory.is_empty())
    {
        return Ok(PathBuf::from(directory).join("dialog-alerts.db"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| DunkError::Refused("HOME is not set for dialog alert storage".into()))?;
    Ok(PathBuf::from(home)
        .join(".config/dunkingsheep")
        .join("dialog-alerts.db"))
}

fn identity(pane_id: &str, generation: &str, signature: &str) -> String {
    let input = format!("{pane_id}\0{generation}\0{signature}");
    format!("{:x}", Sha256::digest(input.as_bytes()))
}

fn alert_body(pane_id: &str, signature: &str) -> String {
    let visible: String = signature.chars().take(500).collect();
    format!(
        "Dunking Sheep: a dialog in {pane_id} needs attention and prevents message delivery. \
Open that terminal to inspect the question. The VP may handle routine \
questions within existing authority; approvals needing the owner remain \
for the owner. No answer or key has been sent automatically.\n\n\
Visible dialog: {visible}"
    )
}

fn open(path: &Path) -> Result<Connection, DunkError> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path).map_err(map_sqlite)?;
    connection
        .busy_timeout(Duration::from_secs(5))
        .map_err(map_sqlite)?;
    connection
        .execute_batch(
            "CREATE TABLE IF NOT EXISTS alerts (
                identity TEXT NOT NULL,
                sink TEXT NOT NULL,
                last_attempt REAL NOT NULL,
                delivered REAL,
                PRIMARY KEY(identity, sink)
            )",
        )
        .map_err(map_sqlite)?;
    Ok(connection)
}

fn attempt_sink(
    db: &mut Connection,
    identity: &str,
    name: &'static str,
    now: f64,
    body: &str,
    sink: &mut dyn FnMut(&str) -> Result<SinkDelivery, String>,
) -> Result<Option<SinkAttempt>, DunkError> {
    let transaction = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(map_sqlite)?;
    let prior: Option<(f64, Option<f64>)> = transaction
        .query_row(
            "SELECT last_attempt, delivered FROM alerts WHERE identity=?1 AND sink=?2",
            (identity, name),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(map_sqlite)?;
    let suppressed = prior.is_some_and(|(last_attempt, delivered)| {
        delivered.is_some_and(|delivered| now - delivered < COOLDOWN_S)
            || now - last_attempt < RETRY_S
    });
    if suppressed {
        transaction.commit().map_err(map_sqlite)?;
        return Ok(None);
    }
    transaction
        .execute(
            "INSERT INTO alerts(identity,sink,last_attempt,delivered) VALUES (?1,?2,?3,NULL) \
             ON CONFLICT(identity,sink) DO UPDATE SET last_attempt=excluded.last_attempt, delivered=NULL",
            (identity, name, now),
        )
        .map_err(map_sqlite)?;
    transaction.commit().map_err(map_sqlite)?;

    let result = match sink(body) {
        Ok(SinkDelivery::Delivered) => {
            db.execute(
                "UPDATE alerts SET delivered=?1 WHERE identity=?2 AND sink=?3",
                (now, identity, name),
            )
            .map_err(map_sqlite)?;
            SinkAttempt {
                sink: name,
                delivered: true,
                error: None,
            }
        }
        Ok(SinkDelivery::Undeliverable) => SinkAttempt {
            sink: name,
            delivered: false,
            error: Some("notification target is unavailable".into()),
        },
        Err(error) => SinkAttempt {
            sink: name,
            delivered: false,
            error: Some(error),
        },
    };
    Ok(Some(result))
}

/// Notify the owner and VP sinks independently for one pane/process/dialog identity.
///
/// Missing generation or signature skips without creating the database. Each sink's
/// `last_attempt` is committed before its callback runs. A callback returning
/// `Undeliverable` is treated as a failure; successful sinks are suppressed for 30m,
/// failed sinks retry after 60s. A crash after delivery but before the delivered
/// update may repeat an alert, preserving at-least-once delivery.
pub fn notify(
    pane_id: &str,
    generation: Option<&str>,
    signature: Option<&str>,
    owner_sink: &mut dyn FnMut(&str) -> Result<SinkDelivery, String>,
    vp_sink: &mut dyn FnMut(&str) -> Result<SinkDelivery, String>,
    path: Option<&Path>,
    now: Option<f64>,
) -> Result<Vec<SinkAttempt>, DunkError> {
    let Some(generation) = generation.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let Some(signature) = signature.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    let identity = identity(pane_id, generation, signature);
    let body = alert_body(pane_id, signature);
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => default_path()?,
    };
    let now = now.unwrap_or_else(crate::clock::now_s);
    let _guard = lock();
    let mut db = open(&path)?;
    let mut results = Vec::new();

    if let Some(result) = attempt_sink(&mut db, &identity, "owner", now, &body, owner_sink)? {
        results.push(result);
    }
    if let Some(result) = attempt_sink(&mut db, &identity, "vp", now, &body, vp_sink)? {
        results.push(result);
    }
    Ok(results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::ReadPool;
    use crate::flock::commands_inbox::{Nudge, RouteEntry};
    use crate::flock::mailbox::MailboxEnv;
    use crate::inbox_store::Store;
    use serde_json::{json, Value};
    use std::cell::Cell;

    struct InboxFixture {
        pool: ReadPool,
        store: Store,
        dir: tempfile::TempDir,
        nudges: Mutex<Vec<String>>,
        panes: Vec<Value>,
    }

    impl InboxFixture {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let store = Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap();
            let pool = ReadPool::open(store.path(), 1).unwrap();
            Self {
                pool,
                store,
                dir,
                nudges: Mutex::new(Vec::new()),
                panes: vec![json!({
                    "pane_id": "w9:pH",
                    "terminal_id": "term_vp",
                    "label": "VP",
                    "tab_label": "VP",
                    "workspace_label": "fixture",
                    "agent": "codex"
                })],
            }
        }
    }

    impl MailboxEnv for InboxFixture {
        fn list_panes(&self) -> Vec<Value> {
            self.panes.clone()
        }
        fn list_panes_grouped(&self) -> Vec<Value> {
            self.panes.clone()
        }
        fn registered_pane(&self, _: &str) -> (bool, bool, Option<String>) {
            (false, false, None)
        }
    }

    impl InboxEnv for InboxFixture {
        fn caller_role(&self, _: &str) -> String {
            "system".into()
        }
        fn mailbox_override(&self) -> Option<String> {
            None
        }
        fn backend_name(&self) -> String {
            "fixture".into()
        }
        fn lane_panes(&self) -> Vec<String> {
            Vec::new()
        }
        fn pool(&self) -> &ReadPool {
            &self.pool
        }
        fn store(&self) -> &Store {
            &self.store
        }
        fn now(&self) -> f64 {
            100.0
        }
        fn record_metric(
            &self,
            _: &str,
            _: Option<&str>,
            _: &str,
            _: Option<&str>,
            _: Option<i64>,
        ) {
        }
        fn pm_routing(&self) -> Vec<RouteEntry> {
            Vec::new()
        }
        fn fold_into_queued_notice(&self, _: &str, _: &str, _: &str) -> Option<String> {
            None
        }
        fn send_now(&self, _: &str, text: &str, _: &str) -> Result<Nudge, DunkError> {
            self.nudges.lock().unwrap().push(text.to_string());
            Ok(Nudge {
                queued: false,
                message: Some("fixture nudge".into()),
            })
        }
        fn record_sent(&self, _: Option<&str>, _: &str, _: Option<&str>, _: usize, _: usize) {}
        fn tasks_for_pane(&self, _: &str) -> Result<Value, DunkError> {
            Ok(json!([]))
        }
    }

    fn fixture_flock(fixture: &InboxFixture) -> Flock {
        Flock::new(
            fixture.dir.path().join("launches.json"),
            fixture.dir.path().join("lanes.json"),
            false,
        )
    }

    fn latest_message_id(fixture: &InboxFixture) -> String {
        fixture
            .pool
            .with(|connection| {
                connection
                    .query_row(
                        "SELECT id FROM messages ORDER BY rowid DESC LIMIT 1",
                        [],
                        |row| row.get(0),
                    )
                    .map_err(crate::db::map_sqlite)
            })
            .unwrap()
    }

    struct FakeOwnerProcess {
        command: Option<OsString>,
        args: Vec<OsString>,
        body: Option<String>,
        timeout: Option<Duration>,
        result: Result<OwnerProcessOutput, String>,
    }

    impl Default for FakeOwnerProcess {
        fn default() -> Self {
            Self {
                command: None,
                args: Vec::new(),
                body: None,
                timeout: None,
                result: Ok(OwnerProcessOutput {
                    success: true,
                    stdout: String::new(),
                    stderr: String::new(),
                }),
            }
        }
    }

    impl OwnerProcess for FakeOwnerProcess {
        fn run(
            &mut self,
            command: &OsStr,
            args: &[OsString],
            body: &str,
            timeout: Duration,
        ) -> Result<OwnerProcessOutput, String> {
            self.command = Some(command.to_os_string());
            self.args = args.to_vec();
            self.body = Some(body.to_string());
            self.timeout = Some(timeout);
            self.result.clone()
        }
    }

    fn executable(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(path, body).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn owner_command_resolution_preserves_override_path_and_fallback_order() {
        let directory = tempfile::tempdir().unwrap();
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let on_path = bin.join("vpctl");
        executable(&on_path, "#!/bin/sh\nexit 0\n");
        let fallback = directory.path().join("workspace/vp-of-agentry/vpctl");
        std::fs::create_dir_all(fallback.parent().unwrap()).unwrap();
        executable(&fallback, "#!/bin/sh\nexit 0\n");

        let config = OwnerPostConfig {
            override_command: Some(OsString::from("/explicit/vpctl")),
            path: Some(bin.as_os_str().to_os_string()),
            home: Some(directory.path().to_path_buf()),
        };
        assert_eq!(
            PathBuf::from("/explicit/vpctl"),
            resolve_owner_command(&config).unwrap()
        );

        let config = OwnerPostConfig {
            override_command: Some(OsString::new()),
            ..config
        };
        assert_eq!(on_path, resolve_owner_command(&config).unwrap());

        let config = OwnerPostConfig {
            path: None,
            ..config
        };
        assert_eq!(fallback, resolve_owner_command(&config).unwrap());

        let missing = OwnerPostConfig {
            override_command: None,
            path: None,
            home: None,
        };
        assert!(resolve_owner_command(&missing)
            .unwrap_err()
            .contains("HOME is not set"));
    }

    #[test]
    fn missing_path_uses_python_default_search_path_and_home_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let default_bin = directory.path().join("default-bin");
        std::fs::create_dir(&default_bin).unwrap();
        let default_vpctl = default_bin.join("vpctl");
        executable(&default_vpctl, "#!/bin/sh\nexit 0\n");

        let config = OwnerPostConfig {
            override_command: None,
            path: None,
            home: None,
        };
        assert_eq!(
            default_vpctl,
            resolve_owner_command_with_default_path(&config, default_bin.as_os_str()).unwrap()
        );

        let account_home = directory.path().join("account-home");
        assert_eq!(
            Some(account_home.clone()),
            python_home(None, || Some(account_home))
        );
    }

    #[test]
    fn owner_post_uses_exact_args_unicode_stdin_and_fifteen_second_timeout() {
        let (_directory, path) = path();
        let mut process = FakeOwnerProcess {
            result: Ok(OwnerProcessOutput {
                success: true,
                stdout: "accepted".into(),
                stderr: String::new(),
            }),
            ..Default::default()
        };
        let config = OwnerPostConfig {
            override_command: Some(path.as_os_str().to_os_string()),
            path: None,
            home: None,
        };
        assert_eq!(
            SinkDelivery::Delivered,
            post_owner("question — ¿?", &config, &mut process).unwrap()
        );
        assert_eq!(Some(path.into_os_string()), process.command);
        assert_eq!(
            vec!["brief", "--channel", "cos"]
                .into_iter()
                .map(OsString::from)
                .collect::<Vec<_>>(),
            process.args
        );
        assert_eq!(Some("question — ¿?".to_string()), process.body);
        assert_eq!(Some(OWNER_POST_TIMEOUT), process.timeout);
    }

    #[test]
    fn owner_post_redacts_nonzero_process_output_and_reports_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let command = directory.path().join("vpctl");
        let config = OwnerPostConfig {
            override_command: Some(command.as_os_str().to_os_string()),
            path: None,
            home: None,
        };
        let mut timed_out = FakeOwnerProcess {
            result: Err("vpctl timed out after 15 seconds".into()),
            ..Default::default()
        };
        assert!(post_owner("body", &config, &mut timed_out)
            .unwrap_err()
            .contains("timed out"));
        let mut failed = FakeOwnerProcess {
            result: Ok(OwnerProcessOutput {
                success: false,
                stdout: "out".into(),
                stderr: "denied".into(),
            }),
            ..Default::default()
        };
        let error = post_owner("body", &config, &mut failed).unwrap_err();
        assert_eq!("CalledProcessError", error);
        assert!(!error.contains("out"));
        assert!(!error.contains("denied"));

        let (_alerts_dir, alerts_db) = path();
        let mut owner = |body: &str| post_owner(body, &config, &mut failed);
        let mut vp = successful_sink;
        let report = notify(
            "%2",
            Some("g-redacted"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&alerts_db),
            Some(100.0),
        )
        .unwrap();
        assert_eq!("CalledProcessError", report[0].error.as_deref().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn std_owner_process_drains_pipes_and_captures_fake_executable_output() {
        let directory = tempfile::tempdir().unwrap();
        let command = directory.path().join("vpctl");
        executable(
            &command,
            "#!/bin/sh\ncat\nprintf 'stdout'\nprintf 'stderr' >&2\n",
        );
        let output = StdOwnerProcess
            .run(
                command.as_os_str(),
                &["brief".into(), "--channel".into(), "cos".into()],
                "unicode λ body",
                Duration::from_secs(2),
            )
            .unwrap();
        assert!(output.success);
        assert_eq!("unicode λ bodystdout", output.stdout);
        assert_eq!("stderr", output.stderr);
    }

    #[cfg(unix)]
    #[test]
    fn std_owner_process_enforces_injected_test_timeout_and_spawn_errors() {
        let mut process = StdOwnerProcess;
        let timeout = process
            .run(
                OsStr::new("/bin/sleep"),
                &[OsString::from("2")],
                "body",
                Duration::from_millis(20),
            )
            .unwrap_err();
        assert!(timeout.contains("timed out"));
        let missing = process
            .run(
                Path::new("/definitely/missing/vpctl").as_os_str(),
                &[],
                "body",
                Duration::from_millis(20),
            )
            .unwrap_err();
        assert!(missing.contains("could not start vpctl"));
    }

    #[test]
    fn vp_target_uses_default_when_unset_and_preserves_empty_override() {
        assert_eq!("w9:pH", resolve_vp_pane(None));
        assert_eq!("w2:pVP", resolve_vp_pane(Some("w2:pVP")));
        assert_eq!("", resolve_vp_pane(Some("")));
    }

    #[test]
    fn vp_sink_stores_watchdog_message_with_exact_fields_and_fake_nudge() {
        let fixture = InboxFixture::new();
        let flock = fixture_flock(&fixture);
        let body = "Dialog needs attention — λ";
        assert_eq!(
            SinkDelivery::Delivered,
            post_vp(&flock, &fixture, body, Some("w9:pH")).unwrap()
        );
        assert_eq!(1, fixture.nudges.lock().unwrap().len());
        let message_id = latest_message_id(&fixture);
        let (summary, stored_body) =
            crate::inbox::get(&fixture.pool, &message_id, fixture.now()).unwrap();
        assert_eq!(body, stored_body);
        assert_eq!(Some("watchdog"), summary.sender.as_deref());
        assert_eq!(
            Some("Dialog blocks queued delivery"),
            summary.subject.as_deref()
        );
        assert_eq!("VP", summary.mailbox);
    }

    #[test]
    fn vp_sink_maps_undeliverable_and_propagates_store_errors() {
        let fixture = InboxFixture::new();
        let flock = fixture_flock(&fixture);
        assert_eq!(
            SinkDelivery::Undeliverable,
            post_vp(&flock, &fixture, "body", Some("missing-vp-mailbox")).unwrap()
        );
        assert!(post_vp(&flock, &fixture, "body", Some("")).is_err());
        let too_large = "x".repeat(crate::inbox::MAX_BODY_BYTES + 1);
        assert!(post_vp(&flock, &fixture, &too_large, Some("w9:pH")).is_err());
    }

    #[test]
    fn real_vp_store_retries_independently_of_failed_owner_sink() {
        let fixture = InboxFixture::new();
        let flock = fixture_flock(&fixture);
        let (_directory, path) = path();
        let owner_calls = Cell::new(0);
        let mut owner = |_: &str| {
            owner_calls.set(owner_calls.get() + 1);
            if owner_calls.get() == 1 {
                Err("owner process failed".into())
            } else {
                Ok(SinkDelivery::Delivered)
            }
        };
        let mut vp = |body: &str| post_vp(&flock, &fixture, body, Some("w9:pH"));
        let first = notify(
            "w1:p1",
            Some("pid:1:start:1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(2, first.len());
        assert!(!first[0].delivered);
        assert!(first[1].delivered);
        assert!(notify(
            "w1:p1",
            Some("pid:1:start:1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(159.0),
        )
        .unwrap()
        .is_empty());
        let retry = notify(
            "w1:p1",
            Some("pid:1:start:1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(160.0),
        )
        .unwrap();
        assert_eq!(
            vec!["owner"],
            retry.iter().map(|attempt| attempt.sink).collect::<Vec<_>>()
        );
        assert_eq!(2, owner_calls.get());
        let counts = crate::inbox::count(&fixture.pool, "VP", &[], fixture.now()).unwrap();
        assert_eq!(
            1, counts.unread,
            "successful VP mail was not repeated during owner retry"
        );
    }

    #[test]
    fn real_undeliverable_vp_retries_without_repeating_successful_owner() {
        let fixture = InboxFixture::new();
        let flock = fixture_flock(&fixture);
        let (_directory, path) = path();
        let owner_calls = Cell::new(0);
        let mut owner = |_: &str| {
            owner_calls.set(owner_calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        let mut vp = |body: &str| post_vp(&flock, &fixture, body, Some("missing-vp-mailbox"));
        let first = notify(
            "w1:p1",
            Some("pid:1:start:2"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(2, first.len());
        assert!(first[0].delivered);
        assert_eq!(
            Some("notification target is unavailable"),
            first[1].error.as_deref()
        );
        assert!(notify(
            "w1:p1",
            Some("pid:1:start:2"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(159.0),
        )
        .unwrap()
        .is_empty());
        let retry = notify(
            "w1:p1",
            Some("pid:1:start:2"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(160.0),
        )
        .unwrap();
        assert_eq!(
            vec!["vp"],
            retry.iter().map(|attempt| attempt.sink).collect::<Vec<_>>()
        );
        assert_eq!(1, owner_calls.get());
    }

    fn path() -> (tempfile::TempDir, PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/alerts.db");
        (directory, path)
    }

    fn delivered(sink: &'static str) -> SinkAttempt {
        SinkAttempt {
            sink,
            delivered: true,
            error: None,
        }
    }

    fn successful_sink(_: &str) -> Result<SinkDelivery, String> {
        Ok(SinkDelivery::Delivered)
    }

    #[test]
    fn durable_dedupe_is_per_pane_generation_and_dialog() {
        let (_directory, path) = path();
        let owner_calls = Cell::new(0);
        let vp_calls = Cell::new(0);
        let notify = |pane, generation, signature, now| {
            let mut owner = |_: &str| {
                owner_calls.set(owner_calls.get() + 1);
                Ok(SinkDelivery::Delivered)
            };
            let mut vp = |_: &str| {
                vp_calls.set(vp_calls.get() + 1);
                Ok(SinkDelivery::Delivered)
            };
            super::notify(
                pane,
                Some(generation),
                Some(signature),
                &mut owner,
                &mut vp,
                Some(&path),
                Some(now),
            )
            .unwrap()
        };

        assert_eq!(
            2,
            notify("%2", "pid:1:start:2", "Allow command?", 100.0).len()
        );
        assert!(notify("%2", "pid:1:start:2", "Allow command?", 999.0).is_empty());
        assert_eq!(
            2,
            notify("%2", "pid:1:start:2", "Approve network?", 1000.0).len()
        );
        assert_eq!(
            2,
            notify("%2", "pid:2:start:3", "Allow command?", 1001.0).len()
        );
        assert_eq!(
            2,
            notify("%3", "pid:1:start:2", "Allow command?", 1002.0).len()
        );
        assert_eq!(4, owner_calls.get());
        assert_eq!(4, vp_calls.get());
        assert!(
            path.exists(),
            "notify creates the database parent directory"
        );
    }

    #[test]
    fn recurrent_identical_dialog_realerts_at_the_thirty_minute_boundary() {
        let (_directory, path) = path();
        let mut owner = successful_sink;
        let mut vp = successful_sink;
        assert_eq!(
            2,
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                &mut owner,
                &mut vp,
                Some(&path),
                Some(100.0)
            )
            .unwrap()
            .len()
        );
        assert!(notify(
            "%2",
            Some("g1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(1899.0)
        )
        .unwrap()
        .is_empty());
        assert_eq!(
            2,
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                &mut owner,
                &mut vp,
                Some(&path),
                Some(1900.0)
            )
            .unwrap()
            .len()
        );
    }

    #[test]
    fn failed_owner_retries_after_sixty_seconds_without_repeating_successful_vp() {
        let (_directory, path) = path();
        let owner_calls = Cell::new(0);
        let vp_calls = Cell::new(0);
        let mut owner = |_: &str| {
            owner_calls.set(owner_calls.get() + 1);
            if owner_calls.get() == 1 {
                Err("offline".into())
            } else {
                Ok(SinkDelivery::Delivered)
            }
        };
        let mut vp = |_: &str| {
            vp_calls.set(vp_calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        let first = notify(
            "%2",
            Some("g1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(
            vec![
                SinkAttempt {
                    sink: "owner",
                    delivered: false,
                    error: Some("offline".into())
                },
                delivered("vp"),
            ],
            first
        );
        assert!(notify(
            "%2",
            Some("g1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(159.0)
        )
        .unwrap()
        .is_empty());
        assert_eq!(
            vec![delivered("owner")],
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                &mut owner,
                &mut vp,
                Some(&path),
                Some(160.0)
            )
            .unwrap()
        );
        assert_eq!(2, owner_calls.get());
        assert_eq!(1, vp_calls.get());
    }

    #[test]
    fn undeliverable_sink_is_failed_and_retried() {
        let (_directory, path) = path();
        let vp_calls = Cell::new(0);
        let mut owner = successful_sink;
        let mut vp = |_: &str| {
            vp_calls.set(vp_calls.get() + 1);
            if vp_calls.get() == 1 {
                Ok(SinkDelivery::Undeliverable)
            } else {
                Ok(SinkDelivery::Delivered)
            }
        };
        let first = notify(
            "%2",
            Some("g1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(
            Some("notification target is unavailable".to_string()),
            first[1].error
        );
        assert_eq!(
            vec![delivered("vp")],
            notify(
                "%2",
                Some("g1"),
                Some("question"),
                &mut owner,
                &mut vp,
                Some(&path),
                Some(160.0)
            )
            .unwrap()
        );
        assert_eq!(2, vp_calls.get());
    }

    #[test]
    fn unidentified_dialog_skips_without_creating_storage_or_calling_sinks() {
        let (directory, path) = path();
        let calls = Cell::new(0);
        let mut owner = |_: &str| {
            calls.set(calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        let mut vp = |_: &str| {
            calls.set(calls.get() + 1);
            Ok(SinkDelivery::Delivered)
        };
        assert!(notify(
            "%2",
            None,
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0)
        )
        .unwrap()
        .is_empty());
        assert!(notify(
            "%2",
            Some("g1"),
            Some(""),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0)
        )
        .unwrap()
        .is_empty());
        assert_eq!(0, calls.get());
        assert!(!directory.path().join("nested").exists());
    }

    #[test]
    fn last_attempt_is_committed_before_the_sink_runs() {
        let (_directory, path) = path();
        let path_for_sink = path.clone();
        let mut owner = |_: &str| {
            let connection = Connection::open(&path_for_sink).unwrap();
            let (last_attempt, delivered): (f64, Option<f64>) = connection
                .query_row(
                    "SELECT last_attempt, delivered FROM alerts WHERE sink='owner'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(100.0, last_attempt);
            assert_eq!(None, delivered);
            Err("crash after reservation".into())
        };
        let mut vp = successful_sink;
        let report = notify(
            "%2",
            Some("g1"),
            Some("question"),
            &mut owner,
            &mut vp,
            Some(&path),
            Some(100.0),
        )
        .unwrap();
        assert_eq!(
            "crash after reservation",
            report[0].error.as_deref().unwrap()
        );
    }
}
