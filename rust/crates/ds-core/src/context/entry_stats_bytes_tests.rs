//! t3565 (t1395 gap 1): byte-level golden for entry_stats.
//!
//! `rust/fixtures/context/entry_stats.json` is the Python store's entry_stats written by
//! `tools/snapshot_entry_stats.py` in the canonical form of `json.dumps(indent=2, sort_keys=True,
//! ensure_ascii=False)`. This test renders the Rust value the same way and compares the bytes.
//! The value comparison in golden_tests.rs (entry_stats_match_the_python_with_fixed_access_times)
//! does not check key order or text bytes; this one does.

use serde_json::Value;

use super::golden_tests::{fixed_clock, fixture};

/// `json.dumps(indent=2, sort_keys=True, ensure_ascii=False)` for a serde_json value. Keys are
/// sorted by code point (String's order is the same for valid UTF-8), and the text ends with a
/// newline as the Python writer's does.
fn canonical_text(value: &Value) -> String {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for key in keys {
                    out.insert(key.clone(), sorted(&map[key]));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string_pretty(&sorted(value)).unwrap() + "\n"
}

#[test]
fn entry_stats_bytes_match_the_python_canonical_text() {
    let (_dir, store, mut ledger, expect) = fixture();
    let _clock = fixed_clock(&expect);
    let got = canonical_text(&serde_json::json!(store.entry_stats(&mut ledger).unwrap()));
    let want = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../fixtures/context/entry_stats.json"),
    )
    .unwrap();
    assert_eq!(
        want, got,
        "entry_stats bytes differ from the Python canonical text (tools/snapshot_entry_stats.py)"
    );
}
