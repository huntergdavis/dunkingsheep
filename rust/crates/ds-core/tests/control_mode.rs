//! T23: tmux control mode, against a recorded `%begin/%end/%output` transcript.
//!
//! Nothing here spawns a process: `Client::handle_line` takes one line, which is
//! exactly what a pty driver feeds it. The transcript below is the shape a real
//! `tmux -C` session produces, including the parts that surprise a first
//! implementation: the reply number is echoed back, body lines arrive BETWEEN begin
//! and end, a second command can be answered before the first, and `%exit` means the
//! numbers in flight will never arrive.

use ds_core::terminal::control::{parse_line, Client, Event, PaneTable};

const SEP: &str = "\x1f";

fn pane_line(pane: &str, session: &str, window: &str, command: &str, pid: &str) -> String {
    [pane, session, window, "/home/hunter/work", command, "title", "1", "work", "1",
     "2", "1", pid, "120", "40"].join(SEP)
}

#[test]
fn every_protocol_line_parses_to_its_event() {
    assert_eq!(Event::Begin { number: 7 }, parse_line("%begin 1793347200 7 1"));
    assert_eq!(Event::End { number: 7, flags: String::new() },
               parse_line("%end 1793347200 7"));
    assert_eq!(Event::Error { number: 9 }, parse_line("%error 1793347200 9"));
    assert_eq!(Event::Output { pane_id: "%2".to_string(), text: "hi\\015".to_string() },
               parse_line("%output %2 hi\\015"));
    assert_eq!(Event::LayoutChange { window_id: "@2".to_string() },
               parse_line("%layout-change @2 b25d,120x40,0,0,1 0"));
    assert_eq!(Event::WindowAdd { window_id: "@3".to_string() }, parse_line("%window-add @3"));
    assert_eq!(Event::WindowClose { window_id: "@3".to_string() },
               parse_line("%window-close @3"));
    assert_eq!(Event::SessionChanged { session_id: "$2".to_string(),
                                       name: "work".to_string() },
               parse_line("%session-changed $2 work"));
    assert_eq!(Event::Exit { reason: String::new() }, parse_line("%exit"));
    assert_eq!(Event::Exit { reason: "server exited".to_string() },
               parse_line("%exit server exited"));
    // A CRLF must not defeat the match (a pty on some paths keeps it).
    assert_eq!(Event::Begin { number: 1 }, parse_line("%begin 1 1\r"));
    // An event tmux adds later is VISIBLE, not silently swallowed.
    assert_eq!(Event::Unknown("%pane-mode-changed %2".to_string()),
               parse_line("%pane-mode-changed %2"));
}

#[test]
fn a_reply_is_the_lines_between_begin_and_end() {
    let mut client = Client::new();
    let number = client.send(&Client::refresh_command());
    assert_eq!(1, number, "the first command is numbered 1");
    assert!(client.handle_line(&format!("%begin 1793347200 {number} 1")).is_none());
    assert!(client.handle_line(&pane_line("%2", "$2", "@2", "claude", "2202")).is_none());
    let reply = client.handle_line(&format!("%end 1793347200 {number}"))
        .expect("the reply completed");
    assert_eq!(number, reply.number);
    assert!(!reply.failed);
    assert!(reply.text().contains("%2"), "the body is the command output: {reply:?}");
    assert_eq!(0, client.replies.in_flight());
}

#[test]
fn a_reply_loads_the_pane_table_and_get_pane_is_a_lookup() {
    let mut client = Client::new();
    let number = client.send(&Client::refresh_command());
    client.handle_line(&format!("%begin 1 {number} 1"));
    client.handle_line(&pane_line("%2", "$2", "@2", "claude", "2202"));
    let reply = client.handle_line(&format!("%end 1 {number}")).expect("reply");
    assert_eq!(1, client.table.load_panes(&reply.text()));
    let pane = client.table.get_pane("%2").expect("in the table");
    assert_eq!("claude", pane.command);
    assert_eq!(Some(2202), pane.pane_pid);
    assert!(!client.table.stale, "a fresh refresh clears the stale flag");
}

#[test]
fn two_commands_can_be_answered_out_of_order() {
    let mut client = Client::new();
    let first = client.send("list-windows");
    let second = client.send("list-sessions");
    assert_eq!((1, 2), (first, second));
    client.handle_line(&format!("%begin 1 {second} 1"));
    client.handle_line("$2" .to_string().as_str());
    let reply = client.handle_line(&format!("%end 1 {second}")).expect("second first");
    assert_eq!(second, reply.number);
    assert_eq!(1, client.replies.in_flight(), "the first is still waiting");
    client.handle_line(&format!("%begin 1 {first} 1"));
    let reply = client.handle_line(&format!("%end 1 {first}")).expect("first second");
    assert_eq!(first, reply.number);
}

#[test]
fn a_failing_command_is_marked_failed_not_dropped() {
    let mut client = Client::new();
    let number = client.send("kill-pane -t %9");
    client.handle_line(&format!("%begin 1 {number} 1"));
    client.handle_line("can't find pane: %9");
    client.handle_line(&format!("%error 1 {number}"));
    let reply = client.handle_line(&format!("%end 1 {number}")).expect("end closes it");
    assert!(reply.failed, "the %error marked it failed");
    assert!(reply.text().contains("can't find pane"), "{reply:?}");
}

#[test]
fn a_world_event_marks_the_table_stale() {
    let mut merged = PaneTable::default();
    assert!(merged.apply(&Event::LayoutChange { window_id: "@2".to_string() }));
    assert!(merged.stale);
    assert!(!merged.apply(&Event::Output { pane_id: "%2".to_string(),
                                           text: "x".to_string() }),
            "pane output does not move the layout");
}

#[test]
fn exit_leaves_nothing_in_flight_and_the_client_disconnected() {
    let mut client = Client::new();
    let number = client.send("list-panes -a");
    client.handle_line(&format!("%begin 1 {number} 1"));
    assert_eq!(1, client.replies.in_flight());
    client.handle_line("%exit server exited");
    assert!(!client.connected);
    assert_eq!(0, client.replies.in_flight(), "a dead server answers nothing");
    assert!(client.table.stale, "the world is unknown, not empty");
}

#[test]
fn a_reconnect_starts_over_with_a_stale_table() {
    let mut client = Client::new();
    client.handle_line("%exit");
    client.on_reconnect();
    assert!(client.connected);
    assert!(client.table.stale);
    let number = client.send("list-panes -a");
    assert_eq!(1, number, "numbers restart on a fresh client");
}

#[test]
fn the_refresh_command_is_the_python_format() {
    let command = Client::refresh_command();
    assert!(command.starts_with("list-panes -a -F "), "{command}");
    assert!(command.contains("#{pane_id}") && command.ends_with("#{pane_height}"));
    assert_eq!(format!("{command}\n"), Client::line_for(&command),
               "control mode reads one newline-terminated command per line");
}
