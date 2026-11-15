//! A lane's effective launch settings, each with where it came from (t5249 slice 1).
//!
//! Read-only: everything here is derived from the lane's launch record, which keeps the argv the
//! lane was started with. Nothing is changed and nothing is launched. Later slices add the owner's
//! project templates and parent inheritance; their values will carry those sources.
//!
//! Fields: harness, daemon (Codex's shared app-server), sandbox, approval, permission_bypass,
//! model, effort and tmpdir. A field the command does not set says so: the provider's own default
//! applies, and DS does not know what that is.

use serde_json::{json, Map, Value};

use super::launched_program_index;

/// Where a value came from when it was read off the command line.
pub const FROM_ARGV: &str = "launch argv";
/// The command does not set it; the provider decides.
pub const PROVIDER_DEFAULT: &str = "not on the launch command (the provider's default applies)";

fn cell(value: Value, source: &str) -> Value {
    json!({"value": value, "source": source})
}

/// The value after `flag` (`-s x`, `--sandbox x`) or in `flag=value`, searching `names`.
fn flag_value(argv: &[String], names: &[&str]) -> Option<String> {
    let mut tokens = argv.iter();
    while let Some(token) = tokens.next() {
        for name in names {
            if token == name {
                return tokens.next().cloned();
            }
            if let Some(value) = token.strip_prefix(&format!("{name}=")) {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn has(argv: &[String], flag: &str) -> bool {
    argv.iter().any(|token| token == flag)
}

/// `-c model_reasoning_effort=<value>` (or `--config`).
fn codex_effort(argv: &[String]) -> Option<String> {
    let mut tokens = argv.iter();
    while let Some(token) = tokens.next() {
        let setting = if token == "-c" || token == "--config" {
            tokens.next().map(String::as_str)
        } else {
            token.strip_prefix("--config=")
        };
        if let Some(effort) = setting.and_then(|s| s.strip_prefix("model_reasoning_effort=")) {
            return Some(effort.trim_matches('"').to_string());
        }
    }
    None
}

/// The settings a launch argv states, by provider. `None` for a program DS does not know.
pub fn profile_from_argv(argv: &[String]) -> Option<(String, Map<String, Value>)> {
    let start = launched_program_index(argv)?;
    let program = argv[start]
        .rsplit('/')
        .next()
        .unwrap_or(&argv[start])
        .to_lowercase();
    let args = &argv[start + 1..];
    let stated = |value: Option<String>| match value {
        Some(value) => cell(json!(value), FROM_ARGV),
        None => cell(Value::Null, PROVIDER_DEFAULT),
    };
    let flagged = |on: bool| {
        if on {
            cell(json!(true), FROM_ARGV)
        } else {
            cell(json!(false), PROVIDER_DEFAULT)
        }
    };
    let not_applicable = || cell(Value::Null, "not a setting of this provider");
    let mut fields = Map::new();
    match program.as_str() {
        "codex" => {
            fields.insert("daemon".into(), {
                if has(args, "--no-daemon") {
                    cell(json!("off (--no-daemon)"), FROM_ARGV)
                } else {
                    cell(json!("shared app-server"), PROVIDER_DEFAULT)
                }
            });
            fields.insert(
                "sandbox".into(),
                stated(flag_value(args, &["-s", "--sandbox"])),
            );
            fields.insert(
                "approval".into(),
                stated(flag_value(args, &["-a", "--ask-for-approval"])),
            );
            fields.insert(
                "permission_bypass".into(),
                flagged(has(args, "--dangerously-bypass-approvals-and-sandbox")),
            );
            fields.insert("model".into(), stated(flag_value(args, &["-m", "--model"])));
            fields.insert("effort".into(), stated(codex_effort(args)));
        }
        "claude" => {
            fields.insert("daemon".into(), not_applicable());
            fields.insert("sandbox".into(), not_applicable());
            fields.insert(
                "approval".into(),
                stated(flag_value(args, &["--permission-mode"])),
            );
            fields.insert(
                "permission_bypass".into(),
                flagged(has(args, "--dangerously-skip-permissions")),
            );
            fields.insert("model".into(), stated(flag_value(args, &["--model"])));
            fields.insert("effort".into(), stated(flag_value(args, &["--effort"])));
        }
        "pi" | "opencode" => {
            for field in [
                "daemon",
                "sandbox",
                "approval",
                "permission_bypass",
                "effort",
            ] {
                fields.insert(field.into(), not_applicable());
            }
            fields.insert("model".into(), stated(flag_value(args, &["--model", "-m"])));
        }
        _ => return None,
    }
    Some((program, fields))
}

/// `lane settings`: the effective settings of the lane in `pane`, from its launch record.
pub fn lane_settings(pane: &str, record: Option<&Value>) -> Value {
    let Some(record) = record else {
        return json!({
            "pane_id": pane,
            "known": false,
            "reason": "no launch record for this pane: it was not started through DS",
            "settings": {},
        });
    };
    let argv: Vec<String> = record
        .get("argv")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|t| t.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let Some((program, mut fields)) = profile_from_argv(&argv) else {
        return json!({
            "pane_id": pane,
            "known": false,
            "reason": "the launch command is not a known agent (claude, codex, pi or opencode)",
            "launched_by": record.get("source").cloned().unwrap_or(Value::Null),
            "settings": {},
        });
    };
    fields.insert("harness".into(), cell(json!(program), FROM_ARGV));
    let tmpdir = match (
        record.get("tmpdir").and_then(Value::as_str),
        record.get("tmpdir_source").and_then(Value::as_str),
    ) {
        (Some(dir), Some(source)) => cell(json!(dir), &format!("launch record ({source})")),
        _ => cell(
            Value::Null,
            "not recorded (lane started before t3912; /tmp unless its shell set TMPDIR)",
        ),
    };
    fields.insert("tmpdir".into(), tmpdir);
    json!({
        "pane_id": pane,
        "known": true,
        "launched_by": record.get("source").cloned().unwrap_or(Value::Null),
        "recorded_at": record.get("recorded_at").cloned().unwrap_or(Value::Null),
        "derived": "read-only, from the launch record's argv (pre-t5249 records carry no settings of their own)",
        "settings": fields,
    })
}

/// One line per field, for the CLI.
pub fn render_text(result: &Value) -> String {
    let pane = result["pane_id"].as_str().unwrap_or("?");
    if result["known"] != true {
        return format!(
            "{pane}: {}\n",
            result["reason"].as_str().unwrap_or("unknown")
        );
    }
    let mut out = format!(
        "{pane}: launch settings (launched by {})\n",
        result["launched_by"].as_str().unwrap_or("unknown")
    );
    for field in [
        "harness",
        "model",
        "effort",
        "daemon",
        "sandbox",
        "approval",
        "permission_bypass",
        "tmpdir",
    ] {
        let cell = &result["settings"][field];
        if cell.is_null() {
            continue;
        }
        let value = match &cell["value"] {
            Value::Null => "-".to_string(),
            Value::String(text) => text.clone(),
            other => other.to_string(),
        };
        out.push_str(&format!(
            "  {field:<18} {value:<28} {}\n",
            cell["source"].as_str().unwrap_or("")
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(tokens: &[&str]) -> Vec<String> {
        tokens.iter().map(|t| t.to_string()).collect()
    }

    /// The incident's two shapes (t5249): a Codex worker without sandbox/approval flags, and
    /// its replacement with the fleet's. Values are placeholders, not copied from a record.
    #[test]
    fn the_incident_shapes_show_the_missing_sandbox_and_approval() {
        let (program, without) = profile_from_argv(&v(&[
            "codex",
            "-m",
            "MODEL",
            "-c",
            "model_reasoning_effort=EFFORT",
            "--no-daemon",
        ]))
        .unwrap();
        assert_eq!(program, "codex");
        assert_eq!(without["sandbox"], cell(Value::Null, PROVIDER_DEFAULT));
        assert_eq!(without["approval"], cell(Value::Null, PROVIDER_DEFAULT));
        assert_eq!(without["daemon"]["value"], "off (--no-daemon)");
        assert_eq!(without["model"], cell(json!("MODEL"), FROM_ARGV));
        assert_eq!(without["effort"], cell(json!("EFFORT"), FROM_ARGV));
        let (_, with) = profile_from_argv(&v(&[
            "codex",
            "--no-daemon",
            "-m",
            "MODEL",
            "-c",
            "model_reasoning_effort=EFFORT",
            "-s",
            "danger-full-access",
            "-a",
            "never",
        ]))
        .unwrap();
        assert_eq!(
            with["sandbox"],
            cell(json!("danger-full-access"), FROM_ARGV)
        );
        assert_eq!(with["approval"], cell(json!("never"), FROM_ARGV));
        assert_eq!(with["permission_bypass"]["value"], false);
    }

    #[test]
    fn other_shapes_env_prefix_long_flags_claude_pi_and_unknown_programs() {
        let (program, codex) = profile_from_argv(&v(&[
            "env",
            "TMPDIR=/s/x",
            "codex",
            "--sandbox=workspace-write",
            "--ask-for-approval",
            "on-request",
            "--dangerously-bypass-approvals-and-sandbox",
            "--model",
            "M",
        ]))
        .unwrap();
        assert_eq!(program, "codex");
        assert_eq!(codex["sandbox"]["value"], "workspace-write");
        assert_eq!(codex["approval"]["value"], "on-request");
        assert_eq!(codex["permission_bypass"], cell(json!(true), FROM_ARGV));
        assert_eq!(codex["daemon"]["value"], "shared app-server");
        let (_, claude) = profile_from_argv(&v(&[
            "claude",
            "--model",
            "M",
            "--effort",
            "low",
            "--dangerously-skip-permissions",
        ]))
        .unwrap();
        assert_eq!(claude["permission_bypass"]["value"], true);
        assert_eq!(claude["effort"]["value"], "low");
        assert_eq!(
            claude["sandbox"]["source"],
            "not a setting of this provider"
        );
        let (_, pi) = profile_from_argv(&v(&["pi", "--model", "M"])).unwrap();
        assert_eq!(pi["model"]["value"], "M");
        assert!(profile_from_argv(&v(&["bash", "-lc", "codex"])).is_none());
        assert!(profile_from_argv(&v(&["env", "A=b"])).is_none());
    }

    #[test]
    fn lane_settings_reads_the_record_and_names_every_source() {
        let record = json!({
            "argv": ["env", "TMPDIR=/w/.dunk-scratch/a-1f2e3d", "codex", "--no-daemon", "-m", "M"],
            "source": "start_agent", "recorded_at": 1.0,
            "tmpdir": "/w/.dunk-scratch/a-1f2e3d", "tmpdir_source": "start-agent default",
        });
        let out = lane_settings("%9", Some(&record));
        assert_eq!(out["known"], true);
        assert_eq!(out["settings"]["harness"]["value"], "codex");
        assert_eq!(
            out["settings"]["tmpdir"]["source"],
            "launch record (start-agent default)"
        );
        assert_eq!(out["settings"]["sandbox"]["source"], PROVIDER_DEFAULT);
        let text = render_text(&out);
        assert!(
            text.contains("sandbox") && text.contains("provider's default"),
            "{text}"
        );
        // An older record: no tmpdir recorded.
        let old = lane_settings(
            "%8",
            Some(&json!({"argv": ["claude", "--model", "M"], "source": "respawn_pane"})),
        );
        assert!(old["settings"]["tmpdir"]["source"]
            .as_str()
            .unwrap()
            .starts_with("not recorded"));
        // No record, or an unknown program: said plainly, nothing guessed.
        assert_eq!(lane_settings("%7", None)["known"], false);
        let unknown = lane_settings("%6", Some(&json!({"argv": ["bash"]})));
        assert_eq!(unknown["known"], false);
        assert!(render_text(&unknown).contains("not a known agent"));
    }
}
