//! T21: parsing the exact `list-panes`/`list-windows`/`list-sessions` lines (t21).
//!
//! The fixtures are the shapes a live tmux emits, including the two that broke the
//! Python: a BLANK session_index (tmux leaves it empty when the session has no index
//! set) and a 12-field line from an older capture with no geometry.

use ds_core::terminal::parse::{
    agent_kind, parse_panes, parse_tabs, parse_workspaces, safe_name, PANE_SEP,
};

fn line(fields: &[&str]) -> String {
    fields.join(&PANE_SEP.to_string())
}

fn no_registry(_pane: &str) -> Option<String> {
    None
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
    let panes = parse_panes(&output, &no_registry);
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
    assert_eq!(Some(2), pane.window_index);
    assert_eq!(Some(1), pane.workspace_index);
    assert_eq!(Some(2202), pane.pane_pid);
    assert_eq!(Some(120), pane.width);
    assert_eq!(Some(40), pane.height);
    assert_eq!("%2", pane.tab_id());
}

#[test]
fn a_blank_session_index_does_not_null_the_pane_pid() {
    // The 2026-10-24 defect: one try/except around every int() meant a blank
    // session_index raised first and pane_pid was nulled with it.
    let output = full_line("%2", "$2", "@2", "bash", "shell", "", "2202");
    let pane = &parse_panes(&output, &no_registry)[0];
    assert_eq!(
        None, pane.workspace_index,
        "t943: a blank index is ABSENT, not 0 - 0 is a guess"
    );
    assert_eq!(Some(2202), pane.pane_pid, "the pid is parsed on its own");
}

#[test]
fn a_twelve_field_line_parses_with_no_geometry() {
    let output = line(&[
        "%1", "$1", "@1", "/tmp", "bash", "t", "0", "work", "1", "1", "1", "1101",
    ]);
    let pane = &parse_panes(&output, &no_registry)[0];
    assert_eq!(
        (None, None),
        (pane.width, pane.height),
        "t943: no size is ABSENT, not zero-by-guess"
    );
    assert!(!pane.active);
    assert_eq!(Some(1101), pane.pane_pid);
}

#[test]
fn a_short_line_is_skipped_not_guessed() {
    let output = line(&["%1", "$1", "@1", "/tmp", "bash"]);
    assert!(parse_panes(&output, &no_registry).is_empty());
}

#[test]
fn an_empty_or_failed_listing_is_empty() {
    assert!(parse_panes("", &no_registry).is_empty());
    assert!(parse_panes("\n", &no_registry).is_empty());
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
    let pane = &parse_panes(&output, &label)[0];
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
    let tabs = parse_tabs(&line(&["@2", "$2", "review", "2", "1"]));
    assert_eq!(1, tabs.len());
    assert_eq!("@2", tabs[0].tab_id);
    assert_eq!("review", tabs[0].label);
    assert_eq!(2, tabs[0].index);
    assert!(tabs[0].active);

    let workspaces = parse_workspaces(&line(&["$2", "work", "1"]));
    assert_eq!(1, workspaces.len());
    assert_eq!("work", workspaces[0].label);
    assert_eq!(1, workspaces[0].index);
}

#[test]
fn multiple_lines_are_parsed_independently() {
    let output = [
        full_line("%1", "$1", "@1", "bash", "one", "1", "1101"),
        full_line("%2", "$2", "@2", "claude", "two", "", ""),
    ]
    .join("\n");
    let panes = parse_panes(&output, &no_registry);
    assert_eq!(2, panes.len());
    assert_eq!(Some(1101), panes[0].pane_pid);
    assert_eq!(None, panes[1].pane_pid, "an empty pid is None, not 0");
}
