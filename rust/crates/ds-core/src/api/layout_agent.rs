//! Start, rename and focus with Python conversion and mutation ordering.
use super::{python_bool_arg, python_scope_arg, Args, HandlerContext};
use crate::error::DunkError;
use crate::flock::{agent_verify, commands_layout::LayoutBackend, resolve};
use crate::terminal::raw::{equal, optional, string, truthy};
use serde_json::{json, Value};

fn arg<'a>(args: &'a Args, key: &str) -> &'a Value {
    args.get(key).unwrap_or(&Value::Null)
}
fn required<'a>(args: &'a Args, key: &str) -> Result<&'a Value, DunkError> {
    args.get(key)
        .ok_or_else(|| DunkError::Refused(format!("'{key}'")))
}
fn python_space(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{1c}'..='\u{1f}')
}
fn text(value: &Value) -> String {
    optional(value).unwrap_or_default()
}
fn parent(
    backend: &dyn LayoutBackend,
    kind: &str,
    target: &Value,
    own: Option<&str>,
) -> Result<Value, DunkError> {
    let spec = text(target).trim_matches(python_space).to_owned();
    let key = format!("{kind}_id");
    let spec = if matches!(spec.to_lowercase().as_str(), "self" | "me" | "here") {
        let pane = resolve::self_pane(own, |id| backend.get_pane(id))?;
        text(pane.get(&key).unwrap_or(&Value::Null))
    } else {
        spec
    };
    let rows = if kind == "tab" {
        backend.list_tabs()
    } else {
        backend.list_workspaces()
    };
    if rows.is_empty() {
        return Err(DunkError::Refused(backend.server_error_hint()));
    }
    Ok(resolve::pick(&rows, &spec, kind, &key, "label")?.clone())
}
fn target(
    backend: &dyn LayoutBackend,
    kind: &str,
    target: &Value,
    own: Option<&str>,
) -> Result<Value, DunkError> {
    match kind {
        "tab" | "workspace" => parent(backend, kind, target, own),
        "pane" => {
            if target.is_object() {
                let id = target.get("pane_id").unwrap_or(&Value::Null);
                if !truthy(id) {
                    return Err(DunkError::Refused("target dict has no pane_id".into()));
                }
                return Ok(backend
                    .list_panes()
                    .into_iter()
                    .find(|p| p.get("pane_id").is_some_and(|v| equal(v, id)))
                    .unwrap_or_else(|| target.clone()));
            }
            let spec = text(target).trim_matches(python_space).to_owned();
            let rows = if spec.trim_matches(python_space).is_empty()
                || (matches!(
                    spec.trim_matches(python_space).to_lowercase().as_str(),
                    "self" | "me" | "here"
                ) && own.is_none())
            {
                vec![]
            } else {
                backend.list_panes()
            };
            Ok(resolve::resolve_target(&rows, &spec, own, &backend.server_error_hint())?.clone())
        }
        _ => Err(DunkError::Refused(
            "kind must be 'workspace', 'tab' or 'pane'".into(),
        )),
    }
}
fn result(
    raw: crate::flock::commands_layout::RawCreateResult,
    prefix: &str,
) -> Result<Value, DunkError> {
    raw.map_err(|e| DunkError::Refused(e.to_string()))?
        .map_err(|e| DunkError::Refused(format!("{prefix}: {e}")))
}

pub fn h_start_agent(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let name = required(&args, "name")?;
    let command = required(&args, "command")?;
    let name = text(name).trim_matches(python_space).to_owned();
    if name.is_empty() {
        return Err(DunkError::Refused("agent name is required".into()));
    }
    let argv = split_command(&text(command)).map_err(DunkError::Refused)?;
    if argv.is_empty() {
        return Err(DunkError::Refused(
            "command is required (e.g. 'claude' or 'codex --model gpt-5')".into(),
        ));
    }
    context.env.with_layout_agent(&mut |backend,panes,verifier,env,verify_s| {
        let tab = arg(&args,"tab"); let workspace=arg(&args,"workspace");
        let tab_id = if truthy(tab) { Some(string(&parent(backend,"tab",tab,context.self_pane_id())?["tab_id"])) } else {None};
        let mut workspace_id = if truthy(workspace) { Some(string(&parent(backend,"workspace",workspace,context.self_pane_id())?["workspace_id"])) } else {None};
        if !truthy(tab) && !truthy(workspace) {
            let label = env.get("DUNKINGSHEEP_SANDBOX_WORKSPACE").unwrap_or("sandbox").trim_matches(python_space);
            let label = if label.is_empty() {"sandbox"} else {label};
            if let Some(row) = backend.list_workspaces().into_iter().find(|row| text(&row["label"]).trim_matches(python_space).to_lowercase()==label.to_lowercase()) {
                workspace_id=optional(&row["workspace_id"]);
            } else {
                let created=result(backend.create_workspace_raw(&json!(label),&Value::Null,false),&format!("could not create sandbox workspace {}",super::python_value_repr(&json!(label))))?;
                workspace_id=optional(&created["workspace"]["workspace_id"]);
                if workspace_id.is_none() { return Err(DunkError::Refused(format!("sandbox workspace {} was created without an id",super::python_value_repr(&json!(label))))); }
            }
        }
        let split=arg(&args,"split");
        if truthy(split) && split!= "right" && split!= "down" { return Err(DunkError::Refused("split must be 'right' or 'down'".into())); }
        let cwd=arg(&args,"cwd");
        let started=result(backend.start_agent_raw(&name,&argv,workspace_id.as_deref(),tab_id.as_deref(),optional(split).as_deref(),optional(cwd).as_deref(),python_bool_arg(&args,"focus",false)),&format!("the terminal backend could not start agent {}",super::python_value_repr(&json!(name))))?;
        let agent=started.get("agent").filter(|v|truthy(v)).cloned().unwrap_or_else(||json!({}));
        let pane_id=optional(&agent["pane_id"]);
        let kind=argv[0].rsplit('/').next().unwrap_or("");
        let child_pid=agent_verify::verify_agent_started(verifier,&name,pane_id.as_deref(),Some(kind),verify_s)?;
        if let Some(id)=pane_id.as_deref() {
            let session=agent.get("agent_session").filter(|v|truthy(v)).cloned().unwrap_or_else(||json!({}));
            context.flock.record_launch(id,&[
                ("agent",json!(name)),("argv",json!(argv)),("command",json!(crate::flock::commands_layout::shlex_join(&argv))),
                ("cwd",if truthy(cwd) {cwd.clone()} else {agent["cwd"].clone()}),
                ("session",session["value"].clone()),("session_kind",session["kind"].clone()),("name",json!(name)),("source",json!("start_agent")),
            ]);
        }
        let mut role=Value::Null;
        if truthy(arg(&args,"role")) && pane_id.is_some() {
            role=context.flock.lane_register(pane_id.as_deref(),Some(&string(arg(&args,"role"))),python_scope_arg(args.get("provider")).as_deref(),python_scope_arg(args.get("model")).as_deref(),Some(&name),context.self_pane_id(),panes)?;
        }
        context.flock.set_last_message(format!("Started agent {name}"));
        Ok(json!({"agent":agent,"pane_id":pane_id,"name":name,"argv":started.get("argv").filter(|v|truthy(v)).cloned().unwrap_or_else(||json!(argv)),"child_pid":child_pid,"role":role}))
    })
}
pub fn h_rename(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let kind = required(&args, "kind")?;
    let target_value = required(&args, "target")?;
    let label = required(&args, "label")?;
    let label = text(label).trim_matches(python_space).to_owned();
    if label.is_empty() {
        return Err(DunkError::Refused("label is required".into()));
    }
    let kind = text(kind).trim_matches(python_space).to_lowercase();
    context.env.with_layout_agent(&mut |backend, _, _, _, _| {
        let row = target(backend, &kind, target_value, context.self_pane_id())?;
        let id = string(&row[format!("{kind}_id")]);
        let renamed = result(
            backend.rename_raw(&kind, &id, &label),
            &format!("the terminal backend could not rename {kind} {id}"),
        )?;
        let mut out = json!({"kind":kind,"id":id,"label":label});
        out[&kind] = renamed[&kind].clone();
        Ok(out)
    })
}
pub fn h_focus(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let kind = required(&args, "kind")?;
    let target_value = required(&args, "target")?;
    let kind = text(kind).trim_matches(python_space).to_lowercase();
    context.env.with_layout_agent(&mut |backend, _, _, _, _| {
        let row = target(backend, &kind, target_value, context.self_pane_id())?;
        let id = string(&row[format!("{kind}_id")]);
        backend.focus_raw(&kind, &id).map_err(|e| {
            DunkError::Refused(format!(
                "the terminal backend could not focus {kind} {id}: {e}"
            ))
        })?;
        Ok(json!({"kind":kind,"id":id,"focused":true}))
    })
}

// Python shlex uses ASCII whitespace, unlike str.strip at the name boundary.
fn split_command(text: &str) -> Result<Vec<String>, String> {
    let mut words = Vec::new();
    let mut cur = String::new();
    let mut in_word = false;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            ' ' | '\t' | '\r' | '\n' => {
                if in_word {
                    words.push(std::mem::take(&mut cur));
                    in_word = false;
                }
            }
            '\'' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('\'') => break,
                        Some(x) => cur.push(x),
                        None => return Err("No closing quotation".into()),
                    }
                }
            }
            '"' => {
                in_word = true;
                loop {
                    match chars.next() {
                        Some('"') => break,
                        Some('\\') => match chars.next() {
                            // POSIX shlex unescapes ONLY `"` and `\` inside double quotes and keeps
                            // the backslash before anything else (`$`, a backtick): bash's rule is
                            // wider, and the difference changes what a pane would execute (t805
                            // review t1348).
                            Some(x @ ('"' | '\\')) => cur.push(x),
                            Some(x) => {
                                cur.push('\\');
                                cur.push(x);
                            }
                            None => return Err("No escaped character".into()),
                        },
                        Some(x) => cur.push(x),
                        None => return Err("No closing quotation".into()),
                    }
                }
            }
            '\\' => {
                in_word = true;
                match chars.next() {
                    Some(x) => cur.push(x),
                    None => return Err("No escaped character".into()),
                }
            }
            x => {
                in_word = true;
                cur.push(x);
            }
        }
    }
    if in_word {
        words.push(cur);
    }
    Ok(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn agent_command_shlex_preserves_python_whitespace_and_errors() {
        assert_eq!(
            split_command("codex\u{a0}--flag").unwrap(),
            vec!["codex\u{a0}--flag"]
        );
        assert_eq!(
            split_command("codex\t--flag").unwrap(),
            vec!["codex", "--flag"]
        );
        assert_eq!(
            split_command("codex \"x\\").unwrap_err(),
            "No escaped character"
        );
        assert_eq!(
            split_command("codex '").unwrap_err(),
            "No closing quotation"
        );
        assert_eq!(
            split_command("codex \"\\$x\"").unwrap(),
            vec!["codex", "\\$x"]
        );
    }
}
