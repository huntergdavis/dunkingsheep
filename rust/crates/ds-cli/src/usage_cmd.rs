//! `dunkingsheep usage`: real provider usage from configurable commands (t1285).
//!
//! Ported from the `usage` branch of `run_client_command` in the `dunkingsheep` script:
//! load the config, run the providers, print the operator table (or `--json`), exit 1 when any
//! provider ERRORED, 2 for a usage error (bad config, unknown provider). `unknown` is not an
//! error: unknown stays unknown and exits 0.
//!
//! The runner, the config loader and the table renderer are `ds-core::usage`; this is only the
//! command body.

use ds_core::usage;
use serde_json::{Map, Value};

/// What the command printed and how it exits.
#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

/// `usage [provider] --config PATH [--max-age S]`, `json` being the global `--json` flag.
pub fn usage_command(
    config_path: &str,
    provider: Option<&str>,
    max_age: Option<f64>,
    json: bool,
) -> Outcome {
    let results =
        usage::load_config(config_path).and_then(|config| usage::run(&config, provider, max_age));
    let results = match results {
        Ok(results) => results,
        Err(error) => {
            return Outcome {
                stdout: String::new(),
                stderr: format!("usage: {error}\n"),
                code: 2,
            }
        }
    };
    let text = if json {
        sorted_json(&Value::Array(results.clone()))
    } else {
        usage::render_text(&results)
    };
    let errored = results
        .iter()
        .any(|item| item.get("status").and_then(Value::as_str) == Some("error"));
    Outcome {
        stdout: format!("{text}\n"),
        stderr: String::new(),
        code: i32::from(errored),
    }
}

/// `json.dumps(value, indent=2, sort_keys=True)`: keys sorted at every level (the runner's maps
/// may hold insertion order), two-space indent, non-ASCII escaped as `\uXXXX`, floats the way
/// Python prints them (`1e-07`, not `1e-7`).
pub fn sorted_json(value: &Value) -> String {
    let mut out = String::new();
    write_value(&mut out, value, 0);
    out
}

fn write_value(out: &mut String, value: &Value, depth: usize) {
    match value {
        Value::Array(items) if items.is_empty() => out.push_str("[]"),
        Value::Object(map) if map.is_empty() => out.push_str("{}"),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                line(out, i, depth + 1);
                write_value(out, item, depth + 1);
            }
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.iter().enumerate() {
                line(out, i, depth + 1);
                out.push_str(&usage::dumps_sorted(&Value::String((*key).clone())));
                out.push_str(": ");
                write_value(out, &map[*key], depth + 1);
            }
            out.push('\n');
            out.push_str(&"  ".repeat(depth));
            out.push('}');
        }
        // Scalars print exactly as the single-line dumper prints them.
        other => out.push_str(&usage::dumps_sorted(other)),
    }
}

fn line(out: &mut String, index: usize, depth: usize) {
    if index > 0 {
        out.push(',');
    }
    out.push('\n');
    out.push_str(&"  ".repeat(depth));
}
