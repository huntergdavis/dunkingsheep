//! Ports (by name, layout halves only) of tests/test_dunk_core.py: the create_*/split/rename/
//! focus/failure tests, against a dict-based fake of `tests/fakes.py::FakeHerdr`.
//! The `close` halves, the dunk-target assertions (`flock.add`), and `start_agent` are in
//! later slices.

use super::*;

struct FakeLayout {
    panes: Vec<Value>,
    tabs: Vec<Value>,
    workspaces: Vec<Value>,
    layout_calls: Vec<Vec<Value>>,
    fail_layout: Option<String>,
    counter: i64,
}

impl FakeLayout {
    fn new() -> Self {
        let pane = |id: &str,
                    tab: &str,
                    ws: &str,
                    cwd: &str,
                    agent: Option<&str>,
                    title: &str,
                    term: &str| {
            let mut p = json!({"pane_id": id, "tab_id": tab, "workspace_id": ws, "cwd": cwd,
                "agent_status": "unknown", "terminal_id": term, "terminal_title_stripped": title});
            if let Some(a) = agent {
                p["agent"] = json!(a);
            }
            p
        };
        let tab = |id: &str, label: &str, number: i64| {
            json!({"tab_id": id, "label": label, "number": number,
                   "workspace_id": id.split(':').next().unwrap()})
        };
        FakeLayout {
            panes: vec![
                pane("w1:p1", "w1:t1", "w1", "/home/x", None, "shell", "term_a"),
                pane(
                    "w1:p2",
                    "w1:t2",
                    "w1",
                    "/home/x/proj",
                    Some("claude"),
                    "claude proj",
                    "term_b",
                ),
                pane(
                    "w2:p1",
                    "w2:t1",
                    "w2",
                    "/home/x/site",
                    Some("codex"),
                    "site",
                    "term_c",
                ),
                pane(
                    "w2:p2",
                    "w2:t2",
                    "w2",
                    "/home/x/sheep",
                    None,
                    "shell",
                    "term_d",
                ),
                pane(
                    "w2:p3",
                    "w2:t3",
                    "w2",
                    "/home/x/other",
                    Some("claude"),
                    "Sheep release work",
                    "term_e",
                ),
            ],
            tabs: vec![
                tab("w1:t1", "shell", 1),
                tab("w1:t2", "Claude Proj", 2),
                tab("w2:t1", "Codex Site", 1),
                tab("w2:t2", "Sheep", 2),
                tab("w2:t3", "Other", 3),
            ],
            workspaces: vec![
                json!({"workspace_id": "w1", "label": "alpha", "number": 1}),
                json!({"workspace_id": "w2", "label": "beta", "number": 2}),
            ],
            layout_calls: vec![],
            fail_layout: None,
            counter: 100,
        }
    }

    fn failed(&mut self) -> Option<String> {
        self.fail_layout.take()
    }

    fn new_pane(&mut self, workspace_id: &str, tab_id: &str, cwd: Option<&str>) -> Value {
        self.counter += 1;
        let pane = json!({"pane_id": format!("{workspace_id}:p{}", self.counter), "tab_id": tab_id,
            "workspace_id": workspace_id, "cwd": cwd.unwrap_or("/home/x"), "agent_status": "unknown",
            "terminal_id": format!("term_{}", self.counter), "pane_pid": 50000 + self.counter,
            "terminal_title_stripped": "shell", "focused": false});
        self.panes.push(pane.clone());
        pane
    }
}

impl LayoutBackend for FakeLayout {
    /// `herdr.list_panes_grouped()`: panes annotated with their tab/workspace labels and
    /// numbers, sorted by (workspace, tab) number. `resolve_target` matches on these labels.
    fn list_panes(&self) -> Vec<Value> {
        let mut panes = self.panes.clone();
        for pane in panes.iter_mut() {
            let (tab_id, ws_id) = (text(pane, "tab_id"), text(pane, "workspace_id"));
            let tab = self.tabs.iter().find(|t| text(t, "tab_id") == tab_id);
            let ws = self
                .workspaces
                .iter()
                .find(|w| text(w, "workspace_id") == ws_id);
            let label = |v: Option<&Value>, id: &str| match v
                .and_then(|v| v.get("label"))
                .and_then(Value::as_str)
            {
                Some(l) if !l.is_empty() => l.to_string(),
                _ => id.to_string(),
            };
            pane["tab_label"] = json!(label(tab, &tab_id));
            pane["tab_number"] = tab
                .and_then(|t| t.get("number"))
                .cloned()
                .unwrap_or(json!(0));
            pane["workspace_label"] = json!(label(ws, &ws_id));
            pane["workspace_number"] = ws
                .and_then(|w| w.get("number"))
                .cloned()
                .unwrap_or(json!(0));
        }
        panes.sort_by_key(|p| {
            (
                p["workspace_number"].as_i64().unwrap_or(0),
                p["tab_number"].as_i64().unwrap_or(0),
            )
        });
        panes
    }
    fn list_tabs(&self) -> Vec<Value> {
        self.tabs.clone()
    }
    fn list_workspaces(&self) -> Vec<Value> {
        self.workspaces.clone()
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.panes
            .iter()
            .find(|p| text(p, "pane_id") == pane_id)
            .cloned()
    }
    fn server_error_hint(&self) -> String {
        "herdr server not running - start herdr".into()
    }
    fn create_workspace(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Value, String> {
        if let Some(m) = self.failed() {
            return Err(m);
        }
        self.counter += 1;
        let ws_id = format!("w{}", self.counter);
        let number = self.workspaces.len() as i64 + 1;
        let workspace = json!({"workspace_id": ws_id, "label": label.map(str::to_string).unwrap_or(number.to_string()),
            "number": number, "focused": focus, "agent_status": "unknown"});
        let tab = json!({"tab_id": format!("{ws_id}:t1"), "workspace_id": ws_id, "label": "1", "number": 1,
            "focused": focus, "agent_status": "unknown", "pane_count": 1});
        self.workspaces.push(workspace.clone());
        self.tabs.push(tab.clone());
        let pane = self.new_pane(&ws_id, &text(&tab, "tab_id"), cwd);
        self.layout_calls.push(vec![
            json!("create_workspace"),
            json!(label),
            json!(cwd),
            json!(focus),
        ]);
        Ok(
            json!({"type": "workspace_created", "workspace": workspace, "tab": tab, "root_pane": pane}),
        )
    }
    fn create_tab(
        &mut self,
        workspace_id: Option<&str>,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Value, String> {
        if let Some(m) = self.failed() {
            return Err(m);
        }
        let workspace_id = workspace_id
            .map(str::to_string)
            .unwrap_or_else(|| text(&self.workspaces[0], "workspace_id"));
        if !self
            .workspaces
            .iter()
            .any(|w| text(w, "workspace_id") == workspace_id)
        {
            return Err(format!("workspace {workspace_id} not found"));
        }
        let number = self
            .tabs
            .iter()
            .filter(|t| text(t, "workspace_id") == workspace_id)
            .map(|t| t["number"].as_i64().unwrap())
            .max()
            .unwrap_or(0)
            + 1;
        let tab = json!({"tab_id": format!("{workspace_id}:t{number}"), "workspace_id": workspace_id,
            "label": label.map(str::to_string).unwrap_or(number.to_string()), "number": number,
            "focused": focus, "agent_status": "unknown", "pane_count": 1});
        self.tabs.push(tab.clone());
        let pane = self.new_pane(&workspace_id, &text(&tab, "tab_id"), cwd);
        self.layout_calls.push(vec![
            json!("create_tab"),
            json!(workspace_id),
            json!(label),
            json!(cwd),
            json!(focus),
        ]);
        Ok(json!({"type": "tab_created", "tab": tab, "root_pane": pane}))
    }
    fn split_pane(
        &mut self,
        pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Value, String> {
        if let Some(m) = self.failed() {
            return Err(m);
        }
        let origin = self
            .get_pane(pane_id)
            .ok_or_else(|| format!("pane {pane_id} not found"))?;
        let pane = self.new_pane(
            &text(&origin, "workspace_id"),
            &text(&origin, "tab_id"),
            cwd,
        );
        self.layout_calls.push(vec![
            json!("split_pane"),
            json!(pane_id),
            json!(direction),
            json!(cwd),
            json!(focus),
        ]);
        Ok(json!({"type": "pane_info", "pane": pane}))
    }
    fn start_agent(
        &mut self,
        name: &str,
        argv: &[String],
        workspace_id: Option<&str>,
        tab_id: Option<&str>,
        split: Option<&str>,
        cwd: Option<&str>,
        _focus: bool,
    ) -> Result<Value, String> {
        if let Some(m) = self.failed() {
            return Err(m);
        }
        let (workspace_id, tab_id) = if let Some(tab_id) = tab_id {
            let tab = self
                .tabs
                .iter()
                .find(|t| text(t, "tab_id") == tab_id)
                .ok_or_else(|| format!("tab {tab_id} not found"))?;
            (text(tab, "workspace_id"), tab_id.to_string())
        } else {
            let workspace_id = workspace_id
                .map(str::to_string)
                .unwrap_or_else(|| text(&self.workspaces[0], "workspace_id"));
            let created = self.create_tab(Some(&workspace_id), Some(name), cwd, false)?;
            let tab_id = text(&created["tab"], "tab_id");
            let root = text(&created["root_pane"], "pane_id");
            self.panes.retain(|p| text(p, "pane_id") != root);
            (workspace_id, tab_id)
        };
        let pane = self.new_pane(&workspace_id, &tab_id, cwd);
        let pid = text(&pane, "pane_id");
        for stored in self.panes.iter_mut() {
            if text(stored, "pane_id") == pid {
                stored["label"] = json!(name);
                stored["agent"] = json!(name);
                stored["agent_status"] = json!("idle");
            }
        }
        self.layout_calls.push(vec![
            json!("start_agent"),
            json!(name),
            json!(argv),
            json!(workspace_id),
            json!(tab_id),
            json!(split),
        ]);
        let mut agent = self.get_pane(&pid).unwrap();
        agent["name"] = json!(name);
        Ok(json!({"type": "agent_started", "agent": agent, "argv": argv}))
    }
    fn rename(&mut self, kind: &str, target_id: &str, label: &str) -> Result<Value, String> {
        if let Some(m) = self.failed() {
            return Err(m);
        }
        let (items, key) = match kind {
            "workspace" => (&mut self.workspaces, "workspace_id"),
            "tab" => (&mut self.tabs, "tab_id"),
            _ => (&mut self.panes, "pane_id"),
        };
        for item in items.iter_mut() {
            if text(item, key) == target_id {
                item["label"] = json!(label);
                let copy = item.clone();
                self.layout_calls.push(vec![
                    json!("rename"),
                    json!(kind),
                    json!(target_id),
                    json!(label),
                ]);
                return Ok(json!({"type": format!("{kind}_info"), kind: copy}));
            }
        }
        Err(format!("{kind} {target_id} not found"))
    }
    fn focus(&mut self, kind: &str, target_id: &str) -> Result<Value, String> {
        self.layout_calls
            .push(vec![json!("focus"), json!(kind), json!(target_id)]);
        Ok(json!({"type": "ok"}))
    }
}

/// A recording submit: the Python's `herdr.sends`.
fn recorder() -> (std::cell::RefCell<Vec<(String, String)>>,) {
    (std::cell::RefCell::new(Vec::new()),)
}

#[test]
fn test_create_workspace_runs_command_in_its_pane_and_is_dunkable() {
    let mut herdr = FakeLayout::new();
    let (sends,) = recorder();
    let mut submit = |pane: &str, command: &str| {
        sends
            .borrow_mut()
            .push((pane.to_string(), command.to_string()));
        (true, "sent".to_string())
    };
    let created = create_workspace(
        &mut herdr,
        Some("Site"),
        Some("/home/x/site"),
        Some("claude"),
        false,
        &mut submit,
    )
    .unwrap();
    assert_eq!("Site", created["workspace"]["label"]);
    assert_eq!(created["pane"]["pane_id"], created["pane_id"]);
    assert_eq!("claude", created["ran"]);
    assert_eq!(
        vec![(text(&created, "pane_id"), "claude".to_string())],
        *sends.borrow()
    );
    assert_eq!(
        vec![
            json!("create_workspace"),
            json!("Site"),
            json!("/home/x/site"),
            json!(false)
        ],
        herdr.layout_calls[0]
    );
    // "dunkable": the new pane is immediately a valid target by id and by label.
    let panes = herdr.list_panes();
    let hint = herdr.server_error_hint();
    assert_eq!(
        text(&created, "pane_id"),
        text(
            resolve_target(&panes, "Site", None, &hint).unwrap(),
            "pane_id"
        )
    );
}

#[test]
fn test_create_tab_defaults_to_the_callers_workspace() {
    let mut herdr = FakeLayout::new();
    let (sends,) = recorder();
    let mut submit = |pane: &str, command: &str| {
        sends
            .borrow_mut()
            .push((pane.to_string(), command.to_string()));
        (true, "sent".to_string())
    };
    let created = create_tab(
        &mut herdr,
        None,
        Some("writer"),
        None,
        None,
        false,
        Some("w2:p1"),
        &mut submit,
    )
    .unwrap();
    assert_eq!("w2", created["tab"]["workspace_id"]);
    assert_eq!("writer", created["tab"]["label"]);
    assert!(created["ran"].is_null());
    assert!(sends.borrow().is_empty());
    // Explicit workspace by label, from anywhere.
    let other = create_tab(
        &mut herdr,
        Some("alpha"),
        Some("tests"),
        None,
        Some("pytest -q"),
        false,
        None,
        &mut submit,
    )
    .unwrap();
    assert_eq!("w1", other["tab"]["workspace_id"]);
    assert_eq!(
        vec![(text(&other, "pane_id"), "pytest -q".to_string())],
        *sends.borrow()
    );
}

#[test]
fn test_split_pane_validates_direction() {
    let mut herdr = FakeLayout::new();
    let mut submit = |_: &str, _: &str| (true, String::new());
    let created = split_pane(
        &mut herdr,
        "Sheep",
        Some("down"),
        None,
        Some("htop"),
        false,
        None,
        &mut submit,
    )
    .unwrap();
    assert_eq!("w2:t2", created["pane"]["tab_id"]);
    assert_eq!(
        vec![
            json!("split_pane"),
            json!("w2:p2"),
            json!("down"),
            Value::Null,
            json!(false)
        ],
        herdr.layout_calls[0]
    );
    let error = split_pane(
        &mut herdr,
        "Sheep",
        Some("sideways"),
        None,
        None,
        false,
        None,
        &mut submit,
    )
    .unwrap_err()
    .to_string();
    assert!(error.contains("direction"), "{error}");
}

#[test]
fn test_rename_focus_by_kind() {
    // The Python test also closes; `close_target` is a later slice.
    let mut herdr = FakeLayout::new();
    let renamed = rename(&mut herdr, "tab", "Sheep", "Sheep Ops", None).unwrap();
    assert_eq!(
        json!({"kind": "tab", "id": "w2:t2", "label": "Sheep Ops"}),
        json!({"kind": renamed["kind"], "id": renamed["id"], "label": renamed["label"]})
    );
    let tabs = herdr.list_tabs();
    let hint = herdr.server_error_hint();
    assert_eq!(
        "w2:t2",
        text(
            resolve_tab(&tabs, "Sheep Ops", || unreachable!(), &hint).unwrap(),
            "tab_id"
        )
    );
    assert_eq!(
        "w1",
        rename(&mut herdr, "workspace", "alpha", "Alpha Prime", None).unwrap()["id"]
    );
    assert_eq!(
        "w2:p1",
        rename(&mut herdr, "pane", "codex", "codex main", None).unwrap()["id"]
    );
    let error = rename(&mut herdr, "tab", "Other", "  ", None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("label is required"), "{error}");
    let error = rename(&mut herdr, "window", "Other", "x", None)
        .unwrap_err()
        .to_string();
    assert!(error.contains("kind must be"), "{error}");
    assert_eq!(
        json!({"kind": "tab", "id": "w2:t3", "focused": true}),
        focus(&mut herdr, "tab", "Other", None).unwrap()
    );
    assert_eq!(
        vec![json!("focus"), json!("tab"), json!("w2:t3")],
        *herdr.layout_calls.last().unwrap()
    );
}

#[test]
fn test_herdr_failures_become_dunk_errors() {
    // The start_agent line of the Python test belongs to the start_agent slice.
    let mut herdr = FakeLayout::new();
    let mut submit = |_: &str, _: &str| (true, String::new());
    herdr.fail_layout = Some("workspace limit reached".into());
    let error = create_workspace(&mut herdr, Some("x"), None, None, false, &mut submit)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("could not create the workspace x: workspace limit"),
        "{error}"
    );
    herdr.fail_layout = Some("nope".into());
    let error = rename(&mut herdr, "tab", "Sheep", "y", None)
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("could not rename tab w2:t2: nope"),
        "{error}"
    );
}

#[test]
fn a_failed_submit_is_reported_in_error_not_raised() {
    let mut herdr = FakeLayout::new();
    let mut submit = |_: &str, _: &str| (false, "pane not found".to_string());
    let created = create_tab(
        &mut herdr,
        Some("beta"),
        Some("t"),
        None,
        Some("ls"),
        false,
        None,
        &mut submit,
    )
    .unwrap();
    assert!(created["ran"].is_null());
    assert_eq!(
        "created, but running the command failed: pane not found",
        created["error"]
    );
}

#[test]
fn the_sandbox_workspace_is_found_case_insensitively_else_created() {
    let mut herdr = FakeLayout::new();
    herdr
        .workspaces
        .push(json!({"workspace_id": "w7", "label": "SandBox", "number": 3}));
    assert_eq!("w7", sandbox_workspace_id(&mut herdr, None).unwrap());
    assert!(
        herdr.layout_calls.is_empty(),
        "an existing sandbox is reused, nothing created"
    );
    let created = sandbox_workspace_id(&mut herdr, Some("scratch")).unwrap();
    assert!(created.starts_with('w'));
    assert_eq!(json!("create_workspace"), herdr.layout_calls[0][0]);
    assert_eq!(json!("scratch"), herdr.layout_calls[0][1]);
    herdr.fail_layout = Some("limit".into());
    let error = sandbox_workspace_id(&mut herdr, Some("other"))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("could not create sandbox workspace \"other\": limit"),
        "{error}"
    );
}

fn flock_for_tests(tag: &str) -> Flock {
    let dir = std::env::temp_dir().join(format!("ds-layout-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    Flock::new(dir.join("launches.json"), dir.join("lanes.json"), false)
}

fn no_verify() -> impl FnMut(&str, Option<&str>, &str) -> Result<Option<i64>, DunkError> {
    |_, _, _| Ok(None)
}

fn no_register(
) -> impl FnMut(&str, &str, Option<&str>, Option<&str>, &str) -> Result<Value, DunkError> {
    |_, _, _, _, _| Ok(Value::Null)
}

#[test]
fn test_start_agent_registers_by_name_and_places_it() {
    let mut herdr = FakeLayout::new();
    let flock = flock_for_tests("start-by-name");
    let (mut verify, mut register) = (no_verify(), no_register());
    let started = start_agent(
        &mut herdr,
        &flock,
        "writer",
        "claude --model opus",
        Some("beta"),
        None,
        None,
        Some("/home/x/site"),
        false,
        None,
        None,
        None,
        None,
        None,
        StartHooks {
            verify: &mut verify,
            lane_register: &mut register,
        },
    )
    .unwrap();
    assert_eq!("writer", started["name"]);
    assert_eq!(json!(["claude", "--model", "opus"]), started["argv"]);
    let call = herdr.layout_calls.last().unwrap();
    assert_eq!(json!("start_agent"), call[0]);
    assert_eq!(
        (
            json!("writer"),
            json!(["claude", "--model", "opus"]),
            json!("w2")
        ),
        (call[1].clone(), call[2].clone(), call[3].clone())
    );
    // The backend now lists it as an agent pane, so it resolves as a target by name.
    let panes = herdr.list_panes();
    let hint = herdr.server_error_hint();
    assert_eq!(
        started["pane_id"],
        json!(text(
            resolve_target(&panes, "writer", None, &hint).unwrap(),
            "pane_id"
        ))
    );
    let (mut verify, mut register) = (no_verify(), no_register());
    let e = start_agent(
        &mut herdr,
        &flock,
        "x",
        "",
        None,
        None,
        None,
        None,
        false,
        None,
        None,
        None,
        None,
        None,
        StartHooks {
            verify: &mut verify,
            lane_register: &mut register,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("command is required"), "{e}");
    let e = start_agent(
        &mut herdr,
        &flock,
        "x",
        "claude",
        None,
        Some("Sheep"),
        Some("left"),
        None,
        false,
        None,
        None,
        None,
        None,
        None,
        StartHooks {
            verify: &mut verify,
            lane_register: &mut register,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("split must be"), "{e}");
}

#[test]
fn test_start_agent_into_a_tab_with_split() {
    let mut herdr = FakeLayout::new();
    let flock = flock_for_tests("start-split");
    let (mut verify, mut register) = (no_verify(), no_register());
    let started = start_agent(
        &mut herdr,
        &flock,
        "helper",
        "codex",
        None,
        Some("Sheep"),
        Some("right"),
        None,
        false,
        None,
        None,
        None,
        None,
        None,
        StartHooks {
            verify: &mut verify,
            lane_register: &mut register,
        },
    )
    .unwrap();
    assert_eq!("w2:t2", started["agent"]["tab_id"]);
    let call = herdr.layout_calls.last().unwrap();
    assert_eq!(json!("w2:t2"), call[4]);
    assert_eq!(json!("right"), call[5]);
}

#[test]
fn start_agent_failure_and_the_sandbox_default_and_the_launch_record() {
    // The start_agent line of test_herdr_failures_become_dunk_errors, the default-to-sandbox
    // rule (2026-10-18) and role registration. (The launch record it also writes has no
    // public reader on Flock yet, so it is not asserted here.)
    let mut herdr = FakeLayout::new();
    let flock = flock_for_tests("start-misc");
    let (mut verify, mut register) = (no_verify(), no_register());
    herdr.fail_layout = Some("nope".into());
    let e = start_agent(
        &mut herdr,
        &flock,
        "a",
        "claude",
        Some("beta"),
        None,
        None,
        None,
        false,
        None,
        None,
        None,
        None,
        None,
        StartHooks {
            verify: &mut verify,
            lane_register: &mut register,
        },
    )
    .unwrap_err()
    .to_string();
    assert!(e.contains("could not start agent 'a': nope"), "{e}");
    // No tab and no workspace: the sandbox workspace is created and used.
    let mut seen = Vec::new();
    let mut verify = |name: &str, pane: Option<&str>, kind: &str| {
        seen.push((name.to_string(), pane.map(str::to_string), kind.to_string()));
        Ok(Some(4242))
    };
    let mut registered = Vec::new();
    let mut register =
        |pane: &str, role: &str, provider: Option<&str>, model: Option<&str>, label: &str| {
            registered.push((
                pane.to_string(),
                role.to_string(),
                provider.map(str::to_string),
                model.map(str::to_string),
                label.to_string(),
            ));
            Ok(json!({"role": role}))
        };
    let started = start_agent(
        &mut herdr,
        &flock,
        "  scribe ",
        "/opt/bin/pi --offline",
        None,
        None,
        None,
        Some("/tmp"),
        false,
        None,
        Some("worker"),
        Some("p"),
        Some("m"),
        Some("scratch"),
        StartHooks {
            verify: &mut verify,
            lane_register: &mut register,
        },
    )
    .unwrap();
    assert_eq!(
        json!(["create_workspace"]),
        json!([herdr
            .layout_calls
            .iter()
            .find(|c| c[0] == json!("create_workspace"))
            .unwrap()[0]])
    );
    assert_eq!(4242, started["child_pid"]);
    assert_eq!(json!({"role": "worker"}), started["role"]);
    assert_eq!(
        vec![(
            "scribe".to_string(),
            started["pane_id"].as_str().map(str::to_string),
            "pi".to_string()
        )],
        seen
    );
    assert_eq!(1, registered.len());
    assert_eq!(
        ("worker", Some("p"), Some("m"), "scribe"),
        (
            registered[0].1.as_str(),
            registered[0].2.as_deref(),
            registered[0].3.as_deref(),
            registered[0].4.as_str()
        )
    );
}

#[test]
fn shlex_join_matches_pythons_quote_rules() {
    let join = |a: &[&str]| shlex_join(&a.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    assert_eq!("claude --model opus", join(&["claude", "--model", "opus"]));
    assert_eq!("a,b x_y", join(&["a,b", "x_y"])); // ',' and '_' are safe in Python
    assert_eq!("'it'\"'\"'s' ''", join(&["it's", ""]));
    assert_eq!("'a b' 'c$d'", join(&["a b", "c$d"]));
}
