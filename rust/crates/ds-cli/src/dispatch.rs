//! T44, slice 6: run a parsed command. `main.rs` used to stop at "parsed but not ported yet";
//! this runs the commands whose bodies exist in the CLI crate and says so honestly for the
//! rest. Wired today: `context` (the READ subcommands, `context_cmd`), `usage`
//! (`usage_cmd`), `install` and `secrets-check` (`secrets_cmd`), the daemon-backed `gh-token`,
//! `git-credential` and `doctor` (`daemon_cmd`), and `github wrappers` (`github_cmd`). Local
//! `context add`, `promote`, `supersede` and `retire` writes are also wired; remaining unsupported
//! commands keep the "parsed but not ported yet" answer (exit 1).
//!
//! `--json` for `context`: Python prints `json.dumps(result, indent=2)`; the ordered mode in
//! `usage_cmd` preserves daemon/local object insertion order with Python-compatible scalars.

use serde_json::{Map, Value};
use std::cell::RefCell;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::context_cmd::context_read;
use crate::usage_cmd::{ordered_json, sorted_json, usage_command, Outcome};
use ds_core::context::digest::{digest_both, select_stores, DEFAULT_CAP};
use ds_core::context::ledger::Ledger;
use ds_core::context::store::Store;

// The generated registry sorts its rows by name for reproducible generation. The Python CLI's
// `commands` and `mcp-map` printers use declaration order, so keep that presentation order here.
const PYTHON_COMMAND_ORDER: &[&str] = &[
    "owner_push",
    "owner_poll",
    "owner_take",
    "owner_stats",
    "owner_receipts",
    "state_capture",
    "state_latest",
    "roster",
    "message_state",
    "reply_stats",
    "project_summary",
    "lanes",
    "lane_set",
    "retire_pane",
    "dispatch",
    "clear_box",
    "delivery",
    "help",
    "status",
    "turn_start",
    "turn_end",
    "poll",
    "list_dunks",
    "get_dunk",
    "add_dunk",
    "update_dunk",
    "remove_dunk",
    "start_dunk",
    "stop_dunk",
    "toggle_dunk",
    "stop_all",
    "fire_dunk",
    "send_text",
    "send_keys",
    "flush_box",
    "list_messages",
    "get_message",
    "cancel_message",
    "report_run",
    "report_list",
    "owner_conversation",
    "owner_reply",
    "owner_ingest",
    "owner_outbox",
    "owner_delivered",
    "inbox_send",
    "inbox_count",
    "pm_digest",
    "pm_routing",
    "inbox_list",
    "inbox_read",
    "inbox_status",
    "inbox_ack",
    "inbox_recover",
    "inbox_archive",
    "inbox_cancel",
    "inbox_delete",
    "inbox_stats",
    "inbox_fetch",
    "allocation",
    "task_add",
    "task_list",
    "task_read",
    "task_update",
    "task_assign",
    "task_complete",
    "task_block",
    "task_unblock",
    "task_cancel",
    "task_delete",
    "task_note",
    "task_next",
    "task_heartbeat",
    "task_release",
    "task_follow",
    "task_unfollow",
    "task_followers",
    "task_stats",
    "task_metrics",
    "task_reap",
    "memory_recall",
    "memory_context",
    "memory_search",
    "memory_stats",
    "memory_status",
    "memory_backfill",
    "memory_ingest",
    "health",
    "doctor",
    "recover",
    "metrics",
    "list_panes",
    "read_pane",
    "delivery_capability",
    "list_workspaces",
    "create_workspace",
    "create_tab",
    "split_pane",
    "start_agent",
    "respawn_pane",
    "rename",
    "focus",
    "close",
    "herdr",
    "herdr_help",
    "shutdown",
    "gh_token",
    "review_sync",
    "review_list",
    "context_add",
    "context_promote",
    "context_supersede",
    "context_retire",
    "poolrad_sync",
    "poolrad_progress",
    "whoami",
    "lane_register",
    "lane_unregister",
    "token_issue",
    "token_revoke",
    "superuser_status",
    "superuser_add",
    "superuser_remove",
    "superuser_recover",
];

fn commands_in_python_order() -> Vec<&'static ds_core::registry::Command> {
    PYTHON_COMMAND_ORDER
        .iter()
        .map(|name| ds_core::registry::command(name).expect("Python CLI command is registered"))
        .collect()
}

fn command_description(command: &ds_core::registry::Command) -> &'static str {
    // The registry generator normalizes whitespace, while the Python CLI preserves it.
    if command.name == "respawn_pane" {
        "Replace a managed pane's process in place, preserving the pane id, task lease/ownership, mailbox and launch history.  Superuser/operator only.  Requires the exact live pane pid/start, --confirm-idle evidence, an argv command (no shell interpolation) and a rollback argv.  Refuses active turns, unread mail, queued outbox, live task leases and any non-ready delivery state; verifies the replacement and rolls back on failure."
    } else {
        command.description
    }
}

fn cli_param_required(command: &ds_core::registry::Command, name: &str) -> bool {
    // The generated `required` field is MCP-required. argparse leaves these two
    // CLI parameters optional, even though MCP requires them in its schema.
    !matches!(command.name, "add_dunk" | "create_tab") && command.required.contains(&name)
}

fn mcp_group_action(command_name: &str) -> Option<(&'static str, &'static str)> {
    let mut mapping = None;
    for group in ds_core::registry::GROUPS {
        for (action, name) in group.actions {
            if *name == command_name {
                mapping = Some((group.name, *action));
            }
        }
    }
    mapping
}

fn text<'a>(ns: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    ns.get(key).and_then(Value::as_str)
}

fn task_stamp(at: f64) -> String {
    use chrono::TimeZone;
    chrono::Local
        .timestamp_opt(at.floor() as i64, 0)
        .single()
        .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

fn joined_words(ns: &Map<String, Value>, key: &str) -> String {
    ns.get(key)
        .and_then(Value::as_array)
        .map(|words| {
            words
                .iter()
                .map(crate::render_fleet::py_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default()
}

fn watchdog_deadman(ns: &Map<String, Value>) -> Outcome {
    use ds_core::client::{default_timeout, service_token, FlockClient, UnixTransport};
    use ds_core::config::{config_dir, socket_path, Env};
    use std::path::PathBuf;

    let env = Env::from_process();
    let config = match config_dir(&env) {
        Ok(path) => path,
        Err(error) => {
            return Outcome {
                stdout: String::new(),
                stderr: format!("error: {error}\n"),
                code: error.exit_code().max(2),
            }
        }
    };
    let route_socket = text(ns, "daemon_path");
    let default_socket = std::env::var("DUNKINGSHEEP_SOCKET")
        .ok()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| socket_path(&env).ok());
    let socket = route_socket
        .map(PathBuf::from)
        .or(default_socket)
        .unwrap_or_else(|| config.join("dunkingsheep.sock"));
    let socket_text =
        match ds_core::terminal::tmux_registry::expand_home(&socket.to_string_lossy(), &env) {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(error) => {
                return Outcome {
                    stdout: String::new(),
                    stderr: format!("error: {error}\n"),
                    code: 2,
                }
            }
        };
    let process_env = |name: &str| std::env::var(name).ok();
    let client = FlockClient::new_with_lazy_token(
        UnixTransport {
            path: PathBuf::from(socket_text.clone()),
        },
        // This is always a pane-less system sender. Resolving inherited shell/tmux identity
        // before the watchdog checks for dry-run/fresh/cooldown can fail a no-send invocation.
        None,
        "watchdog system sender".to_string(),
        default_timeout(&process_env),
        || {
            let process_env = |name: &str| std::env::var(name).ok();
            service_token(&process_env, &|path| std::fs::read_to_string(path))
        },
    );

    let expand = |path: &str| ds_core::terminal::tmux_registry::expand_home(path, &env);
    let expected_dir = match text(ns, "expected_dir").filter(|path| !path.is_empty()) {
        Some(path) => match expand(path) {
            Ok(path) => path,
            Err(error) => {
                return Outcome {
                    stdout: String::new(),
                    stderr: format!("error: {error}\n"),
                    code: 2,
                }
            }
        },
        None => config,
    };
    let heartbeat = text(ns, "heartbeat").unwrap_or("");
    let to = text(ns, "to").unwrap_or("");
    let state = text(ns, "state").unwrap_or("");
    let error_file = text(ns, "error_file").unwrap_or("");
    let status = text(ns, "status").unwrap_or("");
    let inbox = text(ns, "inbox").unwrap_or("");
    let args = ds_core::watchdog_deadman::MainArgs {
        heartbeat,
        cadence_minutes: ns.get("cadence_minutes").cloned().unwrap_or(Value::Null),
        multiplier: ns.get("multiplier").cloned().unwrap_or(Value::from(3)),
        to,
        state,
        cooldown_minutes: ns
            .get("cooldown_minutes")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        now: ns.get("now").and_then(Value::as_f64),
        dry_run: matches!(ns.get("dry_run"), Some(Value::Bool(true))),
        error_file,
        status,
        inbox,
        daemon_path: Some(&socket_text),
        expected_dir: Some(&expected_dir),
        json: matches!(ns.get("json"), Some(Value::Bool(true))),
    };
    let mut stdout = Vec::new();
    match ds_core::watchdog_deadman::main(&args, client, &mut stdout) {
        Ok(code) => Outcome {
            stdout: String::from_utf8_lossy(&stdout).into_owned(),
            stderr: String::new(),
            code,
        },
        Err(error) => Outcome {
            stdout: String::new(),
            stderr: format!("error: {error}\n"),
            code: error.exit_code().max(2),
        },
    }
}

fn string_values(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|values| values.iter().map(crate::render_fleet::py_str).collect())
        .unwrap_or_default()
}

fn run_command_with_stderr(
    ns: &Map<String, Value>,
    command: &str,
    args: Map<String, Value>,
    render: impl Fn(&Value) -> (String, String),
) -> Outcome {
    let stderr = RefCell::new(None);
    let mut outcome = crate::daemon_cmd::run_command(ns, command, args, |result| {
        let (stdout, message) = render(result);
        *stderr.borrow_mut() = Some(message);
        stdout
    });
    if !matches!(ns.get("json"), Some(Value::Bool(true))) {
        outcome.stderr = stderr.into_inner().unwrap_or_default();
    }
    outcome
}

fn respawn_argv(ns: &Map<String, Value>) -> Result<(Value, Value), Outcome> {
    let invalid_json = |error: serde_json::Error| Outcome {
        stdout: String::new(),
        stderr: format!("--command-json/--rollback-json must be JSON arrays: {error}\n"),
        code: 2,
    };
    let command_json = text(ns, "command_json").unwrap_or_default();
    let argv: Value = serde_json::from_str(command_json).map_err(invalid_json)?;
    if !argv
        .as_array()
        .is_some_and(|values| !values.is_empty() && values.iter().all(Value::is_string))
    {
        return Err(Outcome {
            stdout: String::new(),
            stderr: "--command-json must be a non-empty JSON array of strings\n".into(),
            code: 2,
        });
    }

    let rollback_json = text(ns, "rollback_json").filter(|value| !value.is_empty());
    let rollback = match rollback_json {
        Some(value) => {
            let argv: Value = serde_json::from_str(value).map_err(invalid_json)?;
            if !argv
                .as_array()
                .is_some_and(|values| !values.is_empty() && values.iter().all(Value::is_string))
            {
                return Err(Outcome {
                    stdout: String::new(),
                    stderr: "--rollback-json must be a non-empty JSON array of strings\n".into(),
                    code: 2,
                });
            }
            argv
        }
        None => Value::Null,
    };
    Ok((argv, rollback))
}

fn not_ported(command: &str) -> Outcome {
    Outcome {
        stdout: String::new(),
        stderr: format!("dunkingsheep (rust port): `{command}` is parsed but not ported yet\n"),
        code: 1,
    }
}

fn serve(ns: &Map<String, Value>) -> Outcome {
    let options = ds_core::server_runtime::DaemonOptions {
        socket_path: text(ns, "socket")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        state_file: text(ns, "state_file")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from),
        no_log_file: matches!(ns.get("no_log_file"), Some(Value::Bool(true))),
    };
    match ds_core::server_runtime::run_daemon(options) {
        Ok(code) => Outcome {
            stdout: String::new(),
            stderr: String::new(),
            code,
        },
        Err(error) => Outcome {
            stdout: String::new(),
            stderr: format!("error: {error}\n"),
            code: error.exit_code(),
        },
    }
}

fn guide(ns: &Map<String, Value>) -> Outcome {
    let markdown = matches!(ns.get("markdown"), Some(Value::Bool(true)));
    let version = crate::cli_table::ROOT
        .args
        .iter()
        .find(|arg| arg.dest == "version")
        .and_then(|arg| arg.version)
        .and_then(|version| version.strip_prefix("dunkingsheep "))
        .and_then(|version| version.split_whitespace().next())
        .expect("generated CLI table has an application version");
    Outcome {
        stdout: ds_core::help::guide(markdown, version),
        stderr: String::new(),
        code: 0,
    }
}

fn commands(json: bool) -> Outcome {
    let commands = commands_in_python_order();

    if json {
        let rows = commands
            .iter()
            .map(|command| {
                serde_json::json!({
                    "name": command.name,
                    "description": command_description(command),
                    "input_schema": ds_core::registry::input_schema(command.name)
                        .expect("registered command has an input schema"),
                    "mcp_tool": command.mcp,
                })
            })
            .collect::<Vec<_>>();
        return Outcome {
            stdout: format!("{}\n", ordered_json(&Value::Array(rows))),
            stderr: String::new(),
            code: 0,
        };
    }

    let mut output = String::new();
    for command in commands {
        let params = command
            .params
            .iter()
            .map(|param| {
                format!(
                    "{}{}",
                    param.name,
                    if cli_param_required(command, param.name) {
                        "*"
                    } else {
                        ""
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(", ");
        let marker = if command.mcp {
            ""
        } else {
            "  [socket/cli only]"
        };
        use std::fmt::Write as _;
        let _ = writeln!(
            output,
            "{}({params}){marker}\n    {}",
            command.name,
            command_description(command)
        );
    }
    Outcome {
        stdout: output,
        stderr: String::new(),
        code: 0,
    }
}

fn python_json_string_len(value: &str) -> usize {
    let mut len = 2;
    for ch in value.chars() {
        len += match ch {
            '"' | '\\' | '\u{0008}' | '\t' | '\n' | '\u{000c}' | '\r' => 2,
            ch if ch <= '\u{001f}' => 6,
            ch if ch.is_ascii() => 1,
            ch if ch <= '\u{ffff}' => 6,
            _ => 12,
        };
    }
    len
}

fn python_json_len(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(true) => 4,
        Value::Bool(false) => 5,
        Value::Number(number) => number.to_string().len(),
        Value::String(value) => python_json_string_len(value),
        Value::Array(values) => {
            2 + values.iter().map(python_json_len).sum::<usize>()
                + values.len().saturating_sub(1) * 2
        }
        Value::Object(values) => {
            2 + values
                .iter()
                .map(|(key, value)| python_json_string_len(key) + 2 + python_json_len(value))
                .sum::<usize>()
                + values.len().saturating_sub(1) * 2
        }
    }
}

fn mcp_map(ns: &Map<String, Value>) -> Outcome {
    use ds_core::registry::{self, GROUPS};

    let markdown = matches!(ns.get("markdown"), Some(Value::Bool(true)));
    let commands = commands_in_python_order();
    let flat = commands
        .iter()
        .copied()
        .filter(|command| command.mcp)
        .collect::<Vec<_>>();
    let schema_rows = |commands: &[&ds_core::registry::Command], include_description: bool| {
        commands
            .iter()
            .map(|command| {
                let mut row = Map::new();
                row.insert("name".into(), Value::String(command.name.to_string()));
                if include_description {
                    row.insert(
                        "description".into(),
                        Value::String(command_description(command).to_string()),
                    );
                }
                row.insert(
                    "inputSchema".into(),
                    registry::input_schema(command.name).expect("registered schema"),
                );
                Value::Object(row)
            })
            .collect::<Vec<_>>()
    };
    let grouped_rows = GROUPS
        .iter()
        .map(|group| {
            let mut row = Map::new();
            row.insert("name".into(), Value::String(group.name.to_string()));
            row.insert(
                "description".into(),
                Value::String(group.description.to_string()),
            );
            row.insert(
                "inputSchema".into(),
                registry::grouped_input_schema(group.name).expect("registered group schema"),
            );
            Value::Object(row)
        })
        .collect::<Vec<_>>();
    let flat_bytes = python_json_len(&Value::Array(schema_rows(&flat, true)));
    let grouped_bytes = python_json_len(&Value::Array(grouped_rows));
    let hot = GROUPS
        .iter()
        .filter(|group| matches!(group.name, "inbox" | "task" | "memory"))
        .map(|group| {
            let mut row = Map::new();
            row.insert("name".into(), Value::String(group.name.to_string()));
            row.insert(
                "inputSchema".into(),
                registry::grouped_input_schema(group.name).expect("registered group schema"),
            );
            Value::Object(row)
        })
        .collect::<Vec<_>>();
    let hot_bytes = python_json_len(&Value::Array(hot));
    let mut output = Vec::new();

    if markdown {
        output.extend([
            "# MCP tool map (t8 grouping audit)".to_string(),
            String::new(),
            "The declared MCP surface is one namespaced tool per domain, each taking an `action`. Nothing was removed: every flat name still resolves, and every command stays on the CLI/socket API.".to_string(),
            String::new(),
            format!("- declared tools: **{}** grouped (was **{}** flat)", GROUPS.len(), flat.len()),
            format!("- grouped schema bytes: **{grouped_bytes}** (was **{flat_bytes}**, -{:.1}%)", 100.0 * (1.0 - grouped_bytes as f64 / flat_bytes as f64)),
            format!("- hot set (`inbox`, `task`, `memory`): **{hot_bytes}** bytes (target <10 KB)"),
            String::new(), "## Grouped tools".to_string(), String::new(),
            "| Tool | Actions |".to_string(), "| --- | --- |".to_string(),
        ]);
        for group in GROUPS {
            output.push(format!(
                "| `{}` | {} |",
                group.name,
                group
                    .actions
                    .iter()
                    .map(|(action, _)| format!("`{action}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        output.extend([
            String::new(),
            "## OLD flat tool -> NEW grouped tool".to_string(),
            String::new(),
            "| Old tool | New tool | action |".to_string(),
            "| --- | --- | --- |".to_string(),
        ]);
        for command in &flat {
            let (group, action) = mcp_group_action(command.name).unwrap_or(("None", "None"));
            output.push(format!("| `{}` | `{group}` | `{action}` |", command.name));
        }
        output.extend([
            String::new(),
            "## CLI/socket-only commands (not MCP tools)".to_string(),
            String::new(),
            "| Command | Why |".to_string(),
            "| --- | --- |".to_string(),
            "| `shutdown` | daemon stop; CLI/socket only |".to_string(),
        ]);
    } else {
        output.push(format!("declared {} grouped tools (was {} flat); schema {grouped_bytes} bytes (was {flat_bytes}); hot set (inbox, task, memory) {hot_bytes} bytes", GROUPS.len(), flat.len()));
        output.push(String::new());
        for group in GROUPS {
            output.push(format!(
                "{}: {}",
                group.name,
                group
                    .actions
                    .iter()
                    .map(|(action, _)| *action)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        output.push(String::new());
        for command in &flat {
            let (group, action) = mcp_group_action(command.name).unwrap_or(("None", "None"));
            output.push(format!("{} -> {group}(action={action})", command.name));
        }
        for command in commands.iter().filter(|command| !command.mcp) {
            output.push(format!("{} -> (CLI/socket only)", command.name));
        }
    }
    Outcome {
        stdout: format!("{}\n", output.join("\n")),
        stderr: String::new(),
        code: 0,
    }
}

fn tui(ns: &Map<String, Value>) -> Outcome {
    let client = match crate::daemon_cmd::connect(ns) {
        Ok(client) => client,
        Err(outcome) => return outcome,
    };
    match ds_tui::runtime::run_terminal(client) {
        Ok(()) => Outcome {
            stdout: String::new(),
            stderr: String::new(),
            code: 0,
        },
        Err(error) => Outcome {
            stdout: String::new(),
            stderr: format!("error: {error}\n"),
            code: 3,
        },
    }
}

fn memory_aux(ns: &Map<String, Value>) -> Outcome {
    let sub = text(ns, "memory_command").unwrap_or("");
    match sub {
        "hook-context" => {
            let query = text(ns, "query")
                .filter(|query| !query.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .and_then(|cwd| cwd.file_name().map(|name| name.to_string_lossy().into()))
                        .unwrap_or_default()
                });
            let mut args = Map::new();
            args.insert("query".into(), Value::String(query));
            args.insert(
                "sessions".into(),
                ns.get("sessions").cloned().unwrap_or(Value::Null),
            );
            crate::daemon_cmd::run_command(ns, "memory_context", args, |result| {
                crate::render_print::render_memory_hook_context(result)
            })
        }
        "stats" => crate::daemon_cmd::run_command(
            ns,
            "memory_stats",
            Map::new(),
            crate::render_print::render_memory_stats,
        ),
        "status" => crate::daemon_cmd::run_command(
            ns,
            "memory_status",
            Map::new(),
            crate::render_print::render_memory_stats,
        ),
        _ => not_ported(&format!("memory {sub}")),
    }
}

fn memory(ns: &Map<String, Value>) -> Outcome {
    let mut args = Map::new();
    match text(ns, "memory_command") {
        Some("recall" | "search") => {
            for key in ["query", "limit", "harness", "project", "since"] {
                args.insert(key.into(), ns.get(key).cloned().unwrap_or(Value::Null));
            }
            let rpc = if text(ns, "memory_command") == Some("search") {
                "memory_search"
            } else {
                "memory_recall"
            };
            crate::daemon_cmd::run_command(ns, rpc, args, |result| {
                crate::render_print::render_memory(
                    result.as_array().map(Vec::as_slice).unwrap_or_default(),
                )
            })
        }
        Some("context") => {
            for key in ["query", "sessions"] {
                args.insert(key.into(), ns.get(key).cloned().unwrap_or(Value::Null));
            }
            crate::daemon_cmd::run_command(ns, "memory_context", args, |result| {
                crate::render_print::render_memory_context(result)
            })
        }
        Some("ingest") => {
            for key in ["path", "harness", "project"] {
                args.insert(key.into(), ns.get(key).cloned().unwrap_or(Value::Null));
            }
            crate::daemon_cmd::run_command(ns, "memory_ingest", args, |result| {
                format!(
                    "queued {} ({})\n",
                    crate::render_fleet::py_str(&result["path"]),
                    crate::render_fleet::py_str(&result["harness"])
                )
            })
        }
        Some("backfill") => {
            let action = if ns.get("pause") == Some(&Value::Bool(true)) {
                "pause"
            } else if ns.get("resume") == Some(&Value::Bool(true)) {
                "resume"
            } else if ns.get("status") == Some(&Value::Bool(true)) {
                "status"
            } else {
                "start"
            };
            args.insert("action".into(), Value::String(action.to_string()));
            args.insert(
                "limit".into(),
                ns.get("limit").cloned().unwrap_or(Value::Null),
            );
            crate::daemon_cmd::run_command(ns, "memory_backfill", args, |result| {
                crate::render_print::render_memory_backfill(result)
            })
        }
        Some(other) => not_ported(&format!("memory {other}")),
        None => not_ported("memory"),
    }
}

fn superuser(ns: &Map<String, Value>) -> Outcome {
    let (rpc, args) = match text(ns, "superuser_command") {
        Some("status") => ("superuser_status", Map::new()),
        Some("add") => {
            let mut args = Map::new();
            args.insert(
                "entry".into(),
                ns.get("entry").cloned().unwrap_or(Value::Null),
            );
            ("superuser_add", args)
        }
        Some("remove") => {
            let mut args = Map::new();
            args.insert(
                "entry".into(),
                ns.get("entry").cloned().unwrap_or(Value::Null),
            );
            ("superuser_remove", args)
        }
        Some("recover") => {
            let mut args = Map::new();
            args.insert(
                "name".into(),
                ns.get("name").cloned().unwrap_or(Value::Null),
            );
            ("superuser_recover", args)
        }
        Some(other) => return not_ported(&format!("superuser {other}")),
        None => return not_ported("superuser"),
    };
    crate::daemon_cmd::run_command(ns, rpc, args, crate::render_fleet::render_superuser)
}

fn health(ns: &Map<String, Value>) -> Outcome {
    let mut client = match crate::daemon_cmd::connect(ns) {
        Ok(client) => client,
        Err(outcome) => return outcome,
    };
    if matches!(ns.get("deep"), Some(Value::Bool(true))) {
        client.timeout = client.timeout.max(std::time::Duration::from_secs(180));
    }
    let mut args = Map::new();
    args.insert(
        "repair".into(),
        ns.get("repair").cloned().unwrap_or(Value::Bool(false)),
    );
    args.insert(
        "deep".into(),
        ns.get("deep").cloned().unwrap_or(Value::Bool(false)),
    );
    let result = match crate::daemon_cmd::call(&mut client, "health", args) {
        Ok(result) => result,
        Err(outcome) => return outcome,
    };

    let json = matches!(ns.get("json"), Some(Value::Bool(true)));
    let mut stdout = if json {
        format!("{}\n", ordered_json(&result))
    } else {
        crate::render_print::render_health(&result)
    };
    if !json {
        if let Some(requested) = text(ns, "write") {
            let requested = if requested.is_empty() {
                None
            } else {
                Some(requested)
            };
            match write_health_report(&result, requested) {
                Ok(path) => stdout.push_str(&format!("  report written to {path}\n")),
                Err(error) => {
                    return Outcome {
                        stdout,
                        stderr: format!("{error}\n"),
                        code: 3,
                    }
                }
            }
        }
    }
    Outcome {
        stdout,
        stderr: String::new(),
        code: if result.get("ok") == Some(&Value::Bool(false)) {
            2
        } else {
            0
        },
    }
}

fn write_health_report(result: &Value, requested: Option<&str>) -> Result<String, String> {
    let path = requested.map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".config/dunkingsheep/health.json")
    });
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create {}: {error}", parent.display()))?;
    }
    let body = sorted_json(result);
    std::fs::write(&path, body)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(requested
        .unwrap_or_else(|| path.to_str().unwrap_or_default())
        .to_string())
}

fn recover(ns: &Map<String, Value>) -> Outcome {
    let mut args = Map::new();
    args.insert(
        "apply".into(),
        ns.get("apply").cloned().unwrap_or(Value::Bool(false)),
    );
    args.insert(
        "record".into(),
        ns.get("record").cloned().unwrap_or(Value::Bool(false)),
    );
    crate::daemon_cmd::run_command(ns, "recover", args, crate::render_print::render_recover)
}

fn capability(ns: &Map<String, Value>) -> Outcome {
    let mut args = Map::new();
    args.insert(
        "target".into(),
        ns.get("target").cloned().unwrap_or(Value::Null),
    );
    crate::daemon_cmd::run_command(
        ns,
        "delivery_capability",
        args,
        crate::render_fleet::render_capability,
    )
}

fn lane(ns: &Map<String, Value>) -> Outcome {
    type LanePrinter = Box<dyn Fn(&Value) -> String>;
    let action = text(ns, "lane_command").unwrap_or("list");
    let (rpc, args, printer): (&str, Map<String, Value>, LanePrinter) = match action {
        "register" => {
            let mut args = Map::new();
            for key in ["pane", "role", "provider", "model", "label"] {
                args.insert(key.into(), ns.get(key).cloned().unwrap_or(Value::Null));
            }
            (
                "lane_register",
                args,
                Box::new(|result| {
                    format!(
                        "registered {} as {} (pid {})\n",
                        crate::render_fleet::py_str(&result["pane_id"]),
                        crate::render_fleet::py_str(&result["role"]),
                        result
                            .get("pane_pid")
                            .filter(|value| !value.is_null())
                            .map(crate::render_fleet::py_str)
                            .unwrap_or_else(|| "None".into())
                    )
                }),
            )
        }
        "unregister" => {
            let mut args = Map::new();
            args.insert(
                "pane".into(),
                ns.get("pane").cloned().unwrap_or(Value::Null),
            );
            (
                "lane_unregister",
                args,
                Box::new(|result| {
                    format!(
                        "unregistered {}: {}\n",
                        crate::render_fleet::py_str(&result["pane_id"]),
                        crate::render_fleet::py_str(&Value::Bool(crate::render_print::truthy(
                            &result["removed"]
                        )))
                    )
                }),
            )
        }
        "set" => {
            let mut args = Map::new();
            args.insert(
                "pane".into(),
                ns.get("pane").cloned().unwrap_or(Value::Null),
            );
            args.insert(
                "mailbox".into(),
                ns.get("mailbox").cloned().unwrap_or(Value::Null),
            );
            ("lane_set", args, Box::new(crate::render_fleet::render_lane))
        }
        _ => (
            "lanes",
            Map::new(),
            Box::new(crate::render_fleet::render_lanes),
        ),
    };
    crate::daemon_cmd::run_command(ns, rpc, args, printer)
}

fn retired(ns: &Map<String, Value>) -> Outcome {
    let mut args = Map::new();
    args.insert(
        "action".into(),
        ns.get("retire_command").cloned().unwrap_or(Value::Null),
    );
    args.insert(
        "pane".into(),
        ns.get("pane").cloned().unwrap_or(Value::Null),
    );
    args.insert(
        "reason".into(),
        ns.get("reason").cloned().unwrap_or(Value::Null),
    );
    let stamp = |at: f64| {
        use chrono::TimeZone;
        chrono::Local
            .timestamp_opt(at.floor() as i64, 0)
            .single()
            .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_default()
    };
    crate::daemon_cmd::run_command(ns, "retire_pane", args, |result| {
        crate::render_print::render_retired(result, &stamp)
    })
}

fn context_local(ns: &Map<String, Value>, json: bool) -> Option<Outcome> {
    let sub = text(ns, "context_command").unwrap_or("");
    let path = text(ns, "path")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from);
    let global = matches!(ns.get("global_store"), Some(Value::Bool(true)));
    let now = chrono::Local::now().naive_local();

    let outcome = |value: Value, text: String| Outcome {
        stdout: if json {
            format!("{}\n", ordered_json(&value))
        } else {
            text
        },
        stderr: String::new(),
        code: 0,
    };
    let failed = |message: String| Outcome {
        stdout: String::new(),
        stderr: format!("{message}\n"),
        code: 1,
    };

    match sub {
        "init" => {
            let store = Store::new(path.as_deref(), global);
            match store.ensure() {
                Ok(root) => {
                    let root = root.display().to_string();
                    Some(outcome(
                        serde_json::json!({"root": root}),
                        format!("store at {root}\n"),
                    ))
                }
                Err(error) => Some(failed(error.0)),
            }
        }
        "snapshot" => {
            let as_of = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let selected = if global {
                Some("global")
            } else if matches!(ns.get("project"), Some(Value::Bool(true))) {
                Some("project")
            } else {
                None
            };
            let mut ledger = Ledger::new(None);
            let digest = digest_both(
                path.as_deref(),
                selected,
                DEFAULT_CAP,
                None,
                false,
                &mut ledger,
                now,
            );
            let stores = select_stores(path.as_deref(), selected);
            let mut snapshots = Map::new();
            for (name, store) in stores {
                let stats = match store.stats() {
                    Ok(stats) => stats,
                    Err(error) => return Some(failed(error.0)),
                };
                let entries = match store.entry_stats(&mut ledger) {
                    Ok(entries) => entries,
                    Err(error) => return Some(failed(error.0)),
                };
                let mut snapshot = Map::new();
                snapshot.insert(
                    "status".into(),
                    Value::String(if store.exists() { "ok" } else { "absent" }.into()),
                );
                snapshot.insert(
                    "path".into(),
                    Value::String(store.root.display().to_string()),
                );
                snapshot.insert(
                    "counts".into(),
                    stats.get("counts").cloned().unwrap_or(Value::Null),
                );
                snapshot.insert(
                    "reads_7d".into(),
                    stats.get("reads_7d").cloned().unwrap_or(Value::Null),
                );
                snapshot.insert(
                    "injected_7d".into(),
                    stats.get("injected_7d").cloned().unwrap_or(Value::Null),
                );
                snapshot.insert(
                    "never_read_count".into(),
                    stats
                        .get("never_read_count")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                snapshot.insert(
                    "never_delivered_count".into(),
                    stats
                        .get("never_delivered_count")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
                snapshot.insert("entries".into(), Value::Array(entries));
                snapshots.insert(name, Value::Object(snapshot));
            }
            let mut value = Map::new();
            value.insert("schema".into(), Value::String("context-snapshot/1".into()));
            value.insert("as_of".into(), Value::from(as_of));
            value.insert("digest".into(), Value::String(digest));
            value.insert("stores".into(), Value::Object(snapshots));
            let value = Value::Object(value);
            Some(outcome(value.clone(), ordered_json(&value) + "\n"))
        }
        "ledger" if text(ns, "action") == Some("adopt") => {
            if std::env::var("DUNKINGSHEEP_SUPERUSER").ok().as_deref() != Some("1") {
                return Some(failed(
                    "refused: `context ledger adopt` is the owner's bootstrap; run it with DUNKINGSHEEP_SUPERUSER=1 from the owner's terminal".into(),
                ));
            }
            let store = Store::new(path.as_deref(), global);
            let mut ledger = Ledger::new(None);
            let adopted = match store.adopt(&mut ledger, "adopt") {
                Ok(adopted) => adopted,
                Err(error) => return Some(failed(error.to_string())),
            };
            let ledger_path = ledger.path.display().to_string();
            let value = serde_json::json!({"adopted": adopted, "ledger": ledger_path});
            let count = value["adopted"].as_array().map_or(0, Vec::len);
            Some(outcome(
                value,
                format!("adopted {count} entries into {ledger_path}\n"),
            ))
        }
        _ => None,
    }
}

fn context(ns: &Map<String, Value>, json: bool) -> Outcome {
    let sub = text(ns, "context_command").unwrap_or("");
    if let Some(out) = context_local(ns, json) {
        return out;
    }
    if matches!(sub, "add" | "promote" | "supersede" | "retire")
        && matches!(ns.get("local"), Some(Value::Bool(true)))
    {
        return context_local_write(sub, ns, json);
    }
    // add / promote / supersede / retire go to the daemon (role-gated there) unless --local.
    if matches!(sub, "add" | "promote" | "supersede" | "retire") {
        if matches!(ns.get("local"), Some(Value::Bool(true))) {
            return not_ported(&format!("context {sub} --local"));
        }
        let stdin = std::io::stdin();
        return crate::daemon_cmd::context_write(ns, &mut stdin.lock());
    }
    let now = chrono::Local::now().naive_local();
    match context_read(sub, ns, None, now) {
        Ok(out) => Outcome {
            stdout: if json {
                format!("{}\n", ordered_json(&out.value))
            } else {
                out.text
            },
            stderr: String::new(),
            code: 0,
        },
        // `context init/add/promote/supersede/retire/ledger adopt` are writes (not ported).
        Err(error) if error.to_string().contains("not a read-side command") => {
            not_ported(&format!("context {sub}"))
        }
        // Python's `SystemExit("message")`: the message on stderr, exit 1.
        Err(error) => Outcome {
            stdout: String::new(),
            stderr: format!("{error}\n"),
            code: 1,
        },
    }
}

fn context_local_write(sub: &str, ns: &Map<String, Value>, json: bool) -> Outcome {
    use ds_core::context::store::{AddArgs, Store, SupersedeMeta};

    let global_store = matches!(ns.get("global_store"), Some(Value::Bool(true)));
    let store = Store::new(None, global_store);
    let added = match sub {
        "add" => {
            let cls = text(ns, "cls").unwrap_or("");
            let type_ = text(ns, "type_").unwrap_or("reference");
            let authority = text(ns, "authority").unwrap_or("agent");
            if cls == "ground-truth"
                || matches!(type_, "rule" | "principle")
                || authority == "owner"
            {
                return Outcome {
                    stdout: String::new(),
                    stderr: "refused: local context add cannot verify the bound owner role for protected entries; omit --local to use the daemon\n".into(),
                    code: 2,
                };
            }
            let body = if let Some(body) = text(ns, "text").filter(|body| !body.is_empty()) {
                body.to_string()
            } else {
                let file = text(ns, "file").unwrap_or("-");
                if file == "-" {
                    let mut body = String::new();
                    if let Err(error) =
                        std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut body)
                    {
                        return Outcome {
                            stdout: String::new(),
                            stderr: format!("{error}\n"),
                            code: 3,
                        };
                    }
                    body.replace("\r\n", "\n").replace('\r', "\n")
                } else {
                    match std::fs::read_to_string(file) {
                        Ok(body) => body.replace("\r\n", "\n").replace('\r', "\n"),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            return Outcome {
                                stdout: String::new(),
                                stderr: format!(
                                    "no such file: {} - the body comes from --text, stdin or a \
                                     file; for text inline use: dunkingsheep context add --text \
                                     '<claim>' --class ... --subject ...\n",
                                    ds_core::context::store::py_repr(file)
                                ),
                                code: 2,
                            };
                        }
                        Err(error) => {
                            return Outcome {
                                stdout: String::new(),
                                stderr: format!("{error}\n"),
                                code: 3,
                            };
                        }
                    }
                }
            };
            let args = AddArgs {
                cls,
                subject: text(ns, "subject").unwrap_or(""),
                type_,
                authority: text(ns, "authority"),
                source: text(ns, "source").unwrap_or(""),
                stated: text(ns, "stated"),
                recheck_after: text(ns, "recheck_after"),
                supersedes: text(ns, "supersedes"),
                ..AddArgs::default()
            };
            store.add(&body, &args, None)
        }
        "promote" => {
            let slug = text(ns, "slug").unwrap_or("");
            let Some(_entry) = store.find(slug) else {
                return Outcome {
                    stdout: String::new(),
                    stderr: String::new(),
                    code: 1,
                };
            };
            return Outcome {
                stdout: String::new(),
                stderr: "refused: local context promote cannot verify the bound owner role; omit --local to use the daemon\n".into(),
                code: 2,
            };
        }
        "supersede" => {
            let file = text(ns, "file").unwrap_or("-");
            let body = if file == "-" {
                let mut body = String::new();
                if let Err(error) =
                    std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut body)
                {
                    return Outcome {
                        stdout: String::new(),
                        stderr: format!("{error}\n"),
                        code: 3,
                    };
                }
                body.replace("\r\n", "\n").replace('\r', "\n")
            } else {
                match std::fs::read_to_string(file) {
                    Ok(body) => body.replace("\r\n", "\n").replace('\r', "\n"),
                    Err(error) => {
                        return Outcome {
                            stdout: String::new(),
                            stderr: format!("{error}\n"),
                            code: 3,
                        };
                    }
                }
            };
            let slug = text(ns, "slug").unwrap_or("");
            match store.resolve(slug) {
                Ok(Some(entry))
                    if entry.get("class") == Some("ground-truth")
                        || entry.get("authority") == Some("owner") =>
                {
                    return Outcome {
                        stdout: String::new(),
                        stderr: "refused: local context supersede cannot verify the bound owner role for protected entries; omit --local to use the daemon\n".into(),
                        code: 2,
                    };
                }
                Err(error) => {
                    return Outcome {
                        stdout: String::new(),
                        stderr: format!("context: {error}\n"),
                        code: 2,
                    };
                }
                _ => {}
            }
            store.supersede(slug, &body, None, &SupersedeMeta::default(), None)
        }
        "retire" => {
            let slug = text(ns, "slug").unwrap_or("");
            return match store.resolve(slug) {
                Ok(Some(_)) => Outcome {
                    stdout: String::new(),
                    stderr: "refused: local context retire cannot verify the bound owner role; omit --local to use the daemon\n".into(),
                    code: 2,
                },
                Ok(None) => Outcome {
                    stdout: String::new(),
                    stderr: format!("context: entry {slug} not found\n"),
                    code: 2,
                },
                Err(error) => Outcome {
                    stdout: String::new(),
                    stderr: format!("context: {error}\n"),
                    code: 2,
                },
            };
        }
        _ => unreachable!("only local context writes are routed here"),
    };

    match added {
        Ok(added) => {
            let mut value = Map::new();
            value.insert("slug".into(), Value::String(added.slug.clone()));
            value.insert(
                "path".into(),
                Value::String(added.path.display().to_string()),
            );
            value.insert("class".into(), Value::String(added.cls.clone()));
            Outcome {
                stdout: if json {
                    format!("{}\n", ordered_json(&Value::Object(value)))
                } else if sub == "promote" {
                    format!("promoted {}\n", added.slug)
                } else if sub == "supersede" {
                    format!("superseded by {}\n", added.slug)
                } else {
                    format!("added {}/{}\n", added.cls, added.slug)
                },
                stderr: String::new(),
                code: 0,
            }
        }
        Err(error) => Outcome {
            stdout: String::new(),
            stderr: format!("context: {error}\n"),
            code: 2,
        },
    }
}

/// `github wrappers [--bin-dir DIR]` and `github protect` (the owner's one-shot setup).
fn github(ns: &Map<String, Value>) -> Outcome {
    match text(ns, "github_command") {
        Some("protect") => crate::github_cmd::protect(
            text(ns, "repo").unwrap_or(""),
            ns.get("approvals").and_then(Value::as_i64).unwrap_or(1),
            matches!(ns.get("bypass_reviewer_app"), Some(Value::Bool(true))),
            matches!(ns.get("dry_run"), Some(Value::Bool(true))),
        ),
        Some("wrappers") => {
            let bin_dir = text(ns, "bin_dir").unwrap_or("~/.local/bin");
            let path = std::env::var("PATH").unwrap_or_default();
            match crate::github_cmd::wrappers_install(bin_dir, &path) {
                Ok(result) => Outcome {
                    stdout: crate::github_cmd::render_wrappers(&result),
                    stderr: String::new(),
                    code: 0,
                },
                // Python's `SystemExit("no real `gh` found on PATH to wrap")`: stderr, exit 1.
                Err(message) => Outcome {
                    stdout: String::new(),
                    stderr: format!("{message}\n"),
                    code: 1,
                },
            }
        }
        Some(other) => not_ported(&format!("github {other}")),
        None => not_ported("github"),
    }
}

fn mcp_setup_text(harness: &str, scope: &str, executable: &Path) -> (String, Option<Vec<String>>) {
    let executable = executable.display().to_string();
    let launch = format!("{executable} mcp");
    let quoted_executable = serde_json::to_string(&executable).expect("serialize executable path");
    let json_block = format!(
        "{{\n  \"mcpServers\": {{\n    \"dunkingsheep\": {{\n      \"command\": {quoted_executable},\n      \"args\": [\"mcp\"]\n    }}\n  }}\n}}"
    );
    let toml_block =
        format!("[mcp_servers.dunkingsheep]\ncommand = {quoted_executable}\nargs = [\"mcp\"]\n");
    match harness {
        "claude" => {
            let cmd = vec![
                "claude".to_string(),
                "mcp".to_string(),
                "add".to_string(),
                "--scope".to_string(),
                scope.to_string(),
                "dunkingsheep".to_string(),
                "--".to_string(),
                executable,
                "mcp".to_string(),
            ];
            let rendered = shell_join(&cmd);
            (
                format!("# Claude Code\n{rendered}\nclaude mcp list   # expect: dunkingsheep: {launch} - Connected\n"),
                Some(cmd),
            )
        }
        "codex" => {
            let cmd = vec![
                "codex".to_string(),
                "mcp".to_string(),
                "add".to_string(),
                "dunkingsheep".to_string(),
                "--".to_string(),
                executable,
                "mcp".to_string(),
            ];
            let rendered = shell_join(&cmd);
            (
                format!("# OpenAI Codex CLI\n{rendered}\ncodex mcp list    # expect a dunkingsheep row, status enabled\n# or in ~/.codex/config.toml:\n{toml_block}"),
                Some(cmd),
            )
        }
        "gemini" => {
            let cmd = vec![
                "gemini".to_string(),
                "mcp".to_string(),
                "add".to_string(),
                "--scope".to_string(),
                scope.to_string(),
                "dunkingsheep".to_string(),
                executable,
                "mcp".to_string(),
            ];
            let rendered = shell_join(&cmd);
            (
                format!("# Gemini CLI\n{rendered}\n# or add the JSON block below to ~/.gemini/settings.json\n"),
                Some(cmd),
            )
        }
        "cursor" => (
            format!("# Cursor / Windsurf / VS Code / Claude Desktop: add to the mcp.json (or claude_desktop_config.json) file:\n{json_block}\n"),
            None,
        ),
        "json" => (format!("{json_block}\n"), None),
        "toml" => (toml_block, None),
        _ => (String::new(), None),
    }
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn mcp_setup(ns: &Map<String, Value>) -> Outcome {
    let harness = text(ns, "harness").unwrap_or("all");
    let scope = text(ns, "scope").unwrap_or("user");
    let apply = matches!(ns.get("apply"), Some(Value::Bool(true)));
    let harnesses: &[&str] = if harness == "all" {
        &["claude", "codex", "gemini", "cursor"]
    } else {
        &[harness]
    };
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => {
            return Outcome {
                stdout: String::new(),
                stderr: format!("cannot locate dunkingsheep executable: {error}\n"),
                code: 3,
            }
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut code = 0;
    if harness == "all" {
        stdout.push_str("Register Dunking Sheep as an MCP server. Pick your harness, or run\n`dunkingsheep mcp-setup <harness> --apply` to have this done for you.\n\n");
    }
    for name in harnesses {
        let (instructions, command) = mcp_setup_text(name, scope, &executable);
        stdout.push_str(&instructions);
        stdout.push('\n');
        if apply && harness != "all" {
            let Some(command) = command else {
                stderr = format!("error: {name} has no CLI to apply; paste the block above\n");
                code = 2;
                break;
            };
            let path_env = std::env::var("PATH").unwrap_or_default();
            if find_executable(&command[0], &path_env).is_none() {
                stderr = format!("error: {} is not on PATH\n", command[0]);
                code = 3;
                break;
            }
            stdout.push_str(&format!("$ {}\n", command.join(" ")));
            match Command::new(&command[0])
                .args(&command[1..])
                .stdin(Stdio::inherit())
                .output()
            {
                Ok(output) => {
                    stdout.push_str(&String::from_utf8_lossy(&output.stdout));
                    stderr.push_str(&String::from_utf8_lossy(&output.stderr));
                    code = output.status.code().unwrap_or(1);
                }
                Err(error) => {
                    stderr = format!("error: cannot run {}: {error}\n", command[0]);
                    code = 3;
                }
            }
            break;
        }
    }
    Outcome {
        stdout,
        stderr,
        code,
    }
}

fn install_launcher(ns: &Map<String, Value>, json: bool) -> Outcome {
    let requested_dir = text(ns, "bin_dir").unwrap_or("~/.local/bin");
    let bin_dir = if requested_dir == "~" {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(requested_dir))
    } else if let Some(rest) = requested_dir.strip_prefix("~/") {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(rest)
    } else {
        PathBuf::from(requested_dir)
    };
    let executable = match std::env::current_exe() {
        Ok(path) => path,
        Err(error) => return install_error(error.to_string()),
    };
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")));
    let poller = project_root.join("tools/dunk_poll.py");
    let path_env = std::env::var("PATH").unwrap_or_default();
    let python = find_executable("python3", &path_env);

    let result = (|| -> Result<Value, String> {
        std::fs::create_dir_all(&bin_dir)
            .map_err(|error| format!("cannot create {}: {error}", bin_dir.display()))?;
        if !poller.is_file() {
            return Err(format!("no dunk-poll script at {}", poller.display()));
        }
        let python = python.ok_or_else(|| "no python3 found on PATH for dunk-poll".to_string())?;
        let launcher = bin_dir.join("dunkingsheep");
        let poll_launcher = bin_dir.join("dunk-poll");
        let launcher_body = format!(
            "#!/bin/sh\n# Dunking Sheep Rust CLI launcher, installed by `dunkingsheep install`.\nexec {} \"$@\"\n",
            shell_quote(&executable.display().to_string())
        );
        let poll_body = format!(
            "#!/bin/sh\nexec {} {} \"$@\"\n",
            shell_quote(&python.display().to_string()),
            shell_quote(&poller.display().to_string())
        );
        write_executable(&launcher, &launcher_body)?;
        write_executable(&poll_launcher, &poll_body)?;

        let on_path_here = std::env::split_paths(&path_env)
            .filter(|path| !path.as_os_str().is_empty())
            .any(|path| real_path(&path) == real_path(&bin_dir));
        let mut result = Map::new();
        result.insert(
            "launcher".into(),
            Value::String(launcher.display().to_string()),
        );
        result.insert(
            "poll_launcher".into(),
            Value::String(poll_launcher.display().to_string()),
        );
        result.insert(
            "runs".into(),
            Value::String(executable.display().to_string()),
        );
        result.insert(
            "poll_runs".into(),
            Value::String(poller.display().to_string()),
        );
        result.insert("on_path_here".into(), Value::Bool(on_path_here));
        result.insert(
            "resolves_to".into(),
            find_executable("dunkingsheep", &path_env)
                .map(|path| Value::String(path.display().to_string()))
                .unwrap_or(Value::Null),
        );
        result.insert(
            "poll_resolves_to".into(),
            find_executable("dunk-poll", &path_env)
                .map(|path| Value::String(path.display().to_string()))
                .unwrap_or(Value::Null),
        );
        Ok(Value::Object(result))
    })();

    match result {
        Ok(value) if json => Outcome {
            stdout: format!("{}\n", ordered_json(&value)),
            stderr: String::new(),
            code: 0,
        },
        Ok(value) => {
            let mut stdout = String::new();
            if let Some(object) = value.as_object() {
                for (key, value) in object {
                    stdout.push_str(&format!("{key:<16} {}\n", python_str(value)));
                }
            }
            if value["resolves_to"].is_null() {
                stdout.push_str(&format!(
                    "\nNote: {requested_dir} is not on this shell's PATH.\n"
                ));
            }
            stdout.push_str(
                "\nSuperuser setup: run `dunkingsheep superuser recover` from the VP pane, then `dunkingsheep superuser status`.\n",
            );
            Outcome {
                stdout,
                stderr: String::new(),
                code: 0,
            }
        }
        Err(message) => install_error(message),
    }
}

fn install_error(message: String) -> Outcome {
    Outcome {
        stdout: String::new(),
        stderr: format!("{message}\n"),
        code: 1,
    }
}

fn write_executable(path: &Path, body: &str) -> Result<(), String> {
    std::fs::write(path, body)
        .map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("cannot chmod {}: {error}", path.display()))
}

fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn find_executable(name: &str, path_env: &str) -> Option<PathBuf> {
    std::env::split_paths(path_env)
        .map(|dir| {
            if dir.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                dir
            }
        })
        .map(|dir| dir.join(name))
        .find(|path| {
            std::fs::metadata(path)
                .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        })
}

fn shell_quote(text: &str) -> String {
    if !text.is_empty()
        && text
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        return text.to_string();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

fn python_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(text) => text.clone(),
        _ => value.to_string(),
    }
}

fn guard_config_dir() -> PathBuf {
    std::env::var_os("DUNKINGSHEEP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config/dunkingsheep")
        })
}

fn executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.is_file()
        && std::fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
}

fn file_has_marker(path: &Path, marker: &[u8]) -> bool {
    std::fs::read(path)
        .map(|bytes| bytes.windows(marker.len()).any(|window| window == marker))
        .unwrap_or(false)
}

/// Python `_resolve_real_herdr()`: ignore the config wrapper directory and, when
/// PATH already contains a shadow wrapper, return its adjacent real executable.
fn resolve_real_herdr(guard_dir: &Path, home: &Path) -> (PathBuf, PathBuf) {
    let guard_real = std::fs::canonicalize(guard_dir).ok();
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            if directory.as_os_str().is_empty()
                || std::fs::canonicalize(&directory).ok() == guard_real
            {
                continue;
            }
            let candidate = directory.join("herdr");
            if !executable(&candidate) {
                continue;
            }
            let sibling = candidate.with_file_name("herdr-real");
            if file_has_marker(&candidate, b"dunkingsheep-herdr-guard") {
                if sibling.is_file() {
                    return (candidate, sibling);
                }
                continue;
            }
            return (candidate, sibling);
        }
    }
    let fallback = home.join(".local/bin/herdr");
    (fallback.clone(), fallback.with_file_name("herdr-real"))
}

fn real_tmux(guard_dir: &Path) -> Result<PathBuf, String> {
    let mut candidates = Vec::new();
    if let Some(path) = std::env::var_os("DUNKINGSHEEP_TMUX_BIN") {
        candidates.push(PathBuf::from(path));
    }
    candidates.push(guard_dir.join("tmux-real"));
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(
            std::env::split_paths(&path)
                .filter(|p| std::fs::canonicalize(p).ok() != std::fs::canonicalize(guard_dir).ok())
                .map(|p| p.join("tmux")),
        );
    }
    for path in candidates {
        let resolved = std::fs::canonicalize(&path).unwrap_or(path);
        if executable(&resolved) {
            let is_guard = std::fs::read(&resolved)
                .map(|b| {
                    b.windows(b"dunkingsheep-tmux-guard".len())
                        .any(|w| w == b"dunkingsheep-tmux-guard")
                })
                .unwrap_or(false);
            if !is_guard {
                return Ok(resolved);
            }
        }
    }
    Err("no executable non-guard tmux found; configure DUNKINGSHEEP_TMUX_BIN or install tmux on PATH".into())
}

fn guard_install_tmux() -> Result<Value, String> {
    let dir = guard_config_dir().join("tmux-guard");
    let real = real_tmux(&dir)?;
    let target = dir.join("tmux");
    if std::fs::canonicalize(&target)
        .ok()
        .is_some_and(|p| p == real)
    {
        return Err("cannot replace the selected real tmux with its guard".into());
    }
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let link = dir.join("tmux-real");
    let existing_copy = link.is_file()
        && !std::fs::symlink_metadata(&link)
            .map(|m| m.file_type().is_symlink())
            .unwrap_or(false)
        && std::fs::canonicalize(&link).ok().as_ref() == Some(&real);
    if !existing_copy {
        if std::fs::symlink_metadata(&link).is_ok() {
            std::fs::remove_file(&link).map_err(|e| e.to_string())?;
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link)
            .or_else(|_| std::fs::copy(&real, &link).map(|_| ()))
            .map_err(|e| e.to_string())?;
    }
    let source = include_str!("../../../../dunk_tmux_guard.py");
    let cli = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../dunkingsheep");
    let cli = std::fs::canonicalize(&cli).unwrap_or(cli);
    let template = "os.path.expanduser(\n    \"~/.local/bin/dunkingsheep\")";
    let rendered = source.replace(template, &format!("{:?}", cli.to_string_lossy()));
    std::fs::write(&target, &rendered).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    Ok(serde_json::json!({
        "ok": true,
        "wrapper": target,
        "real_tmux": real,
        "guard_dir": dir,
        "note": "agent panes started by the tmux backend get this first on PATH; the user's shell is untouched"
    }))
}

fn guard_install(no_shadow: bool) -> Result<Value, String> {
    // The wrapper and shell setup are confined to the caller's config directory.
    // Shadowing herdr is intentionally only requested by the default install form.
    let dir = guard_config_dir();
    let guard_dir = dir.join("bin");
    std::fs::create_dir_all(&guard_dir).map_err(|e| e.to_string())?;
    let target = guard_dir.join("herdr");
    let source = include_str!("../../../../dunk_guard_wrapper.py");
    let cli = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../dunkingsheep");
    let cli = std::fs::canonicalize(&cli).unwrap_or(cli);
    let rendered = source.replace("__DUNKINGSHEEP__", &format!("{:?}", cli.to_string_lossy()));
    std::fs::write(&target, &rendered).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| e.to_string())?;
    }
    let bashrc = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".bashrc");
    let marker = "# dunkingsheep herdr guard";
    let already = std::fs::read_to_string(&bashrc)
        .map(|s| s.contains(marker))
        .unwrap_or(false);
    let updated = if already {
        false
    } else {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&bashrc)
            .map_err(|e| e.to_string())?;
        writeln!(file, "\n{marker} (routes agent typing through the daemon; dunkingsheep guard uninstall to remove)\nexport PATH=\"{}:$PATH\"", guard_dir.display()).map_err(|e| e.to_string())?;
        true
    };
    let mut shadow_error = None;
    if !no_shadow {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let (herdr, sibling) = resolve_real_herdr(&guard_dir, &home);
        if executable(&herdr) {
            let is_guard = file_has_marker(&herdr, b"dunkingsheep-herdr-guard");
            let shadow_result = if is_guard {
                if executable(&sibling) {
                    Ok(())
                } else {
                    Err(format!(
                        "cannot find the herdr binary (looked at {})",
                        sibling.display()
                    ))
                }
            } else if sibling.exists() {
                Err(format!(
                    "{} already exists; move it aside first",
                    sibling.display()
                ))
            } else {
                std::fs::rename(&herdr, &sibling).map_err(|e| e.to_string())
            };
            match shadow_result {
                Ok(()) => {
                    if let Err(e) = std::fs::write(&herdr, &rendered) {
                        shadow_error = Some(e.to_string());
                    } else {
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            if let Err(e) = std::fs::set_permissions(
                                &herdr,
                                std::fs::Permissions::from_mode(0o755),
                            ) {
                                shadow_error = Some(e.to_string());
                            }
                        }
                    }
                }
                Err(e) => shadow_error = Some(e),
            }
        } else {
            shadow_error = Some(format!(
                "cannot find the herdr binary (looked at {})",
                herdr.display()
            ));
        }
    }
    let resolved = std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|p| p.join("herdr"))
        .find(|p| executable(p));
    let bashrc_configured = std::fs::read_to_string(&bashrc)
        .map(|s| s.contains(marker))
        .unwrap_or(false);
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let (herdr_path, herdr_real_path) = resolve_real_herdr(&guard_dir, &home);
    let shadowing =
        file_has_marker(&herdr_path, b"dunkingsheep-herdr-guard") && herdr_real_path.exists();
    let real_herdr = if shadowing {
        herdr_real_path
    } else {
        herdr_path
    };
    let tmux_dir = dir.join("tmux-guard");
    let tmux_wrapper = tmux_dir.join("tmux");
    let tmux_real = tmux_dir.join("tmux-real");
    let tmux_installed = tmux_wrapper.exists();
    let mut value = serde_json::json!({
        "installed": true, "wrapper": target, "shell_configured": bashrc_configured,
        "active_in_this_shell": resolved.as_ref().is_some_and(|p| p.parent() == Some(guard_dir.as_path())),
        "shadowing_herdr": shadowing, "covers_agents_started_by_herdr": shadowing,
        "herdr_resolves_to": resolved, "real_herdr": real_herdr,
        "guarded_commands": ["pane run", "pane send-text", "pane send-keys", "agent send", "pane close", "pane kill", "tab close", "workspace close", "pane split/rename/focus", "tab create/rename/focus", "workspace create/rename/focus", "agent start"],
        "tmux_guard": {
            "installed": tmux_installed,
            "wrapper": if tmux_wrapper.exists() { Some(tmux_wrapper) } else { None::<PathBuf> },
            "real_tmux": if tmux_real.exists() { Some(tmux_real) } else { None::<PathBuf> },
            "covers": "agent panes started by TmuxBackend (PATH prefix); user shells untouched"
        },
        "bashrc_updated": updated
    });
    if let Some(error) = shadow_error {
        value["shadow_error"] = Value::String(error);
    }
    Ok(value)
}

fn guard_status() -> Value {
    let dir = guard_config_dir();
    let guard_dir = dir.join("bin");
    let target = guard_dir.join("herdr");
    let installed = target.exists();
    let path = std::env::var_os("PATH").unwrap_or_default();
    let resolved = find_executable("herdr", &path.to_string_lossy());
    let active = resolved.as_ref().is_some_and(|path| {
        path.parent()
            .is_some_and(|parent| real_path(parent) == real_path(&guard_dir))
    });
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let bashrc = home.join(".bashrc");
    let shell_configured = std::fs::read_to_string(bashrc)
        .map(|contents| contents.contains("# dunkingsheep herdr guard"))
        .unwrap_or(false);
    let (herdr, real_herdr) = resolve_real_herdr(&guard_dir, &home);
    let shadowing = file_has_marker(&herdr, b"dunkingsheep-herdr-guard") && real_herdr.exists();
    let tmux_dir = dir.join("tmux-guard");
    let tmux_wrapper = tmux_dir.join("tmux");
    let tmux_real = tmux_dir.join("tmux-real");
    serde_json::json!({
        "installed": installed,
        "wrapper": if installed { Some(target) } else { None::<PathBuf> },
        "shell_configured": shell_configured,
        "active_in_this_shell": active,
        "shadowing_herdr": shadowing,
        "covers_agents_started_by_herdr": shadowing,
        "herdr_resolves_to": resolved,
        "real_herdr": if shadowing { real_herdr } else { herdr },
        "guarded_commands": ["pane run", "pane send-text", "pane send-keys", "agent send", "pane close", "pane kill", "tab close", "workspace close", "pane split/rename/focus", "tab create/rename/focus", "workspace create/rename/focus", "agent start"],
        "tmux_guard": {
            "installed": tmux_wrapper.exists(),
            "wrapper": if tmux_wrapper.exists() { Some(tmux_wrapper) } else { None::<PathBuf> },
            "real_tmux": if tmux_real.exists() { Some(tmux_real) } else { None::<PathBuf> },
            "covers": "agent panes started by TmuxBackend (PATH prefix); user shells untouched"
        }
    })
}

fn guard_unshadow() -> Result<Value, String> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let guard_dir = guard_config_dir().join("bin");
    let (herdr, real_herdr) = resolve_real_herdr(&guard_dir, &home);
    let restored = if file_has_marker(&herdr, b"dunkingsheep-herdr-guard") && real_herdr.exists() {
        std::fs::rename(&real_herdr, &herdr).map_err(|error| error.to_string())?;
        true
    } else {
        false
    };
    let mut value = guard_status();
    value["restored"] = Value::Bool(restored);
    value["herdr"] = serde_json::to_value(herdr).unwrap();
    Ok(value)
}

fn guard_shadow() -> Result<Value, String> {
    let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
    let dir = guard_config_dir();
    let guard_dir = dir.join("bin");
    let (herdr, real_herdr) = resolve_real_herdr(&guard_dir, &home);
    if !herdr.exists() {
        return Err(format!(
            "cannot find the herdr binary (looked at {})",
            herdr.display()
        ));
    }
    if !file_has_marker(&herdr, b"dunkingsheep-herdr-guard") {
        if real_herdr.exists() {
            return Err(format!(
                "{} already exists; move it aside first",
                real_herdr.display()
            ));
        }
        std::fs::rename(&herdr, &real_herdr).map_err(|error| error.to_string())?;
    }
    std::fs::create_dir_all(&guard_dir).map_err(|error| error.to_string())?;
    let source = include_str!("../../../../dunk_guard_wrapper.py");
    let cli = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../dunkingsheep");
    let cli = std::fs::canonicalize(&cli).unwrap_or(cli);
    let rendered = source.replace("__DUNKINGSHEEP__", &format!("{:?}", cli.to_string_lossy()));
    let wrapper = guard_dir.join("herdr");
    for target in [&herdr, &wrapper] {
        std::fs::write(target, &rendered).map_err(|error| error.to_string())?;
        std::fs::set_permissions(target, std::fs::Permissions::from_mode(0o755))
            .map_err(|error| error.to_string())?;
    }
    let mut value = guard_status();
    value["shadowed"] = Value::Bool(true);
    Ok(value)
}

fn guard_uninstall() -> Result<Value, String> {
    let _ = guard_unshadow()?;
    let target = guard_config_dir().join("bin/herdr");
    let wrapper_removed = if target.exists() {
        std::fs::remove_file(target).map_err(|error| error.to_string())?;
        true
    } else {
        false
    };
    let bashrc = PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".bashrc");
    let path_line = format!(
        "export PATH=\"{}:$PATH\"",
        guard_config_dir().join("bin").display()
    );
    let mut bashrc_cleaned = false;
    if let Ok(contents) = std::fs::read_to_string(&bashrc) {
        let mut lines = Vec::new();
        let mut skip_path_line = false;
        for line in contents.lines() {
            if line.contains("# dunkingsheep herdr guard") {
                skip_path_line = true;
                bashrc_cleaned = true;
                continue;
            }
            if skip_path_line && line.trim() == path_line {
                skip_path_line = false;
                continue;
            }
            skip_path_line = false;
            lines.push(line);
        }
        if bashrc_cleaned {
            let mut cleaned = lines.join("\n");
            if contents.ends_with('\n') && !cleaned.is_empty() {
                cleaned.push('\n');
            }
            std::fs::write(bashrc, cleaned).map_err(|error| error.to_string())?;
        }
    }
    let mut value = guard_status();
    value["wrapper_removed"] = Value::Bool(wrapper_removed);
    value["bashrc_cleaned"] = Value::Bool(bashrc_cleaned);
    Ok(value)
}

fn guard_check() -> Result<Value, String> {
    let path = guard_config_dir().join("bin/herdr");
    if !path.exists() {
        return Ok(serde_json::json!({"ok": false, "detail": "guard is not installed"}));
    }
    let mut command = Command::new("python3");
    command
        .arg(&path)
        .args(["pane", "run", "no-such-pane-xyz", "echo hi"])
        .env_remove("DUNKINGSHEEP_GUARD")
        .env_remove("HERDR_GUARD_OFF")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.stdin(Stdio::null());
    let output = command_output_timeout(&mut command, Duration::from_secs(60))?;
    let mut detail = String::from_utf8_lossy(&output.stdout).into_owned();
    detail.push_str(&String::from_utf8_lossy(&output.stderr));
    let ok = {
        let lower = detail.to_lowercase();
        lower.contains("no pane matches")
            || lower.contains("no herdr pane matches")
            || lower.contains("dunk")
    };
    let detail: String = detail.trim().chars().take(300).collect();
    Ok(serde_json::json!({"ok": ok, "detail": detail}))
}

fn command_output_timeout(
    command: &mut Command,
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| "guard check stdout was not captured".to_string())?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| "guard check stderr was not captured".to_string())?;
    let stdout_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stdout.read_to_end(&mut output).map(|_| output)
    });
    let stderr_reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        stderr.read_to_end(&mut output).map(|_| output)
    });
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!(
                    "guard check timed out after {} seconds",
                    timeout.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(error.to_string());
            }
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| "guard check stdout reader panicked".to_string())?
        .map_err(|error| error.to_string())?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "guard check stderr reader panicked".to_string())?
        .map_err(|error| error.to_string())?;
    Ok(std::process::Output {
        status: status?,
        stdout,
        stderr,
    })
}

fn owner_command(ns: &Map<String, Value>) -> Outcome {
    match text(ns, "owner_command") {
        Some("push") => {
            let mut args = Map::new();
            args.insert(
                "target".into(),
                ns.get("target").cloned().unwrap_or(Value::Null),
            );
            args.insert("body".into(), Value::String(joined_words(ns, "body")));
            args.insert(
                "subject".into(),
                ns.get("subject").cloned().unwrap_or(Value::Null),
            );
            crate::daemon_cmd::run_command(ns, "owner_push", args, |result| {
                format!(
                    "pushed owner message {} for {} (no inbox row, nothing typed)\n",
                    crate::render_print::py_str(&result["id"]),
                    crate::render_print::py_str(&result["pane_id"])
                )
            })
        }
        Some("poll") => {
            let mut args = Map::new();
            args.insert(
                "pane_id".into(),
                ns.get("pane_id").cloned().unwrap_or(Value::Null),
            );
            args.insert(
                "by".into(),
                ns.get("by")
                    .cloned()
                    .unwrap_or_else(|| Value::String("harness".into())),
            );
            crate::daemon_cmd::run_command(ns, "owner_poll", args, |result| {
                format!(
                    "poll recorded for {} by {}\n",
                    crate::render_print::py_str(&result["pane_id"]),
                    crate::render_print::py_str(&result["by"])
                )
            })
        }
        Some("take") => {
            let mut args = Map::new();
            args.insert(
                "pane_id".into(),
                ns.get("pane_id").cloned().unwrap_or(Value::Null),
            );
            args.insert("by".into(), ns.get("by").cloned().unwrap_or(Value::Null));
            let stamp = |at: f64| {
                use chrono::TimeZone;
                chrono::Local
                    .timestamp_opt(at.floor() as i64, 0)
                    .single()
                    .map(|time| time.format("%H:%M:%S").to_string())
                    .unwrap_or_default()
            };
            crate::daemon_cmd::run_command(ns, "owner_take", args, |result| {
                crate::render_print::render_owner_take(result, &stamp)
            })
        }
        other => not_ported(&format!("owner {}", other.unwrap_or(""))),
    }
}

fn read_text_file_universal_newlines(path: &str) -> std::io::Result<String> {
    std::fs::read_to_string(path).map(|contents| contents.replace("\r\n", "\n").replace('\r', "\n"))
}

fn owner_inbox(ns: &Map<String, Value>) -> Outcome {
    match text(ns, "owner_command") {
        Some("conversation") => {
            let mut args = Map::new();
            args.insert(
                "limit".into(),
                ns.get("limit").cloned().unwrap_or(Value::from(50)),
            );
            if let Some(before) = ns.get("before").filter(|value| !value.is_null()) {
                args.insert("before".into(), before.clone());
            }
            let stamp = |at: f64| {
                use chrono::TimeZone;
                chrono::Local
                    .timestamp_opt(at.floor() as i64, 0)
                    .single()
                    .map(|time| time.format("%m-%d %H:%M").to_string())
                    .unwrap_or_default()
            };
            crate::daemon_cmd::run_command(ns, "owner_conversation", args, |result| {
                crate::render_print::render_owner_conversation(result, &stamp)
            })
        }
        Some("reply") => {
            let body_file = text(ns, "body_file").filter(|path| !path.is_empty());
            let has_body = body_file.is_some()
                || ns
                    .get("body")
                    .and_then(Value::as_array)
                    .is_some_and(|items| !items.is_empty());
            let body = if let Some(path) = body_file {
                match read_text_file_universal_newlines(path) {
                    Ok(body) => body,
                    Err(error) => {
                        return Outcome {
                            stdout: String::new(),
                            stderr: format!("error: {error}\n"),
                            code: 3,
                        }
                    }
                }
            } else {
                joined_words(ns, "body")
            };
            if !has_body {
                return Outcome {
                    stdout: String::new(),
                    stderr: "owner reply: a body or --body-file is required\n".into(),
                    code: 1,
                };
            }
            let mut args = Map::new();
            args.insert(
                "message_id".into(),
                ns.get("message_id").cloned().unwrap_or(Value::Null),
            );
            args.insert("body".into(), Value::String(body));
            crate::daemon_cmd::run_command(ns, "owner_reply", args, |result| {
                let reply = result.get("reply").filter(|value| value.is_object());
                let transport = result
                    .get("transport")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("no transport");
                format!(
                    "answered {} with {} on {} (question {} acked)\n",
                    crate::render_print::py_str(&result["question"]),
                    reply
                        .map(|value| crate::render_print::py_str(&value["message_id"]))
                        .unwrap_or_else(|| "None".into()),
                    transport,
                    crate::render_print::py_str(&result["acked"])
                )
            })
        }
        Some("ingest") => {
            let body = if let Some(path) = text(ns, "body_file").filter(|path| !path.is_empty()) {
                match read_text_file_universal_newlines(path) {
                    Ok(body) => body,
                    Err(error) => {
                        return Outcome {
                            stdout: String::new(),
                            stderr: format!("error: {error}\n"),
                            code: 3,
                        }
                    }
                }
            } else {
                joined_words(ns, "body")
            };
            let mut args = Map::new();
            args.insert(
                "transport".into(),
                ns.get("transport").cloned().unwrap_or(Value::Null),
            );
            args.insert("body".into(), Value::String(body));
            for key in ["external_id", "subject", "attach"] {
                if let Some(value) = ns.get(key).filter(|value| !value.is_null()) {
                    args.insert(key.to_string(), value.clone());
                }
            }
            crate::daemon_cmd::run_command(ns, "owner_ingest", args, |result| {
                crate::render_print::render_view(result)
            })
        }
        other => not_ported(&format!("owner {}", other.unwrap_or(""))),
    }
}

fn python_json_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            ' '..='\u{7e}' => out.push(c),
            other => {
                let mut units = [0u16; 2];
                for unit in other.encode_utf16(&mut units) {
                    out.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
    }
    out.push('"');
}

struct PythonDemoObject {
    payload: Map<String, Value>,
    constants: Vec<(String, &'static str)>,
    surrogates: Vec<(String, String)>,
}

// Preserve Python's nonfinite constants and unpaired UTF-16 escapes through
// serde_json's stricter parser. Markers cannot collide with decoded user strings.
fn python_demo_object(raw: &str) -> Option<PythonDemoObject> {
    fn string_end(bytes: &[u8], start: usize) -> Option<usize> {
        let mut at = start + 1;
        while at < bytes.len() {
            match bytes[at] {
                b'\\' => at += 2,
                b'"' => return Some(at + 1),
                _ => at += 1,
            }
        }
        None
    }
    fn string_units(literal: &str) -> Option<Vec<u16>> {
        let mut units = Vec::new();
        let mut chars = literal[1..literal.len() - 1].chars();
        while let Some(ch) = chars.next() {
            if ch == '\\' {
                let unit = match chars.next()? {
                    '"' => b'"' as u16,
                    '\\' => b'\\' as u16,
                    '/' => b'/' as u16,
                    'b' => 8,
                    'f' => 12,
                    'n' => 10,
                    'r' => 13,
                    't' => 9,
                    'u' => {
                        let mut unit = 0;
                        for _ in 0..4 {
                            unit = unit * 16 + chars.next()?.to_digit(16)? as u16;
                        }
                        unit
                    }
                    _ => return None,
                };
                units.push(unit);
            } else {
                if ch < ' ' {
                    return None;
                }
                let mut encoded = [0; 2];
                units.extend_from_slice(ch.encode_utf16(&mut encoded));
            }
        }
        Some(units)
    }
    fn unicode_escape(bytes: &[u8], at: usize) -> Option<u16> {
        let escape = bytes.get(at..at + 6)?;
        if !escape.starts_with(b"\\u") {
            return None;
        }
        let mut unit = 0;
        for byte in &escape[2..] {
            unit = unit * 16 + (*byte as char).to_digit(16)? as u16;
        }
        Some(unit)
    }
    let bytes = raw.as_bytes();
    let mut strings = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        if bytes[at] == b'"' {
            let end = string_end(bytes, at)?;
            strings.push(string_units(&raw[at..end])?);
            at = end;
        } else {
            at += 1;
        }
    }
    let mut prefix = "__dunk_pm_escape_".to_string();
    loop {
        let units: Vec<u16> = prefix.encode_utf16().collect();
        if !strings.iter().any(|value| {
            value
                .windows(units.len())
                .any(|part| part == units.as_slice())
        }) {
            break;
        }
        prefix.push('_');
    }
    let constants: Vec<_> = ["NaN", "Infinity", "-Infinity"]
        .into_iter()
        .map(|token| (format!("{prefix}{token}"), token))
        .collect();
    let mut surrogates = Vec::new();
    let mut rewritten = String::new();
    at = 0;
    let mut copied = 0;
    while at < bytes.len() {
        if bytes[at] == b'"' {
            let end = string_end(bytes, at)?;
            at += 1;
            while at < end - 1 {
                if bytes[at] != b'\\' {
                    at += raw[at..].chars().next()?.len_utf8();
                    continue;
                }
                if let Some(unit) = unicode_escape(bytes, at) {
                    if (0xd800..=0xdbff).contains(&unit)
                        && unicode_escape(bytes, at + 6)
                            .is_some_and(|low| (0xdc00..=0xdfff).contains(&low))
                    {
                        at += 12;
                        continue;
                    }
                    if (0xd800..=0xdfff).contains(&unit) {
                        let marker = format!("{prefix}surrogate_{unit:04x}_");
                        rewritten.push_str(&raw[copied..at]);
                        rewritten.push_str(&marker);
                        surrogates.push((marker, format!("\\u{unit:04x}")));
                        at += 6;
                        copied = at;
                        continue;
                    }
                }
                at += 2;
            }
            at = end;
        } else if let Some((marker, token)) = constants
            .iter()
            .find(|(_, token)| raw[at..].starts_with(*token))
        {
            // Arrays cannot make an invalid, unquoted object key valid.
            rewritten.push_str(&raw[copied..at]);
            rewritten.push('[');
            python_json_string(marker, &mut rewritten);
            rewritten.push(']');
            at += token.len();
            copied = at;
        } else {
            at += raw[at..].chars().next()?.len_utf8();
        }
    }
    rewritten.push_str(&raw[copied..]);
    let value: Value = serde_json::from_str(&rewritten).ok()?;
    Some(PythonDemoObject {
        payload: value.as_object()?.clone(),
        constants,
        surrogates,
    })
}

fn python_compact_json(
    value: &Value,
    constants: &[(String, &str)],
    surrogates: &[(String, String)],
) -> String {
    fn write(value: &Value, constants: &[(String, &str)], out: &mut String) {
        match value {
            Value::Null => out.push_str("null"),
            Value::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
            Value::Number(value) => {
                let raw = value.to_string();
                if raw.bytes().any(|byte| matches!(byte, b'.' | b'e' | b'E')) {
                    match raw.parse::<f64>() {
                        Ok(number) if number.is_infinite() => {
                            out.push_str(if number.is_sign_negative() {
                                "-Infinity"
                            } else {
                                "Infinity"
                            });
                        }
                        Ok(number) => out.push_str(&ds_core::usage::py_float_repr(number)),
                        Err(_) => out.push_str(&raw),
                    }
                } else {
                    out.push_str(if raw == "-0" { "0" } else { &raw });
                }
            }
            Value::String(text) => python_json_string(text, out),
            Value::Array(items) => {
                if let [Value::String(marker)] = items.as_slice() {
                    if let Some((_, token)) = constants.iter().find(|(name, _)| name == marker) {
                        out.push_str(token);
                        return;
                    }
                }
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    write(item, constants, out);
                }
                out.push(']');
            }
            Value::Object(object) => {
                out.push('{');
                for (index, (key, item)) in object.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    python_json_string(key, out);
                    out.push_str(": ");
                    write(item, constants, out);
                }
                out.push('}');
            }
        }
    }
    let mut out = String::new();
    write(value, constants, &mut out);
    for (marker, escaped) in surrogates {
        out = out.replace(marker, escaped);
    }
    out
}

fn pm(ns: &Map<String, Value>) -> Outcome {
    let Some(subcommand) = text(ns, "pm_command") else {
        return not_ported("pm");
    };
    if subcommand == "routing" {
        return crate::daemon_cmd::run_command(
            ns,
            "pm_routing",
            Map::new(),
            crate::render_print::render_pm_routing,
        );
    }
    if subcommand != "digest" {
        return not_ported(&format!("pm {subcommand}"));
    }
    let mut raw = String::new();
    if let Err(error) = std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut raw) {
        return Outcome {
            stdout: String::new(),
            stderr: format!("error: {error}\n"),
            code: 3,
        };
    }
    if matches!(ns.get("demo"), Some(Value::Bool(true))) {
        let (mut payload, constants, surrogates) = if raw.trim_start().starts_with('{') {
            python_demo_object(&raw)
                .map(|value| (value.payload, value.constants, value.surrogates))
                .unwrap_or_else(|| {
                    let mut object = Map::new();
                    object.insert("note".into(), Value::String(raw.clone()));
                    (object, Vec::new(), Vec::new())
                })
        } else {
            let mut object = Map::new();
            object.insert("note".into(), Value::String(raw));
            (object, Vec::new(), Vec::new())
        };
        payload.insert("demo".into(), Value::Bool(true));
        raw = python_compact_json(&Value::Object(payload), &constants, &surrogates);
    }
    let mut args = Map::new();
    args.insert(
        "project".into(),
        ns.get("project").cloned().unwrap_or(Value::Null),
    );
    args.insert("payload".into(), Value::String(raw));
    args.insert("to".into(), ns.get("to").cloned().unwrap_or(Value::Null));
    let mut client = match crate::daemon_cmd::connect(ns) {
        Ok(client) => client,
        Err(outcome) => return outcome,
    };
    let result = match crate::daemon_cmd::call(&mut client, "pm_digest", args) {
        Ok(result) => result,
        Err(outcome) => return outcome,
    };
    Outcome {
        stdout: if matches!(ns.get("json"), Some(Value::Bool(true))) {
            format!("{}\n", crate::usage_cmd::ordered_json(&result))
        } else {
            crate::render_print::render_pm_digest(&result)
        },
        stderr: String::new(),
        code: if result.get("ok") == Some(&Value::Bool(false)) {
            2
        } else {
            0
        },
    }
}

fn guard(ns: &Map<String, Value>) -> Outcome {
    let action = text(ns, "action").unwrap_or("status");
    let result = match action {
        "status" => Ok(guard_status()),
        "install" => guard_install(matches!(ns.get("no_shadow"), Some(Value::Bool(true)))),
        "install-tmux" => guard_install_tmux(),
        "check" => guard_check(),
        "shadow" => guard_shadow(),
        "unshadow" => guard_unshadow(),
        "uninstall" => guard_uninstall(),
        other => return not_ported(&format!("guard {other}")),
    };
    match result {
        Ok(value) if matches!(ns.get("json"), Some(Value::Bool(true))) => Outcome {
            stdout: crate::render_print::render_json_indent2(&value),
            stderr: String::new(),
            code: if value.get("ok") == Some(&Value::Bool(false)) {
                1
            } else {
                0
            },
        },
        Ok(value) => Outcome {
            stdout: value
                .as_object()
                .unwrap()
                .iter()
                .map(|(k, v)| format!("{k:<22} {v}\n"))
                .collect(),
            stderr: String::new(),
            code: if value.get("ok") == Some(&Value::Bool(false)) {
                1
            } else {
                0
            },
        },
        Err(message) => Outcome {
            stdout: String::new(),
            stderr: format!("{message}\n"),
            code: 1,
        },
    }
}

fn owner_outbox(ns: &Map<String, Value>) -> Outcome {
    let mut args = Map::new();
    let (command, printer): (&str, fn(&Value) -> String) = match text(ns, "owner_command") {
        Some("outbox") => {
            args.insert(
                "transport".into(),
                ns.get("transport").cloned().unwrap_or(Value::Null),
            );
            args.insert(
                "limit".into(),
                ns.get("limit").cloned().unwrap_or(Value::from(20)),
            );
            (
                "owner_outbox",
                crate::render_print::render_view as fn(&Value) -> String,
            )
        }
        Some("delivered") => {
            args.insert(
                "message_id".into(),
                ns.get("message_id").cloned().unwrap_or(Value::Null),
            );
            args.insert(
                "transport".into(),
                ns.get("transport").cloned().unwrap_or(Value::Null),
            );
            if let Some(external_id) = ns.get("external_id").filter(|value| !value.is_null()) {
                args.insert("external_id".into(), external_id.clone());
            }
            ("owner_delivered", crate::render_print::render_view)
        }
        Some("stats") => ("owner_stats", crate::render_print::render_owner_stats),
        Some(other) => return not_ported(&format!("owner {other}")),
        None => ("owner_stats", crate::render_print::render_owner_stats),
    };
    crate::daemon_cmd::run_command(ns, command, args, printer)
}

fn state(ns: &Map<String, Value>) -> Outcome {
    let command = text(ns, "state_command").unwrap_or("");
    let (rpc, args) = match command {
        "capture" => {
            let mut args = Map::new();
            args.insert(
                "source".into(),
                ns.get("source")
                    .cloned()
                    .unwrap_or_else(|| Value::String("periodic".into())),
            );
            ("state_capture", args)
        }
        "latest" => {
            let mut args = Map::new();
            for (source, target) in [
                ("pane", "pane_id"),
                ("project", "project"),
                ("limit", "limit"),
            ] {
                if let Some(value) = ns.get(source).filter(|value| !value.is_null()) {
                    args.insert(target.into(), value.clone());
                }
            }
            ("state_latest", args)
        }
        other => return not_ported(&format!("state {other}")),
    };
    crate::daemon_cmd::run_command(ns, rpc, args, crate::render_print::render_view)
}

fn read_view(ns: &Map<String, Value>) -> Outcome {
    let command = text(ns, "command").unwrap_or("");
    let mut args = Map::new();
    let rpc = match command {
        "roster" => "roster",
        "message-state" => {
            args.insert(
                "message_ids".into(),
                ns.get("ids").cloned().unwrap_or(Value::Null),
            );
            "message_state"
        }
        "reply-stats" => {
            if let Some(mailboxes) = ns
                .get("mailboxes")
                .filter(|v| !v.is_null() && !v.as_str().unwrap_or("").is_empty())
            {
                args.insert("mailboxes".into(), mailboxes.clone());
            }
            "reply_stats"
        }
        "project-summary" => {
            args.insert(
                "project".into(),
                ns.get("project").cloned().unwrap_or(Value::Null),
            );
            if let Some(path) = text(ns, "path").filter(|path| !path.is_empty()) {
                args.insert("path".into(), Value::String(path.to_string()));
            }
            "project_summary"
        }
        other => return not_ported(other),
    };
    crate::daemon_cmd::run_command(ns, rpc, args, crate::render_print::render_view)
}

fn dispatch_route(ns: &Map<String, Value>) -> Outcome {
    let mut args = Map::new();
    args.insert(
        "action".into(),
        Value::String(text(ns, "dispatch_command").unwrap_or("").to_owned()),
    );
    if let Some(brief) = ns
        .get("brief")
        .and_then(Value::as_array)
        .filter(|words| !words.is_empty())
    {
        args.insert(
            "brief".into(),
            Value::String(
                brief
                    .iter()
                    .map(crate::render_fleet::py_str)
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        );
    }
    // Match run_client_command's exact optional-field forwarding. The parsed --lanes option is
    // not included by the Python route today; only task_id, lane and id are forwarded here.
    for key in ["task_id", "lane", "id"] {
        if let Some(value) = ns.get(key).filter(|value| !value.is_null()) {
            args.insert(key.into(), value.clone());
        }
    }
    if matches!(ns.get("dry_run"), Some(Value::Bool(true))) {
        args.insert("dry_run".into(), Value::Bool(true));
    }
    crate::daemon_cmd::run_command(ns, "dispatch", args, |result| {
        crate::render_print::render_dispatch(result)
            .unwrap_or_else(|| crate::render_print::render_dispatch_tick(result))
    })
}

fn delivery_queue_route(ns: &Map<String, Value>) -> Outcome {
    match text(ns, "command").unwrap_or("") {
        "clear-box" => {
            let mut args = Map::new();
            args.insert(
                "target".into(),
                ns.get("target").cloned().unwrap_or(Value::Null),
            );
            args.insert(
                "reason".into(),
                ns.get("reason").cloned().unwrap_or(Value::Null),
            );
            crate::daemon_cmd::run_command(ns, "clear_box", args, crate::render_fleet::render_clear)
        }
        "delivery" => {
            let mut args = Map::new();
            let target = text(ns, "target").unwrap_or("").to_owned();
            args.insert("target".into(), Value::String(target.clone()));
            args.insert(
                "status".into(),
                ns.get("status").cloned().unwrap_or(Value::Null),
            );
            crate::daemon_cmd::run_command(ns, "delivery", args, move |result| {
                let reason = if crate::render_print::truthy(&result["reason"]) {
                    format!(" - {}", crate::render_fleet::py_str(&result["reason"]))
                } else {
                    String::new()
                };
                format!(
                    "{target}: {}{reason}\n",
                    crate::render_fleet::py_str(&result["state"])
                )
            })
        }
        "flush" => {
            let mut args = Map::new();
            args.insert(
                "target".into(),
                ns.get("target").cloned().unwrap_or(Value::Null),
            );
            crate::daemon_cmd::run_command(ns, "flush_box", args, |result| {
                let mut output = format!(
                    "{} ({}): {}",
                    crate::render_fleet::py_str(&result["target_label"]),
                    crate::render_fleet::py_str(&result["pane_id"]),
                    crate::render_fleet::py_str(&result["message"])
                );
                if crate::render_print::truthy(&result["text"]) {
                    let text = crate::render_fleet::py_str(&result["text"]);
                    let preview: String = text.chars().take(60).collect();
                    output.push_str(&format!(
                        " -> {}",
                        crate::render_print::py_repr_str(&preview)
                    ));
                }
                output.push('\n');
                output
            })
        }
        "messages" | "queue" => crate::daemon_cmd::run_command(
            ns,
            "list_messages",
            Map::new(),
            crate::render_fleet::render_messages,
        ),
        "cancel-message" | "cancel" => {
            let mut args = Map::new();
            args.insert(
                "message_id".into(),
                ns.get("message_id").cloned().unwrap_or(Value::Null),
            );
            crate::daemon_cmd::run_command(ns, "cancel_message", args, |result| {
                format!(
                    "cancelled {} for {}\n",
                    crate::render_fleet::py_str(&result["message_id"]),
                    crate::render_fleet::py_str(&result["target_label"])
                )
            })
        }
        "metrics" => {
            let mut args = Map::new();
            for (key, value) in [
                ("window_s", "window"),
                ("since", "since"),
                ("until", "until"),
            ] {
                args.insert(key.into(), ns.get(value).cloned().unwrap_or(Value::Null));
            }
            crate::daemon_cmd::run_command(
                ns,
                "metrics",
                args,
                crate::render_print::render_json_indent2,
            )
        }
        other => not_ported(other),
    }
}

fn poolrad(ns: &Map<String, Value>) -> Outcome {
    match text(ns, "poolrad_command") {
        Some("backlog-sync") => {
            let mut args = Map::new();
            args.insert(
                "limit".into(),
                ns.get("limit").cloned().unwrap_or(Value::from(6)),
            );
            args.insert(
                "dry_run".into(),
                Value::Bool(matches!(ns.get("dry_run"), Some(Value::Bool(true)))),
            );
            crate::daemon_cmd::run_command(
                ns,
                "poolrad_sync",
                args,
                crate::render_fleet::render_poolrad_sync,
            )
        }
        Some("progress") => crate::daemon_cmd::run_command(
            ns,
            "poolrad_progress",
            Map::new(),
            crate::render_fleet::render_poolrad_progress,
        ),
        Some(other) => not_ported(&format!("poolrad {other}")),
        None => not_ported("poolrad"),
    }
}

fn review(ns: &Map<String, Value>) -> Outcome {
    match text(ns, "review_command") {
        Some("sync") => {
            let mut args = Map::new();
            args.insert(
                "dry_run".into(),
                Value::Bool(matches!(ns.get("dry_run"), Some(Value::Bool(true)))),
            );
            crate::daemon_cmd::run_command(
                ns,
                "review_sync",
                args,
                crate::render_fleet::render_review_sync,
            )
        }
        Some("list") => crate::daemon_cmd::run_command(
            ns,
            "review_list",
            Map::new(),
            crate::render_fleet::render_review_list,
        ),
        Some(other) => not_ported(&format!("review {other}")),
        None => not_ported("review"),
    }
}

fn load_report(ns: &Map<String, Value>) -> Outcome {
    let mut client = match crate::daemon_cmd::connect(ns) {
        Ok(client) => client,
        Err(outcome) => return outcome,
    };
    let mut args = Map::new();
    args.insert("limit".into(), Value::from(500));
    let result = match crate::daemon_cmd::call(&mut client, "task_list", args) {
        Ok(result) => result,
        Err(outcome) => return outcome,
    };
    let tasks = result
        .get("tasks")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let callers = ds_core::load::registered_callers(tasks);
    let policies_path = text(ns, "policies").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join(".config/dunkingsheep/load-policies.json")
    });
    let policies = std::fs::read(&policies_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| {
            let agents = value
                .get("agents")
                .filter(|v| v.as_object().is_some_and(|object| !object.is_empty()));
            agents.or(Some(&value)).and_then(Value::as_object).cloned()
        });
    let env = ds_core::config::Env::from_process();
    let state_path = match ds_core::load::default_state_path(&env) {
        Ok(path) => path,
        Err(error) => return load_failure(error.to_string(), 3),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64());
    let evaluate = |sample: &Map<String, Value>, policy: &Map<String, Value>| {
        ds_core::policy::evaluate(sample, policy).map_err(|error| error.to_string())
    };
    let report =
        match ds_core::load::collect(&callers, now, policies.as_ref(), &evaluate, &state_path) {
            Ok(report) => report,
            Err(error) => return load_failure(error.to_string(), error.exit_code().max(2)),
        };
    if matches!(ns.get("json"), Some(Value::Bool(true))) {
        Outcome {
            stdout: format!("{}\n", crate::usage_cmd::ordered_json(&report)),
            stderr: String::new(),
            code: 0,
        }
    } else {
        Outcome {
            stdout: crate::render_print::render_load(&report),
            stderr: String::new(),
            code: 0,
        }
    }
}

fn load_failure(message: String, code: i32) -> Outcome {
    Outcome {
        stdout: String::new(),
        stderr: format!("error: {message}\n"),
        code,
    }
}

fn allocation(ns: &Map<String, Value>) -> Outcome {
    let mut args = Map::new();
    for key in [
        "action", "pane_id", "pool", "pools", "roles", "skills", "requires", "task_id", "revision",
        "body",
    ] {
        if let Some(value) = ns.get(key).filter(|value| !value.is_null()) {
            args.insert(key.to_string(), value.clone());
        }
    }
    if text(ns, "action") == Some("register") {
        args.insert(
            "enabled".into(),
            Value::Bool(!matches!(ns.get("disable"), Some(Value::Bool(true)))),
        );
    }
    crate::daemon_cmd::run_command(
        ns,
        "allocation",
        args,
        crate::render_print::render_json_indent2,
    )
}

fn report(ns: &Map<String, Value>) -> Outcome {
    match text(ns, "report_command") {
        Some("list") => {
            let mut args = Map::new();
            args.insert(
                "out_dir".into(),
                ns.get("out_dir").cloned().unwrap_or(Value::Null),
            );
            let stamp = |at: f64| {
                use chrono::TimeZone;
                chrono::Local
                    .timestamp_opt(at.floor() as i64, 0)
                    .single()
                    .map(|time| time.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or_default()
            };
            crate::daemon_cmd::run_command(ns, "report_list", args, |result| {
                crate::render_print::render_report_list(result, &stamp)
            })
        }
        Some("run") => {
            let prose = if let Some(path) = text(ns, "prose_file").filter(|p| !p.is_empty()) {
                match std::fs::read_to_string(path) {
                    Ok(prose) => prose,
                    Err(error) => {
                        return Outcome {
                            stdout: String::new(),
                            stderr: format!("error: {error}\n"),
                            code: 3,
                        }
                    }
                }
            } else {
                text(ns, "prose").unwrap_or_default().to_string()
            };
            let mut args = Map::new();
            args.insert(
                "post".into(),
                Value::Bool(!matches!(ns.get("no_post"), Some(Value::Bool(true)))),
            );
            if !prose.is_empty() {
                args.insert("prose".into(), Value::String(prose));
            }
            if let Some(out_dir) = text(ns, "out_dir").filter(|value| !value.is_empty()) {
                args.insert("out_dir".into(), Value::String(out_dir.to_string()));
            }
            crate::daemon_cmd::run_command(ns, "report_run", args, |result| {
                crate::render_fleet::render_report_run(result)
            })
        }
        Some(other) => not_ported(&format!("report {other}")),
        None => not_ported("report"),
    }
}

fn token(ns: &Map<String, Value>) -> Outcome {
    match text(ns, "token_command") {
        Some("issue") => {
            let mut args = Map::new();
            args.insert(
                "role".into(),
                ns.get("role").cloned().unwrap_or(Value::Null),
            );
            args.insert(
                "label".into(),
                ns.get("label").cloned().unwrap_or(Value::Null),
            );
            crate::daemon_cmd::run_command(ns, "token_issue", args, |result| {
                format!("{}\n", crate::render_fleet::py_str(&result["token"]))
            })
        }
        Some("revoke") => {
            let mut args = Map::new();
            args.insert(
                "label".into(),
                ns.get("label").cloned().unwrap_or(Value::Null),
            );
            crate::daemon_cmd::run_command(ns, "token_revoke", args, |result| {
                format!(
                    "revoked {} token(s) labelled {}\n",
                    crate::render_fleet::py_str(&result["revoked"]),
                    crate::render_fleet::py_str(&result["label"])
                )
            })
        }
        Some("list") => {
            crate::daemon_cmd::run_command(ns, "superuser_status", Map::new(), |result| {
                let rows = result
                    .get("tokens")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|token| {
                        let label = crate::render_fleet::py_str(&token["label"]);
                        let role = crate::render_fleet::py_str(&token["role"]);
                        let padding = 24usize.saturating_sub(label.chars().count());
                        format!("{label}{} {role}", " ".repeat(padding))
                    })
                    .collect::<Vec<_>>();
                if rows.is_empty() {
                    "no tokens\n".to_string()
                } else {
                    format!("{}\n", rows.join("\n"))
                }
            })
        }
        Some(other) => not_ported(&format!("token {other}")),
        None => not_ported("token"),
    }
}

/// Run the local HTTP facade. The HTTP adapter owns the listener and makes its
/// daemon connection lazily when a request reaches a daemon-backed route.
fn api(ns: &Map<String, Value>) -> Outcome {
    let host = text(ns, "host").unwrap_or("127.0.0.1").to_owned();
    let port_value = ns.get("port").cloned().unwrap_or_else(|| Value::from(8792));
    let port = match &port_value {
        Value::Number(number) => number.as_u64().and_then(|port| u16::try_from(port).ok()),
        Value::String(value) => value.parse::<u16>().ok(),
        _ => None,
    };
    let port = match port {
        Some(port) => port,
        None => {
            let invalid_port = match &port_value {
                Value::String(value) => value.clone(),
                value => value.to_string(),
            };
            return Outcome {
                stdout: String::new(),
                stderr: format!("invalid --port: {invalid_port:?}\n"),
                code: 2,
            };
        }
    };

    // The HTTP adapter resolves its daemon client from the process environment,
    // just as dunk_http._client() does. Carry the CLI's selected socket into it.
    if let Some(socket) = text(ns, "socket").filter(|value| !value.is_empty()) {
        std::env::set_var("DUNKINGSHEEP_SOCKET", socket);
    }
    if let Some(self_pane) = text(ns, "self_pane").filter(|value| !value.is_empty()) {
        std::env::set_var("HERDR_PANE_ID", self_pane);
    }

    let daemon_executable =
        std::env::current_exe().unwrap_or_else(|_| PathBuf::from("dunkingsheep"));
    let socket_path = text(ns, "socket")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let self_pane = text(ns, "self_pane").filter(|value| !value.is_empty());
    let start_daemon = !matches!(ns.get("no_start_daemon"), Some(Value::Bool(true)));
    if let Err(error) = ds_core::client::connect_or_start_system(
        socket_path.as_deref(),
        self_pane,
        start_daemon,
        Duration::from_secs(10),
        &daemon_executable,
    ) {
        let (message, code) = match error {
            ds_core::error::DunkError::Unavailable(message) => (message, 3),
            error => (error.to_string(), error.exit_code().max(2)),
        };
        return Outcome {
            stdout: String::new(),
            stderr: format!("error: {message}\n"),
            code,
        };
    }
    let app = match ds_web::http_adapter::system_router(daemon_executable) {
        Ok(app) => app,
        Err(error) => {
            return Outcome {
                stdout: String::new(),
                stderr: format!("error: {error}\n"),
                code: 1,
            }
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return Outcome {
                stdout: String::new(),
                stderr: format!("error: could not create HTTP runtime: {error}\n"),
                code: 1,
            }
        }
    };
    let result = runtime.block_on(async {
        let listener = ds_web::http_adapter::bind_listener(&host, port).await?;
        ds_web::http_adapter::serve_configured(listener, app, &host, port).await
    });
    match result {
        Ok(()) => Outcome {
            stdout: String::new(),
            stderr: String::new(),
            code: 0,
        },
        Err(error) => Outcome {
            stdout: String::new(),
            stderr: format!("error: {error}\n"),
            code: 1,
        },
    }
}

/// Run one parsed command (`ns` is the argparse namespace as a dictionary).
pub fn run(ns: &Map<String, Value>) -> Outcome {
    let command = text(ns, "command").unwrap_or("");
    let json = matches!(ns.get("json"), Some(Value::Bool(true)));
    match command {
        "guide" | "help" | "docs" => guide(ns),
        "commands" => commands(json),
        "mcp-map" => mcp_map(ns),
        // Match the Python dispatcher: MCP inherits these CLI overrides through its environment.
        // The shared server still resolves identity normally, including TMUX_PANE precedence and
        // its terminal verification; HERDR_PANE_ID is only set when the caller supplied it.
        "mcp" => {
            if let Some(socket) = text(ns, "socket").filter(|value| !value.is_empty()) {
                std::env::set_var("DUNKINGSHEEP_SOCKET", socket);
            }
            if let Some(self_pane) = text(ns, "self_pane").filter(|value| !value.is_empty()) {
                std::env::set_var("HERDR_PANE_ID", self_pane);
            }
            // MCP owns stdout for JSON-RPC even when the CLI's global --json flag is present.
            Outcome {
                stdout: String::new(),
                stderr: String::new(),
                code: ds_mcp::run_stdio(),
            }
        }
        "install" => install_launcher(ns, json),
        "api" => api(ns),
        "mcp-setup" => mcp_setup(ns),
        "context" => context(ns, json),
        // `secrets-check [--base REF] [--cwd DIR] [--quiet]`: exit 1 + the refusal on stderr.
        "secrets-check" => crate::secrets_cmd::secrets_check_command(
            text(ns, "cwd").filter(|c| !c.is_empty()),
            text(ns, "base").filter(|b| !b.is_empty()),
            matches!(ns.get("quiet"), Some(Value::Bool(true))),
        ),
        "usage" => {
            let config = text(ns, "config").unwrap_or("");
            usage_command(
                config,
                text(ns, "provider"),
                ns.get("max_age").and_then(Value::as_f64),
                json,
            )
        }
        "watchdog-deadman" => watchdog_deadman(ns),
        "github" => github(ns),
        "guard" => guard(ns),
        "report" => report(ns),
        "superuser" => superuser(ns),
        "health" => health(ns),
        "recover" => recover(ns),
        "capability" | "capabilities" => capability(ns),
        "lane" | "lanes" => lane(ns),
        "retired" => retired(ns),
        "memory" => match text(ns, "memory_command") {
            Some("hook-context" | "stats" | "status") => memory_aux(ns),
            _ => memory(ns),
        },
        "allocation" => allocation(ns),
        "load" => load_report(ns),
        "pm" => pm(ns),
        "owner" => match text(ns, "owner_command") {
            Some("conversation" | "reply" | "ingest") => owner_inbox(ns),
            Some("outbox" | "delivered" | "stats") | None => owner_outbox(ns),
            _ => owner_command(ns),
        },
        "token" => token(ns),
        "review" => review(ns),
        "state" => state(ns),
        "roster" | "message-state" | "reply-stats" | "project-summary" => read_view(ns),
        "poolrad" => poolrad(ns),
        "dispatch" => dispatch_route(ns),
        "clear-box" | "delivery" | "flush" | "messages" | "queue" | "cancel-message" | "cancel"
        | "metrics" => delivery_queue_route(ns),
        "gh-token" => crate::daemon_cmd::gh_token(ns),
        "tui" => tui(ns),
        "serve" => serve(ns),
        // `client.call("list_panes"), print_panes`
        "fire" | "test" => {
            let mut args = Map::new();
            args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
            args.insert(
                "countdown_s".into(),
                ns.get("countdown").cloned().unwrap_or(Value::from(0)),
            );
            args.insert("wait".into(), Value::Bool(true));
            crate::daemon_cmd::run_command(ns, "fire_dunk", args, |result| {
                let state = if crate::render_print::truthy(&result["queued"]) {
                    "queued"
                } else if crate::render_print::truthy(&result["ok"]) {
                    "sent"
                } else {
                    "FAILED"
                };
                format!(
                    "{}: {state} ({})\n",
                    crate::render_fleet::py_str(&result["id"]),
                    crate::render_fleet::py_str(&result["message"])
                )
            })
        }
        "send" | "send-keys" => {
            let is_keys = command == "send-keys";
            let mut args = Map::new();
            args.insert(
                "target".into(),
                ns.get("target").cloned().unwrap_or(Value::Null),
            );
            args.insert(
                if is_keys { "keys" } else { "text" }.into(),
                Value::String(joined_words(ns, if is_keys { "keys" } else { "text" })),
            );
            args.insert(
                "hold_while_typing".into(),
                Value::Bool(!matches!(ns.get("ignore_typing"), Some(Value::Bool(true)))),
            );
            args.insert(
                "wait_s".into(),
                Value::from(ns.get("wait").and_then(Value::as_f64).unwrap_or(10.0)),
            );
            args.insert(
                "desk_token".into(),
                ns.get("desk_token").cloned().unwrap_or(Value::Null),
            );
            if !is_keys {
                args.insert(
                    "press_enter".into(),
                    Value::Bool(!matches!(ns.get("no_enter"), Some(Value::Bool(true)))),
                );
            }
            let rpc = if is_keys { "send_keys" } else { "send_text" };
            crate::daemon_cmd::run_command(ns, rpc, args, crate::render_fleet::render_message)
        }
        "panes" => crate::daemon_cmd::run_command(
            ns,
            "list_panes",
            Map::new(),
            crate::render_print::render_panes,
        ),
        // `workspaces` and its alias `tabs`: `client.call("list_workspaces"), print_workspaces`
        "workspaces" => crate::daemon_cmd::run_command(
            ns,
            "list_workspaces",
            Map::new(),
            crate::render_fleet::render_workspaces,
        ),
        "new-workspace" | "workspace" => {
            let mut args = Map::new();
            for key in [
                "label", "cwd", "run", "focus", "mailbox", "role", "provider", "model",
            ] {
                args.insert(
                    match key {
                        "run" => "command",
                        other => other,
                    }
                    .into(),
                    ns.get(key).cloned().unwrap_or(Value::Null),
                );
            }
            run_command_with_stderr(ns, "create_workspace", args, |result| {
                (
                    crate::render_fleet::render_created(result),
                    crate::render_fleet::created_error(result).unwrap_or_default(),
                )
            })
        }
        "new-tab" | "tab" => {
            let mut args = Map::new();
            for key in ["workspace", "label", "cwd", "run", "focus"] {
                args.insert(
                    if key == "run" { "command" } else { key }.into(),
                    ns.get(key).cloned().unwrap_or(Value::Null),
                );
            }
            run_command_with_stderr(ns, "create_tab", args, |result| {
                (
                    crate::render_fleet::render_created(result),
                    crate::render_fleet::created_error(result).unwrap_or_default(),
                )
            })
        }
        "split" => {
            let mut args = Map::new();
            args.insert(
                "target".into(),
                ns.get("target").cloned().unwrap_or(Value::Null),
            );
            args.insert(
                "direction".into(),
                Value::String(
                    if matches!(ns.get("down"), Some(Value::Bool(true))) {
                        "down"
                    } else {
                        "right"
                    }
                    .into(),
                ),
            );
            for key in ["cwd", "run", "focus"] {
                args.insert(
                    if key == "run" { "command" } else { key }.into(),
                    ns.get(key).cloned().unwrap_or(Value::Null),
                );
            }
            run_command_with_stderr(ns, "split_pane", args, |result| {
                (
                    crate::render_fleet::render_created(result),
                    crate::render_fleet::created_error(result).unwrap_or_default(),
                )
            })
        }
        "start-agent" | "agent" => {
            let argv = ns.get("argv").map(string_values).unwrap_or_default();
            let mut args = Map::new();
            args.insert(
                "name".into(),
                ns.get("name").cloned().unwrap_or(Value::Null),
            );
            args.insert(
                "command".into(),
                Value::String(ds_core::flock::commands_layout::shlex_join(&argv)),
            );
            for key in [
                "workspace",
                "tab",
                "split",
                "cwd",
                "focus",
                "role",
                "provider",
                "model",
            ] {
                args.insert(key.into(), ns.get(key).cloned().unwrap_or(Value::Null));
            }
            crate::daemon_cmd::run_command(ns, "start_agent", args, |result| {
                let argv = string_values(&result["argv"]);
                format!(
                    "started agent {} in {}: {}\n",
                    crate::render_fleet::py_str(&result["name"]),
                    crate::render_fleet::py_str(&result["pane_id"]),
                    ds_core::flock::commands_layout::shlex_join(&argv)
                )
            })
        }
        "respawn-pane" | "respawn" => {
            let (argv, rollback) = match respawn_argv(ns) {
                Ok(values) => values,
                Err(outcome) => return outcome,
            };
            let mut args = Map::new();
            for (key, value) in [
                ("pane", ns.get("pane").cloned().unwrap_or(Value::Null)),
                ("argv", argv),
                ("rollback_argv", rollback),
                (
                    "expected_pid",
                    ns.get("expected_pid").cloned().unwrap_or(Value::Null),
                ),
                (
                    "expected_start",
                    ns.get("expected_start").cloned().unwrap_or(Value::Null),
                ),
                (
                    "confirm_idle",
                    ns.get("confirm_idle")
                        .cloned()
                        .unwrap_or(Value::Bool(false)),
                ),
                ("reason", ns.get("reason").cloned().unwrap_or(Value::Null)),
            ] {
                args.insert(key.into(), value);
            }
            crate::daemon_cmd::run_command(ns, "respawn_pane", args, |result| {
                format!(
                    "respawned pane {} ({} -> {}); reason={}\n",
                    crate::render_fleet::py_str(&result["pane_id"]),
                    crate::render_fleet::py_str(&result["old_pid"]),
                    crate::render_fleet::py_str(&result["pane_pid"]),
                    crate::render_fleet::py_str(&result["reason"])
                )
            })
        }
        "rename" => {
            let mut args = Map::new();
            for key in ["kind", "target", "label"] {
                args.insert(key.into(), ns.get(key).cloned().unwrap_or(Value::Null));
            }
            crate::daemon_cmd::run_command(ns, "rename", args, crate::render_fleet::render_renamed)
        }
        "focus" => {
            let mut args = Map::new();
            for key in ["kind", "target"] {
                args.insert(key.into(), ns.get(key).cloned().unwrap_or(Value::Null));
            }
            crate::daemon_cmd::run_command(ns, "focus", args, |result| {
                format!(
                    "focused {} {}\n",
                    crate::render_fleet::py_str(&result["kind"]),
                    crate::render_fleet::py_str(&result["id"])
                )
            })
        }
        "close" => {
            let mut args = Map::new();
            for key in ["kind", "target"] {
                args.insert(key.into(), ns.get(key).cloned().unwrap_or(Value::Null));
            }
            args.insert(
                "force".into(),
                ns.get("force").cloned().unwrap_or(Value::Bool(false)),
            );
            crate::daemon_cmd::run_command(ns, "close", args, |result| {
                let joined = |key: &str| {
                    result[key]
                        .as_array()
                        .map(|values| {
                            values
                                .iter()
                                .map(crate::render_fleet::py_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .unwrap_or_default()
                };
                let panes = joined("closed_panes");
                let stopped = joined("stopped_dunks");
                format!(
                    "closed {} {}; panes {}{}\n",
                    crate::render_fleet::py_str(&result["kind"]),
                    crate::render_fleet::py_str(&result["id"]),
                    if panes.is_empty() { "-" } else { &panes },
                    if stopped.is_empty() {
                        String::new()
                    } else {
                        format!("; stopped dunks {stopped}")
                    }
                )
            })
        }
        "herdr" => {
            let argv = ns.get("argv").map(string_values).unwrap_or_default();
            let mut args = Map::new();
            args.insert(
                "command".into(),
                Value::String(ds_core::flock::commands_layout::shlex_join(&argv)),
            );
            run_command_with_stderr(ns, "herdr", args, |result| {
                let (stderr, stdout) = crate::render_print::render_herdr(result);
                (stdout, stderr)
            })
        }
        "herdr-help" => {
            let mut args = Map::new();
            args.insert(
                "topic".into(),
                ns.get("topic").cloned().unwrap_or(Value::Null),
            );
            crate::daemon_cmd::run_command(ns, "herdr_help", args, |result| {
                let text = crate::render_fleet::py_str(&result["text"]);
                if text.ends_with('\n') {
                    text
                } else {
                    format!("{text}\n")
                }
            })
        }
        "read" => {
            let mut args = Map::new();
            args.insert(
                "target".into(),
                ns.get("target").cloned().unwrap_or(Value::Null),
            );
            args.insert(
                "lines".into(),
                ns.get("lines").cloned().unwrap_or(Value::from(40)),
            );
            crate::daemon_cmd::run_command(ns, "read_pane", args, |result| {
                let text = crate::render_fleet::py_str(&result["text"]);
                if text.ends_with('\n') {
                    text
                } else {
                    format!("{text}\n")
                }
            })
        }
        "shutdown" => {
            let mut args = Map::new();
            args.insert(
                "stop_all".into(),
                ns.get("stop_all").cloned().unwrap_or(Value::Bool(false)),
            );
            crate::daemon_cmd::run_command(ns, "shutdown", args, |result| {
                format!(
                    "daemon shutting down{}\n",
                    if crate::render_fleet::truthy(&result["stopped_all"]) {
                        " (all dunks stopped)"
                    } else {
                        ""
                    }
                )
            })
        }
        "list" | "ls" => crate::daemon_cmd::run_command(ns, "list_dunks", Map::new(), |result| {
            crate::render_print::render_dunks(result["dunks"].as_array().map_or(&[], Vec::as_slice))
        }),
        "get" => {
            let mut args = Map::new();
            args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
            crate::daemon_cmd::run_command(ns, "get_dunk", args, |result| {
                crate::render_print::render_dunk(result)
            })
        }
        "add" => {
            let mut args = Map::new();
            args.insert(
                "text".into(),
                ns.get("text")
                    .cloned()
                    .unwrap_or_else(|| Value::String("continue".into())),
            );
            args.insert(
                "interval_minutes".into(),
                ns.get("every")
                    .cloned()
                    .unwrap_or_else(|| Value::String("10".into())),
            );
            args.insert(
                "start".into(),
                Value::Bool(!matches!(ns.get("no_start"), Some(Value::Bool(true)))),
            );
            if let Some(target) = text(ns, "target").filter(|value| !value.is_empty()) {
                args.insert("target".into(), Value::String(target.to_string()));
            }
            if let Some(name) = text(ns, "name").filter(|value| !value.is_empty()) {
                args.insert("name".into(), Value::String(name.to_string()));
            }
            if matches!(ns.get("only_idle"), Some(Value::Bool(true))) {
                args.insert("only_when".into(), Value::String("idle".into()));
            }
            if let Some(max_sends) = ns
                .get("max_sends")
                .filter(|value| crate::render_print::truthy(value))
            {
                args.insert("max_sends".into(), max_sends.clone());
            }
            if matches!(ns.get("skip_if_unconsumed"), Some(Value::Bool(true))) {
                args.insert("skip_if_unconsumed".into(), Value::Bool(true));
            }
            if matches!(ns.get("ignore_typing"), Some(Value::Bool(true))) {
                args.insert("hold_while_typing".into(), Value::Bool(false));
            }
            crate::daemon_cmd::run_command(ns, "add_dunk", args, |result| {
                crate::render_print::render_dunk(result)
            })
        }
        "inbox" => match text(ns, "inbox_command").unwrap_or("") {
            "ack" | "cancel" | "delete" => {
                let command = text(ns, "inbox_command").unwrap_or("ack");
                let ids = ns
                    .get("message_ids")
                    .and_then(Value::as_array)
                    .map(|ids| {
                        ids.iter()
                            .map(crate::render_fleet::py_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                let mut args = Map::new();
                args.insert("message_ids".into(), Value::String(ids));
                if let Some(mailbox) = text(ns, "mailbox").filter(|mailbox| !mailbox.is_empty()) {
                    args.insert("mailbox".into(), Value::String(mailbox.into()));
                }
                let rpc = format!("inbox_{command}");
                crate::daemon_cmd::run_command(ns, &rpc, args, |result| {
                    let state = result
                        .get("state")
                        .cloned()
                        .unwrap_or_else(|| Value::String(command.into()));
                    let changed = ["changed", "deleted"]
                        .into_iter()
                        .find_map(|key| {
                            result
                                .get(key)
                                .and_then(Value::as_array)
                                .filter(|values| !values.is_empty())
                        })
                        .map(|values| {
                            values
                                .iter()
                                .map(crate::render_fleet::py_str)
                                .collect::<Vec<_>>()
                                .join(", ")
                        })
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| "nothing".into());
                    let mailbox = result
                        .get("acked_in")
                        .filter(|value| crate::render_print::truthy(value))
                        .map(|value| {
                            format!(
                                " in mailbox {}",
                                crate::render_print::py_repr_str(&crate::render_fleet::py_str(
                                    value
                                ))
                            )
                        })
                        .unwrap_or_default();
                    let unchanged = result
                        .get("unchanged")
                        .filter(|value| crate::render_print::truthy(value))
                        .map(|value| format!("; unchanged {}", crate::render_fleet::py_str(value)))
                        .unwrap_or_default();
                    format!(
                        "{}: {changed}{mailbox}{unchanged}\n",
                        crate::render_fleet::py_str(&state)
                    )
                })
            }
            "count" => {
                let mut args = Map::new();
                args.insert(
                    "mailbox".into(),
                    ns.get("mailbox").cloned().unwrap_or(Value::Null),
                );
                crate::daemon_cmd::run_command(ns, "inbox_count", args, |result| {
                    crate::render_print::render_inbox_count(result)
                })
            }
            "send" => {
                let body_values = ns.get("body").and_then(Value::as_array);
                let body_words = body_values
                    .map(|words| {
                        words
                            .iter()
                            .map(crate::render_fleet::py_str)
                            .collect::<Vec<_>>()
                            .join(" ")
                    })
                    .unwrap_or_default();
                let body = if let Some(body_file) = text(ns, "body_file").filter(|p| !p.is_empty())
                {
                    if body_values.is_some_and(|words| !words.is_empty()) {
                        return Outcome {
                            stdout: String::new(),
                            stderr: "inbox send: pass a body or --body-file, not both\n".into(),
                            code: 2,
                        };
                    }
                    if body_file == "-" {
                        let mut body = String::new();
                        if let Err(error) =
                            std::io::Read::read_to_string(&mut std::io::stdin().lock(), &mut body)
                        {
                            return Outcome {
                                stdout: String::new(),
                                stderr: format!("{error}\n"),
                                code: 3,
                            };
                        }
                        body
                    } else {
                        match std::fs::read_to_string(body_file) {
                            Ok(body) => body,
                            Err(error) => {
                                return Outcome {
                                    stdout: String::new(),
                                    stderr: format!("{error}\n"),
                                    code: 3,
                                }
                            }
                        }
                    }
                } else {
                    body_words
                };
                let mut args = Map::new();
                args.insert("to".into(), ns.get("to").cloned().unwrap_or(Value::Null));
                args.insert("body".into(), Value::String(body));
                args.insert(
                    "subject".into(),
                    ns.get("subject").cloned().unwrap_or(Value::Null),
                );
                let attachments = ns
                    .get("attach")
                    .and_then(Value::as_array)
                    .map(|paths| {
                        paths
                            .iter()
                            .map(crate::render_fleet::py_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default();
                args.insert(
                    "attach".into(),
                    if attachments.is_empty() {
                        Value::Null
                    } else {
                        Value::String(attachments)
                    },
                );
                args.insert(
                    "notify".into(),
                    Value::Bool(!matches!(ns.get("no_notify"), Some(Value::Bool(true)))),
                );
                crate::daemon_cmd::run_command(ns, "inbox_send", args, |result| {
                    crate::render_print::render_inbox_sent(result)
                })
            }
            "list" => {
                let mut args = Map::new();
                args.insert(
                    "mailbox".into(),
                    ns.get("mailbox").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "unread_only".into(),
                    Value::Bool(!matches!(ns.get("all"), Some(Value::Bool(true)))),
                );
                args.insert(
                    "limit".into(),
                    ns.get("limit").cloned().unwrap_or(Value::from(50)),
                );
                crate::daemon_cmd::run_command(ns, "inbox_list", args, |result| {
                    crate::render_print::render_inbox_list(result)
                })
            }
            "read" => {
                let mut args = Map::new();
                args.insert(
                    "mailbox".into(),
                    ns.get("mailbox").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "limit".into(),
                    ns.get("limit").cloned().unwrap_or(Value::from(10)),
                );
                args.insert(
                    "peek".into(),
                    Value::Bool(matches!(ns.get("peek"), Some(Value::Bool(true)))),
                );
                if matches!(ns.get("messages_only"), Some(Value::Bool(true))) {
                    args.insert("include_tasks".into(), Value::Bool(false));
                }
                crate::daemon_cmd::run_command(ns, "inbox_read", args, |result| {
                    crate::render_print::render_inbox_read(result)
                })
            }
            "recover" => {
                let mut args = Map::new();
                args.insert(
                    "mailbox".into(),
                    ns.get("mailbox").cloned().unwrap_or(Value::Null),
                );
                crate::daemon_cmd::run_command(ns, "inbox_recover", args, |result| {
                    crate::render_print::render_inbox_recover(result)
                })
            }
            "status" => {
                let mut args = Map::new();
                args.insert(
                    "message_id".into(),
                    ns.get("message_id").cloned().unwrap_or(Value::Null),
                );
                let stamp = |at: f64| {
                    use chrono::TimeZone;
                    chrono::Local
                        .timestamp_opt(at.floor() as i64, 0)
                        .single()
                        .map(|time| time.format("%Y-%m-%d %H:%M:%S").to_string())
                        .unwrap_or_default()
                };
                crate::daemon_cmd::run_command(ns, "inbox_status", args, |result| {
                    crate::render_print::render_inbox_status(result, &stamp)
                })
            }
            "archive" => {
                let mut args = Map::new();
                args.insert(
                    "mailbox".into(),
                    ns.get("mailbox").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "before".into(),
                    ns.get("before").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "state".into(),
                    ns.get("state")
                        .cloned()
                        .unwrap_or_else(|| Value::String("queued,acked".into())),
                );
                crate::daemon_cmd::run_command(ns, "inbox_archive", args, |result| {
                    crate::render_print::render_inbox_archive(result)
                })
            }
            "stats" => crate::daemon_cmd::run_command(
                ns,
                "inbox_stats",
                Map::new(),
                crate::render_fleet::render_inbox_stats,
            ),
            "fetch" => {
                let mut args = Map::new();
                args.insert(
                    "attachment_id".into(),
                    ns.get("attachment_id").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "dest".into(),
                    ns.get("dest")
                        .cloned()
                        .unwrap_or_else(|| Value::String(".".into())),
                );
                crate::daemon_cmd::run_command(ns, "inbox_fetch", args, |result| {
                    format!(
                        "wrote {} ({} bytes)\n",
                        crate::render_fleet::py_str(&result["path"]),
                        crate::render_fleet::py_str(&result["size"])
                    )
                })
            }
            _ => not_ported("inbox"),
        },
        "task" => match text(ns, "task_command").unwrap_or("") {
            "update" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                for (argument, field) in [
                    ("title", "title"),
                    ("body", "body"),
                    ("status", "status"),
                    ("priority", "priority"),
                    ("parent", "parent_id"),
                    ("block_reason", "block_reason"),
                    ("assign", "assignees"),
                    ("depends_on", "depends_on"),
                    ("multi_assignee", "multi_assignee"),
                ] {
                    if let Some(value) = ns.get(argument).filter(|value| !value.is_null()) {
                        args.insert(field.into(), value.clone());
                    }
                }
                crate::daemon_cmd::run_command(ns, "task_update", args, |task| {
                    crate::render_print::render_task(task, &task_stamp)
                })
            }
            "assign" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                args.insert(
                    "assignees".into(),
                    ns.get("assignees").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "mode".into(),
                    ns.get("mode")
                        .cloned()
                        .unwrap_or(Value::String("add".into())),
                );
                crate::daemon_cmd::run_command(ns, "task_assign", args, |task| {
                    crate::render_print::render_task(task, &task_stamp)
                })
            }
            "complete" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                args.insert("by".into(), ns.get("by").cloned().unwrap_or(Value::Null));
                args.insert(
                    "verdict".into(),
                    ns.get("verdict").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "landed".into(),
                    ns.get("landed").cloned().unwrap_or(Value::Bool(true)),
                );
                crate::daemon_cmd::run_command(ns, "task_complete", args, |task| {
                    crate::render_print::render_task(task, &task_stamp)
                })
            }
            "add" => {
                let mut args = Map::new();
                for (key, value) in [
                    ("title", ns.get("title").cloned().unwrap_or(Value::Null)),
                    ("scope", ns.get("scope").cloned().unwrap_or(Value::Null)),
                    (
                        "body",
                        ns.get("body")
                            .cloned()
                            .unwrap_or(Value::String(String::new())),
                    ),
                    (
                        "status",
                        ns.get("status")
                            .cloned()
                            .unwrap_or(Value::String("open".into())),
                    ),
                    (
                        "priority",
                        ns.get("priority")
                            .cloned()
                            .unwrap_or(Value::String("low".into())),
                    ),
                    (
                        "assignees",
                        ns.get("assign").cloned().unwrap_or(Value::Null),
                    ),
                    (
                        "depends_on",
                        ns.get("depends_on").cloned().unwrap_or(Value::Null),
                    ),
                    (
                        "parent_id",
                        ns.get("parent").cloned().unwrap_or(Value::Null),
                    ),
                    (
                        "multi_assignee",
                        Value::Bool(matches!(ns.get("multi"), Some(Value::Bool(true)))),
                    ),
                    (
                        "kind",
                        ns.get("kind")
                            .cloned()
                            .unwrap_or(Value::String("work".into())),
                    ),
                    ("author", ns.get("author").cloned().unwrap_or(Value::Null)),
                    ("repo", ns.get("repo").cloned().unwrap_or(Value::Null)),
                    ("pr", ns.get("pr").cloned().unwrap_or(Value::Null)),
                    ("head", ns.get("head").cloned().unwrap_or(Value::Null)),
                ] {
                    args.insert(key.into(), value);
                }
                crate::daemon_cmd::run_command(ns, "task_add", args, |task| {
                    crate::render_print::render_task(task, &task_stamp)
                })
            }
            "list" | "ls" => {
                let mut args = Map::new();
                for (key, value) in [
                    ("scope", ns.get("scope").cloned().unwrap_or(Value::Null)),
                    ("status", ns.get("status").cloned().unwrap_or(Value::Null)),
                    (
                        "assignee",
                        ns.get("assignee").cloned().unwrap_or(Value::Null),
                    ),
                    (
                        "include_done",
                        Value::Bool(matches!(ns.get("all"), Some(Value::Bool(true)))),
                    ),
                    (
                        "limit",
                        ns.get("limit").cloned().unwrap_or(Value::from(200)),
                    ),
                    ("kind", ns.get("kind").cloned().unwrap_or(Value::Null)),
                    (
                        "needs_owner",
                        Value::Bool(matches!(ns.get("needs_owner"), Some(Value::Bool(true)))),
                    ),
                ] {
                    args.insert(key.into(), value);
                }
                crate::daemon_cmd::run_command(
                    ns,
                    "task_list",
                    args,
                    crate::render_print::render_task_list,
                )
            }
            "read" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                crate::daemon_cmd::run_command(ns, "task_read", args, |task| {
                    crate::render_print::render_task(task, &task_stamp)
                })
            }
            "block" | "cancel" => {
                let sub = text(ns, "task_command").unwrap_or("");
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                args.insert(
                    "reason".into(),
                    ns.get("reason").cloned().unwrap_or(Value::Null),
                );
                let method = if sub == "block" {
                    "task_block"
                } else {
                    "task_cancel"
                };
                crate::daemon_cmd::run_command(ns, method, args, |task| {
                    crate::render_print::render_task(task, &task_stamp)
                })
            }
            "unblock" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                crate::daemon_cmd::run_command(ns, "task_unblock", args, |task| {
                    crate::render_print::render_task(task, &task_stamp)
                })
            }
            "delete" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                crate::daemon_cmd::run_command(ns, "task_delete", args, |result| {
                    format!(
                        "deleted {}\n",
                        crate::render_fleet::py_str(&result["deleted"])
                    )
                })
            }
            "note" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                args.insert(
                    "event".into(),
                    ns.get("event")
                        .cloned()
                        .unwrap_or(Value::String("note".into())),
                );
                args.insert(
                    "reason".into(),
                    ns.get("reason").cloned().unwrap_or(Value::Null),
                );
                crate::daemon_cmd::run_command(ns, "task_note", args, |result| {
                    format!(
                        "noted {}: {}\n",
                        crate::render_fleet::py_str(&result["noted"]),
                        crate::render_fleet::py_str(&result["event"])
                    )
                })
            }
            "next" => {
                let mut args = Map::new();
                args.insert(
                    "scope".into(),
                    ns.get("scope").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "lease_seconds".into(),
                    ns.get("lease").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "allow_busy".into(),
                    Value::Bool(matches!(ns.get("allow_busy"), Some(Value::Bool(true)))),
                );
                crate::daemon_cmd::run_command(ns, "task_next", args, |result| {
                    crate::render_print::render_task_next(result, &task_stamp)
                })
            }
            "heartbeat" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                args.insert(
                    "lease_seconds".into(),
                    ns.get("lease").cloned().unwrap_or(Value::Null),
                );
                crate::daemon_cmd::run_command(ns, "task_heartbeat", args, |result| {
                    let seconds = crate::render_print::task_lease_seconds(&result["lease_seconds"]);
                    format!(
                        "lease on {} extended ({}s)\n",
                        crate::render_fleet::py_str(&result["task_id"]),
                        seconds
                    )
                })
            }
            "release" => {
                let mut args = Map::new();
                args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
                crate::daemon_cmd::run_command(ns, "task_release", args, |task| {
                    crate::render_print::render_task(task, &task_stamp)
                })
            }
            "follow" => {
                let mut args = Map::new();
                args.insert(
                    "scope".into(),
                    ns.get("scope").cloned().unwrap_or(Value::Null),
                );
                crate::daemon_cmd::run_command(ns, "task_follow", args, |result| {
                    let followers = result["followers"].as_array().map_or(0, Vec::len);
                    format!(
                        "following {} as {} ({} follower(s))\n",
                        crate::render_fleet::py_str(&result["scope"]),
                        crate::render_fleet::py_str(&result["pane_id"]),
                        followers
                    )
                })
            }
            "unfollow" => {
                crate::daemon_cmd::run_command(ns, "task_unfollow", Map::new(), |result| {
                    if matches!(result.get("was_following"), Some(Value::Bool(true))) {
                        "stopped following\n".into()
                    } else {
                        "was not following\n".into()
                    }
                })
            }
            "followers" => crate::daemon_cmd::run_command(
                ns,
                "task_followers",
                Map::new(),
                crate::render_print::render_task_followers,
            ),
            "stats" => {
                let mut args = Map::new();
                args.insert(
                    "scope".into(),
                    ns.get("scope").cloned().unwrap_or(Value::Null),
                );
                crate::daemon_cmd::run_command(ns, "task_stats", args, |result| {
                    crate::render_print::render_task_stats(result)
                })
            }
            "metrics" => {
                let mut args = Map::new();
                args.insert(
                    "scope".into(),
                    ns.get("scope").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "window_s".into(),
                    ns.get("window").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "since".into(),
                    ns.get("since").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "until".into(),
                    ns.get("until").cloned().unwrap_or(Value::Null),
                );
                args.insert(
                    "bucket".into(),
                    ns.get("bucket")
                        .cloned()
                        .unwrap_or(Value::String("hour".into())),
                );
                crate::daemon_cmd::run_command(ns, "task_metrics", args, |result| {
                    crate::render_print::render_task_metrics(result, &task_stamp)
                })
            }
            "reap" => crate::daemon_cmd::run_command(ns, "task_reap", Map::new(), |result| {
                let requeued = result["requeued"].as_array().cloned().unwrap_or_default();
                let ids = requeued
                    .iter()
                    .map(|task| crate::render_fleet::py_str(&task["task_id"]))
                    .collect::<Vec<_>>()
                    .join(", ");
                let suffix = if ids.is_empty() {
                    String::new()
                } else {
                    format!(": {ids}")
                };
                format!("requeued {} lease(s){suffix}\n", requeued.len())
            }),
            _ => not_ported("task"),
        },
        "stop" | "toggle" => {
            let mut args = Map::new();
            args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
            let rpc = if command == "stop" {
                "stop_dunk"
            } else {
                "toggle_dunk"
            };
            crate::daemon_cmd::run_command(ns, rpc, args, |result| {
                crate::render_print::render_dunk(result)
            })
        }
        "stop-all" => crate::daemon_cmd::run_command(ns, "stop_all", Map::new(), |result| {
            crate::render_print::render_dunks(result["dunks"].as_array().map_or(&[], Vec::as_slice))
        }),
        "whoami" => crate::daemon_cmd::whoami(ns),
        "update" | "set" => {
            let mut args = Map::new();
            args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
            if let Some(target) = text(ns, "target").filter(|value| !value.is_empty()) {
                args.insert("target".into(), Value::String(target.to_string()));
            }
            for (source, destination) in [
                ("text", "text"),
                ("every", "interval_minutes"),
                ("name", "name"),
            ] {
                if let Some(value) = ns.get(source).filter(|value| !value.is_null()) {
                    args.insert(destination.into(), value.clone());
                }
            }
            if matches!(ns.get("only_idle"), Some(Value::Bool(true))) {
                args.insert("only_when".into(), Value::String("idle".into()));
            } else if matches!(ns.get("any_time"), Some(Value::Bool(true))) {
                args.insert("only_when".into(), Value::Null);
            }
            if let Some(max_sends) = ns.get("max_sends").filter(|value| !value.is_null()) {
                args.insert(
                    "max_sends".into(),
                    if max_sends.as_i64() == Some(0) {
                        Value::Null
                    } else {
                        max_sends.clone()
                    },
                );
            }
            if matches!(ns.get("skip_if_unconsumed"), Some(Value::Bool(true))) {
                args.insert("skip_if_unconsumed".into(), Value::Bool(true));
            } else if matches!(ns.get("always_send"), Some(Value::Bool(true))) {
                args.insert("skip_if_unconsumed".into(), Value::Bool(false));
            }
            if matches!(ns.get("hold_while_typing"), Some(Value::Bool(true))) {
                args.insert("hold_while_typing".into(), Value::Bool(true));
            } else if matches!(ns.get("ignore_typing"), Some(Value::Bool(true))) {
                args.insert("hold_while_typing".into(), Value::Bool(false));
            }
            crate::daemon_cmd::run_command(ns, "update_dunk", args, |result| {
                crate::render_print::render_dunk(result)
            })
        }
        "remove" | "rm" | "delete" => {
            let mut args = Map::new();
            args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
            crate::daemon_cmd::run_command(ns, "remove_dunk", args, |result| {
                format!("removed {}\n", crate::render_fleet::py_str(&result["id"]))
            })
        }
        "start" => {
            let mut args = Map::new();
            args.insert("id".into(), ns.get("id").cloned().unwrap_or(Value::Null));
            crate::daemon_cmd::run_command(ns, "start_dunk", args, |result| {
                crate::render_print::render_dunk(result)
            })
        }
        "turn" => match text(ns, "turn_command") {
            Some("start") => {
                let mut args = Map::new();
                for key in ["pane", "ttl_s", "source", "handoff_id"] {
                    args.insert(key.to_string(), ns.get(key).cloned().unwrap_or(Value::Null));
                }
                crate::daemon_cmd::run_command(ns, "turn_start", args, |result| {
                    format!(
                        "turn start {} ttl {}s\n",
                        crate::render_fleet::py_str(&result["pane_id"]),
                        crate::render_fleet::py_str(&result["ttl_s"])
                    )
                })
            }
            Some("end") => {
                let mut args = Map::new();
                for key in ["pane", "reason", "turn_id"] {
                    args.insert(key.to_string(), ns.get(key).cloned().unwrap_or(Value::Null));
                }
                crate::daemon_cmd::run_command(
                    ns,
                    "turn_end",
                    args,
                    crate::render_fleet::render_turn_end,
                )
            }
            Some(other) => not_ported(&format!("turn {other}")),
            None => not_ported("turn"),
        },
        "poll" => {
            let mut args = Map::new();
            for (key, value) in [
                ("limit", ns.get("limit").cloned().unwrap_or(Value::from(5))),
                (
                    "panes",
                    Value::Bool(matches!(ns.get("panes"), Some(Value::Bool(true)))),
                ),
                (
                    "tasks",
                    Value::Bool(matches!(ns.get("tasks"), Some(Value::Bool(true)))),
                ),
                (
                    "include_done",
                    Value::Bool(matches!(ns.get("include_done"), Some(Value::Bool(true)))),
                ),
                (
                    "check_herdr",
                    Value::Bool(matches!(ns.get("check_herdr"), Some(Value::Bool(true)))),
                ),
            ] {
                args.insert(key.to_string(), value);
            }
            if let Some(mailbox) = text(ns, "mailbox").filter(|value| !value.is_empty()) {
                args.insert("mailbox".into(), Value::String(mailbox.to_string()));
            }
            if let Some(task_scope) = text(ns, "task_scope").filter(|value| !value.is_empty()) {
                args.insert("task_scope".into(), Value::String(task_scope.to_string()));
            }
            crate::daemon_cmd::run_command(ns, "poll", args, crate::render_fleet::render_poll)
        }
        // `client.call("status", check_herdr=args.check_herdr), print_status`
        "status" => {
            let mut args = Map::new();
            args.insert(
                "check_herdr".into(),
                Value::Bool(matches!(ns.get("check_herdr"), Some(Value::Bool(true)))),
            );
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            // `time.strftime(..., time.localtime(at))`: the host's zone, via chrono's Local.
            let stamp = |at: f64| {
                use chrono::TimeZone;
                chrono::Local
                    .timestamp_opt(at.floor() as i64, 0)
                    .single()
                    .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
                    .unwrap_or_default()
            };
            crate::daemon_cmd::run_command(ns, "status", args, |result| {
                crate::render_print::render_status(result, &stamp, now)
            })
        }
        // `client.call("doctor"), print_doctor`
        "doctor" => crate::daemon_cmd::run_command(
            ns,
            "doctor",
            Map::new(),
            crate::render_print::render_doctor,
        ),
        "git-credential" => {
            let stdin = std::io::stdin();
            crate::daemon_cmd::git_credential(ns, &mut stdin.lock())
        }
        other => not_ported(other),
    }
}

#[cfg(test)]
mod guard_timeout_tests {
    use super::command_output_timeout;
    use std::process::Command;
    use std::time::Duration;

    #[test]
    fn guard_check_child_obeys_timeout() {
        let mut command = Command::new("sleep");
        command
            .arg("5")
            .stdout(super::Stdio::piped())
            .stderr(super::Stdio::piped());
        let error = command_output_timeout(&mut command, Duration::from_millis(30)).unwrap_err();
        assert!(error.contains("timed out"), "{error}");
    }

    #[test]
    fn guard_check_drains_large_child_output() {
        let mut command = Command::new("python3");
        command
            .args([
                "-c",
                "import sys; sys.stdout.write('o' * 1048576); sys.stderr.write('e' * 1048576)",
            ])
            .stdout(super::Stdio::piped())
            .stderr(super::Stdio::piped());
        let output = command_output_timeout(&mut command, Duration::from_secs(10)).unwrap();
        assert_eq!(output.stdout.len(), 1_048_576);
        assert_eq!(output.stderr.len(), 1_048_576);
        assert!(output.stdout.iter().all(|byte| *byte == b'o'));
        assert!(output.stderr.iter().all(|byte| *byte == b'e'));
    }
}
