//! T44, slice 6: run a parsed command. `main.rs` used to stop at "parsed but not ported yet";
//! this runs the commands whose bodies exist in the CLI crate and says so honestly for the
//! rest. Wired today: `context` (the READ subcommands, `context_cmd`) and `usage`
//! (`usage_cmd`). Everything else, including the `context` write subcommands, keeps the
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

/// Run one parsed command (`ns` is the argparse namespace as a dictionary).
pub fn run(ns: &Map<String, Value>) -> Outcome {
    let command = text(ns, "command").unwrap_or("");
    let json = matches!(ns.get("json"), Some(Value::Bool(true)));
    match command {
        "context" => context(ns, json),
        "usage" => {
            let config = text(ns, "config").unwrap_or("");
            usage_command(
                config,
                text(ns, "provider"),
                ns.get("max_age").and_then(Value::as_f64),
                json,
            )
        }
        other => not_ported(other),
    }
}
