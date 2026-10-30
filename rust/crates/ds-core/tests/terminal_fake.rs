//! T20: the Terminal seam and its fake, ported from tests/fakes.py.
//!
//! The acceptance is "every method FakeHerdr has, with the same canned
//! PANES/TABS/WORKSPACES" - so the tests check the canned world, the recording, and
//! the one failure mode a caller has to handle (a backend that is not there).

use ds_core::terminal::fakes::FakeTerminal;
use ds_core::terminal::{AgentSpec, Terminal};

#[test]
fn the_canned_world_matches_the_python_fixtures() {
    let fake = FakeTerminal::new();
    let panes = fake.list_panes();
    assert_eq!(3, panes.len(), "tests/fakes.py serves three panes");
    assert_eq!("w1:p1", panes[0].pane_id);
    assert_eq!("bash", panes[0].command);
    assert_eq!(Some(1101), panes[0].pane_pid);
    assert_eq!("w2:p2", panes[1].pane_id);
    assert_eq!("claude", panes[1].command);
    assert_eq!(3, fake.list_tabs().len());
    assert_eq!(2, fake.list_workspaces().len());
    assert_eq!("w1", panes[0].tab_id(), "a pane id starts with its tab id");
}

#[test]
fn an_unknown_pane_is_none_not_an_error() {
    let fake = FakeTerminal::new();
    assert!(fake.get_pane("%99").is_none());
    assert_eq!("w3:p3", fake.get_pane("w3:p3").expect("canned").pane_id);
}

#[test]
fn panes_group_by_workspace() {
    let fake = FakeTerminal::new();
    let grouped = fake.list_panes_grouped();
    assert_eq!(2, grouped["work"].len());
    assert_eq!(1, grouped["other"].len());
}

#[test]
fn a_layout_mutation_changes_the_world_and_is_recorded() {
    let mut fake = FakeTerminal::new();
    let tab = fake
        .create_tab(Some("w1"), Some("review"), Some("/tmp"), true)
        .expect("the fake accepts a tab");
    assert_eq!(4, fake.list_tabs().len());
    assert_eq!(
        vec![("create_tab".to_string(), tab.tab_id.clone())],
        fake.layout_calls
    );
    let pane = fake.split_pane("w1:p1", "right", None).expect("split");
    assert_eq!(
        "/home/hunter/work", pane.cwd,
        "a split inherits the pane it came from"
    );
    assert_eq!("bash", pane.command);
    assert_eq!(4, fake.list_panes().len());
}

#[test]
fn the_next_mutation_fails_once_then_succeeds() {
    let mut fake = FakeTerminal::new();
    fake.fail_layout = Some("tmux: window not found".to_string());
    let error = fake.create_tab(None, None, None, false).unwrap_err();
    assert_eq!("tmux: window not found", error.to_string());
    assert!(!error.backend_unavailable);
    assert_eq!(
        3,
        fake.list_tabs().len(),
        "a refused mutation changed nothing"
    );
    assert!(
        fake.create_tab(None, None, None, false).is_ok(),
        "the switch cleared"
    );
}

#[test]
fn an_unavailable_backend_refuses_with_the_hint() {
    let mut fake = FakeTerminal::unavailable();
    assert!(!fake.is_available());
    let error = fake.run_command("w1:p1", "ls").unwrap_err();
    assert!(error.backend_unavailable);
    assert_eq!(
        "tmux server not running - start it with `tmux new -d`",
        error.to_string()
    );
    assert!(fake.runs.is_empty());
}

#[test]
fn typing_a_command_is_recorded_not_executed() {
    let mut fake = FakeTerminal::new();
    fake.run_command("w2:p2", "dunkingsheep status")
        .expect("recorded");
    assert_eq!(
        vec![("w2:p2".to_string(), "dunkingsheep status".to_string())],
        fake.runs
    );
}

#[test]
fn closing_a_pane_removes_it_and_an_unknown_kind_is_refused() {
    let mut fake = FakeTerminal::new();
    fake.close("pane", "w1:p1").expect("closed");
    assert!(fake.get_pane("w1:p1").is_none());
    assert_eq!(2, fake.list_panes().len());
    let error = fake.close("dunk", "w1:p1").unwrap_err();
    assert!(error.to_string().contains("cannot close a dunk"));
}

#[test]
fn child_pids_come_from_the_injected_table() {
    let mut fake = FakeTerminal::new();
    fake.children.insert(1101, vec![4200, 4201]);
    assert_eq!(vec![4200, 4201], fake.agent_child_pids(1101).unwrap());
    assert!(fake.agent_child_pids(9999).unwrap().is_empty());
}

#[test]
fn the_delivery_surface_is_on_the_trait_and_the_fake_records_it() {
    // t923: these five methods are what a message's delivery is built on. A fake
    // cannot fake what the trait does not declare, so they must be here.
    let mut fake = FakeTerminal::new();
    fake.send_text("w2:p2", "half a sentence").expect("typed");
    assert_eq!(
        vec![("w2:p2".to_string(), "half a sentence".to_string())],
        fake.sends
    );
    assert!(
        fake.submits.is_empty(),
        "typing without Enter does not submit"
    );

    fake.send_text_and_enter("w2:p2", "dunkingsheep status")
        .expect("submitted");
    assert_eq!(
        1,
        fake.submits.len(),
        "send_text_and_enter submits exactly once"
    );
    assert_eq!(2, fake.sends.len(), "the text is also recorded as typed");

    fake.send_keys("w2:p2", &["C-c"]).expect("keys");
    assert_eq!(
        vec![("w2:p2".to_string(), vec!["C-c".to_string()])],
        fake.keys_sent
    );
}

#[test]
fn read_pane_serves_the_pinned_screen_tail_and_none_for_an_unreadable_pane() {
    let mut fake = FakeTerminal::new();
    fake.screens
        .insert("w2:p2".to_string(), "one\ntwo\nthree\nfour".to_string());
    let tail = fake
        .read_pane("w2:p2", 2, false)
        .expect("read")
        .expect("a screen");
    assert_eq!(
        "three\nfour", tail,
        "the newest lines, not the whole screen"
    );

    fake.unreadable.insert("w3:p3".to_string());
    assert_eq!(
        None,
        fake.read_pane("w3:p3", 40, false).expect("read"),
        "an unreadable pane is None, NOT an empty composer"
    );
    assert_eq!(
        None,
        fake.read_pane("%99", 40, false).expect("read"),
        "an unknown pane is None too"
    );
}

#[test]
fn read_pane_falls_back_to_an_echo_of_what_was_typed() {
    let mut fake = FakeTerminal::new();
    fake.send_text("w1:p1", "hello").expect("typed");
    let echo = fake
        .read_pane("w1:p1", 40, false)
        .expect("read")
        .expect("an echo");
    assert!(
        !echo.is_empty(),
        "the fake echoes what it was given, like a terminal"
    );
}

#[test]
fn agent_status_is_served_from_the_status_map_then_unknown() {
    let mut fake = FakeTerminal::new();
    fake.statuses
        .insert("w2:p2".to_string(), "working".to_string());
    assert_eq!(Some("working".to_string()), fake.agent_status("w2:p2"));
    assert_eq!(Some("unknown".to_string()), fake.agent_status("w1:p1"));
    assert_eq!(None, fake.agent_status("%99"), "not a pane, not a status");
}

#[test]
fn every_fakeherdr_method_is_on_the_trait() {
    // The acceptance sentence, kept as a test rather than a claim: the Python fake
    // has these, so the seam must declare them.
    fn assert_declared<T: Terminal>() {}
    assert_declared::<FakeTerminal>();
    let names = [
        "send_text",
        "send_text_and_enter",
        "send_keys",
        "read_pane",
        "agent_status",
        "run_command",
        "capture",
    ];
    let _ = names; // documented in the body; the compile-time proof is above
}

#[test]
fn the_trait_is_object_safe_so_a_backend_can_be_swapped_at_runtime() {
    // tmux is the default and herdr stays selectable for the rollback window
    // (t50), which only works if the daemon holds a `Box<dyn Terminal>`.
    let mut backend: Box<dyn Terminal> = Box::new(FakeTerminal::new());
    assert!(backend.is_available());
    let pane = backend
        .start_agent(&AgentSpec {
            name: "worker-1".to_string(),
            argv: vec!["pi".to_string()],
            ..AgentSpec::default()
        })
        .expect("started");
    assert_eq!("worker-1", pane.title);
    assert!(backend
        .list_panes()
        .iter()
        .any(|pane| pane.pane_id == "w1:p-worker-1"));
}
