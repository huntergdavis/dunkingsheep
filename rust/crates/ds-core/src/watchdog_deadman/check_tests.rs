use super::*;
use std::time::{Duration, SystemTime};

struct Fixture {
    dir: tempfile::TempDir,
    heartbeat: PathBuf,
    state: PathBuf,
    status: PathBuf,
    inbox: PathBuf,
    socket: String,
}
impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        Self {
            heartbeat: dir.path().join("heartbeat"),
            state: dir.path().join("state.json"),
            status: dir.path().join("status.json"),
            inbox: dir.path().join("inbox.db"),
            socket: dir.path().join("daemon.sock").to_string_lossy().into(),
            dir,
        }
    }
    fn options(&self, now: f64) -> CheckOptions<'_> {
        CheckOptions {
            heartbeat: &self.heartbeat,
            cadence_minutes: json!(10),
            multiplier: json!(3),
            target: "fake-vp",
            state_path: &self.state,
            cooldown_minutes: 60.0,
            now,
            dry_run: false,
            error_file: None,
            status_path: Some(&self.status),
            inbox_path: Some(&self.inbox),
            daemon_path: Some(&self.socket),
            expected_dir: Some(self.dir.path()),
        }
    }
    fn beat(&self, at: f64, body: &str) {
        fs::write(&self.heartbeat, body).unwrap();
        fs::File::open(&self.heartbeat)
            .unwrap()
            .set_times(
                fs::FileTimes::new()
                    .set_modified(SystemTime::UNIX_EPOCH + Duration::from_secs_f64(at)),
            )
            .unwrap();
    }
    fn rows(&self) -> i64 {
        if !self.inbox.exists() {
            return 0;
        }
        rusqlite::Connection::open(&self.inbox)
            .unwrap()
            .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
            .unwrap()
    }
}
fn down(_: &str, _: &str, _: &str) -> Result<(), CheckSendError> {
    Err(io::Error::new(io::ErrorKind::ConnectionRefused, "refused").into())
}
const NOW: f64 = 1_700_000_000.0;
#[test]
fn test_1_daemon_down_writes_one_verified_direct_row() {
    for kind in [io::ErrorKind::ConnectionRefused, io::ErrorKind::NotFound] {
        let f = Fixture::new();
        let report = check(&f.options(NOW), |_, _, _| {
            Err(io::Error::new(kind, "absent").into())
        })
        .unwrap();
        assert_eq!(report["alerted"], true);
        assert_eq!(report["delivered_via"], "direct");
        assert_eq!(f.rows(), 1);
        let inbox =
            crate::inbox_store::Store::open(&f.inbox, f.dir.path().join("attachments")).unwrap();
        let row = inbox
            .get_message(report["direct_message_id"].as_str().unwrap())
            .unwrap();
        assert_eq!(row.summary.sender.as_deref(), Some("watchdog"));
        assert_eq!(row.summary.mailbox, "fake-vp");
        assert!(row.body.contains("watchdog.deadman"));
    }
}
#[test]
fn test_3_daemon_up_but_failing_never_writes_directly() {
    let f = Fixture::new();
    let report = check(&f.options(NOW), |_, _, _| {
        Err(crate::client::ClientError::Dunk(refuse("unknown mailbox")).into())
    })
    .unwrap();
    assert_eq!(report["alerted"], false);
    assert_eq!(report["cause"], "other");
    assert_eq!(f.rows(), 0);
    assert!(!f.state.exists());
    assert_eq!(load(&f.status).unwrap()["attempted"], "daemon");
}
#[test]
fn test_5_retry_after_a_failed_direct_write_makes_exactly_one_row() {
    let mut f = Fixture::new();
    let parent = f.dir.path().join("late-dir");
    fs::write(&parent, "blocker").unwrap();
    f.inbox = parent.join("inbox.db");
    let first = check(&f.options(NOW), down).unwrap();
    assert_eq!(first["alerted"], false);
    let native = python_direct_error(&f.inbox);
    let expected = format!("no daemon (ConnectionRefusedError: refused); direct write failed: {}: {}", native["name"].as_str().unwrap(), native["message"].as_str().unwrap());
    assert_eq!(first["delivery_error"], expected);
    assert_eq!(load(&f.status).unwrap()["error"], expected);
    assert!(!f.state.exists());
    assert!(!f.inbox.exists());
    fs::remove_file(&parent).unwrap();
    let second = check(&f.options(NOW + 60.0), down).unwrap();
    assert_eq!(second["alerted"], true);
    assert_eq!(load(&f.state).unwrap()["last_alerted"], NOW + 60.0);
    let third = check(&f.options(NOW + 3661.0), down).unwrap();
    assert_eq!(third["alerted"], true);
    assert!(third.get("direct_message_id").is_none());
    assert_eq!(f.rows(), 1);
    // A different heartbeat incident may insert another row.
    f.beat(NOW - 4000.0, "");
    check(&f.options(NOW + 8000.0), down).unwrap();
    assert_eq!(f.rows(), 2);
}
#[test]
fn fresh_boundary_cooldown_dry_run_and_raw_config_error() {
    let f = Fixture::new();
    f.beat(NOW - 1800.0, "");
    assert_eq!(
        check(&f.options(NOW), |_, _, _| panic!("fresh")).unwrap()["stale"],
        false
    );
    let error_path = f.dir.path().join("error");
    fs::write(&error_path, "  CONFIG: bad key 🐑\n").unwrap();
    let mut options = f.options(NOW + 1.0);
    options.error_file = Some(&error_path);
    let report = check(&options, |to, subject, body| {
        assert_eq!(to, "fake-vp");
        assert_eq!(subject, "watchdog.deadman: watchdog heartbeat stale");
        assert!(body.contains("CONFIG: bad key 🐑"));
        Ok(())
    })
    .unwrap();
    assert_eq!(report["delivered_via"], "daemon");
    assert_eq!(
        check(&f.options(NOW + 2.0), |_, _, _| panic!("cooldown")).unwrap()["reason"],
        "cooldown"
    );
    let fresh = Fixture::new();
    let mut options = fresh.options(NOW);
    options.dry_run = true;
    options.daemon_path = None;
    assert_eq!(
        check(&options, |_, _, _| panic!("dry")).unwrap()["reason"],
        "would alert (dry run)"
    );
    assert!(!fresh.state.exists());
    assert!(!fresh.status.exists());
    assert_eq!(fresh.rows(), 0);
}
#[test]
fn slow_daemon_config_bad_and_status_failures_do_not_mark_delivered() {
    for kind in [
        io::ErrorKind::TimedOut,
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::NotADirectory,
    ] {
        let f = Fixture::new();
        let report = check(&f.options(NOW), |_, _, _| {
            Err(io::Error::new(kind, "failed").into())
        })
        .unwrap();
        assert_eq!(report["alerted"], false);
        assert_eq!(f.rows(), 0);
        assert!(!f.state.exists());
    }
    let f = Fixture::new();
    let mut options = f.options(NOW);
    options.daemon_path = None;
    assert_eq!(
        check(&options, |_, _, _| panic!("bad path")).unwrap()["cause"],
        "config_bad"
    );
    let mut options = f.options(NOW);
    options.status_path = Some(f.dir.path());
    assert!(check(&options, |_, _, _| Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "slow"
    )
    .into()))
    .is_err());
    assert!(!f.state.exists());
    // Sender success followed by persistence failure must propagate, never return alerted=true.
    let fresh = Fixture::new();
    let mut called = false;
    assert!(check(&fresh.options(NOW), |_, _, _| {
        called = true;
        fs::create_dir(&fresh.state).unwrap();
        Ok(())
    })
    .is_err());
    assert!(called);
}
#[test]
fn recorded_cadence_widens_never_narrows_and_clamps() {
    for (recorded, age, limit, clamped, stale) in [
        (15, 31.0, 45.0, false, false),
        (15, 46.0, 45.0, false, true),
        (5, 29.0, 30.0, false, false),
        (1440, 100.0, 180.0, true, false),
        (1440, 181.0, 180.0, true, true),
    ] {
        let f = Fixture::new();
        f.beat(
            NOW - age * 60.0,
            &json!({"cadence_minutes":recorded}).to_string(),
        );
        let mut sends = 0;
        let report = check(&f.options(NOW), |_, _, _| {
            sends += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(report["limit_minutes"], limit);
        assert_eq!(report["clamped"], clamped);
        assert_eq!(report["stale"], stale);
        assert_eq!(sends, usize::from(stale));
    }
    let f = Fixture::new();
    f.beat(NOW - 1900.0, "bad JSON");
    let report = check(&f.options(NOW), |_, _, body| {
        assert!(body.contains("using the unit cadence 10 min."));
        Ok(())
    })
    .unwrap();
    assert!(report["heartbeat_problem"].is_string());
    assert_eq!(report["limit_minutes"], 30.0);
}

#[test]
fn primitive_filesystem_error_context_matches_actual_python_oracle() {
    let f = Fixture::new();
    let parent = f.dir.path().join("blocked ' parent");
    fs::write(&parent, "blocker").unwrap();
    let path = parent.join("inbox.db");
    let rust = match direct_write_context(&path, "fake-vp", "stale", "body") {
        Ok(_) => panic!("blocked parent must fail"),
        Err(error) => json!({"name":error.name,"message":error.message}),
    };
    let python = python_direct_error(&path);
    assert_eq!(rust, python);
    assert!(!path.exists());
}

#[test]
fn native_daemon_error_keeps_python_errno_message() {
    let error: CheckSendError = io::Error::from_raw_os_error(111).into();
    assert_eq!(error.name, "ConnectionRefusedError");
    assert_eq!(error.message, "[Errno 111] Connection refused");
    assert_eq!(error.io_error.unwrap().raw_os_error(), Some(111));
}

fn python_direct_error(path: &Path) -> Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
    let output = std::process::Command::new("python3").args(["-c", "import json,sys; sys.path.insert(0,sys.argv[1]); import dunk_watchdog_deadman as d\ntry: d.direct_write(sys.argv[2], 'fake-vp', 'stale', 'body')\nexcept Exception as e: print(json.dumps({'name':type(e).__name__,'message':str(e)}))"]).arg(root).arg(path).output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let python: Value = serde_json::from_slice(&output.stdout).unwrap();
    python
}

#[test]
fn primitive_sqlite_and_readback_errors_match_actual_python_oracle() {
    for scenario in ["corrupt", "directory", "constraint", "readback"] {
        let f = Fixture::new();
        match scenario {
            "corrupt" => fs::write(&f.inbox, "not a database").unwrap(),
            "directory" => fs::create_dir(&f.inbox).unwrap(),
            _ => {
                drop(
                    crate::inbox_store::Store::open(&f.inbox, f.dir.path().join("attachments"))
                        .unwrap(),
                );
                let connection = rusqlite::Connection::open(&f.inbox).unwrap();
                let sql = if scenario == "constraint" {
                    "CREATE TRIGGER reject BEFORE INSERT ON messages BEGIN SELECT RAISE(ABORT, 'fixture rejection'); END;"
                } else {
                    "CREATE TRIGGER corrupt AFTER INSERT ON messages BEGIN UPDATE messages SET body='corrupt' WHERE id=NEW.id; END;"
                };
                connection.execute_batch(sql).unwrap();
            }
        }
        let rust = match direct_write_context(&f.inbox, "fake-vp", "stale", "body") {
            Ok(_) => panic!("{scenario} must fail"),
            Err(error) => json!({"name":error.name,"message":error.message}),
        };
        // Restore the first ID for Python's independent actual write/readback attempt.
        if scenario == "readback" {
            let connection = rusqlite::Connection::open(&f.inbox).unwrap();
            connection
                .execute_batch(
                    "DELETE FROM messages; DELETE FROM counters WHERE name='message';",
                )
                .unwrap();
        }
        assert_eq!(rust, python_direct_error(&f.inbox), "{scenario}");
    }
}

#[test]
fn sqlite_context_is_scoped_and_restored_after_success_or_panic() {
    use crate::db::{map_sqlite, with_sqlite_error_context};
    fn failure() -> DunkError {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        map_sqlite(
            connection
                .execute_batch("SELECT * FROM missing_table")
                .unwrap_err(),
        )
    }
    let (_, captured) = with_sqlite_error_context::<()>(|| Err(failure())).unwrap_err();
    assert!(captured.is_some());
    with_sqlite_error_context(|| {
        let _ = failure();
        Ok(())
    })
    .unwrap();
    let (_, captured) =
        with_sqlite_error_context::<()>(|| Err(DunkError::Refused("plain".into()))).unwrap_err();
    assert!(captured.is_none());
    let (_, outer) = with_sqlite_error_context::<()>(|| {
        let error = failure();
        let (_, inner) =
            with_sqlite_error_context::<()>(|| Err(DunkError::Refused("inner".into())))
                .unwrap_err();
        assert!(inner.is_none());
        let _ = std::panic::catch_unwind(|| with_sqlite_error_context::<()>(|| panic!("fixture")));
        Err(error)
    })
    .unwrap_err();
    assert_eq!(outer.unwrap().message, "no such table: missing_table");
}
