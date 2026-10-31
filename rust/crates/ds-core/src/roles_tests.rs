//! Ports of tests/test_dunk_roles.py: the desk's narrow grant (t1170) and the allowlist shape.

use super::*;

#[test]
fn the_desk_has_a_narrow_grant_of_reversible_queue_actions_and_nothing_destructive() {
    // owner decision 2026-10-31: block, unblock and update. Assign is NOT granted (its
    // handler refuses a desk anyway, t1172), so the allowlist must not promise it.
    for verb in ["task_block", "task_unblock", "task_update"] {
        assert!(allowed("desk", verb), "{verb}");
    }
    for verb in [
        "task_assign",
        "task_cancel",
        "task_delete",
        "task_complete",
        "task_next",
        "respawn_pane",
        "start_agent",
        "send_keys",
        "send_text",
        "lane_set",
        "close",
        "shutdown",
        "create_workspace",
        "superuser_add",
        "retire_pane",
        "add_dunk",
        "stop_all",
        "gh_token",
        "token_issue",
    ] {
        assert!(!allowed("desk", verb), "the desk must not have {verb}");
    }
}

#[test]
fn the_rest_of_the_allowlist_shape() {
    assert!(allowed("owner", "shutdown"));
    assert!(!allowed("web", "shutdown"));
    assert!(allowed("web", "send_text"));
    assert!(!allowed("desk", "send_text"));
    assert!(!allowed("desk", "inbox_send"));
    assert!(!allowed("desk", "task_complete"));
    assert!(allowed("desk", "task_add"));
    assert!(!allowed("planner", "inbox_send"));
    assert!(!allowed("anonymous", "task_next"));
    assert!(allowed("anonymous", "task_list"));
    assert!(!allowed("worker", "task_assign"));
    assert!(!allowed("system", "send_text"));
}

#[test]
fn every_role_may_read_and_an_unknown_role_gets_only_that() {
    for role in [
        "desk",
        "planner",
        "worker",
        "reviewer",
        "system",
        "anonymous",
        "nobody",
    ] {
        assert!(allowed(role, "task_list"), "{role}");
        assert!(allowed(role, "status"), "{role}");
    }
    assert!(
        !allowed("nobody", "task_add"),
        "an unknown role is read-only, like ALLOW.get(role, READ_ONLY)"
    );
}

#[test]
fn a_worker_and_a_reviewer_share_one_grant() {
    for verb in WORKER_EXTRA {
        assert_eq!(allowed("worker", verb), allowed("reviewer", verb), "{verb}");
        assert!(allowed("worker", verb), "{verb}");
    }
    assert!(allowed("worker", "task_next") && allowed("reviewer", "task_complete"));
}

#[test]
fn the_tables_have_the_pythons_sizes() {
    // Sizes of the sets in dunk_roles.py when these tables were generated (t1170). A
    // one-sided edit to either language changes a count here or fails a Python test.
    assert_eq!(47, READ_ONLY.len());
    assert_eq!(28, WORKER_EXTRA.len());
    assert_eq!(30, SYSTEM_EXTRA.len());
    assert_eq!(15, DESK_EXTRA.len());
    assert_eq!(3, PLANNER_EXTRA.len());
    assert_eq!(30, WEB_DENIED.len());
}

#[test]
fn the_desk_grant_is_exactly_the_reviewed_set() {
    let mut desk: Vec<&str> = DESK_EXTRA.to_vec();
    desk.sort_unstable();
    assert_eq!(
        vec![
            "inbox_ack",
            "inbox_fetch",
            "inbox_read",
            "owner_conversation",
            "owner_outbox",
            "owner_poll",
            "owner_reply",
            "owner_take",
            "read_pane",
            "report_run",
            "task_add",
            "task_block",
            "task_note",
            "task_unblock",
            "task_update",
        ],
        desk
    );
}

#[test]
fn web_may_do_everything_but_the_fleet_shaping_verbs() {
    for verb in WEB_DENIED {
        assert!(!allowed("web", verb), "{verb}");
        assert!(allowed("owner", verb), "owner may call {verb}");
    }
}

fn inbox_result(n: usize, size: usize) -> serde_json::Value {
    let messages: Vec<_> = (0..n)
        .map(|i| serde_json::json!({"message_id": format!("i{i}"), "sender": "x", "subject": format!("s{i}"), "body": "b".repeat(size)}))
        .collect();
    serde_json::json!({"mailbox": "%133", "messages": messages})
}

#[test]
fn over_cap_inbox_keeps_newest_messages_and_the_messages_key() {
    let out = clamp_desk_result(&inbox_result(10, 900));
    assert_eq!(Some(true), out["truncated"].as_bool());
    let msgs = out["messages"].as_array().expect("messages key kept");
    assert_eq!("i9", msgs.last().unwrap()["message_id"]);
    assert_eq!((10 - msgs.len()) as u64, out["omitted"].as_u64().unwrap());
    assert!(py_dumps(&out).len() <= DESK_RESULT_BYTES);
}

#[test]
fn under_cap_result_is_untouched() {
    let r = inbox_result(2, 10);
    assert_eq!(r, clamp_desk_result(&r));
}

#[test]
fn over_cap_result_without_messages_is_the_capped_shape_and_prints_its_head() {
    let big = serde_json::json!({"rows": "r".repeat(9000)});
    let out = clamp_desk_result(&big);
    assert!(out.get("messages").is_none());
    assert_eq!(DESK_RESULT_BYTES as u64, out["cap"].as_u64().unwrap());
    let text = capped_result_text(&out);
    assert!(text.contains("result capped by the desk role") && text.contains("rrrr"));
}

#[test]
fn non_ascii_is_escaped_like_python_so_the_clamp_fires_at_the_same_size() {
    // Python: len(json.dumps({"x": "\u2014" * 1000})) == 6009, over the 4096 cap.
    let dashes = serde_json::json!({"x": "\u{2014}".repeat(1000)});
    assert_eq!(6009, py_dumps(&dashes).len());
    assert_eq!(
        Some(true),
        clamp_desk_result(&dashes)["truncated"].as_bool()
    );
    // astral: a surrogate pair, 12 chars (Python '\ud83d\ude00').
    assert_eq!(
        r#""\ud83d\ude00""#,
        py_dumps(&serde_json::json!("\u{1F600}"))
    );
}

#[test]
fn a_missing_bytes_or_cap_prints_none_like_python() {
    let text = capped_result_text(&serde_json::json!({"head": "h"}));
    assert!(text.contains("(None bytes > None)"), "{text}");
}

// -- T31a: the Roles registry (tests/test_dunk_roles.py RegistryTests) ----------------

mod registry {
    use serde_json::json;

    use crate::config::Env;
    use crate::roles::Roles;

    const OWNER: &str = "w1:p2";
    const WORKER: &str = "w2:p3";
    const DESK: &str = "w2:p1";
    const OTHER: &str = "w2:p9";

    fn fresh(tag: &str) -> (std::path::PathBuf, Roles) {
        let dir = std::env::temp_dir().join(format!("ds-roles-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let roles = Roles::new(
            &Env::default(),
            Some(dir.join("roles.json")),
            Some(dir.join("tokens.json")),
            None,
        )
        .unwrap();
        (dir, roles)
    }

    fn reopen(roles: &Roles) -> Roles {
        Roles::new(
            &Env::default(),
            Some(roles.path.clone()),
            Some(roles.tokens_path.clone()),
            None,
        )
        .unwrap()
    }

    fn register(roles: &mut Roles, pane: &str, role: &str) {
        roles
            .register(pane, role, None, None, None, None, None, None)
            .unwrap();
    }

    #[test]
    fn unregistered_pane_is_a_worker_until_the_first_registration() {
        let (_dir, mut roles) = fresh("bootstrap");
        assert_eq!(roles.role_of(Some("w9:p9"), None).unwrap(), "worker");
        assert_eq!(roles.role_of(None, None).unwrap(), "anonymous");
        register(&mut roles, OWNER, "owner");
        assert_eq!(roles.role_of(Some("w9:p9"), None).unwrap(), "anonymous");
        assert_eq!(reopen(&roles).default, "anonymous");
    }

    #[test]
    fn a_label_grants_nothing() {
        let (_dir, mut roles) = fresh("label");
        register(&mut roles, OWNER, "owner");
        let info = json!({ "label": "owner", "tab_label": "owner" });
        assert_eq!(
            roles.role_of(Some(OTHER), Some(&info)).unwrap(),
            "anonymous"
        );
    }

    #[test]
    fn a_reused_pane_id_with_a_new_root_pid_loses_the_role() {
        let (_dir, mut roles) = fresh("reuse");
        roles
            .register(
                WORKER,
                "worker",
                Some(4100),
                Some("100"),
                None,
                None,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            roles
                .role_of(Some(WORKER), Some(&json!({ "pane_pid": 4100 })))
                .unwrap(),
            "worker"
        );
        assert_eq!(
            roles
                .role_of(Some(WORKER), Some(&json!({ "pane_pid": 9999 })))
                .unwrap(),
            "anonymous"
        );
        // Unverifiable without a live record: the default role.
        assert_eq!(
            roles.role_of(Some(WORKER), Some(&json!({}))).unwrap(),
            "anonymous"
        );
        assert_eq!(roles.role_of(Some(WORKER), None).unwrap(), "anonymous");
        // A registration without a pid (a fake backend) still holds.
        register(&mut roles, DESK, "desk");
        assert_eq!(roles.role_of(Some(DESK), None).unwrap(), "desk");
    }

    #[test]
    fn file_is_private_and_reread_when_changed() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, mut roles) = fresh("private");
        register(&mut roles, OWNER, "owner");
        let mode = std::fs::metadata(&roles.path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let mut other = reopen(&roles);
        other
            .register(DESK, "desk", None, None, Some("pi"), None, None, None)
            .unwrap();
        assert_eq!(roles.role_of(Some(DESK), None).unwrap(), "desk");
    }

    #[test]
    fn tokens_hash_at_rest_and_resolve_to_a_role() {
        let (_dir, mut roles) = fresh("tokens");
        let token = roles.issue_token("web", Some("desk-web"), None).unwrap();
        assert_eq!(
            token.len(),
            43,
            "token_urlsafe(32) is 43 base64url characters"
        );
        let at_rest = std::fs::read_to_string(&roles.tokens_path).unwrap();
        assert!(!at_rest.contains(&token));
        assert_eq!(roles.token_role(&token).unwrap().as_deref(), Some("web"));
        assert_eq!(roles.token_role("nope").unwrap(), None);
        assert_eq!(roles.revoke_token("desk-web").unwrap(), 1);
        assert_eq!(roles.token_role(&token).unwrap(), None);
        assert!(
            roles.issue_token("worker", None, None).is_err(),
            "not a token role"
        );
    }

    #[test]
    fn token_hashing_is_sha256_hex_like_the_python() {
        assert_eq!(
            crate::roles::hash_token("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn reviewers_must_be_on_the_provider_list() {
        let (_dir, mut roles) = fresh("providers");
        roles
            .register(
                "w3:p1",
                "reviewer",
                None,
                None,
                Some("pi"),
                None,
                None,
                None,
            )
            .unwrap();
        roles
            .register(
                "w3:p2",
                "reviewer",
                None,
                None,
                Some("claude"),
                None,
                None,
                None,
            )
            .unwrap();
        assert!(!roles.may_review("w3:p1", None).unwrap());
        assert!(roles.may_review("w3:p2", None).unwrap());
    }

    #[test]
    fn may_review_trusts_the_resolved_role_of_a_pid_bound_pane() {
        // Regression 2026-10-29: re-resolving without the live record demoted the reviewer.
        let (_dir, mut roles) = fresh("resolved");
        roles
            .register(
                "w3:p3",
                "reviewer",
                Some(4321),
                None,
                Some("claude"),
                None,
                None,
                None,
            )
            .unwrap();
        assert!(
            !roles.may_review("w3:p3", None).unwrap(),
            "no live record here"
        );
        assert!(roles.may_review("w3:p3", Some("reviewer")).unwrap());
    }

    #[test]
    fn a_registration_needs_a_pane_and_a_lane_role() {
        let (_dir, mut roles) = fresh("refusals");
        let blank = roles
            .register("  ", "worker", None, None, None, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(blank.contains("a registration needs a pane id"), "{blank}");
        let bad = roles
            .register(OWNER, "web", None, None, None, None, None, None)
            .unwrap_err()
            .to_string();
        assert!(
            bad.contains("role must be one of desk, planner, worker, reviewer, owner; got \"web\""),
            "{bad}"
        );
    }

    #[test]
    fn a_copy_of_the_fixture_roles_file_resolves_every_row() {
        let dir = std::env::temp_dir().join(format!("ds-roles-fixture-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roles.json");
        std::fs::write(&path, include_str!("../../../fixtures/state/roles.json")).unwrap();
        let mut roles = Roles::new(
            &Env::default(),
            Some(path),
            Some(dir.join("tokens.json")),
            None,
        )
        .unwrap();
        assert_eq!(roles.role_of(Some("%7"), None).unwrap(), "worker");
        assert_eq!(roles.role_of(Some("%9"), None).unwrap(), "reviewer");
        assert_eq!(roles.role_of(Some("%1"), None).unwrap(), "anonymous");
        assert!(roles.may_review("%9", Some("reviewer")).unwrap());
        assert!(!roles.has_owner().unwrap());
    }

    #[test]
    fn an_unparseable_roles_file_is_a_named_refusal() {
        let dir = std::env::temp_dir().join(format!("ds-roles-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("roles.json");
        std::fs::write(&path, "{not json").unwrap();
        let refused = Roles::new(&Env::default(), Some(path), Some(dir.join("t.json")), None)
            .unwrap_err()
            .to_string();
        assert!(
            refused.contains("is not valid JSON; fix it or remove it"),
            "{refused}"
        );
    }
}
