//! T50 acceptance (t1395): the read side against a fixture the PYTHON store built
//! (`tools/snapshot_context.py --write`). Fails when a digest, the entries listing, a search
//! result, check report, get projection, stats or entry_stats differs from what the Python
//! printed. Access-log timestamps are fixed in the fixture; both calendar and Unix clocks are fixed without adjusting expected values.

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
pub(super) fn fixture() -> (tempfile::TempDir, Store, Ledger, Value) {
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

fn entry_value(entry: &super::store::Entry) -> Value {
    let mut value = serde_json::Map::new();
    for key in [
        "slug",
        "class",
        "subject",
        "type",
        "scope",
        "authority",
        "stated",
        "source",
        "recheck_after",
        "supersedes",
        "superseded_by",
        "retired_at",
        "retired_reason",
    ] {
        value.insert(
            key.to_string(),
            if key == "slug" {
                json!(entry.slug)
            } else {
                entry.get(key).map_or(Value::Null, |field| json!(field))
            },
        );
    }
    value.insert(
        "body".into(),
        if entry.error.is_some() {
            Value::Null
        } else {
            json!(entry.body.trim())
        },
    );
    value.insert(
        "error".into(),
        entry.error.as_ref().map_or(Value::Null, |e| json!(e)),
    );
    Value::Object(value)
}

pub(super) fn fixed_clock(expect: &Value) -> super::store::ReadClockGuard {
    let local = chrono::NaiveDateTime::parse_from_str(
        expect["clock"]["local"]
            .as_str()
            .expect("fixed local clock"),
        "%Y-%m-%dT%H:%M:%S",
    )
    .unwrap();
    super::store::fixed_read_clock(local, expect["clock"]["unix"].as_f64().unwrap())
}

#[test]
fn digests_are_byte_identical_to_the_python() {
    let (_dir, store, _ledger, expect) = fixture();
    let _clock = fixed_clock(&expect);
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
    let _clock = fixed_clock(&expect);
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
    let _clock = fixed_clock(&expect);
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
fn get_resolution_matches_the_python() {
    let (_dir, store, _ledger, expect) = fixture();
    let _clock = fixed_clock(&expect);
    let cases = case(&expect, "get");
    assert_eq!(4, cases.len());
    for c in cases {
        let key = c["args"]["key"].as_str().unwrap();
        let got = store
            .resolve(key)
            .unwrap()
            .as_ref()
            .map(entry_value)
            .unwrap_or(Value::Null);
        assert_eq!(c["expected"], got, "get {key}");
    }
}

#[test]
fn the_check_report_matches_the_python() {
    let (_dir, store, mut ledger, expect) = fixture();
    let _clock = fixed_clock(&expect);
    let got = store.check(&mut ledger).unwrap();
    assert_eq!(case(&expect, "check")[0]["expected"], got);
}

#[test]
fn stats_match_the_python_with_fixed_access_times() {
    let (_dir, store, _ledger, expect) = fixture();
    let _clock = fixed_clock(&expect);
    assert_eq!(1, case(&expect, "stats").len());
    assert_eq!(
        case(&expect, "stats")[0]["expected"],
        store.stats().unwrap()
    );
}

#[test]
fn entry_stats_match_the_python_with_fixed_access_times() {
    let (_dir, store, mut ledger, expect) = fixture();
    let _clock = fixed_clock(&expect);
    assert_eq!(1, case(&expect, "entry_stats").len());
    assert_eq!(
        case(&expect, "entry_stats")[0]["expected"],
        json!(store.entry_stats(&mut ledger).unwrap())
    );
}

/// Every difference between the Rust read side and the Python's recorded answers, one line
/// each (the live-tree run wants all of them, not the first).
fn all_differences(store: &Store, ledger: &mut Ledger, expect: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let access_path = store.access_path();
    let original_access = std::fs::read(&access_path).ok();
    for c in case(expect, "digest") {
        let cap = c["args"]["max_bytes"].as_u64().unwrap() as usize;
        let got = store.digest(cap, c["args"]["subject"].as_str());
        if c["expected"].as_str() != Some(got.as_str()) {
            out.push(format!("digest {} differs", c["args"]));
        }
    }
    for c in case(expect, "get") {
        let key = c["args"]["key"].as_str().unwrap();
        match store.resolve(key) {
            Ok(entry) => {
                let got = entry.as_ref().map(entry_value).unwrap_or(Value::Null);
                if c["expected"] != got {
                    out.push(format!(
                        "get {key} differs: python {} rust {got}",
                        c["expected"]
                    ));
                }
            }
            Err(error) => out.push(format!("get {key} failed: {error}")),
        }
    }
    let entries: Vec<Value> = store
        .entries()
        .iter()
        .map(|e| {
            json!({
                "slug": e.slug, "class": e.get("class"), "subject": e.get("subject"),
                "stated": e.get("stated"),
                "body": if e.error.is_some() { Value::Null } else { json!(e.body.trim()) },
                "error": e.error,
            })
        })
        .collect();
    if case(expect, "entries")[0]["expected"] != json!(entries) {
        out.push("entries listing differs".into());
    }
    for c in case(expect, "search") {
        let a = &c["args"];
        let got = match store.search(a["query"].as_str().unwrap(), None, None, 20) {
            Ok(rows) => json!(rows),
            Err(error) => json!({ "error": error.to_string() }),
        };
        if c["expected"] != got {
            out.push(format!(
                "search {a} differs: python {} rust {got}",
                c["expected"]
            ));
        }
    }
    match store.check(ledger) {
        Ok(got) if case(expect, "check")[0]["expected"] == got => {}
        Ok(got) => out.push(format!("check differs: rust {got}")),
        Err(error) => out.push(format!("check failed: {error}")),
    }
    // The Python helper restores the copied access log after its search probes for the same
    // reason: acceptance queries must not become input to the stats comparison.
    match original_access {
        Some(bytes) => {
            if let Err(error) = std::fs::write(&access_path, bytes) {
                out.push(format!("could not restore copied access log: {error}"));
            }
        }
        None => {
            let _ = std::fs::remove_file(&access_path);
        }
    }
    match store.stats() {
        Ok(got) if case(expect, "stats")[0]["expected"] == got => {}
        Ok(got) => out.push(format!("stats differs: rust {got}")),
        Err(error) => out.push(format!("stats failed: {error}")),
    }
    match store.entry_stats(ledger) {
        Ok(got) if case(expect, "entry_stats")[0]["expected"] == json!(got) => {}
        Ok(got) => out.push(format!("entry_stats differs: rust {}", json!(got))),
        Err(error) => out.push(format!("entry_stats failed: {error}")),
    }
    out
}

/// t1395 part 2: a READ-ONLY COPY of a live tree. `tools/context_live_diff.py SRC OUT` makes
/// OUT/tree and OUT/expected.json; run with DS_CONTEXT_LIVE_DIR=OUT and `--ignored`.
#[test]
#[ignore = "needs DS_CONTEXT_LIVE_DIR from tools/context_live_diff.py"]
fn live_tree_matches_the_python() {
    let Ok(out) = std::env::var("DS_CONTEXT_LIVE_DIR") else {
        panic!("set DS_CONTEXT_LIVE_DIR");
    };
    let out = std::path::PathBuf::from(out);
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join(".dunkingsheep");
    copy_tree(&out.join("tree"), &root);
    let expect: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("expected.json")).unwrap()).unwrap();
    let store = Store::new(Some(&root), false);
    let mut ledger = Ledger::new(Some(&dir.path().join("ledger.db")));
    store.adopt(&mut ledger, "adopt").unwrap();
    let _clock = fixed_clock(&expect);
    let diffs = all_differences(&store, &mut ledger, &expect);
    for line in &diffs {
        eprintln!("DIFF {line}");
    }
    assert!(diffs.is_empty(), "{} differences", diffs.len());
}
