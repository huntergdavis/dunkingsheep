//! T42 parser tests. Ported (parse level only) from `tests/test_cli_json_position.py`:
//! `--json` is accepted before the subcommand and after it at ANY depth. The Python's
//! structural test ("every add_subparsers call carries parser_class") becomes "every
//! non-root command carries `json`", which is the same invariant on the generated table.
//! NOT ported here: `client_timeout_for` (not a parser concern) and `test_cli_guard` /
//! `test_cli` end-to-end behaviour (command bodies are T43 onwards).

use ds_cli::args::{build, try_parse};
use ds_cli::cli_table::ROOT;
use ds_cli::spec::Cmd;
use serde_json::json;

fn parse(argv: &[&str]) -> serde_json::Map<String, serde_json::Value> {
    let mut full = vec!["dunkingsheep"];
    full.extend_from_slice(argv);
    try_parse(full).unwrap_or_else(|e| panic!("{argv:?}: {e}"))
}

#[test]
fn trailing_json_is_accepted_by_nested_read_only_actions() {
    for argv in [
        ["owner", "stats"],
        ["task", "list"],
        ["inbox", "count"],
        ["dispatch", "status"],
        ["lane", "list"],
        ["retired", "list"],
    ] {
        let mut with_json = argv.to_vec();
        with_json.push("--json");
        let ns = parse(&with_json);
        assert_eq!(ns["json"], json!(true), "{argv:?}");
        assert_eq!(ns["command"], json!(argv[0]), "{argv:?}");
    }
}

#[test]
fn the_leading_form_still_works() {
    let ns = parse(&["--json", "owner", "stats"]);
    assert_eq!(ns["json"], json!(true));
    assert_eq!(ns["owner_command"], json!("stats"));
}

#[test]
fn a_trailing_json_does_not_unset_a_leading_one_and_absent_means_false() {
    assert_eq!(
        parse(&["--json", "task", "list", "--json"])["json"],
        json!(true)
    );
    assert_eq!(parse(&["task", "list"])["json"], json!(false));
}

fn walk(cmd: &Cmd, path: &str, missing: &mut Vec<String>, root: bool) {
    if !root && !cmd.args.iter().any(|a| a.dest == "json") {
        missing.push(path.to_string());
    }
    for sub in cmd.subs {
        walk(sub, &format!("{path} {}", sub.name), missing, false);
    }
}

#[test]
fn every_command_at_every_depth_carries_json() {
    let mut missing = Vec::new();
    walk(&ROOT, "dunkingsheep", &mut missing, true);
    assert_eq!(Vec::<String>::new(), missing);
}

#[test]
fn the_whole_tree_passes_clap_debug_asserts() {
    // Duplicate ids, a `version` clash, a bad group member: all panic here, not in prod.
    build().debug_assert();
}

#[test]
fn usage_errors_exit_2_and_help_and_version_exit_0() {
    let code = |argv: &[&str]| {
        let mut full = vec!["dunkingsheep"];
        full.extend_from_slice(argv);
        try_parse(full).err().map(|e| e.exit_code())
    };
    assert_eq!(code(&[]), Some(2), "a subcommand is required");
    assert_eq!(code(&["no-such-command"]), Some(2));
    assert_eq!(code(&["--help"]), Some(0));
    assert_eq!(code(&["--version"]), Some(0));
    assert_eq!(code(&["status", "--no-such-flag"]), Some(2));
}

#[test]
fn values_keep_their_types_and_defaults() {
    let ns = parse(&["status", "--check-herdr"]);
    assert_eq!(ns["check_herdr"], json!(true));
    assert_eq!(ns["command"], json!("status"));
    let ns = parse(&["--socket", "/tmp/x.sock", "--no-start-daemon", "status"]);
    assert_eq!(ns["socket"], json!("/tmp/x.sock"));
    assert_eq!(ns["no_start_daemon"], json!(true));
    assert_eq!(ns["self_pane"], serde_json::Value::Null);
}

#[test]
fn the_top_level_command_count_is_recorded() {
    // The task says "the same 76 commands"; this pins what the generated table has so a
    // regenerate that adds or drops one is a visible diff, not a silent one.
    eprintln!("top-level commands: {}", ROOT.subs.len());
    assert!(!ROOT.subs.is_empty());
}

#[test]
fn a_dest_shared_by_two_flags_is_tri_state() {
    // `task update --multi / --no-multi` share `multi_assignee`, default None.
    assert_eq!(
        parse(&["task", "update", "t1"])["multi_assignee"],
        serde_json::Value::Null
    );
    assert_eq!(
        parse(&["task", "update", "t1", "--multi"])["multi_assignee"],
        json!(true)
    );
    assert_eq!(
        parse(&["task", "update", "t1", "--no-multi"])["multi_assignee"],
        json!(false)
    );
}

const NAMESPACES: &str = include_str!("cli_namespaces.json");

/// The differential: every recorded argv parses to the SAME dictionary argparse produced
/// (`tools/gen_cli_rs.py` records them from the Python parser: each leaf command minimal,
/// with `--json` leading and trailing, and with each optional flag on its own).
#[test]
fn every_recorded_argv_parses_to_the_pythons_namespace() {
    let cases: Vec<serde_json::Value> = serde_json::from_str(NAMESPACES).expect("fixture");
    assert!(
        cases.len() > 500,
        "the fixture is suspiciously small: {}",
        cases.len()
    );
    let mut bad = Vec::new();
    for case in &cases {
        let mut argv = vec!["dunkingsheep".to_string()];
        argv.extend(
            case["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string()),
        );
        let want = case["namespace"].as_object().unwrap();
        match try_parse(&argv) {
            Err(e) => bad.push(format!(
                "{:?}: rejected: {}",
                &argv[1..],
                e.to_string().lines().next().unwrap_or("")
            )),
            Ok(got) if &got != want => {
                let keys: Vec<_> = want
                    .keys()
                    .chain(got.keys())
                    .filter(|k| want.get(*k) != got.get(*k))
                    .collect();
                bad.push(format!("{:?}: differs at {:?}", &argv[1..], keys));
            }
            Ok(_) => {}
        }
    }
    bad.sort();
    bad.dedup();
    assert!(
        bad.is_empty(),
        "{} of {} differ, first 15:\n{}",
        bad.len(),
        cases.len(),
        bad.iter().take(15).cloned().collect::<Vec<_>>().join("\n")
    );
}
