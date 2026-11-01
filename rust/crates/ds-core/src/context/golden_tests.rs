//! T50 acceptance (t1395): the read side against a fixture the PYTHON store built
//! (`tools/snapshot_context.py --write`). Fails when a digest, the entries listing, a search
//! result or the check report differs from what the Python printed. `stats` is not covered.

use serde_json::{json, Value};

use super::digest::DEFAULT_CAP;
use super::ledger::Ledger;
use super::store::Store;

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

fn expected() -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../fixtures/context/expected.json");
    serde_json::from_str(&std::fs::read_to_string(path).expect("expected.json")).unwrap()
}

/// A COPY of the committed tree with a ledger rebuilt from the `ledger` block: every entry is
/// recorded with its text, except `unrecorded` ones (no row) and `modified` ones (a row for
/// other text).
fn fixture() -> (tempfile::TempDir, Store, Ledger, Value) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(".dunkingsheep");
    let source =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/context/tree");
    copy_tree(&source, &root);
    let expect = expected();
    let names = |key: &str| -> Vec<String> {
        expect["ledger"][key]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect()
    };
    let (modified, unrecorded) = (names("modified"), names("unrecorded"));
    let store = Store::new(Some(&root), false);
    let mut ledger = Ledger::new(Some(&dir.path().join("ledger.db")));
    for class in ["ground-truth", "fact", "provisional", "history"] {
        let folder = root.join(class);
        let Ok(files) = std::fs::read_dir(&folder) else {
            continue;
        };
        for file in files {
            let path = file.unwrap().path();
            let slug = path.file_stem().unwrap().to_string_lossy().to_string();
            if unrecorded.contains(&slug) {
                continue;
            }
            let mut text = std::fs::read_to_string(&path).unwrap();
            if modified.contains(&slug) {
                text.push_str("other text");
            }
            ledger
                .record(&root, &slug, &text, None, None, None)
                .unwrap();
        }
    }
    (dir, store, ledger, expect)
}

fn case<'a>(expect: &'a Value, op: &str) -> Vec<&'a Value> {
    expect["cases"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["op"] == op)
        .collect()
}

#[test]
fn digests_are_byte_identical_to_the_python() {
    let (_dir, store, _ledger, expect) = fixture();
    let cases = case(&expect, "digest");
    assert_eq!(3, cases.len());
    for c in cases {
        let cap = c["args"]["max_bytes"].as_u64().unwrap() as usize;
        assert!(cap > 0 && DEFAULT_CAP > 0);
        let subject = c["args"]["subject"].as_str();
        let got = store.digest(cap, subject);
        assert_eq!(c["expected"].as_str().unwrap(), got, "digest {}", c["args"]);
    }
}

#[test]
fn the_entries_listing_matches_the_python() {
    let (_dir, store, _ledger, expect) = fixture();
    let got: Vec<Value> = store
        .entries()
        .iter()
        .map(|e| {
            json!({
                "slug": e.slug,
                "class": e.get("class"),
                "subject": e.get("subject"),
                "stated": e.get("stated"),
                "body": if e.error.is_some() { Value::Null } else { json!(e.body.trim()) },
                "error": e.error,
            })
        })
        .collect();
    assert_eq!(case(&expect, "entries")[0]["expected"], json!(got));
}

#[test]
fn search_results_match_the_python() {
    let (_dir, store, _ledger, expect) = fixture();
    for c in case(&expect, "search") {
        let a = &c["args"];
        let got = store
            .search(
                a["query"].as_str().unwrap(),
                a["cls"].as_str(),
                a["scope"].as_str(),
                a["limit"].as_i64().unwrap(),
            )
            .unwrap();
        assert_eq!(c["expected"], json!(got), "search {a}");
    }
}

#[test]
fn the_check_report_matches_the_python() {
    let (_dir, store, mut ledger, expect) = fixture();
    let got = store.check(&mut ledger).unwrap();
    assert_eq!(case(&expect, "check")[0]["expected"], got);
}
