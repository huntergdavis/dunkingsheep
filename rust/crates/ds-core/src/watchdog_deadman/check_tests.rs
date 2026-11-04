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
