//! Ports of tests/test_dunk_context.py (digest) and test_dunk_context_governance.py
//! (ledger). `add`/`adopt` are not ported yet, so entries are written as files and ledger
//! rows with `Ledger::record`. Expected values were checked against the Python.

use chrono::NaiveDate;

use super::super::ledger::{path_key, sha, Ledger, Status};
use super::super::store::{render, Meta};
use super::*;

fn now() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2026, 10, 31)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: std::path::PathBuf,
    ledger: Ledger,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join(".dunkingsheep");
        let ledger = Ledger::new(Some(&dir.path().join("ledger.db")));
        Fixture {
            _dir: dir,
            root,
            ledger,
        }
    }

    fn put(
        &self,
        class: &str,
        slug: &str,
        subject: &str,
        extra: &[(&str, &str)],
        body: &str,
    ) -> std::path::PathBuf {
        let mut meta = Meta::new();
        for (key, value) in [
            ("class", class),
            ("subject", subject),
            ("type", "measurement"),
            ("scope", "project"),
            ("authority", "owner"),
            ("stated", "2026-10-30"),
        ] {
            meta.insert(key.into(), Some(value.into()));
        }
        for (key, value) in extra {
            meta.insert((*key).into(), Some((*value).into()));
        }
        let dir = self.root.join(class);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{slug}.md"));
        std::fs::write(&path, render(&meta, body)).unwrap();
        path
    }

    fn record(&mut self, path: &std::path::Path, slug: &str) {
        let text = std::fs::read_to_string(path).unwrap();
        self.ledger
            .record(&self.root, slug, &text, None, None, Some("test"))
            .unwrap();
    }

    fn digest(&mut self, max: usize, subject: Option<&str>, record: bool) -> String {
        digest_both(
            Some(&self.root),
            Some("project"),
            max,
            subject,
            record,
            &mut self.ledger,
            now(),
        )
    }
}

#[test]
fn the_digest_is_bounded_and_names_what_it_omitted() {
    let mut fx = Fixture::new();
    for index in 0..40 {
        fx.put(
            "fact",
            &format!("s{index}"),
            &format!("s{index}"),
            &[("recheck_after", "30d")],
            &format!("entry number {index} with some text"),
        );
    }
    let gt = fx.put(
        "ground-truth",
        "authority",
        "authority",
        &[],
        "the owner decides",
    );
    fx.record(&gt, "authority");
    let digest = fx.digest(400, None, false);
    assert!(digest.len() <= 500, "{} bytes", digest.len());
    assert!(digest.contains("omitted:"));
    assert!(digest.contains("Ground truths (1)"));
}

#[test]
fn a_stale_fact_is_a_warning_not_a_deletion() {
    let mut fx = Fixture::new();
    fx.put(
        "fact",
        "m",
        "m",
        &[("stated", "2020-01-01"), ("recheck_after", "7d")],
        "measured once",
    );
    let digest = fx.digest(DEFAULT_CAP, None, false);
    assert!(digest.contains("STALE"), "{digest}");
    assert!(
        digest.contains("measured once"),
        "stale is a marker, the entry stays"
    );
}

#[test]
fn a_fresh_fact_is_not_marked_and_a_zero_day_recheck_never_goes_stale() {
    let mut fx = Fixture::new();
    fx.put(
        "fact",
        "fresh",
        "fresh",
        &[("stated", "2026-10-30"), ("recheck_after", "30d")],
        "recent",
    );
    fx.put(
        "fact",
        "zero",
        "zero",
        &[("stated", "2020-01-01"), ("recheck_after", "0d")],
        "no horizon",
    );
    let digest = fx.digest(DEFAULT_CAP, None, false);
    assert!(!digest.contains("STALE"), "{digest}");
}

#[test]
fn the_exact_digest_layout_matches_the_python() {
    let mut fx = Fixture::new();
    let gt = fx.put(
        "ground-truth",
        "rule-a",
        "rule-a",
        &[],
        "rule a\nsecond line is not shown",
    );
    fx.record(&gt, "rule-a");
    fx.put(
        "fact",
        "p",
        "p",
        &[("recheck_after", "30d")],
        "project fact",
    );
    assert_eq!(
        "## Ground truths (1)\n- [rule-a] rule a\n\n## Relevant\n- [project/fact/p] project fact\n",
        fx.digest(DEFAULT_CAP, None, false)
    );
}

#[test]
fn an_empty_store_says_none_and_a_subject_filters_the_relevant_section() {
    let mut fx = Fixture::new();
    assert_eq!(
        "## Ground truths (0)\n- none\n\n## Relevant\n",
        fx.digest(DEFAULT_CAP, None, false)
    );
    fx.put("fact", "a", "alpha", &[], "about alpha");
    fx.put("fact", "b", "beta", &[], "about beta");
    let digest = fx.digest(DEFAULT_CAP, Some("beta"), false);
    assert!(
        digest.contains("about beta") && !digest.contains("about alpha"),
        "{digest}"
    );
}

#[test]
fn delivery_is_recorded_separately_from_reading() {
    let mut fx = Fixture::new();
    let gt = fx.put("ground-truth", "rule-a", "rule-a", &[], "rule a");
    fx.record(&gt, "rule-a");
    let store = Store::new(Some(&fx.root), false);
    fx.digest(DEFAULT_CAP, None, false);
    assert!(
        store.reads().is_empty(),
        "a dashboard view (record=false) is not a delivery"
    );
    fx.digest(DEFAULT_CAP, None, true);
    let reads = store.reads();
    assert_eq!(1, reads.len());
    assert_eq!("injected", reads[0]["kind"]);
    assert_eq!("rule-a", reads[0]["slug"]);
    store.log_access("rule-a", Some("%9"), "read");
    let kinds: Vec<String> = store
        .reads()
        .iter()
        .map(|r| r["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(vec!["injected", "read"], kinds, "reading is its own fact");
}

#[test]
fn an_access_line_is_the_pythons_json_dumps_byte_for_byte() {
    let fx = Fixture::new();
    std::fs::create_dir_all(&fx.root).unwrap();
    let store = Store::new(Some(&fx.root), false);
    store.log_access("é-slug", Some("who \"x\""), "read");
    let text = std::fs::read_to_string(store.access_path()).unwrap();
    let prefix = "{\"slug\": \"\\u00e9-slug\", \"at\": ";
    assert!(text.starts_with(prefix), "{text}");
    assert!(
        text.ends_with(", \"kind\": \"read\", \"by\": \"who \\\"x\\\"\"}\n"),
        "{text}"
    );
    let at = &text[prefix.len()..text.find(", \"kind\"").unwrap()];
    assert!(at.contains('.'), "a float, like Python's repr: {at}");
    store.log_access("", None, "read");
    assert_eq!(1, store.reads().len(), "an empty slug logs nothing");
}

#[test]
fn a_forged_ground_truth_is_not_injected_and_is_named() {
    let mut fx = Fixture::new();
    let good = fx.put("ground-truth", "real", "real", &[], "a real rule");
    fx.record(&good, "real");
    fx.put(
        "ground-truth",
        "2026-10-29-forged",
        "forged",
        &[],
        "an unrecorded rule",
    );
    let digest = fx.digest(DEFAULT_CAP, None, false);
    assert!(digest.contains("Ground truths (1)"), "{digest}");
    assert!(digest.contains("NOT injected"), "{digest}");
    assert!(digest.contains("2026-10-29-forged"), "{digest}");
    assert!(!digest.contains("an unrecorded rule"));
}

#[test]
fn an_edit_after_the_daemon_wrote_is_detected() {
    let mut fx = Fixture::new();
    let path = fx.put("ground-truth", "rule", "rule", &[], "the claim");
    fx.record(&path, "rule");
    let store = Store::new(Some(&fx.root), false);
    let entry = store.find("rule").unwrap();
    assert!(store.verified(&entry, &mut fx.ledger));
    let edited = std::fs::read_to_string(&path)
        .unwrap()
        .replace("the claim", "a wider claim");
    std::fs::write(&path, edited).unwrap();
    assert!(!store.verified(&store.find("rule").unwrap(), &mut fx.ledger));
}

#[test]
fn bootstrap_trusts_everything_until_a_row_exists() {
    let mut fx = Fixture::new();
    fx.put("ground-truth", "old", "old", &[], "an old rule");
    assert!(
        fx.digest(DEFAULT_CAP, None, false).contains("[old]"),
        "bootstrap: nothing refused"
    );
    let other = fx.put("ground-truth", "other", "other", &[], "x");
    fx.record(&other, "other");
    assert!(
        !fx.digest(DEFAULT_CAP, None, false).contains("[old]"),
        "one row ends bootstrap"
    );
}

#[test]
fn ledger_status_matches_the_pythons_four_states() {
    let dir = tempfile::tempdir().unwrap();
    let mut ledger = Ledger::new(Some(&dir.path().join("l.db")));
    let root = std::path::Path::new("./store/./x/");
    assert_eq!(Status::Bootstrap, ledger.status(root, "a", "t").unwrap());
    ledger
        .record(root, "a", "text", Some("fact"), Some("owner"), None)
        .unwrap();
    assert_eq!(1, ledger.count().unwrap());
    assert_eq!(
        Status::Recorded,
        ledger
            .status(std::path::Path::new("store/x"), "a", "text")
            .unwrap(),
        "str(Path) normalises"
    );
    assert_eq!(Status::Modified, ledger.status(root, "a", "other").unwrap());
    assert_eq!(
        Status::Unrecorded,
        ledger.status(root, "b", "text").unwrap()
    );
    assert_eq!("modified", Status::Modified.as_str());
}

#[test]
fn path_key_and_sha_match_the_python() {
    assert_eq!("store/x", path_key(std::path::Path::new("./store//x/")));
    assert_eq!(".", path_key(std::path::Path::new("")));
    assert_eq!(
        "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824",
        sha("hello")
    );
}

#[test]
fn first_line_splits_where_pythons_splitlines_does() {
    assert_eq!("a", first_line("a\nb"));
    assert_eq!("a", first_line("a\rb"));
    assert_eq!("a", first_line("a\u{2028}b"));
    assert_eq!("whole", first_line("whole"));
}

#[test]
fn select_stores_follows_which_and_the_project_directory() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join(".dunkingsheep");
    let names = |which: Option<&str>| {
        select_stores(Some(&project), which)
            .into_iter()
            .map(|(n, _)| n)
            .collect::<Vec<_>>()
    };
    assert_eq!(vec!["global"], names(None), "no project dir: global only");
    std::fs::create_dir_all(&project).unwrap();
    assert_eq!(vec!["global", "project"], names(None));
    assert_eq!(vec!["global"], names(Some("global")));
    assert_eq!(vec!["project"], names(Some("project")));
}
