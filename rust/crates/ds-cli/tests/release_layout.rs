//! Release layout, install, rollback and an isolated probe for the Rust CLI (t3229, P0-1).
//!
//! Every test works in its own temporary directories. The scripts receive every path
//! explicitly, and their PATH is fixed. Nothing reaches the real home, systemd or a
//! live state directory.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use ds_cli::release_layout::{
    is_release_id, launcher_body, poll_launcher_body, release_dir_of, shell_quote,
};

fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("repository root")
        .to_path_buf()
}

fn run_script(name: &str, args: &[&str]) -> Output {
    run_script_env(name, args, &[])
}

/// Runs a release script with the test fixtures enabled. Later entries in `env` override earlier ones.
fn run_script_env(name: &str, args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut command = Command::new("sh");
    command
        .arg(project_root().join("packaging/rust-release").join(name))
        .args(args)
        .env("PATH", "/usr/bin:/bin")
        .env("DUNKINGSHEEP_RELEASE_TEST_FIXTURES", "1");
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().expect("run release script")
}

fn ok(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn sha256(path: &str) -> String {
    let output = Command::new("sha256sum")
        .arg(path)
        .output()
        .expect("sha256sum");
    String::from_utf8(output.stdout)
        .expect("sha256sum output")
        .split_whitespace()
        .next()
        .expect("digest")
        .to_string()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

struct Sandbox {
    _dir: tempfile::TempDir,
    root: PathBuf,
    prefix: PathBuf,
    bin: PathBuf,
    unit: PathBuf,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("temporary sandbox");
        let root = std::fs::canonicalize(dir.path()).expect("canonical sandbox");
        let prefix = root.join("prefix");
        let bin = root.join("bin");
        let unit = root.join("unit");
        for path in [&prefix, &bin, &unit] {
            std::fs::create_dir_all(path).expect("sandbox directory");
        }
        Self {
            _dir: dir,
            root,
            prefix,
            bin,
            unit,
        }
    }

    fn release(&self, id: &str) -> PathBuf {
        self.prefix.join("releases").join(id)
    }

    fn dropin(&self) -> PathBuf {
        self.unit
            .join("dunkingsheep.service.d/99-rust-release.conf")
    }

    fn install(&self, id: &str) -> Output {
        self.install_with(id, &[])
    }

    fn install_with(&self, id: &str, extra: &[&str]) -> Output {
        self.install_with_env(id, extra, &[], false)
    }

    /// Runs the installer. With `standard` false the fixtures pass --no-standard-scan, so they never
    /// read the host's drop-ins. With `standard` true the standard systemd load paths are scanned.
    fn install_with_env(
        &self,
        id: &str,
        extra: &[&str],
        env: &[(&str, &str)],
        standard: bool,
    ) -> Output {
        let poll = PathBuf::from(env!("CARGO_BIN_EXE_dunk-poll"));
        let mut args = vec![
            "--binary",
            env!("CARGO_BIN_EXE_dunkingsheep"),
            "--poll",
            poll.to_str().expect("utf-8 path"),
            "--release-id",
            id,
            "--prefix",
            self.prefix.to_str().expect("utf-8 path"),
            "--bin-dir",
            self.bin.to_str().expect("utf-8 path"),
            "--unit-dir",
            self.unit.to_str().expect("utf-8 path"),
            "--revision",
            "f76c1801",
        ];
        if !standard {
            args.push("--no-standard-scan");
        }
        args.extend_from_slice(extra);
        run_script_env("install-release.sh", &args, env)
    }

    fn uninstall(&self, extra: &[&str]) -> Output {
        let mut args = vec![
            "--prefix",
            self.prefix.to_str().expect("utf-8 path"),
            "--bin-dir",
            self.bin.to_str().expect("utf-8 path"),
            "--unit-dir",
            self.unit.to_str().expect("utf-8 path"),
        ];
        args.extend_from_slice(extra);
        run_script("uninstall-release.sh", &args)
    }

    fn rollback(&self, extra: &[&str]) -> Output {
        let mut args = vec![
            "--prefix",
            self.prefix.to_str().expect("utf-8 path"),
            "--bin-dir",
            self.bin.to_str().expect("utf-8 path"),
            "--unit-dir",
            self.unit.to_str().expect("utf-8 path"),
        ];
        args.push("--no-standard-scan");
        args.extend_from_slice(extra);
        run_script("rollback-release.sh", &args)
    }
}

fn python_era_launcher() -> &'static str {
    "#!/bin/sh\n# python-era launcher (fixture)\nexec /usr/bin/python3 old-release\n"
}

#[test]
fn release_dir_of_accepts_only_an_installed_release_binary() {
    let installed = Path::new("/srv/dunk/releases/t3229-abc/dunkingsheep");
    assert_eq!(
        Some(Path::new("/srv/dunk/releases/t3229-abc")),
        release_dir_of(installed).ok().as_deref()
    );
    for refused in [
        "/repo/rust/target/debug/dunkingsheep",
        "/home/x/bin/dunkingsheep",
        "/srv/dunk/releases/bad id/dunkingsheep",
    ] {
        let error = release_dir_of(Path::new(refused)).expect_err(refused);
        assert!(error.contains("refusing to install launchers"), "{error}");
    }
    assert!(is_release_id("t3229.r1_abc-2"));
    assert!(!is_release_id("-leading"));
    assert!(!is_release_id("has space"));
}

#[test]
fn launchers_point_only_at_the_release_binary_and_a_pinned_python() {
    let binary = Path::new("/srv/dunk/releases/r1/dunkingsheep");
    assert_eq!(
        "#!/bin/sh\n# Dunking Sheep Rust CLI launcher, installed by `dunkingsheep install`.\nexec '/srv/dunk/releases/r1/dunkingsheep' \"$@\"\n",
        launcher_body(binary)
    );
    let poll = poll_launcher_body(
        Path::new("/usr/bin/python3"),
        Path::new("/srv/dunk/releases/r1/dunk-poll.py"),
    );
    assert_eq!(
        "#!/bin/sh\n# Dunking Sheep poll wrapper, installed by `dunkingsheep install`.\nexec '/usr/bin/python3' '/srv/dunk/releases/r1/dunk-poll.py' \"$@\"\n",
        poll
    );
    assert_eq!("'it'\\''s'", shell_quote("it's"));
}

#[test]
fn install_writes_an_immutable_release_launchers_and_drop_in() {
    let sandbox = Sandbox::new();
    ok(&sandbox.install("t3229-test"));
    let release = sandbox.release("t3229-test");
    let binary = release.join("dunkingsheep");
    let poll = release.join("dunk-poll");
    assert_eq!(0o555, mode(&binary));
    assert_eq!(0o555, mode(&poll));

    let manifest = std::fs::read_to_string(release.join("MANIFEST")).expect("manifest");
    assert!(manifest.contains("release_id=t3229-test\n"), "{manifest}");
    assert!(manifest.contains("revision=f76c1801\n"), "{manifest}");
    let source_sha = sha256(env!("CARGO_BIN_EXE_dunkingsheep"));
    assert!(
        manifest.contains(&format!("binary_sha256={source_sha}\n")),
        "{manifest}"
    );

    let launcher = std::fs::read_to_string(sandbox.bin.join("dunkingsheep")).expect("launcher");
    assert_eq!(launcher_body(&binary), launcher);
    let poll_launcher =
        std::fs::read_to_string(sandbox.bin.join("dunk-poll")).expect("poll launcher");
    assert!(poll_launcher.starts_with("#!/bin/sh\n"));
    assert!(poll_launcher.contains(&shell_quote(&poll.display().to_string())));
    assert!(!poll_launcher.contains("/target/"));

    let dropin = std::fs::read_to_string(sandbox.dropin()).expect("drop-in");
    assert!(dropin.contains("# release_id=t3229-test\n"), "{dropin}");
    assert!(
        dropin.contains("[Service]\nExecStart=\nExecStart="),
        "{dropin}"
    );
    assert!(
        dropin.contains(&format!("ExecStart={} serve\n", binary.display())),
        "{dropin}"
    );
    assert!(
        dropin.contains("Environment=DUNKINGSHEEP_SUPERVISED=1\n"),
        "{dropin}"
    );
    assert!(!dropin.contains("/target/"), "{dropin}");

    // The same id with identical bytes is a no-op.
    ok(&sandbox.install("t3229-test"));
    // The same id with different bytes is refused, because releases are immutable.
    let other = sandbox.root.join("other-binary");
    std::fs::write(&other, "#!/bin/sh\necho different\n").expect("other binary");
    std::fs::set_permissions(&other, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let poll_path = PathBuf::from(env!("CARGO_BIN_EXE_dunk-poll"));
    let refused = run_script(
        "install-release.sh",
        &[
            "--binary",
            other.to_str().expect("utf-8 path"),
            "--poll",
            poll_path.to_str().expect("utf-8 path"),
            "--release-id",
            "t3229-test",
            "--prefix",
            sandbox.prefix.to_str().expect("utf-8 path"),
            "--bin-dir",
            sandbox.bin.to_str().expect("utf-8 path"),
            "--unit-dir",
            sandbox.unit.to_str().expect("utf-8 path"),
            "--no-standard-scan",
        ],
    );
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("releases are immutable"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
}

#[test]
fn uninstall_removes_our_artifacts_and_keeps_releases_unless_purged() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.bin.join("dunkingsheep"), python_era_launcher()).expect("launcher");
    ok(&sandbox.install("rel-u"));
    let uninstall_args = [
        "--prefix",
        sandbox.prefix.to_str().expect("utf-8 path"),
        "--bin-dir",
        sandbox.bin.to_str().expect("utf-8 path"),
        "--unit-dir",
        sandbox.unit.to_str().expect("utf-8 path"),
    ];
    ok(&run_script("uninstall-release.sh", &uninstall_args));
    assert!(!sandbox.dropin().exists());
    assert_eq!(
        python_era_launcher(),
        std::fs::read_to_string(sandbox.bin.join("dunkingsheep")).expect("launcher")
    );
    assert!(
        sandbox.release("rel-u").join("dunkingsheep").is_file(),
        "history kept"
    );
    let mut purge = uninstall_args.to_vec();
    purge.push("--purge");
    ok(&run_script("uninstall-release.sh", &purge));
    assert!(
        !sandbox.release("rel-u").exists(),
        "purge removes the releases"
    );
}

#[test]
fn rollback_restores_a_consistent_drop_in_and_launcher_pair() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.bin.join("dunkingsheep"), python_era_launcher()).expect("launcher");
    ok(&sandbox.install("rel-a"));
    ok(&sandbox.install("rel-b"));
    assert_pair(&sandbox, "rel-b");

    // The plain rollback restores the pair from before rel-b: rel-a in both files.
    ok(&sandbox.rollback(&[]));
    assert_pair(&sandbox, "rel-a");
    // The same command again undoes it.
    ok(&sandbox.rollback(&[]));
    assert_pair(&sandbox, "rel-b");

    // --original restores the pre-Rust state: no drop-in and the python-era launcher.
    ok(&sandbox.rollback(&["--original"]));
    assert!(!sandbox.dropin().exists());
    assert_eq!(
        python_era_launcher(),
        std::fs::read_to_string(sandbox.bin.join("dunkingsheep")).expect("launcher")
    );

    ok(&sandbox.rollback(&["--to", "rel-b"]));
    assert_pair(&sandbox, "rel-b");
}

/// The drop-in names `id`, and the launcher execs that same release binary.
fn assert_pair(sandbox: &Sandbox, id: &str) {
    let dropin = std::fs::read_to_string(sandbox.dropin()).expect("drop-in");
    assert!(dropin.contains(&format!("# release_id={id}\n")), "{dropin}");
    assert_eq!(
        launcher_body(&sandbox.release(id).join("dunkingsheep")),
        std::fs::read_to_string(sandbox.bin.join("dunkingsheep")).expect("launcher"),
        "the launcher does not match the drop-in for {id}"
    );
}

#[test]
fn rollback_without_a_prior_state_removes_both_generated_files() {
    let sandbox = Sandbox::new();
    ok(&sandbox.install("rel-only"));
    assert!(sandbox.dropin().is_file());
    assert!(sandbox.bin.join("dunkingsheep").is_file());
    ok(&sandbox.rollback(&[]));
    assert!(
        !sandbox.dropin().exists(),
        "ExecStart returns to the base unit"
    );
    assert!(
        !sandbox.bin.join("dunkingsheep").exists(),
        "the generated launcher is removed"
    );
    assert!(
        !sandbox.bin.join("dunk-poll").exists(),
        "the generated poll wrapper is removed"
    );
}

#[test]
fn uninstall_restores_a_pre_existing_operator_drop_in() {
    let sandbox = Sandbox::new();
    let operator = "[Service]\nEnvironment=OPERATOR=1\n";
    std::fs::create_dir_all(sandbox.dropin().parent().expect("drop-in directory"))
        .expect("drop-in directory");
    std::fs::write(sandbox.dropin(), operator).expect("operator drop-in");
    ok(&sandbox.install("rel-op"));
    assert!(std::fs::read_to_string(sandbox.dropin())
        .expect("drop-in")
        .contains("# release_id=rel-op\n"));
    ok(&sandbox.uninstall(&[]));
    assert_eq!(
        operator,
        std::fs::read_to_string(sandbox.dropin()).expect("restored operator drop-in")
    );
}

#[test]
fn uninstall_refuses_to_remove_a_drop_in_it_did_not_write() {
    let sandbox = Sandbox::new();
    ok(&sandbox.install("rel-own"));
    let unowned = "[Service]\nEnvironment=SOMEONE_ELSE=1\n";
    std::fs::write(sandbox.dropin(), unowned).expect("unowned drop-in");
    let refused = sandbox.uninstall(&[]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("not written by install-release.sh"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        unowned,
        std::fs::read_to_string(sandbox.dropin()).expect("unowned drop-in kept")
    );
}

#[test]
fn native_poll_install_does_not_require_python_in_path() {
    let sandbox = Sandbox::new();
    ok(&sandbox.install("rel-py"));
    let cli = sandbox.release("rel-py").join("dunkingsheep");
    let output = Command::new(&cli)
        .args([
            "install",
            "--bin-dir",
            sandbox.bin.to_str().expect("utf-8 path"),
        ])
        .current_dir(&sandbox.root)
        .env("PATH", "")
        .env("HOME", &sandbox.root)
        .output()
        .expect("run install with a relative PATH entry");
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn reinstall_of_the_same_release_refuses_an_operator_modified_launcher() {
    let sandbox = Sandbox::new();
    ok(&sandbox.install("rel-drift"));
    let operator = "#!/bin/sh\necho operator launcher\n";
    std::fs::write(sandbox.bin.join("dunkingsheep"), operator).expect("operator launcher");
    let refused = sandbox.install("rel-drift");
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("drift"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        operator,
        std::fs::read_to_string(sandbox.bin.join("dunkingsheep")).expect("launcher kept")
    );
}

#[test]
fn uninstall_refuses_an_operator_launcher_that_replaced_ours() {
    let sandbox = Sandbox::new();
    ok(&sandbox.install("rel-replaced"));
    let operator = "#!/bin/sh\necho operator launcher\n";
    std::fs::write(sandbox.bin.join("dunkingsheep"), operator).expect("operator launcher");
    let refused = sandbox.uninstall(&[]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("drift"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        operator,
        std::fs::read_to_string(sandbox.bin.join("dunkingsheep")).expect("launcher kept")
    );
    assert!(sandbox.dropin().is_file(), "nothing was changed");
}

#[test]
fn rollback_refuses_a_launcher_edited_after_the_last_install() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.bin.join("dunkingsheep"), python_era_launcher()).expect("launcher");
    ok(&sandbox.install("rel-r1"));
    ok(&sandbox.install("rel-r2"));
    let operator = "#!/bin/sh\necho operator launcher\n";
    std::fs::write(sandbox.bin.join("dunkingsheep"), operator).expect("operator launcher");
    let refused = sandbox.rollback(&[]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("drift"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        operator,
        std::fs::read_to_string(sandbox.bin.join("dunkingsheep")).expect("launcher kept")
    );
    assert!(std::fs::read_to_string(sandbox.dropin())
        .expect("drop-in kept")
        .contains("# release_id=rel-r2\n"));
}

#[test]
fn uninstall_refuses_a_drop_in_edited_after_the_last_install() {
    let sandbox = Sandbox::new();
    ok(&sandbox.install("rel-d2"));
    let edited = "# release_id=rel-d2\n[Service]\nEnvironment=OPERATOR=1\n";
    std::fs::write(sandbox.dropin(), edited).expect("edited drop-in");
    let refused = sandbox.uninstall(&[]);
    assert!(!refused.status.success());
    assert!(
        String::from_utf8_lossy(&refused.stderr).contains("drift"),
        "{}",
        String::from_utf8_lossy(&refused.stderr)
    );
    assert_eq!(
        edited,
        std::fs::read_to_string(sandbox.dropin()).expect("edited drop-in kept")
    );
}

#[test]
fn install_that_fails_after_the_first_launcher_write_restores_the_pair() {
    let sandbox = Sandbox::new();
    std::fs::write(sandbox.bin.join("dunkingsheep"), python_era_launcher()).expect("launcher");
    // A read-only poll wrapper makes the second launcher write fail, after the first succeeded.
    let poll_wrapper =
        "#!/bin/sh\n# operator poll wrapper (fixture)\nexec /usr/bin/python3 old-poll\n";
    let poll = sandbox.bin.join("dunk-poll");
    std::fs::write(&poll, poll_wrapper).expect("poll wrapper");
    std::fs::set_permissions(&poll, std::fs::Permissions::from_mode(0o444)).expect("read-only");

    let failed = sandbox.install("rel-fail");
    assert!(
        !failed.status.success(),
        "the second launcher write must fail"
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("cannot write") && stderr.contains("dunk-poll"),
        "the failure must come from the second launcher write: {stderr}"
    );
    assert!(
        stderr.contains("restored to their previous state"),
        "the EXIT handler must restore the pair: {stderr}"
    );
    assert_eq!(
        python_era_launcher(),
        std::fs::read_to_string(sandbox.bin.join("dunkingsheep")).expect("launcher restored")
    );
    assert_eq!(
        poll_wrapper,
        std::fs::read_to_string(&poll).expect("poll wrapper restored")
    );
    assert!(!sandbox.dropin().exists(), "the drop-in was never written");
    assert!(
        !sandbox.prefix.join("rollback/owned").exists(),
        "no owned pair is recorded for a failed install"
    );

    // Recovery: once the poll wrapper is writable, the same install succeeds.
    std::fs::set_permissions(&poll, std::fs::Permissions::from_mode(0o644)).expect("writable");
    ok(&sandbox.install("rel-fail"));
    assert_pair(&sandbox, "rel-fail");
}

#[test]
fn rollback_to_a_release_that_fails_mid_write_keeps_the_pair_and_previous() {
    let sandbox = Sandbox::new();
    ok(&sandbox.install("rel-a"));
    ok(&sandbox.install("rel-b"));
    assert_pair(&sandbox, "rel-b");
    let previous = sandbox.prefix.join("rollback/previous/dropin.conf");
    let previous_before = std::fs::read_to_string(&previous).expect("previous pair");
    assert!(previous_before.contains("# release_id=rel-a\n"));
    let poll = sandbox.bin.join("dunk-poll");
    std::fs::set_permissions(&poll, std::fs::Permissions::from_mode(0o444)).expect("read-only");

    let failed = sandbox.rollback(&["--to", "rel-a"]);
    assert!(
        !failed.status.success(),
        "the second launcher write must fail"
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("cannot write") && stderr.contains("dunk-poll"),
        "the failure must come from the second launcher write: {stderr}"
    );
    assert!(
        stderr.contains("restored to their previous state"),
        "the EXIT handler must restore the pair: {stderr}"
    );
    assert_pair(&sandbox, "rel-b");
    assert_eq!(
        previous_before,
        std::fs::read_to_string(&previous).expect("previous kept")
    );

    // Recovery: the plain rollback still restores the pair before rel-b.
    ok(&sandbox.rollback(&[]));
    assert_pair(&sandbox, "rel-a");
}

#[test]
fn install_refuses_when_a_later_drop_in_sets_execstart() {
    let sandbox = Sandbox::new();
    let dropins = sandbox.unit.join("dunkingsheep.service.d");
    std::fs::create_dir_all(&dropins).expect("drop-in dir");
    std::fs::write(
        dropins.join("zz-operator.conf"),
        "[Service]\nExecStart=/bin/false\n",
    )
    .expect("later drop-in");

    let failed = sandbox.install("rel-a");
    assert!(
        !failed.status.success(),
        "a later drop-in with ExecStart must block the install"
    );
    let stderr = String::from_utf8_lossy(&failed.stderr);
    assert!(
        stderr.contains("zz-operator.conf") && stderr.contains("sorts after 99-rust-release.conf"),
        "the refusal must name the later drop-in: {stderr}"
    );
    assert!(
        !sandbox.dropin().exists(),
        "nothing is written after a refusal"
    );
    assert!(
        !sandbox.bin.join("dunkingsheep").exists(),
        "no launcher is written after a refusal"
    );
}

#[test]
fn install_is_not_blocked_by_an_earlier_drop_in_that_sets_execstart() {
    let sandbox = Sandbox::new();
    let dropins = sandbox.unit.join("dunkingsheep.service.d");
    std::fs::create_dir_all(&dropins).expect("drop-in dir");
    std::fs::write(
        dropins.join("96-review-status-reconcile.conf"),
        "[Service]\nExecStart=\nExecStart=/usr/bin/python3 /old/dunkingsheep serve\n",
    )
    .expect("earlier drop-in");

    ok(&sandbox.install("rel-a"));
    assert_pair(&sandbox, "rel-a");
}

#[test]
fn install_refuses_a_later_drop_in_whose_execstart_has_spaces_or_indent() {
    let variants = [
        "[Service]\nExecStart = /usr/bin/python3 /old/dunkingsheep serve\n",
        "[Service]\n  ExecStart=/usr/bin/python3 /old/dunkingsheep serve\n",
        "[Service]\nExecStart\t=\t/usr/bin/python3 /old/dunkingsheep serve\n",
        "[Service]\r\nExecStart = /usr/bin/python3 /old/dunkingsheep serve\r\n",
    ];
    for body in variants {
        let sandbox = Sandbox::new();
        let dropins = sandbox.unit.join("dunkingsheep.service.d");
        std::fs::create_dir_all(&dropins).expect("drop-in dir");
        std::fs::write(dropins.join("zz-operator.conf"), body).expect("later drop-in");

        let failed = sandbox.install("rel-a");
        assert!(
            !failed.status.success(),
            "a later drop-in that sets ExecStart must block the install: {body:?}"
        );
        assert!(
            !sandbox.dropin().exists(),
            "nothing is written after a refusal: {body:?}"
        );
    }
}

#[test]
fn install_refuses_a_later_symlinked_drop_in_that_sets_execstart() {
    let sandbox = Sandbox::new();
    let dropins = sandbox.unit.join("dunkingsheep.service.d");
    std::fs::create_dir_all(&dropins).expect("drop-in dir");
    let target = sandbox.bin.join("zz-target.conf");
    std::fs::write(
        &target,
        "[Service]\nExecStart=/usr/bin/python3 /old/dunkingsheep serve\n",
    )
    .expect("symlink target");
    std::os::unix::fs::symlink(&target, dropins.join("zz-link.conf")).expect("symlink");

    let failed = sandbox.install("rel-a");
    assert!(
        !failed.status.success(),
        "a symlinked later drop-in that sets ExecStart must block the install"
    );
    assert!(
        !sandbox.dropin().exists(),
        "nothing is written after a refusal"
    );
}

#[test]
fn install_accepts_a_later_drop_in_that_only_names_execstart_in_comments_or_other_keys() {
    let sandbox = Sandbox::new();
    let dropins = sandbox.unit.join("dunkingsheep.service.d");
    std::fs::create_dir_all(&dropins).expect("drop-in dir");
    std::fs::write(
        dropins.join("zz-notes.conf"),
        "[Service]\n# ExecStart=/bin/false\n; ExecStart=/bin/false\nExecStartPre=/bin/true\nEnvironment=ExecStart=/bin/false\n",
    )
    .expect("later drop-in");

    ok(&sandbox.install("rel-a"));
    assert_pair(&sandbox, "rel-a");
}

#[test]
fn a_failed_install_leaves_the_rollback_history_as_it_was() {
    let sandbox = Sandbox::new();
    ok(&sandbox.install("rel-a"));
    assert!(sandbox
        .prefix
        .join("rollback/previous/dropin.absent")
        .exists());

    // A read-only entry inside owned makes the owned snapshot fail after previous has already been
    // replaced for rel-b.
    let locked = sandbox.prefix.join("rollback/owned/locked");
    std::fs::create_dir_all(&locked).expect("locked entry");
    std::fs::write(locked.join("keep"), "x").expect("entry");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555)).expect("read-only");

    let failed = sandbox.install("rel-b");
    assert!(!failed.status.success(), "the owned snapshot must fail");
    assert_pair(&sandbox, "rel-a");
    assert!(
        sandbox
            .prefix
            .join("rollback/previous/dropin.absent")
            .exists(),
        "previous must still be the pre-rel-a state, not rel-a"
    );

    // The replaced owned copy may still hold the read-only entry. Make it writable so the sandbox
    // can be removed, then roll back.
    for entry in std::fs::read_dir(sandbox.prefix.join("rollback")).expect("rollback dir") {
        let stale = entry.expect("entry").path().join("locked");
        if stale.exists() {
            std::fs::set_permissions(&stale, std::fs::Permissions::from_mode(0o755))
                .expect("writable");
        }
    }
    ok(&sandbox.rollback(&[]));
    assert!(
        !sandbox.dropin().exists(),
        "a plain rollback must restore the pre-rel-a state, not rel-a"
    );
}

#[test]
fn install_refuses_a_later_drop_in_in_an_extra_scan_root() {
    let sandbox = Sandbox::new();
    let extra = sandbox.prefix.join("extra-unit-path");
    std::fs::create_dir_all(extra.join("dunkingsheep.service.d")).expect("extra root");
    std::fs::write(
        extra.join("dunkingsheep.service.d/zz-override.conf"),
        "[Service]\nExecStart=/usr/bin/python3 /old/dunkingsheep serve\n",
    )
    .expect("override");

    let failed = sandbox.install_with(
        "rel-a",
        &["--scan-dir", extra.to_str().expect("utf-8 path")],
    );
    assert!(
        !failed.status.success(),
        "an override in an extra scan root must block the install"
    );
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("zz-override.conf"),
        "the refusal must name the override"
    );
    assert!(
        !sandbox.dropin().exists(),
        "nothing is written after a refusal"
    );
}

#[test]
fn install_refuses_a_later_type_wide_drop_in_that_sets_execstart() {
    let sandbox = Sandbox::new();
    let service_d = sandbox.unit.join("service.d");
    std::fs::create_dir_all(&service_d).expect("type-wide drop-in dir");
    std::fs::write(
        service_d.join("zz-all-services.conf"),
        "[Service]\nExecStart=/usr/bin/python3 /old/dunkingsheep serve\n",
    )
    .expect("type-wide drop-in");

    let failed = sandbox.install("rel-a");
    assert!(
        !failed.status.success(),
        "a later type-wide drop-in that sets ExecStart must block the install"
    );
    assert!(
        !sandbox.dropin().exists(),
        "nothing is written after a refusal"
    );
}

#[test]
fn install_refuses_a_same_named_drop_in_elsewhere_that_sets_execstart() {
    let sandbox = Sandbox::new();
    let extra = sandbox.prefix.join("shadow-root");
    std::fs::create_dir_all(extra.join("dunkingsheep.service.d")).expect("extra root");
    std::fs::write(
        extra.join("dunkingsheep.service.d/99-rust-release.conf"),
        "[Service]\nExecStart=/usr/bin/python3 /old/dunkingsheep serve\n",
    )
    .expect("shadow drop-in");

    let failed = sandbox.install_with(
        "rel-a",
        &["--scan-dir", extra.to_str().expect("utf-8 path")],
    );
    assert!(
        !failed.status.success(),
        "a same-named drop-in that sets ExecStart elsewhere must block the install"
    );
    assert!(
        !sandbox.dropin().exists(),
        "nothing is written after a refusal"
    );
}

#[test]
fn install_refuses_a_same_named_drop_in_elsewhere_without_execstart() {
    let sandbox = Sandbox::new();
    let extra = sandbox.prefix.join("shadow-root");
    std::fs::create_dir_all(extra.join("dunkingsheep.service.d")).expect("extra root");
    std::fs::write(
        extra.join("dunkingsheep.service.d/99-rust-release.conf"),
        "[Service]\nEnvironment=FOO=1\n",
    )
    .expect("same-named drop-in");

    let failed = sandbox.install_with(
        "rel-a",
        &["--scan-dir", extra.to_str().expect("utf-8 path")],
    );
    assert!(
        !failed.status.success(),
        "a same-named drop-in replaces ours, so it must block the install even without ExecStart"
    );
    assert!(
        !sandbox.dropin().exists(),
        "nothing is written after a refusal"
    );
}

#[test]
fn install_refuses_a_symlinked_launcher_without_writing_through() {
    let sandbox = Sandbox::new();
    let target = sandbox.root.join("launcher-target");
    std::fs::write(&target, "old target\n").expect("link target");
    std::os::unix::fs::symlink(&target, sandbox.bin.join("dunkingsheep")).expect("launcher link");

    let failed = sandbox.install("rel-a");
    assert!(
        !failed.status.success(),
        "a symlinked launcher must block the install"
    );
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("symbolic link"),
        "the refusal must say why"
    );
    assert!(
        std::fs::symlink_metadata(sandbox.bin.join("dunkingsheep"))
            .expect("launcher path")
            .file_type()
            .is_symlink(),
        "the launcher path must still be the link"
    );
    assert_eq!(
        "old target\n",
        std::fs::read_to_string(&target).expect("link target"),
        "the file the link points at must not change"
    );
    assert!(
        !sandbox.dropin().exists(),
        "nothing is written after a refusal"
    );
}

#[test]
fn no_standard_scan_is_refused_outside_the_test_fixtures() {
    let sandbox = Sandbox::new();
    let failed = sandbox.install_with_env(
        "rel-a",
        &[],
        &[("DUNKINGSHEEP_RELEASE_TEST_FIXTURES", "0")],
        false,
    );
    assert!(
        !failed.status.success(),
        "--no-standard-scan must be refused outside the fixtures"
    );
    assert!(
        String::from_utf8_lossy(&failed.stderr).contains("for the test fixtures only"),
        "the refusal must say the flag is for the test fixtures"
    );
    assert!(
        !sandbox.dropin().exists(),
        "nothing is written after a refusal"
    );
}

#[test]
fn standard_scan_refuses_a_later_drop_in_in_every_xdg_load_path() {
    let candidates = [
        "runtime/systemd/user.control",
        "runtime/systemd/transient",
        "runtime/systemd/generator.early",
        "runtime/systemd/user",
        "runtime/systemd/generator",
        "runtime/systemd/generator.late",
        "config/systemd/user.control",
        "config/systemd/user",
        "xdg-config-a/systemd/user",
        "xdg-config-b/systemd/user",
        "data/systemd/user",
        "xdg-data-a/systemd/user",
        "xdg-data-b/systemd/user",
    ];
    for candidate in candidates {
        let sandbox = Sandbox::new();
        let root = sandbox.root.clone();
        let dir = root.join(candidate).join("dunkingsheep.service.d");
        std::fs::create_dir_all(&dir).expect("load path");
        std::fs::write(
            dir.join("zz-override.conf"),
            "[Service]\nExecStart=/usr/bin/python3 /old/dunkingsheep serve\n",
        )
        .expect("override");

        let home = root.display().to_string();
        let runtime = root.join("runtime").display().to_string();
        let config_home = root.join("config").display().to_string();
        let data_home = root.join("data").display().to_string();
        let config_dirs = format!("{home}/xdg-config-a:{home}/xdg-config-b");
        let data_dirs = format!("{home}/xdg-data-a:{home}/xdg-data-b");
        let env = [
            ("HOME", home.as_str()),
            ("XDG_RUNTIME_DIR", runtime.as_str()),
            ("XDG_CONFIG_HOME", config_home.as_str()),
            ("XDG_CONFIG_DIRS", config_dirs.as_str()),
            ("XDG_DATA_HOME", data_home.as_str()),
            ("XDG_DATA_DIRS", data_dirs.as_str()),
        ];
        let failed = sandbox.install_with_env("rel-a", &[], &env, true);
        assert!(
            !failed.status.success(),
            "a later ExecStart drop-in under {candidate} must block the install"
        );
        assert!(
            String::from_utf8_lossy(&failed.stderr).contains("zz-override.conf"),
            "the refusal under {candidate} must name the override"
        );
        assert!(
            !sandbox.dropin().exists(),
            "nothing is written under {candidate}"
        );
    }
}

/// Kills the probe daemon if the test fails before it shuts the daemon down.
struct CleanupChild(Option<Child>);

impl Drop for CleanupChild {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// A CLI command pointed only at the probe's copied state.
fn isolated(cli: &Path, root: &Path, config: &Path, socket: &Path, tmux: &Path) -> Command {
    let mut command = Command::new(cli);
    command
        .env("HOME", root)
        .env("DUNKINGSHEEP_DIR", config)
        .env("DUNKINGSHEEP_SOCKET", socket)
        .env("DUNKINGSHEEP_TMUX_BIN", tmux)
        .env("DUNKINGSHEEP_BIND_IDENTITY", "0")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE");
    command
}

#[cfg(unix)]
#[test]
fn installed_release_serves_status_and_task_list_against_copied_state() {
    let sandbox = Sandbox::new();
    ok(&sandbox.install("rel-probe"));
    let cli = sandbox.release("rel-probe").join("dunkingsheep");

    // A copied state for the probe: a temporary config, a fixture socket and state file.
    let state = sandbox.root.join("state");
    let config = state.join("config");
    std::fs::create_dir_all(&config).expect("copied config");
    std::fs::write(
        config.join("roles.json"),
        r#"{"default":"anonymous","lanes":{"%99":{"role":"owner"}}}"#,
    )
    .expect("roles fixture");
    let socket = state.join("runtime.sock");
    let state_file = state.join("dunks.json");
    let tmux = state.join("tmux-fixture");
    std::fs::write(
        &tmux,
        // A separate process: it cannot share this test's token, so it reads the separator
        // from the -F format it is given, as tmux expands it.
        "#!/bin/sh\nfmt=\"\"; prev=\"\"; for a in \"$@\"; do if [ \"$prev\" = \"-F\" ]; then fmt=\"$a\"; fi; prev=\"$a\"; done\nsep=$(printf '%s' \"$fmt\" | sed -e 's/^#{[a-z_]*}//' -e 's/#{.*$//')\nprintf \"%%99${sep}\\$1${sep}@1${sep}/tmp${sep}bash${sep}fixture${sep}1${sep}fixture${sep}fixture${sep}0${sep}0${sep}1${sep}80${sep}24\\n\"\n",
    )
    .expect("tmux fixture");
    std::fs::set_permissions(&tmux, std::fs::Permissions::from_mode(0o755)).expect("chmod");

    let serve_log = std::fs::File::create(state.join("serve.log")).expect("serve log");
    let mut daemon = CleanupChild(Some(
        isolated(&cli, &sandbox.root, &config, &socket, &tmux)
            .args(["serve", "--state-file"])
            .arg(&state_file)
            .arg("--no-log-file")
            .stdout(Stdio::from(serve_log.try_clone().expect("clone serve log")))
            .stderr(Stdio::from(serve_log))
            .spawn()
            .expect("spawn installed release serve"),
    ));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if socket.exists() && UnixStream::connect(&socket).is_ok() {
            break;
        }
        if let Some(status) = daemon
            .0
            .as_mut()
            .expect("daemon guard")
            .try_wait()
            .expect("daemon status")
        {
            panic!("installed serve exited before binding the socket: {status}");
        }
        if Instant::now() >= deadline {
            let log = std::fs::read_to_string(state.join("serve.log")).unwrap_or_default();
            panic!("installed serve did not bind the socket; serve.log:\n{log}");
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    for args in [&["status"][..], &["task", "list"][..]] {
        let output = isolated(&cli, &sandbox.root, &config, &socket, &tmux)
            .args(args)
            .output()
            .expect("run installed CLI");
        assert_eq!(
            Some(0),
            output.status.code(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!output.stdout.is_empty(), "{args:?} printed nothing");
    }

    let shutdown = isolated(&cli, &sandbox.root, &config, &socket, &tmux)
        .args(["--self-pane", "%99", "shutdown"])
        .output()
        .expect("shutdown through the installed CLI");
    assert_eq!(Some(0), shutdown.status.code(), "{shutdown:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = daemon
            .0
            .as_mut()
            .expect("daemon guard")
            .try_wait()
            .expect("daemon exit")
        {
            assert!(status.success(), "{status}");
            break;
        }
        assert!(
            Instant::now() < deadline,
            "installed serve did not exit after shutdown"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    daemon.0 = None;
}
