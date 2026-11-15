use super::*;
use std::os::unix::fs::symlink;
struct Fake {
    units: BTreeMap<String, (String, String)>,
    images: Vec<PathBuf>,
    calls: Vec<String>,
}
impl Host for Fake {
    fn systemd(&mut self, unit: &str) -> Result<(String, String)> {
        self.calls.push(unit.into());
        self.units
            .get(unit)
            .cloned()
            .ok_or("missing fixture unit".into())
    }
    fn images(&mut self) -> (Vec<PathBuf>, Vec<String>) {
        (self.images.clone(), vec![])
    }
}
struct Fixture {
    dir: tempfile::TempDir,
    options: Options,
    host: Fake,
    paths: BTreeMap<String, PathBuf>,
}
fn put(path: &Path, text: &str) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}
fn release(root: &Path, name: &str, unit: &str, revision: char) -> PathBuf {
    let dir = root.join(name);
    let binary = match unit {
        "web" => "ds-desk",
        "telegram" => "dunkingsheep-telegram",
        "poll" => "dunk-poll",
        _ => "dunkingsheep",
    };
    let body = format!("private binary {name}");
    put(&dir.join(binary), &body);
    put(
        &dir.join("MANIFEST"),
        &format!(
            "format=1\nunit={unit}\nrevision={}\nbinary={binary}\nbinary_sha256={}\n",
            revision.to_string().repeat(40),
            hash(body.as_bytes())
        ),
    );
    dir
}
impl Fixture {
    fn new() -> Self {
        let dir = crate::testsupport::dir("release-inventory");
        let base = dir.path();
        let root = base.join("releases");
        fs::create_dir(&root).unwrap();
        let mut paths = BTreeMap::new();
        for (name, unit, revision) in [
            ("current", "daemon", 'b'),
            ("shadow", "daemon", 'a'),
            ("rollback", "daemon", 'a'),
            ("pending", "daemon", 'd'),
            ("unused", "daemon", 'e'),
            ("client", "daemon", 'a'),
            ("poll", "poll", 'a'),
            ("web", "web", 'c'),
            ("telegram", "telegram", 'c'),
            ("old-web", "web", 'a'),
            ("source-only", "daemon", 'a'),
        ] {
            paths.insert(name.to_string(), release(&root, name, unit, revision));
        }
        let mut host = Fake {
            units: BTreeMap::new(),
            images: vec![paths["client"].join("dunkingsheep")],
            calls: vec![],
        };
        for (key, service) in UNITS {
            let fragment = base.join("units").join(service);
            let drop = base
                .join("units")
                .join(format!("{service}.d/99-current.conf"));
            let binary = if key == "web" {
                "ds-desk"
            } else if key == "telegram" {
                "dunkingsheep-telegram"
            } else {
                "dunkingsheep"
            };
            let target = paths[if key == "daemon" { "current" } else { key }].join(binary);
            let shadow = paths["shadow"].join("dunkingsheep");
            let mut chain = vec![(
                fragment.clone(),
                format!("[Service]\nExecStart={}\n", shadow.display()),
            )];
            if key == "daemon" {
                chain.push((
                    base.join("units")
                        .join(format!("{service}.d/10-shadow.conf")),
                    format!("[Service]\nExecStart=\nExecStart={}\n", shadow.display()),
                ));
            }
            chain.push((
                drop.clone(),
                format!("[Service]\nExecStart=\nExecStart={}\n", target.display()),
            ));
            let cat = chain
                .iter()
                .map(|(p, s)| format!("# {}\n{s}\n", p.display()))
                .collect::<String>();
            let drops = chain
                .iter()
                .skip(1)
                .map(|(p, _)| p.display().to_string())
                .collect::<Vec<_>>()
                .join(" ");
            let show=format!("FragmentPath={}\nDropInPaths={drops}\nExecStart={{ path={} ; argv[]={}; }}\nMainPID=42\n",fragment.display(),target.display(),target.display());
            for (path, text) in chain {
                put(&path, &text);
            }
            host.units.insert(service.into(), (show, cat));
        }
        let records = base.join("records");
        let complete = records.join("complete");
        put(&complete.join("STAGING-MANIFEST.json"),&json!({"source_commit":"b".repeat(40),"units":{"daemon":{"release":paths["current"],"baseline":{"executable":paths["rollback"].join("dunkingsheep")}},"web":{"release":paths["web"],"baseline":{"executable":paths["old-web"].join("ds-desk")}}},"rows":[{"unit":"daemon","source":base.join("deleted-build/dunkingsheep"),"stage":"deleted-stage","release":paths["current"].join("dunkingsheep")},{"unit":"daemon","source":paths["source-only"].join("dunkingsheep"),"release":paths["current"].join("dunkingsheep")}],"token_file":base.join("never-read-token")}).to_string());
        put(
            &complete.join("state/ACTIVATION-RESULT.json"),
            "{\"exit_code\":0}",
        );
        put(&records.join("pending/STAGING-MANIFEST.json"),&json!({"units":{"daemon":{"release":paths["pending"],"baseline":{"executable":paths["rollback"].join("dunkingsheep")}}}}).to_string());
        let cli = base.join("bin/cli");
        put(
            &cli,
            &format!(
                "#!/bin/sh\nexec {} \"$@\"\n",
                paths["current"].join("dunkingsheep").display()
            ),
        );
        let poll = base.join("bin/poll");
        put(
            &poll,
            &format!(
                "#!/bin/sh\nexec {} \"$@\"\n",
                paths["poll"].join("dunk-poll").display()
            ),
        );
        let constraints = base.join("constraints.json");
        put(&constraints,&json!({"format":1,"constraints":[{"units":["web","telegram"],"minimum_revision":"c".repeat(40),"reason":"implicit_ack rows exist since t5168","source":"private fixture authority"}]}).to_string());
        let options = Options {
            releases: root,
            record_roots: vec![records],
            records: vec![],
            launchers: vec![cli, poll],
            constraints: Some(constraints),
        };
        Self {
            dir,
            options,
            host,
            paths,
        }
    }
    fn row<'a>(&self, report: &'a Value, name: &str) -> &'a Value {
        report["releases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["path"] == self.paths[name].to_str().unwrap())
            .unwrap()
    }
}
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (u32, u64, i64, Vec<u8>)> {
    let mut entries = BTreeMap::new();
    let mut queue = vec![root.to_path_buf()];
    while let Some(path) = queue.pop() {
        let meta = fs::symlink_metadata(&path).unwrap();
        let bytes = if meta.is_file() {
            fs::read(&path).unwrap()
        } else if meta.file_type().is_symlink() {
            fs::read_link(&path)
                .unwrap()
                .as_os_str()
                .as_encoded_bytes()
                .to_vec()
        } else {
            vec![]
        };
        entries.insert(path.clone(), (meta.mode(), meta.len(), meta.mtime(), bytes));
        if meta.is_dir() {
            for entry in fs::read_dir(&path).unwrap() {
                queue.push(entry.unwrap().path());
            }
        }
    }
    entries
}
#[test]
fn release_inventory_unions_all_sources_and_never_writes() {
    let mut f = Fixture::new();
    let before = snapshot(f.dir.path());
    let report = collect_with_host(&f.options, &mut f.host).unwrap();
    assert_eq!(
        before,
        snapshot(f.dir.path()),
        "inventory must not create locks/state or edit inputs"
    );
    assert_eq!(f.host.calls, UNITS.map(|(_, service)| service.to_string()));
    assert_eq!(report["units"][0]["chain"].as_array().unwrap().len(), 3);
    assert!(report["units"][0]["exec_start_winner"]
        .as_str()
        .unwrap()
        .ends_with("99-current.conf"));
    for (name, role) in [
        ("current", "current"),
        ("shadow", "overlay"),
        ("rollback", "recorded-rollback"),
        ("pending", "pending-stage"),
        ("client", "running-process"),
        ("poll", "launcher"),
        ("source-only", "recorded-artifact"),
        ("web", "floor"),
        ("telegram", "floor"),
    ] {
        assert!(
            f.row(&report, name)["roles"]
                .as_array()
                .unwrap()
                .contains(&json!(role)),
            "{name} needs {role}"
        );
    }
    assert_eq!(
        f.row(&report, "unused")["reference_status"],
        "unreferenced in observed inputs"
    );
    assert_eq!(f.row(&report, "old-web")["disposition"], "unknown: keep");
    assert!(f.row(&report, "old-web")["roles"]
        .as_array()
        .unwrap()
        .contains(&json!("recorded-rollback")));
    assert_eq!(
        report["constraints"][0]["reason"],
        "implicit_ack rows exist since t5168"
    );
    assert_eq!(report["summary"]["removal_authorized"], false);
    assert_eq!(report["issues"], json!([]));
    let text = text(&report);
    assert!(
        text.contains("99-current.conf")
            && text.contains("unreferenced in observed inputs")
            && text.contains("implicit_ack")
    );
}
#[test]
fn release_inventory_unknown_metadata_and_links_are_kept_without_reading_targets() {
    let mut f = Fixture::new();
    let secret = f.dir.path().join("secret");
    put(&secret, "SECRET_PAYLOAD_MUST_NOT_APPEAR");
    let legacy = f.options.releases.join("legacy");
    put(&legacy.join("binary"), "legacy");
    symlink(&secret, f.options.releases.join("linked-release")).unwrap();
    let linked = f.options.releases.join("linked-manifest");
    fs::create_dir(&linked).unwrap();
    symlink(&secret, linked.join("MANIFEST")).unwrap();
    put(
        &f.options.record_roots[0].join("bad/STAGING-MANIFEST.json"),
        "invalid JSON",
    );
    symlink(
        &secret,
        f.options.record_roots[0].join("pending/state-link"),
    )
    .unwrap();
    let result = f.options.record_roots[0].join("complete/state/ROLLBACK-RESULT.json");
    symlink(&secret, &result).unwrap();
    let before = snapshot(f.dir.path());
    let report = collect_with_host(&f.options, &mut f.host).unwrap();
    assert_eq!(before, snapshot(f.dir.path()));
    assert!(!report
        .to_string()
        .contains("SECRET_PAYLOAD_MUST_NOT_APPEAR"));
    for row in report["releases"].as_array().unwrap() {
        if ["legacy", "linked-release", "linked-manifest", "unused"]
            .iter()
            .any(|n| row["path"].as_str().unwrap().ends_with(n))
        {
            assert_eq!(row["disposition"], "unknown: keep");
        }
    }
    assert_eq!(report["summary"]["reference_discovery_incomplete"], true);
    assert!(report["records"]
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["state"] == "unknown-state"));
}
#[test]
fn release_inventory_rejects_dynamic_launchers_and_missing_floor_provenance() {
    let mut f = Fixture::new();
    let canary = f.dir.path().join("executed");
    put(
        &f.options.launchers[0],
        &format!(
            "#!/bin/sh\nexec {} \"$@\"\ntouch {}\n",
            f.paths["current"].join("dunkingsheep").display(),
            canary.display()
        ),
    );
    put(
        f.options.constraints.as_ref().unwrap(),
        &json!({"format":1,"constraints":[{"units":["web"],"minimum_revision":"c".repeat(40)}]})
            .to_string(),
    );
    let before = snapshot(f.dir.path());
    let report = collect_with_host(&f.options, &mut f.host).unwrap();
    assert_eq!(before, snapshot(f.dir.path()));
    assert!(!canary.exists());
    assert_eq!(f.row(&report, "unused")["disposition"], "unknown: keep");
    let issues = report["issues"].to_string();
    assert!(issues.contains("unsupported launcher") && issues.contains("reason and source"));
}
#[test]
fn release_inventory_bad_systemd_chain_is_unknown_not_a_guessed_winner() {
    let mut f = Fixture::new();
    f.host
        .units
        .get_mut("dunkingsheep.service")
        .unwrap()
        .0
        .push_str("DropInPaths=/different.conf\n");
    let report = collect_with_host(&f.options, &mut f.host).unwrap();
    assert!(!report["units"][0]["unknown"].as_array().unwrap().is_empty());
    assert!(report["units"][0]["exec_start_winner"].is_null());
    assert_eq!(f.row(&report, "unused")["disposition"], "unknown: keep");
    assert!(f.row(&report, "current")["roles"]
        .as_array()
        .unwrap()
        .contains(&json!("current")));
}
