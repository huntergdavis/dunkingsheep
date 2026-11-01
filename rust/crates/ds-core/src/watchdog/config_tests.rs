//! Ported by name from `ConfigTests` in `tests/test_dunk_watchdog.py` (t1400 slice A). The CLI test
//! (`test_cli_refuses_bad_config_...`) belongs to the CLI slice. The `extra_` tests cover behaviour
//! this file ports but the Python suite does not name.

use serde_json::{json, Value};

use super::*;

const EXAMPLE_TEXT: &str = include_str!("../../../../../packaging/watchdog.example.json");

fn example() -> Value {
    serde_json::from_str(EXAMPLE_TEXT).expect("the shipped example is JSON")
}

fn named(mutate: impl FnOnce(&mut Value), key: &str) {
    let mut config = example();
    mutate(&mut config);
    let error = validate_config(&mut config).expect_err("the mutation must be refused");
    assert_eq!(key, error.key, "{error}");
}

#[test]
fn test_example_config_is_valid_and_quiet() {
    let mut config = example();
    validate_config(&mut config).unwrap();
    assert_eq!(config["enabled"], json!(false));
    assert_eq!(config["dry_run"], json!(true));
    assert_eq!(config["escalation"]["owner"]["enabled"], json!(false));
}

#[test]
fn test_missing_enabled_or_dry_run_is_an_error_not_a_default() {
    named(
        |c| drop(c.as_object_mut().unwrap().remove("enabled")),
        "enabled",
    );
    named(
        |c| drop(c.as_object_mut().unwrap().remove("dry_run")),
        "dry_run",
    );
}

#[test]
fn test_zero_and_negative_values_are_refused() {
    named(
        |c| c["rules"]["host.load"]["load1_per_cpu"] = json!(0),
        "rules.host.load.load1_per_cpu",
    );
    named(
        |c| c["rules"]["dunk.overdue"]["cooldown_minutes"] = json!(0),
        "rules.dunk.overdue.cooldown_minutes",
    );
    named(
        |c| c["escalation"]["owner"]["max_pages_per_hour"] = json!(0),
        "escalation.owner.max_pages_per_hour",
    );
    named(
        |c| c["rules"]["task.blocked_stale"]["threshold_minutes"] = json!(-5),
        "rules.task.blocked_stale.threshold_minutes",
    );
    named(
        |c| c["rules"]["host.disk_full"]["must_hold_runs"] = json!(0),
        "rules.host.disk_full.must_hold_runs",
    );
}

#[test]
fn test_named_errors() {
    named(|c| c["pagethreshold"] = json!(1), "pagethreshold");
    named(
        |c| c["rules"]["host.tmp"]["max_used_fraction"] = json!("high"),
        "rules.host.tmp.max_used_fraction",
    );
    named(
        |c| drop(c["rules"].as_object_mut().unwrap().remove("host.load")),
        "rules.host.load",
    );
    named(
        |c| c["panes"]["pane_id_pattern"] = json!("("),
        "panes.pane_id_pattern",
    );
    named(
        |c| c["rules"]["security.secret_text"]["allowlist_sha256"] = json!(["xyz"]),
        "rules.security.secret_text.allowlist_sha256",
    );
}

#[test]
fn test_ops_rules_cannot_be_given_an_owner_class() {
    named(
        |c| c["rules"]["host.tmp"]["class"] = json!("outage"),
        "rules.host.tmp.class",
    );
    named(
        |c| c["rules"]["host.load"]["class"] = json!("money"),
        "rules.host.load.class",
    );
}

#[test]
fn test_money_cannot_be_enabled_without_source_and_cap() {
    named(
        |c| c["rules"]["money.spend_projection"]["enabled"] = json!(true),
        "rules.money.spend_projection.enabled",
    );
}

#[test]
fn test_owner_paging_requires_an_approver() {
    named(
        |c| c["escalation"]["owner"]["enabled"] = json!(true),
        "escalation.owner.approved_by",
    );
}

// -- extra: texts and cross-checks the Python carries but its suite does not name ---------------

fn reason(mutate: impl FnOnce(&mut Value)) -> String {
    let mut config = example();
    mutate(&mut config);
    validate_config(&mut config).expect_err("refused").reason
}

#[test]
fn extra_the_reason_texts_are_the_pythons() {
    assert_eq!(
        "must be > 0, got 0",
        reason(|c| c["rules"]["host.load"]["load1_per_cpu"] = json!(0))
    );
    assert_eq!(
        "must be > 0, got -0.5",
        reason(|c| c["rules"]["host.load"]["load1_per_cpu"] = json!(-0.5))
    );
    assert_eq!(
        "must be an integer",
        reason(|c| c["max_alerts_per_run"] = json!(2.5))
    );
    assert_eq!(
        "must be a number",
        reason(|c| c["cadence_minutes"] = json!(true))
    );
    assert_eq!(
        "must be one of ['info', 'warning', 'critical'], got 'loud'",
        reason(|c| c["escalation"]["vp"]["min_severity"] = json!("loud"))
    );
    assert_eq!(
        "must be <= 50, got 51",
        reason(|c| c["rules"]["inbox.ambiguous_sender"]["sample_ids"] = json!(51))
    );
    assert_eq!(
        "must be <= 1, got 1.5",
        reason(|c| c["rules"]["host.tmp"]["max_used_fraction"] = json!(1.5))
    );
}

#[test]
fn extra_unknown_keys_are_named_before_missing_ones_and_sorted() {
    let mut config = example();
    config["zzz"] = json!(1);
    config["aaa"] = json!(1);
    config.as_object_mut().unwrap().remove("enabled");
    assert_eq!("aaa", validate_config(&mut config).unwrap_err().key);
}

#[test]
fn extra_a_non_object_names_the_path_or_root() {
    assert_eq!("<root>", validate_config(&mut json!([1])).unwrap_err().key);
    let mut config = example();
    config["stores"] = json!(3);
    let error = validate_config(&mut config).unwrap_err();
    assert_eq!(
        ("stores", "must be an object"),
        (error.key.as_str(), error.reason.as_str())
    );
}

#[test]
fn extra_cross_checks() {
    named(
        |c| {
            c["stuck_recovery"]["enabled"] = json!(true);
            c["stuck_recovery"]["observation_interval_s"] = json!(500);
            c["stuck_recovery"]["threshold_s"] = json!(120);
        },
        "stuck_recovery.observation_interval_s",
    );
    named(
        |c| {
            c["rules"]["host.tmp"]["max_used_fraction"] = json!(0.9);
            c["rules"]["host.tmp"]["critical_used_fraction"] = json!(0.5);
        },
        "rules.host.tmp.critical_used_fraction",
    );
    named(
        |c| {
            c["escalation"]["owner"]["enabled"] = json!(true);
            c["escalation"]["owner"]["approved_by"] = json!("vp");
            c["escalation"]["owner"]["classes"] = json!([]);
        },
        "escalation.owner.classes",
    );
    assert_eq!(
        "'outage' is owner-eligible, but host.tmp is not an approved owner rule (spec section 1); \
         use ops or project",
        reason(|c| c["rules"]["host.tmp"]["class"] = json!("outage"))
    );
}

#[test]
fn extra_load_config_refuses_a_missing_or_unreadable_file_naming_file() {
    let error = load_config("").unwrap_err();
    assert_eq!(
        ("<file>", "config file not found: "),
        (error.key.as_str(), error.reason.as_str())
    );
    let dir = std::env::temp_dir().join(format!("ds-wd-config-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let missing = dir.join("nope.json");
    assert_eq!(
        "<file>",
        load_config(missing.to_str().unwrap()).unwrap_err().key
    );
    let bad = dir.join("bad.json");
    std::fs::write(&bad, "{not json").unwrap();
    let error = load_config(bad.to_str().unwrap()).unwrap_err();
    assert!(
        error.reason.starts_with("unreadable or not JSON: "),
        "{error}"
    );
    let good = dir.join("good.json");
    std::fs::write(&good, EXAMPLE_TEXT).unwrap();
    assert_eq!(example(), load_config(good.to_str().unwrap()).unwrap());
}

#[test]
fn extra_a_config_that_predates_the_newer_rules_is_migrated_disabled() {
    let mut config = example();
    let rules = config["rules"].as_object_mut().unwrap();
    rules.remove("delivery.ambiguous_queued");
    rules.remove("fleet.assignment_starvation");
    let added = migrate_config(&mut config);
    assert_eq!(
        vec!["delivery.ambiguous_queued", "fleet.assignment_starvation"],
        added
    );
    let block = &config["rules"]["delivery.ambiguous_queued"];
    assert_eq!(block["enabled"], json!(false));
    assert_eq!(block["min_holds"], json!(HOLD_ATTEMPT_LIMIT_DEFAULT));
    assert_eq!(block["min_age_minutes"], json!(20));
    assert_eq!(
        config["rules"]["fleet.assignment_starvation"]["worker_panes"],
        json!(["configure-worker-pane"])
    );
    validate_config(&mut config).unwrap();
    // a second run adds nothing
    assert!(migrate_config(&mut config).is_empty());
}

#[test]
fn extra_an_older_rule_that_goes_missing_still_fails_loudly() {
    // migrate only fills RULE_ADDITIONS: losing host.load is an error, not a default.
    named(
        |c| drop(c["rules"].as_object_mut().unwrap().remove("host.load")),
        "rules.host.load",
    );
    let mut config = example();
    config["rules"]
        .as_object_mut()
        .unwrap()
        .remove("delivery.ambiguous_queued");
    assert!(validate_config(&mut config).is_ok());
}
