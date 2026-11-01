//! T37 (slice 1): `flock/commands_layout.rs` - the layout commands of `dunk_core.Flock`:
//! `create_workspace`, `create_tab`, `split_pane`, `_after_create`, `_sandbox_workspace_id`,
//! `_resolve_kind`, `rename`, `focus`.
//!
//! The Python talks to `self.herdr`, whose calls return `(ok, result)` with a result DICT.
//! The Rust `terminal::Terminal` trait is typed, and the rest of `flock/` (panes.rs,
//! resolve.rs) works on dicts, so this slice keeps the dict contract behind a small
//! [`LayoutBackend`]; an adapter over `Terminal` is a later piece. Everything the Python
//! reads from `self` besides the backend is passed in: `submit` stands for `submit_text`
//! (the confirmed send), `self_pane_id` for the caller.
//!
//! Slice 3 adds `start_agent` (its start verification is `agent_verify.rs`, injected).
//!
//! NOT in this slice: `close_target` (it halts dunkers, which `Flock` does not hold yet),
//! `respawn_pane`, `retire_pane` and `last_message`. The `mailbox`/`role` write-back of
//! `create_workspace` is `Flock::create_workspace_with_lane` (t1420).

use serde_json::{json, Map, Value};

use super::resolve::{resolve_tab, resolve_target, resolve_workspace};
use super::Flock;
use crate::error::DunkError;

/// The calls `Flock` makes on `self.herdr` for layout. Mutations return the backend's
/// result dict, or its message as `Err` (the Python's `(False, message)`).
pub trait LayoutBackend {
    /// The panes ANNOTATED with `tab_label` / `workspace_label` (+ numbers), i.e.
    /// `herdr.list_panes_grouped()`: `resolve_target` matches on those labels.
    fn list_panes(&self) -> Vec<Value>;
    fn list_tabs(&self) -> Vec<Value>;
    fn list_workspaces(&self) -> Vec<Value>;
    fn get_pane(&self, pane_id: &str) -> Option<Value>;
    fn server_error_hint(&self) -> String;
    fn create_workspace(
        &mut self,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Value, String>;
    fn create_tab(
        &mut self,
        workspace_id: Option<&str>,
        label: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Value, String>;
    fn split_pane(
        &mut self,
        pane_id: &str,
        direction: &str,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Value, String>;
    #[allow(clippy::too_many_arguments)]
    fn start_agent(
        &mut self,
        name: &str,
        argv: &[String],
        workspace_id: Option<&str>,
        tab_id: Option<&str>,
        split: Option<&str>,
        cwd: Option<&str>,
        focus: bool,
    ) -> Result<Value, String>;
    fn rename(&mut self, kind: &str, target_id: &str, label: &str) -> Result<Value, String>;
    fn focus(&mut self, kind: &str, target_id: &str) -> Result<Value, String>;
}

/// `submit_text(pane_id, command) -> (ran, message)`.
pub type Submit<'a> = &'a mut dyn FnMut(&str, &str) -> (bool, String);

fn refused<T>(text: String) -> Result<T, DunkError> {
    Err(DunkError::Refused(text))
}

fn text(v: &Value, key: &str) -> String {
    match v.get(key) {
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// `_self_pane`.
fn self_pane(backend: &dyn LayoutBackend, self_pane_id: Option<&str>) -> Result<Value, DunkError> {
    match self_pane_id.filter(|s| !s.is_empty()) {
        None => refused("'self' needs TMUX_PANE (or HERDR_PANE_ID on the herdr rollback; run from inside a pane)".into()),
        Some(id) => backend
            .get_pane(id)
            .ok_or_else(|| DunkError::Refused(format!("own pane {id} not found on the current backend"))),
    }
}

/// `_after_create`: shape the backend result and, when `command` is given, type it into the
/// new pane through the confirmed submit. A failed submit is reported in `error`, not raised.
pub fn after_create(
    result: Result<Value, String>,
    command: Option<&str>,
    what: &str,
    submit: Submit<'_>,
) -> Result<Value, DunkError> {
    let result = match result {
        Ok(r) => r,
        Err(message) => {
            return refused(format!(
                "the terminal backend could not create the {what}: {message}"
            ))
        }
    };
    let pane = ["root_pane", "pane", "agent"]
        .iter()
        .find_map(|k| {
            result
                .get(k)
                .filter(|v| v.as_object().is_some_and(|o| !o.is_empty()))
        })
        .cloned()
        .unwrap_or_else(|| json!({}));
    let pane_id = pane.get("pane_id").cloned().unwrap_or(Value::Null);
    let mut out = Map::new();
    out.insert(
        "workspace".into(),
        result.get("workspace").cloned().unwrap_or(Value::Null),
    );
    out.insert(
        "tab".into(),
        result.get("tab").cloned().unwrap_or(Value::Null),
    );
    out.insert("pane".into(), pane.clone());
    out.insert("pane_id".into(), pane_id.clone());
    out.insert("ran".into(), Value::Null);
    if let (Some(command), Some(id)) = (
        command.filter(|c| !c.is_empty()),
        pane_id.as_str().filter(|s| !s.is_empty()),
    ) {
        let (ran, message) = submit(id, command);
        if ran {
            out.insert("ran".into(), json!(command));
        } else {
            out.insert(
                "error".into(),
                json!(format!(
                    "created, but running the command failed: {message}"
                )),
            );
        }
    }
    Ok(Value::Object(out))
}

/// `create_workspace` (without the `mailbox`/`role` write-back, see the module note).
pub fn create_workspace(
    backend: &mut dyn LayoutBackend,
    label: Option<&str>,
    cwd: Option<&str>,
    command: Option<&str>,
    focus: bool,
    submit: Submit<'_>,
) -> Result<Value, DunkError> {
    let result = backend.create_workspace(label, cwd, focus);
    let what = format!("workspace {}", label.unwrap_or(""))
        .trim()
        .to_string();
    after_create(result, command, &what, submit)
}

/// `_title_pane`: best effort, put the lane name in the pane title too. A title is a courtesy
/// (the agent may replace it); the registry is the record. A failed rename is `false`, never an
/// error, and never undoes the registration that preceded it.
pub fn title_pane(backend: &mut dyn LayoutBackend, pane_id: &str, mailbox: &str) -> bool {
    backend.rename("pane", pane_id, mailbox).is_ok()
}

impl Flock {
    /// `create_workspace` with its `mailbox`/`role` write-back. `mailbox` names the lane the new
    /// pane IS and is written into the lane registry as soon as the pane exists (declared), the
    /// write-back a backend flip needs: without it the only record of the lane is a title the
    /// agent may overwrite, and a restore produces a pane nobody can address. `role` registers
    /// the pane through `lane_register` (owner/system only) and reports under `out["role"]`; a
    /// refusal there propagates AFTER the workspace exists, as in the Python.
    #[allow(clippy::too_many_arguments)]
    pub fn create_workspace_with_lane(
        &self,
        backend: &mut dyn LayoutBackend,
        label: Option<&str>,
        cwd: Option<&str>,
        command: Option<&str>,
        focus: bool,
        mailbox: Option<&str>,
        role: Option<&str>,
        provider: Option<&str>,
        model: Option<&str>,
        self_pane_id: Option<&str>,
        submit: Submit<'_>,
        env: &dyn super::panes::PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        let mut out = create_workspace(backend, label, cwd, command, focus, submit)?;
        let pane_id = out
            .get("pane_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string);
        // `if mailbox and out.get("pane_id")`: truthiness of the raw string, then `.strip()`.
        let mailbox = mailbox.filter(|m| !m.is_empty());
        if let (Some(mailbox), Some(pane)) = (mailbox, pane_id.as_deref()) {
            self.remember_lane(pane, mailbox.trim(), true);
            title_pane(backend, pane, mailbox);
        }
        if let (Some(role), Some(pane)) = (role.filter(|r| !r.is_empty()), pane_id.as_deref()) {
            let lane_label = mailbox.or(label.filter(|l| !l.is_empty()));
            let registered = self.lane_register(
                Some(pane),
                Some(role),
                provider,
                model,
                lane_label,
                self_pane_id,
                env,
            )?;
            out["role"] = registered;
        }
        Ok(out)
    }
}

/// `create_tab`: `workspace` is an id, label or 'self'; omitted means the caller's own
/// workspace when known, else the backend's focused one.
pub fn create_tab(
    backend: &mut dyn LayoutBackend,
    workspace: Option<&str>,
    label: Option<&str>,
    cwd: Option<&str>,
    command: Option<&str>,
    focus: bool,
    self_pane_id: Option<&str>,
    submit: Submit<'_>,
) -> Result<Value, DunkError> {
    let workspace_id: Option<String> = if let Some(spec) = workspace.filter(|w| !w.is_empty()) {
        let workspaces = backend.list_workspaces();
        let hint = backend.server_error_hint();
        let found = resolve_workspace(
            &workspaces,
            spec,
            || self_pane(backend, self_pane_id),
            &hint,
        )?;
        Some(text(found, "workspace_id"))
    } else if let Some(id) = self_pane_id.filter(|s| !s.is_empty()) {
        backend.get_pane(id).map(|own| text(&own, "workspace_id"))
    } else {
        None
    };
    let result = backend.create_tab(workspace_id.as_deref(), label, cwd, focus);
    let what = format!("tab {}", label.unwrap_or("")).trim().to_string();
    after_create(result, command, &what, submit)
}

/// `split_pane`: split an existing pane (any pane target) right or down.
pub fn split_pane(
    backend: &mut dyn LayoutBackend,
    target: &str,
    direction: Option<&str>,
    cwd: Option<&str>,
    command: Option<&str>,
    focus: bool,
    self_pane_id: Option<&str>,
    submit: Submit<'_>,
) -> Result<Value, DunkError> {
    let direction = direction
        .filter(|d| !d.is_empty())
        .unwrap_or("right")
        .to_lowercase();
    if direction != "right" && direction != "down" {
        return refused("direction must be 'right' or 'down'".into());
    }
    let panes = backend.list_panes();
    let hint = backend.server_error_hint();
    let pane_id = text(
        resolve_target(&panes, target, self_pane_id, &hint)?,
        "pane_id",
    );
    let result = backend.split_pane(&pane_id, &direction, cwd, focus);
    after_create(result, command, "pane", submit)
}

/// `_sandbox_workspace_id`: the dedicated workspace where spawned agents go by default
/// (the 2026-10-18 incident: never default into the caller's tab). `label` is
/// `$DUNKINGSHEEP_SANDBOX_WORKSPACE`, default "sandbox".
pub fn sandbox_workspace_id(
    backend: &mut dyn LayoutBackend,
    env_label: Option<&str>,
) -> Result<String, DunkError> {
    let label = env_label
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .unwrap_or("sandbox")
        .to_string();
    for workspace in backend.list_workspaces() {
        if text(&workspace, "label").trim().to_lowercase() == label.to_lowercase() {
            return Ok(text(&workspace, "workspace_id"));
        }
    }
    let result = backend
        .create_workspace(Some(&label), None, false)
        .map_err(|m| {
            DunkError::Refused(format!("could not create sandbox workspace {label:?}: {m}"))
        })?;
    let id = result
        .get("workspace")
        .map(|w| text(w, "workspace_id"))
        .unwrap_or_default();
    if id.is_empty() {
        return refused(format!(
            "sandbox workspace {label:?} was created without an id"
        ));
    }
    Ok(id)
}

/// `_resolve_kind`: `(kind, id, item)` for a workspace, tab or pane target.
pub fn resolve_kind(
    backend: &dyn LayoutBackend,
    kind: &str,
    target: &str,
    self_pane_id: Option<&str>,
) -> Result<(String, String, Value), DunkError> {
    let kind = kind.trim().to_lowercase();
    let hint = backend.server_error_hint();
    match kind.as_str() {
        "workspace" => {
            let items = backend.list_workspaces();
            let item =
                resolve_workspace(&items, target, || self_pane(backend, self_pane_id), &hint)?;
            Ok((kind, text(item, "workspace_id"), item.clone()))
        }
        "tab" => {
            let items = backend.list_tabs();
            let item = resolve_tab(&items, target, || self_pane(backend, self_pane_id), &hint)?;
            Ok((kind, text(item, "tab_id"), item.clone()))
        }
        "pane" => {
            let items = backend.list_panes();
            let item = resolve_target(&items, target, self_pane_id, &hint)?;
            Ok((kind, text(item, "pane_id"), item.clone()))
        }
        _ => refused("kind must be 'workspace', 'tab' or 'pane'".into()),
    }
}

/// Python `shlex.quote`: `[A-Za-z0-9_@%+=:,./-]` is safe, anything else is single-quoted
/// with `'` written as `'"'"'` (byte-for-byte what the launch record keeps).
fn shlex_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c))
    {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

/// Python `shlex.join`.
pub fn shlex_join(argv: &[String]) -> String {
    argv.iter()
        .map(|a| shlex_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// What `start_agent` needs besides the layout backend: the start check (`agent_verify`),
/// the lane registration (`lane_register`, unported) and the launch record.
pub struct StartHooks<'a> {
    /// `_verify_agent_started(name, pane_id, kind)`: the confirmed child pid, or `None`.
    pub verify: &'a mut dyn FnMut(&str, Option<&str>, &str) -> Result<Option<i64>, DunkError>,
    /// `lane_register(pane, role, provider, model, label)`; only called when `role` is given.
    pub lane_register:
        &'a mut dyn FnMut(&str, &str, Option<&str>, Option<&str>, &str) -> Result<Value, DunkError>,
}

/// `start_agent`: launch `command` in a new pane registered by name. Placement: inside `tab`
/// (splitting it when `split` is right/down), else in `workspace` (id, label or 'self'),
/// else in the dedicated sandbox workspace (`sandbox_label` is
/// `$DUNKINGSHEEP_SANDBOX_WORKSPACE`).
#[allow(clippy::too_many_arguments)]
pub fn start_agent(
    backend: &mut dyn LayoutBackend,
    flock: &Flock,
    name: &str,
    command: &str,
    workspace: Option<&str>,
    tab: Option<&str>,
    split: Option<&str>,
    cwd: Option<&str>,
    focus: bool,
    self_pane_id: Option<&str>,
    role: Option<&str>,
    provider: Option<&str>,
    model: Option<&str>,
    sandbox_label: Option<&str>,
    hooks: StartHooks<'_>,
) -> Result<Value, DunkError> {
    let name = name.trim();
    if name.is_empty() {
        return refused("agent name is required".into());
    }
    let argv = crate::usage::shlex_split(command)
        .map_err(|e| DunkError::Refused(format!("No closing quotation: {e}")))?;
    if argv.is_empty() {
        return refused("command is required (e.g. 'claude' or 'codex --model gpt-5')".into());
    }
    let (mut workspace_id, mut tab_id): (Option<String>, Option<String>) = (None, None);
    let tab = tab.filter(|t| !t.is_empty());
    let workspace = workspace.filter(|w| !w.is_empty());
    if let Some(spec) = tab {
        let tabs = backend.list_tabs();
        let hint = backend.server_error_hint();
        let found = resolve_tab(&tabs, spec, || self_pane(backend, self_pane_id), &hint)?;
        tab_id = Some(text(found, "tab_id"));
    }
    if let Some(spec) = workspace {
        let workspaces = backend.list_workspaces();
        let hint = backend.server_error_hint();
        let found = resolve_workspace(
            &workspaces,
            spec,
            || self_pane(backend, self_pane_id),
            &hint,
        )?;
        workspace_id = Some(text(found, "workspace_id"));
    }
    if tab.is_none() && workspace.is_none() {
        workspace_id = Some(sandbox_workspace_id(backend, sandbox_label)?);
    }
    let split = split.filter(|s| !s.is_empty());
    if let Some(split) = split {
        if split != "right" && split != "down" {
            return refused("split must be 'right' or 'down'".into());
        }
    }
    let result = backend
        .start_agent(
            name,
            &argv,
            workspace_id.as_deref(),
            tab_id.as_deref(),
            split,
            cwd,
            focus,
        )
        .map_err(|m| {
            DunkError::Refused(format!(
                "the terminal backend could not start agent {}: {m}",
                super::agent_verify::repr_name(name)
            ))
        })?;
    let agent = result.get("agent").cloned().unwrap_or_else(|| json!({}));
    let pane_id = agent
        .get("pane_id")
        .and_then(Value::as_str)
        .map(str::to_string);
    // VERIFY BEFORE CLAIMING "STARTED" (t215): `kind` is the argv0 basename, deliberately NOT
    // resolved via the caller's PATH (the pane inherits the tmux server's environment).
    let kind = argv[0].rsplit('/').next().unwrap_or(&argv[0]).to_string();
    let child_pid = (hooks.verify)(name, pane_id.as_deref(), &kind)?;
    let session = agent
        .get("agent_session")
        .cloned()
        .unwrap_or_else(|| json!({}));
    let cwd_value = match cwd.filter(|c| !c.is_empty()) {
        Some(c) => json!(c),
        None => agent.get("cwd").cloned().unwrap_or(Value::Null),
    };
    if let Some(id) = pane_id.as_deref() {
        flock.record_launch(
            id,
            &[
                ("agent", json!(name)),
                ("argv", json!(argv)),
                ("command", json!(shlex_join(&argv))),
                ("cwd", cwd_value),
                (
                    "session",
                    session.get("value").cloned().unwrap_or(Value::Null),
                ),
                (
                    "session_kind",
                    session.get("kind").cloned().unwrap_or(Value::Null),
                ),
                ("name", json!(name)),
                ("source", json!("start_agent")),
            ],
        );
    }
    let mut registered = Value::Null;
    if let (Some(role), Some(id)) = (role.filter(|r| !r.is_empty()), pane_id.as_deref()) {
        // Bind the role to the pane's root process now, from the daemon's own launch record.
        registered = (hooks.lane_register)(id, role, provider, model, name)?;
    }
    Ok(json!({
        "agent": agent,
        "pane_id": pane_id,
        "name": name,
        "argv": result.get("argv").filter(|a| a.as_array().is_some_and(|a| !a.is_empty())).cloned().unwrap_or_else(|| json!(argv)),
        "child_pid": child_pid,
        "role": registered,
    }))
}

/// `rename`.
pub fn rename(
    backend: &mut dyn LayoutBackend,
    kind: &str,
    target: &str,
    label: &str,
    self_pane_id: Option<&str>,
) -> Result<Value, DunkError> {
    let label = label.trim();
    if label.is_empty() {
        return refused("label is required".into());
    }
    let (kind, target_id, _item) = resolve_kind(backend, kind, target, self_pane_id)?;
    let result = backend.rename(&kind, &target_id, label).map_err(|m| {
        DunkError::Refused(format!(
            "the terminal backend could not rename {kind} {target_id}: {m}"
        ))
    })?;
    let mut out = Map::new();
    out.insert("kind".into(), json!(kind));
    out.insert("id".into(), json!(target_id));
    out.insert("label".into(), json!(label));
    out.insert(
        kind.clone(),
        result.get(&kind).cloned().unwrap_or(Value::Null),
    );
    Ok(Value::Object(out))
}

/// `focus`.
pub fn focus(
    backend: &mut dyn LayoutBackend,
    kind: &str,
    target: &str,
    self_pane_id: Option<&str>,
) -> Result<Value, DunkError> {
    let (kind, target_id, _item) = resolve_kind(backend, kind, target, self_pane_id)?;
    backend.focus(&kind, &target_id).map_err(|m| {
        DunkError::Refused(format!(
            "the terminal backend could not focus {kind} {target_id}: {m}"
        ))
    })?;
    Ok(json!({"kind": kind, "id": target_id, "focused": true}))
}

#[cfg(test)]
#[path = "commands_layout_tests.rs"]
mod tests;
