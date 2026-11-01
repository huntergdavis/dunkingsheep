//! Ported from `tests/test_dunk_fallback.py` (t359): the table IS the specification. Every tier
//! transition the owner named, plus the refusals.
//!
//! Not portable: `test_credit_must_be_a_mapping` (the credit is a typed `Credit` here, so a
//! non-mapping cannot be passed) and the dict-order reversal in
//! `test_same_numbers_same_target_twice` (Rust structs have no key order; the planner is called
//! twice and compared).

use super::*;

fn windows_for(provider: &str) -> &'static [&'static str] {
    match provider {
        "claude" => &["five_hour", "weekly_all"],
        "codex" => &["weekly"],
        "opencode-go" => &["five_hour", "weekly", "monthly"],
        "free" => &["monthly"],
        _ => &["weekly"],
    }
}

/// One provider reading shaped like `dunkingsheep usage --json`. Every required window is
/// present, so a test that wants a MISSING window builds the reading itself.
fn reading(provider: &str, percent: Option<f64>, status: &str) -> Reading {
    let windows = match percent {
        None => Vec::new(),
        Some(p) => windows_for(provider)
            .iter()
            .map(|n| Window {
                name: (*n).into(),
                percent: Percent::Number(p),
            })
            .collect(),
    };
    Reading {
        provider: provider.into(),
        status: status.into(),
        windows,
    }
}

fn ok(provider: &str, percent: f64) -> Reading {
    reading(provider, Some(percent), "ok")
}

fn none_ok(provider: &str) -> Reading {
    reading(provider, None, "ok")
}

fn win(name: &str, percent: f64) -> Window {
    Window {
        name: name.into(),
        percent: Percent::Number(percent),
    }
}

fn custom(provider: &str, windows: Vec<Window>) -> Reading {
    Reading {
        provider: provider.into(),
        status: "ok".into(),
        windows,
    }
}

fn all_full() -> Vec<Reading> {
    vec![
        ok("claude", 100.0),
        ok("codex", 90.0),
        ok("opencode-go", 80.0),
    ]
}
fn claude_spent() -> Vec<Reading> {
    vec![
        ok("claude", 0.0),
        ok("codex", 90.0),
        ok("opencode-go", 80.0),
    ]
}
fn claude_codex_spent() -> Vec<Reading> {
    vec![ok("claude", 0.0), ok("codex", 0.0), ok("opencode-go", 80.0)]
}
fn all_spent() -> Vec<Reading> {
    vec![
        ok("claude", 0.0),
        ok("codex", 0.0),
        ok("opencode-go", 0.0),
        none_ok("pi"),
    ]
}

fn pane(tier: &str, turn: Option<&str>, owned: bool, pane_id: &str) -> Value {
    json!({"pane_id": pane_id, "tier": tier, "turn_state": turn, "owned_by_mechanism": owned, "harness": tier})
}
fn p(tier: &str) -> Value {
    pane(tier, Some("idle"), true, "%9")
}

fn plan(pane: &Value, readings: &[Reading]) -> Value {
    plan_swap(pane, &credit_state(readings), None).unwrap()
}

// ---- CreditStateTests ----

#[test]
fn reduces_readings_to_three_states() {
    let mut readings = all_spent();
    readings.push(ok("pi2", 55.0));
    let state = credit_state(&readings);
    assert_eq!(state.state_of("claude"), "exhausted");
    assert_eq!(state.state_of("pi2"), "available");
}

#[test]
fn failed_reading_is_unknown_not_available() {
    let state = credit_state(&[reading("codex", None, "error")]);
    assert_eq!(state.state_of("codex"), "unknown");
}

#[test]
fn no_windows_is_unknown_not_plenty() {
    // pi reports a local session estimate, not plan quota: absence of numbers must never read
    // as "credit available".
    assert_eq!(credit_state(&[none_ok("pi")]).state_of("pi"), "unknown");
}

#[test]
fn rounding_tail_is_treated_as_spent() {
    assert_eq!(
        credit_state(&[ok("codex", 0.05)]).state_of("codex"),
        "exhausted"
    );
}

// ---- WindowValidationTests: CoS review i33219 finding 3 ----

#[test]
fn missing_required_window_is_unknown() {
    assert_eq!(
        credit_state(&[custom("codex", vec![])]).state_of("codex"),
        "unknown"
    );
}

#[test]
fn missing_one_of_several_required_windows_is_unknown() {
    let partial = custom("claude", vec![win("five_hour", 100.0)]);
    assert_eq!(credit_state(&[partial]).state_of("claude"), "unknown");
}

#[test]
fn nan_window_is_unknown() {
    let nan = custom("codex", vec![win("weekly", f64::NAN)]);
    assert_eq!(credit_state(&[nan]).state_of("codex"), "unknown");
}

#[test]
fn non_numeric_window_is_unknown() {
    let bad = custom(
        "codex",
        vec![Window {
            name: "weekly".into(),
            percent: Percent::NotANumber,
        }],
    );
    assert_eq!(credit_state(&[bad]).state_of("codex"), "unknown");
}

#[test]
fn free_capacity_is_never_assumed() {
    assert_eq!(tier_state(&Credit::default(), "free"), "unknown");
    let mut credit = Credit::default();
    credit.states.insert("free".into(), "available".into());
    assert_eq!(tier_state(&credit, "free"), "available");
}

#[test]
fn live_json_readings_convert_with_booleans_and_strings_as_non_numbers() {
    let live = json!([
        {"provider": "codex", "status": "ok", "windows": [{"name": "weekly", "percent_remaining": 42.0}]},
        {"provider": "claude", "status": "ok", "windows": [
            {"name": "five_hour", "percent_remaining": true}, {"name": "weekly_all", "percent_remaining": 50.0}]},
        {"provider": " ", "status": "ok", "windows": []},
    ]);
    let readings: Vec<Reading> = live
        .as_array()
        .unwrap()
        .iter()
        .map(Reading::from_json)
        .collect();
    let credit = credit_state(&readings);
    assert_eq!(credit.state_of("codex"), "available");
    assert_eq!(
        credit.state_of("claude"),
        "unknown",
        "a boolean percentage is not a number"
    );
    assert_eq!(credit.headroom.get("codex"), Some(&42.0));
    assert_eq!(credit.states.len(), 2, "a blank provider is skipped");
}

// ---- TransitionTableTests ----

#[test]
fn every_transition_row() {
    struct Row(
        &'static str,
        Value,
        Vec<Reading>,
        &'static str,
        &'static str,
    );
    let mut with_free = all_spent();
    with_free.push(ok("free", 100.0));
    let mut pi_back = vec![
        ok("claude", 100.0),
        ok("codex", 100.0),
        ok("opencode-go", 0.0),
    ];
    pi_back.push(ok("free", 100.0));
    let rows = vec![
        Row(
            "claude has credit: nothing to do",
            p("claude"),
            all_full(),
            "skip",
            "current_has_credit",
        ),
        Row(
            "claude spent -> codex",
            p("claude"),
            claude_spent(),
            "swap",
            "codex",
        ),
        Row(
            "claude+codex spent -> pi",
            p("claude"),
            claude_codex_spent(),
            "swap",
            "pi",
        ),
        Row(
            "claude+codex+pi spent, no free capacity -> NOTHING is swapped",
            p("claude"),
            all_spent(),
            "skip",
            "no_tier_has_credit",
        ),
        Row(
            "claude+codex+pi spent WITH a funded free row -> free",
            p("claude"),
            with_free,
            "swap",
            "free",
        ),
        Row(
            "codex spent -> pi",
            p("codex"),
            vec![
                ok("claude", 100.0),
                ok("codex", 0.0),
                ok("opencode-go", 80.0),
            ],
            "swap",
            "pi",
        ),
        Row(
            "codex spent, pi unknown, no free capacity -> NOTHING is swapped",
            p("codex"),
            vec![
                ok("claude", 100.0),
                ok("codex", 0.0),
                reading("opencode-go", None, "error"),
            ],
            "skip",
            "no_tier_has_credit",
        ),
        Row(
            "pi spent, no free capacity -> NOTHING is swapped",
            p("pi"),
            vec![
                ok("claude", 100.0),
                ok("codex", 100.0),
                ok("opencode-go", 0.0),
            ],
            "skip",
            "no_tier_has_credit",
        ),
        Row(
            "pi never falls BACK to claude or codex",
            p("pi"),
            pi_back,
            "swap",
            "free",
        ),
        Row(
            "free is the last tier: nothing after it",
            p("free"),
            all_spent(),
            "skip",
            "on_last_tier",
        ),
        Row(
            "mid-turn is never swapped",
            pane("claude", Some("working"), true, "%9"),
            claude_spent(),
            "skip",
            "mid_turn",
        ),
        Row(
            "unknown turn state is not idle",
            pane("claude", None, true, "%9"),
            claude_spent(),
            "skip",
            "turn_unknown",
        ),
        Row(
            "a lane the mechanism does not own is untouched",
            pane("claude", Some("idle"), false, "%9"),
            claude_spent(),
            "skip",
            "not_owned_by_mechanism",
        ),
        Row(
            "unknown current tier is refused, not guessed",
            p("mystery"),
            claude_spent(),
            "skip",
            "unknown_current_tier",
        ),
    ];
    for Row(name, pane_state, readings, action, expected) in rows {
        let decision = plan(&pane_state, &readings);
        assert_eq!(decision["action"], action, "{name}: {decision}");
        let key = if action == "swap" { "to" } else { "reason" };
        assert_eq!(decision[key], expected, "{name}: {decision}");
    }
}

#[test]
fn exhausted_everywhere_swaps_nothing_and_says_so() {
    // The owner's third acceptance case: credit exhausted everywhere. Nothing moves - not to
    // another provider and NOT to free, because free capacity is not assumed.
    let panes = vec![
        pane("claude", Some("idle"), true, "%1"),
        pane("codex", Some("idle"), true, "%2"),
        pane("pi", Some("idle"), true, "%3"),
        pane("free", Some("idle"), true, "%4"),
    ];
    let out = plan_all(&panes, &credit_state(&all_spent()), None).unwrap();
    assert_eq!(out["swapped"], json!([]));
    assert_eq!(out["summary"], "0 swap(s), 4 skip(s)");
    let reason = |id: &str| {
        out["decisions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|d| d["pane_id"] == id)
            .unwrap()["reason"]
            .clone()
    };
    for id in ["%1", "%2", "%3"] {
        assert_eq!(reason(id), "no_tier_has_credit", "{id}");
    }
    assert_eq!(reason("%4"), "on_last_tier");
}

#[test]
fn all_unknown_swaps_nothing() {
    let unknown = vec![
        reading("claude", None, "error"),
        reading("codex", None, "error"),
        reading("opencode-go", None, "error"),
    ];
    let out = plan_all(
        &[pane("codex", Some("idle"), true, "%1")],
        &credit_state(&unknown),
        None,
    )
    .unwrap();
    assert_eq!(out["swapped"], json!([]));
    assert_eq!(out["decisions"][0]["reason"], "current_credit_unknown");
}

#[test]
fn all_unknown_credit_never_swaps_to_a_provider() {
    let unknown = vec![
        reading("claude", None, "error"),
        reading("codex", None, "error"),
        reading("opencode-go", None, "error"),
    ];
    let decision = plan(&p("codex"), &unknown);
    assert_eq!(
        (decision["action"].as_str(), decision["reason"].as_str()),
        (Some("skip"), Some("current_credit_unknown"))
    );
}

// ---- PiTierTests: the board's "pi" reading is a local session estimate, not plan quota ----

#[test]
fn pi_tier_reads_opencode_go_credit() {
    let credit = credit_state(&[none_ok("pi"), ok("opencode-go", 96.9)]);
    assert_eq!(tier_state(&credit, "pi"), "available");
    assert_eq!(
        plan_swap(&p("pi"), &credit, None).unwrap()["reason"],
        "current_has_credit"
    );
}

#[test]
fn pi_exhausted_only_when_opencode_go_is() {
    let credit = credit_state(&[none_ok("pi"), ok("opencode-go", 0.0)]);
    assert_eq!(tier_state(&credit, "pi"), "exhausted");
}

#[test]
fn pi_unknown_when_the_only_quota_source_is_unknown() {
    let credit = credit_state(&[none_ok("pi"), reading("opencode-go", None, "error")]);
    assert_eq!(tier_state(&credit, "pi"), "unknown");
}

// ---- ReserveAndWindowPolicyTests: VP i33250, the above-5 reserve and all-reported-window ----

fn codex_at(percent: f64) -> Vec<Reading> {
    vec![ok("claude", 0.0), ok("codex", percent)]
}

#[test]
fn a_one_percent_target_is_not_a_landing_place() {
    // CoS reproduced 1% weekly codex being chosen. It has credit, so it is not exhausted - and
    // it is still not a target.
    let decision = plan(&p("claude"), &codex_at(1.0));
    assert_eq!(
        (decision["action"].as_str(), decision["reason"].as_str()),
        (Some("skip"), Some("targets_below_reserve"))
    );
}

#[test]
fn exactly_five_percent_is_refused_and_above_it_is_allowed() {
    assert_eq!(
        plan(&p("claude"), &codex_at(5.0))["reason"],
        "targets_below_reserve"
    );
    let above = plan(&p("claude"), &codex_at(5.1));
    assert_eq!(
        (above["action"].as_str(), above["to"].as_str()),
        (Some("swap"), Some("codex"))
    );
}

#[test]
fn current_exhaustion_and_target_reserve_are_different_questions() {
    // A current tier at 1% has credit: nothing has failed, so the pane is NOT swapped off it.
    let decision = plan(&p("claude"), &[ok("claude", 1.0), ok("codex", 90.0)]);
    assert_eq!(
        (decision["action"].as_str(), decision["reason"].as_str()),
        (Some("skip"), Some("current_has_credit"))
    );
}

#[test]
fn an_exhausted_extra_reported_window_fails_closed() {
    // CoS reproduced: five_hour 0 with weekly 99 read as "available".
    let state = credit_state(&[custom(
        "codex",
        vec![win("weekly", 99.0), win("five_hour", 0.0)],
    )]);
    assert_eq!(state.state_of("codex"), "exhausted");
}

#[test]
fn an_unknown_extra_reported_window_fails_closed() {
    let state = credit_state(&[custom(
        "codex",
        vec![win("weekly", 99.0), win("five_hour", f64::NAN)],
    )]);
    assert_eq!(state.state_of("codex"), "unknown");
}

#[test]
fn a_healthy_extra_window_does_not_break_availability() {
    let state = credit_state(&[custom(
        "codex",
        vec![win("weekly", 99.0), win("five_hour", 80.0)],
    )]);
    assert_eq!(state.state_of("codex"), "available");
}

fn hand_built(headroom: Option<f64>) -> Credit {
    let mut credit = Credit::default();
    credit.states.insert("claude".into(), "exhausted".into());
    credit.states.insert("codex".into(), "available".into());
    if let Some(h) = headroom {
        credit.headroom.insert("codex".into(), h);
    }
    credit
}

#[test]
fn target_without_headroom_data_is_refused() {
    // A hand-built snapshot (no headroom) must refuse: the reserve is unproven, not satisfied.
    let decision = plan_swap(&p("claude"), &hand_built(None), None).unwrap();
    assert_eq!(
        (decision["action"].as_str(), decision["reason"].as_str()),
        (Some("skip"), Some("target_headroom_unknown"))
    );
    assert!(!decision.to_string().contains("None"));
}

#[test]
fn target_with_nan_headroom_is_refused() {
    let decision = plan_swap(&p("claude"), &hand_built(Some(f64::NAN)), None).unwrap();
    assert_eq!(decision["reason"], "target_headroom_unknown");
}

#[test]
fn target_with_headroom_present_swaps_and_never_renders_none() {
    let decision = plan_swap(&p("claude"), &hand_built(Some(30.0)), None).unwrap();
    assert_eq!(decision["action"], "swap");
    let record = decision["record"].as_str().unwrap();
    assert!(
        !record.contains("None") && record.contains("30.0% left"),
        "{record}"
    );
}

#[test]
fn record_line_reports_headroom_not_just_availability() {
    let decision = plan(&p("claude"), &codex_at(42.0));
    assert!(decision["record"]
        .as_str()
        .unwrap()
        .contains("codex 42.0% left, above 5 reserve"));
}

// ---- UnmeasuredWindowTests: reviewer %19 residual ----

fn claude_spent_reading() -> Reading {
    custom(
        "claude",
        vec![win("five_hour", 0.0), win("weekly_all", 0.0)],
    )
}

fn plan_with_codex(windows: Vec<Window>) -> (Credit, Value) {
    let credit = credit_state(&[claude_spent_reading(), custom("codex", windows)]);
    let decision = plan_swap(&p("claude"), &credit, None).unwrap();
    (credit, decision)
}

#[test]
fn unnamed_window_is_not_dropped_from_the_contract() {
    let (credit, decision) = plan_with_codex(vec![win("weekly", 99.0), win("", 0.0)]);
    assert_eq!(credit.state_of("codex"), "unknown");
    assert_eq!(decision["action"], "skip");
}

#[test]
fn duplicate_window_names_are_not_last_wins() {
    let (credit, decision) = plan_with_codex(vec![win("weekly", 0.0), win("weekly", 99.0)]);
    assert_eq!(credit.state_of("codex"), "unknown");
    assert_eq!(decision["action"], "skip");
}

#[test]
fn a_fully_identified_reading_still_swaps() {
    let (credit, decision) = plan_with_codex(vec![win("weekly", 42.0)]);
    assert_eq!(credit.state_of("codex"), "available");
    assert_eq!(decision["action"], "swap");
}

// ---- ReproducibilityTests ----

#[test]
fn same_numbers_same_target_twice() {
    let credit = credit_state(&claude_codex_spent());
    let first = plan_swap(&p("claude"), &credit, None).unwrap();
    let second = plan_swap(&p("claude"), &credit, None).unwrap();
    assert_eq!(first, second);
}

#[test]
fn plan_all_is_ordered_by_pane_id() {
    let panes = vec![
        pane("claude", Some("idle"), true, "%3"),
        pane("claude", Some("idle"), true, "%1"),
        pane("claude", Some("idle"), true, "%2"),
    ];
    let out = plan_all(&panes, &credit_state(&claude_spent()), None).unwrap();
    let ids: Vec<&str> = out["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["pane_id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["%1", "%2", "%3"]);
}

#[test]
fn credit_order_in_the_input_does_not_matter() {
    let forward = credit_state(&claude_codex_spent());
    let mut reversed = claude_codex_spent();
    reversed.reverse();
    let backward = credit_state(&reversed);
    assert_eq!(forward, backward);
    assert_eq!(
        plan_swap(&p("claude"), &forward, None).unwrap(),
        plan_swap(&p("claude"), &backward, None).unwrap()
    );
}

// ---- WiringTests ----

#[test]
fn codex_swap_reports_its_required_args() {
    let decision = plan(&p("claude"), &claude_spent());
    assert_eq!(decision["to"], "codex");
    assert_eq!(
        decision["wiring"]["args"],
        json!(["-s", "danger-full-access", "-a", "never"])
    );
    // i33219 finding 4: paired native producers now exist, so a relauncher must wire codex's
    // turn producer rather than skip it.
    assert_eq!(decision["wiring"]["emits_turn_events"], true);
}

#[test]
fn no_static_reset_date_is_asserted() {
    for tier in TIER_ORDER {
        let wiring = harness_wiring(tier);
        assert!(
            !wiring["note"].as_str().unwrap().contains("2026-"),
            "{tier}"
        );
    }
}

#[test]
fn pi_swap_reports_a_turn_producer() {
    let decision = plan(&p("codex"), &claude_codex_spent());
    assert_eq!(decision["to"], "pi");
    assert_eq!(decision["wiring"]["emits_turn_events"], true);
}

#[test]
fn record_line_names_pane_from_to_and_credit_reason() {
    let decision = plan(&pane("claude", Some("idle"), true, "%42"), &claude_spent());
    let record = decision["record"].as_str().unwrap();
    assert!(
        record.starts_with("pane %42: claude -> codex because "),
        "{record}"
    );
    assert!(record.contains("claude credit exhausted"), "{record}");
}

// ---- OwnershipTests ----

#[test]
fn allowlist_is_authoritative_when_given() {
    let allowed = ["%1".to_string(), "%2".to_string()];
    let decision = plan_swap(&p("claude"), &credit_state(&claude_spent()), Some(&allowed)).unwrap();
    assert_eq!(decision["reason"], "not_owned_by_mechanism");
    // ...even when the pane itself says it is owned
    assert_eq!(p("claude")["owned_by_mechanism"], true);
}

#[test]
fn allowlist_member_is_planned() {
    let allowed = ["%2".to_string()];
    let decision = plan_swap(
        &pane("claude", Some("idle"), false, "%2"),
        &credit_state(&claude_spent()),
        Some(&allowed),
    )
    .unwrap();
    assert_eq!(decision["action"], "swap");
}

// ---- InputValidationTests ----

#[test]
fn pane_without_id_is_refused() {
    let error = plan_swap(&json!({"tier": "claude"}), &Credit::default(), None).unwrap_err();
    assert_eq!(error.to_string(), "pane needs a pane_id");
    assert_eq!(
        plan_swap(&json!(["x"]), &Credit::default(), None)
            .unwrap_err()
            .to_string(),
        "pane must be a mapping"
    );
}

#[test]
fn tier_headroom_and_tier_credit_read_what_the_python_reads() {
    let credit = credit_state(&[ok("codex", 42.0), ok("opencode-go", 17.5)]);
    assert_eq!(tier_headroom(&credit, "codex"), Some(42.0));
    assert_eq!(
        tier_headroom(&credit, "pi"),
        Some(17.5),
        "the pi tier reads opencode-go"
    );
    assert_eq!(tier_headroom(&credit, "free"), None);
    assert_eq!(tier_headroom(&credit, "mystery"), None);
    // tier_credit is keyed by the TIER name in the headroom map, as the Python reads it
    assert_eq!(tier_credit(&credit, "codex"), Some(42.0));
    assert_eq!(tier_credit(&credit, "pi"), None);
    assert_eq!(tier_credit(&credit, "mystery"), None);
}

#[test]
fn the_credit_dict_form_matches_the_pythons_shape() {
    let value = credit_state(&claude_spent()).to_value();
    assert_eq!(value["claude"], "exhausted");
    assert_eq!(value["codex"], "available");
    assert_eq!(value["headroom"]["codex"], 90.0);
}
