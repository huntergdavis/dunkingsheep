//! The terminal seam: one trait the daemon talks to, and a fake for tests (T20).
//!
//! Ported from `dunk_terminal.py`'s `TmuxClient` public surface and
//! `tests/fakes.py::FakeHerdr` (RUST_PORT.md 5.1). The seam is what makes the
//! daemon testable without a terminal, and what keeps a backend swappable: tmux is
//! the default (t50), herdr stays selectable for the rollback window.
//!
//! What is here: the record shapes (the fields `PANE_FORMAT`, `TAB_FORMAT` and
//! `WORKSPACE_FORMAT` produce), the trait, and [`fakes::FakeTerminal`] with the same
//! canned panes/tabs/workspaces the Python fixtures serve. What is NOT here: the
//! tmux client itself (T22), the control-mode client (T23) and the output parser
//! (T21) - this task is the seam and its fake.

use std::collections::BTreeMap;

pub mod fakes;
pub mod parse;
pub mod tmux;

/// One pane, as `tmux list-panes -F PANE_FORMAT` describes it.
///
/// The fields are the Python's, in the same order, because the parser (T21) fills
/// them positionally and every caller reads them by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pane {
    pub pane_id: String,
    pub session_id: String,
    pub window_id: String,
    pub cwd: String,
    pub command: String,
    /// The stable, human-chosen label: the recorded start_agent name when there is
    /// one, else the pane title, else the window name (the Python's `label`).
    pub label: String,
    /// tmux's own `#{pane_title}`, untouched (the Python's `terminal_title`).
    pub title: String,
    pub active: bool,
    pub workspace: String,
    pub tab: String,
    pub window_index: i64,
    pub workspace_index: i64,
    pub pane_pid: Option<i64>,
    pub width: i64,
    pub height: i64,
}

impl Pane {
    /// The tab this pane belongs to, as `fakes.py` computes it (`w2:p2` -> `w2`).
    pub fn tab_id(&self) -> String {
        self.pane_id.split(':').next().unwrap_or("").to_string()
    }
}

/// One tab (window), as `TAB_FORMAT` describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tab {
    pub tab_id: String,
    pub workspace_id: String,
    pub label: String,
    pub index: i64,
    pub active: bool,
}

/// One workspace (session), as `WORKSPACE_FORMAT` describes it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Workspace {
    pub workspace_id: String,
    pub label: String,
    pub index: i64,
}

/// A terminal failure, as the backend reported it.
///
/// The Python returns `(ok, message)` tuples from the mutating calls and raises
/// elsewhere; a port needs one shape, so everything fallible is a `TerminalError`
/// whose `message` is the text the operator should see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalError {
    pub message: String,
    /// True when the backend itself is unreachable (a refused command, not a bug).
    pub backend_unavailable: bool,
}

impl TerminalError {
    pub fn refused(message: impl Into<String>) -> Self {
        TerminalError {
            message: message.into(),
            backend_unavailable: false,
        }
    }

    pub fn unavailable(message: impl Into<String>) -> Self {
        TerminalError {
            message: message.into(),
            backend_unavailable: true,
        }
    }
}

impl std::fmt::Display for TerminalError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

/// The bounds the Python's `resize()` enforces before it touches tmux.
pub const RESIZE_MIN: i64 = 1;
pub const RESIZE_MAX: i64 = 1000;

/// One pane the re-layout moved, with the size it had before.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResizedPane {
    pub pane_id: String,
    pub cols: i64,
    pub rows: i64,
    pub cols_before: Option<i64>,
    pub rows_before: Option<i64>,
}

/// What a resize actually did - the geometry READ BACK, never the request echoed.
///
/// tmux has no absolute per-pane size: `resize-window -x/-y` sets the WINDOW grid and
/// every pane in it re-lays-out, so a caller that asked about one pane must be told
/// which neighbours moved too.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resize {
    pub pane_id: String,
    pub requested_cols: i64,
    pub requested_rows: i64,
    /// The pane's size after the call, when the backend could read it back.
    pub cols: Option<i64>,
    pub rows: Option<i64>,
    pub window_pane_count: usize,
    pub resized_panes: Vec<ResizedPane>,
    /// Attached clients. Reported rather than refused: measured 2026-10-28,
    /// `resize-window` succeeds with a client attached, and with `window-size=latest`
    /// that client's next resize event wins - so the caller is told, not lied to.
    pub clients_attached: usize,
    pub overridable_by_client: bool,
}

/// Where a new agent goes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AgentSpec {
    pub name: String,
    pub argv: Vec<String>,
    pub workspace_id: Option<String>,
    pub tab_id: Option<String>,
    pub split: Option<String>,
    pub cwd: Option<String>,
    pub label: Option<String>,
    pub env: BTreeMap<String, String>,
}

/// One backend. Every method has a Python counterpart in `TmuxClient`/`FakeHerdr`.
///
/// Reads are `Vec`/`Option` and cannot fail in the Python (a missing pane is `None`,
/// a missing list is empty); mutations return a `Result` so a refusal is visible.
pub trait Terminal: std::fmt::Debug {
    /// e.g. "tmux 3.4" - the version string the daemon logs.
    fn version(&self) -> String;
    /// Whether the backend can be used at all right now.
    fn is_available(&self) -> bool;
    /// One line an operator can act on when it cannot.
    fn server_error_hint(&self) -> String;

    fn list_panes(&self) -> Vec<Pane>;
    fn get_pane(&self, pane_id: &str) -> Option<Pane>;
    fn list_tabs(&self) -> Vec<Tab>;
    fn list_workspaces(&self) -> Vec<Workspace>;
    /// The panes under one grouped view (`list_panes_grouped` in the Python).
    fn list_panes_grouped(&self) -> BTreeMap<String, Vec<Pane>>;

    /// Child pids of a pane's shell, for the process-ancestry guard.
    fn agent_child_pids(&self, pane_pid: i64) -> Result<Vec<i64>, TerminalError>;

    fn create_workspace(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Workspace, TerminalError>;
    fn create_tab(
        &mut self,
        workspace_id: Option<&str>,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Tab, TerminalError>;
    fn split_pane(
        &mut self,
        pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
    ) -> Result<Pane, TerminalError>;
    fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError>;
    fn rename(&mut self, kind: &str, target_id: &str, label: &str) -> Result<(), TerminalError>;
    fn close(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError>;
    fn focus(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError>;
    /// Type one command line into a pane (the fake records it instead).
    ///
    /// NOTE: this is the Python's `run_command` (run a shell command in the pane at
    /// dunk_terminal.py:739), NOT the delivery surface below. The two were confused
    /// in the first version of this trait, which is what t923 caught.
    fn run_command(&mut self, pane_id: &str, command: &str) -> Result<(), TerminalError>;

    // -- the DELIVERY surface (t923) ---------------------------------------
    //
    // These are the methods a message's delivery is built on: T22 implements them
    // against tmux, T33 builds send/delivery/outbox on them, and a fake cannot fake
    // what the trait does not declare. They are declared here, in T20, exactly
    // because the later tasks must IMPLEMENT this seam rather than extend it.

    /// Type text into a pane's composer WITHOUT submitting it (chunked in the
    /// Python: `send-keys -l -- <chunk>` per chunk, with a delay between chunks).
    fn send_text(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError>;

    /// Type the text, let it settle, then press Enter once (`send_text_and_enter`).
    fn send_text_and_enter(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError>;

    /// Send KEY NAMES (not literal text): `send-keys -t <pane> <keys...>`, the
    /// escape hatch for C-c, Up, Escape and friends.
    fn send_keys(&mut self, pane_id: &str, keys: &[&str]) -> Result<(), TerminalError>;

    /// The pane's screen text, newest `lines` lines, optionally with ANSI colour
    /// (`capture-pane -p [-e] -S -<lines> -t <pane>`).
    ///
    /// `Ok(None)` means the pane could not be read (the Python returns None), which
    /// the stuck-message machine treats as "unverified", NOT as an empty composer.
    fn read_pane(
        &self,
        pane_id: &str,
        lines: i64,
        ansi: bool,
    ) -> Result<Option<String>, TerminalError>;

    /// The agent status the backend knows for a pane (`ready`/`working`/`unknown`),
    /// or `None` when the pane is not an agent.
    fn agent_status(&self, pane_id: &str) -> Option<String>;

    /// Size the pane's window and report the geometry read back (t928).
    ///
    /// In T22's own scope (`resize-window`, TASKS.md) and used by the web desk:
    /// `dunk_http.py` calls `backend.resize(pane_id, cols, rows)` with the Python's
    /// own refusal bounds. A backend that cannot size panes must say so with a named
    /// refusal rather than omitting the method - an absent method is discovered by
    /// compiling, which is not a sentence a caller can act on.
    fn resize(&mut self, pane_id: &str, cols: i64, rows: i64) -> Result<Resize, TerminalError>;
}
