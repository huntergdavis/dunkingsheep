//! Port of `tests/test_dunk_context.py::ContextCliTests`: the CLI surface an agent types.
//! A wrong shape must teach, not traceback. Like the Python class, each test runs the
//! `dunkingsheep` binary as a subprocess inside a throwaway project and store.
//!
//! The stale-cache test plants its drifted FTS5 index with `rusqlite`, the same 0.32 that
//! ds-core uses. ds-core's store-level twin is
//! `context::store::store_tests::test_context_search_survives_a_stale_cache_and_a_fresh_search_finds_the_entry`.

use std::path::Path;
use std::process::{Command, Output};

use ds_core::context::store::{AddArgs, Store, SupersedeMeta};

fn run_cli(sandbox: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dunkingsheep"))
        .arg("--no-start-daemon")
        .args(args)
        .current_dir(sandbox)
        .env("DUNKINGSHEEP_DIR", sandbox)
        .env(
            "DUNKINGSHEEP_CONTEXT_LEDGER",
            sandbox.join("context-ledger.db"),
        )
        .env("HOME", sandbox)
        .env_remove("DUNKINGSHEEP_SOCKET")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
        .output()
        .expect("run the Rust dunkingsheep CLI")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn test_text_adds_inline_and_a_bare_sentence_is_a_named_refusal() {
    let sandbox = tempfile::tempdir().unwrap();
    let added = run_cli(
        sandbox.path(),
        &[
            "context",
            "add",
            "--text",
            "a claim",
            "--class",
            "fact",
            "--subject",
            "inline",
            "--authority",
            "agent",
            "--local",
        ],
    );
    assert_eq!(Some(0), added.status.code(), "{}", text(&added.stderr));
    assert!(
        text(&added.stdout).contains("added fact/"),
        "{}",
        text(&added.stdout)
    );

    let bare = run_cli(
        sandbox.path(),
        &[
            "context",
            "add",
            "a bare sentence",
            "--class",
            "fact",
            "--subject",
            "x",
            "--local",
        ],
    );
    assert_eq!(Some(2), bare.status.code());
    let stderr = text(&bare.stderr);
    assert!(stderr.contains("no such file"), "{stderr}");
    assert!(
        stderr.contains("--text"),
        "the refusal must name the form that works: {stderr}"
    );
    assert!(!stderr.contains("Traceback"), "{stderr}");
}

#[test]
fn test_a_bad_enum_is_a_named_refusal_not_a_traceback() {
    let sandbox = tempfile::tempdir().unwrap();
    let bad = run_cli(
        sandbox.path(),
        &[
            "context",
            "add",
            "--text",
            "x",
            "--class",
            "fact",
            "--subject",
            "y",
            "--authority",
            "stated",
            "--local",
        ],
    );
    assert_eq!(Some(2), bad.status.code());
    let stderr = text(&bad.stderr);
    assert!(stderr.contains("authority must be one of"), "{stderr}");
    assert!(!stderr.contains("Traceback"), "{stderr}");
}

#[test]
fn test_get_original_slug_and_subject_resolve_three_revision_head() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join(".dunkingsheep");
    let store = Store::new(Some(root.as_path()), false);
    let first = store
        .add(
            "old-body",
            &AddArgs {
                cls: "fact",
                subject: "chain",
                type_: "reference",
                ..AddArgs::default()
            },
            None,
        )
        .unwrap();
    store
        .supersede(
            &first.slug,
            "middle-body",
            None,
            &SupersedeMeta::default(),
            None,
        )
        .unwrap();
    let head = store
        .supersede(
            &first.slug,
            "newest-body",
            None,
            &SupersedeMeta::default(),
            None,
        )
        .unwrap();

    for key in [first.slug.as_str(), "chain"] {
        let result = run_cli(sandbox.path(), &["context", "get", key]);
        assert_eq!(Some(0), result.status.code(), "{}", text(&result.stderr));
        let stdout = text(&result.stdout);
        assert!(stdout.contains("newest-body"), "{stdout}");
        assert!(!stdout.contains("old-body"), "{stdout}");
    }

    store.retire(&head.slug, "withdrawn", None, None).unwrap();
    let result = run_cli(sandbox.path(), &["context", "get", first.slug.as_str()]);
    assert!(
        !text(&result.stdout).contains("newest-body"),
        "{}",
        text(&result.stdout)
    );
}

#[test]
fn test_context_search_survives_a_stale_cache() {
    let sandbox = tempfile::tempdir().unwrap();
    let root = sandbox.path().join(".dunkingsheep");
    let store = Store::new(Some(root.as_path()), false);
    store
        .add(
            "the wiki artefact pattern is a durable design rule",
            &AddArgs {
                cls: "fact",
                subject: "wiki-artefact-pattern",
                type_: "reference",
                authority: Some("measured"),
                ..AddArgs::default()
            },
            None,
        )
        .unwrap();
    store.index().unwrap();
    let path = store.index_path();
    std::fs::remove_file(&path).unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "CREATE VIRTUAL TABLE entries USING fts5(slug, subject, body)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO entries VALUES ('drifted','drifted','body')",
        [],
    )
    .unwrap();
    drop(conn);

    let found = run_cli(sandbox.path(), &["context", "search", "artefact"]);
    let stdout = text(&found.stdout);
    let output = format!("{}{}", text(&found.stderr), stdout);
    assert_eq!(Some(0), found.status.code(), "{output}");
    assert!(!output.contains("no such column"), "{output}");
    assert!(stdout.contains("wiki-artefact-pattern"), "{stdout}");
}
