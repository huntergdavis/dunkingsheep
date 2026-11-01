//! Tests for the context store's read side. Expected strings were produced by running the
//! Python (`dunk_context._parse_front`/`_render`/`_slug`/`_days`) on the same inputs.
//! `add`/`supersede`/`retire` are not ported yet, so chains are written as files.

use super::*;

fn front(extra: &[(&str, &str)]) -> Meta {
    let mut meta = Meta::new();
    for (key, value) in [
        ("class", "fact"),
        ("subject", "s"),
        ("type", "reference"),
        ("scope", "project"),
        ("authority", "measured"),
        ("stated", "2026-10-01"),
    ] {
        meta.insert(key.into(), Some(value.into()));
    }
    for (key, value) in extra {
        meta.insert(
            (*key).into(),
            if value.is_empty() {
                None
            } else {
                Some((*value).into())
            },
        );
    }
    meta
}

fn write(store: &Store, class: &str, slug: &str, extra: &[(&str, &str)], body: &str) {
    let dir = store.root.join(class);
    std::fs::create_dir_all(&dir).expect("class dir");
    std::fs::write(dir.join(format!("{slug}.md")), render(&front(extra), body)).expect("write");
}

fn store() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().expect("tempdir");
    let store = Store::new(Some(&dir.path().join(".dunkingsheep")), false);
    (dir, store)
}

#[test]
fn front_matter_refusals_quote_the_pythons_text() {
    let err = |text: &str| parse_front(text).expect_err("refused").0;
    assert_eq!(
        "entry has no front matter (must start with ---)",
        err("no front")
    );
    assert_eq!(
        "entry front matter is not closed with ---",
        err("---\nclass: fact\n")
    );
    assert_eq!(
        "bad front-matter line: 'bad line here'",
        err("---\nbad line here\n---\nbody")
    );
    assert_eq!(
        "bad front-matter line: \"it's bad\"",
        err("---\nit's bad\n---\nbody")
    );
    assert_eq!(
        "front matter is missing stated",
        err("---\nclass: fact\nsubject: s\ntype: t\nscope: global\nauthority: owner\n---\nx")
    );
    assert_eq!(
        "class must be one of ('ground-truth', 'fact', 'provisional', 'history')",
        err("---\nclass: nope\nsubject: s\ntype: t\nscope: global\nauthority: owner\nstated: 2026-01-01\n---\nx")
    );
    assert_eq!(
        "authority must be one of ('owner', 'measured', 'agent')",
        err("---\nclass: fact\nsubject: s\ntype: t\nscope: global\nauthority: boss\nstated: 2026-01-01\n---\nx")
    );
}

#[test]
fn parse_keeps_colons_in_values_skips_comments_and_nulls_the_empties() {
    let text = "---\n# c\nclass: fact\nsubject: a: b\ntype: t\nscope: global\nauthority: owner\nstated: 2026-01-01\nsource: ~\n---\n\n\nbody\nline\n";
    let (meta, body) = parse_front(text).expect("parsed");
    assert_eq!(Some(Some("a: b".to_string())), meta.get("subject").cloned());
    assert_eq!(Some(None), meta.get("source").cloned(), "~ is null");
    assert_eq!(
        "body\nline\n", body,
        "leading newlines are stripped, the rest kept"
    );
}

#[test]
fn render_writes_the_fixed_key_order_with_null_and_a_stripped_body() {
    let (meta, _) = parse_front(
        "---\nclass: fact\nsubject: a: b\ntype: t\nscope: global\nauthority: owner\nstated: 2026-01-01\nsource: ~\n---\nx",
    )
    .expect("parsed");
    assert_eq!(
        "---\nclass: fact\nsubject: a: b\ntype: t\nscope: global\nauthority: owner\nstated: 2026-01-01\nsource: null\nrecheck_after: null\nsupersedes: null\nsuperseded_by: null\nretired_at: null\nretired_reason: null\n---\n\nhi\n",
        render(&meta, "  hi  \n")
    );
}

#[test]
fn slug_and_days_match_the_python() {
    for (input, expected) in [
        ("Hello, World!", "hello-world"),
        ("--a--", "a"),
        ("!!!", "entry"),
        ("A_B  C", "a-b-c"),
        ("x1", "x1"),
    ] {
        assert_eq!(expected, slug(input), "{input}");
    }
    assert_eq!(Some(7), days(Some("7d")));
    assert_eq!(Some(30), days(Some(" 30d ")));
    for bad in [Some("d"), Some("7"), Some("7D"), None, Some("12dd")] {
        assert_eq!(None, days(bad), "{bad:?}");
    }
}

#[test]
fn an_absent_store_is_not_an_empty_one() {
    let (_dir, store) = store();
    assert!(!store.exists());
    assert!(store.entries().is_empty());
    std::fs::create_dir_all(store.root.join("fact")).expect("dir");
    assert!(store.exists());
}

#[test]
fn entries_are_class_ordered_sorted_and_a_bad_file_is_an_error_entry() {
    let (_dir, store) = store();
    write(&store, "fact", "b-one", &[], "b");
    write(&store, "fact", "a-one", &[], "a");
    write(
        &store,
        "ground-truth",
        "gt",
        &[("class", "ground-truth")],
        "g",
    );
    std::fs::write(store.root.join("fact").join("broken.md"), "no front matter").expect("write");
    let slugs: Vec<String> = store.entries().into_iter().map(|e| e.slug).collect();
    assert_eq!(
        vec!["gt", "a-one", "b-one", "broken"],
        slugs,
        "CLASSES order, then sorted by path"
    );
    let broken = store
        .entries()
        .into_iter()
        .find(|e| e.slug == "broken")
        .expect("broken");
    assert_eq!(
        Some("entry has no front matter (must start with ---)".to_string()),
        broken.error
    );
    assert!(
        store.live_entries().iter().all(|e| e.slug != "broken"),
        "an error entry is never live"
    );
}

#[test]
fn find_prefers_the_slug_over_a_subject_and_matches_either() {
    let (_dir, store) = store();
    write(&store, "fact", "alpha", &[("subject", "beta")], "x");
    write(&store, "fact", "beta", &[("subject", "gamma")], "y");
    assert_eq!(
        "beta",
        store.find("beta").expect("found").slug,
        "slug beats subject"
    );
    assert_eq!(
        "alpha",
        store.find("gamma").map(|_| "alpha").expect("by subject")
    );
    assert!(store.find("missing").is_none());
}

#[test]
fn chain_head_is_the_only_live_article_and_history_survives() {
    let (_dir, store) = store();
    write(
        &store,
        "fact",
        "first",
        &[("subject", "chain"), ("superseded_by", "second")],
        "obsolete-one",
    );
    write(
        &store,
        "fact",
        "second",
        &[
            ("subject", "chain"),
            ("supersedes", "first"),
            ("superseded_by", "third"),
        ],
        "obsolete-two",
    );
    write(
        &store,
        "fact",
        "third",
        &[("subject", "chain"), ("supersedes", "second")],
        "current-three",
    );
    write(
        &store,
        "fact",
        "gone",
        &[("subject", "retired"), ("retired_at", "2026-10-02")],
        "retired-claim",
    );
    for key in ["first", "chain", "third"] {
        assert_eq!(
            "current-three",
            store.resolve(key).expect("resolves").expect("head").body,
            "{key}"
        );
    }
    assert!(
        store.resolve("gone").expect("resolves").is_none(),
        "a retired head resolves to nothing"
    );
    assert_eq!(4, store.entries().len(), "history must survive");
    let live: Vec<String> = store.live_entries().into_iter().map(|e| e.slug).collect();
    assert_eq!(vec!["third"], live);
}

#[test]
fn legacy_retirement_and_corrupt_chains_fail_closed() {
    let (_dir, store) = store();
    write(
        &store,
        "fact",
        "old",
        &[("subject", "legacy")],
        "old\n\n**Retired 2026-10-01**: withdrawn",
    );
    assert!(
        store.resolve("old").expect("resolves").is_none(),
        "the dated body marker retires it"
    );
    assert!(store.live_entries().is_empty());
    for successor in ["missing", "old"] {
        write(
            &store,
            "fact",
            "old",
            &[("subject", "legacy"), ("superseded_by", successor)],
            "old\n\n**Retired 2026-10-01**: withdrawn",
        );
        let refused = store.resolve("old").expect_err("fails closed");
        assert!(
            refused.0 == "supersession target missing not found"
                || refused.0 == "supersession cycle at old",
            "{refused}"
        );
    }
}

#[test]
fn a_chain_through_a_corrupt_file_refuses_with_that_files_error() {
    let (_dir, store) = store();
    write(&store, "fact", "head", &[("superseded_by", "broken")], "x");
    std::fs::write(
        store.root.join("fact").join("broken.md"),
        "---\nclass: nope\n---\n",
    )
    .expect("write");
    let refused = store.resolve("head").expect_err("refused");
    assert!(
        refused.0.starts_with("front matter is missing") || refused.0.starts_with("class must be"),
        "{refused}"
    );
}

#[test]
fn the_retired_marker_must_start_a_line_and_carry_a_date() {
    let marker = |body: &str| {
        Store::is_retired(&Entry {
            meta: Meta::new(),
            slug: "s".into(),
            path: PathBuf::new(),
            body: body.into(),
            error: None,
        })
    };
    assert!(marker("x\n**Retired 2026-10-01**: y"));
    assert!(
        !marker("see **Retired 2026-10-01**: y"),
        "not at a line start"
    );
    assert!(!marker("**Retired soon**: y"), "no date");
}

// ---- slice 3: ensure / add / supersede / retire / adopt ---------------------------------
// Ported from tests/test_dunk_context.py (chain, supersession, same-day slug, enum refusals)
// and the Store-level governance tests (ledger on write, edit detection, adopt).

use crate::context::ledger::{Ledger, Status};

fn args<'a>(cls: &'a str, subject: &'a str, type_: &'a str, authority: &'a str) -> AddArgs<'a> {
    AddArgs {
        cls,
        subject,
        type_,
        authority: Some(authority),
        ..Default::default()
    }
}

#[test]
fn test_chain_head_is_the_only_live_article() {
    let (_dir, store) = store();
    let first = store
        .add(
            "obsolete-one",
            &args("fact", "chain", "reference", "measured"),
            None,
        )
        .unwrap();
    store
        .supersede(
            &first.slug,
            "obsolete-two",
            None,
            &SupersedeMeta::default(),
            None,
        )
        .unwrap();
    let head = store
        .supersede(
            &first.slug,
            "current-three",
            None,
            &SupersedeMeta::default(),
            None,
        )
        .unwrap();
    let retired = store
        .add(
            "retired-claim",
            &args("fact", "retired", "reference", "measured"),
            None,
        )
        .unwrap();
    store
        .retire(&retired.slug, "withdrawn", None, None)
        .unwrap();
    for key in [first.slug.as_str(), "chain", head.slug.as_str()] {
        assert_eq!(
            "current-three",
            store.resolve(key).unwrap().unwrap().body,
            "{key}"
        );
    }
    assert!(store.resolve(&retired.slug).unwrap().is_none());
    assert!(store
        .find(&retired.slug)
        .unwrap()
        .get("retired_at")
        .is_some());
    assert_eq!(4, store.entries().len(), "history must survive");
    store
        .retire(&first.slug, "retire the current head", None, None)
        .unwrap();
    assert!(store.resolve("chain").unwrap().is_none());
    assert!(store.live_entries().is_empty());
}

#[test]
fn test_supersession_keeps_both_and_links_them() {
    let (_dir, store) = store();
    let mut a = args("ground-truth", "thing", "principle", "owner");
    a.stated = Some("2026-10-01");
    let old = store.add("old truth", &a, None).unwrap();
    let new = store
        .supersede(
            &old.slug,
            "new truth",
            None,
            &SupersedeMeta::default(),
            None,
        )
        .unwrap();
    assert!(store.find(&old.slug).is_some());
    assert_eq!(
        Some(old.slug.as_str()),
        store.find(&new.slug).unwrap().get("supersedes").map(|s| s)
    );
    assert_eq!(
        Some(new.slug.as_str()),
        store.find(&old.slug).unwrap().get("superseded_by")
    );
}

#[test]
fn test_a_supersede_target_must_exist() {
    let (_dir, store) = store();
    let mut a = args("fact", "s", "measurement", "measured");
    a.supersedes = Some("nope");
    assert_eq!(
        "supersede target nope not found",
        store.add("x", &a, None).unwrap_err().0
    );
}

#[test]
fn test_a_supersede_within_the_same_day_gets_its_own_slug() {
    let (_dir, store) = store();
    let mut a = args("fact", "claim", "lesson", "agent");
    a.stated = Some("2026-10-22");
    let first = store.add("the old claim", &a, None).unwrap();
    let second = store
        .supersede(
            &first.slug,
            "the corrected claim",
            None,
            &SupersedeMeta::default(),
            None,
        )
        .unwrap();
    assert_ne!(first.slug, second.slug);
    assert_eq!(
        format!("{}-claim", super::today()),
        second.slug,
        "supersede derives from today"
    );
    let old = store.find(&first.slug).unwrap();
    let new = store.find(&second.slug).unwrap();
    assert_eq!(Some(second.slug.as_str()), old.get("superseded_by"));
    assert_eq!(Some(first.slug.as_str()), new.get("supersedes"));
    assert!(new.body.contains("the corrected claim"));
}

#[test]
fn test_a_bad_enum_is_a_named_refusal() {
    let (_dir, store) = store();
    assert_eq!(
        "authority must be one of ('owner', 'measured', 'agent')",
        store
            .add("x", &args("fact", "y", "reference", "stated"), None)
            .unwrap_err()
            .0
    );
    assert_eq!(
        "class must be one of ('ground-truth', 'fact', 'provisional', 'history')",
        store
            .add("x", &args("nope", "y", "reference", "agent"), None)
            .unwrap_err()
            .0
    );
    let err = store
        .add("x", &args("fact", "y", "gossip", "agent"), None)
        .unwrap_err()
        .0;
    assert!(
        err.starts_with("type must be one of ('principle', 'decision',"),
        "{err}"
    );
    assert!(err.ends_with("; got 'gossip'"), "{err}");
}

#[test]
fn an_agent_ground_truth_is_refused_and_an_existing_slug_is_not_overwritten() {
    let (_dir, store) = store();
    assert_eq!(
        "a ground truth has authority owner; an agent proposes a fact and the owner promotes it",
        store
            .add("x", &args("ground-truth", "g", "rule", "agent"), None)
            .unwrap_err()
            .0
    );
    let mut a = args("fact", "dup", "reference", "measured");
    a.stated = Some("2026-10-01");
    store.add("one", &a, None).unwrap();
    assert_eq!(
        "entry 2026-10-01-dup already exists",
        store.add("two", &a, None).unwrap_err().0
    );
}

#[test]
fn ensure_makes_the_class_dirs_and_the_readme_once() {
    let (_dir, store) = store();
    assert!(!store.exists());
    store.ensure().unwrap();
    assert!(store.exists());
    for class in CLASSES {
        assert!(store.root.join(class).is_dir(), "{class}");
    }
    let readme = std::fs::read_to_string(store.root.join("README.md")).unwrap();
    assert!(readme.starts_with("# .dunkingsheep context store\n"));
    std::fs::write(store.root.join("README.md"), "mine").unwrap();
    store.ensure().unwrap();
    assert_eq!(
        "mine",
        std::fs::read_to_string(store.root.join("README.md")).unwrap()
    );
}

#[test]
fn a_provisional_entry_defaults_to_a_seven_day_recheck() {
    let (_dir, store) = store();
    let made = store
        .add("maybe", &args("provisional", "p", "lesson", "agent"), None)
        .unwrap();
    assert_eq!(
        Some("7d"),
        store.find(&made.slug).unwrap().get("recheck_after")
    );
    let fact = store
        .add("sure", &args("fact", "f", "lesson", "agent"), None)
        .unwrap();
    assert_eq!(None, store.find(&fact.slug).unwrap().get("recheck_after"));
}

#[test]
fn a_supersede_inherits_class_subject_and_source_unless_overridden() {
    let (_dir, store) = store();
    let mut a = args("fact", "inherit", "measurement", "measured");
    a.source = "file.rs";
    let first = store.add("v1", &a, None).unwrap();
    let kept = store
        .supersede(&first.slug, "v2", None, &SupersedeMeta::default(), None)
        .unwrap();
    let entry = store.find(&kept.slug).unwrap();
    assert_eq!(
        (
            Some("fact"),
            Some("inherit"),
            Some("measurement"),
            Some("measured"),
            Some("file.rs")
        ),
        (
            entry.get("class"),
            entry.get("subject"),
            entry.get("type"),
            entry.get("authority"),
            entry.get("source")
        )
    );
    let meta = SupersedeMeta {
        source: Some("other.rs"),
        recheck_after: Some(Some("30d")),
        ..Default::default()
    };
    let changed = store
        .supersede(&first.slug, "v3", None, &meta, None)
        .unwrap();
    let entry = store.find(&changed.slug).unwrap();
    assert_eq!(
        (Some("other.rs"), Some("30d")),
        (entry.get("source"), entry.get("recheck_after"))
    );
    assert_eq!(
        "entry nope not found",
        store
            .supersede("nope", "x", None, &SupersedeMeta::default(), None)
            .unwrap_err()
            .0
    );
}

#[test]
fn retire_stamps_the_entry_and_appends_the_reason() {
    let (_dir, store) = store();
    let made = store
        .add("claim", &args("fact", "r", "reference", "measured"), None)
        .unwrap();
    let (slug, reason) = store
        .retire(&made.slug, "line one\nline two", None, None)
        .unwrap();
    assert_eq!(
        (made.slug.clone(), "line one\nline two".to_string()),
        (slug, reason)
    );
    let entry = store.find(&made.slug).unwrap();
    assert_eq!(
        Some("line one line two"),
        entry.get("retired_reason"),
        "newlines flattened"
    );
    assert!(entry.body.contains("**Retired "), "{}", entry.body);
    assert!(
        entry.body.ends_with("line one\nline two"),
        "{:?}",
        entry.body
    );
    assert_eq!(
        "entry nope not found",
        store.retire("nope", "", None, None).unwrap_err().0
    );
}

#[test]
fn a_write_is_recorded_in_the_ledger_and_an_edit_after_it_is_detected() {
    let (dir, store) = store();
    let mut ledger = Ledger::new(Some(&dir.path().join("ledger.db")));
    let made = store
        .add(
            "the claim",
            &args("ground-truth", "never-reboot", "rule", "owner"),
            Some(&mut ledger),
        )
        .unwrap();
    let entry = store.find(&made.slug).unwrap();
    let text = std::fs::read_to_string(&entry.path).unwrap();
    assert_eq!(
        Status::Recorded,
        ledger.status(&store.root, &made.slug, &text).unwrap()
    );
    assert!(store.verified(&entry, &mut ledger));
    std::fs::write(&entry.path, text.replace("the claim", "a wider claim")).unwrap();
    assert!(!store.verified(&entry, &mut ledger));
}

#[test]
fn superseding_re_records_the_old_entry_too() {
    let (dir, store) = store();
    let mut ledger = Ledger::new(Some(&dir.path().join("ledger.db")));
    let first = store
        .add(
            "v1",
            &args("fact", "chain", "reference", "measured"),
            Some(&mut ledger),
        )
        .unwrap();
    store
        .supersede(
            &first.slug,
            "v2",
            Some("w1"),
            &SupersedeMeta::default(),
            Some(&mut ledger),
        )
        .unwrap();
    let old = store.find(&first.slug).unwrap();
    assert!(
        store.verified(&old, &mut ledger),
        "the rewritten old file is the recorded one"
    );
    assert_eq!(2, ledger.count().unwrap());
}

#[test]
fn bootstrap_trusts_everything_until_adopt() {
    let (dir, store) = store();
    write(
        &store,
        "ground-truth",
        "old",
        &[
            ("type", "principle"),
            ("authority", "owner"),
            ("scope", "global"),
        ],
        "an old rule",
    );
    let mut ledger = Ledger::new(Some(&dir.path().join("ledger.db")));
    let entry = store.find("old").unwrap();
    assert!(
        store.verified(&entry, &mut ledger),
        "bootstrap: no rows yet"
    );
    assert_eq!(vec!["old".to_string()], store.adopt(&mut ledger, "adopt"));
    assert_eq!(1, ledger.count().unwrap());
    assert!(store.verified(&entry, &mut ledger));
}

#[test]
fn a_real_same_day_supersede_collision_gets_a_numbered_slug() {
    let (_dir, store) = store();
    let today = super::today();
    let mut a = args("fact", "claim", "lesson", "agent");
    a.stated = Some(&today);
    let first = store.add("old", &a, None).unwrap();
    let second = store
        .supersede(&first.slug, "new", None, &SupersedeMeta::default(), None)
        .unwrap();
    let third = store
        .supersede(&first.slug, "newer", None, &SupersedeMeta::default(), None)
        .unwrap();
    assert_eq!(format!("{today}-claim"), first.slug);
    assert_eq!(format!("{today}-claim-2"), second.slug);
    assert_eq!(format!("{today}-claim-3"), third.slug);
}

// ---- slice 4: check / stats / entry_stats / Store.digest / report -----------------------
// Ported from tests/test_dunk_context.py (refusal by check, owner truth survives, bounded
// digest, stale fact is a warning, access logging) and test_dunk_context_governance.py
// (a hand-dropped file is named by check).

fn ledger_in(dir: &tempfile::TempDir) -> Ledger {
    Ledger::new(Some(&dir.path().join("ledger.db")))
}

fn problems(check: &serde_json::Value, key: &str) -> Vec<String> {
    check[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["problem"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn test_an_agent_ground_truth_is_refused_by_check() {
    let (dir, store) = store();
    assert!(store
        .add(
            "agents may propose",
            &args("ground-truth", "authority", "principle", "agent"),
            None
        )
        .is_err());
    store.ensure().unwrap();
    write(
        &store,
        "ground-truth",
        "2026-10-29-authority",
        &[
            ("class", "ground-truth"),
            ("type", "principle"),
            ("authority", "agent"),
            ("stated", "2026-10-29"),
        ],
        "agents may propose but never promote",
    );
    let mut ledger = ledger_in(&dir);
    let check = store.check(&mut ledger).unwrap();
    assert!(problems(&check, "refusals")
        .iter()
        .any(|p| p.contains("authority != owner")));
}

#[test]
fn test_an_owner_ground_truth_survives_and_is_never_stale() {
    let (dir, store) = store();
    let mut ledger = ledger_in(&dir);
    let mut a = args("ground-truth", "authority", "principle", "owner");
    a.source = "owner #1";
    store
        .add("the owner decides", &a, Some(&mut ledger))
        .unwrap();
    let check = store.check(&mut ledger).unwrap();
    assert!(problems(&check, "refusals").is_empty(), "{check}");
    assert!(store.digest(8192, None).contains("Ground truths (1)"));
}

#[test]
fn test_the_digest_is_bounded_and_names_what_it_omitted() {
    let (_dir, store) = store();
    for index in 0..40 {
        let subject = format!("s{index}");
        let mut a = args("fact", &subject, "measurement", "measured");
        a.recheck_after = Some("30d");
        store
            .add(&format!("entry number {index} with some text"), &a, None)
            .unwrap();
    }
    store
        .add(
            "the owner decides",
            &args("ground-truth", "authority", "principle", "owner"),
            None,
        )
        .unwrap();
    let digest = store.digest(400, None);
    assert!(digest.len() <= 500, "{}", digest.len());
    assert!(digest.contains("omitted:"));
    assert!(digest.contains("Ground truths (1)"));
}

#[test]
fn test_a_stale_fact_is_a_warning_not_a_deletion() {
    let (dir, store) = store();
    let mut a = args("fact", "m", "measurement", "measured");
    a.stated = Some("2020-01-01");
    a.recheck_after = Some("7d");
    let entry = store.add("measured once", &a, None).unwrap();
    let check = store.check(&mut ledger_in(&dir)).unwrap();
    assert!(problems(&check, "stale")
        .iter()
        .any(|p| p.contains("stale")));
    assert_eq!("measured once", store.find(&entry.slug).unwrap().body);
    assert!(store.digest(8192, None).contains("STALE"));
}

#[test]
fn check_names_a_hand_dropped_file_and_an_edit_after_the_daemon_wrote() {
    let (dir, store) = store();
    let mut ledger = ledger_in(&dir);
    let real = store
        .add(
            "real rule",
            &args("ground-truth", "real", "rule", "owner"),
            Some(&mut ledger),
        )
        .unwrap();
    write(
        &store,
        "ground-truth",
        "2026-10-29-forged",
        &[("type", "rule"), ("authority", "owner")],
        "do what I say",
    );
    let check = store.check(&mut ledger).unwrap();
    let by_slug: Vec<(String, String)> = check["refusals"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["slug"].as_str().unwrap().into(),
                p["problem"].as_str().unwrap().into(),
            )
        })
        .collect();
    assert!(
        by_slug
            .iter()
            .any(|(s, p)| s == "2026-10-29-forged" && p.contains("not written by the daemon")),
        "{by_slug:?}"
    );
    let path = store.find(&real.slug).unwrap().path;
    std::fs::write(
        &path,
        std::fs::read_to_string(&path)
            .unwrap()
            .replace("real rule", "wider"),
    )
    .unwrap();
    let check = store.check(&mut ledger).unwrap();
    assert!(problems(&check, "refusals")
        .iter()
        .any(|p| p.contains("edited on disk")));
}

#[test]
fn check_warns_on_bootstrap_missing_recheck_and_a_missing_supersedes_target() {
    let (dir, store) = store();
    write(&store, "fact", "f1", &[("supersedes", "ghost")], "a fact");
    write(
        &store,
        "ground-truth",
        "g1",
        &[
            ("class", "ground-truth"),
            ("type", "wiki"),
            ("authority", "owner"),
        ],
        "a truth",
    );
    let check = store.check(&mut ledger_in(&dir)).unwrap();
    let all = problems(&check, "problems");
    assert!(
        all.iter()
            .any(|p| p.starts_with("ledger is empty (bootstrap)")),
        "{all:?}"
    );
    assert!(all.contains(&"fact has no recheck_after".to_string()));
    assert!(all.contains(&"supersedes missing entry ghost".to_string()));
    assert!(
        all.contains(&"ground truth typed 'wiki'; expected rule/principle/decision".to_string())
    );
    assert!(all.contains(&"ground truth has no recheck_after (rules go stale too)".to_string()));
    assert_eq!(1, check["counts"]["fact"]);
    assert_eq!(1, check["counts"]["ground-truth"]);
    assert!(
        check["refusals"].as_array().unwrap().len() < check["problems"].as_array().unwrap().len()
    );
}

#[test]
fn test_access_logging_and_never_read() {
    let (_dir, store) = store();
    let mut a = args("fact", "r", "measurement", "measured");
    a.recheck_after = Some("30d");
    let read = store.add("read me", &a, None).unwrap();
    let mut b = args("fact", "n", "measurement", "measured");
    b.recheck_after = Some("30d");
    let never = store.add("never me", &b, None).unwrap();
    store.log_access(&read.slug, None, "read");
    let stats = store.stats().unwrap();
    assert_eq!(1, stats["reads_total"]);
    assert_eq!(1, stats["never_read_count"]);
    assert_eq!(serde_json::json!([never.slug]), stats["never_read"]);
    assert_eq!(1, stats["reads_7d"]);
    assert_eq!(0, stats["injected_total"]);
    store.log_access(&never.slug, None, "injected");
    let stats = store.stats().unwrap();
    assert_eq!(
        (2, 1, 1, 0),
        (
            stats["reads_total"].as_i64().unwrap(),
            stats["injected_total"].as_i64().unwrap(),
            stats["injected_7d"].as_i64().unwrap(),
            stats["never_delivered_count"].as_i64().unwrap(),
        )
    );
    assert_eq!(1, stats["never_read_count"], "injected is not read");
}

#[test]
fn stats_ages_use_python_rounding_for_the_percentiles() {
    // round(0.9 * 5) = round(4.5) = 4 in Python (ties to even); f64::round would say 5.
    assert_eq!(
        serde_json::json!(40),
        super::pct(&[0, 10, 20, 30, 40, 50], 0.9)
    );
    assert_eq!(
        serde_json::json!(20),
        super::pct(&[0, 10, 20, 30, 40, 50], 0.5),
        "round(2.5) = 2"
    );
    assert_eq!(serde_json::Value::Null, super::pct(&[], 0.5));
    assert_eq!(serde_json::json!(7), super::pct(&[7], 0.9));
}

#[test]
fn entry_stats_counts_per_article_and_flags_stale() {
    let (dir, store) = store();
    let mut a = args("fact", "old", "measurement", "measured");
    a.stated = Some("2020-01-01");
    a.recheck_after = Some("7d");
    let old = store.add("old", &a, None).unwrap();
    let fresh = store
        .add(
            "fresh",
            &args("history", "fresh", "reference", "agent"),
            None,
        )
        .unwrap();
    store.log_access(&old.slug, Some("w1"), "read");
    store.log_access(&old.slug, Some("w1"), "read");
    store.log_access(&old.slug, Some("w1"), "injected");
    let rows = store.entry_stats(&mut ledger_in(&dir)).unwrap();
    let find = |slug: &str| rows.iter().find(|r| r["slug"] == slug).unwrap().clone();
    let old_row = find(&old.slug);
    assert_eq!(
        (2, 1, true),
        (
            old_row["reads"].as_i64().unwrap(),
            old_row["injected"].as_i64().unwrap(),
            old_row["stale"].as_bool().unwrap(),
        )
    );
    assert!(old_row["last_read"].is_f64() && old_row["last_injected"].is_f64());
    let fresh_row = find(&fresh.slug);
    assert_eq!(
        (0, 0, false),
        (
            fresh_row["reads"].as_i64().unwrap(),
            fresh_row["injected"].as_i64().unwrap(),
            fresh_row["stale"].as_bool().unwrap(),
        )
    );
    assert!(fresh_row["last_read"].is_null());
}

#[test]
fn test_chain_head_is_the_only_entry_in_stats() {
    let (dir, store) = store();
    let first = store
        .add(
            "obsolete-one",
            &args("fact", "chain", "reference", "measured"),
            None,
        )
        .unwrap();
    store
        .supersede(
            &first.slug,
            "obsolete-two",
            None,
            &SupersedeMeta::default(),
            None,
        )
        .unwrap();
    let head = store
        .supersede(
            &first.slug,
            "current-three",
            None,
            &SupersedeMeta::default(),
            None,
        )
        .unwrap();
    let digest = store.digest(8192, None);
    assert!(digest.contains("current-three"));
    for gone in ["obsolete-one", "obsolete-two"] {
        assert!(!digest.contains(gone));
    }
    let stats = store.stats().unwrap();
    assert_eq!(serde_json::json!([head.slug]), stats["never_read"]);
    assert_eq!(1, stats["counts"]["fact"]);
    let slugs: Vec<String> = store
        .entry_stats(&mut ledger_in(&dir))
        .unwrap()
        .iter()
        .map(|r| r["slug"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(vec![head.slug], slugs);
}
