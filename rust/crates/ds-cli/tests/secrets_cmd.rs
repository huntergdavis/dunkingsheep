//! T44: the real secrets guard behind `git-credential`, and the `secrets-check` command,
//! against real temporary git repositories (that is where the leak happened).

use std::path::{Path, PathBuf};
use std::process::Command;

use ds_cli::local::git_credential;
use ds_cli::secrets_cmd::{guard_refusal, secrets_check_command};
use serde_json::json;

fn git(dir: &Path, args: &[&str]) {
    let done = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .expect("git");
    assert!(
        done.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&done.stderr)
    );
}

/// A repository path that owns its directory: removed when the test drops it (t1490; these
/// used to be left in the temp dir by every run).
struct Repo {
    path: PathBuf,
    _tmp: tempfile::TempDir,
}

impl std::ops::Deref for Repo {
    type Target = PathBuf;
    fn deref(&self) -> &PathBuf {
        &self.path
    }
}

fn repo(tag: &str) -> Repo {
    let tmp = tempfile::Builder::new()
        .prefix(&format!("ds-cli-secrets-{tag}-"))
        .tempdir()
        .expect("scratch dir");
    let dir = tmp.path().to_path_buf();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.email", "t@example.com"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("README.md"), "base\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    git(&dir, &["checkout", "-q", "-b", "lane/x"]);
    Repo {
        path: dir,
        _tmp: tmp,
    }
}

fn leak(dir: &Path) {
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::write(dir.join("data/owner-credentials"), "Password: hunter2\n").unwrap();
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-q", "-m", "oops"]);
}

const GITHUB: &str = "protocol=https\nhost=github.com\n\n";

fn credential(dir: &Path, token: serde_json::Value) -> (String, String) {
    let (mut out, mut err) = (Vec::new(), Vec::new());
    // `base` is the default here, as in `_git_credential`: with a `main` branch it resolves.
    let dir = dir.to_path_buf();
    git_credential(
        "get",
        &mut GITHUB.as_bytes(),
        &mut out,
        &mut err,
        &move || guard_refusal(&dir),
        &move || Ok(token.clone()),
    )
    .unwrap();
    (
        String::from_utf8(out).unwrap(),
        String::from_utf8(err).unwrap(),
    )
}

#[test]
fn a_clean_branch_gets_its_credential() {
    let dir = repo("clean");
    std::fs::write(dir.join("feature.py"), "x").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "feature"]);
    let (out, err) = credential(&dir, json!({"token": "ghs_x"}));
    assert_eq!("username=x-access-token\npassword=ghs_x\n", out);
    assert_eq!("", err);
}

#[test]
fn a_branch_that_adds_a_secret_is_refused_before_a_token_is_handed_out() {
    let dir = repo("leak");
    leak(&dir);
    let (out, err) = credential(&dir, json!({"token": "ghs_x"}));
    assert_eq!("", out, "no credential on a refusal");
    assert!(err.contains("data/owner-credentials"), "{err}");
    assert!(
        err.contains("REFUSED by the dunkingsheep git credential helper (t756)."),
        "{err}"
    );
}

#[test]
fn secrets_check_exits_1_with_the_refusal_on_stderr_and_0_when_clean() {
    let dir = repo("check");
    let dir_text = dir.display().to_string();
    assert_eq!(
        0,
        secrets_check_command(Some(&dir_text), Some("main"), true).code
    );
    leak(&dir);
    let refused = secrets_check_command(Some(&dir_text), Some("main"), true);
    assert_eq!(1, refused.code);
    assert_eq!("", refused.stdout);
    assert!(
        refused.stderr.contains("must never leave this host"),
        "{}",
        refused.stderr
    );
}

#[test]
fn an_unknown_base_is_a_warning_unless_quiet() {
    let dir = repo("warn");
    let dir_text = dir.display().to_string();
    let loud = secrets_check_command(Some(&dir_text), Some("no-such-ref"), false);
    assert_eq!(0, loud.code);
    assert!(
        loud.stderr.contains("WARNING could not fully check"),
        "{}",
        loud.stderr
    );
    assert_eq!(
        "",
        secrets_check_command(Some(&dir_text), Some("no-such-ref"), true).stderr
    );
}
