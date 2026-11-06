//! Guard shims (t3230, audit P0-8) against a fake packaged launcher and a fake real herdr. Nothing
//! here starts herdr or tmux or types into a pane: every CLI call is answered by a script in the
//! sandbox, and every process gets its environment from the test, not from this process.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Stands in for `~/.local/bin/dunkingsheep`, the packaged launcher. It logs its arguments and the
/// guard marker, then answers with the exit status and stderr the test asks for.
const FAKE_LAUNCHER: &str = r#"#!/bin/sh
if [ -n "${FAKE_CLI_LOG:-}" ]; then
    printf 'GUARD=%s ARGS=%s\n' "${DUNKINGSHEEP_GUARD:-}" "$*" >> "$FAKE_CLI_LOG"
fi
if [ -n "${FAKE_CLI_STDERR:-}" ]; then
    printf '%s\n' "$FAKE_CLI_STDERR" >&2
fi
exit "${FAKE_CLI_EXIT:-0}"
"#;

/// Stands in for the real herdr. Reaching it means the guard let a command through.
const FAKE_REAL_HERDR: &str = r#"#!/bin/sh
printf 'REAL %s\n' "$*" >> "$FAKE_HERDR_LOG"
echo "real herdr ran"
"#;

fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, body).expect("write fake script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
}

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    home: PathBuf,
    config: PathBuf,
    cli_log: PathBuf,
    herdr_log: PathBuf,
    real_herdr: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("sandbox");
        let root = std::fs::canonicalize(dir.path()).expect("canonical sandbox");
        let home = root.join("home");
        let config = home.join(".config/dunkingsheep");
        let launcher_dir = home.join(".local/bin");
        std::fs::create_dir_all(&launcher_dir).expect("launcher dir");
        write_script(&launcher_dir.join("dunkingsheep"), FAKE_LAUNCHER);
        let real_herdr = root.join("herdr-real");
        write_script(&real_herdr, FAKE_REAL_HERDR);
        Self {
            cli_log: root.join("cli.log"),
            herdr_log: root.join("herdr.log"),
            _dir: dir,
            root,
            home,
            config,
            real_herdr,
        }
    }

    fn launcher(&self) -> PathBuf {
        self.home.join(".local/bin/dunkingsheep")
    }

    /// Runs the shipped `dunkingsheep` binary with a sandboxed HOME and config directory.
    fn dunkingsheep(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_dunkingsheep"));
        command
            .args(args)
            .env("HOME", &self.home)
            .env("DUNKINGSHEEP_DIR", &self.config)
            .env("PATH", "/usr/bin:/bin")
            .env("FAKE_CLI_LOG", &self.cli_log)
            .env_remove("DUNKINGSHEEP_GUARD")
            .env_remove("HERDR_GUARD_OFF");
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("run dunkingsheep")
    }

    /// Renders the herdr shim with the shipped `guard install`, so the tests exercise that path.
    fn install_herdr_shim(&self) -> PathBuf {
        let out = self.dunkingsheep(&["guard", "install", "--no-shadow", "--json"], &[]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        self.config.join("bin/herdr")
    }

    /// Runs the rendered herdr shim the way an agent pane would.
    fn run_shim(&self, shim: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new("python3");
        command
            .arg(shim)
            .args(args)
            .env("HOME", &self.home)
            .env("PATH", "/usr/bin:/bin")
            .env("DUNKINGSHEEP_HERDR_BIN", &self.real_herdr)
            .env("FAKE_CLI_LOG", &self.cli_log)
            .env("FAKE_HERDR_LOG", &self.herdr_log)
            .env_remove("DUNKINGSHEEP_GUARD")
            .env_remove("HERDR_GUARD_OFF");
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().expect("run herdr shim")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn rendered_herdr_shim_points_at_the_packaged_launcher_and_no_repo_path() {
    let fixture = Fixture::new();
    let shim = fixture.install_herdr_shim();
    let rendered = std::fs::read_to_string(&shim).expect("rendered shim");
    let expected = format!("DUNKINGSHEEP = \"{}\"", fixture.launcher().display());
    assert!(
        rendered.contains(&expected),
        "shim must name the launcher: {expected}"
    );
    assert!(
        !rendered.contains(env!("CARGO_MANIFEST_DIR")),
        "the shim must not embed a build-tree or repo path"
    );
    assert!(
        !rendered.contains("[sys.executable, DUNKINGSHEEP"),
        "the CLI must not run through sys.executable"
    );
    assert!(
        rendered.contains("dunkingsheep-herdr-guard"),
        "the guard marker must stay"
    );
}

#[test]
fn rendered_tmux_shim_points_at_the_packaged_launcher_and_no_repo_path() {
    let fixture = Fixture::new();
    let tmux = fixture.root.join("tmux");
    write_script(&tmux, "#!/bin/sh\nexit 0\n");
    let out = fixture.dunkingsheep(
        &["guard", "install-tmux", "--json"],
        &[("DUNKINGSHEEP_TMUX_BIN", tmux.to_str().expect("utf-8 path"))],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let rendered =
        std::fs::read_to_string(fixture.config.join("tmux-guard/tmux")).expect("tmux shim");
    let expected = format!("\"{}\"", fixture.launcher().display());
    assert!(
        rendered.contains(&expected),
        "tmux shim must name the launcher"
    );
    assert!(!rendered.contains(env!("CARGO_MANIFEST_DIR")));
    assert!(!rendered.contains("[sys.executable, DUNKINGSHEEP"));
}

#[test]
fn allowed_typing_reaches_the_packaged_cli_and_never_the_real_herdr() {
    let fixture = Fixture::new();
    let shim = fixture.install_herdr_shim();
    // Before the fix the shim ran the CLI as Python, which fails on a non-Python program. A
    // shell fixture here exercises the direct execution the packaged launcher needs.
    let out = fixture.run_shim(&shim, &["pane", "run", "pane-7", "echo hi"], &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let log = std::fs::read_to_string(&fixture.cli_log).expect("cli log");
    assert!(
        log.contains("GUARD=1 ARGS=--json send pane-7 echo hi"),
        "{log}"
    );
    assert!(
        !fixture.herdr_log.exists(),
        "typing must never reach the real herdr"
    );
}

#[test]
fn refused_typing_stays_refused_and_never_falls_back_to_the_real_herdr() {
    let fixture = Fixture::new();
    let shim = fixture.install_herdr_shim();
    let out = fixture.run_shim(
        &shim,
        &["pane", "run", "pane-7", "echo hi"],
        &[
            ("FAKE_CLI_EXIT", "1"),
            ("FAKE_CLI_STDERR", "refused: superuser allowlist"),
        ],
    );
    assert!(!out.status.success(), "a refused send must fail");
    assert!(text(&out.stderr).contains("refused: superuser allowlist"));
    assert!(
        !fixture.herdr_log.exists(),
        "a refusal must not fall back to real herdr"
    );
}

#[test]
fn an_unreachable_cli_refuses_and_never_falls_back_to_the_real_herdr() {
    let fixture = Fixture::new();
    let shim = fixture.install_herdr_shim();
    std::fs::remove_file(fixture.launcher()).expect("remove launcher");
    let out = fixture.run_shim(&shim, &["pane", "run", "pane-7", "echo hi"], &[]);
    assert!(!out.status.success(), "an unreachable daemon must refuse");
    assert!(
        text(&out.stderr).contains("daemon unreachable"),
        "{}",
        text(&out.stderr)
    );
    assert!(
        !fixture.herdr_log.exists(),
        "an unreachable daemon must not reach real herdr"
    );
}

#[test]
fn unguarded_commands_pass_through_to_the_real_herdr() {
    let fixture = Fixture::new();
    let shim = fixture.install_herdr_shim();
    let out = fixture.run_shim(&shim, &["pane", "list"], &[]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let log = std::fs::read_to_string(&fixture.herdr_log).expect("herdr log");
    assert!(log.contains("REAL pane list"), "{log}");
    assert!(
        !fixture.cli_log.exists(),
        "an unguarded command must not call the CLI"
    );
}

#[test]
fn guard_check_passes_with_the_packaged_cli_refusing_the_probe_pane() {
    let fixture = Fixture::new();
    fixture.install_herdr_shim();
    // The probe pane does not exist, so the daemon says so, as it does for a real herdr.
    let out = fixture.dunkingsheep(
        &["guard", "check", "--json"],
        &[
            ("FAKE_CLI_EXIT", "1"),
            ("FAKE_CLI_STDERR", "no pane matches no-such-pane-xyz"),
        ],
    );
    assert!(out.status.success(), "{}", text(&out.stderr));
    let value: Value = serde_json::from_slice(&out.stdout).expect("guard check json");
    assert_eq!(Value::Bool(true), value["ok"], "{value}");
}
