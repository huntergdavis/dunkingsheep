use super::*;
use std::os::unix::fs::PermissionsExt;

struct Fixture {
    _dir: tempfile::TempDir,
    root: PathBuf,
    digest: String,
    host: Fake,
}
#[derive(Clone)]
struct State {
    unit: Unit,
    status: UnitStatus,
    candidate: bool,
}
struct Fake {
    units: BTreeMap<String, State>,
    token: PathBuf,
    calls: Vec<String>,
    now: i64,
    bad_web_once: bool,
    fail_old_tg_start: bool,
    terminal: bool,
    terminal_after_daemon: bool,
}
fn put(path: &Path, body: &[u8], mode: u32) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, body).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
impl Fixture {
    fn new() -> Self {
        let dir = crate::testsupport::dir("release-packet");
        let root = dir.path().join("packet");
        fs::create_dir_all(root.join("state")).unwrap();
        let base = dir.path();
        let cli = base.join("bin/dunkingsheep");
        put(&cli, b"old-cli", 0o755);
        put(&root.join("launcher.before"), b"old-cli", 0o755);
        put(&root.join("launcher.pending"), b"new-cli", 0o755);
        let poll = base.join("bin/dunk-poll");
        put(&poll, b"poll-unchanged", 0o755);
        let token = base.join("config/telegram-bridge-token");
        put(&token, b"private-fixture", 0o600);
        let database = token.parent().unwrap().join("inbox.db");
        seed_database(&database);
        let mut units = serde_json::Map::new();
        let mut rows = Vec::new();
        for (index, key) in ORDER.iter().enumerate() {
            let (service, binary) = match *key {
                "daemon" => ("dunkingsheep.service", "dunkingsheep"),
                "web" => ("dunkingsheep-web.service", "ds-desk"),
                _ => ("dunkingsheep-telegram.service", "dunkingsheep-telegram"),
            };
            let old = base.join(format!("old-{key}/{binary}"));
            let new = base.join(format!("new-{key}/{binary}"));
            let staged = root.join(format!("{key}/{binary}"));
            put(&old, format!("old-{key}").as_bytes(), 0o555);
            put(&new, format!("new-{key}").as_bytes(), 0o555);
            put(&staged, format!("new-{key}").as_bytes(), 0o555);
            let config = base.join(format!("units/{service}.d/10-existing.conf"));
            let drop = config.parent().unwrap().join("99-reviewed-release.conf");
            put(&config, b"old configuration", 0o644);
            put(
                &root.join(format!("{key}.conf.pending")),
                format!("[Service]\nExecStart=\nExecStart={}\n", new.display()).as_bytes(),
                0o444,
            );
            units.insert((*key).into(), json!({"unit":service,"binary":binary,"release":new.parent().unwrap(),"drop":drop,"snippet":format!("{key}.conf.pending"),"baseline":{"MainPID":(11+index).to_string(),"NRestarts":"0","executable":old,"binary_sha256":hash(&old).unwrap(),"DropInPaths":config.to_str().unwrap(),"unit_file_hashes":{config.to_str().unwrap():hash(&config).unwrap()}}}));
            rows.push(json!({"unit":key,"source":new,"stage":staged.strip_prefix(&root).unwrap(),"release":new,"sha256":hash(&new).unwrap()}));
            if *key == "web" {
                put(
                    &old.parent().unwrap().join("ui/dist/index.html"),
                    b"OLD UI",
                    0o444,
                );
                let page = new.parent().unwrap().join("ui/dist/index.html");
                let copy = root.join("web/ui/dist/index.html");
                put(&page, b"NEW UI", 0o444);
                put(&copy, b"NEW UI", 0o444);
                rows.push(json!({"unit":"web","source":page,"stage":"web/ui/dist/index.html","release":page,"sha256":hash(&page).unwrap()}));
            }
        }
        put(
            &root.join("rollback-triggers.sql"),
            b"reviewed trigger-list metadata",
            0o444,
        );
        let manifest = json!({"source_commit":"1111111111111111111111111111111111111111","operator":"operator","units":units,"rows":rows,"launcher":{"path":cli,"old_sha256":hash(&cli).unwrap(),"new_sha256":hash(&root.join("launcher.pending")).unwrap()},"poll_unchanged":{poll.to_str().unwrap():hash(&poll).unwrap()},"token_file":token,"rollback_triggers":["message_threads_delete","message_thread_routes_delete","message_thread_refs_delete"]});
        put(
            &root.join("STAGING-MANIFEST.json"),
            &serde_json::to_vec_pretty(&manifest).unwrap(),
            0o444,
        );
        fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
            for p in fs::read_dir(dir).unwrap() {
                let p = p.unwrap().path();
                if p.is_dir() {
                    if p.file_name().unwrap() != "state" {
                        walk(&p, out);
                    }
                } else {
                    out.push(p);
                }
            }
        }
        let mut paths = Vec::new();
        walk(&root, &mut paths);
        paths.sort();
        let sums = paths
            .iter()
            .map(|p| {
                format!(
                    "{}  {}\n",
                    hash(p).unwrap(),
                    p.strip_prefix(&root).unwrap().display()
                )
            })
            .collect::<String>();
        put(&root.join("SHA256SUMS"), sums.as_bytes(), 0o444);
        let digest = hash(&root.join("SHA256SUMS")).unwrap();
        put(&root.join("state/ACTIVATION-GO.json"), &serde_json::to_vec(&json!({"authority_message":"fixture owner GO","packet_sha256":digest,"valid_from":"2026-11-14T18:00:00Z","valid_until":"2026-11-14T20:00:00Z","no_terminal_confirmed":true,"rollback_on_apply_failure":true,"rollback_triggers":["apply failure"],"observations_only":["owner workflow"]})).unwrap(),0o600);
        put(&root.join("state/REVIEW-ACCEPTANCE.json"), &serde_json::to_vec(&json!({"packet_sha256":digest,"accepted":true,"reviewer":"independent","evidence_message":"fixture review"})).unwrap(),0o600);
        let packet = Packet::open(&root, &digest).unwrap();
        let states = packet
            .m
            .units
            .values()
            .map(|u| {
                (
                    u.unit.clone(),
                    State {
                        unit: u.clone(),
                        status: UnitStatus {
                            pid: u.baseline.pid.parse().unwrap(),
                            active: true,
                            restarts: 0,
                        },
                        candidate: false,
                    },
                )
            })
            .collect();
        let host = Fake {
            units: states,
            token,
            calls: Vec::new(),
            now: DateTime::parse_from_rfc3339("2026-11-14T19:00:00Z")
                .unwrap()
                .timestamp_millis(),
            bad_web_once: false,
            fail_old_tg_start: false,
            terminal: false,
            terminal_after_daemon: false,
        };
        Self {
            _dir: dir,
            root,
            digest,
            host,
        }
    }
    fn run(&mut self, action: &str, apply: bool) -> Result<String> {
        execute_with_host(&self.root, &self.digest, action, apply, &mut self.host)
    }
    fn old_active(&self) {
        assert!(self
            .host
            .units
            .values()
            .all(|s| s.status.active && !s.candidate));
    }
}
impl Host for Fake {
    fn run(&mut self, program: &Path, args: &[&str]) -> Result<String> {
        if program == Path::new("systemctl") {
            fail(!args.contains(&""), "empty systemctl argument")?;
            let action = args[1];
            self.calls
                .push(format!("{action} {}", args.get(2).unwrap_or(&"")));
            if action == "daemon-reload" {
                return Ok(String::new());
            }
            let s = self.units.get_mut(args[2]).ok_or("unknown private unit")?;
            if action == "show" {
                let p = if s.unit.drop.exists() {
                    s.unit.release.join(&s.unit.binary)
                } else {
                    s.unit.baseline.executable.clone()
                };
                return Ok(format!("path={} ", p.display()));
            }
            match action {
                "stop" => {
                    s.status.pid = 0;
                    s.status.active = false;
                }
                "restart" | "start" => {
                    s.candidate = s.unit.drop.exists();
                    if !s.candidate
                        && args[2] == "dunkingsheep-telegram.service"
                        && self.fail_old_tg_start
                    {
                        self.fail_old_tg_start = false;
                        return Err("injected old bridge start failure after unlink".into());
                    }
                    s.status.pid = s.unit.baseline.pid.parse::<u32>().unwrap()
                        + if s.candidate { 100 } else { 200 };
                    s.status.active = true;
                }
                _ => return Err("unexpected private systemctl operation".into()),
            }
            return Ok(String::new());
        }
        fail(!args.contains(&"issue"), "no fixture token issuance")?;
        if args.contains(&"list") {
            return Ok("telegram-bridge system\n".into());
        }
        Ok("{}".into())
    }
    fn status(&mut self, unit: &str) -> Result<UnitStatus> {
        Ok(self.units.get(unit).ok_or("unknown unit")?.status.clone())
    }
    fn image(&mut self, pid: u32) -> Result<(PathBuf, String)> {
        let s = self
            .units
            .values()
            .find(|s| s.status.pid == pid && pid > 0)
            .ok_or("no such private process")?;
        let p = if s.candidate {
            s.unit.release.join(&s.unit.binary)
        } else {
            s.unit.baseline.executable.clone()
        };
        Ok((p.clone(), hash(&p)?))
    }
    fn children(&mut self, _: u32) -> Result<Vec<(Vec<String>, PathBuf)>> {
        let attached = self.terminal
            || self.terminal_after_daemon && self.units["dunkingsheep.service"].candidate;
        if attached {
            Ok(vec![(
                vec!["tmux".into(), "-C".into(), "attach-session".into()],
                "/usr/bin/tmux".into(),
            )])
        } else {
            Ok(vec![(
                vec!["usage-provider".into(), "opencode".into()],
                "/private/usage-native".into(),
            )])
        }
    }
    fn token_path(&mut self, _: u32) -> Result<Option<PathBuf>> {
        Ok(Some(self.token.clone()))
    }
    fn http(&mut self, _: &str, host: &str, path: &str) -> Result<(u16, Vec<u8>)> {
        if path == "/api/terminals" {
            return Ok((
                if host.starts_with("evil.") { 403 } else { 401 },
                Vec::new(),
            ));
        }
        let s = &self.units["dunkingsheep-web.service"];
        if s.candidate && self.bad_web_once {
            self.bad_web_once = false;
            return Ok((500, Vec::new()));
        }
        let base = if s.candidate {
            s.unit.release.clone()
        } else {
            s.unit.baseline.executable.parent().unwrap().to_path_buf()
        };
        Ok((
            200,
            io(fs::read(base.join("ui/dist").join(if path == "/" {
                "index.html"
            } else {
                path.trim_start_matches('/')
            })))?,
        ))
    }
    fn now_millis(&self) -> i64 {
        self.now
    }
    fn pause(&mut self) {}
}
fn seed_database(path: &Path) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let db = Connection::open(path).unwrap();
    db.execute_batch("CREATE TABLE messages(id INTEGER);INSERT INTO messages VALUES(1);CREATE TABLE sidecar(id INTEGER);INSERT INTO sidecar VALUES(1);").unwrap();
    for name in [
        "message_threads_delete",
        "message_thread_routes_delete",
        "message_thread_refs_delete",
    ] {
        db.execute_batch(&format!("CREATE TRIGGER {name} AFTER DELETE ON messages BEGIN DELETE FROM sidecar WHERE id=OLD.id; END;")).unwrap();
    }
}

#[test]
fn every_dry_run_and_plan_apply_leave_prefix_unchanged() {
    let mut f = Fixture::new();
    let cli = f.host.units["dunkingsheep.service"]
        .unit
        .baseline
        .executable
        .clone();
    let before = hash(&cli).unwrap();
    for action in ["plan", "activate", "verify", "rollback"] {
        assert!(f.run(action, false).unwrap().contains("dry_run"));
    }
    f.run("plan", true).unwrap();
    assert!(f.host.calls.is_empty());
    assert!(!f.root.join("state/operation.lock").exists());
    assert!(!f.root.join("state/ACTIVATION-START.json").exists());
    assert_eq!(hash(&cli).unwrap(), before);
}
#[test]
fn three_units_verify_and_repeated_rollback_restore_old_without_duplicate_dropins() {
    let mut f = Fixture::new();
    f.run("activate", true).unwrap();
    f.run("verify", true).unwrap();
    for s in f.host.units.values() {
        assert!(s.candidate);
        assert!(s.unit.drop.exists());
    }
    f.run("rollback", true).unwrap();
    f.old_active();
    let snapshot = f.root.join("state/backup/inbox-before-trigger-drop.db");
    let before = (
        hash(&snapshot).unwrap(),
        fs::metadata(&snapshot).unwrap().ino(),
    );
    f.host.now += 7_200_000;
    f.run("rollback", true).unwrap();
    f.old_active();
    assert_eq!(
        before,
        (
            hash(&snapshot).unwrap(),
            fs::metadata(&snapshot).unwrap().ino()
        )
    );
    assert!(f.host.units.values().all(|s| !s.unit.drop.exists()));
}
#[test]
fn failed_postcheck_automatically_rolls_back_and_does_not_allow_second_apply() {
    let mut f = Fixture::new();
    f.host.bad_web_once = true;
    assert!(f
        .run("activate", true)
        .unwrap_err()
        .contains("automatic rollback: PASS"));
    f.old_active();
    let restarts = f
        .host
        .calls
        .iter()
        .filter(|s| s.starts_with("restart"))
        .count();
    assert!(f.run("activate", true).is_err());
    assert_eq!(
        f.host
            .calls
            .iter()
            .filter(|s| s.starts_with("restart"))
            .count(),
        restarts
    );
}
#[test]
fn rollback_resumes_old_bridge_after_overlay_unlink_and_failed_start() {
    let mut f = Fixture::new();
    f.run("activate", true).unwrap();
    f.host.fail_old_tg_start = true;
    assert!(f.run("rollback", true).is_err());
    assert!(!f.host.units["dunkingsheep-telegram.service"]
        .unit
        .drop
        .exists());
    assert!(!f.host.units["dunkingsheep-telegram.service"].status.active);
    f.host.now += 7_200_000;
    f.run("rollback", true).unwrap();
    f.old_active();
}
#[test]
fn attached_terminal_before_desk_step_rolls_back_only_changed_daemon() {
    let mut f = Fixture::new();
    f.host.terminal_after_daemon = true;
    assert!(f
        .run("activate", true)
        .unwrap_err()
        .contains("automatic rollback: PASS"));
    f.old_active();
    assert_eq!(f.host.units["dunkingsheep-web.service"].status.pid, 12);
    assert_eq!(f.host.units["dunkingsheep-telegram.service"].status.pid, 13);
}
#[test]
fn sqlite_snapshot_keeps_first_bytes_and_rows_when_downgrade_is_repeated() {
    let dir = crate::testsupport::dir("release-snapshot");
    let db = dir.path().join("inbox.db");
    seed_database(&db);
    let snapshot = dir.path().join("snapshot.db");
    let triggers = [
        "message_threads_delete",
        "message_thread_routes_delete",
        "message_thread_refs_delete",
    ]
    .map(String::from);
    rollback_database(&db, &snapshot, &triggers).unwrap();
    let first = (
        hash(&snapshot).unwrap(),
        fs::metadata(&snapshot).unwrap().ino(),
    );
    Connection::open(&db)
        .unwrap()
        .execute("INSERT INTO messages VALUES(2)", [])
        .unwrap();
    rollback_database(&db, &snapshot, &triggers).unwrap();
    assert_eq!(
        first,
        (
            hash(&snapshot).unwrap(),
            fs::metadata(&snapshot).unwrap().ino()
        )
    );
    let saved = Connection::open(snapshot).unwrap();
    assert_eq!(
        saved
            .query_row("SELECT COUNT(*) FROM messages", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        Connection::open(db)
            .unwrap()
            .query_row("SELECT COUNT(*) FROM sidecar", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        1
    );
}
#[test]
fn stale_or_self_accepted_authority_cannot_start_a_switch() {
    for change in ["expired", "self-review", "wrong-digest"] {
        let mut f = Fixture::new();
        let review = f.root.join("state/REVIEW-ACCEPTANCE.json");
        let go = f.root.join("state/ACTIVATION-GO.json");
        match change {
            "expired" => f.host.now += 7_200_000,
            "self-review" => {
                let mut v: Value = load(&review).unwrap();
                v["reviewer"] = json!("operator");
                put(&review, &serde_json::to_vec(&v).unwrap(), 0o600);
            }
            _ => {
                let mut v: Value = load(&go).unwrap();
                v["packet_sha256"] = json!("wrong");
                put(&go, &serde_json::to_vec(&v).unwrap(), 0o600);
            }
        }
        assert!(f.run("activate", true).is_err());
        assert!(!f.root.join("state/ACTIVATION-START.json").exists());
        assert!(!f.host.calls.iter().any(|s| s.starts_with("restart")));
    }
}
#[test]
fn terminal_guard_distinguishes_control_clients_from_usage_children() {
    assert!(terminal_child(
        &["tmux".into(), "-C".into(), "attach-session".into()],
        Path::new("/usr/bin/tmux")
    ));
    assert!(terminal_child(
        &[
            "python3".into(),
            "/guard/tmux".into(),
            "-C".into(),
            "attach-session".into()
        ],
        Path::new("/usr/bin/python3")
    ));
    assert!(!terminal_child(
        &["usage-provider".into(), "opencode".into()],
        Path::new("/private/dunk-usage-native")
    ));
}
