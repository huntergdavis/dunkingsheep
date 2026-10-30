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
use super::{AgentSpec, Pane, Tab, Terminal, TerminalError, Workspace};

/// The tmux field format, verbatim from `TmuxBackend.PANE_FORMAT`.
pub const PANE_FORMAT: &str = "#{pane_id}\x1f#{session_id}\x1f#{window_id}\x1f#{pane_current_path}\x1f#{pane_current_command}\x1f#{pane_title}\x1f#{pane_active}\x1f#{session_name}\x1f#{window_name}\x1f#{window_index}\x1f#{session_index}\x1f#{pane_pid}\x1f#{pane_width}\x1f#{pane_height}";
pub const TAB_FORMAT: &str = "#{window_id}\x1f#{session_id}\x1f#{window_name}\x1f#{window_index}\x1f#{window_active}";
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
        TmuxBackend { tmux_bin: tmux_bin.into(), created: BTreeSet::new() }
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
        let output = Command::new(&self.tmux_bin).args(args).output().map_err(|error| {
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
        let under: Vec<&Pane> = panes.iter().filter(|pane| match kind {
            "tab" => pane.window_id == target_id,
            "workspace" => pane.session_id == target_id,
            _ => pane.pane_id == target_id,
        }).collect();
        under.into_iter().all(|pane| self.created.contains(&pane.pane_id)
                              || self.created.contains(&pane.window_id)
                              || self.created.contains(&pane.session_id))
    }

    fn capture_pane(&self, pane_id: &str, lines: i64, ansi: bool)
        -> Result<String, TerminalError>
    {
        let mut args = vec!["capture-pane", "-p"];
        if ansi {
            args.push("-e");
        }
        let start = format!("-{lines}");
        args.extend(["-S", start.as_str(), "-t", pane_id]);
        let result = self.run(&args)?;
        if result.code != 0 {
            return Err(TerminalError::refused(result.message("tmux capture-pane failed")));
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

    fn get_pane(&self, pane_id: &str) -> Option<Pane> {
        self.list_panes().into_iter().find(|pane| pane.pane_id == pane_id)
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
            grouped.entry(pane.workspace.clone()).or_default().push(pane);
        }
        grouped
    }

    fn agent_child_pids(&self, _pane_pid: i64) -> Result<Vec<i64>, TerminalError> {
        // PORT NOTE: the Python delegates to dunk_callers.pane_agent_pids (a /proc
        // read, T03). Until T03 lands this returns an empty list rather than
        // inventing one - a caller that needs ancestry must not read "no children"
        // as "no agent".
        Err(TerminalError::refused(
            "agent child pids need proc.rs (T03); not ported in T22".to_string()))
    }

    fn create_workspace(&mut self, label: Option<&str>, cwd: Option<&str>, focus: bool)
        -> Result<Workspace, TerminalError>
    {
        let name = safe_name(label.unwrap_or("dunkingsheep"), "dunkingsheep");
        let mut args = vec!["new-session", "-d", "-s", name.as_str()];
        if let Some(cwd) = cwd {
            args.extend(["-c", cwd]);
        }
        let result = self.run(&args)?;
        if result.code != 0 {
            return Err(TerminalError::refused(result.message("tmux new-session failed")));
        }
        let sessions = self.list_workspaces();
        let created = sessions.iter().find(|workspace| workspace.label == name)
            .or_else(|| sessions.last())
            .ok_or_else(|| TerminalError::refused(
                "new-session succeeded but the session list did not show it"))?;
        self.created.insert(created.workspace_id.clone());
        if focus {
            self.focus("workspace", &created.workspace_id)?;
        }
        Ok(created.clone())
    }

    fn create_tab(&mut self, workspace_id: Option<&str>, label: Option<&str>,
                  cwd: Option<&str>, focus: bool) -> Result<Tab, TerminalError>
    {
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
            return Err(TerminalError::refused(result.message("tmux new-window failed")));
        }
        let tab_id = result.stdout.trim().to_string();
        let tab = self.list_tabs().into_iter().find(|tab| tab.tab_id == tab_id)
            .ok_or_else(|| TerminalError::refused(
                format!("new-window created {tab_id} but the tab list did not show it")))?;
        self.created.insert(tab.tab_id.clone());
        if focus {
            self.focus("tab", &tab.tab_id)?;
        }
        Ok(tab)
    }

    fn split_pane(&mut self, pane_id: &str, direction: &str, cwd: Option<&str>)
        -> Result<Pane, TerminalError>
    {
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
            return Err(TerminalError::refused(result.message("tmux split-window failed")));
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
        let pane = spec.tab_id.as_deref()
            .and_then(|tab| self.list_panes().into_iter().find(|pane| pane.window_id == tab))
            .ok_or_else(|| TerminalError::refused(
                "start_agent needs an existing target pane (create_workspace/\
                 create_tab/split_pane first; T37 owns the layout)".to_string()))?;
        let argv: Vec<&str> = spec.argv.iter().map(String::as_str).collect();
        let command = shell_join(&argv);
        let result = self.run(&["respawn-pane", "-k", "-t", pane.pane_id.as_str(),
                                command.as_str()])?;
        if result.code != 0 {
            return Err(TerminalError::refused(result.message("tmux respawn-pane failed")));
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
            return Err(TerminalError::refused(result.message(&format!("tmux {command} failed"))));
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

    fn run_command(&mut self, pane_id: &str, command: &str) -> Result<(), TerminalError> {
        // `send-keys -l --` sends the text LITERALLY (the Python's chunked send),
        // then a separate Enter: a single send-keys would interpret a `;` in the
        // text as a tmux command separator.
        let result = self.run(&["send-keys", "-t", pane_id, "-l", "--", command])?;
        if result.code != 0 {
            return Err(TerminalError::refused(result.message("tmux send-keys failed")));
        }
        let enter = self.run(&["send-keys", "-t", pane_id, "Enter"])?;
        if enter.code != 0 {
            return Err(TerminalError::refused(enter.message("tmux send-keys Enter failed")));
        }
        Ok(())
    }
}

/// The Python's `shlex.join` for an argv a shell will read back.
pub fn shell_join(argv: &[&str]) -> String {
    argv.iter().map(|part| {
        if !part.is_empty() && part.chars().all(|character| {
            character.is_ascii_alphanumeric() || "-_./:=@%+".contains(character)
        }) {
            part.to_string()
        } else {
            format!("'{}'", part.replace('\'', "'\\''"))
        }
    }).collect::<Vec<_>>().join(" ")
}

/// The separator the parsed formats use (exported for tests and callers).
pub fn pane_separator() -> char {
    PANE_SEP
}

/// A convenience for a caller that wants `capture-pane` without the trait.
impl TmuxBackend {
    pub fn capture(&self, pane_id: &str, lines: i64, ansi: bool)
        -> Result<String, TerminalError>
    {
        self.capture_pane(pane_id, lines, ansi)
    }
}

/// The temp path helper the tests use to point at a fake tmux.
pub fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().map(|meta| meta.permissions().mode() & 0o111 != 0).unwrap_or(false)
}
