//! T44 end to end: argv -> the generated parser -> `dispatch::run` prints what the real Python
//! CLI printed. Replays the recorded `context` steps (tests/context_cmd.json, built by
//! tools/gen_context_cmd_fixture.py from the real Python) through the WHOLE path, so a parser
//! or namespace mistake that the `context_read` differential cannot see shows up here.
//! One test fn: cwd and environment are process-wide.

use ds_cli::args::try_parse;
use ds_cli::dispatch::run;
use serde_json::Value;

const FIXTURE: &str = include_str!("context_cmd.json");

#[test]
fn recorded_context_steps_run_end_to_end_through_the_parser_and_dispatch() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture");
    let base = tempfile::tempdir().expect("tempdir");
    for (relative, text) in fixture["files"].as_object().unwrap() {
        let path = base.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text.as_str().unwrap()).unwrap();
    }
    std::env::set_var("DUNKINGSHEEP_DIR", base.path().join("cfg"));
    std::env::set_var("DUNKINGSHEEP_SELF", "fixture");
    std::env::set_current_dir(base.path().join("proj")).unwrap();
    let t = base.path().display().to_string();
    let mut bad = Vec::new();
    for step in fixture["steps"].as_array().unwrap() {
        let mut argv = vec!["dunkingsheep".to_string(), "context".to_string()];
        argv.extend(
            step["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string()),
        );
        let ns = try_parse(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let out = run(&ns);
        let (want_out, want_err) = (
            step["stdout"].as_str().unwrap(),
            step["stderr"].as_str().unwrap(),
        );
        let want_code = step["code"].as_i64().unwrap() as i32;
        let (got_out, got_err) = (out.stdout.replace(&t, "<T>"), out.stderr.replace(&t, "<T>"));
        if out.code != want_code || got_out != want_out || got_err != want_err {
            bad.push(format!(
                "{argv:?}\n  got:  code {} out {got_out:?} err {got_err:?}\n  want: code {want_code} out {want_out:?} err {want_err:?}",
                out.code
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} steps differ:\n{}",
        bad.len(),
        bad.join("\n")
    );

    // --json is valid JSON of the same result (key order differs from Python: documented).
    let ns = try_parse(["dunkingsheep", "context", "stats", "--json"]).unwrap();
    let out = run(&ns);
    assert_eq!(0, out.code);
    let parsed: Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    assert!(
        parsed["global"]["counts"]["ground-truth"].is_number(),
        "{parsed}"
    );
}

#[test]
fn commands_without_a_body_say_so_and_exit_1_and_writes_are_not_ported() {
    for argv in [
        vec!["dunkingsheep", "status"],
        vec!["dunkingsheep", "context", "init"],
    ] {
        let ns = try_parse(&argv).unwrap();
        let out = run(&ns);
        assert_eq!(1, out.code, "{argv:?}");
        assert_eq!("", out.stdout);
        assert!(
            out.stderr.contains("is parsed but not ported yet"),
            "{}",
            out.stderr
        );
    }
}
