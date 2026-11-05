//! Close and guarded rollback-backend passthrough, in Python evaluation order.
use super::{
    layout_agent::{python_space, split_command, target},
    Args, HandlerContext,
};
use crate::terminal::raw::{equal, optional, string, truthy};
use crate::{
    error::{refusal, DunkError},
    flock::commands_layout::LayoutBackend,
};
use serde_json::{json, Value};

pub type CloseOperation<'a> = dyn FnMut(&mut dyn LayoutBackend) -> Result<Value, DunkError> + 'a;
pub trait LayoutCloseEnv {
    fn with_layout(&self, operation: &mut CloseOperation<'_>) -> Result<Value, DunkError>;
    fn close(&self, kind: &str, id: &str) -> crate::terminal::RawCreateResult<()>;
    fn halt_aimed_at(&self, panes: &[String]) -> Result<Vec<String>, DunkError>;
    fn backend_name(&self) -> String;
    fn herdr_run(
        &self,
        argv: &[String],
        timeout_s: u64,
    ) -> Result<(i32, String, String), DunkError>;
}
fn required<'a>(args: &'a Args, key: &str) -> Result<&'a Value, DunkError> {
    args.get(key).ok_or_else(|| refusal(format!("'{key}'")))
}
fn text(value: &Value) -> String {
    optional(value).unwrap_or_default()
}

pub fn h_close(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let kind = required(&args, "kind")?;
    let spec = required(&args, "target")?;
    let force = super::python_bool_arg(&args, "force", false);
    let kind = text(kind).trim_matches(python_space).to_lowercase();
    let env = context.env.layout_close_env()?;
    env.with_layout(&mut |backend| {
        let item = target(backend, &kind, spec, context.self_pane_id())?;
        let key = format!("{kind}_id");
        let id = item.get(&key).ok_or_else(|| refusal(format!("'{key}'")))?;
        let own = context.self_pane_id().filter(|s| !s.is_empty()).and_then(|id| backend.get_pane(id));
        if !force && own.as_ref().is_some_and(|p| equal(p.get(&key).unwrap_or(&Value::Null), id)) {
            return Err(refusal(format!("refusing to close the {kind} this caller runs in ({}); pass --force only if you intend to kill your own pane/tab/workspace", string(id))));
        }
        let doomed: Vec<Value> = backend.list_panes().iter()
            .filter(|p| equal(p.get(&key).unwrap_or(&Value::Null), id))
            .map(|p| p.get("pane_id").cloned().ok_or_else(|| refusal("'pane_id'")))
            .collect::<Result<_,_>>()?;
        env.close(&kind, &string(id)).map_err(|e| refusal(e.to_string()))?
            .map_err(|e| refusal(format!("the terminal backend could not close {kind} {}: {}", string(id), e.message)))?;
        let stopped = env.halt_aimed_at(&doomed.iter().map(string).collect::<Vec<_>>())?;
        let label = item.get("label").filter(|v| truthy(v)).unwrap_or(id);
        context.flock.set_last_message(format!("Closed {kind} {}", string(label)));
        Ok(json!({"kind":kind,"id":id,"closed_panes":doomed,"stopped_dunks":stopped}))
    })
}

pub fn h_herdr(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let command = required(&args, "command")?;
    let env = context.env.layout_close_env()?;
    let name = env.backend_name();
    if name != "herdr" {
        return Err(refusal(format!("the herdr passthrough is not available with the {name} terminal backend; use the dedicated workspace/tab/pane commands instead")));
    }
    let mut argv = match command.as_array() {
        Some(values) => values.iter().map(string).collect(),
        None => split_command(&text(command)).map_err(refusal)?,
    };
    if argv.first().is_some_and(|s| s == "herdr") {
        argv.remove(0);
    }
    if argv.is_empty() {
        return Err(refusal(
            "command is required, e.g. 'workspace list' (see `herdr --help`)",
        ));
    }
    for blocked in [
        &["server", "stop"][..],
        &["update"],
        &["channel", "set"],
        &["session"],
        &["completion"],
        &["agent", "attach"],
        &["integration"],
    ] {
        if argv
            .iter()
            .map(String::as_str)
            .take(blocked.len())
            .eq(blocked.iter().copied())
        {
            return Err(refusal(format!("'herdr {}' is not allowed from here (it would stop, replace or attach herdr itself)", blocked.join(" "))));
        }
    }
    if argv.len() >= 2
        && matches!(
            (argv[0].as_str(), argv[1].as_str()),
            ("pane", "send-text" | "send-keys" | "run") | ("agent", "send")
        )
    {
        return Err(refusal(format!("'herdr {} {}' types into a pane, which would land on top of whatever a human is writing there. Use send_text(target=..., text=...) instead: it waits until the input box is clear, keeps ordering, and reports back. Pass hold_while_typing=false if you really must type over a draft.", argv[0], argv[1])));
    }
    if argv
        .iter()
        .any(|a| matches!(a.as_str(), "--remote" | "--session"))
    {
        return Err(refusal(
            "launching or attaching a herdr session is not allowed from here",
        ));
    }
    if let Some(own) = context.self_pane_id().filter(|s| !s.is_empty()) {
        if argv.iter().any(|a| a == "self") {
            let mut own_id = own.to_owned();
            if matches!(argv[0].as_str(), "tab" | "workspace") {
                let key = format!("{}_id", argv[0]);
                let row = context.env.get_pane(own);
                if let Some(value) = row.as_ref().and_then(|p| p.get(&key)).filter(|v| truthy(v)) {
                    own_id = string(value);
                }
            }
            for arg in &mut argv {
                if arg == "self" {
                    *arg = own_id.clone();
                }
            }
        }
    }
    let (code, out, err) = env.herdr_run(&argv, 30)?;
    let mut payload = Value::Null;
    for stream in [&out, &err] {
        if !stream.trim_matches(python_space).is_empty() {
            if let Ok(value) = crate::wire_json::from_str(stream) {
                payload = value;
                break;
            }
        }
    }
    let text = if out.trim_matches(python_space).is_empty() {
        err.as_str()
    } else {
        out.as_str()
    };
    let error = if let Some(object) = payload
        .as_object()
        .and_then(|p| p.get("error"))
        .and_then(Value::as_object)
    {
        object
            .get("message")
            .filter(|v| truthy(v))
            .or_else(|| object.get("code"))
            .cloned()
            .unwrap_or(Value::Null)
    } else if code != 0 && !looks_like_usage(text) {
        let message = if !err.trim_matches(python_space).is_empty() {
            err.trim_matches(python_space).to_owned()
        } else if !out.trim_matches(python_space).is_empty() {
            out.trim_matches(python_space).to_owned()
        } else {
            format!("herdr exited {code}")
        };
        json!(message)
    } else {
        Value::Null
    };
    let result = payload
        .as_object()
        .and_then(|p| p.get("result"))
        .cloned()
        .unwrap_or(payload);
    let ok = error.is_null();
    let mut response = serde_json::Map::new();
    response.insert("ok".into(), json!(ok));
    response.insert("argv".into(), json!(argv));
    response.insert("result".into(), if ok { result } else { Value::Null });
    response.insert("text".into(), json!(text));
    response.insert("error".into(), error);
    response.insert("exit_code".into(), json!(code));
    Ok(Value::Object(response))
}
fn looks_like_usage(text: &str) -> bool {
    let head: String = text
        .trim_start_matches(python_space)
        .chars()
        .take(200)
        .collect::<String>()
        .to_lowercase();
    head.starts_with("usage:") || head.starts_with("herdr") && head.contains("commands:")
}
pub fn h_herdr_help(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let env = context.env.layout_close_env()?;
    let name = env.backend_name();
    if name != "herdr" {
        return Err(refusal(format!(
            "herdr help is not available with the {name} terminal backend"
        )));
    }
    let topic = text(args.get("topic").unwrap_or(&Value::Null));
    let topic: Vec<_> = topic
        .split(python_space)
        .filter(|s| !s.is_empty())
        .collect();
    let argv = vec![topic.first().copied().unwrap_or("--help").to_owned()];
    let (code, out, err) = env.herdr_run(&argv, 10)?;
    let text = if out.trim_matches(python_space).is_empty() {
        err
    } else {
        out
    };
    if text.trim_matches(python_space).is_empty() {
        return Err(refusal(format!(
            "herdr printed no help for {} (exit {code})",
            super::python_value_repr(&json!(argv.join(" ")))
        )));
    }
    Ok(json!({"topic":if topic.is_empty(){None}else{Some(topic.join(" "))},"text":text}))
}
