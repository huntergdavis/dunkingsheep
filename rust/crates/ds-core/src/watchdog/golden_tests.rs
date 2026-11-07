//! t1400 (T53 slice 2): the `watchdog --dry-run` golden.
//!
//! `rust/fixtures/watchdog/golden/dry_run.expected.json` is the Python `dunkingsheep watchdog
//! --dry-run --json --now 1793448000` output over the golden config and the fixture state, written
//! by `tools/watchdog_dry_run_golden.py`. This test runs `watchdog::run` over the same config and
//! state (copied to a scratch directory, the Python path replaced by "@STATE@") and compares the
//! parsed JSON. Dry-run sends nothing, so the owner route is a stub that refuses.

use std::path::{Path, PathBuf};

use serde_json::Value;

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

#[test]
fn watchdog_dry_run_matches_the_python_golden() {
    let dir = tempfile::tempdir().unwrap();
    let state = scratch_state(dir.path());
    let state_text = state.to_str().unwrap().to_string();
    let config_text =
        std::fs::read_to_string(fixtures().join("watchdog/golden/watchdog.golden.json"))
            .unwrap()
            .replace(PLACEHOLDER, &state_text);
    let config: Value = serde_json::from_str(&config_text).unwrap();
    let mut send = |_: &str, _: &str, _: &str| -> Result<(), SendError> { Ok(()) };
    let report = run(&config, NOW, true, &mut send, &NoOwnerRoute, None, None).unwrap();
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
