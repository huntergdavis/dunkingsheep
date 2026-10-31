//! T43 differential: each Rust `render_*` equals the Python `print_*` output for the same
//! payload (`tools/gen_printer_fixtures.py` records the Python side).

use ds_cli::render_fleet as r;
use serde_json::Value;

const FIXTURE: &str = include_str!("printers_fleet.json");

fn render(printer: &str, input: &Value) -> String {
    match printer {
        "print_poolrad_sync" => r::render_poolrad_sync(input),
        "print_poolrad_progress" => r::render_poolrad_progress(input),
        "print_review_sync" => r::render_review_sync(input),
        "print_review_list" => r::render_review_list(input),
        "print_whoami" => r::render_whoami(input),
        "print_superuser" => r::render_superuser(input),
        "print_message" => r::render_message(input),
        "print_capability" => r::render_capability(input),
        "print_lanes" => r::render_lanes(input),
        "print_lane" => r::render_lane(input),
        "print_inbox_stats" => r::render_inbox_stats(input),
        "print_owner_push" => r::render_owner_push(input),
        "print_report_run" => r::render_report_run(input),
        "print_owner_reply" => r::render_owner_reply(input),
        "print_clear" => r::render_clear(input),
        "print_messages" => r::render_messages(input),
        "print_workspaces" => r::render_workspaces(input),
        "print_created" => r::render_created(input),
        "print_poll" => r::render_poll(input),
        "print_turn_end" => r::render_turn_end(input),
        other => panic!("no Rust renderer wired for {other}"),
    }
}

#[test]
fn every_recorded_payload_prints_what_the_python_printed() {
    let cases: Vec<Value> = serde_json::from_str(FIXTURE).expect("fixture");
    assert!(cases.len() >= 20, "fixture too small: {}", cases.len());
    let mut bad = Vec::new();
    for case in &cases {
        let printer = case["printer"].as_str().unwrap();
        let want = case["output"].as_str().unwrap();
        let got = render(printer, &case["input"]);
        if got != want {
            bad.push(format!(
                "{printer} {}\n  got:  {got:?}\n  want: {want:?}",
                case["input"]
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} of {} differ:\n{}",
        bad.len(),
        cases.len(),
        bad.join("\n")
    );
}
