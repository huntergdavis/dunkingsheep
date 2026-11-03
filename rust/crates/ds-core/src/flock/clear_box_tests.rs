use super::clear_box::ClearBoxEnv;
use super::panes::PaneInfoEnv;
use super::sends::SendEnv;
use super::Flock;
use crate::config::Env;
use crate::roles::Roles;
use serde_json::{json, Value};
use std::cell::Cell;
use std::collections::BTreeMap;

const OWNER: &str = "w1:p1";
const WORKER: &str = "w2:p1";

#[derive(Default)]
struct Backend {
    panes: Vec<Value>,
    screens: BTreeMap<String, Option<String>>,
    statuses: BTreeMap<String, String>,
    keys: Vec<String>,
    fail_at: Option<usize>,
    list_calls: Cell<usize>,
}

impl SendEnv for Backend {
    fn read_pane(&self, pane_id: &str, _: i64, _: bool) -> Option<String> {
        self.screens.get(pane_id).cloned().flatten()
    }
    fn pane_harness(&self, _: &str) -> Option<String> {
        None
    }
    fn session_process_gone(&self, _: &str) -> bool {
        false
    }
    fn read_status(&self, pane_id: &str) -> Option<String> {
        self.statuses.get(pane_id).cloned()
    }
    fn send_key(&mut self, _: &str, key: &str) -> (bool, String) {
        self.keys.push(key.to_owned());
        let ok = self.fail_at != Some(self.keys.len());
        (
            ok,
            if ok { "sent" } else { "terminal refused key" }.to_owned(),
        )
    }
    fn federated_input(&self) -> bool {
        false
    }
}

impl PaneInfoEnv for Backend {
    fn list_panes_grouped(&self) -> Vec<Value> {
        self.list_calls.set(self.list_calls.get() + 1);
        self.panes.clone()
    }
    fn get_pane(&self, pane_id: &str) -> Option<Value> {
        self.panes.iter().find(|p| p["pane_id"] == pane_id).cloned()
    }
}

impl ClearBoxEnv for Backend {
    fn server_error_hint(&self) -> String {
        "server unavailable".into()
    }
}

fn flock(tag: &str, with_owner: bool) -> (Flock, crate::testsupport::Scratch<std::path::PathBuf>) {
    let scratch = crate::testsupport::dir(&format!("clear-box-{tag}"));
    let dir = crate::testsupport::Scratch::new(scratch.path().to_path_buf(), scratch);
    let flock = Flock::new(dir.join("launches.json"), dir.join("lanes.json"), false);
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.join("roles.json")),
        Some(dir.join("tokens.json")),
        None,
    )
    .unwrap();
    if with_owner {
        roles
            .register(OWNER, "owner", None, None, None, None, None, None)
            .unwrap();
    } else {
        roles
            .register(
                WORKER,
                "worker",
                None,
                None,
                Some("codex"),
                None,
                None,
                None,
            )
            .unwrap();
    }
    flock.attach_roles(roles, None);
    (flock, dir)
}

fn pane(label: &str) -> Value {
    json!({"pane_id": WORKER, "tab_label": label, "label": "fallback", "name": "name"})
}

fn ready_screen() -> String {
    "› Ask Codex to do anything\n".into()
}

fn pending_screen(text: &str) -> String {
    format!("│ ❯ {text}\n────────\n")
}

#[test]
fn unreadable_and_modal_delivery_states_preserve_wire_shape() {
    let (flock, _dir) = flock("unreadable", false);
    let backend = Backend::default();
    let unreadable = flock.delivery_state(&backend, WORKER, None, None, None);
    assert_eq!(
        unreadable,
        json!({"state":"unknown", "reason":"pane unreadable"})
    );

    let mut backend = Backend::default();
    backend.screens.insert(
        WORKER.into(),
        Some("Press Enter to confirm or Esc to cancel\n".into()),
    );
    let modal = flock.delivery_state(&backend, WORKER, None, None, None);
    // The modal itself is not composer text, while its Escape affordance classifies as working.
    assert_eq!(modal["state"], "working");
    assert_eq!(modal["foreign_text"], false);
    assert_eq!(modal["foreign_excerpt"], Value::Null);
}

#[test]
fn delivery_state_keeps_busy_deliverable_and_blocked_dead_classifications() {
    let (flock, _dir) = flock("classifications", false);
    let mut backend = Backend::default();
    backend.screens.insert(WORKER.into(), Some(ready_screen()));
    backend.statuses.insert(WORKER.into(), "busy".into());
    let working = flock.delivery_state(&backend, WORKER, None, None, None);
    assert_eq!(working["state"], "working");
    assert_eq!(working["deliverable"], true);

    for (screen, state) in [
        (
            "Goal paused: wait for input\n› Ask Codex to do anything\n",
            "blocked",
        ),
        (
            "Context is full - please /clear\n› Ask Codex to do anything\n",
            "blocked",
        ),
        ("Session expired\n", "dead"),
    ] {
        let state_value = flock.delivery_state(&backend, WORKER, Some("idle"), None, Some(screen));
        assert_eq!(state_value["state"], state, "{screen}");
        assert_eq!(state_value["deliverable"], false, "{screen}");
    }
}

#[test]
fn delivery_state_marks_ambiguous_composer_unknown_and_collapses_foreign_excerpt() {
    let (flock, _dir) = flock("ambiguity-and-foreign", false);
    let ambiguous = "› old transcript\n› live draft\n\n\n  gpt · ~/repo\n";
    let state = flock.delivery_state(
        &Backend::default(),
        WORKER,
        Some("idle"),
        None,
        Some(ambiguous),
    );
    assert_eq!(state["state"], "unknown");
    assert!(state["reason"]
        .as_str()
        .unwrap()
        .starts_with("ambiguous composer/transcript:"));

    let text = "  one   two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen seventeen eighteen nineteen twenty twenty-one ";
    let screen = pending_screen(text);
    let foreign = flock.delivery_state(
        &Backend::default(),
        WORKER,
        Some("idle"),
        None,
        Some(&screen),
    );
    assert_eq!(foreign["foreign_text"], true);
    let excerpt = foreign["foreign_excerpt"].as_str().unwrap();
    assert_eq!(excerpt.chars().count(), 60);
    assert!(!excerpt.contains('\n'));

    flock.remember_typed(WORKER, text);
    let ours = flock.delivery_state(
        &Backend::default(),
        WORKER,
        Some("idle"),
        None,
        Some(&screen),
    );
    assert_eq!(ours["foreign_text"], false);
    assert_eq!(ours["foreign_excerpt"], Value::Null);
}

#[test]
fn clear_box_authorizes_before_resolution_and_sends_only_the_ordered_pair() {
    let (flock, _dir) = flock("clear-success", true);
    let mut backend = Backend::default();
    backend.panes.push(pane("tab first"));
    backend.screens.insert(
        WORKER.into(),
        Some("Context is full - please /clear\n".into()),
    );

    let refused = flock
        .clear_box(&mut backend, &json!("missing pane"), Some(WORKER), None)
        .unwrap_err();
    assert!(refused.to_string().contains("requires a superuser"));
    assert_eq!(
        backend.list_calls.get(),
        0,
        "authorization must precede pane resolution"
    );

    let result = flock
        .clear_box(
            &mut backend,
            &json!({"pane_id": WORKER}),
            Some(OWNER),
            Some("cleanup"),
        )
        .unwrap();
    assert_eq!(backend.keys, ["C-u", "Escape"]);
    assert_eq!(result["flushed"], true);
    assert_eq!(result["target_label"], "tab first");
    assert_eq!(result["before"]["foreign_text"], true);
    assert_eq!(result["by"], OWNER);
    assert_eq!(result["reason"], "cleanup");
}

#[test]
fn clear_box_noops_for_ready_and_working_and_stops_after_first_key_failure() {
    let (flock, _dir) = flock("clear-noop-error", true);
    let mut backend = Backend::default();
    backend
        .panes
        .push(json!({"pane_id": WORKER, "agent_status":"busy"}));
    backend.statuses.insert(WORKER.into(), "busy".into());
    backend.screens.insert(WORKER.into(), Some(ready_screen()));
    let working = flock
        .clear_box(&mut backend, &json!({"pane_id": WORKER}), Some(OWNER), None)
        .unwrap();
    assert_eq!(working["flushed"], false);
    assert_eq!(
        working["reason"],
        "the lane is working; a clear would interrupt it"
    );
    assert!(backend.keys.is_empty());

    backend.panes[0]["agent_status"] = json!("idle");
    backend.statuses.insert(WORKER.into(), "idle".into());
    let ready = flock
        .clear_box(&mut backend, &json!({"pane_id": WORKER}), Some(OWNER), None)
        .unwrap();
    assert_eq!(ready["flushed"], false);
    assert_eq!(
        ready["target_label"], WORKER,
        "pane id is the final label fallback"
    );
    assert_eq!(
        ready["reason"],
        "nothing to clear: the input box is already clear"
    );

    backend.screens.insert(
        WORKER.into(),
        Some("Context is full - please /clear\n".into()),
    );
    backend.fail_at = Some(1);
    let error = flock
        .clear_box(&mut backend, &json!({"pane_id": WORKER}), Some(OWNER), None)
        .unwrap_err();
    assert!(error
        .to_string()
        .contains(&format!("flush failed for {WORKER}: terminal refused key")));
    assert_eq!(backend.keys, ["C-u"], "Escape must not follow a failed C-u");
}
