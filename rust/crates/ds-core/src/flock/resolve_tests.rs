//! Ported from tests/test_dunk_core.py: test_resolve_target_by_self_substring_and_terminal_id,
//! test_exact_tab_label_beats_title_substring_on_an_agent_pane,
//! test_resolve_workspace_and_tab_by_id_label_substring_and_self. The fixtures are
//! tests/fakes.py PANES/TABS/WORKSPACES with `list_panes_grouped`'s annotations applied.

use super::resolve::*;
use crate::error::DunkError;
use serde_json::{json, Value};

fn tabs() -> Vec<Value> {
    vec![
        json!({"tab_id": "w1:t1", "workspace_id": "w1", "label": "shell", "number": 1}),
        json!({"tab_id": "w1:t2", "workspace_id": "w1", "label": "Claude Proj", "number": 2}),
        json!({"tab_id": "w2:t1", "workspace_id": "w2", "label": "Codex Site", "number": 1}),
        json!({"tab_id": "w2:t2", "workspace_id": "w2", "label": "Sheep", "number": 2}),
        json!({"tab_id": "w2:t3", "workspace_id": "w2", "label": "Other", "number": 3}),
    ]
}

fn workspaces() -> Vec<Value> {
    vec![
        json!({"workspace_id": "w1", "label": "alpha", "number": 1}),
        json!({"workspace_id": "w2", "label": "beta", "number": 2}),
    ]
}

fn panes() -> Vec<Value> {
    let raw = [
        (
            "w1:p1", "w1:t1", "w1", "/home/x", "", "term_a", "shell", "shell", "alpha",
        ),
        (
            "w1:p2",
            "w1:t2",
            "w1",
            "/home/x/proj",
            "claude",
            "term_b",
            "claude proj",
            "Claude Proj",
            "alpha",
        ),
        (
            "w2:p1",
            "w2:t1",
            "w2",
            "/home/x/site",
            "codex",
            "term_c",
            "site",
            "Codex Site",
            "beta",
        ),
        (
            "w2:p2",
            "w2:t2",
            "w2",
            "/home/x/sheep",
            "",
            "term_d",
            "shell",
            "Sheep",
            "beta",
        ),
        (
            "w2:p3",
            "w2:t3",
            "w2",
            "/home/x/other",
            "claude",
            "term_e",
            "Sheep release work",
            "Other",
            "beta",
        ),
    ];
    raw.iter()
        .map(|(p, t, w, cwd, agent, term, title, tab_label, ws_label)| {
            let mut pane = json!({"pane_id": p, "tab_id": t, "workspace_id": w, "cwd": cwd,
                "terminal_id": term, "terminal_title_stripped": title,
                "tab_label": tab_label, "workspace_label": ws_label});
            if !agent.is_empty() {
                pane["agent"] = json!(agent);
            }
            pane
        })
        .collect()
}

fn id_of(v: &Value) -> &str {
    v["pane_id"].as_str().unwrap()
}

const HINT: &str = "terminal server not reachable";

fn refusal(result: Result<&Value, DunkError>) -> String {
    result.expect_err("expected an error").to_string()
}

#[test]
fn test_resolve_target_by_self_substring_and_terminal_id() {
    let panes = panes();
    let go = |spec: &str, me: Option<&str>| resolve_target(&panes, spec, me, HINT);
    assert_eq!("w1:p1", id_of(go("self", Some("w1:p1")).unwrap()));
    assert_eq!("w2:p1", id_of(go("codex site", None).unwrap()));
    assert_eq!("w1:p2", id_of(go("term_b", None).unwrap()));
    // "alpha" matches both alpha panes, but only one hosts an agent.
    assert_eq!("w1:p2", id_of(go("alpha", None).unwrap()));
    assert!(refusal(go("nothing-here", None)).contains("no pane matches"));
    assert!(refusal(go("self", None)).contains("needs TMUX_PANE"));
    let ambiguous = go("/home/x", None).expect_err("ambiguous");
    assert!(matches!(ambiguous, DunkError::Ambiguous(_)));
    assert!(ambiguous.to_string().contains("ambiguous"));
}

#[test]
fn test_exact_tab_label_beats_title_substring_on_an_agent_pane() {
    let panes = panes();
    let go = |spec: &str| resolve_target(&panes, spec, None, HINT);
    assert_eq!("w2:p2", id_of(go("sheep").unwrap()));
    assert_eq!("w2:p2", id_of(go("Sheep").unwrap()));
    assert_eq!("w2:p3", id_of(go("release work").unwrap()));
    assert_eq!("w1:p2", id_of(go("alpha").unwrap()));
}

#[test]
fn test_resolve_workspace_and_tab_by_id_label_substring_and_self() {
    let (ws, tabs, panes) = (workspaces(), tabs(), panes());
    let own = |id: &'static str| {
        let panes = panes.clone();
        move || self_pane(Some(id), |p| panes.iter().find(|x| id_of(x) == p).cloned())
    };
    let none = || self_pane(None, |_| None);
    let w = |spec: &str| resolve_workspace(&ws, spec, none, HINT);
    assert_eq!("w2", w("w2").unwrap()["workspace_id"]);
    assert_eq!("w2", w("BETA").unwrap()["workspace_id"]);
    assert_eq!("w1", w("alp").unwrap()["workspace_id"]);
    assert_eq!(
        "w1",
        resolve_workspace(&ws, "self", own("w1:p2"), HINT).unwrap()["workspace_id"]
    );
    assert_eq!(
        "w2:t2",
        resolve_tab(&tabs, "Sheep", none, HINT).unwrap()["tab_id"]
    );
    assert_eq!(
        "w2:t1",
        resolve_tab(&tabs, "codex", none, HINT).unwrap()["tab_id"]
    );
    assert_eq!(
        "w1:t2",
        resolve_tab(&tabs, "self", own("w1:p2"), HINT).unwrap()["tab_id"]
    );
    assert!(refusal(w("gamma")).contains("no workspace matches"));
    assert!(refusal(w("self")).contains("needs TMUX_PANE"));
    // Codex Site, Sheep, Other
    assert!(refusal(resolve_tab(&tabs, "e", none, HINT)).contains("ambiguous"));
}

#[test]
fn test_pick_messages_match_the_python_texts() {
    let ws = workspaces();
    let none = || self_pane(None, |_| None);
    assert_eq!(
        "workspace is required",
        refusal(resolve_workspace(&ws, "  ", none, HINT))
    );
    assert_eq!(
        "no workspace matches 'gamma' on the current backend",
        refusal(resolve_workspace(&ws, "gamma", none, HINT))
    );
    let tabs = tabs();
    assert_eq!(
        "tab 'e' is ambiguous: w1:t1 (shell), w1:t2 (Claude Proj), w2:t1 (Codex Site), w2:t2 (Sheep), w2:t3 (Other)",
        refusal(resolve_tab(&tabs, "e", none, HINT))
    );
}

#[test]
fn test_an_empty_backend_reports_the_server_hint_and_a_dict_target_needs_a_pane_id() {
    let none = || self_pane(None, |_| None);
    assert_eq!(HINT, refusal(resolve_tab(&[], "x", none, HINT)));
    assert_eq!(HINT, refusal(resolve_target(&[], "x", None, HINT)));
    let panes = panes();
    assert_eq!(
        "target dict has no pane_id",
        resolve_target_dict(&panes, &json!({}))
            .unwrap_err()
            .to_string()
    );
    assert_eq!(
        "w1:p2",
        id_of(&resolve_target_dict(&panes, &json!({"pane_id": "w1:p2"})).unwrap())
    );
    let unknown = json!({"pane_id": "w9:p9"});
    assert_eq!(unknown, resolve_target_dict(&panes, &unknown).unwrap());
}

#[test]
fn test_describe_matches_the_python_format() {
    let panes = panes();
    let both = [&panes[1], &panes[0]];
    assert_eq!(
        "w1:p2 (alpha/Claude Proj claude), w1:p1 (alpha/shell)",
        describe(&both)
    );
}
