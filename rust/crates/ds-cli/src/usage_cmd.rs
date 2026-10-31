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

/// `json.dumps(value, indent=2, sort_keys=True)`: keys sorted at every level (a map built in
/// sorted order, so it holds whether or not serde_json keeps insertion order), two-space
/// indent, non-ASCII escaped as `\uXXXX`.
pub fn sorted_json(value: &Value) -> String {
    let pretty = serde_json::to_string_pretty(&sorted(value)).unwrap_or_default();
    let mut out = String::with_capacity(pretty.len());
    for character in pretty.chars() {
        if character.is_ascii() {
            out.push(character);
        } else {
            let mut units = [0u16; 2];
            for unit in character.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

fn sorted(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = Map::new();
            for key in keys {
                out.insert(key.clone(), sorted(&map[key]));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
        other => other.clone(),
    }
}
