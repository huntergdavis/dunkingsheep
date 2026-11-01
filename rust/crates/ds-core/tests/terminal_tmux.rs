//! T22: the tmux subprocess backend, driven by a fake `tmux` script.
//!
//! No test needs a live server: each one points `tmux_bin` at a script that records
//! the argv it was handed and answers with canned output. That checks the two things
//! a port gets wrong - the ARGV (which is the contract with tmux) and the mapping of
//! exit codes to "empty world" versus "failure".

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use ds_core::terminal::tmux::{shell_join, TmuxBackend, PANE_FORMAT};
use ds_core::terminal::{Terminal, TerminalError};

use std::sync::{Mutex, MutexGuard};

/// The tests in this file take turns. Each one writes a shell script and executes it at once, and
/// Linux refuses to exec a file that ANY process still holds open for writing (ETXTBSY, "Text
/// file busy", errno 26). With tests on parallel threads, a `fork` on one thread copies another
/// thread's still-open write descriptor into a child that keeps it until its own `exec`, so a
/// fresh script failed to run about one run in three (a different test each time; measured
/// 2026-10-31, error `tmux: Text file busy (os error 26)`). The whole file takes ~0.3 s, so
/// serialising it costs nothing and removes the race instead of narrowing it.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

const SEP: &str = "\x1f";

/// Write a fake tmux that logs its argv to `<dir>/argv.txt` and prints `stdout`,
/// exiting with `code`.
fn fake_tmux(dir: &PathBuf, stdout: &str, code: i32) -> PathBuf {
    let script = dir.join("tmux");
    let log = dir.join("argv.txt");
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\ncat <<'OUT'\n{stdout}\nOUT\nexit {code}\n",
        log = log.display()
    );
    fs::write(&script, body).expect("write fake tmux");
    let mut permissions = fs::metadata(&script).expect("stat").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("chmod");
    script
}

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

fn pane_line(pane: &str, session: &str, window: &str, command: &str, pid: &str) -> String {
    [
        pane,
        session,
        window,
        "/home/hunter/work",
        command,
        "title",
        "1",
        "work",
        "1",
        "2",
        "1",
        pid,
        "120",
        "40",
    ]
    .join(SEP)
}

#[test]
fn list_panes_parses_the_canned_listing() {
    let _serial = serial();
    let dir = tempdir();
    let listing = pane_line("%2", "$2", "@2", "claude", "2202");
    let fake = fake_tmux(&dir.path().to_path_buf(), &listing, 0);
    let backend = TmuxBackend::new(fake);
    let panes = backend.list_panes();
    assert_eq!(1, panes.len());
    assert_eq!("%2", panes[0].pane_id);
    assert_eq!("claude", panes[0].command);
    assert_eq!(Some(2202), panes[0].pane_pid);
    let argv = fs::read_to_string(dir.path().join("argv.txt")).unwrap();
    assert!(
        argv.contains("list-panes -a -F"),
        "the argv carries the format: {argv}"
    );
    assert!(
        argv.contains("{pane_id}"),
        "the format is passed through verbatim: {argv}"
    );
}

/// How many times the fake tmux ran (one argv line per process).
fn spawns(dir: &std::path::Path) -> usize {
    fs::read_to_string(dir.join("argv.txt"))
        .unwrap_or_default()
        .lines()
        .count()
}

#[test]
fn listing_panes_with_their_status_is_one_tmux_process_not_one_per_pane() {
    // t1296: TerminalEnv asked `agent_status` for every pane, and on tmux that is a
    // get_pane -> list-panes: N panes cost N+1 spawns where Python's `_parse_panes` costs 1.
    let _serial = serial();
    let dir = tempdir();
    let listing = (0..6)
        .map(|n| {
            pane_line(
                &format!("%{n}"),
                "$1",
                "@1",
                "claude",
                &format!("{}", 3000 + n),
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let fake = fake_tmux(&dir.path().to_path_buf(), &listing, 0);
    let backend = TmuxBackend::new(fake);
    let panes = backend.list_panes_with_status();
    assert_eq!(6, panes.len());
    assert!(panes.iter().all(|(_, s)| s.as_deref() == Some("unknown")));
    assert_eq!(1, spawns(dir.path()), "one list-panes for all six panes");
}

#[test]
fn one_pane_with_its_status_is_one_tmux_process_and_a_missing_pane_is_none() {
    let _serial = serial();
    let dir = tempdir();
    let listing = pane_line("%2", "$2", "@2", "claude", "2202");
    let fake = fake_tmux(&dir.path().to_path_buf(), &listing, 0);
    let backend = TmuxBackend::new(fake);
    let (pane, status) = backend.get_pane_with_status("%2").expect("pane");
    assert_eq!("%2", pane.pane_id);
    assert_eq!(Some("unknown".to_string()), status);
    assert_eq!(1, spawns(dir.path()), "get_pane + agent_status was two");
    assert!(backend.get_pane_with_status("%99").is_none());
}

#[test]
fn a_missing_server_is_an_empty_world_not_an_error() {
    let _serial = serial();
    let dir = tempdir();
    let fake = fake_tmux(
        &dir.path().to_path_buf(),
        "no server running on /tmp/tmux-1000/default",
        1,
    );
    let backend = TmuxBackend::new(fake);
    assert!(backend.list_panes().is_empty());
    assert!(backend.list_tabs().is_empty());
    assert!(backend.list_workspaces().is_empty());
}

#[test]
fn fallible_listing_preserves_the_exact_tmux_failure() {
    let _serial = serial();
    let dir = tempdir();
    let fake = fake_tmux(&dir.path().to_path_buf(), "pane listing is offline", 7);
    let backend = TmuxBackend::new(fake);
    let expected = Err(TerminalError::refused("pane listing is offline"));

    assert_eq!(expected, backend.try_list_panes());
    assert_eq!(expected, backend.try_list_panes_grouped());
}

#[test]
fn fallible_listing_keeps_a_missing_server_as_an_empty_world() {
    let _serial = serial();
    let dir = tempdir();
    let fake = fake_tmux(
        &dir.path().to_path_buf(),
        "no server running on /tmp/tmux-1000/default",
        1,
    );
    let backend = TmuxBackend::new(fake);

    assert_eq!(Ok(Vec::new()), backend.try_list_panes());
    assert_eq!(Ok(Default::default()), backend.try_list_panes_grouped());
}

#[test]
fn the_pane_format_is_the_python_format() {
    let _serial = serial();
    // Fourteen fields, in the Python's order, separator included.
    assert_eq!(14, PANE_FORMAT.split('\x1f').count());
    assert!(PANE_FORMAT.starts_with("#{pane_id}"));
    assert!(PANE_FORMAT.ends_with("#{pane_width}\x1f#{pane_height}"));
}

#[test]
fn close_refuses_anything_this_backend_did_not_create() {
    let _serial = serial();
    let dir = tempdir();
    let listing = pane_line("%2", "$2", "@2", "bash", "2202");
    let fake = fake_tmux(&dir.path().to_path_buf(), &listing, 0);
    let mut backend = TmuxBackend::new(fake);
    let error: TerminalError = backend.close("pane", "%2").unwrap_err();
    assert!(error.to_string().contains("did not create it"), "{error}");
    let calls = fs::read_to_string(dir.path().join("argv.txt")).unwrap_or_default();
    assert!(!calls.contains("kill-pane"), "no kill was sent: {calls}");
}

#[test]
fn a_created_pane_can_be_closed_and_the_kill_argv_is_right() {
    let _serial = serial();
    let dir = tempdir();
    let listing = pane_line("%2", "$2", "@2", "bash", "2202");
    let fake = fake_tmux(&dir.path().to_path_buf(), &listing, 0);
    let mut backend = TmuxBackend::new(fake);
    backend.mark_created("%2");
    backend.close("pane", "%2").expect("a created pane closes");
    let calls = fs::read_to_string(dir.path().join("argv.txt")).unwrap();
    assert!(calls.contains("kill-pane -t %2"), "{calls}");
}

/// t994 finding 5 + 1: this asserted the DEFECTIVE behaviour and was named for
/// send_keys while calling run_command. It now asserts what the Python does: chunked
/// literal sends, then a settle, then Enter.
#[test]
fn run_command_is_literal_chunked_and_settles_before_enter() {
    let _serial = serial();
    let dir = tempdir();
    let fake = fake_tmux(&dir.path().to_path_buf(), "", 0);
    let mut backend = TmuxBackend::new(fake);
    backend
        .run_command("%2", "dunkingsheep status; echo hi")
        .expect("typed");
    let calls = fs::read_to_string(dir.path().join("argv.txt")).unwrap();
    assert!(
        calls.contains("send-keys -t %2 -l -- dunkingsheep status; echo hi"),
        "literal, after --, in one argv: {calls}"
    );
    assert!(
        calls.contains("send-keys -t %2 Enter"),
        "a separate Enter: {calls}"
    );
}

/// t994 finding 1: a long message is typed in CHUNKS, as the Python's
/// `_text_chunks(text)` + SEND_TEXT_CHUNK_DELAY_S do. A 5 KB message was ONE argv here and
/// six in Python; the limit exists because a TUI's input buffer does not take an arbitrary
/// length in one keystroke run.
#[test]
fn a_long_message_is_sent_in_chunks() {
    let _serial = serial();
    let dir = tempdir();
    let fake = fake_tmux(&dir.path().to_path_buf(), "", 0);
    let mut backend = TmuxBackend::new(fake);
    let message = "x".repeat(2000); // 3 chunks of <= 900
    backend.send_text("%2", &message).expect("typed");
    let calls = fs::read_to_string(dir.path().join("argv.txt")).unwrap();
    let literals = calls.lines().filter(|line| line.contains("-l --")).count();
    assert_eq!(
        3, literals,
        "2000 chars at 900 per chunk is 3 argv calls: {calls}"
    );
    for line in calls.lines().filter(|line| line.contains("-l --")) {
        let payload = line.split("-l -- ").nth(1).unwrap_or("");
        assert!(
            payload.chars().count() <= 900,
            "a chunk is at most 900 chars: {line}"
        );
    }
}

/// t994 finding 1 (the splitter itself): no chunk may cut a CRLF pair in half, and empty
/// text produces no chunks at all (the Python's generator yields nothing).
#[test]
fn chunks_never_split_a_crlf_pair_and_empty_text_yields_none() {
    let _serial = serial();
    let text = format!("{}\r\nabc", "y".repeat(899)); // the CRLF lands on the boundary
    let chunks = ds_core::terminal::tmux::text_chunks(&text, 900);
    assert_eq!(2, chunks.len(), "{chunks:?}");
    assert!(
        !chunks[0].ends_with('\r'),
        "the CR was moved to the next chunk: {chunks:?}"
    );
    assert!(chunks[1].starts_with("\r\n"), "{chunks:?}");
    assert!(ds_core::terminal::tmux::text_chunks("", 900).is_empty());
}

/// t994 finding 2: the text SETTLES before Enter (SEND_TEXT_SETTLE_DELAY_S = 0.2 s).
/// Timing is measured through the fake tmux, which stamps each invocation.
#[test]
fn enter_waits_for_the_text_to_settle() {
    let _serial = serial();
    let dir = tempdir();
    let script = dir.path().join("tmux");
    let log = dir.path().join("stamps.txt");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nprintf '%s %s\\n' \"$(date +%s%N)\" \"$*\" >> {log}\nexit 0\n",
            log = log.display()
        ),
    )
    .expect("script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).expect("stat").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
    }
    let mut backend = TmuxBackend::new(script);
    backend.send_text_and_enter("%2", "short").expect("sent");
    let stamps: Vec<String> = std::fs::read_to_string(&log)
        .expect("stamps")
        .lines()
        .map(str::to_string)
        .collect();
    assert_eq!(
        2,
        stamps.len(),
        "one literal send and one Enter: {stamps:?}"
    );
    let nanos = |line: &str| {
        line.split_whitespace()
            .next()
            .unwrap_or("0")
            .parse::<u128>()
            .unwrap_or(0)
    };
    let gap_ms = (nanos(&stamps[1]).saturating_sub(nanos(&stamps[0]))) / 1_000_000;
    assert!(
        gap_ms >= 150,
        "Enter came {gap_ms} ms after the text; the settle is 200 ms"
    );
}

/// t994 finding 3: read_pane strips leading newlines, as the Python's `out.lstrip("\\n")`.
#[test]
fn read_pane_strips_leading_newlines() {
    let _serial = serial();
    let dir = tempdir();
    let listing = pane_line("%2", "$2", "@2", "bash", "2202");
    let script = dir.path().join("tmux2");
    // One script that answers list-panes with the pane, capture-pane with a blank first line.
    std::fs::write(&script, format!(
        "#!/bin/sh\ncase \"$1\" in\n  list-panes) printf '%s\\n' '{listing}' ;;\n  capture-pane) printf '\\n\\nhello\\n' ;;\n  *) exit 0 ;;\nesac\n"))
        .expect("script");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = std::fs::metadata(&script).expect("stat").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");
    }
    let backend = TmuxBackend::new(script);
    let screen = backend
        .read_pane("%2", 40, false)
        .expect("read")
        .expect("a screen");
    assert!(
        screen.starts_with("hello"),
        "leading newlines are stripped: {screen:?}"
    );
}

/// t994 finding 4: an empty target is refused and a missing pane has NO status.
#[test]
fn an_empty_target_is_refused_and_a_missing_pane_has_no_status() {
    let _serial = serial();
    let dir = tempdir();
    // A fake that serves the canned pane, so "%2" EXISTS and "%404" does not.
    let listing = pane_line("%2", "$2", "@2", "bash", "2202");
    let fake = fake_tmux(&dir.path().to_path_buf(), &listing, 0);
    let mut backend = TmuxBackend::new(fake);
    let error = backend.send_text("", "hello").unwrap_err();
    assert_eq!("No target", error.to_string());
    assert_eq!(
        Some("unknown".to_string()),
        backend.agent_status("%2"),
        "a live pane answers the word rather than a guess"
    );
    assert_eq!(
        None,
        backend.agent_status("%404"),
        "a missing pane is not 'unknown'"
    );
}

#[test]
fn a_failing_tmux_is_a_refusal_with_tmux_s_own_message() {
    let _serial = serial();
    let dir = tempdir();
    let fake = fake_tmux(&dir.path().to_path_buf(), "can't find pane: %9", 1);
    let mut backend = TmuxBackend::new(fake);
    let error = backend.run_command("%9", "true").unwrap_err();
    assert!(error.to_string().contains("can't find pane: %9"), "{error}");
    assert!(!error.backend_unavailable);
}

#[test]
fn a_missing_tmux_binary_is_unavailable_not_a_refusal() {
    let _serial = serial();
    let mut backend = TmuxBackend::new("/nonexistent/tmux-for-tests");
    let error = backend.run_command("%2", "true").unwrap_err();
    assert!(error.backend_unavailable, "{error}");
    assert!(error.to_string().contains("tmux-for-tests"));
}

#[test]
fn capture_pane_asks_for_the_ansi_and_line_flags() {
    let _serial = serial();
    let dir = tempdir();
    let fake = fake_tmux(&dir.path().to_path_buf(), "screen text", 0);
    let backend = TmuxBackend::new(fake);
    assert_eq!("screen text\n", backend.capture("%2", 50, true).unwrap());
    let calls = fs::read_to_string(dir.path().join("argv.txt")).unwrap();
    assert!(calls.contains("capture-pane -p -e -S -50 -t %2"), "{calls}");
}

#[test]
fn agent_child_pids_says_it_needs_proc_rs_rather_than_answering_empty() {
    let _serial = serial();
    // The Python delegates to dunk_callers.pane_agent_pids (T03). An empty Vec here
    // would read as "no agent behind this pane", which is a different claim.
    let dir = tempdir();
    let fake = fake_tmux(&dir.path().to_path_buf(), "", 0);
    let backend = TmuxBackend::new(fake);
    let error = backend.agent_child_pids(2202).unwrap_err();
    assert!(error.to_string().contains("T03"), "{error}");
}

#[test]
fn shell_join_quotes_only_what_needs_it() {
    let _serial = serial();
    assert_eq!("pi --continue", shell_join(&["pi", "--continue"]));
    assert_eq!("a 'b c'", shell_join(&["a", "b c"]));
    assert_eq!("'it'\\''s'", shell_join(&["it's"]));
}

// -- resize (t928): in T22's own scope, and argv-tested like everything else -----

/// A fake tmux whose pane listing CHANGES after `resize-window` is called, so a test
/// can assert that the reported geometry is the one READ BACK, not the request.
fn fake_tmux_that_resizes(dir: &PathBuf, before: &str, after: &str) -> PathBuf {
    let script = dir.join("tmux");
    let log = dir.join("argv.txt");
    let listing = dir.join("listing.txt");
    fs::write(&listing, format!("{before}\n")).expect("write listing");
    let body = format!(
        "#!/bin/sh\n\
         printf '%s\\n' \"$*\" >> {log}\n\
         case \"$*\" in\n\
           *resize-window*) printf '%s\\n' \"{after}\" > {listing} ;;\n\
         esac\n\
         case \"$*\" in\n\
           *list-panes*) cat {listing} ;;\n\
         esac\n\
         exit 0\n",
        log = log.display(),
        listing = listing.display(),
        after = after
    );
    fs::write(&script, body).expect("write fake tmux");
    let mut permissions = fs::metadata(&script).expect("stat").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).expect("chmod");
    script
}

#[test]
fn resize_sends_the_python_argv_and_reports_the_size_read_back() {
    let _serial = serial();
    let dir = tempdir();
    let before = pane_line("%2", "$2", "@2", "claude", "2202");
    let after = pane_line("%2", "$2", "@2", "claude", "2202")
        .replace(&["120", "40"].join(SEP), &["90", "30"].join(SEP));
    let fake = fake_tmux_that_resizes(&dir.path().to_path_buf(), &before, &after);
    let mut backend = TmuxBackend::new(fake);

    let resize = backend.resize("%2", 90, 30).expect("resize");
    assert_eq!("%2", resize.pane_id);
    assert_eq!((90, 30), (resize.requested_cols, resize.requested_rows));
    // READ BACK, not echoed: the fake changed the listing only after resize-window.
    assert_eq!(Some(90), resize.cols);
    assert_eq!(Some(30), resize.rows);
    assert_eq!(1, resize.window_pane_count);
    assert_eq!(1, resize.resized_panes.len());
    assert_eq!(Some(120), resize.resized_panes[0].cols_before);
    assert_eq!(Some(40), resize.resized_panes[0].rows_before);
    assert_eq!(0, resize.clients_attached);
    assert!(!resize.overridable_by_client);

    let argv = fs::read_to_string(dir.path().join("argv.txt")).unwrap();
    let resize_line = argv
        .lines()
        .find(|line| line.contains("resize-window"))
        .unwrap_or_else(|| panic!("no resize-window in argv: {argv}"));
    // The exact argv the Python sends: the pane first, then -x cols -y rows.
    assert!(
        resize_line.contains("resize-window -t %2 -x 90 -y 30"),
        "unexpected argv: {resize_line}"
    );
}

#[test]
fn resize_refuses_out_of_bounds_before_touching_tmux() {
    let _serial = serial();
    let dir = tempdir();
    let listing = pane_line("%2", "$2", "@2", "claude", "2202");
    let fake = fake_tmux_that_resizes(&dir.path().to_path_buf(), &listing, &listing);
    let mut backend = TmuxBackend::new(fake);

    for (cols, rows) in [(0, 30), (30, 0), (1001, 30), (30, 1001)] {
        let error = backend.resize("%2", cols, rows).expect_err("must refuse");
        assert!(
            error.message.contains("between 1 and 1000"),
            "{}",
            error.message
        );
        assert!(
            !error.backend_unavailable,
            "a bounds refusal is not an outage"
        );
    }
    let argv = fs::read_to_string(dir.path().join("argv.txt")).unwrap_or_default();
    assert!(
        !argv.contains("resize-window"),
        "a refused size must not reach tmux at all: {argv}"
    );
}

#[test]
fn resize_names_a_missing_pane() {
    let _serial = serial();
    let dir = tempdir();
    let listing = pane_line("%2", "$2", "@2", "claude", "2202");
    let fake = fake_tmux_that_resizes(&dir.path().to_path_buf(), &listing, &listing);
    let mut backend = TmuxBackend::new(fake);
    let error = backend.resize("%99", 80, 24).expect_err("no such pane");
    assert_eq!("no such pane %99", error.message);
    let argv = fs::read_to_string(dir.path().join("argv.txt")).unwrap_or_default();
    assert!(!argv.contains("resize-window"), "nothing to resize: {argv}");
}

#[test]
fn a_refused_resize_reports_tmux_words_and_no_geometry() {
    let _serial = serial();
    let dir = tempdir();
    // The listing stays the same; resize-window fails with tmux's own words.
    let script = dir.path().join("tmux");
    let log = dir.path().join("argv.txt");
    let listing = pane_line("%2", "$2", "@2", "claude", "2202");
    let body = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {log}\n\
         case \"$*\" in\n  *resize-window*) echo 'width too large' >&2; exit 1 ;;\n\
         esac\n\
         case \"$*\" in\n  *list-panes*) echo '{listing}' ;;\n         esac\nexit 0\n",
        log = log.display()
    );
    fs::write(&script, body).unwrap();
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&script, permissions).unwrap();
    let mut backend = TmuxBackend::new(script);
    let error = backend.resize("%2", 999, 999).expect_err("tmux refused");
    assert_eq!("width too large", error.message);
}
