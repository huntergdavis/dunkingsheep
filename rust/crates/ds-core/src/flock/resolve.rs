//! `flock/resolve.rs` (T30, slice 2): turning a spec into a pane, tab or workspace.
//! Ports `Flock._pick`, `_self_pane`, `resolve_workspace`, `resolve_tab`, `resolve_target`
//! and the module-level `_describe`.
//!
//! These are PURE over the backend's lists (as JSON objects, the shape `list_panes_grouped`
//! annotates with `tab_label`/`workspace_label`/`agent`/`terminal_id`), so the logic is
//! testable without a backend; the `Flock` methods that fetch the lists belong with the
//! backend wiring (slice 3). NOT here: `resolve_mailbox`, `_pane_info`, `resolve_lanes`.

use serde_json::Value;

use crate::error::DunkError;

/// The self-words every resolver accepts.
fn is_self_word(spec: &str) -> bool {
    matches!(spec.to_lowercase().as_str(), "self" | "me" | "here")
}

/// Python `str(item.get(key) or "")`.
fn text(item: &Value, key: &str) -> String {
    match item.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None | Some(Value::Bool(false)) => String::new(),
        Some(Value::Number(n)) if n.as_f64() == Some(0.0) => String::new(),
        Some(other) => other.to_string(),
    }
}

fn truthy(item: &Value, key: &str) -> bool {
    !text(item, key).is_empty()
}

/// `_describe`: `id (workspace/tab[ agent])`, comma separated.
pub fn describe(panes: &[&Value]) -> String {
    panes
        .iter()
        .map(|p| {
            let agent = if truthy(p, "agent") {
                format!(" {}", text(p, "agent"))
            } else {
                String::new()
            };
            format!(
                "{} ({}/{}{})",
                value_or_none(p, "pane_id"),
                value_or_none(p, "workspace_label"),
                value_or_none(p, "tab_label"),
                agent
            )
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Python formats a missing value as `None` inside an f-string.
fn value_or_none(item: &Value, key: &str) -> String {
    match item.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => "None".to_string(),
        Some(other) => other.to_string(),
    }
}

/// `_self_pane`: the caller's own pane, or the named refusals.
pub fn self_pane(
    self_pane_id: Option<&str>,
    get_pane: impl Fn(&str) -> Option<Value>,
) -> Result<Value, DunkError> {
    let id = self_pane_id.filter(|s| !s.is_empty()).ok_or_else(|| {
        DunkError::Refused(
            "'self' needs TMUX_PANE (or HERDR_PANE_ID on the herdr rollback; run from inside a pane)"
                .to_string(),
        )
    })?;
    get_pane(id).ok_or_else(|| {
        DunkError::Refused(format!("own pane {id} not found on the current backend"))
    })
}

/// `_pick`: exact id, exact label (case-insensitive), then a unique label substring.
pub fn pick<'a>(
    items: &'a [Value],
    spec: &str,
    kind: &str,
    id_key: &str,
    label_key: &str,
) -> Result<&'a Value, DunkError> {
    let needle = spec.trim();
    if needle.is_empty() {
        return Err(DunkError::Refused(format!("{kind} is required")));
    }
    if let Some(item) = items
        .iter()
        .find(|i| text(i, id_key) == needle && i.get(id_key).is_some())
    {
        return Ok(item);
    }
    let low = needle.to_lowercase();
    let exact: Vec<&Value> = items
        .iter()
        .filter(|i| text(i, label_key).to_lowercase() == low)
        .collect();
    match exact.len() {
        1 => return Ok(exact[0]),
        0 => {}
        _ => {
            let ids: Vec<String> = exact.iter().map(|i| text(i, id_key)).collect();
            return Err(DunkError::Refused(format!(
                "{kind} {} is ambiguous: {}",
                py_repr(spec),
                ids.join(", ")
            )));
        }
    }
    let partial: Vec<&Value> = items
        .iter()
        .filter(|i| text(i, label_key).to_lowercase().contains(&low))
        .collect();
    match partial.len() {
        1 => Ok(partial[0]),
        0 => Err(DunkError::Refused(format!(
            "no {kind} matches {} on the current backend",
            py_repr(spec)
        ))),
        _ => {
            let listed: Vec<String> = partial
                .iter()
                .map(|i| format!("{} ({})", text(i, id_key), value_or_none(i, label_key)))
                .collect();
            Err(DunkError::Refused(format!(
                "{kind} {} is ambiguous: {}",
                py_repr(spec),
                listed.join(", ")
            )))
        }
    }
}

/// Python `repr()` of a plain string: single quotes unless it holds one and no double quote.
fn py_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// `resolve_workspace`. `own` is `_self_pane`, called only for self/me/here.
pub fn resolve_workspace<'a>(
    workspaces: &'a [Value],
    spec: &str,
    own: impl FnOnce() -> Result<Value, DunkError>,
    server_error_hint: &str,
) -> Result<&'a Value, DunkError> {
    let mut spec = spec.trim().to_string();
    if is_self_word(&spec) {
        spec = text(&own()?, "workspace_id");
    }
    if workspaces.is_empty() {
        return Err(DunkError::Refused(server_error_hint.to_string()));
    }
    pick(workspaces, &spec, "workspace", "workspace_id", "label")
}

/// `resolve_tab`.
pub fn resolve_tab<'a>(
    tabs: &'a [Value],
    spec: &str,
    own: impl FnOnce() -> Result<Value, DunkError>,
    server_error_hint: &str,
) -> Result<&'a Value, DunkError> {
    let mut spec = spec.trim().to_string();
    if is_self_word(&spec) {
        spec = text(&own()?, "tab_id");
    }
    if tabs.is_empty() {
        return Err(DunkError::Refused(server_error_hint.to_string()));
    }
    pick(tabs, &spec, "tab", "tab_id", "label")
}

/// `resolve_target` for a pane DICT: the live (annotated) pane with that id, else the dict.
pub fn resolve_target_dict(panes: &[Value], spec: &Value) -> Result<Value, DunkError> {
    let id = text(spec, "pane_id");
    if id.is_empty() {
        return Err(DunkError::Refused("target dict has no pane_id".to_string()));
    }
    Ok(panes
        .iter()
        .find(|p| text(p, "pane_id") == id)
        .unwrap_or(spec)
        .clone())
}

/// `resolve_target` for a string spec. Exactly one pane must match.
pub fn resolve_target<'a>(
    panes: &'a [Value],
    spec: &str,
    self_pane_id: Option<&str>,
    server_error_hint: &str,
) -> Result<&'a Value, DunkError> {
    let mut spec = spec.trim().to_string();
    if spec.is_empty() {
        return Err(DunkError::Refused("target is required".to_string()));
    }
    if is_self_word(&spec) {
        match self_pane_id.filter(|s| !s.is_empty()) {
            Some(id) => spec = id.to_string(),
            None => {
                return Err(DunkError::Refused(
                    "target 'self' needs TMUX_PANE (or HERDR_PANE_ID on the herdr rollback; run from inside a pane)"
                        .to_string(),
                ))
            }
        }
    }
    if panes.is_empty() {
        return Err(DunkError::Refused(server_error_hint.to_string()));
    }
    if let Some(pane) = panes
        .iter()
        .find(|p| text(p, "pane_id") == spec || text(p, "terminal_id") == spec)
    {
        return Ok(pane);
    }
    let needle = spec.to_lowercase();
    // Exact label matches beat substring matches, so a tab named "Dunking Sheep" wins over a
    // pane whose title merely contains it.
    for key in ["tab_label", "label", "workspace_label", "agent"] {
        let exact: Vec<&Value> = panes
            .iter()
            .filter(|p| text(p, key).to_lowercase() == needle)
            .collect();
        match exact.len() {
            0 => {}
            1 => return Ok(exact[0]),
            _ => {
                let agents: Vec<&&Value> = exact.iter().filter(|p| truthy(p, "agent")).collect();
                if agents.len() == 1 {
                    return Ok(agents[0]);
                }
                return Err(DunkError::Ambiguous(format!(
                    "target {} is ambiguous: {}",
                    py_repr(&spec),
                    describe(&exact)
                )));
            }
        }
    }
    let matches: Vec<&Value> = panes
        .iter()
        .filter(|p| {
            [
                "tab_label",
                "workspace_label",
                "agent",
                "cwd",
                "terminal_title_stripped",
                "label",
            ]
            .iter()
            .map(|k| text(p, k))
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
            .contains(&needle)
        })
        .collect();
    match matches.len() {
        1 => return Ok(matches[0]),
        0 => {
            return Err(DunkError::Refused(format!(
                "no pane matches {} on the current backend",
                py_repr(&spec)
            )))
        }
        _ => {}
    }
    // Prefer a single agent pane among several matches.
    let agents: Vec<&&Value> = matches.iter().filter(|p| truthy(p, "agent")).collect();
    if agents.len() == 1 {
        return Ok(agents[0]);
    }
    Err(DunkError::Ambiguous(format!(
        "target {} is ambiguous: {}",
        py_repr(&spec),
        describe(&matches)
    )))
}
