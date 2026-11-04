use super::*;
use std::cell::Cell;
use std::time::{Duration, SystemTime};

struct Fixture {
    dir: tempfile::TempDir,
    paths: Vec<String>,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let paths = [
            "heartbeat",
            "state.json",
            "error",
            "status.json",
            "inbox.db",
            "daemon.sock",
        ]
        .map(|name| dir.path().join(name).to_string_lossy().into_owned())
        .to_vec();
        Self { dir, paths }
    }
    fn args(&self) -> MainArgs<'_> {
        MainArgs {
            heartbeat: &self.paths[0],
            cadence_minutes: json!(10),
            multiplier: json!(3),
            to: "fake-vp",
            state: &self.paths[1],
            cooldown_minutes: 60.0,
            now: Some(1_700_000_000.0),
            dry_run: false,
            error_file: &self.paths[2],
            status: &self.paths[3],
            inbox: &self.paths[4],
            daemon_path: Some(&self.paths[5]),
            expected_dir: Some(self.dir.path()),
            json: true,
        }
    }
    fn beat(&self, at: f64, body: &str) {
        fs::write(&self.paths[0], body).unwrap();
        fs::File::open(&self.paths[0])
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs_f64(at)),
            )
            .unwrap();
    }
}
struct FakeDaemon<'a> {
    calls: &'a Cell<usize>,
    scenario: &'a str,
}
impl Transport for FakeDaemon<'_> {
    fn exchange(&self, line: &str, _: Duration) -> io::Result<Option<String>> {
        self.calls.set(self.calls.get() + 1);
        let wire: Value = serde_json::from_str(line).unwrap();
        assert_eq!(wire["cmd"], "inbox_send");
        assert_eq!(wire["args"]["system_sender"], "watchdog");
        assert_eq!(wire["args"]["to"], "fake-vp");
        match self.scenario {
            "timeout" => Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")),
            "down" => Err(io::Error::new(io::ErrorKind::ConnectionRefused, "refused")),
            _ => Ok(Some(
                json!({"ok":true,"result":{"message_id":"i1"}}).to_string(),
            )),
        }
    }
}
fn invoke(args: &MainArgs<'_>, scenario: &str) -> (i32, String, usize) {
    let calls = Cell::new(0);
    let client = FlockClient::new(
        FakeDaemon {
            calls: &calls,
            scenario,
        },
        None,
        "test".into(),
        Duration::from_secs(1),
        None,
    );
    let mut out = Vec::new();
    let code = main(args, client, &mut out).unwrap();
    (code, String::from_utf8(out).unwrap(), calls.get())
}
fn python_main(args: &MainArgs<'_>, scenario: &str) -> Value {
    let data = json!({"heartbeat":args.heartbeat,"cadence_minutes":args.cadence_minutes,
        "multiplier":args.multiplier,"to":args.to,"state":args.state,"cooldown_minutes":args.cooldown_minutes,
        "now":args.now,"dry_run":args.dry_run,"error_file":args.error_file,"status":args.status,
        "inbox":args.inbox,"daemon_path":args.daemon_path,"expected_dir":args.expected_dir,"json":args.json});
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            r#"
import argparse, contextlib, io, json, sys
sys.path.insert(0, sys.argv[1])
import dunk_watchdog_deadman as d
args = argparse.Namespace(**json.loads(sys.argv[2]))
calls = 0
def send(*unused):
    global calls
    calls += 1
    if sys.argv[3] == 'timeout': raise TimeoutError('timed out')
    if sys.argv[3] == 'down': raise ConnectionRefusedError('refused')
d.daemon_sender = lambda path: send
out = io.StringIO()
with contextlib.redirect_stdout(out): code = d.main(args)
print(json.dumps({'code':code, 'stdout':out.getvalue(), 'calls':calls}))
"#,
        ])
        .arg(root)
        .arg(data.to_string())
        .arg(scenario)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn test_cli_exit_codes_by_cause() {
    for (scenario, expected, attempts) in [
        ("config", 2, 0),
        ("timeout", 1, 1),
        ("success", 0, 1),
        ("down", 0, 1),
        ("dry", 0, 0),
    ] {
        let f = Fixture::new();
        let mut args = f.args();
        if scenario == "config" {
            args.daemon_path = Some("");
        }
        args.dry_run = scenario == "dry";
        let (code, stdout, calls) = invoke(&args, scenario);
        assert_eq!((code, calls), (expected, attempts), "{scenario}");
        let report: Value = serde_json::from_str(&stdout).unwrap();
        if scenario == "config" {
            assert_eq!(load(args.status).unwrap()["cause"], "config_bad");
        }
        if scenario == "timeout" {
            assert_eq!(load(args.status).unwrap()["attempted"], "daemon");
        }
        if scenario == "down" {
            assert_eq!(report["delivered_via"], "direct");
            let rows: i64 = rusqlite::Connection::open(args.inbox)
                .unwrap()
                .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
                .unwrap();
            assert_eq!(rows, 1);
        }
        // Give Python the same initial state, after inspecting Rust's real artifacts.
        for path in [args.state, args.inbox] {
            if Path::new(path).exists() {
                fs::remove_file(path).unwrap();
            }
        }
        let python = python_main(&args, scenario);
        assert_eq!(python["code"], code);
        assert_eq!(python["stdout"], stdout, "{scenario}");
        assert_eq!(python["calls"], calls);
    }
}
#[test]
fn test_cli_fresh_and_clamped_exits_zero_with_json_fields() {
    let f = Fixture::new();
    f.beat(
        1_700_000_000.0 - 100.0 * 60.0,
        r#"{"cadence_minutes":1440}"#,
    );
    let args = f.args();
    let (code, stdout, calls) = invoke(&args, "success");
    assert_eq!((code, calls), (0, 0));
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["clamped"], true);
    assert_eq!(report["recorded_cadence_minutes"], 1440);
    assert_eq!(report["alerted"], false);
    assert!(!Path::new(args.inbox).exists());
    assert_eq!(python_main(&args, "success")["stdout"], stdout);
}
#[test]
fn cli_text_matches_python_for_missing_dry_run_fresh_and_cooldown() {
    for scenario in ["missing", "dry", "fresh", "cooldown"] {
        let f = Fixture::new();
        let mut args = f.args();
        args.json = false;
        args.dry_run = scenario == "dry";
        if scenario == "fresh" {
            f.beat(args.now.unwrap() - 60.0, "");
        }
        if scenario == "cooldown" {
            save(args.state, &json!({"last_alerted":args.now.unwrap()})).unwrap();
        }
        let (code, stdout, calls) = invoke(&args, "timeout");
        let python = python_main(&args, "timeout");
        assert_eq!(python["code"], code);
        assert_eq!(python["stdout"], stdout, "{scenario}");
        assert_eq!(python["calls"], calls);
    }
}
#[test]
fn test_deadman_does_not_import_the_watchdog() {
    // The Rust equivalent guards production dependencies, not test-only oracle imports.
    for source in [
        include_str!("../watchdog_deadman.rs")
            .split("#[cfg(test)]")
            .next()
            .unwrap(),
        include_str!("check.rs"),
        include_str!("main.rs"),
    ] {
        assert!(!source.contains("crate::watchdog::"));
        assert!(!source.contains("crate::watchdog_run"));
    }
}

#[test]
fn test_watchdog_dry_run_never_touches_the_heartbeat() {
    // Source-runner oracle + actual Rust deadman. The Rust watchdog runner is a
    // separate prerequisite, so this test does not claim native watchdog coverage.
    let f = Fixture::new();
    let args = f.args();
    f.beat(args.now.unwrap() - 3600.0, "");
    let before = fs::metadata(args.heartbeat).unwrap().modified().unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let output = std::process::Command::new("python3")
        .args([
            "-c",
            r#"
import os, sys
sys.path[:0] = [sys.argv[1], os.path.join(sys.argv[1], 'tests')]
from test_dunk_watchdog import Fleet
fleet = Fleet(sys.argv[2])
config = fleet.config()
before = os.stat(config['heartbeat_path']).st_mtime_ns
_, sent = fleet.run(config, now=1700000000, dry=True)
assert not sent
assert os.stat(config['heartbeat_path']).st_mtime_ns == before
"#,
        ])
        .arg(root)
        .arg(f.dir.path())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fs::metadata(args.heartbeat).unwrap().modified().unwrap(),
        before
    );
    let (code, stdout, calls) = invoke(&args, "success");
    assert_eq!((code, calls), (0, 1));
    let report: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        report["alerted"], true,
        "a watchdog dry run is not coverage"
    );
    assert_eq!(
        fs::metadata(args.heartbeat).unwrap().modified().unwrap(),
        before
    );
}
#[test]
fn cli_expands_user_paths_and_uses_current_time_when_unspecified() {
    let f = Fixture::new();
    f.beat(crate::clock::now_s(), "");
    let mut args = f.args();
    args.now = None;
    args.heartbeat = "~/heartbeat";
    args.state = "~/state.json";
    args.error_file = "~/error";
    args.status = "~/status.json";
    args.inbox = "~/inbox.db";
    let calls = Cell::new(0);
    let client = FlockClient::new(
        FakeDaemon {
            calls: &calls,
            scenario: "success",
        },
        None,
        "test".into(),
        Duration::from_secs(1),
        None,
    );
    let mut output = Vec::new();
    let env = Env::from_pairs(&[("HOME", f.dir.path().to_str().unwrap())]);
    assert_eq!(main_with_env(&args, client, &mut output, &env).unwrap(), 0);
    let report: Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["heartbeat"], f.paths[0]);
    assert_eq!(report["stale"], false);
    assert_eq!(calls.get(), 0);
}
#[test]
fn cli_output_failure_propagates_after_accepted_send_without_retry() {
    struct BrokenOutput;
    impl Write for BrokenOutput {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(io::ErrorKind::BrokenPipe, "closed"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let f = Fixture::new();
    let args = f.args();
    let calls = Cell::new(0);
    let client = FlockClient::new(
        FakeDaemon {
            calls: &calls,
            scenario: "success",
        },
        None,
        "test".into(),
        Duration::from_secs(1),
        None,
    );
    assert!(main(&args, client, &mut BrokenOutput).is_err());
    assert_eq!(calls.get(), 1);
    assert_eq!(load(args.state).unwrap()["last_alerted"], args.now.unwrap());
}
