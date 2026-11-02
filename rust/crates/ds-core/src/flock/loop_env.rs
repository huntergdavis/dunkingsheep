//! Blocking-worker bridges for background Flock loops.
//!
//! A terminal backend is intentionally not required to be `Send`. The factory and
//! operation can cross into Tokio's blocking pool, while the terminal itself is created,
//! used through its local mutex, and dropped on that worker.

use std::ffi::OsString;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use crate::error::DunkError;
use crate::server_runtime::{configured_backend_name, DEFAULT_TERMINAL_BACKEND};
use crate::terminal::tmux::TmuxBackend;
use crate::terminal::Terminal;

/// A boxed future used by the async background-loop adapters.
pub type LoopFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Creates a terminal backend. The result is deliberately not `Send`.
pub type LoopTerminalFactory = Arc<dyn Fn() -> Box<dyn Terminal> + Send + Sync + 'static>;

/// Build the selected backend factory, refusing unsupported configured names before the
/// loop starts. The backend object itself is constructed later by [`with_loop_terminal`]'s
/// blocking-worker closure.
pub fn configured_loop_terminal_factory() -> Result<LoopTerminalFactory, DunkError> {
    let name = configured_backend_name()?;
    match name.as_str() {
        DEFAULT_TERMINAL_BACKEND => {
            let tmux_bin = tmux_binary_from_override(std::env::var_os("DUNKINGSHEEP_TMUX_BIN"));
            Ok(Arc::new(move || {
                Box::new(TmuxBackend::new(tmux_bin.clone()))
            }))
        }
        "herdr" => Err(crate::error::refusal(
            "terminal backend 'herdr' is retired; expected 'tmux'",
        )),
        _ => Err(crate::error::refusal(format!(
            "unknown terminal backend {name:?}; expected 'tmux'"
        ))),
    }
}

fn tmux_binary_from_override(override_bin: Option<OsString>) -> PathBuf {
    override_bin
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("tmux"))
}

/// Construct a fresh terminal inside a blocking worker and run one synchronous operation
/// against its local mutex. Only the factory, operation, and owned `Send` result cross the
/// worker boundary.
pub async fn with_loop_terminal<T, F>(
    factory: LoopTerminalFactory,
    operation: F,
) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce(&Mutex<Box<dyn Terminal>>) -> T + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let terminal = Mutex::new(factory());
        operation(&terminal)
    })
    .await
}

/// Run a synchronous store-only transition away from Tokio's async worker threads.
/// `SyncWriter` owns a private current-thread runtime and must not be called from an
/// ordinary async task.
pub async fn with_sync_store<T, F>(operation: F) -> Result<T, tokio::task::JoinError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    tokio::task::spawn_blocking(operation).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::marker::PhantomData;
    use std::rc::Rc;
    use std::sync::Mutex as StdMutex;

    use crate::metrics::{Event, MetricsStore};
    use crate::terminal::fakes::FakeTerminal;
    use crate::terminal::{AgentSpec, Pane, Resize, Tab, TerminalError, Workspace};

    fn thread_label() -> String {
        format!("{:?}", std::thread::current().id())
    }

    #[test]
    fn loop_terminal_factory_uses_configured_tmux_binary() {
        assert_eq!(
            tmux_binary_from_override(Some(OsString::from("/opt/ds/tmux-wrapper"))),
            PathBuf::from("/opt/ds/tmux-wrapper")
        );
    }

    #[test]
    fn loop_terminal_factory_uses_path_lookup_for_empty_or_unset_tmux_binary() {
        assert_eq!(
            tmux_binary_from_override(Some(OsString::new())),
            PathBuf::from("tmux")
        );
        assert_eq!(tmux_binary_from_override(None), PathBuf::from("tmux"));
    }

    #[derive(Debug)]
    struct NonSendTerminal {
        inner: FakeTerminal,
        dropped_on: Arc<StdMutex<Option<String>>>,
        _not_send: PhantomData<Rc<()>>,
    }

    impl Drop for NonSendTerminal {
        fn drop(&mut self) {
            *self.dropped_on.lock().expect("drop thread lock") = Some(thread_label());
        }
    }

    impl Terminal for NonSendTerminal {
        fn version(&self) -> String {
            self.inner.version()
        }
        fn is_available(&self) -> bool {
            self.inner.is_available()
        }
        fn server_error_hint(&self) -> String {
            self.inner.server_error_hint()
        }
        fn list_panes(&self) -> Vec<Pane> {
            self.inner.list_panes()
        }
        fn get_pane(&self, pane_id: &str) -> Option<Pane> {
            self.inner.get_pane(pane_id)
        }
        fn list_tabs(&self) -> Vec<Tab> {
            self.inner.list_tabs()
        }
        fn list_workspaces(&self) -> Vec<Workspace> {
            self.inner.list_workspaces()
        }
        fn list_panes_grouped(&self) -> BTreeMap<String, Vec<Pane>> {
            self.inner.list_panes_grouped()
        }
        fn agent_child_pids(&self, pane_pid: i64) -> Result<Vec<i64>, TerminalError> {
            self.inner.agent_child_pids(pane_pid)
        }
        fn create_workspace(
            &mut self,
            label: Option<&str>,
            cwd: Option<&str>,
            focus: bool,
        ) -> Result<Workspace, TerminalError> {
            self.inner.create_workspace(label, cwd, focus)
        }
        fn create_tab(
            &mut self,
            workspace_id: Option<&str>,
            label: Option<&str>,
            cwd: Option<&str>,
            focus: bool,
        ) -> Result<Tab, TerminalError> {
            self.inner.create_tab(workspace_id, label, cwd, focus)
        }
        fn split_pane(
            &mut self,
            pane_id: &str,
            direction: &str,
            cwd: Option<&str>,
        ) -> Result<Pane, TerminalError> {
            self.inner.split_pane(pane_id, direction, cwd)
        }
        fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError> {
            self.inner.start_agent(spec)
        }
        fn rename(
            &mut self,
            kind: &str,
            target_id: &str,
            label: &str,
        ) -> Result<(), TerminalError> {
            self.inner.rename(kind, target_id, label)
        }
        fn close(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
            self.inner.close(kind, target_id)
        }
        fn focus(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
            self.inner.focus(kind, target_id)
        }
        fn run_command(&mut self, pane_id: &str, command: &str) -> Result<(), TerminalError> {
            self.inner.run_command(pane_id, command)
        }
        fn send_text(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
            self.inner.send_text(pane_id, text)
        }
        fn send_text_and_enter(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
            self.inner.send_text_and_enter(pane_id, text)
        }
        fn send_keys(&mut self, pane_id: &str, keys: &[&str]) -> Result<(), TerminalError> {
            self.inner.send_keys(pane_id, keys)
        }
        fn read_pane(
            &self,
            pane_id: &str,
            lines: i64,
            ansi: bool,
        ) -> Result<Option<String>, TerminalError> {
            self.inner.read_pane(pane_id, lines, ansi)
        }
        fn agent_status(&self, pane_id: &str) -> Option<String> {
            self.inner.agent_status(pane_id)
        }
        fn resize(&mut self, pane_id: &str, cols: i64, rows: i64) -> Result<Resize, TerminalError> {
            self.inner.resize(pane_id, cols, rows)
        }
    }

    #[test]
    fn loop_terminal_bridge_constructs_and_drops_terminal_inside_worker() {
        let constructed_on = Arc::new(StdMutex::new(None));
        let dropped_on = Arc::new(StdMutex::new(None));
        let factory_constructed = Arc::clone(&constructed_on);
        let factory_dropped = Arc::clone(&dropped_on);
        let factory: LoopTerminalFactory = Arc::new(move || {
            *factory_constructed.lock().expect("factory thread lock") = Some(thread_label());
            Box::new(NonSendTerminal {
                inner: FakeTerminal::new(),
                dropped_on: Arc::clone(&factory_dropped),
                _not_send: PhantomData,
            })
        });
        let caller_thread = thread_label();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime");
        let (worker_thread, version) = runtime
            .block_on(with_loop_terminal(factory, |terminal| {
                let guard = terminal.lock().expect("terminal mutex");
                (thread_label(), guard.version())
            }))
            .expect("blocking terminal worker");
        assert_ne!(worker_thread, caller_thread);
        assert_eq!(version, "tmux 3.4 (fake)");
        assert_eq!(*constructed_on.lock().unwrap(), Some(worker_thread.clone()));
        assert_eq!(*dropped_on.lock().unwrap(), Some(worker_thread));

        let dir = crate::testsupport::dir("loop-sync-writer-worker");
        let metrics = Arc::new(MetricsStore::open(dir.path().join("metrics.db")).unwrap());
        let for_worker = Arc::clone(&metrics);
        let earliest = runtime
            .block_on(with_sync_store(move || {
                for_worker.record(&Event {
                    at: 42.0,
                    kind: "worker.probe".into(),
                    ..Event::default()
                });
                for_worker.earliest().expect("metrics read")
            }))
            .expect("sync store worker");
        assert_eq!(earliest, Some(42.0));
    }
}
