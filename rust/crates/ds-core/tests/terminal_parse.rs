//! T21: parsing the exact `list-panes`/`list-windows`/`list-sessions` lines (t21).
//!
//! The fixtures are the shapes a live tmux emits, including the two that broke the
//! Python: a BLANK session_index (tmux leaves it empty when the session has no index
//! set) and a 12-field line from an older capture with no geometry. A line whose field
//! count is wrong refuses the whole listing; it is never read with shifted fields.

use ds_core::terminal::parse::{
    agent_kind, pane_separator, parse_panes, parse_tabs, parse_workspaces, safe_name,
};
use ds_core::terminal::Pane;

fn line(fields: &[&str]) -> String {
    fields.join(pane_separator())
}

fn no_registry(_pane: &str) -> Option<String> {
    None
}

/// A listing that must parse. The refusals are asserted on the `Err` directly.
fn parsed(output: &str, label_of: &dyn Fn(&str) -> Option<String>) -> Vec<Pane> {
    parse_panes(output, label_of).expect("the listing parses")
}

/// A full 14-field pane line, as tmux emits it with PANE_FORMAT.
fn full_line(
    pane: &str,
    session: &str,
    window: &str,
    command: &str,
    title: &str,
    session_index: &str,
    pid: &str,
) -> String {
    line(&[
        pane,
        session,
        window,
        "/home/hunter/work",
        command,
        title,
        "1",
        "work",
        "1",
        "2",
        session_index,
        pid,
        "120",
        "40",
    ])
}

#[test]
fn a_full_line_parses_every_field() {
    let output = full_line("%2", "$2", "@2", "claude", "pi - review", "1", "2202");
    let panes = parsed(&output, &no_registry);
    assert_eq!(1, panes.len());
    let pane = &panes[0];
    assert_eq!("%2", pane.pane_id);
    assert_eq!("$2", pane.session_id);
    assert_eq!("@2", pane.window_id);
    assert_eq!("/home/hunter/work", pane.cwd);
    assert_eq!(
        "claude", pane.command,
        "the agent kind replaces the command"
    );
    assert_eq!("pi - review", pane.title, "tmux's own title is kept");
    assert_eq!(
        "pi - review", pane.label,
        "no registry name, so the title is the label"
    );
    assert!(pane.active);
    assert_eq!("work", pane.workspace);
    assert_eq!("1", pane.tab);
    assert_eq!(2, pane.window_index);
    assert_eq!(1, pane.workspace_index);
    assert_eq!(Some(2202), pane.pane_pid);
    assert_eq!(120, pane.width);
    assert_eq!(40, pane.height);
    assert_eq!("%2", pane.tab_id());
}

#[test]
fn a_blank_session_index_does_not_null_the_pane_pid() {
    // The 2026-10-24 defect: one try/except around every int() meant a blank
    // session_index raised first and pane_pid was nulled with it.
    let output = full_line("%2", "$2", "@2", "bash", "shell", "", "2202");
    let pane = &parsed(&output, &no_registry)[0];
    assert_eq!(
        0, pane.workspace_index,
        "a blank index is 0, its own absence"
    );
    assert_eq!(Some(2202), pane.pane_pid, "the pid is parsed on its own");
}

#[test]
fn a_twelve_field_line_refuses_the_listing() {
    // The format has fourteen fields. A twelve-field line is an older capture: refused, not
    // guessed at, so no pane is read with a size or a pid taken from another field.
    let output = line(&[
        "%1", "$1", "@1", "/tmp", "bash", "t", "0", "work", "1", "1", "1", "1101",
    ]);
    let error = parse_panes(&output, &no_registry).expect_err("a twelve-field line is refused");
    assert_eq!((1, 12, 14), (error.line, error.fields, error.expected));
}

#[test]
fn a_short_line_refuses_the_listing() {
    let output = line(&["%1", "$1", "@1", "/tmp", "bash"]);
    let error = parse_panes(&output, &no_registry).expect_err("a short line is refused");
    assert_eq!((1, 5), (error.line, error.fields));
}

#[test]
fn an_empty_listing_is_empty() {
    assert!(parsed("", &no_registry).is_empty());
    assert!(parsed("\n", &no_registry).is_empty());
}

#[test]
fn a_blank_line_between_panes_is_skipped() {
    let output = [
        full_line("%1", "$1", "@1", "bash", "one", "1", "1101"),
        String::new(),
        full_line("%2", "$2", "@2", "bash", "two", "1", "1102"),
    ]
    .join("\n");
    assert_eq!(2, parsed(&output, &no_registry).len());
}

#[test]
fn a_title_that_carries_the_separator_refuses_the_whole_listing() {
    // A title with the token adds a field. The line after it cannot be trusted, and neither
    // can the good line before it, so the listing is refused whole, not half read.
    let carrying = format!("pi{}review", pane_separator());
    let output = [
        full_line("%1", "$1", "@1", "bash", "one", "1", "1101"),
        full_line("%2", "$2", "@2", "claude", &carrying, "1", "2202"),
    ]
    .join("\n");
    let error = parse_panes(&output, &no_registry).expect_err("a carrying title is refused");
    assert_eq!((2, 15, 14), (error.line, error.fields, error.expected));
}

#[test]
fn a_raw_unit_separator_is_never_a_separator() {
    // tmux 3.4 escapes a control character in format output, so the old raw 0x1f can arrive
    // as the text `\037` and split nothing. The token is what splits; a 0x1f line is one field.
    let raw = [
        "%1", "$1", "@1", "/tmp", "bash", "t", "0", "work", "1", "1", "1", "1101", "120", "40",
    ]
    .join("\u{1f}");
    let error = parse_panes(&raw, &no_registry).expect_err("a raw 0x1f line is refused");
    assert_eq!((1, 1, 14), (error.line, error.fields, error.expected));
}

#[test]
fn the_separator_is_printable_and_stable_within_a_process() {
    let separator = pane_separator();
    assert_eq!(24, separator.len());
    assert!(separator.bytes().all(|byte| byte.is_ascii_alphanumeric()));
    assert_eq!(separator, pane_separator());
}

#[test]
fn the_registry_label_wins_over_the_title() {
    let output = full_line("%2", "$2", "@2", "bash", "some title", "1", "2202");
    let label = |pane: &str| {
        if pane == "%2" {
            Some("worker-4".to_string())
        } else {
            None
        }
    };
    let pane = &parsed(&output, &label)[0];
    assert_eq!(
        "worker-4", pane.label,
        "the recorded start_agent name is the stable label"
    );
}

#[test]
fn agent_kind_prefers_the_command_then_a_whole_word_in_the_title() {
    assert_eq!("pi", agent_kind("/usr/local/bin/pi", "anything"));
    assert_eq!("claude", agent_kind("claude", ""));
    assert_eq!("codex", agent_kind("bash", "π - codex review"));
    assert_eq!("", agent_kind("bash", "π - editor"));
    // Whole-word only: `pilot` is not `pi`, and `claudette` is not `claude`.
    assert_eq!("", agent_kind("bash", "pilot claudette"));
    // Longest kind first: `mastracode` must win over `code`-prefixed kinds.
    assert_eq!("mastracode", agent_kind("bash", "mastracode lane"));
}

#[test]
fn safe_name_strips_what_tmux_cannot_hold() {
    assert_eq!("worker-4", safe_name("worker-4", "dunkingsheep"));
    assert_eq!("a-b-c-d", safe_name("a b:c.d", "dunkingsheep"));
    assert_eq!("dunkingsheep", safe_name("   ", "dunkingsheep"));
    assert_eq!(60, safe_name(&"x".repeat(120), "dunkingsheep").len());
}

#[test]
fn tabs_and_workspaces_parse_from_their_own_formats() {
    let tabs = parse_tabs(&line(&["@2", "$2", "review", "2", "1"])).expect("tabs parse");
    assert_eq!(1, tabs.len());
    assert_eq!("@2", tabs[0].tab_id);
    assert_eq!("review", tabs[0].label);
    assert_eq!(2, tabs[0].index);
    assert!(tabs[0].active);

    let workspaces = parse_workspaces(&line(&["$2", "work", "1"])).expect("workspaces parse");
    assert_eq!(1, workspaces.len());
    assert_eq!("work", workspaces[0].label);
    assert_eq!(1, workspaces[0].index);
}

#[test]
fn a_short_tab_or_workspace_line_refuses_its_listing() {
    let tab_error = parse_tabs(&line(&["@2", "$2", "review", "2"])).expect_err("refused");
    assert_eq!(
        (1, 4, 5),
        (tab_error.line, tab_error.fields, tab_error.expected)
    );
    let workspace_error = parse_workspaces(&line(&["$2", "work"])).expect_err("refused");
    assert_eq!(
        (1, 2, 3),
        (
            workspace_error.line,
            workspace_error.fields,
            workspace_error.expected
        )
    );
}

#[test]
fn multiple_lines_are_parsed_independently() {
    let output = [
        full_line("%1", "$1", "@1", "bash", "one", "1", "1101"),
        full_line("%2", "$2", "@2", "claude", "two", "", ""),
    ]
    .join("\n");
    let panes = parsed(&output, &no_registry);
    assert_eq!(2, panes.len());
    assert_eq!(Some(1101), panes[0].pane_pid);
    assert_eq!(None, panes[1].pane_pid, "an empty pid is None, not 0");
}
