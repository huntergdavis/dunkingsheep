//! t1400 (T53 slice 2): the `watchdog --dry-run` golden.
//!
//! `rust/fixtures/watchdog/golden/dry_run.expected.json` is the Python `dunkingsheep watchdog
//! --dry-run --json --now 1793448000` output over the golden config and the fixture state, written
//! by `tools/watchdog_dry_run_golden.py`. This test runs `watchdog::run` over the same config and
//! state (copied to a scratch directory, the Python path replaced by "@STATE@") and compares the
//! parsed JSON. Dry-run sends nothing, so the owner route is a stub that refuses.
//!
//! t3580: the test reads only fixture inputs. The host probes are pinned to a healthy reading (the
//! host the golden was recorded on), the heavy-job count comes from a fixed test provider
//! (`evaluate::test_seam`), and the heavy-slot scan reads an empty scratch directory. The result does
//! not depend on this machine's /tmp, load or CPU count, and the test checks that the provider was used.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::evaluate;
use super::rules_host::HostSample;
use super::run::{run, OwnerTransport, SendError};
use crate::error::DunkError;

const NOW: f64 = 1793448000.0;
const PLACEHOLDER: &str = "@STATE@";

struct NoOwnerRoute;

impl OwnerTransport for NoOwnerRoute {
    fn desk_channel(&self) -> Result<String, DunkError> {
        Err(DunkError::Refused("the golden has no owner channel".into()))
    }

    fn post(&self, _body: &str) -> Result<i64, SendError> {
        Err(SendError {
            kind: "unconfigured".into(),
            message: "dry-run sends nothing".into(),
        })
    }

    fn confirmed_through(&self) -> Result<Option<Value>, DunkError> {
        Ok(None)
    }
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures")
}

/// The golden state in a scratch directory, as tools/watchdog_dry_run_golden.py builds it.
fn scratch_state(dir: &Path) -> PathBuf {
    let state = dir.join("state");
    std::fs::create_dir_all(&state).unwrap();
    for name in ["inbox.db", "tasks.db", "roles.json"] {
        std::fs::copy(fixtures().join("state").join(name), state.join(name)).unwrap();
    }
    for name in ["dunks.json", "health.json"] {
        std::fs::copy(
            fixtures().join("watchdog/extra").join(name),
            state.join(name),
        )
        .unwrap();
    }
    state
}

/// Replace the scratch state path inside every string, the same normalization the recorder does.
fn normalize(value: &Value, state: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace(state, PLACEHOLDER)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| normalize(item, state)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), normalize(item, state)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// A healthy host: `/tmp` and `/` well under every limit in the golden config, load at zero. The golden
/// checks the rules over this reading, not over whichever machine runs the test.
fn healthy_host(_path: &Path) -> Result<HostSample, DunkError> {
    Ok(HostSample {
        load1: 0.0,
        load5: Some(0.0),
        cpus: 4,
        used_fraction: 0.5,
        free_bytes: 50 * 1024 * 1024 * 1024,
        inode_used_fraction: 0.1,
    })
}

#[test]
fn watchdog_dry_run_matches_the_python_golden() {
    let dir = tempfile::tempdir().unwrap();
    let state = scratch_state(dir.path());
    let state_text = state.to_str().unwrap().to_string();
    let config_text =
        std::fs::read_to_string(fixtures().join("watchdog/golden/watchdog.golden.json"))
            .unwrap()
            .replace(PLACEHOLDER, &state_text);
    let mut config: Value = serde_json::from_str(&config_text).unwrap();
    // The heavy-slot scan reads an empty scratch directory, not the live /tmp.
    let slots = dir.path().join("slots");
    std::fs::create_dir_all(&slots).unwrap();
    config["rules"]["host.heavy_slot_stale"]["directory"] = json!(slots.to_str().unwrap());
    // The heavy-job count is a fixed test provider, so the live /tmp is never read.
    let _heavy = evaluate::test_seam::fix_heavy_jobs(0);
    let mut send = |_: &str, _: &str, _: &str| -> Result<(), SendError> { Ok(()) };
    let report = run(
        &config,
        NOW,
        true,
        &mut send,
        &NoOwnerRoute,
        None,
        Some(&healthy_host),
    )
    .unwrap();
    assert!(
        evaluate::test_seam::reads() >= 1,
        "the golden did not consult the fixed heavy-job count"
    );
    let got = normalize(&report, &state_text);
    let want: Value = serde_json::from_str(
        &std::fs::read_to_string(fixtures().join("watchdog/golden/dry_run.expected.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        want, got,
        "watchdog --dry-run output differs from the Python golden (tools/watchdog_dry_run_golden.py)"
    );
}
