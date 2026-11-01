//! T31 tests: the role gate on a `Flock`, ported by name from the gate half of
//! `tests/test_dunk_roles.py` (`DispatchGateTests`, `BootstrapTests`) and the gating half of
//! `tests/test_dunk_superusers.py`. The dispatch-level cases (the allowlist refusing a command
//! by role) belong to `api.rs` (T39).

use serde_json::{json, Value};

use super::panes::PaneInfoEnv;
use super::Flock;
use crate::config::Env;
use crate::roles::Roles;
use crate::superusers::Superusers;

const OWNER: &str = "w1:p2";
const DESK: &str = "w2:p1";
const WORKER: &str = "w2:p3";
const OTHER: &str = "w2:p9";

/// A backend that knows no panes (the lane registry alone decides).
struct NoPanes;

impl PaneInfoEnv for NoPanes {
    fn list_panes_grouped(&self) -> Vec<Value> {
        Vec::new()
    }
    fn get_pane(&self, _: &str) -> Option<Value> {
        None
    }
}

fn dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("ds-gate-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn bare(tag: &str) -> (Flock, std::path::PathBuf) {
    let dir = dir(tag);
    let flock = Flock::new(dir.join("launches.json"), dir.join("lanes.json"), false);
    let roles = Roles::new(
        &Env::default(),
        Some(dir.join("roles.json")),
        Some(dir.join("tokens.json")),
        None,
    )
    .unwrap();
    flock.attach_roles(roles, None);
    (flock, dir)
}

/// The Python's `make_flock` + the three registrations every `DispatchGateTests` starts with.
fn staffed(tag: &str) -> Flock {
    let (flock, dir) = bare(tag);
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.join("roles.json")),
        Some(dir.join("tokens.json")),
        None,
    )
    .unwrap();
    roles
        .register(OWNER, "owner", None, None, None, None, None, None)
        .unwrap();
    roles
        .register(DESK, "desk", None, None, Some("pi"), None, None, None)
        .unwrap();
    roles
        .register(WORKER, "worker", None, None, Some("pi"), None, None, None)
        .unwrap();
    flock.attach_roles(roles, None);
    flock
}

fn role(flock: &Flock, pane: Option<&str>) -> String {
    flock.clear_caller();
    flock.caller_role(pane, &NoPanes).unwrap()
}

fn err(result: Result<Value, crate::error::DunkError>) -> String {
    result.unwrap_err().to_string()
}

#[test]
fn whoami_reports_the_bound_role() {
    let flock = staffed("whoami");
    for (pane, want) in [
        (Some(OWNER), "owner"),
        (Some(DESK), "desk"),
        (Some(OTHER), "anonymous"),
        (None, "anonymous"),
    ] {
        flock.clear_caller();
        assert_eq!(
            flock.whoami(pane, &NoPanes).unwrap()["role"],
            want,
            "{pane:?}"
        );
    }
    flock.clear_caller();
    let me = flock.whoami(Some(DESK), &NoPanes).unwrap();
    assert_eq!(me["registered"]["role"], "desk");
    assert_eq!(me["via_token"], false);
    assert_eq!(me["caller"]["pid"], Value::Null);
}

#[test]
fn a_system_token_grants_system_and_a_web_token_grants_the_owner_transport() {
    let flock = staffed("tokens");
    flock.clear_caller();
    let system = flock
        .token_issue("system", Some("watchdog"), Some(OWNER), &NoPanes)
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    let web = flock
        .token_issue("web", Some("desk"), Some(OWNER), &NoPanes)
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    for (token, want) in [
        (Some(system.as_str()), "system"),
        (Some(web.as_str()), "web"),
        (Some("garbage"), "anonymous"),
    ] {
        flock.clear_caller();
        flock.set_service_token(token);
        assert_eq!(flock.whoami(None, &NoPanes).unwrap()["role"], want);
    }
    flock.clear_caller();
    flock.set_service_token(Some(&system));
    assert_eq!(flock.whoami(None, &NoPanes).unwrap()["via_token"], true);
}

#[test]
fn only_the_owner_issues_tokens_and_registers_lanes() {
    let flock = staffed("owner-only");
    flock.clear_caller();
    assert!(err(flock.token_issue("web", None, Some(WORKER), &NoPanes))
        .contains("requires a superuser"));
    flock.clear_caller();
    let refused = err(flock.lane_register(
        Some(OTHER),
        Some("owner"),
        None,
        None,
        None,
        Some(WORKER),
        &NoPanes,
    ));
    assert!(refused.contains("requires the owner role"), "{refused}");
    // The system token cannot mint owners.
    flock.clear_caller();
    let system = flock
        .token_issue("system", None, Some(OWNER), &NoPanes)
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    flock.clear_caller();
    flock.set_service_token(Some(&system));
    let refused =
        err(flock.lane_register(Some(OTHER), Some("owner"), None, None, None, None, &NoPanes));
    assert!(
        refused.contains("the system token cannot create owners"),
        "{refused}"
    );
}

#[test]
fn superuser_add_refuses_names() {
    let flock = staffed("names");
    flock.clear_caller();
    let refused = err(flock.superuser_add("VP of agentry", Some(OWNER), &NoPanes));
    assert!(refused.contains("names are no longer matched"), "{refused}");
}

#[test]
fn the_first_owner_self_registers_then_nobody_else_can() {
    let (flock, _dir) = bare("bootstrap");
    flock.clear_caller();
    let refused = err(flock
        .require_superuser(Some(OWNER), "close", &NoPanes)
        .map(|_| Value::Null));
    assert!(refused.contains("no owner is registered"), "{refused}");
    flock.clear_caller();
    flock.superuser_recover(Some(OWNER), &NoPanes).unwrap();
    assert_eq!(role(&flock, Some(OWNER)), "owner");
    flock.clear_caller();
    let refused = err(flock.superuser_recover(Some(OTHER), &NoPanes));
    assert!(refused.contains("requires the owner role"), "{refused}");
    flock.clear_caller();
    assert!(err(flock.superuser_recover(None, &NoPanes)).contains("requires a known caller pane"));
}

#[test]
fn a_non_owner_is_refused_by_name_and_a_label_grants_nothing() {
    let flock = staffed("gating");
    flock.clear_caller();
    let refused = err(flock
        .require_superuser(Some(WORKER), "close", &NoPanes)
        .map(|_| Value::Null));
    assert!(
        refused.contains("close requires a superuser (the owner role); w2:p3 is worker"),
        "{refused}"
    );
    flock.clear_caller();
    flock
        .require_superuser(Some(OWNER), "close", &NoPanes)
        .unwrap();
    flock.clear_caller();
    let nobody = err(flock
        .require_superuser(None, "close", &NoPanes)
        .map(|_| Value::Null));
    assert!(nobody.contains("this caller is anonymous"), "{nobody}");
}

#[test]
fn superuser_add_and_remove_are_gated_and_the_last_owner_stays() {
    let flock = staffed("add-remove");
    flock.clear_caller();
    assert!(
        err(flock.superuser_remove(OWNER, Some(WORKER), &NoPanes)).contains("requires a superuser")
    );
    flock.clear_caller();
    assert!(err(flock.superuser_remove(OWNER, Some(OWNER), &NoPanes))
        .contains("refusing to remove the last owner"));
    flock.clear_caller();
    let added = flock.superuser_add("w3:p1", Some(OWNER), &NoPanes).unwrap();
    assert_eq!(added["added"], "w3:p1");
    assert_eq!(added["entry"]["role"], "owner");
    flock.clear_caller();
    let removed = flock
        .superuser_remove("w3:p1", Some(OWNER), &NoPanes)
        .unwrap();
    assert_eq!(removed["removed"], true);
    assert_eq!(role(&flock, Some("w3:p1")), "anonymous");
}

#[test]
fn a_legacy_superuser_is_an_owner_only_where_the_registry_says_default() {
    let (flock, dir) = bare("legacy");
    let roles = Roles::new(
        &Env::default(),
        Some(dir.join("roles.json")),
        Some(dir.join("tokens.json")),
        None,
    )
    .unwrap();
    std::fs::write(
        dir.join("superusers.json"),
        json!({"panes": ["w9:p9"], "names": []}).to_string(),
    )
    .unwrap();
    let legacy = Superusers::new(dir.join("superusers.json"), true).unwrap();
    flock.attach_roles(roles, Some(legacy));
    assert_eq!(
        role(&flock, Some("w9:p9")),
        "owner",
        "no registry file: worker default is not 'default', legacy honoured only at the default"
    );
}

#[test]
fn an_unattached_registry_is_a_named_refusal() {
    let dir = dir("unattached");
    let flock = Flock::new(dir.join("l.json"), dir.join("r.json"), false);
    let refused = flock
        .caller_role(Some(OWNER), &NoPanes)
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("no role registry is attached"),
        "{refused}"
    );
}
