//! Ports, by name, of tests/test_dunk_start_agent_verify.py::StartAgentVerifyTests, against
//! a scripted [`VerifyBackend`] that models `tests/fakes.py::FakeHerdr`'s relevant behaviour
//! (a new pane with a pane_pid, a screen that is a string or a list of frames served one per
//! read and clamped to the last, a process table that is `None` (no table), a list, or a
//! list of frames served one per call). `start_agent` itself is a later slice, so the tests
//! call `verify_agent_started` with `kind = "claude"` (or "pi"), as `start_agent` would.
//! NOT ported here: TmuxBackendContractTests (the real tmux backend), and the
//! `result["name"]` assertions, which belong to `start_agent`.

use super::*;
use serde_json::json;

const PANE: &str = "w2:p101";
const PANE_PID: i64 = 50101;

enum Screen {
    One(String),
    Frames(Vec<String>),
}

struct Scripted {
    pane_exists: bool,
    screen: Screen,
    /// `None` = no process table on this platform.
    procs: Option<Vec<Vec<Value>>>,
    reads: usize,
    proc_calls: usize,
}

impl Scripted {
    fn new(screen: Screen, procs: Option<Vec<Vec<Value>>>) -> Self {
        Scripted {
            pane_exists: true,
            screen,
            procs,
            reads: 0,
            proc_calls: 0,
        }
    }
}

impl VerifyBackend for Scripted {
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        (self.pane_exists && pane_id == PANE)
            .then(|| json!({"pane_id": PANE, "pane_pid": PANE_PID}))
    }
    fn read_pane(&mut self, _pane_id: &str, lines: usize) -> Option<String> {
        self.reads += 1;
        let screen = match &self.screen {
            Screen::One(s) => s.clone(),
            Screen::Frames(f) => f[(self.reads - 1).min(f.len() - 1)].clone(),
        };
        let tail: Vec<&str> = screen.lines().collect();
        let tail = &tail[tail.len().saturating_sub(lines)..];
        Some(format!("{}\n", tail.join("\n")))
    }
    fn agent_child_pids(&mut self, pane_pid: i64) -> Option<Vec<Value>> {
        let frames = self.procs.as_ref()?;
        let frame = &frames[self.proc_calls.min(frames.len() - 1)];
        self.proc_calls += 1;
        // FakeHerdr: pid = ppid + 1 + index, comm defaults to the argv0 basename.
        Some(
            frame
                .iter()
                .enumerate()
                .map(|(index, child)| {
                    let mut e = child.clone();
                    e["pid"] = json!(pane_pid + 1 + index as i64);
                    e
                })
                .collect(),
        )
    }
    fn process_start(&self, _pid: i64) -> Option<i64> {
        None // a fixture pid has no /proc entry: the generation clause is skipped
    }
}

fn child(argv: &[&str], comm: Option<&str>, state: &str, start: i64) -> Value {
    let comm = comm
        .map(str::to_string)
        .unwrap_or_else(|| argv[0].rsplit('/').next().unwrap().to_string());
    json!({"argv": argv, "comm": comm, "state": state, "start": start})
}

fn run(screen: Screen, procs: Option<Vec<Vec<Value>>>) -> Result<Option<i64>, DunkError> {
    let mut backend = Scripted::new(screen, procs);
    verify_agent_started(&mut backend, "probe", Some(PANE), Some("claude"), 0.4)
}

fn start(text: &str, procs: Option<Vec<Vec<Value>>>) -> Result<Option<i64>, DunkError> {
    run(Screen::One(text.to_string()), procs)
}

fn err(r: Result<Option<i64>, DunkError>) -> String {
    r.expect_err("expected a refusal").to_string()
}

fn has(text: &str, parts: &[&str]) {
    for part in parts {
        assert!(text.contains(part), "{part:?} not in {text:?}");
    }
}

#[test]
fn test_a_missing_command_is_refused_with_the_captured_line() {
    let e = err(start("/bin/sh: 1: opencode: not found\n", None));
    assert!(e.contains("never ran") || e.contains("not found"), "{e}");
}

#[test]
fn test_an_immediate_exit_is_refused_with_the_status() {
    has(
        &err(start("\n[agent exited rc=127]\n", None)),
        &["exited immediately"],
    );
}

#[test]
fn test_a_running_agent_is_reported_started() {
    assert_eq!(None, start("✻ Welcome to Claude Code\n❯ \n", None).unwrap());
}

#[test]
fn test_a_benign_startup_line_containing_not_found_still_starts() {
    start(
        "\u{273b} theme cache not found, rebuilding\n\u{273b} ready\n\u{276f} \n",
        None,
    )
    .unwrap();
}

#[test]
fn test_shell_shaped_failures_are_still_refused() {
    for line in [
        "/bin/sh: 1: opencode: not found\n",
        "bash: opencode: command not found\n",
        "sh: cannot execute opencode\n",
    ] {
        let e = err(start(line, None));
        assert!(
            e.contains("never ran") || e.contains("not found") || e.contains("cannot execute"),
            "{e}"
        );
    }
}

#[test]
fn test_the_refusal_names_the_pane_it_left_behind() {
    let e = err(start("/bin/sh: 1: opencode: not found\n", None));
    assert!(e.contains(PANE), "{e}");
}

#[test]
fn test_a_pane_that_vanishes_is_refused() {
    let mut backend = Scripted::new(Screen::One("starting...\n".into()), None);
    backend.pane_exists = false;
    let e = verify_agent_started(&mut backend, "probe", Some(PANE), Some("claude"), 0.4)
        .unwrap_err()
        .to_string();
    has(&e, &["already gone"]);
}

#[test]
fn test_a_launcher_noise_line_does_not_refuse_a_live_pi_lane() {
    // t599: process evidence wins over the screen proxy.
    let screen = "sed: can't read /home/hunter/.pi/settings.json: No such file or directory\n\n\u{276f} ready\n";
    let procs = vec![vec![child(
        &["/home/hunter/.local/bin/pi", "--offline"],
        Some("pi"),
        "S",
        2000,
    )]];
    let mut backend = Scripted::new(Screen::One(screen.into()), Some(procs));
    let pid = verify_agent_started(&mut backend, "probe", Some(PANE), Some("pi"), 0.4).unwrap();
    assert!(pid.is_some());
}

#[test]
fn test_the_same_line_without_a_live_process_is_still_refused() {
    let screen = "sed: can't read /home/hunter/.pi/settings.json: No such file or directory\n";
    let e = err(start(screen, Some(vec![vec![]])));
    assert!(e.contains("never ran") || e.contains("No such file"), "{e}");
}

#[test]
fn test_an_exit_line_still_beats_a_pid_sighting() {
    let screen = Screen::Frames(vec!["$ \n".into(), "\n[agent exited rc=127]\n".into()]);
    let procs = Some(vec![vec![child(
        &["/usr/bin/claude"],
        Some("claude"),
        "S",
        2000,
    )]]);
    has(&err(run(screen, procs)), &["exited immediately"]);
}

#[test]
fn test_a_first_run_trust_dialog_is_refused_and_named() {
    let screen = "Do you trust the files in this folder?\n❯ 1. Yes, proceed\n  2. No, exit\n";
    let e = err(start(screen, None));
    has(&e, &["NOT interactive", "trust the files"]);
}

#[test]
fn test_a_first_run_login_dialog_is_refused_too() {
    let e = err(start(
        "Select login method:\n❯ 1. Claude account\n  2. API key\n",
        None,
    ));
    has(&e, &["NOT interactive", "login method"]);
}

#[test]
fn test_the_dialog_refusal_wins_over_a_live_process() {
    let procs = Some(vec![vec![child(&["claude"], None, "S", 2000)]]);
    has(
        &err(start("Do you trust the files in this folder?\n", procs)),
        &["NOT interactive"],
    );
}

fn codex_approval_dialog() -> String {
    [
        "  \u{2514} Run the command: inspect pc \u{2026}",
        "  \u{2514}   1. Yes, proceed (y)",
        "  \u{2514}   2. No, and tell Codex what to do differently (esc)",
        "",
        "      Press Enter to confirm or Esc to cancel",
        "",
        "  gpt-6-sol high \u{b7} ~/workspace",
    ]
    .join("\n")
        + "\n"
}

#[test]
fn test_the_captured_codex_approval_dialog_is_refused() {
    let e = err(start(&codex_approval_dialog(), None));
    has(&e, &["NOT interactive", "confirm or Esc"]);
}

#[test]
#[allow(non_snake_case)] // the Python test name, kept verbatim
fn test_the_ANSWERED_dialog_in_scrollback_is_not_a_refusal() {
    // The same prompt, answered long ago: 20 output lines and an idle box under it. (The
    // idle box is the plain-text rendering of fakes.py::codex_box_hint_ansi.)
    let mut lines: Vec<String> = vec![
        "  \u{2514} Run the command: inspect pc \u{2026}".into(),
        "  \u{2514}   1. Yes, proceed (y)".into(),
        "  \u{2514}   2. No, and tell Codex what to do differently (esc)".into(),
        "      Press Enter to confirm or Esc to cancel".into(),
    ];
    lines.extend((1..=20).map(|n| format!("  \u{2514} output line {n}")));
    lines.extend([
        "".into(),
        "\u{203a}\u{2801}Ask Codex to do anything".into(),
        "".into(),
        "  gpt-5.6-sol xhigh \u{b7} ~/workspace/site".into(),
    ]);
    start(&(lines.join("\n") + "\n"), None).expect("a healthy lane must start");
}

#[test]
fn test_the_dialog_match_reads_only_the_live_tail() {
    let screen = format!(
        "Press Enter to confirm or Esc to cancel\n{}\n❯ \n",
        (1..20)
            .map(|n| format!("  output line {n}"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    start(&screen, None).unwrap();
}

#[test]
fn test_a_welcome_screen_with_no_dialog_still_starts() {
    start("✻ Welcome to Claude Code\n❯ \n", None).unwrap();
}

#[test]
fn test_a_clean_screen_with_no_process_is_refused() {
    has(
        &err(start("$ \n", Some(vec![vec![]]))),
        &["never became a process"],
    );
}

#[test]
fn test_a_matching_child_is_confirmed_with_its_pid() {
    let procs = Some(vec![vec![child(
        &["/usr/bin/claude", "--model", "opus"],
        Some("claude"),
        "R",
        2000,
    )]]);
    assert_eq!(Some(PANE_PID + 1), start("$ \n", procs).unwrap());
}

#[test]
fn test_early_match_then_exit_line_is_refused() {
    let screen = Screen::Frames(vec!["$ \n".into(), "\n[agent exited rc=127]\n".into()]);
    let procs = Some(vec![vec![child(
        &["/usr/bin/claude"],
        Some("claude"),
        "S",
        2000,
    )]]);
    has(&err(run(screen, procs)), &["exited immediately"]);
}

#[test]
fn test_transient_pid_then_gone_is_refused() {
    let frames = vec![
        vec![child(&["/usr/bin/claude"], Some("claude"), "S", 2000)],
        vec![],
    ];
    has(&err(start("$ \n", Some(frames))), &["gone at the end"]);
}

#[test]
fn test_argv_spoof_is_not_identity() {
    let entries = vec![
        json!({"pid": 101, "comm": "sleep", "state": "S", "start": 2000,
                              "argv": ["claude", "--model", "opus"]}),
    ];
    assert!(match_agent_process(&entries, "claude", None).is_empty());
}

#[test]
fn test_stopped_and_zombie_are_not_evidence() {
    for state in ["T", "t", "Z", "X", "x"] {
        let entries = vec![
            json!({"pid": 101, "comm": "claude", "state": state, "start": 2000,
                                  "argv": ["/usr/bin/claude"]}),
        ];
        assert!(
            match_agent_process(&entries, "claude", None).is_empty(),
            "state {state} must not match"
        );
    }
}

#[test]
fn test_stale_generation_is_not_evidence() {
    let entries = vec![
        json!({"pid": 101, "comm": "claude", "state": "R", "start": 500,
                              "argv": ["/usr/bin/claude"]}),
    ];
    assert!(match_agent_process(&entries, "claude", Some(1000)).is_empty());
    assert_eq!(
        vec![101],
        match_agent_process(&entries, "claude", Some(500))
    );
    // Unknown generation skips the clause rather than refusing blind.
    assert_eq!(vec![101], match_agent_process(&entries, "claude", None));
}

#[test]
fn test_kernel_comm_match_holds() {
    let entries = vec![
        json!({"pid": 11, "comm": "bash", "state": "S", "start": 1000, "argv": ["bash"]}),
        json!({"pid": 12, "comm": "claude", "state": "R", "start": 2000, "argv": ["/usr/bin/claude"]}),
    ];
    assert_eq!(vec![12], match_agent_process(&entries, "claude", None));
    assert_eq!(vec![12], match_agent_process(&entries, "Claude", None));
    assert!(match_agent_process(&entries, "sh", None).is_empty());
    assert!(match_agent_process(&[], "claude", None).is_empty());
}

#[test]
fn test_no_process_table_stays_screen_only() {
    assert_eq!(None, start("✻ Welcome to Claude Code\n❯ \n", None).unwrap());
}

#[test]
fn the_verify_window_defaults_to_two_seconds_and_reads_the_env_value() {
    assert_eq!(2.0, agent_verify_s_from(None));
    assert_eq!(0.5, agent_verify_s_from(Some(" 0.5 ")));
    assert_eq!(2.0, agent_verify_s_from(Some("soon")));
}
