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
    Command::new("sh")
        .arg(project_root().join("packaging/rust-release").join(name))
        .args(args)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .expect("run release script")
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
            .join("dunkingsheep.service.d/10-rust-release.conf")
    }

    fn install(&self, id: &str) -> Output {
        let poll = project_root().join("tools/dunk_poll.py");
        run_script(
            "install-release.sh",
            &[
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
            ],
        )
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
    let poll = release.join("dunk-poll.py");
    assert_eq!(0o555, mode(&binary));
    assert_eq!(0o444, mode(&poll));

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
    let poll_path = project_root().join("tools/dunk_poll.py");
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
        "#!/bin/sh\nprintf '%%99\\037$1\\037@1\\037/tmp\\037bash\\037fixture\\0371\\037fixture\\037fixture\\0370\\0370\\0371\\03780\\03724\\n'\n",
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
