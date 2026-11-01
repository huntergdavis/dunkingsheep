//! T44, slice 6: run a parsed command. `main.rs` used to stop at "parsed but not ported yet";
//! this runs the commands whose bodies exist in the CLI crate and says so honestly for the
//! rest. Wired today: `context` (the READ subcommands, `context_cmd`), `usage`
//! (`usage_cmd`) and `secrets-check` (`secrets_cmd`), the daemon-backed `gh-token`, `git-credential` and `doctor`
//! (`daemon_cmd`), and `github wrappers` (`github_cmd`). Everything else, including the `context` write subcommands, keeps the
//! "parsed but not ported yet" answer (exit 1).
//!
//! `--json` for `context`: the Python prints `json.dumps(result, indent=2)` (insertion order);
//! serde_json's maps are key-sorted, so this prints the same document with SORTED keys
//! (`usage_cmd::sorted_json`). Valid, equal as JSON, not byte-identical: held with the other
//! json.dumps printers pending the owner's `preserve_order` decision.

use serde_json::{Map, Value};

use crate::context_cmd::context_read;
use crate::usage_cmd::{sorted_json, usage_command, Outcome};

fn text<'a>(ns: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    ns.get(key).and_then(Value::as_str)
}

fn not_ported(command: &str) -> Outcome {
    Outcome {
        stdout: String::new(),
        stderr: format!("dunkingsheep (rust port): `{command}` is parsed but not ported yet\n"),
        code: 1,
    }
}

fn context(ns: &Map<String, Value>, json: bool) -> Outcome {
    let sub = text(ns, "context_command").unwrap_or("");
    let now = chrono::Local::now().naive_local();
    match context_read(sub, ns, None, now) {
        Ok(out) => Outcome {
            stdout: if json {
                format!("{}\n", sorted_json(&out.value))
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

/// `github wrappers [--bin-dir DIR]` (the owner's one-shot setup). `github protect` needs
/// `dunk_github` (unported) and keeps the not-ported answer.
fn github(ns: &Map<String, Value>) -> Outcome {
    match text(ns, "github_command") {
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

/// Run one parsed command (`ns` is the argparse namespace as a dictionary).
pub fn run(ns: &Map<String, Value>) -> Outcome {
    let command = text(ns, "command").unwrap_or("");
    let json = matches!(ns.get("json"), Some(Value::Bool(true)));
    match command {
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
        "github" => github(ns),
        "gh-token" => crate::daemon_cmd::gh_token(ns),
        // `client.call("list_panes"), print_panes`
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
        "whoami" => crate::daemon_cmd::whoami(ns),
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
