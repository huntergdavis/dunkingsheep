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
