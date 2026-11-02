//! resolve_mailbox behaviour, from the docstring and comments of dunk_core.resolve_mailbox
//! (the 2026-10-19/24/29 incidents). NOT a 1:1 port of the 37 Python call sites; the Python
//! tests that drive it through inbox_send are T34's.

use super::mailbox::*;
use super::Flock;
use crate::error::DunkError;
use crate::flock::lanes::TerminalEnv;
use crate::terminal::fakes::FakeTerminal;
use crate::terminal::TerminalError;
use serde_json::{json, Value};

struct Env {
    panes: Vec<Value>,
    registered: Option<(bool, bool, Option<String>)>,
}

impl MailboxEnv for Env {
    fn list_panes(&self) -> Vec<Value> {
        self.panes.clone()
    }
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.panes.clone()
    }
    fn registered_pane(&self, _: &str) -> (bool, bool, Option<String>) {
        self.registered.clone().unwrap_or((false, false, None))
    }
}

fn pane(id: &str, label: &str, tab: &str, ws: &str, agent: &str) -> Value {
    let mut p = json!({"pane_id": id, "terminal_id": format!("term_{}", id.replace([':', '%'], "")),
        "label": label, "tab_label": tab, "workspace_label": ws});
    if !agent.is_empty() {
        p["agent"] = json!(agent);
    }
    p
}

fn env() -> Env {
    Env {
        panes: vec![
            pane("w1:p1", "shell", "shell", "alpha", ""),
            pane("w1:p2", "poolrad pm", "Poolrad", "alpha", "codex"),
            pane("w2:p1", "worker", "Site", "beta", "codex"),
            pane("w2:p2", "worker", "Site2", "beta", "claude"),
        ],
        registered: None,
    }
}

#[test]
fn terminal_env_keeps_grouped_pane_backend_errors_fallible() {
    let mut terminal = FakeTerminal::new();
    terminal.fail_grouped_panes = Some(TerminalError::refused("offline"));
    let registered = |_: &str| (false, false, None);
    let env = TerminalEnv {
        terminal: &terminal,
        registered: &registered,
    };

    assert!(matches!(
        MailboxEnv::try_list_panes_grouped(&env),
        Err(DunkError::Refused(message)) if message == "offline"
    ));
}

#[test]
fn terminal_env_keeps_a_successful_empty_grouped_listing_empty() {
    let mut terminal = FakeTerminal::new();
    terminal.panes.clear();
    let registered = |_: &str| (false, false, None);
    let env = TerminalEnv {
        terminal: &terminal,
        registered: &registered,
    };

    assert!(MailboxEnv::try_list_panes_grouped(&env).unwrap().is_empty());
}

fn flock(dir: &tempfile::TempDir) -> Flock {
    Flock::new(
        dir.path().join("l.json"),
        dir.path().join("lanes.json"),
        false,
    )
}

const HINT: &str = "no terminal";

fn go(f: &Flock, e: &Env, spec: &str, strict: bool) -> Result<Mailbox, String> {
    f.resolve_mailbox(e, spec, Some("w1:p1"), strict, HINT)
        .map_err(|x| x.to_string())
}

#[test]
fn an_exact_pane_id_resolves_with_its_unique_label_as_the_key() {
    let dir = tempfile::tempdir().unwrap();
    let (f, e) = (flock(&dir), env());
    assert_eq!(
        go(&f, &e, "w1:p2", false).unwrap(),
        (
            "poolrad pm".into(),
            "poolrad pm (w1:p2)".into(),
            Some("w1:p2".into())
        )
    );
    // Two panes share the label 'worker': the key falls back to the unique tab label.
    assert_eq!(
        go(&f, &e, "w2:p1", false).unwrap(),
        ("Site".into(), "worker (w2:p1)".into(), Some("w2:p1".into()))
    );
    // A terminal id is accepted too.
    assert_eq!(
        go(&f, &e, "term_w1p2", false).unwrap().2,
        Some("w1:p2".into())
    );
}

#[test]
fn a_dead_pane_id_reads_permissively_and_refuses_a_send() {
    let dir = tempfile::tempdir().unwrap();
    let (f, e) = (flock(&dir), env());
    assert_eq!(
        go(&f, &e, "w9:p9", false).unwrap(),
        ("w9:p9".into(), "w9:p9".into(), None)
    );
    let error = go(&f, &e, "w9:p9", true).unwrap_err();
    assert!(error.contains("reason=dead_lane"), "{error}");
    assert!(error.starts_with("no live pane 'w9:p9'"), "{error}");
}

#[test]
fn a_registered_live_process_is_a_terminal_gap_not_a_dead_lane() {
    let dir = tempfile::tempdir().unwrap();
    let f = flock(&dir);
    let mut e = env();
    e.registered = Some((true, true, Some("4242".into())));
    let error = go(&f, &e, "w9:p9", true).unwrap_err();
    assert!(error.contains("reason=backend_missing_pane"), "{error}");
    assert!(error.contains("(pid 4242)"), "{error}");
    e.registered = Some((true, false, Some("4242".into())));
    let error = go(&f, &e, "w9:p9", true).unwrap_err();
    assert!(
        error.contains("no longer running") && error.contains("reason=dead_lane"),
        "{error}"
    );
}

#[test]
fn a_decorated_address_is_decided_by_its_embedded_pane_id() {
    let dir = tempfile::tempdir().unwrap();
    let (f, e) = (flock(&dir), env());
    assert_eq!(
        go(&f, &e, "whatever (w1:p2)", true).unwrap().2,
        Some("w1:p2".into())
    );
    assert_eq!(
        go(&f, &e, "PM (par-a) (w9:p1)", false).unwrap(),
        (
            "PM (par-a) (w9:p1)".into(),
            "PM (par-a) (w9:p1)".into(),
            None
        )
    );
    let error = go(&f, &e, "PM (w9:p1)", true).unwrap_err();
    assert!(
        error.contains("target 'PM (w9:p1)' names pane w9:p1, which is not live"),
        "{error}"
    );
}

#[test]
fn the_owner_mailbox_is_reserved_and_has_no_pane() {
    let dir = tempfile::tempdir().unwrap();
    let (f, e) = (flock(&dir), env());
    assert_eq!(
        go(&f, &e, " Owner ", true).unwrap(),
        ("owner".into(), "owner".into(), None)
    );
}

#[test]
fn the_lane_registry_wins_and_two_live_declarers_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (f, e) = (flock(&dir), env());
    f.remember_lane("w2:p2", "Nav Fixer", true);
    assert_eq!(
        go(&f, &e, "nav fixer", true).unwrap(),
        (
            "Nav Fixer".into(),
            "Nav Fixer (w2:p2)".into(),
            Some("w2:p2".into())
        )
    );
    // A declared identity beats the pane's own labels, on the id path too.
    assert_eq!(go(&f, &e, "w2:p2", false).unwrap().0, "Nav Fixer");
    f.remember_lane("w2:p1", "Nav Fixer", true);
    let error = go(&f, &e, "nav fixer", true).unwrap_err();
    assert!(
        error.contains("is DECLARED by more than one live pane (w2:p1, w2:p2)"),
        "{error}"
    );
}

#[test]
fn an_exact_name_shared_by_two_panes_is_refused_not_pooled() {
    let dir = tempfile::tempdir().unwrap();
    let (f, e) = (flock(&dir), env());
    let error = go(&f, &e, "worker", false).unwrap_err();
    assert!(
        error.starts_with("target mailbox 'worker' is ambiguous: w2:p1 (beta/Site codex)"),
        "{error}"
    );
}

#[test]
fn an_agent_type_is_never_a_mailbox() {
    let dir = tempfile::tempdir().unwrap();
    let (f, e) = (flock(&dir), env());
    // 'codex' matches two agent panes exactly, so it is refused as ambiguous...
    assert!(go(&f, &e, "codex", false)
        .unwrap_err()
        .contains("ambiguous"));
    // ...and with one codex pane left, the exact-name rule resolves that pane.
    let mut one = env();
    one.panes.remove(2);
    assert_eq!(
        go(&f, &one, "codex", false).unwrap().2,
        Some("w1:p2".into())
    );
}

#[test]
fn an_unknown_name_is_a_permissive_read_and_a_refused_send() {
    let dir = tempfile::tempdir().unwrap();
    let (f, e) = (flock(&dir), env());
    assert_eq!(
        go(&f, &e, "nav-fixer2", false).unwrap(),
        ("nav-fixer2".into(), "nav-fixer2".into(), None)
    );
    let error = go(&f, &e, "nav-fixer2", true).unwrap_err();
    assert!(error.contains("reason=unknown_name"), "{error}");
}

#[test]
fn a_registered_lane_without_a_live_pane_is_dead_or_unhoused() {
    let dir = tempfile::tempdir().unwrap();
    let (f, e) = (flock(&dir), env());
    let mut data = serde_json::Map::new();
    data.insert(
        "w7:p7".into(),
        json!({"mailbox": "gone-lane", "declared": true, "alive": false, "dead_since": "2026-10-20"}),
    );
    data.insert(
        "w7:p8".into(),
        json!({"mailbox": "between", "declared": true, "alive": true}),
    );
    assert!(f.write_lane_registry(&data));
    let dead = go(&f, &e, "gone-lane", true).unwrap_err();
    assert!(
        dead.contains("registered as DEAD (dead since 2026-10-20)")
            && dead.contains("reason=dead_lane"),
        "{dead}"
    );
    let unhoused = go(&f, &e, "between", true).unwrap_err();
    assert!(unhoused.contains("reason=unhoused_lane"), "{unhoused}");
    assert_eq!(go(&f, &e, "between", false).unwrap().2, None);
}

#[test]
fn an_empty_spec_and_self_without_a_pane_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (f, e) = (flock(&dir), env());
    assert_eq!(
        go(&f, &e, "  ", false).unwrap_err(),
        "a target mailbox is required"
    );
    let error = f
        .resolve_mailbox(&e, "self", None, false, HINT)
        .unwrap_err()
        .to_string();
    assert!(error.contains("needs TMUX_PANE"), "{error}");
}
