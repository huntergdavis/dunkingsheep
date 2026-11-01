//! Ports, by name, of tests/test_dunk_secrets.py: `PatternTests` and `RepositoryTests` (a real
//! git repository, because that is where the leak happened). NOT ported: the
//! `AgreementWithTheCiScanner` and `WiringTests` classes (they read the Python CI scanner,
//! `.gitignore` and the script's text).

use super::*;
use std::fs;
use std::path::PathBuf;

#[test]
fn test_the_leaked_names_are_refused() {
    for path in [
        "data/auth.json",
        "data/owner-credentials",
        "data/messages.sqlite3",
        "web.sqlite3",
        "owner-credentials",
        "app.pem",
        ".env",
        "keys/token_store.py",
        "sub/dir/state.sqlite3",
    ] {
        assert!(matches(path).is_some(), "{path}");
    }
}

#[test]
fn test_ordinary_source_files_are_not() {
    for path in [
        "tests/test_telegram_page_watermark.py",
        "docs/re/README.md",
        "dunk_secrets.py",
        "src/recovered/overlay_u20_11ac_cpu.c",
        "packaging/systemd/dunkingsheep-report.service",
    ] {
        assert_eq!(None, matches(path), "{path}");
    }
}

#[test]
fn test_a_dotfile_keeps_its_dot() {
    // The first version lstrip'd './' and turned '.env' into 'env'.
    assert_eq!(Some(".env"), matches(".env"));
    assert_eq!(Some(".env"), matches("./.env"));
}

#[test]
fn test_the_refusal_names_the_file_and_the_rule() {
    assert_eq!(
        vec![("data/auth.json".to_string(), "data/")],
        leaks(["data/auth.json", "docs/x.md"])
    );
}

#[test]
fn fnmatch_follows_python_for_star_question_and_classes() {
    assert!(fnmatch("a/b.sqlite3", "*.sqlite3"), "* crosses '/'");
    assert!(fnmatch("x1", "x?"));
    assert!(!fnmatch("x12", "x?"));
    assert!(fnmatch("b", "[abc]"));
    assert!(!fnmatch("d", "[abc]"));
    assert!(fnmatch("d", "[!abc]"));
    assert!(
        fnmatch("a.b", "a.b") && !fnmatch("axb", "a.b"),
        "a dot is a dot"
    );
}

struct Repo {
    dir: PathBuf,
}

impl Repo {
    fn new(tag: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("ds-secrets-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let repo = Repo { dir };
        repo.git(&["init", "-q", "-b", "main"]);
        repo.git(&["config", "user.email", "t@example.com"]);
        repo.git(&["config", "user.name", "t"]);
        fs::write(repo.dir.join("README.md"), "base\n").unwrap();
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "-q", "-m", "base"]);
        repo
    }
    fn git(&self, args: &[&str]) -> String {
        let (code, out, err) = super::git(args, &self.dir);
        assert_eq!(0, code, "git {args:?}: {err}");
        out
    }
    fn write(&self, path: &str, text: &str) {
        let target = self.dir.join(path);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::write(target, text).unwrap();
    }
}

#[test]
fn test_a_clean_branch_passes() {
    let repo = Repo::new("clean");
    repo.write("feature.py", "x");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "feature"]);
    assert_eq!((None, None), refusal(&repo.dir, Some("main")));
}

#[test]
fn test_a_committed_data_directory_is_refused() {
    let repo = Repo::new("committed");
    repo.git(&["checkout", "-q", "-b", "lane/leak"]); // lanes push branches, not main
    repo.write("data/owner-credentials", "Password: hunter2\n");
    repo.write("data/messages.sqlite3", "sqlite");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "oops"]);
    let (message, _error) = refusal(&repo.dir, Some("main"));
    let message = message.expect("refused");
    assert!(message.contains("data/owner-credentials"), "{message}");
    assert!(message.contains("must never leave this host"), "{message}");
    assert!(message.contains("(matches 'data/')"), "{message}");
}

#[test]
fn test_a_staged_but_uncommitted_data_file_is_refused() {
    // The exact moment of the incident: `git add -A` with data/ present.
    let repo = Repo::new("staged");
    repo.write("data/auth.json", "{\"token\": \"x\"}");
    repo.git(&["add", "-A"]);
    let (message, _error) = refusal(&repo.dir, Some("main"));
    assert!(message.expect("refused").contains("data/auth.json"));
}

#[test]
fn test_the_cli_exit_code_is_what_the_hooks_branch_on() {
    let repo = Repo::new("exit");
    repo.git(&["checkout", "-q", "-b", "lane/leak"]);
    repo.write("data/owner-credentials", "Password: hunter2\n");
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "oops"]);
    assert_eq!(1, check(&repo.dir, Some("main"), true).0);
    fs::remove_file(repo.dir.join("data/owner-credentials")).unwrap();
    repo.git(&["add", "-A"]);
    repo.git(&["commit", "-q", "-m", "fix"]);
    assert_eq!((0, String::new()), check(&repo.dir, Some("main"), false));
}

#[test]
fn an_undeterminable_range_is_a_warning_not_a_refusal() {
    // A repo with no base ref: the guard must not fail closed (it would block every lane).
    let dir = std::env::temp_dir().join(format!("ds-secrets-nobase-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let _ = super::git(&["init", "-q", "-b", "trunk"], &dir);
    let (code, stderr) = check(&dir, None, false);
    assert_eq!(0, code);
    assert!(
        stderr.contains("WARNING could not fully check: no base ref"),
        "{stderr}"
    );
    assert_eq!((0, String::new()), check(&dir, None, true));
}

/// Expected values produced by the real `dunk_secrets.matches` (note the rule NAMES: a
/// `owner-credentials` file is caught by `*-credentials`, listed before the literal).
#[test]
fn matches_agrees_with_the_real_python_on_a_corpus() {
    let cases: &[(&str, Option<&str>)] = &[
        ("data", Some("data/")),
        ("data/", Some("data/")),
        ("data/x", Some("data/")),
        ("mydata/x", None),
        ("a/data", Some("data")),
        ("a/data/b", Some("data")),
        ("x.sqlite3", Some("*.sqlite3")),
        ("x.sqlite", None),
        ("a/b/c.sqlite3", Some("*.sqlite3")),
        ("foo-credentials", Some("*-credentials")),
        ("foo-credentials/x", Some("*-credentials")),
        ("credentials", None),
        ("owner-credentials", Some("*-credentials")),
        ("a/owner-credentials/b", Some("*-credentials")),
        ("x.pem", Some("*.pem")),
        ("x.pem.bak", None),
        ("a.key", Some("*.key")),
        ("mykey", None),
        ("tokens", Some("*token*")),
        ("a/token", Some("*token*")),
        ("a/mytoken.txt", Some("*token*")),
        ("a/token/x", Some("*token*")),
        ("token/", Some("*token*")),
        ("./data/x", Some("data/")),
        ("././.env", Some(".env")),
        (".env", Some(".env")),
        ("a/.env", Some(".env")),
        (".env.local", None),
        ("env", None),
        (".envrc", None),
        ("docs/token-notes.md", Some("*token*")),
        ("src/lib.rs", None),
        ("README.md", None),
        ("a b/data", Some("data")),
        ("Data/x", None),
        ("DATA", None),
        ("a/b/c/d.sqlite3x", None),
        ("", None),
    ];
    for (path, want) in cases {
        assert_eq!(*want, matches(path), "matches({path:?})");
    }
}
