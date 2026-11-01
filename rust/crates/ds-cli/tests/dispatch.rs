//! T44 end to end: argv -> the generated parser -> `dispatch::run` prints what the real Python
//! CLI printed. Replays the recorded `context` steps (tests/context_cmd.json, built by
//! tools/gen_context_cmd_fixture.py from the real Python) through the WHOLE path, so a parser
//! or namespace mistake that the `context_read` differential cannot see shows up here.
//! One test fn: cwd and environment are process-wide.

use ds_cli::args::try_parse;
use ds_cli::dispatch::run;
use serde_json::Value;

const FIXTURE: &str = include_str!("context_cmd.json");

#[test]
fn recorded_context_steps_run_end_to_end_through_the_parser_and_dispatch() {
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture");
    let base = tempfile::tempdir().expect("tempdir");
    for (relative, text) in fixture["files"].as_object().unwrap() {
        let path = base.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text.as_str().unwrap()).unwrap();
    }
    std::env::set_var("DUNKINGSHEEP_DIR", base.path().join("cfg"));
    std::env::set_var("DUNKINGSHEEP_SELF", "fixture");
    std::env::set_current_dir(base.path().join("proj")).unwrap();
    let t = base.path().display().to_string();
    let mut bad = Vec::new();
    for step in fixture["steps"].as_array().unwrap() {
        let mut argv = vec!["dunkingsheep".to_string(), "context".to_string()];
        argv.extend(
            step["argv"]
                .as_array()
                .unwrap()
                .iter()
                .map(|v| v.as_str().unwrap().to_string()),
        );
        let ns = try_parse(&argv).unwrap_or_else(|e| panic!("{argv:?}: {e}"));
        let out = run(&ns);
        let (want_out, want_err) = (
            step["stdout"].as_str().unwrap(),
            step["stderr"].as_str().unwrap(),
        );
        let want_code = step["code"].as_i64().unwrap() as i32;
        let (got_out, got_err) = (out.stdout.replace(&t, "<T>"), out.stderr.replace(&t, "<T>"));
        if out.code != want_code || got_out != want_out || got_err != want_err {
            bad.push(format!(
                "{argv:?}\n  got:  code {} out {got_out:?} err {got_err:?}\n  want: code {want_code} out {want_out:?} err {want_err:?}",
                out.code
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "{} steps differ:\n{}",
        bad.len(),
        bad.join("\n")
    );

    // --json is valid JSON of the same result (key order differs from Python: documented).
    let ns = try_parse(["dunkingsheep", "context", "stats", "--json"]).unwrap();
    let out = run(&ns);
    assert_eq!(0, out.code);
    let parsed: Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    assert!(
        parsed["global"]["counts"]["ground-truth"].is_number(),
        "{parsed}"
    );
}

#[test]
fn commands_without_a_body_say_so_and_exit_1_and_writes_are_not_ported() {
    for argv in [
        vec!["dunkingsheep", "health"],
        vec!["dunkingsheep", "context", "init"],
    ] {
        let ns = try_parse(&argv).unwrap();
        let out = run(&ns);
        assert_eq!(1, out.code, "{argv:?}");
        assert_eq!("", out.stdout);
        assert!(
            out.stderr.contains("is parsed but not ported yet"),
            "{}",
            out.stderr
        );
    }
}

/// `secrets-check` through argv -> parser -> dispatch, against real temporary git repositories.
#[test]
fn secrets_check_runs_end_to_end_and_its_exit_code_is_what_hooks_branch_on() {
    use std::process::Command;
    let git = |dir: &std::path::Path, args: &[&str]| {
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
    };
    // t1490: removed when the test ends, not left in the temp dir by every run.
    let scratch = tempfile::Builder::new()
        .prefix("ds-dispatch-secrets-")
        .tempdir()
        .expect("scratch dir");
    let dir = scratch.path().to_path_buf();
    git(&dir, &["init", "-q", "-b", "main"]);
    git(&dir, &["config", "user.email", "t@example.com"]);
    git(&dir, &["config", "user.name", "t"]);
    std::fs::write(dir.join("README.md"), "base\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "base"]);
    git(&dir, &["checkout", "-q", "-b", "lane/leak"]);
    let d = dir.display().to_string();
    let check = || {
        let ns = try_parse([
            "dunkingsheep",
            "secrets-check",
            "--cwd",
            d.as_str(),
            "--base",
            "main",
            "--quiet",
        ])
        .unwrap();
        run(&ns)
    };
    assert_eq!(0, check().code, "clean");
    std::fs::create_dir_all(dir.join("data")).unwrap();
    std::fs::write(dir.join("data/owner-credentials"), "Password: hunter2\n").unwrap();
    git(&dir, &["add", "-A"]);
    git(&dir, &["commit", "-q", "-m", "oops"]);
    let refused = check();
    assert_eq!(1, refused.code);
    assert_eq!("", refused.stdout);
    assert!(
        refused.stderr.contains("data/owner-credentials"),
        "{}",
        refused.stderr
    );
    assert!(
        refused.stderr.contains("must never leave this host"),
        "{}",
        refused.stderr
    );
}

/// `github wrappers` through argv -> parser -> dispatch, in a sandbox (a fake real `gh`, a
/// throwaway GIT_CONFIG_GLOBAL, a temp bin dir); and `github protect` stays honest.
#[test]
fn github_wrappers_runs_end_to_end_in_a_sandbox_and_protect_is_not_ported() {
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let real_dir = t.path().join("realbin");
    std::fs::create_dir_all(&real_dir).unwrap();
    let real = real_dir.join("gh");
    std::fs::write(&real, "#!/bin/sh\necho real\n").unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = t.path().join("gitconfig");
    std::fs::write(
        &cfg,
        "[credential \"https://github.com\"]\n\thelper = !gh auth git-credential\n",
    )
    .unwrap();
    std::env::set_var("GIT_CONFIG_GLOBAL", &cfg);
    let old_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var("PATH", format!("{}:/usr/bin:/bin", real_dir.display()));
    let bin = t.path().join("bin").display().to_string();
    let ns = try_parse([
        "dunkingsheep",
        "github",
        "wrappers",
        "--bin-dir",
        bin.as_str(),
    ])
    .unwrap();
    let out = run(&ns);
    std::env::set_var("PATH", old_path);
    assert_eq!(0, out.code, "{}", out.stderr);
    assert!(
        out.stdout.starts_with("gh_wrapper           "),
        "{}",
        out.stdout
    );
    assert!(
        out.stdout
            .contains("['!dunkingsheep git-credential', '!gh auth git-credential']"),
        "{}",
        out.stdout
    );
    assert!(t.path().join("bin/gh").exists());
    let ns = try_parse(["dunkingsheep", "github", "protect", "someorg/repo"]).unwrap();
    let out = run(&ns);
    assert_eq!(1, out.code);
    assert!(
        out.stderr
            .contains("`github protect` is parsed but not ported yet"),
        "{}",
        out.stderr
    );
}
