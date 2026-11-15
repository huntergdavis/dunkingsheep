//! Operator-only reviewed release packets behind tools/rust_rollout.sh (t4699).
//! Ports the accepted t5007 operator procedure; never builds or starts timers.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use chrono::DateTime;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

type Result<T> = std::result::Result<T, String>;
const ORDER: [&str; 3] = ["daemon", "web", "telegram"];
static SERIAL: AtomicU64 = AtomicU64::new(0);

fn fail(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn io<T>(value: std::io::Result<T>) -> Result<T> {
    value.map_err(|e| e.to_string())
}
fn hash(path: &Path) -> Result<String> {
    Ok(format!("{:x}", Sha256::digest(io(fs::read(path))?)))
}
fn load<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    serde_json::from_slice(&io(fs::read(path))?).map_err(|e| format!("{}: {e}", path.display()))
}
fn plain(path: &Path) -> Result<()> {
    let s = path.to_str().ok_or("path must be UTF-8")?;
    fail(
        path.is_absolute()
            && s.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"/._-".contains(&c)),
        "path must be absolute and plain",
    )
}
fn regular(path: &Path) -> Result<()> {
    fail(
        io(fs::symlink_metadata(path))?.file_type().is_file(),
        format!("not a regular non-symlink file: {}", path.display()),
    )
}
fn private_dir(path: &Path) -> Result<()> {
    if path.exists() {
        fail(
            io(fs::symlink_metadata(path))?.is_dir(),
            "state directory is not a real directory",
        )?;
    }
    io(fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path))
}
fn temporary(path: &Path) -> PathBuf {
    path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ))
}
fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = io(OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path))?;
    io(file.write_all(bytes))?;
    io(file.sync_all())
}
fn atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let temp = temporary(path);
    let result = (|| {
        write_new(&temp, bytes, mode)?;
        io(fs::rename(&temp, path))
    })();
    if temp.exists() {
        let _ = fs::remove_file(temp);
    }
    result
}

#[derive(Clone, Deserialize)]
struct Baseline {
    #[serde(rename = "MainPID")]
    pid: String,
    #[serde(rename = "NRestarts")]
    restarts: String,
    executable: PathBuf,
    binary_sha256: String,
    unit_file_hashes: BTreeMap<PathBuf, String>,
    #[serde(rename = "DropInPaths", default)]
    dropins: String,
}
#[derive(Clone, Deserialize)]
struct Unit {
    unit: String,
    binary: String,
    release: PathBuf,
    baseline: Baseline,
    drop: PathBuf,
    snippet: PathBuf,
}
#[derive(Deserialize)]
struct Artifact {
    unit: String,
    source: PathBuf,
    stage: PathBuf,
    release: PathBuf,
    sha256: String,
}
#[derive(Deserialize)]
struct Launcher {
    path: PathBuf,
    old_sha256: String,
    new_sha256: String,
}
#[derive(Deserialize)]
struct Manifest {
    source_commit: String,
    operator: String,
    units: BTreeMap<String, Unit>,
    rows: Vec<Artifact>,
    launcher: Launcher,
    poll_unchanged: BTreeMap<PathBuf, String>,
    token_file: PathBuf,
    rollback_triggers: Vec<String>,
    #[serde(default = "default_http")]
    http_authority: String,
    #[serde(default = "default_lan")]
    lan_authority: String,
}
fn default_http() -> String {
    "127.0.0.1:8787".into()
}
fn default_lan() -> String {
    "192.168.0.209:8787".into()
}

#[derive(Clone, Debug)]
pub struct UnitStatus {
    pub pid: u32,
    pub active: bool,
    pub restarts: u64,
}
/// Runtime boundary: tests exercise real files/SQLite with private service controls.
pub trait Host {
    fn run(&mut self, program: &Path, args: &[&str]) -> Result<String>;
    fn status(&mut self, unit: &str) -> Result<UnitStatus>;
    fn image(&mut self, pid: u32) -> Result<(PathBuf, String)>;
    fn children(&mut self, pid: u32) -> Result<Vec<(Vec<String>, PathBuf)>>;
    fn token_path(&mut self, pid: u32) -> Result<Option<PathBuf>>;
    fn http(&mut self, authority: &str, host: &str, path: &str) -> Result<(u16, Vec<u8>)>;
    fn now_millis(&self) -> i64;
    fn pause(&mut self);
}

/// Recognize the desk's control attach, including wrappers and tmux client titles.
pub fn terminal_child(args: &[String], executable: &Path) -> bool {
    let name = executable.file_name().unwrap_or_default().to_string_lossy();
    let tmux = name.starts_with("tmux")
        || args.iter().take(2).any(|v| {
            matches!(
                Path::new(v).file_name().and_then(|n| n.to_str()),
                Some("tmux" | "tmux-real")
            )
        });
    tmux && (args.first().is_some_and(|v| v.starts_with("tmux: client"))
        || args.iter().any(|v| matches!(v.as_str(), "-C" | "-CC"))
            && args
                .iter()
                .any(|v| matches!(v.as_str(), "attach" | "attach-session")))
}

struct Packet {
    root: PathBuf,
    digest: String,
    m: Manifest,
}
impl Packet {
    fn open(root: &Path, digest: &str) -> Result<Self> {
        plain(root)?;
        fail(
            io(fs::symlink_metadata(root))?.is_dir(),
            "packet root is not a real directory",
        )?;
        fail(
            digest.len() == 64
                && digest
                    .bytes()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "expected full lowercase packet SHA256",
        )?;
        let root = io(root.canonicalize())?;
        fail(
            hash(&root.join("SHA256SUMS"))? == digest,
            "reviewed packet digest differs",
        )?;
        let m: Manifest = load(&root.join("STAGING-MANIFEST.json"))?;
        fail(
            m.source_commit.len() == 40 && m.source_commit.bytes().all(|c| c.is_ascii_hexdigit()),
            "full source commit required",
        )?;
        fail(
            m.units.len() == 3 && ORDER.iter().all(|u| m.units.contains_key(*u)),
            "packet requires exactly daemon, web and telegram",
        )?;
        for (key, unit) in &m.units {
            let (service, binary) = match key.as_str() {
                "daemon" => ("dunkingsheep.service", "dunkingsheep"),
                "web" => ("dunkingsheep-web.service", "ds-desk"),
                _ => ("dunkingsheep-telegram.service", "dunkingsheep-telegram"),
            };
            fail(
                unit.unit == service && unit.binary == binary,
                "unexpected unit or binary",
            )?;
            for p in [&unit.release, &unit.drop, &unit.baseline.executable] {
                plain(p)?;
            }
            fail(
                unit.drop
                    .parent()
                    .and_then(Path::file_name)
                    .and_then(|s| s.to_str())
                    == Some(&format!("{service}.d")),
                "drop-in must belong to its unit",
            )?;
        }
        plain(&m.launcher.path)?;
        plain(&m.token_file)?;
        let packet = Self {
            root,
            digest: digest.into(),
            m,
        };
        packet.files()?;
        for key in ORDER {
            packet.candidate(key)?;
        }
        Ok(packet)
    }
    fn state(&self, name: &str) -> PathBuf {
        self.root.join("state").join(name)
    }
    fn relative(&self, name: &Path) -> Result<PathBuf> {
        fail(
            !name.as_os_str().is_empty()
                && name.components().all(|c| matches!(c, Component::Normal(_))),
            "packet path must be relative without traversal",
        )?;
        let p = self.root.join(name);
        regular(&p)?;
        fail(
            io(p.canonicalize())?.starts_with(&self.root),
            "packet file escapes root",
        )?;
        Ok(p)
    }
    fn files(&self) -> Result<()> {
        let sums = io(fs::read_to_string(self.root.join("SHA256SUMS")))?;
        let mut names = BTreeSet::new();
        for line in sums.lines() {
            let (expected, name) = line.split_once("  ").ok_or("invalid checksum record")?;
            fail(names.insert(name), "duplicate checksum record")?;
            fail(
                hash(&self.relative(Path::new(name))?)? == expected,
                format!("packet file drift: {name}"),
            )?;
        }
        fail(
            names.contains("STAGING-MANIFEST.json"),
            "manifest is not covered by checksums",
        )?;
        for name in [
            "launcher.before",
            "launcher.pending",
            "rollback-triggers.sql",
        ] {
            fail(
                names.contains(name),
                format!("operational file is not covered: {name}"),
            )?;
        }
        for u in self.m.units.values() {
            fail(
                u.snippet.to_str().is_some_and(|p| names.contains(p)),
                "unit snippet is not covered",
            )?;
        }
        for row in &self.m.rows {
            let stage = self.relative(&row.stage)?;
            for p in [&row.source, &stage, &row.release] {
                plain(p)?;
                regular(p)?;
                fail(
                    hash(p)? == row.sha256,
                    format!("artifact drift: {}", p.display()),
                )?;
            }
        }
        for (p, expected) in &self.m.poll_unchanged {
            regular(p)?;
            fail(hash(p)? == *expected, "poll drift")?;
        }
        for unit in self.m.units.values() {
            regular(&unit.baseline.executable)?;
            fail(
                hash(&unit.baseline.executable)? == unit.baseline.binary_sha256,
                "old rollback artifact differs",
            )?;
        }
        Ok(())
    }
    fn candidate(&self, key: &str) -> Result<&Artifact> {
        let u = &self.m.units[key];
        self.m
            .rows
            .iter()
            .find(|r| r.unit == key && r.release == u.release.join(&u.binary))
            .ok_or_else(|| format!("candidate binary missing: {key}"))
    }
    fn ctl(&self, host: &mut impl Host, action: &str, unit: &str) -> Result<String> {
        if unit.is_empty() {
            host.run(Path::new("systemctl"), &["--user", action])
        } else {
            host.run(Path::new("systemctl"), &["--user", action, unit])
        }
    }
    fn note(&self, name: &str, value: &Value, exclusive: bool) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|e| e.to_string())?;
        if exclusive {
            write_new(&self.state(name), &bytes, 0o600)
        } else {
            atomic(&self.state(name), &bytes, 0o600)
        }
    }
    fn go(&self, host: &impl Host, recovery: bool) -> Result<Value> {
        let go: Value = if recovery {
            load::<Value>(&self.state("ACTIVATION-START.json"))?["go"].clone()
        } else {
            load(&self.state("ACTIVATION-GO.json"))?
        };
        fail(go["packet_sha256"] == self.digest, "GO packet differs")?;
        fail(
            go["authority_message"]
                .as_str()
                .is_some_and(|s| !s.is_empty()),
            "GO authority missing",
        )?;
        if !recovery {
            for (key, lower) in [("valid_from", true), ("valid_until", false)] {
                let t =
                    DateTime::parse_from_rfc3339(go[key].as_str().ok_or("GO timestamp missing")?)
                        .map_err(|e| e.to_string())?
                        .timestamp_millis();
                fail(
                    if lower {
                        host.now_millis() >= t
                    } else {
                        host.now_millis() <= t
                    },
                    "outside GO window",
                )?;
            }
        }
        fail(
            go["no_terminal_confirmed"] == true,
            "owner no-terminal confirmation required",
        )?;
        fail(
            go["rollback_triggers"]
                .as_array()
                .is_some_and(|v| !v.is_empty()),
            "complete rollback triggers required",
        )?;
        fail(
            go["observations_only"]
                .as_array()
                .is_some_and(|v| !v.is_empty()),
            "observations-only list required",
        )?;
        let review: Value = load(&self.state("REVIEW-ACCEPTANCE.json"))?;
        fail(
            review["packet_sha256"] == self.digest,
            "review packet differs",
        )?;
        fail(review["accepted"] == true, "independent acceptance missing")?;
        fail(
            review["reviewer"]
                .as_str()
                .is_some_and(|v| !v.is_empty() && v != self.m.operator),
            "operator cannot self-accept",
        )?;
        fail(
            review["evidence_message"]
                .as_str()
                .is_some_and(|v| !v.is_empty()),
            "review evidence missing",
        )?;
        Ok(go)
    }
    fn no_terminal(&self, host: &mut impl Host) -> Result<()> {
        let pid = host.status(&self.m.units["web"].unit)?.pid;
        fail(
            !host
                .children(pid)?
                .iter()
                .any(|(a, e)| terminal_child(a, e)),
            "attached terminal: wait for detachment before switching desk",
        )
    }
    fn verify_unit(&self, host: &mut impl Host, key: &str, candidate: bool) -> Result<u32> {
        let u = &self.m.units[key];
        let status = host.status(&u.unit)?;
        fail(
            status.active && status.pid > 0,
            format!("inactive unit: {key}"),
        )?;
        fail(
            status.restarts.to_string() == u.baseline.restarts,
            format!("automatic restart: {key}"),
        )?;
        let (path, digest) = host.image(status.pid)?;
        let (expected, sha) = if candidate {
            let row = self.candidate(key)?;
            (&row.release, &row.sha256)
        } else {
            (&u.baseline.executable, &u.baseline.binary_sha256)
        };
        fail(
            &path == expected && &digest == sha,
            format!("wrong executable: {key}"),
        )?;
        Ok(status.pid)
    }
    fn credentials(&self, host: &mut impl Host) -> Result<()> {
        regular(&self.m.token_file)?;
        let meta = io(fs::metadata(&self.m.token_file))?;
        fail(
            meta.uid() == nix::unistd::geteuid().as_raw(),
            "bridge token owner differs",
        )?;
        fail(meta.mode() & 0o777 == 0o600, "bridge token must be 0600")?;
        let labels = host.run(
            &self.m.launcher.path,
            &["--no-start-daemon", "token", "list"],
        )?;
        fail(
            labels.lines().any(|line| {
                line.split_whitespace().collect::<Vec<_>>() == ["telegram-bridge", "system"]
            }),
            "dedicated bridge token registration missing",
        )
    }
    fn web(&self, host: &mut impl Host, candidate: bool) -> Result<()> {
        let (status, body) = host.http(&self.m.http_authority, &self.m.http_authority, "/")?;
        fail(status == 200, "desk root is not 200")?;
        let u = &self.m.units["web"];
        let base = if candidate {
            u.release.as_path()
        } else {
            u.baseline
                .executable
                .parent()
                .ok_or("old web directory missing")?
        };
        fail(
            format!("{:x}", Sha256::digest(body)) == hash(&base.join("ui/dist/index.html"))?,
            "served index differs",
        )?;
        fail(
            host.http(
                &self.m.http_authority,
                &self.m.lan_authority,
                "/api/terminals",
            )?
            .0 == 401,
            "terminal auth gate differs",
        )?;
        fail(
            host.http(
                &self.m.http_authority,
                "evil.example:8787",
                "/api/terminals",
            )?
            .0 == 403,
            "terminal Host gate differs",
        )?;
        if candidate {
            for row in &self.m.rows {
                if row.unit != "web" {
                    continue;
                }
                let Some((_, suffix)) =
                    row.release.to_str().and_then(|p| p.split_once("/ui/dist/"))
                else {
                    continue;
                };
                let url = if suffix == "index.html" {
                    "/".to_string()
                } else {
                    format!("/{suffix}")
                };
                let (status, body) =
                    host.http(&self.m.http_authority, &self.m.http_authority, &url)?;
                fail(status == 200, "served asset unavailable")?;
                fail(
                    format!("{:x}", Sha256::digest(body)) == row.sha256,
                    "served asset hash differs",
                )?;
            }
        }
        Ok(())
    }
    fn preflight(&self, host: &mut impl Host) -> Result<()> {
        regular(&self.m.launcher.path)?;
        self.credentials(host)?;
        fail(
            hash(&self.m.launcher.path)? == self.m.launcher.old_sha256,
            "CLI baseline differs",
        )?;
        for u in self.m.units.values() {
            let actual = host.status(&u.unit)?;
            fail(
                actual.pid.to_string() == u.baseline.pid,
                "baseline PID differs",
            )?;
            self.verify_unit(host, self.key(&u.unit)?, false)?;
            fail(!u.drop.exists(), "candidate overlay already exists")?;
            self.baseline_files(u)?;
            for path in u.baseline.dropins.split_whitespace() {
                fail(
                    u.drop.file_name() > Path::new(path).file_name(),
                    "candidate overlay would not be last; legacy migration is separate",
                )?;
            }
        }
        self.web(host, false)?;
        self.poll(host)
    }
    fn key(&self, name: &str) -> Result<&str> {
        self.m
            .units
            .iter()
            .find_map(|(k, u)| (u.unit == name).then_some(k.as_str()))
            .ok_or_else(|| "unknown unit".into())
    }
    fn baseline_files(&self, u: &Unit) -> Result<()> {
        for (path, expected) in &u.baseline.unit_file_hashes {
            regular(path)?;
            fail(
                hash(path)? == *expected,
                "baseline unit configuration drift",
            )?;
        }
        Ok(())
    }
    fn poll(&self, host: &mut impl Host) -> Result<()> {
        for (path, expected) in &self.m.poll_unchanged {
            fail(hash(path)? == *expected, "poll changed")?;
        }
        let program = self
            .m
            .launcher
            .path
            .parent()
            .ok_or("CLI directory missing")?
            .join("dunk-poll");
        host.run(&program, &[])?;
        host.run(&program, &["--all"])?;
        Ok(())
    }
    fn check(&self, host: &mut impl Host, key: &str) -> Result<()> {
        let pid = self.verify_unit(host, key, true)?;
        match key {
            "daemon" => {
                fail(
                    hash(&self.m.launcher.path)? == self.m.launcher.new_sha256,
                    "new CLI differs",
                )?;
                host.run(&self.m.launcher.path, &["--version"])?;
                host.run(
                    &self.m.launcher.path,
                    &["--no-start-daemon", "inbox", "read", "--peek"],
                )?;
                host.run(
                    &self.m.launcher.path,
                    &["--no-start-daemon", "task", "backlog"],
                )?;
            }
            "web" => self.web(host, true)?,
            _ => fail(
                host.token_path(pid)?.as_ref() == Some(&self.m.token_file),
                "live bridge token path differs",
            )?,
        }
        self.poll(host)
    }
    fn backup(&self) -> Result<()> {
        let root = self.state("backup");
        io(fs::DirBuilder::new().mode(0o700).create(&root))?;
        write_new(
            &root.join("dunkingsheep"),
            &io(fs::read(&self.m.launcher.path))?,
            0o700,
        )?;
        for (key, u) in &self.m.units {
            let dest = root.join(key);
            io(fs::DirBuilder::new().mode(0o700).create(&dest))?;
            for path in u.baseline.unit_file_hashes.keys() {
                write_new(
                    &dest.join(path.file_name().ok_or("unit filename missing")?),
                    &io(fs::read(path))?,
                    0o600,
                )?;
            }
        }
        Ok(())
    }
    fn wrapper(&self, candidate: bool) -> Result<()> {
        let (name, expected) = if candidate {
            ("launcher.pending", &self.m.launcher.new_sha256)
        } else {
            ("launcher.before", &self.m.launcher.old_sha256)
        };
        let source = self.relative(Path::new(name))?;
        fail(hash(&source)? == *expected, "saved CLI differs")?;
        atomic(&self.m.launcher.path, &io(fs::read(source))?, 0o755)
    }
    fn activate(&self, host: &mut impl Host) -> Result<()> {
        self.preflight(host)?;
        let go = self.go(host, false)?;
        fail(
            go["rollback_on_apply_failure"] == true,
            "GO must explicitly authorize rollback_on_apply_failure",
        )?;
        self.no_terminal(host)?;
        self.note(
            "ACTIVATION-START.json",
            &json!({"go":go,"attempt":1,"at_millis":host.now_millis()}),
            true,
        )?;
        let result = (|| {
            self.backup()?;
            for key in ORDER {
                let u = &self.m.units[key];
                if key == "web" {
                    self.no_terminal(host)?;
                }
                let source = self.relative(&u.snippet)?;
                write_new(&u.drop, &io(fs::read(source))?, 0o644)?;
                self.ctl(host, "daemon-reload", "")?;
                let effective = host.run(
                    Path::new("systemctl"),
                    &["--user", "show", &u.unit, "-p", "ExecStart", "--value"],
                )?;
                fail(
                    effective.contains(&format!("path={} ", u.release.join(&u.binary).display())),
                    "effective candidate ExecStart differs",
                )?;
                self.ctl(host, "restart", &u.unit)?;
                host.pause();
                if key == "daemon" {
                    self.wrapper(true)?;
                }
                self.check(host, key)?;
                for untouched in ORDER.iter().skip_while(|p| **p != key).skip(1) {
                    fail(
                        host.status(&self.m.units[*untouched].unit)?.pid.to_string()
                            == self.m.units[*untouched].baseline.pid,
                        "untouched unit PID changed",
                    )?;
                }
                self.note(
                    &format!("{key}-ACTIVATED.json"),
                    &json!({"at_millis":host.now_millis(),"pid":host.status(&u.unit)?.pid}),
                    false,
                )?;
            }
            self.note(
                "ACTIVATION-RESULT.json",
                &json!({"exit_code":0,"at_millis":host.now_millis()}),
                false,
            )
        })();
        if let Err(error) = result {
            let recovery = self.rollback(host);
            return Err(format!(
                "activation failed: {error}; automatic rollback: {}",
                recovery
                    .map(|_| "PASS".to_string())
                    .unwrap_or_else(|e| format!(
                        "INCOMPLETE: {e}; wait/detach as needed, then rerun rollback"
                    ))
            ));
        }
        Ok(())
    }
    fn rollback(&self, host: &mut impl Host) -> Result<()> {
        self.files()?;
        self.go(host, true)?;
        let cli = hash(&self.m.launcher.path)?;
        fail(
            cli == self.m.launcher.old_sha256 || cli == self.m.launcher.new_sha256,
            "CLI drift before rollback",
        )?;
        for key in ORDER.iter().rev() {
            let u = &self.m.units[*key];
            self.baseline_files(u)?;
            if !u.drop.exists() {
                let status = host.status(&u.unit)?;
                if !status.active || status.pid == 0 {
                    self.baseline_files(u)?;
                    self.ctl(host, "daemon-reload", "")?;
                    let effective = host.run(
                        Path::new("systemctl"),
                        &["--user", "show", &u.unit, "-p", "ExecStart", "--value"],
                    )?;
                    fail(
                        effective.contains(&format!("path={} ", u.baseline.executable.display())),
                        "effective old ExecStart differs",
                    )?;
                    self.ctl(host, "start", &u.unit)?;
                    host.pause();
                }
                self.verify_unit(host, key, false)?;
                continue;
            }
            regular(&u.drop)?;
            fail(
                io(fs::read(&u.drop))? == io(fs::read(self.relative(&u.snippet)?))?,
                "overlay drift; refusing removal",
            )?;
            let pid = host.status(&u.unit)?.pid;
            if pid != 0 {
                let (_, sha) = host.image(pid)?;
                fail(
                    sha == u.baseline.binary_sha256 || sha == self.candidate(key)?.sha256,
                    "unexpected executable; refusing stop",
                )?;
            }
            if *key == "web" {
                self.no_terminal(host)?;
            }
            self.ctl(host, "stop", &u.unit)?;
            if *key == "daemon" {
                fail(
                    host.status(&u.unit)?.pid == 0,
                    "daemon still running before schema downgrade",
                )?;
                let database = self
                    .m
                    .token_file
                    .parent()
                    .ok_or("config directory missing")?
                    .join("inbox.db");
                rollback_database(
                    &database,
                    &self.state("backup/inbox-before-trigger-drop.db"),
                    &self.m.rollback_triggers,
                )?;
                self.wrapper(false)?;
            }
            io(fs::remove_file(&u.drop))?;
            self.ctl(host, "daemon-reload", "")?;
            self.ctl(host, "start", &u.unit)?;
            host.pause();
            self.verify_unit(host, key, false)?;
            self.note(
                &format!("{key}-ROLLED-BACK.json"),
                &json!({"at_millis":host.now_millis()}),
                false,
            )?;
        }
        fail(
            hash(&self.m.launcher.path)? == self.m.launcher.old_sha256,
            "old CLI not restored",
        )?;
        for key in ORDER {
            self.verify_unit(host, key, false)?;
        }
        self.web(host, false)?;
        self.poll(host)?;
        self.note(
            "ROLLBACK-RESULT.json",
            &json!({"exit_code":0,"at_millis":host.now_millis(),"tables_and_rows_retained":true}),
            false,
        )
    }
}

/// Quiesced SQLite snapshot and trigger downgrade; publication never overwrites
/// the first complete snapshot and repeated calls accept already-dropped triggers.
fn rollback_database(database: &Path, snapshot: &Path, triggers: &[String]) -> Result<()> {
    let expected = BTreeSet::from([
        "message_threads_delete",
        "message_thread_routes_delete",
        "message_thread_refs_delete",
    ]);
    fail(
        triggers.len() == 3
            && triggers.iter().map(String::as_str).collect::<BTreeSet<_>>() == expected,
        "unexpected rollback trigger list",
    )?;
    regular(database)?;
    let connection = Connection::open(database).map_err(|e| e.to_string())?;
    if !snapshot.exists() {
        let temp = temporary(snapshot);
        write_new(&temp, &[], 0o600)?;
        let copied = (|| {
            connection
                .execute(
                    "VACUUM INTO ?1",
                    [temp.to_str().ok_or("snapshot path is not UTF-8")?],
                )
                .map_err(|e| e.to_string())?;
            io(File::open(&temp))?
                .sync_all()
                .map_err(|e| e.to_string())?;
            match fs::hard_link(&temp, snapshot) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        })();
        let _ = fs::remove_file(temp);
        copied?;
    }
    regular(snapshot)?;
    let meta = io(fs::metadata(snapshot))?;
    fail(
        meta.len() > 0 && meta.mode() & 0o777 == 0o600,
        "invalid first rollback snapshot",
    )?;
    let before = schema(&connection)?;
    let sql = format!(
        "BEGIN IMMEDIATE;{}COMMIT;",
        triggers
            .iter()
            .map(|s| format!("DROP TRIGGER IF EXISTS {s};"))
            .collect::<String>()
    );
    connection.execute_batch(&sql).map_err(|e| e.to_string())?;
    let after = schema(&connection)?;
    fail(
        after
            == before
                .into_iter()
                .filter(|(name, _)| !expected.contains(name.as_str()))
                .collect(),
        "unrelated schema changed",
    )
}
fn schema(connection: &Connection) -> Result<BTreeMap<String, String>> {
    let mut query = connection
        .prepare("SELECT name,sql FROM sqlite_master WHERE sql IS NOT NULL")
        .map_err(|e| e.to_string())?;
    let rows = query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .map_err(|e| e.to_string())?;
    rows.collect::<rusqlite::Result<_>>()
        .map_err(|e| e.to_string())
}

struct SystemHost;
impl Host for SystemHost {
    fn run(&mut self, program: &Path, args: &[&str]) -> Result<String> {
        let result = io(Command::new("timeout")
            .arg("45s")
            .arg(program)
            .args(args)
            .output())?;
        fail(
            result.status.success(),
            format!("command failed: {} ({})", program.display(), result.status),
        )?;
        String::from_utf8(result.stdout).map_err(|e| e.to_string())
    }
    fn status(&mut self, unit: &str) -> Result<UnitStatus> {
        let output = self.run(
            Path::new("systemctl"),
            &[
                "--user",
                "show",
                unit,
                "-p",
                "MainPID",
                "-p",
                "ActiveState",
                "-p",
                "NRestarts",
            ],
        )?;
        let values: BTreeMap<_, _> = output.lines().filter_map(|s| s.split_once('=')).collect();
        Ok(UnitStatus {
            pid: values
                .get("MainPID")
                .ok_or("missing PID")?
                .parse()
                .map_err(|_| "invalid PID")?,
            active: values.get("ActiveState") == Some(&"active"),
            restarts: values
                .get("NRestarts")
                .ok_or("missing restarts")?
                .parse()
                .map_err(|_| "invalid restarts")?,
        })
    }
    fn image(&mut self, pid: u32) -> Result<(PathBuf, String)> {
        let p = PathBuf::from(format!("/proc/{pid}/exe"));
        Ok((io(fs::read_link(&p))?, hash(&p)?))
    }
    fn children(&mut self, pid: u32) -> Result<Vec<(Vec<String>, PathBuf)>> {
        let result = io(Command::new("pgrep")
            .args(["-P", &pid.to_string()])
            .output())?;
        fail(
            matches!(result.status.code(), Some(0 | 1)),
            "cannot inspect desk children",
        )?;
        let mut out = Vec::new();
        for id in String::from_utf8_lossy(&result.stdout).split_whitespace() {
            fail(id.bytes().all(|c| c.is_ascii_digit()), "invalid child PID")?;
            let p = PathBuf::from(format!("/proc/{id}"));
            match (fs::read(p.join("cmdline")), fs::read_link(p.join("exe"))) {
                (Ok(args), Ok(exe)) => out.push((
                    args.split(|b| *b == 0)
                        .filter(|s| !s.is_empty())
                        .map(|s| String::from_utf8_lossy(s).into_owned())
                        .collect(),
                    exe,
                )),
                (Err(e), _) | (_, Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {}
                (Err(e), _) | (_, Err(e)) => return Err(e.to_string()),
            }
        }
        Ok(out)
    }
    fn token_path(&mut self, pid: u32) -> Result<Option<PathBuf>> {
        let bytes = io(fs::read(format!("/proc/{pid}/environ")))?;
        let paths: Vec<_> = bytes
            .split(|b| *b == 0)
            .filter_map(|entry| entry.strip_prefix(b"DUNKINGSHEEP_SERVICE_TOKEN_FILE="))
            .collect();
        fail(
            paths.len() == 1,
            "bridge token file environment missing or duplicated",
        )?;
        Ok(Some(PathBuf::from(
            std::str::from_utf8(paths[0]).map_err(|_| "token path is not UTF-8")?,
        )))
    }
    fn http(&mut self, authority: &str, host: &str, path: &str) -> Result<(u16, Vec<u8>)> {
        let address: std::net::SocketAddr = authority
            .parse()
            .map_err(|_| "HTTP target must be a numeric loopback authority")?;
        fail(
            address.ip().is_loopback(),
            "HTTP checks must connect to loopback",
        )?;
        fail(path.starts_with('/'), "HTTP path must be absolute")?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        runtime.block_on(async {
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(8))
                .build()
                .map_err(|e| e.to_string())?;
            let response = client
                .get(format!("http://{authority}{path}"))
                .header(reqwest::header::HOST, host)
                .send()
                .await
                .map_err(|e| e.to_string())?;
            let status = response.status().as_u16();
            Ok((
                status,
                response.bytes().await.map_err(|e| e.to_string())?.to_vec(),
            ))
        })
    }
    fn now_millis(&self) -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
    fn pause(&mut self) {
        std::thread::sleep(Duration::from_secs(4));
    }
}

/// Dry-run does no writes (not even a lock) and invokes no host programs.
/// --apply is an operator workflow, not a privilege grant: the independently
/// reviewed GO/REVIEW files must be provisioned by the authorized coordinator.
pub fn execute(root: &Path, digest: &str, action: &str, apply: bool) -> Result<String> {
    execute_with_host(root, digest, action, apply, &mut SystemHost)
}

fn execute_with_host(
    root: &Path,
    digest: &str,
    action: &str,
    apply: bool,
    host: &mut impl Host,
) -> Result<String> {
    fail(
        matches!(action, "plan" | "activate" | "verify" | "rollback"),
        "unknown packet operation",
    )?;
    let packet = Packet::open(root, digest)?;
    if !apply || action == "plan" {
        return Ok(json!({"dry_run":true,"action":action,"source_commit":packet.m.source_commit,"order":ORDER,"packet_sha256":digest,"note":"No services, files, locks, timers or host commands changed. Operator only after exact tooling/packet review and GO."}).to_string());
    }
    private_dir(&packet.root.join("state"))?;
    let lock = packet.state("operation.lock");
    if lock.exists() {
        regular(&lock)?;
    }
    let _guard = crate::lock::Guard::try_acquire(&lock, crate::lock::Mode::Exclusive)
        .map_err(|e| e.to_string())?
        .ok_or("another packet operation is running")?;
    match action {
        "activate" => packet.activate(host)?,
        "rollback" => packet.rollback(host)?,
        _ => {
            packet.credentials(host)?;
            for key in ORDER {
                packet.check(host, key)?;
            }
        }
    }
    Ok(json!({"action":action,"exit_code":0,"packet_sha256":digest}).to_string())
}

#[cfg(test)]
mod tests;
