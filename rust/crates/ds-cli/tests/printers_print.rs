//! T43 differential for `render_print.rs` (t1283): each Rust `render_*` equals the Python
//! `print_*` stdout for the same hand-written payload. `tools/gen_printer_fixtures.py`
//! records the Python side into `printers_print.json` with `TZ=UTC`; the clock printers
//! get a UTC formatter here.

use ds_cli::render_print as r;
use serde_json::Value;

const FIXTURE: &str = include_str!("printers_print.json");

/// `time.strftime(fmt, time.localtime(t))` in UTC for the formats the printers use.
fn utc(fmt: &'static str) -> impl Fn(f64) -> String {
    move |t: f64| {
        let secs = t.floor() as i64;
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        // Howard Hinnant's civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };
        let (hh, mm, ss) = (rem / 3_600, rem % 3_600 / 60, rem % 60);
        match fmt {
            "%Y-%m-%d %H:%M:%S" => format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}:{ss:02}"),
            "%Y-%m-%d %H:%M" => format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}"),
            "%Y-%m-%d %H:00" => format!("{y:04}-{m:02}-{d:02} {hh:02}:00"),
            "%m-%d %H:%M" => format!("{m:02}-{d:02} {hh:02}:{mm:02}"),
            "%H:%M:%S" => format!("{hh:02}:{mm:02}:{ss:02}"),
            other => panic!("no UTC formatter for {other}"),
        }
    }
}

fn list(input: &Value) -> &[Value] {
    input
        .as_array()
        .expect("this printer takes a list")
        .as_slice()
}

fn render(printer: &str, input: &Value) -> String {
    let full = utc("%Y-%m-%d %H:%M:%S");
    match printer {
        "print_dunks" => r::render_dunks(list(input)),
        "print_dunk" => r::render_dunk(input),
        "print_panes" => r::render_panes(input),
        "print_inbox_archive" => r::render_inbox_archive(input),
        "print_load" => r::render_load(input),
        "print_pm_digest" => r::render_pm_digest(input),
        "print_pm_routing" => r::render_pm_routing(input),
        "print_inbox_count" => r::render_inbox_count(input),
        "print_inbox_sent" => r::render_inbox_sent(input),
        "print_inbox_list" => r::render_inbox_list(input),
        "print_task" => r::render_task(input, &full),
        "print_task_list" => r::render_task_list(input),
        "print_task_next" => r::render_task_next(input, &full),
        "print_task_followers" => r::render_task_followers(input),
        "print_task_stats" => r::render_task_stats(input),
        "print_memory" => r::render_memory(list(input)),
        "print_memory_context" => r::render_memory_context(input),
        "print_memory_hook_context" => r::render_memory_hook_context(input),
        "print_memory_backfill" => r::render_memory_backfill(input),
        "print_memory_stats" => r::render_memory_stats(input),
        "print_health" => r::render_health(input),
        "print_doctor" => r::render_doctor(input),
        "print_recover" => r::render_recover(input),
        "print_task_metrics" => r::render_task_metrics(input, &utc("%Y-%m-%d %H:00")),
        "print_inbox_read" => r::render_inbox_read(input),
        "print_inbox_recover" => r::render_inbox_recover(input),
        "print_inbox_status" => r::render_inbox_status(input, &full),
        "print_retired" => r::render_retired(input, &utc("%Y-%m-%d %H:%M")),
        "print_report_list" => r::render_report_list(input, &utc("%Y-%m-%d %H:%M")),
        "print_owner_conversation" => r::render_owner_conversation(input, &utc("%m-%d %H:%M")),
        "print_owner_take" => r::render_owner_take(input, &utc("%H:%M:%S")),
        "print_owner_stats" => r::render_owner_stats(input),
        "print_view" => r::render_view(input),
        other => panic!("no Rust renderer wired for {other}"),
    }
}

/// The printers whose Python output is not a single `render_*` string.
fn render_special(case: &Value) -> Option<(String, String)> {
    let input = &case["input"];
    let full = utc("%Y-%m-%d %H:%M:%S");
    match case["printer"].as_str().unwrap() {
        // (stderr, stdout)
        "print_herdr" => Some(r::render_herdr(input)),
        "print_status" => Some((
            String::new(),
            r::render_status(input, &full, case["now"].as_f64().expect("now")),
        )),
        // A tick prints `dunk_dispatch.summary(result)` (dispatch.rs, not ported) and then the
        // per-record lines; `render_dispatch` is None for that branch, so the printed text
        // must END with `render_dispatched`.
        "print_dispatch" => Some((
            String::new(),
            match r::render_dispatch(input) {
                Some(text) => text,
                None => {
                    let want = case["output"].as_str().unwrap();
                    let tail = r::render_dispatched(input);
                    assert!(
                        want.ends_with(&tail),
                        "dispatch tick tail differs: want {want:?}, tail {tail:?}"
                    );
                    want.to_string() // the summary line is dunk_dispatch's, compared above
                }
            },
        )),
        _ => None,
    }
}

#[test]
fn every_recorded_payload_prints_what_the_python_printed() {
    let cases: Vec<Value> = serde_json::from_str(FIXTURE).expect("fixture");
    assert!(cases.len() >= 50, "fixture too small: {}", cases.len());
    let mut bad = Vec::new();
    for case in &cases {
        let printer = case["printer"].as_str().unwrap();
        let want = case["output"].as_str().unwrap();
        let (got_err, got) = match render_special(case) {
            Some(pair) => pair,
            None => (String::new(), render(printer, &case["input"])),
        };
        let want_err = case.get("stderr").and_then(Value::as_str).unwrap_or("");
        if got_err != want_err {
            bad.push(format!(
                "{printer} {}\n  stderr got:  {got_err:?}\n  stderr want: {want_err:?}",
                case["input"]
            ));
        }
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
