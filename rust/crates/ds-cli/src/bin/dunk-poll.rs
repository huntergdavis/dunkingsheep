//! One read-only batched daemon request. Native replacement for tools/dunk_poll.py.
use clap::{Arg, ArgAction, Command};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::time::Duration;

fn option(name: &'static str) -> Arg {
    Arg::new(name).long(name).num_args(1)
}
fn flag(name: &'static str) -> Arg {
    Arg::new(name).long(name).action(ArgAction::SetTrue)
}
fn main() {
    std::process::exit(run());
}
fn run() -> i32 {
    let args = Command::new("dunk-poll")
        .about("One batched read from the Dunking Sheep daemon")
        .arg(option("socket"))
        .arg(option("self-pane"))
        .arg(option("mailbox"))
        .arg(
            option("limit")
                .default_value("5")
                .value_parser(clap::value_parser!(i64)),
        )
        .arg(flag("panes"))
        .arg(flag("tasks"))
        .arg(option("task-scope").value_parser(["global", "self"]))
        .arg(flag("include-done"))
        .arg(flag("check-herdr"))
        .arg(
            option("timeout")
                .default_value("10")
                .value_parser(clap::value_parser!(f64)),
        )
        .arg(flag("text"))
        .arg(flag("compact"))
        .get_matches();
    let socket = args
        .get_one::<String>("socket")
        .cloned()
        .or_else(|| {
            std::env::var("DUNKINGSHEEP_SOCKET")
                .ok()
                .filter(|v| !v.is_empty())
        })
        .unwrap_or_else(|| {
            let root = std::env::var_os("DUNKINGSHEEP_DIR")
                .filter(|v| !v.is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config/dunkingsheep"))
                })
                .unwrap_or_default();
            root.join("dunkingsheep.sock")
                .to_string_lossy()
                .into_owned()
        });
    let socket = if socket == "~" {
        std::env::var("HOME").unwrap_or(socket)
    } else if let Some(rest) = socket.strip_prefix("~/") {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(rest)
            .to_string_lossy()
            .into_owned()
    } else {
        socket
    };
    let pane = args
        .get_one::<String>("self-pane")
        .cloned()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("TMUX_PANE").ok().filter(|s| !s.is_empty()))
        .or_else(|| {
            std::env::var("HERDR_PANE_ID")
                .ok()
                .filter(|s| !s.is_empty())
        });
    let mut options = json!({"limit":args.get_one::<i64>("limit").copied().unwrap_or(5),
        "panes":args.get_flag("panes"), "tasks":args.get_flag("tasks"),
        "include_done":args.get_flag("include-done"), "check_herdr":args.get_flag("check-herdr")});
    for (arg, key) in [("mailbox", "mailbox"), ("task-scope", "task_scope")] {
        if let Some(value) = args.get_one::<String>(arg).filter(|s| !s.is_empty()) {
            options[key] = json!(value);
        }
    }
    let timeout = *args.get_one::<f64>("timeout").unwrap_or(&10.0);
    let timeout = match Duration::try_from_secs_f64(timeout) {
        Ok(value) if !value.is_zero() => value,
        _ => {
            eprintln!("dunk-poll: timeout must be positive and finite");
            return 1;
        }
    };
    let response = (|| -> Result<Value, String> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let socket_stream = runtime
            .block_on(async {
                tokio::time::timeout(timeout, tokio::net::UnixStream::connect(&socket)).await
            })
            .map_err(|_| "daemon socket connection timed out".to_string())?
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    format!("daemon socket not found at {socket}; is the daemon running?")
                } else {
                    e.to_string()
                }
            })?;
        let mut stream = socket_stream.into_std().map_err(|e| e.to_string())?;
        stream.set_nonblocking(false).map_err(|e| e.to_string())?;
        let timeout = Some(timeout);
        stream
            .set_read_timeout(timeout)
            .map_err(|e| e.to_string())?;
        stream
            .set_write_timeout(timeout)
            .map_err(|e| e.to_string())?;
        writeln!(
            stream,
            "{}",
            json!({"id":1,"cmd":"poll","args":options,"self_pane_id":pane})
        )
        .map_err(|e| e.to_string())?;
        let mut text = String::new();
        BufReader::new(stream)
            .read_line(&mut text)
            .map_err(|e| e.to_string())?;
        if text.trim().is_empty() {
            return Err("daemon closed the connection without a response".into());
        }
        let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        if value.get("ok").and_then(Value::as_bool) != Some(true) {
            return Err(value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("poll failed")
                .to_owned());
        }
        Ok(value
            .get("result")
            .filter(|r| !r.is_null())
            .cloned()
            .unwrap_or_else(|| json!({})))
    })();
    match response {
        Ok(mut value) => {
            value.sort_all_objects();
            if args.get_flag("text") {
                println!("{}", render_text(&value));
            } else if args.get_flag("compact") {
                println!("{}", serde_json::to_string(&value).unwrap());
            } else {
                println!("{}", serde_json::to_string_pretty(&value).unwrap());
            }
            0
        }
        Err(error) => {
            eprintln!("dunk-poll: {error}");
            if error.starts_with("daemon socket not found at ") {
                2
            } else {
                1
            }
        }
    }
}
fn show(value: Option<&Value>, default: &str) -> String {
    match value {
        None | Some(Value::Null) => default.into(),
        Some(Value::String(s)) => s.clone(),
        Some(v) => v.to_string(),
    }
}
fn render_text(result: &Value) -> String {
    let status = &result["status"];
    let count = &result["inbox_count"];
    let dunks = &result["dunks"];
    let mut lines = vec![format!(
        "self {}  v{}  dunks {}/{} running  inbox {} unread / {}",
        show(status.get("self_pane_id"), "?"),
        show(status.get("version"), "None"),
        show(status.get("running"), "0"),
        show(status.get("dunkers"), "0"),
        show(count.get("unread"), "0"),
        show(count.get("total"), "0")
    )];
    if let Some(error) = result.get("error").filter(|v| !v.is_null() && *v != "") {
        lines.push(format!("  note: {}", show(Some(error), "")));
    }
    if let Some(messages) = dunks.get("messages").and_then(Value::as_array) {
        for m in messages.iter().take(5) {
            lines.push(format!(
                "  #{} {}: {}",
                show(m.get("message_id"), "None"),
                show(m.get("sender"), "None"),
                show(
                    m.get("subject").filter(|v| !v.is_null() && *v != ""),
                    "(no subject)"
                )
            ));
        }
    }
    if let Some(rows) = dunks.get("dunks").and_then(Value::as_array) {
        for d in rows.iter().take(10) {
            let target = d
                .get("target_label")
                .filter(|v| !v.is_null() && *v != "")
                .or_else(|| d.get("target_pane_id"));
            lines.push(format!(
                "  {} {} every {}m -> {}",
                show(d.get("id"), "None"),
                if d.get("running").and_then(Value::as_bool).unwrap_or(false) {
                    "running"
                } else {
                    "stopped"
                },
                show(d.get("interval_minutes"), "None"),
                show(target, "None")
            ));
        }
    }
    lines.join("\n")
}
