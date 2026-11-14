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

/// t4553 option A: every accepted detached owner-token call leaves an audit record naming the
/// caller, its parent, the root of its process chain and whether that root changed; an in-pane
/// owner call leaves none. The decision itself is unchanged.
#[test]
fn accepted_detached_owner_token_calls_are_audited() {
    use super::roles_gate::DetachedOwnerCall;
    let stat = |pid: i64, ppid: i64, comm: &str, start: &str| crate::proc::ProcStat {
        pid,
        comm: comm.into(),
        state: "S".into(),
        ppid,
        session: 0,
        start: start.into(),
    };
    let chain = [
        stat(900, 800, "dunkingsheep", "5"),
        stat(800, 700, "codex\nforged line", "4"),
        stat(700, 1, "codex", "3"),
    ];
    let first = DetachedOwnerCall::new(&chain, Some("/work".into()), None).unwrap();
    assert_eq!(
        first.line(),
        "owner token accepted from a detached caller: pid=900 parent=\"codex\\nforged line\" \
         root=700:3 cwd=\"/work\" root_seen=first"
    );
    let again = DetachedOwnerCall::new(&chain, None, Some(&first)).unwrap();
    assert_eq!((again.root_seen, again.cwd.as_str()), ("same_root", ""));
    let moved = [stat(901, 1, "sh", "9")];
    let other = DetachedOwnerCall::new(&moved, None, Some(&first)).unwrap();
    assert_eq!((other.root_seen, other.parent.as_str()), ("new_root", ""));
    assert_eq!(other.root, (901, "9".to_string()));
    assert!(DetachedOwnerCall::new(&[], None, None).is_none());

    struct Panes(Vec<Value>);
    impl PaneInfoEnv for Panes {
        fn list_panes_grouped(&self) -> Vec<Value> {
            self.0.clone()
        }
        fn get_pane(&self, id: &str) -> Option<Value> {
            self.0.iter().find(|p| p["pane_id"] == id).cloned()
        }
        fn try_authority_panes(&self) -> Result<Vec<Value>, crate::error::DunkError> {
            Ok(self.0.clone())
        }
    }
    // Through the gate with the real process chain of this test. Under a tmux server the
    // caller is not detached, so only the pure checks above apply there.
    let caller_pid = std::process::id() as i64;
    let mut cursor = caller_pid;
    let mut ancestors = vec![caller_pid];
    let mut under_tmux = false;
    for _ in 0..crate::proc::MAX_DEPTH {
        let row = crate::proc::stat(cursor).unwrap();
        under_tmux |= row.comm == "tmux: server";
        if row.ppid <= 1 {
            break;
        }
        cursor = row.ppid;
        ancestors.push(cursor);
    }
    if under_tmux {
        return;
    }
    let (flock, dir) = bare("owner-token-audit");
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.join("roles.json")),
        Some(dir.join("tokens.json")),
        None,
    )
    .unwrap();
    let caller_start = crate::proc::process_start(caller_pid).unwrap();
    roles
        .register(
            OWNER,
            "owner",
            Some(caller_pid),
            Some(&caller_start),
            None,
            None,
            None,
            None,
        )
        .unwrap();
    let token = roles.issue_token("owner", Some("test"), None).unwrap();
    flock.attach_roles(roles, None);
    let check = |rows: Vec<Value>| {
        flock.clear_caller();
        flock.set_caller(Some(
            json!({"pid":caller_pid,"start":caller_start,"source":"peer"}),
        ));
        flock.set_service_token(Some(&token));
        flock.set_socket_peer(Some(true));
        flock.with_caller(|state| state.socket_peer_uid = Some(nix::unistd::geteuid().as_raw()));
        flock.caller_role(None, &Panes(rows))
    };
    let recorded = || flock.detached_owner.lock().unwrap().clone();
    // In an owner pane: accepted, not detached, no record.
    assert_eq!(
        check(vec![json!({"pane_id":OWNER,"pane_pid":caller_pid})]).unwrap(),
        "owner"
    );
    assert_eq!(recorded(), None);
    // Outside every listed pane: accepted as before, and recorded.
    let unrelated = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let rows = vec![json!({"pane_id":"%unrelated","pane_pid":unrelated.id()})];
    assert_eq!(check(rows.clone()).unwrap(), "owner");
    let call = recorded().expect("a detached accept is recorded");
    assert_eq!(
        (call.pid, call.root.0, call.root_seen),
        (caller_pid, *ancestors.last().unwrap(), "first")
    );
    assert_eq!(check(rows).unwrap(), "owner");
    assert_eq!(recorded().unwrap().root_seen, "same_root");
    let mut unrelated = unrelated;
    let _ = unrelated.kill();
    let _ = unrelated.wait();
}

// ---- t4782: the system token is accepted only from where the daemon's services run ----

#[test]
fn the_unit_of_a_cgroup_line_and_which_units_are_the_daemons_services() {
    use super::roles_gate::{is_system_unit, unit_of_cgroup};
    let timer = "0::/user.slice/user-1000.slice/user@1000.service/app.slice/dunkingsheep-watchdog.service\n";
    assert_eq!(
        unit_of_cgroup(timer).as_deref(),
        Some("dunkingsheep-watchdog.service")
    );
    let pane = "0::/user.slice/user-1000.slice/user@1000.service/tmux-spawn-1234.scope\n";
    assert_eq!(
        unit_of_cgroup(pane).as_deref(),
        Some("tmux-spawn-1234.scope")
    );
    assert_eq!(unit_of_cgroup("0::/user.slice/user-1000.slice\n"), None);
    assert_eq!(
        unit_of_cgroup("1:name=systemd:/x.service\n"),
        None,
        "v1 only"
    );
    assert_eq!(unit_of_cgroup(""), None);

    for unit in [
        "dunkingsheep-watchdog.service",
        "dunkingsheep-health.service",
        "dunkingsheep-review.service",
        "dunkingsheep-watchdog-deadman.service",
    ] {
        assert!(is_system_unit(unit), "{unit}");
    }
    for unit in [
        "dunkingsheep.service",
        "dunkingsheep-.service",
        "dunkingsheep-watchdog.timer",
        "dunkingsheep-watchdog.scope",
        "tmux-spawn-1234.scope",
        "evil-dunkingsheep-watchdog.service",
        "dunkingsheep-a b.service",
        "",
    ] {
        assert!(!is_system_unit(unit), "{unit}");
    }
}

#[test]
fn a_system_token_is_accepted_from_a_service_unit_and_refused_from_a_detached_stranger() {
    use super::roles_gate::{system_token_verdict, SystemPeer, SystemTokenVerdict};
    let listed = vec!["%7".to_string()];
    // Matching peers: one of the daemon's units, or a pane the owner listed for this label.
    assert_eq!(
        system_token_verdict(
            "timers",
            &SystemPeer::Unit("dunkingsheep-watchdog.service".into()),
            &[]
        ),
        SystemTokenVerdict::Accept
    );
    assert_eq!(
        system_token_verdict("cos", &SystemPeer::Panes(vec!["%7".into()]), &listed),
        SystemTokenVerdict::Accept
    );
    // Non-matching: outside every pane and not a unit is refused.
    let SystemTokenVerdict::Refuse(line) =
        system_token_verdict("timers", &SystemPeer::Detached, &listed)
    else {
        panic!("a detached stranger is refused");
    };
    assert!(
        line.contains("'timers'") && line.contains("refused"),
        "{line}"
    );
    // In a pane the owner did not list (or only partly listed): accepted this release, with a
    // warning that names the label and the pane.
    for panes in [
        vec!["%9".to_string()],
        vec!["%7".to_string(), "%9".to_string()],
    ] {
        let SystemTokenVerdict::Warn(line) =
            system_token_verdict("cos", &SystemPeer::Panes(panes.clone()), &listed)
        else {
            panic!("an unlisted pane warns: {panes:?}");
        };
        assert!(line.starts_with("WARNING system token 'cos'"), "{line}");
        assert!(line.contains("%9"), "{line}");
    }
    // The timers token from a pane is refused now; a pane the owner listed for it is not.
    let SystemTokenVerdict::Refuse(line) =
        system_token_verdict("timers", &SystemPeer::Panes(vec!["%9".into()]), &listed)
    else {
        panic!("timers from a pane is refused");
    };
    assert!(line.contains("'timers'") && line.contains("%9"), "{line}");
    assert!(matches!(
        system_token_verdict("timers", &SystemPeer::Panes(vec![]), &listed),
        SystemTokenVerdict::Refuse(_)
    ));
    assert_eq!(
        system_token_verdict("timers", &SystemPeer::Panes(vec!["%7".into()]), &listed),
        SystemTokenVerdict::Accept
    );
    assert!(matches!(
        system_token_verdict("cos", &SystemPeer::Panes(vec![]), &listed),
        SystemTokenVerdict::Warn(_)
    ));
    assert!(matches!(
        system_token_verdict("cos", &SystemPeer::Unknown, &listed),
        SystemTokenVerdict::Warn(_)
    ));
}

#[test]
fn the_owners_pane_list_for_a_system_token_is_read_by_label_and_fails_empty() {
    let (flock, dir) = bare("system-token-panes");
    let panes = |label: &str| {
        flock
            .with_roles(|roles, _| Ok(roles.system_token_panes(label)))
            .unwrap()
    };
    assert!(panes("cos").is_empty(), "no file lists nothing");
    let file = dir.join(crate::roles::SYSTEM_TOKEN_PANES_FILE);
    std::fs::write(&file, r#"{"cos": ["%7", " %8 ", "", 4], "other": "%9"}"#).unwrap();
    assert_eq!(panes("cos"), ["%7", "%8"]);
    assert!(panes("other").is_empty(), "a label needs a list");
    assert!(panes("timers").is_empty());
    std::fs::write(&file, "{").unwrap();
    assert!(panes("cos").is_empty(), "a malformed file lists nothing");
}

#[test]
fn a_system_token_over_the_socket_is_checked_against_where_the_caller_runs() {
    struct Panes(Vec<Value>);
    impl PaneInfoEnv for Panes {
        fn list_panes_grouped(&self) -> Vec<Value> {
            self.0.clone()
        }
        fn get_pane(&self, id: &str) -> Option<Value> {
            self.0.iter().find(|p| p["pane_id"] == id).cloned()
        }
        fn try_authority_panes(&self) -> Result<Vec<Value>, crate::error::DunkError> {
            Ok(self.0.clone())
        }
    }
    let (flock, dir) = bare("system-token-peer");
    let caller_pid = std::process::id() as i64;
    let caller_start = crate::proc::process_start(caller_pid).unwrap();
    // The pane root this test process sits under: its tmux pane when run inside one, else
    // the test process itself stands in for a pane root.
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
    let pane_root = tmux_root.unwrap_or(caller_pid);
    let mut roles = Roles::new(
        &Env::default(),
        Some(dir.join("roles.json")),
        Some(dir.join("tokens.json")),
        None,
    )
    .unwrap();
    let token = roles.issue_token("system", Some("cos"), None).unwrap();
    flock.attach_roles(roles, None);
    let caller = json!({"pid": caller_pid, "start": caller_start, "source": "peer"});
    let check = |rows: Vec<Value>| {
        flock.clear_caller();
        flock.set_caller(Some(caller.clone()));
        flock.set_service_token(Some(&token));
        flock.set_socket_peer(Some(true));
        flock.with_caller(|state| state.socket_peer_uid = Some(nix::unistd::geteuid().as_raw()));
        flock.caller_role(None, &Panes(rows))
    };

    // Inside a listed pane the owner has not named for this label: accepted this release.
    let mine = json!({"pane_id": WORKER, "pane_pid": pane_root});
    assert_eq!(check(vec![mine.clone()]).unwrap(), "system");
    // Named by the owner for this label: accepted.
    std::fs::write(
        dir.join(crate::roles::SYSTEM_TOKEN_PANES_FILE),
        format!(r#"{{"cos": ["{WORKER}"]}}"#),
    )
    .unwrap();
    assert_eq!(check(vec![mine]).unwrap(), "system");

    // The timers token from the same unlisted pane is refused, with no token in the text.
    let timers = flock
        .with_roles(|roles, _| roles.issue_token("system", Some("timers"), None))
        .unwrap();
    flock.clear_caller();
    flock.set_caller(Some(caller.clone()));
    flock.set_service_token(Some(&timers));
    flock.set_socket_peer(Some(true));
    flock.with_caller(|state| state.socket_peer_uid = Some(nix::unistd::geteuid().as_raw()));
    let refused = flock
        .caller_role(
            None,
            &Panes(vec![json!({"pane_id": OTHER, "pane_pid": pane_root})]),
        )
        .unwrap_err()
        .to_string();
    assert!(refused.starts_with("system token refused:"), "{refused}");
    assert!(!refused.contains(&timers));

    // Outside every listed pane, and this test process is not a dunkingsheep service unit.
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
    let elsewhere = check(vec![
        json!({"pane_id": "%unrelated", "pane_pid": child.0.id()}),
    ]);
    if tmux_root.is_some() {
        // Run from inside a tmux pane that this listing does not account for: the place
        // cannot be established, which is accepted with a warning for this release.
        assert_eq!(elsewhere.unwrap(), "system");
    } else {
        let refused = elsewhere.unwrap_err().to_string();
        assert!(refused.starts_with("system token refused:"), "{refused}");
        assert!(
            !refused.contains(&token),
            "the token is never in the refusal"
        );
    }

    // An in-process caller (no socket request) is trusted code and is not checked.
    flock.clear_caller();
    flock.clear_socket_peer();
    flock.set_service_token(Some(&token));
    assert_eq!(flock.caller_role(None, &NoPanes).unwrap(), "system");
}
