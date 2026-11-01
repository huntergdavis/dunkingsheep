//! T44 differential: `context_read` over a small store prints what the real Python
//! `dunkingsheep context ...` printed. `tools/gen_context_cmd_fixture.py` builds the store
//! with the Python, runs each step IN ORDER (reads log access, digest records injections, so
//! the order is part of the evidence) and records stdout / exit code; this test recreates the
//! files and replays the steps. Only the human (non --json) form is compared.

use std::path::Path;

use ds_cli::context_cmd::context_read;
use serde_json::{json, Map, Value};

const FIXTURE: &str = include_str!("context_cmd.json");

/// argv after `context` -> (subcommand, argparse namespace).
fn namespace(argv: &[String]) -> (String, Map<String, Value>) {
    let sub = argv[0].clone();
    let mut ns = Map::new();
    let mut positional = 0;
    let mut i = 1;
    while i < argv.len() {
        let arg = &argv[i];
        match arg.as_str() {
            "--global" => {
                ns.insert("global_store".into(), json!(true));
            }
            "--project" => {
                ns.insert("project".into(), json!(true));
            }
            "--class" | "--scope" | "--subject" | "--path" => {
                i += 1;
                let key = if arg == "--class" {
                    "cls"
                } else {
                    arg.trim_start_matches("--")
                };
                ns.insert(key.into(), json!(argv[i]));
            }
            "--max-bytes" => {
                i += 1;
                ns.insert("max_bytes".into(), json!(argv[i].parse::<i64>().unwrap()));
            }
            _ => {
                let key = match (sub.as_str(), positional) {
                    ("get", 0) => "key",
                    ("search", 0) => "query",
                    ("ledger", 0) => "action",
                    other => panic!("unmapped positional {other:?}"),
                };
                ns.insert(key.into(), json!(arg));
                positional += 1;
            }
        }
        i += 1;
    }
    (sub, ns)
}

#[test]
fn every_recorded_context_step_prints_what_the_python_printed() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture");
    let base = tempfile::tempdir().expect("tempdir");
    for (relative, text) in fixture["files"].as_object().unwrap() {
        let path = base.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text.as_str().unwrap()).unwrap();
    }
    // The Python ran with cwd = <T>/proj and DUNKINGSHEEP_DIR = <T>/cfg; the project store
    // is `./.dunkingsheep`, the global one `$DUNKINGSHEEP_DIR/context`. One test only: the
    // working directory and the environment are process-wide.
    std::env::set_var("DUNKINGSHEEP_DIR", base.path().join("cfg"));
    std::env::set_var("DUNKINGSHEEP_SELF", "fixture");
    std::env::set_current_dir(base.path().join("proj")).unwrap();
    let t = base.path().display().to_string();
    let now = chrono::Local::now().naive_local();
    let mut bad = Vec::new();
    let steps = fixture["steps"].as_array().unwrap();
    assert!(steps.len() >= 12, "fixture too small");
    for step in steps {
        let argv: Vec<String> = step["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        let (sub, ns) = namespace(&argv);
        let want_code = step["code"].as_i64().unwrap();
        let want_out = step["stdout"].as_str().unwrap();
        let want_err = step["stderr"].as_str().unwrap();
        match context_read(&sub, &ns, None::<&Path>, now) {
            Ok(out) => {
                let got = out.text.replace(&t, "<T>");
                if want_code != 0 || got != want_out {
                    bad.push(format!(
                        "{argv:?}\n  got:  {got:?}\n  want: {want_out:?} (code {want_code})"
                    ));
                }
            }
            Err(error) => {
                let got = error.to_string().replace(&t, "<T>");
                // Python's SystemExit("message") prints the message to stderr, exit 1.
                if want_code == 0 || !want_err.contains(got.trim()) {
                    bad.push(format!("{argv:?}\n  got error: {got:?}\n  want: code {want_code} stderr {want_err:?}"));
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} steps differ:\n{}",
        bad.len(),
        steps.len(),
        bad.join("\n")
    );
}
