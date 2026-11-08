//! t3580: the heavy-job seam (`evaluate::test_seam`). The golden installs a fixed count so it never
//! reads the host's /tmp. These tests show that the seam is on the `host.load` path, and that without a
//! fixed count the live count is what it always was. They use fixture directories only; none reads /tmp.

use super::*;

const NOW: f64 = 1_800_000_000.0;
const EXAMPLE: &str = include_str!("../../../../../packaging/watchdog.example.json");

/// The example config with only `host.load` enabled and the panes source off.
fn host_load_only() -> Value {
    let mut config: Value = serde_json::from_str(EXAMPLE).unwrap();
    for rule in config["rules"].as_object_mut().unwrap().values_mut() {
        rule["enabled"] = json!(false);
    }
    config["rules"]["host.load"]["enabled"] = json!(true);
    config["panes"]["source"] = json!("none");
    config
}

/// A host far past the critical band, so `host.load` fires on every consult.
fn hot_host(_path: &Path) -> Result<HostSample, DunkError> {
    Ok(HostSample {
        load1: 20.0,
        load5: Some(20.0),
        cpus: 4,
        used_fraction: 0.5,
        free_bytes: 50 * 1024 * 1024 * 1024,
        inode_used_fraction: 0.1,
    })
}

#[test]
fn a_fixed_heavy_count_is_the_value_host_load_reports() {
    let config = host_load_only();
    let _fixed = test_seam::fix_heavy_jobs(2);
    let result = evaluate(&config, NOW, None, Some(&hot_host), Some(&["host.load"]));
    assert!(result["errors"].as_array().unwrap().is_empty(), "{result}");
    let finding = result["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["rule"] == "host.load")
        .expect("host.load fires on a hot host");
    assert_eq!(json!(2), finding["evidence"]["heavy"]);
    assert!(
        test_seam::reads() >= 1,
        "host.load did not consult the seam"
    );
}

/// A scratch directory holding one live poolrad-heavy slot, held by this test process.
fn one_live_holder() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let slot = dir.path().join("poolrad-heavy.live");
    std::fs::create_dir(&slot).unwrap();
    std::fs::write(slot.join("pid"), std::process::id().to_string()).unwrap();
    dir
}

#[test]
fn without_a_fixed_count_the_seam_counts_the_live_holders() {
    let holders = one_live_holder();
    assert!(test_seam::heavy_override().is_none());
    assert_eq!(1, heavy_jobs_in(holders.path()));
}

#[test]
fn the_fixed_count_is_released_when_its_guard_drops() {
    let holders = one_live_holder();
    {
        let _fixed = test_seam::fix_heavy_jobs(5);
        assert_eq!(5, heavy_jobs_in(holders.path()));
    }
    assert!(test_seam::heavy_override().is_none());
    assert_eq!(1, heavy_jobs_in(holders.path()));
}
