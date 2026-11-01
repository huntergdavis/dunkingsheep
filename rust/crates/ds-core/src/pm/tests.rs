//! Ported from tests/test_dunk_pm.py, same order and names. NOT ported:
//! `test_one_digest_row_per_project_updated_in_place` and
//! `test_lane_origin_count_separates_lane_mail_from_pm_mail`: they test `Inbox.upsert_digest`
//! and `Inbox.count(lane_panes=...)`, which belong to inbox.rs (no `upsert_digest` in Rust yet).

use super::*;

fn env() -> Env {
    Env::from_pairs(&[("HOME", "/home/x")])
}

fn routing_file(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("pm-routing.json");
    fs::write(&path, text).expect("write routing");
    path
}

fn standard(dir: &Path) -> Routing {
    let path = routing_file(
        dir,
        &json!({
            "poolrad": {"pm": "w1:p1", "members": ["w1:p2"], "mode": "redirect", "vp_pane": "w9:pH"},
            "crackdown": {"pm": "w2:p1", "members": ["w2:p2"], "mode": "flag", "vp_pane": "w9:pH"},
        })
        .to_string(),
    );
    load_routing(Some(&path), None, &env()).expect("routing")
}

fn triple(a: &str, b: &str, c: &str) -> Option<(String, String, String)> {
    Some((a.into(), b.into(), c.into()))
}

// -- RoutingTests ---------------------------------------------------------------------

#[test]
fn test_lane_messages_the_vp_are_redirected_to_its_pm() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        redirect(Some("w1:p2"), &standard(dir.path())),
        triple("w1:p1", "poolrad", "redirect")
    );
}

#[test]
fn test_pm_is_never_redirected_to_itself() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(redirect(Some("w1:p1"), &standard(dir.path())), None);
}

#[test]
fn test_a_non_member_is_left_alone() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(redirect(Some("w9:pZ"), &standard(dir.path())), None);
}

#[test]
fn test_flag_mode_is_reported_not_redirected() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        redirect(Some("w2:p2"), &standard(dir.path())),
        triple("w2:p1", "crackdown", "flag")
    );
}

#[test]
fn test_bad_routing_is_a_named_refusal() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = routing_file(dir.path(), r#"{"poolrad": {"members": []}}"#);
    let error = load_routing(Some(&path), None, &env()).expect_err("refused");
    assert_eq!(error.to_string(), "PM routing poolrad has no pm pane");
}

#[test]
fn test_missing_routing_is_empty_not_an_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    assert_eq!(
        load_routing(Some(&dir.path().join("absent.json")), None, &env()),
        Ok(Vec::new())
    );
}

// -- DigestTests ----------------------------------------------------------------------

#[test]
fn test_digest_has_four_sections_and_evidence_refs() {
    let numbers = [
        json!({"name": "strict C", "value": 252, "unit": "/286", "evidence_ref": "build log line 41"}),
    ];
    let body = render_digest(
        "poolrad",
        &Digest {
            changed: &["strict C 240 -> 252".to_string()],
            blocked: &["keystone unknown".to_string()],
            owner_decisions: &["accept the late-answer rule?".to_string()],
            numbers: &numbers,
            ..Digest::default()
        },
    );
    for heading in [
        "## Changed",
        "## Blocked",
        "## Owner decisions",
        "## Numbers",
    ] {
        assert!(body.contains(heading), "{heading}");
    }
    assert!(body.contains("build log line 41"));
    assert!(body.contains("- strict C: 252 /286 (evidence: build log line 41)"));
}

#[test]
fn test_poolrad_numbers_are_read_at_publish_time_and_carry_as_of() {
    let dir = tempfile::tempdir().expect("tempdir");
    let checkpoint = dir.path().join("checkpoint.json");
    fs::write(
        &checkpoint,
        json!({"strict_full_c_originals": 280, "verified_game_identities": 325,
            "reviewed_change": {"reviewed_commit": "df9ca4e0"}})
        .to_string(),
    )
    .expect("write");
    let numbers = checkpoint_numbers("poolrad", Some(&checkpoint), &env()).expect("numbers");
    assert_eq!(numbers[0]["value"], 280);
    assert_eq!(numbers[0]["unit"], "/325");
    assert!(numbers[0]["evidence_ref"]
        .as_str()
        .expect("ref")
        .contains("df9ca4e0"));
    let as_of = numbers[0]["as_of"].as_str().expect("as_of");
    let re = regex::Regex::new(r"^\d{4}-\d\d-\d\d \d\d:\d\d$").expect("regex");
    assert!(re.is_match(as_of), "{as_of}");
    let body = render_digest(
        "poolrad",
        &Digest {
            numbers: &numbers,
            ..Digest::default()
        },
    );
    assert!(body.contains("280 /325"));
    assert!(body.contains("as of"));
}

#[test]
fn test_a_failed_checkpoint_read_says_unavailable_not_yesterday() {
    let dir = tempfile::tempdir().expect("tempdir");
    let absent = dir.path().join("absent.json");
    let numbers = checkpoint_numbers("poolrad", Some(&absent), &env()).expect("numbers");
    assert_eq!(numbers[0]["value"], "unavailable");
    assert_eq!(numbers[0]["as_of"], Value::Null);
    assert_eq!(
        numbers[0]["evidence_ref"],
        format!(
            "{p}: [Errno 2] No such file or directory: '{p}'",
            p = absent.display()
        )
    );
}

#[test]
fn test_seven_blocked_items_get_a_pm_note() {
    let blocked: Vec<String> = (0..7).map(|i| format!("b{i}")).collect();
    let body = render_digest(
        "poolrad",
        &Digest {
            blocked: &blocked,
            ..Digest::default()
        },
    );
    assert!(body.contains("past six this is no longer a digest"));
}

// -- beyond the Python tests ----------------------------------------------------------

#[test]
fn test_an_empty_digest_says_nothing_in_every_section() {
    let body = render_digest(
        "p",
        &Digest {
            note: Some("hello"),
            ..Digest::default()
        },
    );
    assert_eq!(
        body,
        "# PM digest: p\n_window: 30 min_\n\nhello\n\n## Changed\n- nothing\n\n## Blocked\n- nothing\n\n## Owner decisions\n- nothing\n\n## Numbers\n- nothing\n"
    );
}

#[test]
fn test_a_number_without_unit_or_evidence_has_no_trailing_space() {
    let numbers = [json!({"name": "n", "value": 3})];
    let body = render_digest(
        "p",
        &Digest {
            numbers: &numbers,
            ..Digest::default()
        },
    );
    assert!(
        body.contains("\n- n: 3 (evidence: no evidence ref)\n"),
        "{body}"
    );
}

#[test]
fn test_only_poolrad_has_a_checkpoint_source() {
    assert_eq!(checkpoint_numbers("crackdown", None, &env()), None);
}

#[test]
fn test_routing_defaults_and_refusals() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = routing_file(
        dir.path(),
        r#"{"a": {"pm": " w1:p1 ", "members": [" w1:p2 "]}}"#,
    );
    let routing = load_routing(Some(&path), Some("w7:p7"), &env()).expect("ok");
    assert_eq!(
        routing[0].1,
        Entry {
            pm: "w1:p1".into(),
            members: vec!["w1:p2".into()],
            mode: "redirect".into(),
            vp_pane: "w7:p7".into()
        }
    );
    let routing = load_routing(Some(&path), None, &env()).expect("ok");
    assert_eq!(routing[0].1.vp_pane, "w9:pH");
    for (text, want) in [
        (
            "[]",
            format!(
                "PM routing {} must be an object of projects",
                path.display()
            ),
        ),
        (r#"{"a": 1}"#, "PM routing a must be an object".to_string()),
        (
            r#"{"a": {"pm": "x", "mode": "kill"}}"#,
            "PM routing a mode must be redirect or flag".to_string(),
        ),
    ] {
        fs::write(&path, text).expect("write");
        assert_eq!(
            load_routing(Some(&path), None, &env())
                .expect_err("refused")
                .0,
            want
        );
    }
}

#[test]
fn test_parse_payload() {
    assert_eq!(
        parse_payload(Some("  ")).expect_err("empty").0,
        "a digest needs a body on stdin"
    );
    assert_eq!(
        parse_payload(None).expect_err("none").0,
        "a digest needs a body on stdin"
    );
    assert_eq!(
        parse_payload(Some(" plain body ")).expect("note")["note"],
        "plain body"
    );
    assert_eq!(
        parse_payload(Some(r#"{"changed": ["x"]}"#)).expect("json")["changed"],
        json!(["x"])
    );
    assert!(parse_payload(Some("{nope"))
        .expect_err("bad")
        .0
        .starts_with("digest payload is not valid JSON: "));
    assert_eq!(digest_id("poolrad"), "pm-digest:poolrad");
}
