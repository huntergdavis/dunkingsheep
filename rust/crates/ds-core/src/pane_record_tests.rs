//! Port of tests/test_dunk_roles.py::test_a_failed_pane_lookup_does_not_demote_a_registered_lane
//! and the cache behaviour around it (PR #208, t1112).

use super::*;

fn record() -> serde_json::Value {
    serde_json::json!({"pane_id": "w2:p3", "pane_pid": 4100})
}

#[test]
fn a_failed_pane_lookup_does_not_demote_a_registered_lane() {
    // answers [record, None, None]: the role stays inside the window, drops after it
    let mut cache = RolePaneCache::new();
    let mut warnings = Vec::new();
    let mut answers = vec![Some(record()), None, None].into_iter();
    let mut ask =
        |cache: &mut RolePaneCache<serde_json::Value>, now: f64, warnings: &mut Vec<String>| {
            cache.role_pane_info(
                "w2:p3",
                now,
                || answers.next().flatten(),
                |text| warnings.push(text.to_string()),
            )
        };
    assert_eq!(Some(record()), ask(&mut cache, 1_000.0, &mut warnings));
    cache.clear_cache();
    assert_eq!(
        Some(record()),
        ask(&mut cache, 1_010.0, &mut warnings),
        "one empty answer keeps the role"
    );
    assert_eq!(
        vec![fail_open_message("w2:p3", 10.0)],
        warnings,
        "the fail-open is logged with the Python's text"
    );
    cache.clear_cache();
    // the python sets role_pane_last to time - 400
    cache
        .role_pane_last
        .insert("w2:p3".to_string(), (1_010.0 - 400.0, record()));
    assert_eq!(
        None,
        ask(&mut cache, 1_010.0, &mut warnings),
        "gone past the grace window"
    );
}

#[test]
fn the_warning_text_is_the_pythons() {
    assert_eq!(
        "pane lookup for %5 returned nothing; using the record from 12 s ago (fail open for 180 s)",
        fail_open_message("%5", 12.0)
    );
}

#[test]
fn the_window_is_exclusive_at_180_seconds() {
    let mut cache = RolePaneCache::new();
    cache.role_pane_last.insert("%1".into(), (0.0, 1));
    let mut sink = |_: &str| {};
    assert_eq!(
        Some(1),
        cache.role_pane_info("%1", 179.9, || None, &mut sink)
    );
    cache.clear_cache();
    assert_eq!(
        None,
        cache.role_pane_info("%1", 180.0, || None, &mut sink),
        "now - held < 180"
    );
}

#[test]
fn a_cached_answer_is_served_for_two_seconds_without_asking_again() {
    let mut cache = RolePaneCache::new();
    let mut asked = 0;
    let mut sink = |_: &str| {};
    for now in [10.0, 11.9] {
        cache.role_pane_info(
            "%1",
            now,
            || {
                asked += 1;
                Some(7)
            },
            &mut sink,
        );
    }
    assert_eq!(1, asked, "inside the 2 s TTL");
    cache.role_pane_info(
        "%1",
        12.1,
        || {
            asked += 1;
            Some(7)
        },
        &mut sink,
    );
    assert_eq!(2, asked, "past the TTL");
}

#[test]
fn a_fail_open_answer_is_itself_cached_for_the_ttl() {
    let mut cache = RolePaneCache::new();
    let mut sink = |_: &str| {};
    cache.role_pane_info("%1", 0.0, || Some(1), &mut sink);
    cache.clear_cache();
    let mut warnings = 0;
    for now in [10.0, 10.5] {
        cache.role_pane_info("%1", now, || None, |_| warnings += 1);
    }
    assert_eq!(
        1, warnings,
        "the second call is a cache hit, not a second warning"
    );
}

#[test]
fn an_empty_pane_id_is_none_and_never_asks() {
    let mut cache: RolePaneCache<i32> = RolePaneCache::new();
    let got = cache.role_pane_info("", 0.0, || panic!("must not ask"), |_| {});
    assert_eq!(None, got);
}

#[test]
fn the_cache_is_pruned_of_expired_entries_past_512_panes() {
    let mut cache = RolePaneCache::new();
    let mut sink = |_: &str| {};
    for n in 0..513 {
        cache.role_pane_info(&format!("%{n}"), 0.0, || Some(n), &mut sink);
    }
    // at now=5 everything from t=0 (expiry 2) is expired; the 514th insert triggers the prune
    cache.role_pane_info("%new", 5.0, || Some(0), &mut sink);
    assert!(
        cache.cache.len() <= 2,
        "expired entries are dropped: {}",
        cache.cache.len()
    );
}
