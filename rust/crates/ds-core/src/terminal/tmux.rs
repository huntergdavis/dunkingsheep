//! The tmux subprocess backend (T22).
//!
//! Ported from `TmuxBackend` in `dunk_terminal.py`. Every call is one `tmux`
//! invocation whose argv is exactly the Python's - that is the part a port silently
//! gets wrong, so the tests point `tmux_bin` at a script that records the argv it was
//! given and answers with canned output. No test needs a live tmux, and no test
//! pretends the argv is unimportant.
//!
//! Two behaviours here are not cosmetic:
//!
//! * **A missing server is an empty list, not an error.** `tmux list-panes` fails with
//!   "no server running" on a quiet host, and the daemon must read that as "no panes"
//!   (the Python returns [] for it) rather than as a fault.
//! * **Close protection.** This backend refuses to close a pane/window/session it did
//!   not create: it keeps a registry of what it created, and a target is only
//!   closable when it (or the panes under it) came from here. The Python's second
//!   layer is `dunk_tmux_guard.py`; that is a separate task and is not here.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use super::parse::{parse_panes, parse_tabs, parse_workspaces, safe_name, PANE_SEP};
use super::{
    AgentSpec, Pane, Resize, ResizedPane, Tab, Terminal, TerminalError, Workspace, RESIZE_MAX,
    RESIZE_MIN,
};

/// `SEND_TEXT_CHUNK_CHARS` (herdr_client.py:21): a long message is typed in chunks,
/// because a TUI's input buffer does not take an arbitrary length in one keystroke run.
pub const SEND_TEXT_CHUNK_CHARS: usize = 900;
/// `SEND_TEXT_CHUNK_DELAY_S` (herdr_client.py:22): the pause between chunks.
pub const SEND_TEXT_CHUNK_DELAY_S: f64 = 0.05;
/// `SEND_TEXT_SETTLE_DELAY_S` (dunk_terminal.py): the pause before Enter, so the text is
/// in the composer before the submission arrives.
pub const SEND_TEXT_SETTLE_DELAY_S: f64 = 0.2;

/// The tmux field format, verbatim from `TmuxBackend.PANE_FORMAT`.
pub const PANE_FORMAT: &str = "#{pane_id}\x1f#{session_id}\x1f#{window_id}\x1f#{pane_current_path}\x1f#{pane_current_command}\x1f#{pane_title}\x1f#{pane_active}\x1f#{session_name}\x1f#{window_name}\x1f#{window_index}\x1f#{session_index}\x1f#{pane_pid}\x1f#{pane_width}\x1f#{pane_height}";
pub const TAB_FORMAT: &str =
    "#{window_id}\x1f#{session_id}\x1f#{window_name}\x1f#{window_index}\x1f#{window_active}";
pub const WORKSPACE_FORMAT: &str = "#{session_id}\x1f#{session_name}\x1f#{session_index}";

/// What one tmux invocation returned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandResult {
    /// The Python's `err.strip() or out.strip() or <fallback>` message shape.
    pub fn message(&self, fallback: &str) -> String {
        let error = self.stderr.trim();
        if !error.is_empty() {
            return error.to_string();
        }
        let output = self.stdout.trim();
        if !output.is_empty() {
            return output.to_string();
        }
        fallback.to_string()
    }

    /// "No server running" / "no sessions" is an EMPTY WORLD, not an error.
    pub fn is_no_server(&self) -> bool {
        let blob = format!("{}{}", self.stdout, self.stderr).to_lowercase();
        blob.contains("no server running") || blob.contains("no sessions")
    }
}

/// The tmux backend: sessions are workspaces, windows are tabs, panes are panes.
#[derive(Debug, Clone)]
pub struct TmuxBackend {
    pub tmux_bin: PathBuf,
    /// What this backend created - the close-protection registry.
    created: BTreeSet<String>,
}

impl Default for TmuxBackend {
    fn default() -> Self {
        TmuxBackend::new("tmux")
    }
}

impl TmuxBackend {
    pub fn new(tmux_bin: impl Into<PathBuf>) -> Self {
        TmuxBackend {
            tmux_bin: tmux_bin.into(),
            created: BTreeSet::new(),
        }
    }

    /// The registry of what this backend created (panes/tabs/workspaces, by id).
    pub fn created(&self) -> &BTreeSet<String> {
        &self.created
    }

    /// Record an id as created. Public because the registry is also loaded from a
    /// state file at startup (the Python's `_load_registry`), and a test that starts
    /// from a pre-existing world needs the same door.
    pub fn mark_created(&mut self, id: &str) {
        self.created.insert(id.to_string());
    }

    /// One tmux invocation, argv exactly as given.
    pub fn run(&self, args: &[&str]) -> Result<CommandResult, TerminalError> {
        let output = Command::new(&self.tmux_bin)
            .args(args)
            .output()
            .map_err(|error| {
                TerminalError::unavailable(format!("{}: {error}", self.tmux_bin.display()))
            })?;
        Ok(CommandResult {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    /// The Python's `_managed`: did we create this, or does it hold only panes we did?
    pub fn managed(&self, kind: &str, target_id: &str, panes: &[Pane]) -> bool {
        if !self.created.contains(target_id) {
            return false;
        }
        // A tab/workspace is only ours if every pane under it is ours too, so a
        // session an operator added a pane to cannot be killed by us.
        let under: Vec<&Pane> = panes
            .iter()
            .filter(|pane| match kind {
                "tab" => pane.window_id == target_id,
                "workspace" => pane.session_id == target_id,
                _ => pane.pane_id == target_id,
            })
            .collect();
        under.into_iter().all(|pane| {
            self.created.contains(&pane.pane_id)
                || self.created.contains(&pane.window_id)
                || self.created.contains(&pane.session_id)
        })
    }

    fn capture_pane(&self, pane_id: &str, lines: i64, ansi: bool) -> Result<String, TerminalError> {
        let mut args = vec!["capture-pane", "-p"];
        if ansi {
            args.push("-e");
        }
        let start = format!("-{lines}");
        args.extend(["-S", start.as_str(), "-t", pane_id]);
        let result = self.run(&args)?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message("tmux capture-pane failed"),
            ));
        }
        Ok(result.stdout)
    }
}

impl Terminal for TmuxBackend {
    fn version(&self) -> String {
        match self.run(&["-V"]) {
            Ok(result) if result.code == 0 => result.stdout.trim().to_string(),
            _ => "tmux (unknown version)".to_string(),
        }
    }

    fn is_available(&self) -> bool {
        matches!(self.run(&["list-sessions"]), Ok(result) if result.code == 0 || result.is_no_server())
    }

    fn server_error_hint(&self) -> String {
        "tmux server not running - start it with `tmux new -d`".to_string()
    }

    fn list_panes(&self) -> Vec<Pane> {
        match self.run(&["list-panes", "-a", "-F", PANE_FORMAT]) {
            Ok(result) if result.code == 0 => parse_panes(&result.stdout, &|_| None),
            // No server, no sessions, or a non-zero exit the Python also swallows:
            // an unreachable world is an empty one.
            _ => Vec::new(),
        }
    }

    fn try_list_panes(&self) -> Result<Vec<Pane>, TerminalError> {
        match self.run(&["list-panes", "-a", "-F", PANE_FORMAT]) {
            Ok(result) if result.code == 0 => Ok(parse_panes(&result.stdout, &|_| None)),
            Ok(result) if result.is_no_server() => Ok(Vec::new()),
            Ok(result) => Err(TerminalError::refused(
                result.message("tmux list-panes failed"),
            )),
            Err(error) => Err(error),
        }
    }

    fn get_pane(&self, pane_id: &str) -> Option<Pane> {
        self.list_panes()
            .into_iter()
            .find(|pane| pane.pane_id == pane_id)
    }

    fn list_tabs(&self) -> Vec<Tab> {
        match self.run(&["list-windows", "-a", "-F", TAB_FORMAT]) {
            Ok(result) if result.code == 0 => parse_tabs(&result.stdout),
            _ => Vec::new(),
        }
    }

    fn list_workspaces(&self) -> Vec<Workspace> {
        match self.run(&["list-sessions", "-F", WORKSPACE_FORMAT]) {
            Ok(result) if result.code == 0 => parse_workspaces(&result.stdout),
            _ => Vec::new(),
        }
    }

    fn list_panes_grouped(&self) -> std::collections::BTreeMap<String, Vec<Pane>> {
        let mut grouped: std::collections::BTreeMap<String, Vec<Pane>> = Default::default();
        for pane in self.list_panes() {
            grouped
                .entry(pane.workspace.clone())
                .or_default()
                .push(pane);
        }
        grouped
    }

    fn try_list_panes_grouped(
        &self,
    ) -> Result<std::collections::BTreeMap<String, Vec<Pane>>, TerminalError> {
        let mut grouped: std::collections::BTreeMap<String, Vec<Pane>> = Default::default();
        for pane in self.try_list_panes()? {
            grouped
                .entry(pane.workspace.clone())
                .or_default()
                .push(pane);
        }
        Ok(grouped)
    }

    fn agent_child_pids(&self, _pane_pid: i64) -> Result<Vec<i64>, TerminalError> {
        // PORT NOTE: the Python delegates to dunk_callers.pane_agent_pids (a /proc
        // read, T03). Until T03 lands this returns an empty list rather than
        // inventing one - a caller that needs ancestry must not read "no children"
        // as "no agent".
        Err(TerminalError::refused(
            "agent child pids need proc.rs (T03); not ported in T22".to_string(),
        ))
    }

    fn create_workspace(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Workspace, TerminalError> {
        let name = safe_name(label.unwrap_or("dunkingsheep"), "dunkingsheep");
        let mut args = vec!["new-session", "-d", "-s", name.as_str()];
        if let Some(cwd) = cwd {
            args.extend(["-c", cwd]);
        }
        let result = self.run(&args)?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message("tmux new-session failed"),
            ));
        }
        let sessions = self.list_workspaces();
        let created = sessions
            .iter()
            .find(|workspace| workspace.label == name)
            .or_else(|| sessions.last())
            .ok_or_else(|| {
                TerminalError::refused("new-session succeeded but the session list did not show it")
            })?;
        self.created.insert(created.workspace_id.clone());
        if focus {
            self.focus("workspace", &created.workspace_id)?;
        }
        Ok(created.clone())
    }

    fn create_tab(
        &mut self,
        workspace_id: Option<&str>,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Tab, TerminalError> {
        let name = safe_name(label.unwrap_or("tab"), "tab");
        let mut args = vec!["new-window", "-d", "-P", "-F", "#{window_id}"];
        if let Some(workspace_id) = workspace_id {
            args.extend(["-t", workspace_id]);
        }
        args.extend(["-n", name.as_str()]);
        if let Some(cwd) = cwd {
            args.extend(["-c", cwd]);
        }
        let result = self.run(&args)?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message("tmux new-window failed"),
            ));
        }
        let tab_id = result.stdout.trim().to_string();
        let tab = self
            .list_tabs()
            .into_iter()
            .find(|tab| tab.tab_id == tab_id)
            .ok_or_else(|| {
                TerminalError::refused(format!(
                    "new-window created {tab_id} but the tab list did not show it"
                ))
            })?;
        self.created.insert(tab.tab_id.clone());
        if focus {
            self.focus("tab", &tab.tab_id)?;
        }
        Ok(tab)
    }

    fn split_pane(
        &mut self,
        pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
    ) -> Result<Pane, TerminalError> {
        let mut args = vec!["split-window", "-t", pane_id];
        match direction {
            "down" => args.push("-v"),
            _ => args.push("-h"),
        }
        args.extend(["-P", "-F", "#{pane_id}"]);
        if let Some(cwd) = cwd {
            args.extend(["-c", cwd]);
        }
        let result = self.run(&args)?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message("tmux split-window failed"),
            ));
        }
        let new_pane = result.stdout.trim().to_string();
        let pane = self.get_pane(&new_pane).ok_or_else(|| {
            TerminalError::refused(format!("split created {new_pane} but it is not listed"))
        })?;
        self.created.insert(pane.pane_id.clone());
        Ok(pane)
    }

    fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError> {
        // An agent is started in a pane that already exists: T37 owns the layout
        // decision (workspace/tab/split) and calls create_* first.
        let pane = spec
            .tab_id
            .as_deref()
            .and_then(|tab| {
                self.list_panes()
                    .into_iter()
                    .find(|pane| pane.window_id == tab)
            })
            .ok_or_else(|| {
                TerminalError::refused(
                    "start_agent needs an existing target pane (create_workspace/\
                 create_tab/split_pane first; T37 owns the layout)"
                        .to_string(),
                )
            })?;
        let argv: Vec<&str> = spec.argv.iter().map(String::as_str).collect();
        let command = shell_join(&argv);
        let result = self.run(&[
            "respawn-pane",
            "-k",
            "-t",
            pane.pane_id.as_str(),
            command.as_str(),
        ])?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message("tmux respawn-pane failed"),
            ));
        }
        Ok(self.get_pane(&pane.pane_id).unwrap_or(pane))
    }

    fn rename(&mut self, kind: &str, target_id: &str, label: &str) -> Result<(), TerminalError> {
        let result = match kind {
            "workspace" => self.run(&["rename-session", "-t", target_id, label])?,
            "tab" => self.run(&["rename-window", "-t", target_id, label])?,
            "pane" => self.run(&["select-pane", "-t", target_id, "-T", label])?,
            other => return Err(TerminalError::refused(format!("cannot rename a {other}"))),
        };
        if result.code != 0 {
            return Err(TerminalError::refused(result.message("tmux rename failed")));
        }
        Ok(())
    }

    fn close(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
        let panes = self.list_panes();
        if !self.managed(kind, target_id, &panes) {
            return Err(TerminalError::refused(format!(
                "refusing to close {kind} {target_id}: Dunking Sheep did not create it                  (or it contains panes it did not create)")));
        }
        let command = match kind {
            "pane" => "kill-pane",
            "tab" => "kill-window",
            "workspace" => "kill-session",
            other => return Err(TerminalError::refused(format!("cannot close a {other}"))),
        };
        let result = self.run(&[command, "-t", target_id])?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message(&format!("tmux {command} failed")),
            ));
        }
        self.created.remove(target_id);
        Ok(())
    }

    fn focus(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
        let args: Vec<&str> = match kind {
            "pane" => vec!["select-pane", "-t", target_id],
            "tab" => vec!["select-window", "-t", target_id],
            "workspace" => vec!["switch-client", "-t", target_id],
            other => return Err(TerminalError::refused(format!("cannot focus a {other}"))),
        };
        let result = self.run(&args)?;
        if result.code != 0 {
            return Err(TerminalError::refused(result.message("tmux focus failed")));
        }
        Ok(())
    }

    fn resize(&mut self, pane_id: &str, cols: i64, rows: i64) -> Result<Resize, TerminalError> {
        // The Python's own bounds, refused BEFORE tmux is touched (measured: tmux
        // refuses 'width too small'/'width too large' without changing the layout, so
        // there is nothing to learn by asking it).
        if !(RESIZE_MIN..=RESIZE_MAX).contains(&cols) || !(RESIZE_MIN..=RESIZE_MAX).contains(&rows)
        {
            return Err(TerminalError::refused(format!(
                "cols and rows must be between {RESIZE_MIN} and {RESIZE_MAX}"
            )));
        }
        let Some(before) = self.get_pane(pane_id) else {
            return Err(TerminalError::refused(format!("no such pane {pane_id}")));
        };
        let window = before.tab_id();
        let geometry_before = self.window_geometry(&window);
        let attached = self.client_count(&before.workspace);
        let result = self.run(&[
            "resize-window",
            "-t",
            pane_id,
            "-x",
            &cols.to_string(),
            "-y",
            &rows.to_string(),
        ])?;
        if result.code != 0 {
            // A refusal changed nothing, so reporting before/after geometry would
            // imply a partial result that tmux did not produce.
            return Err(TerminalError::refused(
                result.message("tmux refused the resize"),
            ));
        }
        let geometry_after = self.window_geometry(&window);
        let target = geometry_after.get(pane_id).copied();
        let mut resized = Vec::new();
        for (pane, size) in &geometry_after {
            let was = geometry_before.get(pane).copied();
            if Some(*size) == was {
                continue;
            }
            resized.push(ResizedPane {
                pane_id: pane.clone(),
                cols: size.0,
                rows: size.1,
                cols_before: was.map(|size| size.0),
                rows_before: was.map(|size| size.1),
            });
        }
        Ok(Resize {
            pane_id: pane_id.to_string(),
            requested_cols: cols,
            requested_rows: rows,
            cols: target.map(|size| size.0),
            rows: target.map(|size| size.1),
            window_pane_count: geometry_after.len(),
            resized_panes: resized,
            clients_attached: attached,
            overridable_by_client: attached > 0,
        })
    }

    fn send_text(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
        // FINDING 4: the Python refuses an empty target with "No target".
        if pane_id.is_empty() {
            return Err(TerminalError::refused("No target"));
        }
        // FINDING 1: CHUNKED, and with a pause between chunks, exactly as the Python's
        // `_text_chunks` + SEND_TEXT_CHUNK_DELAY_S do. A 5 KB message is 6 tmux invocations
        // there and was ONE here; the limit exists because a TUI's input buffer does not
        // take an arbitrary length in one keystroke run, and the tests only ever exercised
        // a 35-character command.
        let chunks = text_chunks(text, SEND_TEXT_CHUNK_CHARS);
        for (index, chunk) in chunks.iter().enumerate() {
            // Literal, after `--`: a `;` in the text must not be read as a tmux separator.
            let result = self.run(&["send-keys", "-t", pane_id, "-l", "--", chunk])?;
            if result.code != 0 {
                return Err(TerminalError::refused(
                    result.message("tmux send-keys failed"),
                ));
            }
            if index + 1 < chunks.len() {
                sleep_s(SEND_TEXT_CHUNK_DELAY_S);
            }
        }
        Ok(())
    }

    fn send_text_and_enter(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
        self.send_text(pane_id, text)?;
        // FINDING 2: the Python lets the text SETTLE before Enter. Pressing immediately is
        // the typed-but-not-submitted shape the fleet keeps fixing.
        sleep_s(SEND_TEXT_SETTLE_DELAY_S);
        let enter = self.run(&["send-keys", "-t", pane_id, "Enter"])?;
        if enter.code != 0 {
            return Err(TerminalError::refused(
                enter.message("tmux send-keys Enter failed"),
            ));
        }
        Ok(())
    }

    fn send_keys(&mut self, pane_id: &str, keys: &[&str]) -> Result<(), TerminalError> {
        let mut args = vec!["send-keys", "-t", pane_id];
        args.extend(keys);
        let result = self.run(&args)?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message("tmux send-keys failed"),
            ));
        }
        Ok(())
    }

    fn read_pane(
        &self,
        pane_id: &str,
        lines: i64,
        ansi: bool,
    ) -> Result<Option<String>, TerminalError> {
        match self.capture_pane(pane_id, lines, ansi) {
            // FINDING 3: the Python returns `out.lstrip("\n")`; a capture that starts with
            // a blank line is not the same screen text as one that does not.
            Ok(text) => Ok(Some(text.trim_start_matches('\n').to_string())),
            // An unreadable/unknown pane is None in the Python, and None is NOT an empty
            // composer (dunk_stuck.verify depends on that distinction).
            Err(error) if !error.backend_unavailable => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn list_panes_with_status(&self) -> Vec<(Pane, Option<String>)> {
        // t1296: one `list-panes`, not one more tmux process per pane. Nothing here knows a
        // harness's own status, so every listed pane answers the word `unknown`, as
        // `_parse_panes` starts them.
        self.list_panes()
            .into_iter()
            .map(|pane| (pane, Some("unknown".to_string())))
            .collect()
    }

    fn get_pane_with_status(&self, pane_id: &str) -> Option<(Pane, Option<String>)> {
        // One listing; `agent_status` would list again.
        self.get_pane(pane_id)
            .map(|pane| (pane, Some("unknown".to_string())))
    }

    fn agent_status(&self, pane_id: &str) -> Option<String> {
        // FINDING 4: the Python's base implementation is `get_pane(...).get("agent_status")`,
        // so a pane that does not exist is None - not "unknown". Nothing here knows a
        // harness's own status, so a live pane answers the word rather than a guess.
        self.get_pane(pane_id)?;
        Some("unknown".to_string())
    }

    fn run_command(&mut self, pane_id: &str, command: &str) -> Result<(), TerminalError> {
        if pane_id.is_empty() {
            return Err(TerminalError::refused("No target"));
        }
        // The same chunked literal send as `send_text`, then a settle and a separate Enter:
        // a single send-keys would read a `;` in the text as a tmux command separator.
        let chunks = text_chunks(command, SEND_TEXT_CHUNK_CHARS);
        for (index, chunk) in chunks.iter().enumerate() {
            let result = self.run(&["send-keys", "-t", pane_id, "-l", "--", chunk])?;
            if result.code != 0 {
                return Err(TerminalError::refused(
                    result.message("tmux send-keys failed"),
                ));
            }
            if index + 1 < chunks.len() {
                sleep_s(SEND_TEXT_CHUNK_DELAY_S);
            }
        }
        sleep_s(SEND_TEXT_SETTLE_DELAY_S);
        let result = self.run(&["send-keys", "-t", pane_id, "Enter"])?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message("tmux send-keys Enter failed"),
            ));
        }
        return Ok(());
        #[allow(unreachable_code)]
        let result = self.run(&["send-keys", "-t", pane_id, "-l", "--", command])?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message("tmux send-keys failed"),
            ));
        }
        let enter = self.run(&["send-keys", "-t", pane_id, "Enter"])?;
        if enter.code != 0 {
            return Err(TerminalError::refused(
                enter.message("tmux send-keys Enter failed"),
            ));
        }
        Ok(())
    }
}

impl TmuxBackend {
    /// Measured cols/rows of every pane in one window, keyed by pane id.
    ///
    /// The Python asks tmux a second time for this (`_window_geometry`); the panes
    /// listing already carries both fields, so this is the same measurement without a
    /// second subprocess - and it is measured AFTER the call, which is the point.
    fn window_geometry(&self, window_id: &str) -> std::collections::BTreeMap<String, (i64, i64)> {
        let mut geometry = std::collections::BTreeMap::new();
        for pane in self.list_panes() {
            if pane.window_id == window_id || pane.tab_id() == window_id {
                geometry.insert(pane.pane_id.clone(), (pane.width, pane.height));
            }
        }
        geometry
    }

    /// How many clients are attached to a session (`tmux list-clients`).
    ///
    /// A failed call is 0 clients rather than an error: the resize already happened,
    /// and "we could not tell" must not turn a success into a failure.
    fn client_count(&self, workspace_id: &str) -> usize {
        match self.run(&["list-clients", "-t", workspace_id]) {
            Ok(result) if result.code == 0 => result
                .stdout
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count(),
            _ => 0,
        }
    }
}

/// Split text into chunk-sized pieces WITHOUT splitting a CRLF pair (the Python's
/// `_text_chunks`). Empty text yields no chunks, which is what the Python's generator
/// does too.
pub fn text_chunks(text: &str, max_chars: usize) -> Vec<String> {
    if max_chars == 0 || text.is_empty() {
        return Vec::new();
    }
    let characters: Vec<char> = text.chars().collect();
    let mut chunks = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let mut end = (index + max_chars).min(characters.len());
        // Do not cut a CRLF in half.
        if end < characters.len() && characters[end - 1] == '\r' && characters[end] == '\n' {
            end -= 1;
            if end == index {
                end += 1; // a chunk of one CR is still forward progress
            }
        }
        chunks.push(characters[index..end].iter().collect());
        index = end;
    }
    chunks
}

fn sleep_s(seconds: f64) {
    if seconds > 0.0 {
        std::thread::sleep(std::time::Duration::from_secs_f64(seconds));
    }
}

/// The Python's `shlex.join` for an argv a shell will read back.
pub fn shell_join(argv: &[&str]) -> String {
    argv.iter()
        .map(|part| {
            if !part.is_empty()
                && part.chars().all(|character| {
                    character.is_ascii_alphanumeric() || "-_./:=@%+".contains(character)
                })
            {
                part.to_string()
            } else {
                format!("'{}'", part.replace('\'', "'\\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The separator the parsed formats use (exported for tests and callers).
pub fn pane_separator() -> char {
    PANE_SEP
}

/// A convenience for a caller that wants `capture-pane` without the trait.
impl TmuxBackend {
    pub fn capture(&self, pane_id: &str, lines: i64, ansi: bool) -> Result<String, TerminalError> {
        self.capture_pane(pane_id, lines, ansi)
    }
}

/// The temp path helper the tests use to point at a fake tmux.
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|meta| meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}
