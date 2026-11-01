//! T44 end to end: argv -> the generated parser -> `dispatch::run` prints what the real Python
//! CLI printed. Replays the recorded `context` steps (tests/context_cmd.json, built by
//! tools/gen_context_cmd_fixture.py from the real Python) through the WHOLE path, so a parser
//! or namespace mistake that the `context_read` differential cannot see shows up here.
//! Environment and cwd are process-wide, so the fixtures that change them share a lock.

use ds_cli::args::try_parse;
use ds_cli::dispatch::run;
use serde_json::Value;
use std::sync::Mutex;

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn lock_env() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

const FIXTURE: &str = include_str!("context_cmd.json");

#[test]
fn recorded_context_steps_run_end_to_end_through_the_parser_and_dispatch() {
    let _env_lock = lock_env();
    let fixture: Value = serde_json::from_str(FIXTURE).expect("fixture");
    let base = tempfile::tempdir().expect("tempdir");
    for (relative, text) in fixture["files"].as_object().unwrap() {
        let path = base.path().join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text.as_str().unwrap()).unwrap();
    }
    let old_cwd = std::env::current_dir().unwrap();
    let old_config = std::env::var_os("DUNKINGSHEEP_DIR");
    let old_self = std::env::var_os("DUNKINGSHEEP_SELF");
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

    // --json preserves Python's insertion order and remains valid JSON.
    let ns = try_parse(["dunkingsheep", "context", "stats", "--json"]).unwrap();
    let out = run(&ns);
    assert_eq!(0, out.code);
    let parsed: Value = serde_json::from_str(&out.stdout).expect("valid JSON");
    assert!(
        parsed["global"]["counts"]["ground-truth"].is_number(),
        "{parsed}"
    );
    std::env::set_current_dir(old_cwd).unwrap();
    match old_config {
        Some(v) => std::env::set_var("DUNKINGSHEEP_DIR", v),
        None => std::env::remove_var("DUNKINGSHEEP_DIR"),
    }
    match old_self {
        Some(v) => std::env::set_var("DUNKINGSHEEP_SELF", v),
        None => std::env::remove_var("DUNKINGSHEEP_SELF"),
    }
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
    let _env_lock = lock_env();
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
/// throwaway GIT_CONFIG_GLOBAL, a temp bin dir).
#[test]
fn github_wrappers_runs_end_to_end_in_a_sandbox() {
    let _env_lock = lock_env();
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
    let old_git_config = std::env::var_os("GIT_CONFIG_GLOBAL");
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
    match old_git_config {
        Some(v) => std::env::set_var("GIT_CONFIG_GLOBAL", v),
        None => std::env::remove_var("GIT_CONFIG_GLOBAL"),
    }
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
}

/// `github protect` prints the payload in dry-run mode and sends both API calls only to a fake
/// `gh` executable. No owner credentials or live branch-protection endpoint are used.
#[test]
fn github_protect_uses_fixture_app_and_fake_gh() {
    let _env_lock = lock_env();
    use std::os::unix::fs::PermissionsExt;

    let t = tempfile::tempdir().unwrap();
    let config = t.path().join("config");
    let fake_bin = t.path().join("fake-bin");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::create_dir_all(&fake_bin).unwrap();
    let pem = config.join("reviewer.pem");
    std::fs::write(&pem, "fixture key, never read").unwrap();
    std::fs::write(
        config.join("github-app-reviewer.json"),
        serde_json::json!({
            "id": 5531872,
            "slug": "dunkingsheep-reviewer",
            "pem": pem,
        })
        .to_string(),
    )
    .unwrap();

    let args_capture = t.path().join("gh-args.txt");
    let input_capture = t.path().join("gh-inputs.txt");
    let fake_gh = fake_bin.join("gh");
    std::fs::write(
        &fake_gh,
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$DS_GH_ARGS_FILE\"\ncat >> \"$DS_GH_INPUTS_FILE\"\nprintf '\\n--END--\\n' >> \"$DS_GH_INPUTS_FILE\"\nif [ \"$3\" = PUT ]; then printf '{\"ok\":true}\\n'; exit 0; fi\nprintf 'fixture auto-merge refusal\\n' >&2\nexit 1\n",
    )
    .unwrap();
    std::fs::set_permissions(&fake_gh, std::fs::Permissions::from_mode(0o755)).unwrap();

    let old_config = std::env::var_os("DUNKINGSHEEP_DIR");
    let old_path = std::env::var_os("PATH");
    let old_args_capture = std::env::var_os("DS_GH_ARGS_FILE");
    let old_input_capture = std::env::var_os("DS_GH_INPUTS_FILE");
    std::env::set_var("DUNKINGSHEEP_DIR", &config);
    std::env::set_var("PATH", format!("{}:/usr/bin:/bin", fake_bin.display()));
    std::env::set_var("DS_GH_ARGS_FILE", &args_capture);
    std::env::set_var("DS_GH_INPUTS_FILE", &input_capture);

    let dry_ns = try_parse([
        "dunkingsheep",
        "github",
        "protect",
        "someorg/repo",
        "--approvals",
        "2",
        "--bypass-reviewer-app",
        "--dry-run",
    ])
    .unwrap();
    let dry = run(&dry_ns);
    let dry_invoked_gh = args_capture.exists();
    let apply_ns = try_parse([
        "dunkingsheep",
        "github",
        "protect",
        "someorg/repo",
        "--approvals",
        "2",
        "--bypass-reviewer-app",
    ])
    .unwrap();
    let applied = run(&apply_ns);

    match old_config {
        Some(value) => std::env::set_var("DUNKINGSHEEP_DIR", value),
        None => std::env::remove_var("DUNKINGSHEEP_DIR"),
    }
    match old_path {
        Some(value) => std::env::set_var("PATH", value),
        None => std::env::remove_var("PATH"),
    }
    match old_args_capture {
        Some(value) => std::env::set_var("DS_GH_ARGS_FILE", value),
        None => std::env::remove_var("DS_GH_ARGS_FILE"),
    }
    match old_input_capture {
        Some(value) => std::env::set_var("DS_GH_INPUTS_FILE", value),
        None => std::env::remove_var("DS_GH_INPUTS_FILE"),
    }

    assert_eq!(0, dry.code, "{}", dry.stderr);
    assert!(!dry_invoked_gh, "dry-run invoked gh");
    let payload: Value = serde_json::from_str(&dry.stdout).expect("dry-run JSON payload");
    assert_eq!(
        serde_json::json!([{"context": "dunkingsheep/review", "app_id": 5531872}]),
        payload["required_status_checks"]["checks"]
    );
    assert_eq!(false, payload["required_status_checks"]["strict"]);
    assert_eq!(true, payload["enforce_admins"]);
    assert_eq!(
        true,
        payload["required_pull_request_reviews"]["dismiss_stale_reviews"]
    );
    assert_eq!(
        false,
        payload["required_pull_request_reviews"]["require_code_owner_reviews"]
    );
    assert_eq!(
        2,
        payload["required_pull_request_reviews"]["required_approving_review_count"]
    );
    assert_eq!(
        serde_json::json!(["dunkingsheep-reviewer"]),
        payload["required_pull_request_reviews"]["bypass_pull_request_allowances"]["apps"]
    );
    assert_eq!(Value::Null, payload["restrictions"]);
    assert_eq!(false, payload["allow_force_pushes"]);
    assert_eq!(false, payload["allow_deletions"]);
    assert_eq!(false, payload["required_linear_history"]);

    assert_eq!(0, applied.code, "{}", applied.stderr);
    assert_eq!(
        "protected someorg/repo: 2 approval(s), dunkingsheep/review required (app 5531872), auto-merge NOT enabled: fixture auto-merge refusal\n",
        applied.stdout
    );
    let args = std::fs::read_to_string(&args_capture).unwrap();
    assert_eq!(
        "api -X PUT repos/someorg/repo/branches/main/protection --input -\napi -X PATCH repos/someorg/repo --input -\n",
        args
    );
    let inputs = std::fs::read_to_string(&input_capture).unwrap();
    let bodies: Vec<Value> = inputs
        .split("\n--END--\n")
        .filter(|body| !body.trim().is_empty())
        .map(|body| serde_json::from_str(body).expect("fake gh JSON body"))
        .collect();
    assert_eq!(2, bodies.len());
    assert_eq!(payload, bodies[0]);
    assert_eq!(
        serde_json::json!({"allow_auto_merge": true, "delete_branch_on_merge": true}),
        bodies[1]
    );
}

/// `install` writes the Rust launcher and stdlib poll wrapper only in the requested sandbox.
#[test]
fn install_launcher_runs_end_to_end_in_a_sandbox() {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    let temp = tempfile::tempdir().unwrap();
    let bin_dir = temp.path().join("bin");
    let bin_text = bin_dir.display().to_string();
    let path = format!("{bin_text}:/usr/bin:/bin");
    let output = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"))
        .args(["install", "--bin-dir", &bin_text])
        .env("PATH", &path)
        .env("HOME", temp.path())
        .env_remove("DUNKINGSHEEP_DIR")
        .env_remove("DUNKINGSHEEP_SOCKET")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
        .output()
        .expect("run Rust install command");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let cli = std::fs::canonicalize(env!("CARGO_BIN_EXE_dunkingsheep")).unwrap();
    let project_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .unwrap();
    let poller = project_root.join("tools/dunk_poll.py");
    let launcher = bin_dir.join("dunkingsheep");
    let poll_launcher = bin_dir.join("dunk-poll");
    let rows = [
        ("launcher", launcher.display().to_string()),
        ("poll_launcher", poll_launcher.display().to_string()),
        ("runs", cli.display().to_string()),
        ("poll_runs", poller.display().to_string()),
        ("on_path_here", "True".to_string()),
        ("resolves_to", launcher.display().to_string()),
        ("poll_resolves_to", poll_launcher.display().to_string()),
    ];
    let mut expected = rows
        .iter()
        .map(|(key, value)| format!("{key:<16} {value}\n"))
        .collect::<String>();
    expected.push_str(
        "\nSuperuser setup: run `dunkingsheep superuser recover` from the VP pane, then `dunkingsheep superuser status`.\n",
    );
    assert_eq!(expected, String::from_utf8(output.stdout).unwrap());

    let launcher_body = std::fs::read_to_string(&launcher).unwrap();
    assert!(launcher_body.starts_with("#!/bin/sh\n"));
    assert!(launcher_body.contains(&cli.display().to_string()));
    assert!(launcher_body.ends_with("\"$@\"\n"));
    let poll_body = std::fs::read_to_string(&poll_launcher).unwrap();
    assert!(poll_body.starts_with("#!/bin/sh\nexec "));
    assert!(poll_body.contains(&poller.display().to_string()));
    assert!(poll_body.ends_with("\"$@\"\n"));
    for path in [launcher, poll_launcher] {
        assert_eq!(
            0o755,
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777
        );
    }
}

/// Guard installation is exercised only under a disposable HOME/config/PATH. The
/// fake herdr proves --no-shadow leaves the selected binary in place; fake tmux
/// covers both the helper selection and generated wrapper without launching tmux.
#[test]
fn guard_install_actions_write_only_to_sandbox_and_keep_tmux_helper() {
    let _env_lock = lock_env();
    use std::os::unix::fs::PermissionsExt;
    let t = tempfile::tempdir().unwrap();
    let home = t.path().join("home");
    let config = home.join(".config/dunkingsheep");
    let bin = t.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let herdr = bin.join("herdr");
    let tmux = bin.join("tmux");
    for path in [&herdr, &tmux] {
        std::fs::write(path, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let old_home = std::env::var_os("HOME");
    let old_config = std::env::var_os("DUNKINGSHEEP_DIR");
    let old_path = std::env::var_os("PATH");
    let old_tmux = std::env::var_os("DUNKINGSHEEP_TMUX_BIN");
    std::env::set_var("HOME", &home);
    std::env::set_var("DUNKINGSHEEP_DIR", &config);
    std::env::set_var("PATH", &bin);
    std::env::set_var("DUNKINGSHEEP_TMUX_BIN", &tmux);

    let ns = try_parse(["dunkingsheep", "guard", "install", "--no-shadow", "--json"]).unwrap();
    let out = run(&ns);
    assert_eq!(0, out.code, "{}", out.stderr);
    let installed: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(true, installed["installed"]);
    assert_eq!(false, installed["shadowing_herdr"]);
    assert!(config.join("bin/herdr").is_file());
    assert_eq!(
        "#!/bin/sh\nexit 0\n",
        std::fs::read_to_string(&herdr).unwrap()
    );
    assert!(std::fs::read_to_string(home.join(".bashrc"))
        .unwrap()
        .contains(config.join("bin").to_str().unwrap()));

    let ns = try_parse(["dunkingsheep", "guard", "install-tmux", "--json"]).unwrap();
    let out = run(&ns);
    assert_eq!(0, out.code, "{}", out.stderr);
    let installed: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(true, installed["ok"]);
    assert_eq!(
        tmux.canonicalize().unwrap().to_str().unwrap(),
        installed["real_tmux"]
    );
    assert!(config.join("tmux-guard/tmux").is_file());
    assert_eq!(
        tmux.canonicalize().unwrap(),
        config.join("tmux-guard/tmux-real").canonicalize().unwrap()
    );
    let wrapper = std::fs::read_to_string(config.join("tmux-guard/tmux")).unwrap();
    assert!(wrapper.contains("dunkingsheep-tmux-guard"));
    assert!(!wrapper.contains("os.path.expanduser(\n    \"~/.local/bin/dunkingsheep\")"));

    // Python `install()` returns the complete current status, including an existing
    // tmux guard and the real herdr resolved from PATH, even in --no-shadow mode.
    let ns = try_parse(["dunkingsheep", "guard", "install", "--no-shadow", "--json"]).unwrap();
    let out = run(&ns);
    assert_eq!(0, out.code, "{}", out.stderr);
    let status: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(
        herdr.canonicalize().unwrap().to_str().unwrap(),
        status["real_herdr"]
    );
    assert_eq!(
        herdr.canonicalize().unwrap().to_str().unwrap(),
        status["herdr_resolves_to"]
    );
    assert_eq!(true, status["tmux_guard"]["installed"]);
    assert_eq!(
        config.join("tmux-guard/tmux").to_str().unwrap(),
        status["tmux_guard"]["wrapper"]
    );
    assert_eq!(
        config.join("tmux-guard/tmux-real").to_str().unwrap(),
        status["tmux_guard"]["real_tmux"]
    );

    // Python checks the tmux wrapper and backing binary independently.
    std::fs::remove_file(config.join("tmux-guard/tmux-real")).unwrap();
    let ns = try_parse(["dunkingsheep", "guard", "install", "--no-shadow", "--json"]).unwrap();
    let out = run(&ns);
    assert_eq!(0, out.code, "{}", out.stderr);
    let status: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(true, status["tmux_guard"]["installed"]);
    assert_eq!(
        config.join("tmux-guard/tmux").to_str().unwrap(),
        status["tmux_guard"]["wrapper"]
    );
    assert_eq!(Value::Null, status["tmux_guard"]["real_tmux"]);

    match old_home {
        Some(v) => std::env::set_var("HOME", v),
        None => std::env::remove_var("HOME"),
    }
    match old_config {
        Some(v) => std::env::set_var("DUNKINGSHEEP_DIR", v),
        None => std::env::remove_var("DUNKINGSHEEP_DIR"),
    }
    match old_path {
        Some(v) => std::env::set_var("PATH", v),
        None => std::env::remove_var("PATH"),
    }
    match old_tmux {
        Some(v) => std::env::set_var("DUNKINGSHEEP_TMUX_BIN", v),
        None => std::env::remove_var("DUNKINGSHEEP_TMUX_BIN"),
    }
}
