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

fn dir(tag: &str) -> crate::testsupport::Scratch<std::path::PathBuf> {
    let scratch = crate::testsupport::dir(&format!("gate-{tag}"));
    crate::testsupport::Scratch::new(scratch.path().to_path_buf(), scratch)
}

fn bare(tag: &str) -> (Flock, crate::testsupport::Scratch<std::path::PathBuf>) {
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
fn staffed(tag: &str) -> crate::testsupport::Scratch<Flock> {
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
    // The directory must outlive the helper: the roles file the flock reads is in it.
    dir.map(|_| flock)
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
fn owner_token_rejects_nonowner_pane_but_preserves_headless_and_owner() {
    struct Panes {
        rows: Vec<Value>,
        fail: bool,
    }
    impl PaneInfoEnv for Panes {
        fn list_panes_grouped(&self) -> Vec<Value> {
            self.rows.clone()
        }
        fn get_pane(&self, id: &str) -> Option<Value> {
            self.rows.iter().find(|p| p["pane_id"] == id).cloned()
        }
        fn try_authority_panes(&self) -> Result<Vec<Value>, crate::error::DunkError> {
            if self.fail {
                Err(crate::error::DunkError::Refused("offline".into()))
            } else {
                Ok(self.rows.clone())
            }
        }
    }
    let (flock, dir) = bare("owner-token-peer");
    let caller_pid = std::process::id() as i64;
    let caller_start = crate::proc::process_start(caller_pid).unwrap();
    let mut cursor = caller_pid;
    let mut tmux_root = None;
    for _ in 0..crate::proc::MAX_DEPTH {
        let row = crate::proc::stat(cursor).unwrap();
        if row.ppid <= 1 {
            break;
        }
        let parent = crate::proc::stat(row.ppid).unwrap();
        if parent.comm == "tmux: server" {
            tmux_root = Some(cursor);
            break;
        }
        cursor = row.ppid;
    }
    let pid = tmux_root.unwrap_or(caller_pid);
    let start = crate::proc::process_start(pid).unwrap();
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.join("roles.json")),
        Some(dir.join("tokens.json")),
        None,
    )
    .unwrap();
    roles
        .register(
            WORKER,
            "worker",
            Some(pid),
            Some(&start),
            None,
            None,
            None,
            None,
        )
        .unwrap();
    roles
        .register(
            OWNER,
            "owner",
            Some(pid),
            Some(&start),
            None,
            None,
            None,
            None,
        )
        .unwrap();
    let token = roles.issue_token("owner", Some("test"), None).unwrap();
    let web_token = roles.issue_token("web", Some("web-test"), None).unwrap();
    flock.attach_roles(roles, None);
    let caller = json!({"pid":caller_pid,"start":caller_start,"source":"peer"});
    flock.set_socket_peer(Some(true));
    flock.with_caller(|state| state.socket_peer_uid = Some(nix::unistd::geteuid().as_raw()));
    flock.clear_caller();
    assert_eq!(
        flock.caller_state().socket_peer_uid,
        Some(nix::unistd::geteuid().as_raw())
    );
    flock.clear_socket_peer();
    assert_eq!(flock.caller_state().socket_peer_uid, None);
    let check = |rows: Vec<Value>, fail: bool, claim: Option<&str>| {
        flock.clear_caller();
        flock.set_caller(Some(caller.clone()));
        flock.set_service_token(Some(&token));
        flock.set_socket_peer(Some(true));
        flock.with_caller(|state| state.socket_peer_uid = Some(nix::unistd::geteuid().as_raw()));
        flock.caller_role(claim, &Panes { rows, fail })
    };
    for uid in [None, Some(nix::unistd::geteuid().as_raw().wrapping_add(1))] {
        flock.clear_caller();
        flock.set_service_token(Some(&token));
        flock.set_socket_peer(Some(true));
        flock.with_caller(|state| state.socket_peer_uid = uid);
        let error = flock.caller_role(
            None,
            &Panes {
                rows: vec![],
                fail: false,
            },
        );
        assert!(error
            .unwrap_err()
            .to_string()
            .contains("daemon Unix principal"));
    }
    flock.clear_caller();
    flock.set_service_token(Some(&web_token));
    flock.set_socket_peer(Some(true));
    flock.with_caller(|state| {
        state.socket_peer_uid = Some(nix::unistd::geteuid().as_raw().wrapping_add(1));
    });
    assert_eq!(flock.caller_role(None, &NoPanes).unwrap(), "web");
    let worker = json!({"pane_id":WORKER,"pane_pid":pid});
    for claim in [Some(WORKER), None, Some(OWNER)] {
        assert!(check(vec![worker.clone()], false, claim).is_err());
    }
    assert_eq!(
        check(vec![json!({"pane_id":OWNER,"pane_pid":pid})], false, None).unwrap(),
        "owner"
    );
    struct Child(std::process::Child);
    impl Drop for Child {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let child = Child(
        std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .unwrap(),
    );
    let unrelated = check(
        vec![json!({"pane_id":"%unrelated","pane_pid":child.0.id()})],
        false,
        None,
    );
    if tmux_root.is_some() {
        assert!(unrelated.is_err());
    } else {
        assert_eq!(unrelated.unwrap(), "owner");
    }
    assert!(check(
        vec![json!({"pane_id":"%missing","pane_pid":99999999})],
        false,
        None
    )
    .is_err());
    assert!(check(vec![worker.clone()], true, None).is_err());
    assert!(check(
        vec![worker, json!({"pane_id":OWNER,"pane_pid":pid})],
        false,
        None
    )
    .is_err());
    assert!(check(
        vec![json!({"pane_id":"%unknown","pane_pid":pid})],
        false,
        None
    )
    .is_err());
    flock
        .with_roles(|roles, _| {
            roles
                .register(
                    OWNER,
                    "owner",
                    Some(pid),
                    Some("stale"),
                    None,
                    None,
                    None,
                    None,
                )
                .map(|_| ())
        })
        .unwrap();
    assert!(check(vec![json!({"pane_id":OWNER,"pane_pid":pid})], false, None).is_err());
    flock.clear_caller();
    flock.set_socket_peer(Some(true));
    assert_eq!(
        flock
            .caller_role(
                Some(WORKER),
                &Panes {
                    rows: vec![json!({"pane_id":WORKER,"pane_pid":pid})],
                    fail: false
                }
            )
            .unwrap(),
        "worker"
    );
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
fn owner_pane_and_legacy_roles_require_the_daemon_socket_uid() {
    let registered = staffed("owner-peer-uid-registered");
    let (legacy, dir) = bare("owner-peer-uid-legacy");
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
    legacy.attach_roles(
        roles,
        Some(Superusers::new(dir.join("superusers.json"), true).unwrap()),
    );
    let uid = nix::unistd::geteuid().as_raw();
    for (flock, pane) in [(&*registered, OWNER), (&legacy, "w9:p9")] {
        for (peer_uid, allowed) in [
            (Some(uid), true),
            (Some(uid.wrapping_add(1)), false),
            (None, false),
        ] {
            flock.clear_caller();
            flock.set_socket_peer(Some(true));
            flock.with_caller(|state| state.socket_peer_uid = peer_uid);
            let result = flock.caller_role(Some(pane), &NoPanes);
            if allowed {
                assert_eq!(result.unwrap(), "owner");
            } else {
                assert!(result
                    .unwrap_err()
                    .to_string()
                    .contains("daemon Unix principal"));
            }
        }
    }
    registered.clear_caller();
    registered.set_socket_peer(Some(true));
    registered.with_caller(|state| state.socket_peer_uid = Some(uid.wrapping_add(1)));
    assert_eq!(
        registered.caller_role(Some(WORKER), &NoPanes).unwrap(),
        "worker"
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

// -- t1347 / t1354: the caller state is PER REQUEST (the Python's threading.local) ----------

#[test]
fn one_requests_service_token_is_never_seen_by_another_request() {
    // Request A holds an owner token; request B is an anonymous pane-less caller on the SAME
    // flock. B must be anonymous. With one shared CallerState B inherited A's token and role.
    use std::sync::{Arc, Barrier};

    let flock = Arc::new(staffed("isolation"));
    flock.clear_caller();
    let owner_token = flock
        .token_issue("owner", Some("a"), Some(OWNER), &NoPanes)
        .unwrap()["token"]
        .as_str()
        .unwrap()
        .to_string();
    flock.clear_caller();
    let (a_has_token, b_looked) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));

    let a = {
        let (flock, a_has_token, b_looked) = (flock.clone(), a_has_token.clone(), b_looked.clone());
        std::thread::spawn(move || {
            flock.set_service_token(Some(&owner_token));
            assert_eq!(
                flock.caller_role(None, &NoPanes).unwrap(),
                "owner",
                "A is the owner"
            );
            a_has_token.wait(); // A is mid-request, token set
            b_looked.wait(); // B has looked
            assert_eq!(
                flock.caller_role(None, &NoPanes).unwrap(),
                "owner",
                "A keeps its own role"
            );
            assert_eq!(flock.whoami(None, &NoPanes).unwrap()["via_token"], true);
            flock.clear_caller();
        })
    };
    let b = {
        let (flock, a_has_token, b_looked) = (flock.clone(), a_has_token.clone(), b_looked.clone());
        std::thread::spawn(move || {
            a_has_token.wait();
            let role = flock.caller_role(None, &NoPanes).unwrap();
            let via_token = flock.whoami(None, &NoPanes).unwrap()["via_token"].clone();
            let plain = flock.caller_role(Some(OTHER), &NoPanes).unwrap();
            b_looked.wait();
            (role, via_token, plain)
        })
    };
    a.join().expect("request A");
    let (role, via_token, plain) = b.join().expect("request B");
    assert_eq!(role, "anonymous", "B must not inherit A's owner token");
    assert_eq!(via_token, false);
    assert_eq!(
        plain, "anonymous",
        "a pane with no registration is anonymous, not A's owner"
    );
}

#[test]
fn one_requests_socket_peer_verdict_and_cached_role_do_not_leak() {
    use std::sync::{Arc, Barrier};

    let flock = Arc::new(staffed("peer"));
    let (a_set, b_looked) = (Arc::new(Barrier::new(2)), Arc::new(Barrier::new(2)));
    let a = {
        let (flock, a_set, b_looked) = (flock.clone(), a_set.clone(), b_looked.clone());
        std::thread::spawn(move || {
            flock.set_socket_peer(Some(true));
            flock.with_caller(|state| {
                state.socket_peer_uid = Some(nix::unistd::geteuid().as_raw());
            });
            assert_eq!(flock.caller_role(Some(OWNER), &NoPanes).unwrap(), "owner"); // cached for A
            a_set.wait();
            b_looked.wait();
            assert_eq!(
                flock.socket_peer_ok(),
                Some(true),
                "A keeps its own verdict"
            );
            assert_eq!(
                flock.caller_state().socket_peer_uid,
                Some(nix::unistd::geteuid().as_raw()),
                "A keeps its own peer UID"
            );
            flock.clear_socket_peer();
        })
    };
    let b = {
        let (flock, a_set, b_looked) = (flock.clone(), a_set.clone(), b_looked.clone());
        std::thread::spawn(move || {
            a_set.wait();
            let peer = flock.socket_peer_ok();
            let peer_uid = flock.caller_state().socket_peer_uid;
            // B is also in a request, but its peer credentials failed verification.
            flock.set_socket_peer(Some(false));
            let role = flock.caller_role(Some(OWNER), &NoPanes);
            b_looked.wait();
            (peer, peer_uid, role)
        })
    };
    a.join().expect("request A");
    let (peer, peer_uid, role) = b.join().expect("request B");
    assert_eq!(peer, None, "B must not see A's peer verdict");
    assert_eq!(peer_uid, None, "B must not see A's peer UID");
    assert!(role
        .unwrap_err()
        .to_string()
        .contains("daemon Unix principal"));
}

#[test]
fn clear_caller_on_one_thread_leaves_another_threads_state_alone() {
    use std::sync::Arc;
    let flock = Arc::new(staffed("clear"));
    flock.set_service_token(Some("mine"));
    let other = {
        let flock = flock.clone();
        std::thread::spawn(move || {
            flock.set_service_token(Some("theirs"));
            flock.clear_caller();
            flock.caller_state().service_token
        })
    };
    assert_eq!(other.join().unwrap(), None);
    assert_eq!(flock.caller_state().service_token.as_deref(), Some("mine"));
    flock.clear_caller();
}
