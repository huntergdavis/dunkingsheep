//! [`FakeTerminal`]: the in-memory backend the tests use (T20).
//!
//! Ported from `tests/fakes.py::FakeHerdr`: the same canned panes, tabs and
//! workspaces, the same recording of calls and type-ins, and the same "make the next
//! mutation fail" switch. Layout mutations mutate the canned lists, exactly as the
//! fake does, so a test can create a tab and then find it.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    AgentSpec, Pane, Resize, ResizedPane, Tab, Terminal, TerminalError, Workspace, RESIZE_MAX,
    RESIZE_MIN,
};

/// The canned panes, from `tests/fakes.py::PANES` (pane_id, session, cwd, command,
/// title, workspace/tab labels, pid). Kept as data rather than a builder because the
/// point of the fake is that every test sees the SAME world.
fn canned_panes() -> Vec<Pane> {
    vec![
        Pane {
            pane_id: "w1:p1".into(),
            session_id: "s1".into(),
            window_id: "w1".into(),
            cwd: "/home/hunter/work".into(),
            command: "bash".into(),
            title: "π - work".into(),
            label: "π - work".into(),
            active: true,
            workspace: "work".into(),
            tab: "1".into(),
            window_index: 1,
            workspace_index: 1,
            pane_pid: Some(1101),
            width: 120,
            height: 40,
        },
        Pane {
            pane_id: "w2:p2".into(),
            session_id: "s2".into(),
            window_id: "w2".into(),
            cwd: "/home/hunter/work".into(),
            command: "claude".into(),
            title: "π - review".into(),
            label: "π - review".into(),
            active: true,
            workspace: "work".into(),
            tab: "2".into(),
            window_index: 2,
            workspace_index: 1,
            pane_pid: Some(2202),
            width: 120,
            height: 40,
        },
        Pane {
            pane_id: "w3:p3".into(),
            session_id: "s3".into(),
            window_id: "w3".into(),
            cwd: "/home/hunter/other".into(),
            command: "bash".into(),
            title: "π - other".into(),
            label: "π - other".into(),
            active: true,
            workspace: "other".into(),
            tab: "1".into(),
            window_index: 1,
            workspace_index: 2,
            pane_pid: Some(3303),
            width: 100,
            height: 30,
        },
    ]
}

fn canned_tabs() -> Vec<Tab> {
    vec![
        Tab {
            tab_id: "w1".into(),
            workspace_id: "w1".into(),
            label: "1".into(),
            index: 1,
            active: true,
        },
        Tab {
            tab_id: "w2".into(),
            workspace_id: "w2".into(),
            label: "2".into(),
            index: 2,
            active: true,
        },
        Tab {
            tab_id: "w3".into(),
            workspace_id: "w3".into(),
            label: "1".into(),
            index: 1,
            active: true,
        },
    ]
}

fn canned_workspaces() -> Vec<Workspace> {
    vec![
        Workspace {
            workspace_id: "w1".into(),
            label: "work".into(),
            index: 1,
        },
        Workspace {
            workspace_id: "w2".into(),
            label: "other".into(),
            index: 2,
        },
    ]
}

/// The in-memory backend.
#[derive(Debug)]
pub struct FakeTerminal {
    pub panes: Vec<Pane>,
    pub tabs: Vec<Tab>,
    pub workspaces: Vec<Workspace>,
    pub available: bool,
    /// Failure injected by the fallible grouped-pane listing, for propagation tests.
    pub fail_grouped_panes: Option<TerminalError>,
    /// One entry per layout mutation: `("create_tab", tab_id)` etc., for assertions.
    pub layout_calls: Vec<(String, String)>,
    /// `(pane_id, command)` per `run_command`.
    pub runs: Vec<(String, String)>,
    /// The message the NEXT mutation fails with, then cleared (as the Python does).
    pub fail_layout: Option<String>,
    /// Child pids served per pane, for `agent_child_pids`.
    pub children: BTreeMap<i64, Vec<i64>>,
    /// Sentinel text per pane, served by `read_pane` (the Python's `self.screens`).
    pub screens: BTreeMap<String, String>,
    /// Panes whose read FAILS (the Python's `self.unreadable`): a read that returns
    /// None is not an empty composer, and a fake must be able to say so.
    pub unreadable: BTreeSet<String>,
    /// The status per pane, served by `agent_status` (the Python's `self.statuses`).
    pub statuses: BTreeMap<String, String>,
    /// Every `(pane_id, text)` typed without Enter.
    pub sends: Vec<(String, String)>,
    /// Every `(pane_id, text)` typed and submitted.
    pub submits: Vec<(String, String)>,
    /// Every `(pane_id, keys)` sent as key names.
    pub keys_sent: Vec<(String, Vec<String>)>,
    /// Every `(pane_id, lines, ansi)` read.
    pub reads: Vec<(String, i64, bool)>,
    counter: i64,
}

impl Default for FakeTerminal {
    fn default() -> Self {
        FakeTerminal {
            panes: canned_panes(),
            tabs: canned_tabs(),
            workspaces: canned_workspaces(),
            available: true,
            fail_grouped_panes: None,
            layout_calls: Vec::new(),
            runs: Vec::new(),
            fail_layout: None,
            children: BTreeMap::new(),
            screens: BTreeMap::new(),
            unreadable: BTreeSet::new(),
            statuses: BTreeMap::new(),
            sends: Vec::new(),
            submits: Vec::new(),
            keys_sent: Vec::new(),
            reads: Vec::new(),
            counter: 100,
        }
    }
}

impl FakeTerminal {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn unavailable() -> Self {
        FakeTerminal {
            available: false,
            ..Self::default()
        }
    }

    fn next_id(&mut self, prefix: &str) -> String {
        self.counter += 1;
        format!("{prefix}{}", self.counter)
    }

    /// The Python's `_fail()`: consume the pending failure, if any.
    fn take_failure(&mut self) -> Option<TerminalError> {
        self.fail_layout.take().map(TerminalError::refused)
    }

    fn record(&mut self, kind: &str, id: &str) {
        self.layout_calls.push((kind.to_string(), id.to_string()));
    }
}

impl Terminal for FakeTerminal {
    fn version(&self) -> String {
        "tmux 3.4 (fake)".to_string()
    }

    fn is_available(&self) -> bool {
        self.available
    }

    fn server_error_hint(&self) -> String {
        "tmux server not running - start it with `tmux new -d`".to_string()
    }

    fn list_panes(&self) -> Vec<Pane> {
        self.panes.clone()
    }

    fn get_pane(&self, pane_id: &str) -> Option<Pane> {
        self.panes
            .iter()
            .find(|pane| pane.pane_id == pane_id)
            .cloned()
    }

    fn list_tabs(&self) -> Vec<Tab> {
        self.tabs.clone()
    }

    fn list_workspaces(&self) -> Vec<Workspace> {
        self.workspaces.clone()
    }

    fn list_panes_grouped(&self) -> BTreeMap<String, Vec<Pane>> {
        let mut grouped: BTreeMap<String, Vec<Pane>> = BTreeMap::new();
        for pane in &self.panes {
            grouped
                .entry(pane.workspace.clone())
                .or_default()
                .push(pane.clone());
        }
        grouped
    }

    fn try_list_panes_grouped(&self) -> Result<BTreeMap<String, Vec<Pane>>, TerminalError> {
        if let Some(error) = &self.fail_grouped_panes {
            return Err(error.clone());
        }
        Ok(self.list_panes_grouped())
    }

    fn agent_child_pids(&self, pane_pid: i64) -> Result<Vec<i64>, TerminalError> {
        Ok(self.children.get(&pane_pid).cloned().unwrap_or_default())
    }

    fn create_workspace(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Workspace, TerminalError> {
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        let workspace_id = self.next_id("w");
        let workspace = Workspace {
            workspace_id: workspace_id.clone(),
            label: label.unwrap_or("dunkingsheep").to_string(),
            index: self.workspaces.len() as i64 + 1,
        };
        self.workspaces.push(workspace.clone());
        self.record("create_workspace", &workspace_id);
        let _ = (cwd, focus); // the fake records the intent, the mutation is the list
        Ok(workspace)
    }

    fn create_tab(
        &mut self,
        workspace_id: Option<&str>,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Tab, TerminalError> {
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        let tab_id = self.next_id("w");
        let tab = Tab {
            workspace_id: workspace_id.unwrap_or("w1").to_string(),
            label: label.unwrap_or("tab").to_string(),
            index: self.tabs.len() as i64 + 1,
            active: focus,
            tab_id: tab_id.clone(),
        };
        self.tabs.push(tab.clone());
        self.record("create_tab", &tab_id);
        let _ = cwd;
        Ok(tab)
    }

    fn split_pane(
        &mut self,
        pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
    ) -> Result<Pane, TerminalError> {
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        let source = self
            .get_pane(pane_id)
            .ok_or_else(|| TerminalError::refused(format!("no such pane {pane_id}")))?;
        let pane_id = self.next_id("w9:p");
        let pane = Pane {
            pane_id: pane_id.clone(),
            cwd: cwd.map(str::to_string).unwrap_or(source.cwd.clone()),
            command: source.command.clone(),
            label: format!("{} ({direction})", source.label),
            title: format!("{} ({direction})", source.title),
            active: true,
            width: if direction == "down" {
                source.width
            } else {
                source.width / 2
            },
            height: if direction == "down" {
                source.height / 2
            } else {
                source.height
            },
            ..source
        };
        self.panes.push(pane.clone());
        self.record("split_pane", &pane_id);
        Ok(pane)
    }

    fn start_agent(&mut self, spec: &AgentSpec) -> Result<Pane, TerminalError> {
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        let pane_id = spec.pane_id_hint();
        let pane = Pane {
            pane_id: pane_id.clone(),
            session_id: "s1".into(),
            window_id: "w1".into(),
            cwd: spec
                .cwd
                .clone()
                .unwrap_or_else(|| "/home/hunter/work".into()),
            command: spec.argv.first().cloned().unwrap_or_default(),
            label: spec.label.clone().unwrap_or_else(|| spec.name.clone()),
            title: spec.label.clone().unwrap_or_else(|| spec.name.clone()),
            active: true,
            workspace: "work".into(),
            tab: "1".into(),
            window_index: 1,
            workspace_index: 1,
            pane_pid: Some(9000 + self.counter),
            width: 120,
            height: 40,
        };
        self.panes.push(pane.clone());
        self.record("start_agent", &pane_id);
        Ok(pane)
    }

    fn rename(&mut self, kind: &str, target_id: &str, label: &str) -> Result<(), TerminalError> {
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        match kind {
            "tab" => {
                let tab = self
                    .tabs
                    .iter_mut()
                    .find(|tab| tab.tab_id == target_id)
                    .ok_or_else(|| TerminalError::refused(format!("no such tab {target_id}")))?;
                tab.label = label.to_string();
            }
            "workspace" => {
                let workspace = self
                    .workspaces
                    .iter_mut()
                    .find(|workspace| workspace.workspace_id == target_id)
                    .ok_or_else(|| {
                        TerminalError::refused(format!("no such workspace {target_id}"))
                    })?;
                workspace.label = label.to_string();
            }
            other => return Err(TerminalError::refused(format!("cannot rename a {other}"))),
        }
        self.record("rename", target_id);
        Ok(())
    }

    fn close(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        match kind {
            "pane" => self.panes.retain(|pane| pane.pane_id != target_id),
            "tab" => self.tabs.retain(|tab| tab.tab_id != target_id),
            "workspace" => self.workspaces.retain(|w| w.workspace_id != target_id),
            other => return Err(TerminalError::refused(format!("cannot close a {other}"))),
        }
        self.record("close", target_id);
        Ok(())
    }

    fn focus(&mut self, kind: &str, target_id: &str) -> Result<(), TerminalError> {
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        self.record(&format!("focus:{kind}"), target_id);
        Ok(())
    }

    fn resize(&mut self, pane_id: &str, cols: i64, rows: i64) -> Result<Resize, TerminalError> {
        // Same refusals as the tmux backend, so a daemon test can assert the SHAPE of
        // the failure instead of only the happy path.
        if !(RESIZE_MIN..=RESIZE_MAX).contains(&cols) || !(RESIZE_MIN..=RESIZE_MAX).contains(&rows)
        {
            return Err(TerminalError::refused(format!(
                "cols and rows must be between {RESIZE_MIN} and {RESIZE_MAX}"
            )));
        }
        if self.get_pane(pane_id).is_none() {
            return Err(TerminalError::refused(format!("no such pane {pane_id}")));
        }
        if let Some(error) = self.take_failure() {
            return Err(error);
        }
        self.record("resize", pane_id);
        let window_id = self
            .get_pane(pane_id)
            .map(|pane| pane.tab_id())
            .unwrap_or_default();
        let mut resized = Vec::new();
        for pane in &mut self.panes {
            if pane.tab_id() != window_id {
                continue;
            }
            if !pane.pane_id.eq(pane_id) {
                continue; // the fake moves the window grid the way tmux does: this pane
            }
            let (cols_before, rows_before) = (pane.width, pane.height);
            pane.width = cols;
            pane.height = rows;
            resized.push(ResizedPane {
                pane_id: pane.pane_id.clone(),
                cols,
                rows,
                cols_before: Some(cols_before),
                rows_before: Some(rows_before),
            });
        }
        Ok(Resize {
            pane_id: pane_id.to_string(),
            requested_cols: cols,
            requested_rows: rows,
            cols: Some(cols),
            rows: Some(rows),
            window_pane_count: self
                .panes
                .iter()
                .filter(|p| p.tab_id() == window_id)
                .count(),
            resized_panes: resized,
            clients_attached: 0,
            overridable_by_client: false,
        })
    }

    fn run_command(&mut self, pane_id: &str, command: &str) -> Result<(), TerminalError> {
        if !self.available {
            return Err(TerminalError::unavailable(self.server_error_hint()));
        }
        self.runs.push((pane_id.to_string(), command.to_string()));
        Ok(())
    }

    fn send_text(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
        if !self.available {
            return Err(TerminalError::unavailable(self.server_error_hint()));
        }
        self.sends.push((pane_id.to_string(), text.to_string()));
        Ok(())
    }

    fn send_text_and_enter(&mut self, pane_id: &str, text: &str) -> Result<(), TerminalError> {
        self.send_text(pane_id, text)?;
        self.submits.push((pane_id.to_string(), text.to_string()));
        Ok(())
    }

    fn send_keys(&mut self, pane_id: &str, keys: &[&str]) -> Result<(), TerminalError> {
        if !self.available {
            return Err(TerminalError::unavailable(self.server_error_hint()));
        }
        self.keys_sent.push((
            pane_id.to_string(),
            keys.iter().map(|key| key.to_string()).collect(),
        ));
        Ok(())
    }

    fn read_pane(
        &self,
        pane_id: &str,
        lines: i64,
        ansi: bool,
    ) -> Result<Option<String>, TerminalError> {
        // The Python's order: an unknown pane is None, an unreadable pane is None,
        // then the pinned screen (tail-truncated), else the echo of what we sent.
        if self.get_pane(pane_id).is_none() {
            return Ok(None);
        }
        if self.unreadable.contains(pane_id) {
            return Ok(None);
        }
        if let Some(screen) = self.screens.get(pane_id) {
            let tail: Vec<&str> = screen
                .lines()
                .rev()
                .take(lines.max(0) as usize)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let text = tail.join("\n");
            let _ = ansi; // the fake stores plain screens and says so
            return Ok(Some(text));
        }
        let mine: Vec<&str> = self
            .sends
            .iter()
            .filter(|(pane, _text)| pane == pane_id)
            .map(|(_pane, text)| text.as_str())
            .collect();
        let tail: Vec<&str> = mine
            .iter()
            .rev()
            .take(lines.max(0) as usize)
            .rev()
            .copied()
            .collect();
        Ok(Some(tail.join("\n")))
    }

    fn agent_status(&self, pane_id: &str) -> Option<String> {
        if let Some(status) = self.statuses.get(pane_id) {
            return Some(status.clone());
        }
        self.get_pane(pane_id).map(|_| "unknown".to_string())
    }
}

impl AgentSpec {
    /// The id the fake gives a new agent's pane: derived from the name so a test can
    /// predict it, and stable across a re-run.
    fn pane_id_hint(&self) -> String {
        format!("w1:p-{}", self.name)
    }
}
