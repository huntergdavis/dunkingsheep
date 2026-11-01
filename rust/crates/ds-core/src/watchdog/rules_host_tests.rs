//! Ports of the host, health, retired-pane and money tests of `tests/test_dunk_watchdog.py` (t1400
//! slice D). The Python runs them through `evaluate` (a later slice), so each calls the rule on the
//! same fixture instead; the expectations are the Python's, and findings are ordered with the slice-1
//! `sort_findings` where the Python's `evaluate` ordered them.

use std::time::{Duration, UNIX_EPOCH};

use serde_json::{json, Value};

use super::super::sort_findings;
use super::*;

const NOW: f64 = 1_800_000_000.0;
const MIN: f64 = 60.0;

/// `HOST_OK` of the Python suite.
fn host_ok() -> HostSample {
    HostSample {
        load1: 1.0,
        load5: Some(1.0),
        cpus: 8,
        used_fraction: 0.1,
        free_bytes: 50 * (1 << 30),
        inode_used_fraction: 0.1,
    }
}

fn working(count: usize) -> Panes {
    (0..count)
        .map(|i| (format!("w1:p{i}"), "working".to_string()))
        .collect()
}

fn load_rule() -> Value {
    json!({"severity": "warning", "class": "ops", "load1_per_cpu": 3.0,
           "critical_load1_per_cpu": 3.5, "critical_hold_runs": 3, "must_hold_runs": 1})
}

fn tmp_rule() -> Value {
    json!({"severity": "warning", "class": "ops", "max_used_fraction": 0.7,
           "critical_used_fraction": 0.9, "min_free_bytes": 100, "max_inode_fraction": 0.9,
           "path": "/tmp"})
}

fn sev(findings: &[Value]) -> Vec<(String, String, String)> {
    findings
        .iter()
        .map(|f| {
            (
                f["rule"].as_str().unwrap().to_string(),
                f["key"].as_str().unwrap().to_string(),
                f["severity"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

fn t3(rule: &str, key: &str, severity: &str) -> (String, String, String) {
    (rule.into(), key.into(), severity.into())
}

#[test]
fn test_host_load_and_tmp_bands() {
    // Fitted band: warning 3.0/cpu, critical 3.5/cpu. 24.0/8=3.0 -> warning.
    let panes = working(10);
    let host = HostSample {
        load1: 24.0,
        load5: Some(24.0),
        cpus: 8,
        ..host_ok()
    };
    let tmp = HostSample {
        used_fraction: 0.7,
        ..host_ok()
    };
    let mut found = rule_host_load(&host, Some(&panes), 0, &load_rule());
    found.extend(rule_host_tmp(&tmp, &tmp_rule()));
    sort_findings(&mut found);
    assert_eq!(
        vec![
            t3("host.load", "load5:warning", "warning"),
            t3("host.tmp", "/tmp:used_fraction", "warning")
        ],
        sev(&found)
    );
    let hot = HostSample {
        used_fraction: 0.95,
        ..host_ok()
    };
    assert_eq!("critical", rule_host_tmp(&hot, &tmp_rule())[0]["severity"]);
    assert_eq!(
        "0.9 (critical)",
        rule_host_tmp(&hot, &tmp_rule())[0]["threshold"]
    );
    let quiet_host = HostSample {
        load1: 23.99,
        load5: Some(23.99),
        cpus: 8,
        ..host_ok()
    };
    let quiet_tmp = HostSample {
        used_fraction: 0.69,
        ..host_ok()
    };
    assert!(rule_host_load(&quiet_host, Some(&panes), 0, &load_rule()).is_empty());
    assert!(rule_host_tmp(&quiet_tmp, &tmp_rule()).is_empty());
}

#[test]
fn test_the_fitted_band_does_not_page_on_our_own_productive_work() {
    // 4 working lanes + a compile reached 2.65/cpu and paged CRITICAL at the interim 2.6. The
    // fitted band must call that normal: the recorded maximum while doing assigned work was 2.79/cpu.
    let mut panes = working(4);
    for i in 5..28 {
        panes.insert(format!("w1:p{i}"), "idle".into());
    }
    // The reading the VP checked by hand: a compile, an assembler, an objdump.
    let calm = HostSample {
        load1: 21.2,
        load5: Some(21.2),
        cpus: 8,
        ..host_ok()
    };
    assert!(rule_host_load(&calm, Some(&panes), 0, &load_rule()).is_empty());
    // A genuine runaway still pages, and still only at critical once sustained.
    let runaway = HostSample {
        load1: 40.0,
        load5: Some(40.0),
        cpus: 8,
        ..host_ok()
    };
    let found = rule_host_load(&runaway, Some(&panes), 0, &load_rule());
    assert_eq!(
        vec![t3("host.load", "load5:critical", "critical")],
        sev(&found)
    );
    let threshold = found[0]["threshold"].as_str().unwrap();
    assert!(threshold.contains("3.5/cpu"), "{threshold}");
    assert_eq!("3.5/cpu (sustained 3 runs)", threshold);
    assert!(!found[0].to_string().to_lowercase().contains("interim"));
    // the texts a dry-run line prints
    assert_eq!("5.00/cpu (load5 40.0)", found[0]["measured"]);
    assert_eq!("load5 (5 min)", found[0]["window"]);
    assert_eq!(json!(4), found[0]["evidence"]["busy"]);
    assert_eq!(json!(27), found[0]["evidence"]["live"]);
}

#[test]
fn test_host_disk_full() {
    let rule = json!({"severity": "warning", "class": "outage", "max_used_fraction": 0.98,
                      "paths": ["/tmp", "/"]});
    let sample = |path: &str| -> Result<HostSample, DunkError> {
        Ok(HostSample {
            used_fraction: if path == "/" { 0.98 } else { 0.979 },
            ..host_ok()
        })
    };
    let found = rule_host_disk_full(&sample, &rule).unwrap();
    assert_eq!(vec![t3("host.disk_full", "/:0.98", "warning")], sev(&found));
    assert_eq!("outage", found[0]["class"]);
    assert_eq!("/ is 98.0% full", found[0]["summary"]);
    assert_eq!("0.98", found[0]["measured"]);
}

fn set_mtime(path: &Path, seconds_before_now: f64) {
    let when = UNIX_EPOCH + Duration::from_secs_f64(NOW - seconds_before_now);
    std::fs::File::open(path)
        .unwrap()
        .set_modified(when)
        .unwrap();
}

#[test]
fn test_heavy_slot_stale_uses_holder_liveness_not_age() {
    let dir = tempfile::tempdir().unwrap();
    let slots = dir.path();
    for name in [
        "poolrad-heavy.1",
        "poolrad-heavy.2",
        "poolrad-heavy.3",
        "other",
    ] {
        std::fs::create_dir(slots.join(name)).unwrap();
    }
    // 1: old but the holder is alive -> a legitimate long job, no finding.
    std::fs::write(
        slots.join("poolrad-heavy.1/pid"),
        std::process::id().to_string(),
    )
    .unwrap();
    // 2: old and the holder is gone -> stale.
    std::fs::write(slots.join("poolrad-heavy.2/pid"), "99999999").unwrap();
    // 3: legacy slot with no pid, old -> stale.
    for name in ["poolrad-heavy.1", "poolrad-heavy.2", "poolrad-heavy.3"] {
        set_mtime(&slots.join(name), 60.0 * MIN);
    }
    set_mtime(&slots.join("other"), 99.0 * MIN);
    let rule = json!({"severity": "warning", "class": "ops", "directory": slots.to_str().unwrap(),
                      "prefix": "poolrad-heavy.", "max_age_minutes": 30});
    let found = rule_host_heavy_slot_stale(&rule, NOW).unwrap();
    assert_eq!(
        vec![
            t3("host.heavy_slot_stale", "poolrad-heavy.2", "warning"),
            t3("host.heavy_slot_stale", "poolrad-heavy.3", "warning")
        ],
        sev(&found)
    );
    assert_eq!("holder gone", found[0]["measured"]);
    assert_eq!(json!(false), found[0]["evidence"]["holder"]);
    assert_eq!("legacy slot, age 60.0min", found[1]["measured"]);
    assert_eq!(Value::Null, found[1]["evidence"]["holder"]);
    assert_eq!("holder alive or <30min", found[0]["threshold"]);
}

#[test]
fn test_health_failed() {
    let rule = json!({"severity": "warning", "class": "ops", "stale_minutes": 30});
    let fresh = json!({"checked_at": NOW - 30.0 * MIN + 1.0, "ok": true, "problems": []});
    assert!(rule_health_failed(&fresh, &rule, NOW).is_empty());
    let bad = json!({"checked_at": NOW - 30.0 * MIN, "ok": false, "problems": ["wal"]});
    let mut found = rule_health_failed(&bad, &rule, NOW);
    sort_findings(&mut found);
    assert_eq!(
        vec![
            t3("health.failed", "problems", "warning"),
            t3("health.failed", "stale", "warning")
        ],
        sev(&found)
    );
    assert_eq!("ok=False, 1 problem(s)", found[0]["measured"]);
    assert_eq!(json!({"problems": ["wal"]}), found[0]["evidence"]);
    assert_eq!("30.0min", found[1]["measured"]);
}

fn retired(pane: &str, reason: &str) -> Retired {
    Retired {
        pane_id: pane.into(),
        reason: reason.into(),
        by: None,
        labels: vec!["par-d".into()],
        mailboxes: vec![],
        retired_at: NOW,
    }
}

#[test]
fn test_retired_panes_appear_in_the_digest_as_a_count() {
    let rule = json!({"severity": "info", "class": "ops"});
    let found = rule_pane_retired(&[retired("wW:p4", "superseded-by:wW:p7")], &rule);
    assert_eq!(1, found.len());
    assert!(found[0]["summary"]
        .as_str()
        .unwrap()
        .contains("retired panes: 1 (wW:p4)"));
    assert_eq!("info", found[0]["severity"]);
    assert_eq!("digest", found[0]["source"]);
    assert_eq!("wW:p4: superseded-by:wW:p7", found[0]["measured"]);
    assert!(rule_pane_retired(&[], &rule).is_empty());
}

#[test]
fn test_money_is_skipped_without_a_metering_source() {
    let rule = json!({"severity": "warning", "class": "money", "source": null, "cap": null});
    assert_eq!(
        RuleUnavailable("no metering source".into()),
        rule_money_spend_projection(&rule).unwrap_err()
    );
}

// -- extra: the branches the Python suite does not name ----------------------------------------

#[test]
fn extra_money_with_a_source_and_cap_still_has_no_reader() {
    let rule = json!({"severity": "warning", "class": "money", "source": "ledger", "cap": 10});
    assert_eq!(
        "no metering reader implemented for source 'ledger'",
        rule_money_spend_projection(&rule).unwrap_err().to_string()
    );
}

#[test]
fn extra_host_tmp_free_bytes_and_inode_checks_fire_with_their_units() {
    let host = HostSample {
        free_bytes: 50,
        inode_used_fraction: 0.95,
        ..host_ok()
    };
    let found = rule_host_tmp(&host, &tmp_rule());
    let keys: Vec<&str> = found.iter().map(|f| f["key"].as_str().unwrap()).collect();
    assert_eq!(vec!["/tmp:free_bytes", "/tmp:inode_fraction"], keys);
    assert_eq!("100B", found[0]["threshold"]);
    assert_eq!("50B", found[0]["measured"]);
    assert_eq!("0.95", found[1]["measured"]);
}

#[test]
fn extra_load_falls_back_to_load1_without_a_load5_and_cpus_floor_at_one() {
    let host = HostSample {
        load1: 9.0,
        load5: None,
        cpus: 0,
        ..host_ok()
    };
    let found = rule_host_load(&host, None, 2, &load_rule());
    assert_eq!("load5:critical", found[0]["key"]);
    assert_eq!(json!(1), found[0]["evidence"]["cpus"]);
    assert_eq!(json!(0), found[0]["evidence"]["live"]);
    assert_eq!(json!(2), found[0]["evidence"]["heavy"]);
}

#[test]
fn extra_a_slot_must_match_prefix_dot_digits_and_never_be_a_symlink() {
    let dir = tempfile::tempdir().unwrap();
    let slots = dir.path();
    // not slots: no dot, no digits, trailing text, and a symlink named like a slot
    for name in [
        "poolrad-heavy1",
        "poolrad-heavy.",
        "poolrad-heavy.1x",
        "poolrad-heavy.sh",
    ] {
        std::fs::create_dir(slots.join(name)).unwrap();
        set_mtime(&slots.join(name), 600.0 * MIN);
    }
    let target = slots.join("poolrad-heavy1");
    std::os::unix::fs::symlink(&target, slots.join("poolrad-heavy.9")).unwrap();
    // a FILE slot is a slot
    std::fs::write(slots.join("poolrad-heavy.4"), "").unwrap();
    set_mtime(&slots.join("poolrad-heavy.4"), 600.0 * MIN);
    let rule = json!({"severity": "warning", "class": "ops", "directory": slots.to_str().unwrap(),
                      "prefix": "poolrad-heavy", "max_age_minutes": 30});
    let found = rule_host_heavy_slot_stale(&rule, NOW).unwrap();
    let keys: Vec<&str> = found.iter().map(|f| f["key"].as_str().unwrap()).collect();
    assert_eq!(vec!["poolrad-heavy.4"], keys);
    // a young legacy slot is not stale yet
    set_mtime(&slots.join("poolrad-heavy.4"), 5.0 * MIN);
    assert!(rule_host_heavy_slot_stale(&rule, NOW).unwrap().is_empty());
    // an unreadable directory is an error like Python's listdir
    let missing = json!({"directory": "/nonexistent/slots", "prefix": "p.", "max_age_minutes": 1,
                         "severity": "warning", "class": "ops"});
    assert!(rule_host_heavy_slot_stale(&missing, NOW).is_err());
}

#[test]
fn extra_an_unparseable_or_nonpositive_holder_pid_is_a_gone_holder() {
    let dir = tempfile::tempdir().unwrap();
    for (name, pid) in [
        ("poolrad-heavy.1", "abc"),
        ("poolrad-heavy.2", "0"),
        ("poolrad-heavy.3", "-5"),
    ] {
        std::fs::create_dir(dir.path().join(name)).unwrap();
        std::fs::write(dir.path().join(name).join("pid"), pid).unwrap();
    }
    let rule = json!({"severity": "warning", "class": "ops",
                      "directory": dir.path().to_str().unwrap(), "prefix": "poolrad-heavy.",
                      "max_age_minutes": 30});
    let found = rule_host_heavy_slot_stale(&rule, NOW + 1.0).unwrap();
    assert_eq!(3, found.len());
    assert!(found.iter().all(|f| f["measured"] == "holder gone"));
}

#[test]
fn extra_heavy_jobs_counts_live_holders_and_unreadable_ones_but_not_dead_ones() {
    let dir = tempfile::tempdir().unwrap();
    let make = |name: &str, pid: Option<&str>| {
        std::fs::create_dir(dir.path().join(name)).unwrap();
        if let Some(pid) = pid {
            std::fs::write(dir.path().join(name).join("pid"), pid).unwrap();
        }
    };
    make("poolrad-heavy.1", Some(&std::process::id().to_string())); // alive
    make("poolrad-heavy.2", Some("99999999")); // dead
    make("poolrad-heavy.3", None); // no pid file: counts
    make("poolrad-heavy.4", Some("garbage")); // malformed: counts
    make("unrelated", None);
    assert_eq!(3, heavy_jobs(dir.path()));
    assert_eq!(0, heavy_jobs(Path::new("/nonexistent")));
}

#[test]
fn extra_sample_host_reads_this_machine() {
    let host = sample_host(Path::new("/")).unwrap();
    assert!(host.cpus >= 1);
    assert!((0.0..=1.0).contains(&host.used_fraction));
    assert!(host.load5.is_some());
    assert!(sample_host(Path::new("/nonexistent/for/sure")).is_err());
}

#[test]
fn extra_health_with_an_absent_ok_key_reports_problems_and_prints_none() {
    let rule = json!({"severity": "warning", "class": "ops", "stale_minutes": 30});
    let found = rule_health_failed(&json!({"checked_at": NOW}), &rule, NOW);
    assert_eq!(1, found.len());
    assert_eq!("ok=None, 0 problem(s)", found[0]["measured"]);
    // a missing checked_at is epoch 0: stale
    let stale = rule_health_failed(&json!({"ok": true, "problems": []}), &rule, NOW);
    assert_eq!("stale", stale[0]["key"]);
}
