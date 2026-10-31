//! t1285: `dunkingsheep usage`, checked against the Python command's real output on the same
//! fake providers (captured 2026-10-31 with `python3 dunkingsheep usage --config <cfg>`).

use std::fs;

use ds_cli::usage_cmd::{sorted_json, usage_command};
use serde_json::json;

struct Setup {
    dir: tempfile::TempDir,
}

impl Setup {
    fn config(&self) -> String {
        self.dir.path().join("usage.json").display().to_string()
    }
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    let scripts = [
        (
            "ok.sh",
            r#"echo '{"provider":"a","status":"ok","meaning":"percent_used","measured_at":1,"windows":[{"name":"weekly","percent_used":15.0,"percent_remaining":85.0,"resets_at":"2026-11-05T09:00:00+00:00"}]}'"#,
        ),
        (
            "unk.sh",
            r#"echo '{"provider":"b","status":"unknown","meaning":"percent_used","measured_at":1,"reason":"no live endpoint café","last_snapshot":{"percent_used":100.0}}'; exit 1"#,
        ),
        ("err.sh", "printf boom >&2; exit 3"),
    ];
    let mut providers = serde_json::Map::new();
    for ((file, body), name) in scripts.iter().zip(["a", "b", "c"]) {
        let path = dir.path().join(file);
        fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        providers.insert(
            name.to_string(),
            json!({"command": format!("/bin/sh {}", path.display()), "timeout_s": 5,
                   "parse": {"windows": "windows", "meaning": "meaning",
                             "measured_at": "measured_at"}}),
        );
    }
    fs::write(
        dir.path().join("usage.json"),
        json!({"schema": 1, "providers": providers}).to_string(),
    )
    .unwrap();
    Setup { dir }
}

const HEADER: &str = "PROVIDER   STATUS   MEANING                READING\n";
const ROW_A: &str = "a          ok       percent_used           weekly: 15.0% used / 85.0% left (resets 2026-11-05T09:00:00+00:00)\n";
const ROW_B: &str = "b          unknown  percent_used           UNKNOWN: no live endpoint café | last snapshot 100.0% used\n";
const ROW_C: &str = "c          error    unknown                UNKNOWN: command exited 3 without canonical JSON | stderr: boom\n";

#[test]
fn every_provider_prints_a_row_and_an_error_exits_1() {
    let s = setup();
    let out = usage_command(&s.config(), None, None, false);
    assert_eq!(format!("{HEADER}{ROW_A}{ROW_B}{ROW_C}"), out.stdout);
    assert_eq!("", out.stderr);
    assert_eq!(1, out.code, "an errored provider exits 1");
}

#[test]
fn one_provider_and_max_age() {
    let s = setup();
    let out = usage_command(&s.config(), Some("a"), None, false);
    assert_eq!(format!("{HEADER}{ROW_A}"), out.stdout);
    assert_eq!(0, out.code);
    let aged = usage_command(&s.config(), None, Some(7.0), false);
    assert_eq!(format!("{HEADER}{ROW_A}{ROW_B}{ROW_C}"), aged.stdout);
}

#[test]
fn unknown_is_not_an_error_and_json_is_sorted_and_ascii() {
    let s = setup();
    let out = usage_command(&s.config(), Some("b"), None, true);
    assert_eq!(0, out.code, "unknown stays unknown and exits 0");
    // The Python's `json.dumps(..., indent=2, sort_keys=True)` for the same reading, minus the
    // runner's own fields, which the core tests pin: check the parts this command owns.
    assert!(out.stdout.starts_with("[\n  {\n    \"exit_code\": 1,\n    \"last_snapshot\": {\n      \"percent_used\": 100.0\n    },\n    \"meaning\": \"percent_used\",\n"));
    assert!(out
        .stdout
        .contains("\"reason\": \"no live endpoint caf\\u00e9\""));
    assert!(out.stdout.ends_with("]\n"));
}

#[test]
fn usage_errors_exit_2_with_the_pythons_text() {
    let s = setup();
    let unknown = usage_command(&s.config(), Some("zz"), None, false);
    assert_eq!(
        "usage: unknown provider 'zz'; known: a, b, c\n",
        unknown.stderr
    );
    assert_eq!((2, ""), (unknown.code, unknown.stdout.as_str()));
    let missing = s.dir.path().join("missing.json").display().to_string();
    let out = usage_command(&missing, None, None, false);
    assert_eq!(
        format!("usage: no usage config at {missing}; copy packaging/usage.example.json\n"),
        out.stderr
    );
    assert_eq!(2, out.code);
}

#[test]
fn sorted_json_sorts_every_level_and_escapes_astral_characters() {
    let value = json!({"b": 1, "a": {"z": [1, {"y": 2, "x": 3}], "c": "😀é"}});
    assert_eq!(
        "{\n  \"a\": {\n    \"c\": \"\\ud83d\\ude00\\u00e9\",\n    \"z\": [\n      1,\n      {\n        \"x\": 3,\n        \"y\": 2\n      }\n    ]\n  },\n  \"b\": 1\n}",
        sorted_json(&value)
    );
}
