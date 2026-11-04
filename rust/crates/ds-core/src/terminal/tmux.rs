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

use super::tmux_registry::Registry;
use serde_json::json;
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
    registry: Registry,
    environment: crate::config::Env,
    guard_enabled: bool,
}

impl Default for TmuxBackend {
    fn default() -> Self {
        TmuxBackend::new("tmux")
    }
}

impl TmuxBackend {
    pub fn new(tmux_bin: impl Into<PathBuf>) -> Self {
        Self::from_env(tmux_bin, crate::config::Env::from_process())
    }
    pub fn from_env(tmux_bin: impl Into<PathBuf>, environment: crate::config::Env) -> Self {
        Self {
            tmux_bin: tmux_bin.into(),
            registry: Registry::configured(None, &environment),
            environment,
            guard_enabled: true,
        }
    }
    pub fn with_registry(tmux_bin: impl Into<PathBuf>, registry: impl Into<PathBuf>) -> Self {
        Self {
            tmux_bin: tmux_bin.into(),
            registry: Registry::new(registry),
            environment: crate::config::Env::default(),
            guard_enabled: false,
        }
    }
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// One tmux invocation, argv exactly as given.
    pub fn run(&self, args: &[&str]) -> Result<CommandResult, TerminalError> {
        let mut command = Command::new(&self.tmux_bin);
        if let Some(socket) = self
            .environment
            .get("DUNKINGSHEEP_TMUX_SOCKET")
            .filter(|v| !v.is_empty())
        {
            command.args(["-L", socket]);
        }
        let output = command.args(args).output().map_err(|error| {
            TerminalError::unavailable(format!("{}: {error}", self.tmux_bin.display()))
        })?;
        Ok(CommandResult {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        })
    }

    /// Reload authority for every check; parents never imply pane ownership.
    pub fn managed(&self, kind: &str, target_id: &str, panes: &[Pane]) -> bool {
        self.registry
            .managed(kind, target_id, panes)
            .unwrap_or(false)
    }
    fn parsed_panes(&self, output: &str) -> Result<Vec<Pane>, TerminalError> {
        let labels = self.registry.labels()?;
        let mut panes = parse_panes(output, &|id| labels.get(id).cloned());
        for pane in &mut panes {
            if let Some(name) = labels.get(&pane.pane_id) {
                pane.label = name.clone();
            }
        }
        Ok(panes)
    }
    fn checked_pane(&self, pane_id: &str) -> Result<Option<Pane>, TerminalError> {
        Ok(self
            .try_list_panes()?
            .into_iter()
            .find(|p| p.pane_id == pane_id))
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
    fn is_live_backend(&self) -> bool {
        true
    }
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
            Ok(result) if result.code == 0 => self.parsed_panes(&result.stdout).unwrap_or_default(),
            // No server, no sessions, or a non-zero exit the Python also swallows:
            // an unreachable world is an empty one.
            _ => Vec::new(),
        }
    }

    fn try_list_panes(&self) -> Result<Vec<Pane>, TerminalError> {
        match self.run(&["list-panes", "-a", "-F", PANE_FORMAT]) {
            Ok(result) if result.code == 0 => self.parsed_panes(&result.stdout),
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
        let result = self.create_workspace_raw(&json!(label), &json!(cwd), focus)?;
        if focus {
            self.focus("workspace", &result.workspace_id)?;
        }
        Ok(result)
    }

    fn create_workspace_raw(
        &mut self,
        label: &serde_json::Value,
        cwd: &serde_json::Value,
        focus: bool,
    ) -> Result<Workspace, TerminalError> {
        let label_text = super::raw::optional(label);
        let cwd_text = super::raw::optional(cwd);
        let name = safe_name(
            label_text.as_deref().unwrap_or("dunkingsheep"),
            "dunkingsheep",
        );
        let mut args = vec!["new-session", "-d", "-s", name.as_str()];
        if let Some(cwd) = cwd_text.as_deref() {
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
            .ok_or_else(|| {
                TerminalError::refused("new-session succeeded but the session list did not show it")
            })?;
        self.registry.remember(
            "workspace",
            &created.workspace_id,
            json!({"label": if super::raw::truthy(label) { label.clone() } else { json!(name) }}),
        )?;
        if let Some(tab) = self
            .list_tabs()
            .iter()
            .find(|t| t.workspace_id == created.workspace_id)
        {
            self.registry
                .remember("tab", &tab.tab_id, json!({"label":tab.label}))?;
        }
        if let Some(pane) = self
            .list_panes()
            .iter()
            .find(|p| p.session_id == created.workspace_id)
        {
            self.registry
                .remember("pane", &pane.pane_id, json!({"label":pane.label}))?;
        }
        let _ = focus;
        Ok(created.clone())
    }

    fn create_tab(
        &mut self,
        workspace_id: Option<&str>,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Tab, TerminalError> {
        let result = self.create_tab_raw(workspace_id, &json!(label), &json!(cwd), focus)?;
        if focus {
            self.focus("tab", &result.tab_id)?;
        }
        Ok(result)
    }

    fn create_tab_raw(
        &mut self,
        workspace_id: Option<&str>,
        label: &serde_json::Value,
        cwd: &serde_json::Value,
        focus: bool,
    ) -> Result<Tab, TerminalError> {
        let label_text = super::raw::optional(label);
        let cwd_text = super::raw::optional(cwd);
        let name = safe_name(label_text.as_deref().unwrap_or("tab"), "tab");
        let format = format!("#{{window_id}}{PANE_SEP}#{{pane_id}}");
        let mut args = vec!["new-window", "-d", "-P", "-F", &format];
        if let Some(workspace_id) = workspace_id {
            args.extend(["-t", workspace_id]);
        }
        args.extend(["-n", name.as_str()]);
        if let Some(cwd) = cwd_text.as_deref() {
            args.extend(["-c", cwd]);
        }
        let result = self.run(&args)?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message("tmux new-window failed"),
            ));
        }
        let mut fields = result.stdout.trim().split(PANE_SEP);
        let tab_id = fields.next().unwrap_or_default().to_owned();
        let pane = fields
            .next()
            .and_then(|id| self.get_pane(id))
            .ok_or_else(|| {
                TerminalError::refused("tmux created the window but it is not visible")
            })?;
        let tab = self
            .list_tabs()
            .into_iter()
            .find(|tab| tab.tab_id == tab_id)
            .ok_or_else(|| {
                TerminalError::refused(format!(
                    "new-window created {tab_id} but the tab list did not show it"
                ))
            })?;
        self.registry.remember(
            "tab",
            &tab.tab_id,
            json!({"label": if super::raw::truthy(label) { label.clone() } else { json!(name) }}),
        )?;
        self.registry
            .remember("pane", &pane.pane_id, json!({"label":pane.label}))?;
        let _ = focus;
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
        self.registry
            .remember("pane", &pane.pane_id, json!({"label":pane.label}))?;
        Ok(pane)
    }

    fn split_pane_raw(
        &mut self,
        pane_id: &str,
        direction: &str,
        cwd: &serde_json::Value,
        focus: bool,
    ) -> Result<Pane, TerminalError> {
        let cwd_text = super::raw::optional(cwd);
        let mut args = vec!["split-window", "-t", pane_id];
        match direction {
            "down" => args.push("-v"),
            _ => args.push("-h"),
        }
        args.extend(["-P", "-F", "#{pane_id}"]);
        if !focus {
            args.push("-d");
        }
        if let Some(cwd) = cwd_text.as_deref() {
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
        self.registry
            .remember("pane", &pane.pane_id, json!({"label":pane.label}))?;
        Ok(pane)
    }

    fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError> {
        let kind = spec
            .argv
            .first()
            .and_then(|p| Path::new(p).file_name())
            .and_then(|p| p.to_str())
            .ok_or_else(|| TerminalError::refused("command is required"))?;
        if !super::parse::AGENT_KINDS.contains(&kind) {
            return Err(TerminalError::refused(format!(
                "unsupported agent kind {kind:?}"
            )));
        }
        let mut command = crate::flock::commands_layout::shlex_join(&spec.argv);
        let mut environment = spec.env.clone();
        let guard_dir = if !self.guard_enabled {
            None
        } else {
            Some(
                match self
                    .environment
                    .get("DUNKINGSHEEP_TMUX_GUARD_DIR")
                    .filter(|v| !v.is_empty())
                {
                    Some(path) => PathBuf::from(path),
                    None => match self
                        .environment
                        .get("DUNKINGSHEEP_DIR")
                        .filter(|v| !v.is_empty())
                    {
                        Some(path) => PathBuf::from(path),
                        None => super::tmux_registry::expand_home(
                            "~/.config/dunkingsheep",
                            &self.environment,
                        )?,
                    }
                    .join("tmux-guard"),
                },
            )
        };
        if let Some(guard_dir) = guard_dir.filter(|dir| dir.join("tmux").is_file()) {
            let path = format!(
                "{}:{}",
                guard_dir.display(),
                environment
                    .get("PATH")
                    .filter(|v| !v.is_empty())
                    .map(String::as_str)
                    .unwrap_or_else(|| self.environment.get("PATH").unwrap_or(""))
            );
            command = format!("PATH={} {command}", shell_join(&[&path]));
            environment.insert("PATH".into(), path);
        }
        let mut args: Vec<String>;
        if let Some(tab) = spec.tab_id.as_deref().filter(|v| !v.is_empty()) {
            let pane = self
                .list_panes()
                .into_iter()
                .find(|p| p.window_id == tab)
                .ok_or_else(|| TerminalError::refused(format!("tab {tab} has no pane to split")))?;
            args = vec![
                "split-window".into(),
                "-t".into(),
                pane.pane_id,
                if spec.split.as_deref().unwrap_or("right") == "right" {
                    "-h"
                } else {
                    "-v"
                }
                .into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-d".into(),
            ];
        } else {
            let target = spec
                .workspace_id
                .clone()
                .filter(|v| !v.is_empty())
                .or_else(|| {
                    self.environment
                        .get("TMUX_PANE")
                        .and_then(|id| self.get_pane(id))
                        .map(|p| p.session_id)
                })
                .or_else(|| {
                    self.list_workspaces()
                        .first()
                        .map(|w| w.workspace_id.clone())
                })
                .ok_or_else(|| TerminalError::refused("no tmux session to start the agent in"))?;
            args = vec![
                "new-window".into(),
                "-d".into(),
                "-P".into(),
                "-F".into(),
                "#{pane_id}".into(),
                "-t".into(),
                target,
                "-n".into(),
                safe_name(&spec.name, "dunkingsheep"),
            ];
        }
        if let Some(cwd) = spec.cwd.as_deref().filter(|v| !v.is_empty()) {
            args.extend(["-c".into(), cwd.into()]);
        }
        for (key, value) in environment {
            args.extend(["-e".into(), format!("{key}={value}")]);
        }
        args.push(agent_launch(&command));
        let result = self.run(&args.iter().map(String::as_str).collect::<Vec<_>>())?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message("tmux agent launch failed"),
            ));
        }
        let mut pane = self.get_pane(result.stdout.trim()).ok_or_else(|| {
            TerminalError::refused("tmux started the agent but the pane is not visible")
        })?;
        if spec.tab_id.as_deref().is_none_or(str::is_empty) {
            if let Some(tab) = self.list_tabs().iter().find(|t| t.tab_id == pane.window_id) {
                self.registry
                    .remember("tab", &tab.tab_id, json!({"label":spec.name}))?;
            }
        }
        let _ = self.run(&["select-pane", "-t", &pane.pane_id, "-T", &spec.name]);
        pane = self.get_pane(&pane.pane_id).unwrap_or(pane);
        self.registry.remember(
            "pane",
            &pane.pane_id,
            json!({"agent":spec.name,"agent_kind":kind,"name":spec.name}),
        )?;
        pane.command = kind.into();
        pane.label = spec.name.clone();
        Ok(pane)
    }
    fn supports_guarded_respawn(&self) -> bool {
        true
    }
    fn guarded_respawn(
        &mut self,
        pane_id: &str,
        argv: &[String],
        expected_pid: i64,
        expected_start: &str,
    ) -> Result<Pane, TerminalError> {
        if pane_id.is_empty() {
            return Err(TerminalError::refused("a pane id is required"));
        }
        if argv.is_empty() {
            return Err(TerminalError::refused("an argv command is required"));
        }
        let pane = self
            .checked_pane(pane_id)?
            .ok_or_else(|| TerminalError::refused(format!("pane {pane_id} is not visible")))?;
        if !self.registry.managed("pane", pane_id, &[])? {
            return Err(TerminalError::refused(format!(
                "refusing to respawn pane {pane_id}: Dunking Sheep did not create it"
            )));
        }
        let old_pid = pane.pane_pid.ok_or_else(|| {
            TerminalError::refused(format!("pane {pane_id} has no pane process id"))
        })?;
        if old_pid != expected_pid {
            return Err(TerminalError::refused(format!(
                "refusing: pane {pane_id} pid is {old_pid}, expected {expected_pid}"
            )));
        }
        if crate::proc::process_start(old_pid).as_deref() != Some(expected_start) {
            return Err(TerminalError::refused(format!(
                "refusing: pane {pane_id} process start does not match expected {expected_start}"
            )));
        }
        let command = agent_launch(&crate::flock::commands_layout::shlex_join(argv));
        let current = self.checked_pane(pane_id)?.ok_or_else(|| {
            TerminalError::refused(format!("pane {pane_id} disappeared before respawn"))
        })?;
        let current_pid = current.pane_pid.ok_or_else(|| {
            TerminalError::refused(format!("pane {pane_id} lost its pane pid before respawn"))
        })?;
        if current_pid != old_pid {
            return Err(TerminalError::refused(format!("refusing: pane {pane_id} was replaced between check and mutation (pid {old_pid} -> {current_pid})")));
        }
        if crate::proc::process_start(current_pid).as_deref() != Some(expected_start) {
            return Err(TerminalError::refused(format!(
                "refusing: pane {pane_id} process changed between check and mutation"
            )));
        }
        // tmux has no atomic compare-and-kill: replacement after this read remains possible.
        let result = self.run(&["respawn-pane", "-k", "-t", pane_id, &command])?;
        if result.code != 0 {
            return Err(TerminalError::refused(
                result.message("tmux respawn-pane failed"),
            ));
        }
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while std::time::Instant::now() < deadline {
            if let Some(current) = self.checked_pane(pane_id)? {
                if current.pane_pid.is_some_and(|pid| pid != old_pid) {
                    return Ok(current);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Err(TerminalError::refused(format!(
            "pane {pane_id} did not show a new process after respawn"
        )))
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
        if kind == "pane" && self.get_pane(target_id).is_some() {
            self.registry
                .remember("pane", target_id, json!({"label":label}))?;
        }
        Ok(())
    }

    fn close(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
        let panes = if matches!(kind, "tab" | "workspace") {
            let result = self.run(&["list-panes", "-a", "-F", PANE_FORMAT])?;
            if result.code != 0 {
                return Err(TerminalError::refused(
                    result.message("tmux list-panes failed"),
                ));
            }
            // The display parser deliberately skips malformed rows. Destructive
            // parent operations must not silently omit an unowned child.
            for row in result.stdout.lines() {
                let fields: Vec<_> = row.split(PANE_SEP).collect();
                if fields.len() != 14 || fields[..3].iter().any(|id| id.is_empty()) {
                    return Err(TerminalError::refused("incomplete tmux pane inventory"));
                }
            }
            self.parsed_panes(&result.stdout)?
        } else {
            self.try_list_panes()?
        };
        if !self.registry.managed(kind, target_id, &panes)? {
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
        self.registry.forget(kind, target_id)?;
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

    fn send_text_raw(
        &mut self,
        pane: &str,
        text: &serde_json::Value,
    ) -> Result<Result<(), TerminalError>, crate::error::DunkError> {
        if pane.is_empty() {
            return Ok(Err(TerminalError::refused("No target")));
        }
        let chunks = super::raw::text_chunks(text)?;
        for (index, chunk) in chunks.iter().enumerate() {
            let result = match self.run(&["send-keys", "-t", pane, "-l", "--", chunk]) {
                Ok(result) => result,
                Err(error) => return Ok(Err(error)),
            };
            if result.code != 0 {
                return Ok(Err(TerminalError::refused(
                    result.message("tmux send-keys failed"),
                )));
            }
            if index + 1 < chunks.len() {
                std::thread::sleep(std::time::Duration::from_secs_f64(SEND_TEXT_CHUNK_DELAY_S));
            }
        }
        Ok(Ok(()))
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

    fn try_list_panes_with_status(&self) -> Result<Vec<(Pane, Option<String>)>, TerminalError> {
        // Preserve both the fallible list result and the single-process tmux path.
        self.try_list_panes().map(|panes| {
            panes
                .into_iter()
                .map(|pane| (pane, Some("unknown".to_string())))
                .collect()
        })
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

/// Preserve a fast-exiting agent's diagnostic and leave a visible shell.
pub fn agent_launch(command: &str) -> String {
    let script = format!("{command}; printf '\\n[agent exited rc=%s]\\n' \"$?\"; exec sh");
    format!(
        "sh -c {}",
        crate::flock::commands_layout::shlex_join(&[script])
    )
}
