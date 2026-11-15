//! Read-only release inventory. References are evidence, never deletion or rollback authority.
//! Only named packet metadata is read; no stores, credentials, shell evaluation or locks.
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
    process::Command,
};
type Result<T> = std::result::Result<T, String>;
const LIMIT: u64 = 2 * 1024 * 1024;
const UNITS: [(&str, &str); 3] = [
    ("daemon", "dunkingsheep.service"),
    ("web", "dunkingsheep-web.service"),
    ("telegram", "dunkingsheep-telegram.service"),
];

#[derive(Default)]
pub struct Options {
    pub releases: PathBuf,
    pub record_roots: Vec<PathBuf>,
    pub records: Vec<PathBuf>,
    pub launchers: Vec<PathBuf>,
    pub constraints: Option<PathBuf>,
}
/// There are deliberately no service mutation, daemon RPC or credential methods here.
pub trait Host {
    fn systemd(&mut self, unit: &str) -> Result<(String, String)>;
    fn images(&mut self) -> (Vec<PathBuf>, Vec<String>);
}
struct System;
impl Host for System {
    fn systemd(&mut self, unit: &str) -> Result<(String, String)> {
        let run = |args: &[&str]| -> Result<String> {
            let out = Command::new("systemctl")
                .args(args)
                .env("SYSTEMD_COLORS", "0")
                .env("SYSTEMD_PAGER", "cat")
                .output()
                .map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err(format!("systemctl read failed for {unit}"));
            }
            String::from_utf8(out.stdout).map_err(|e| e.to_string())
        };
        Ok((
            run(&[
                "--user",
                "show",
                unit,
                "-p",
                "FragmentPath",
                "-p",
                "DropInPaths",
                "-p",
                "ExecStart",
                "-p",
                "MainPID",
            ])?,
            run(&["--user", "cat", unit, "--no-pager"])?,
        ))
    }
    fn images(&mut self) -> (Vec<PathBuf>, Vec<String>) {
        let mut images = Vec::new();
        let mut issues = Vec::new();
        let uid = match fs::metadata("/proc/self") {
            Ok(m) => m.uid(),
            Err(_) => return (images, vec!["cannot establish process owner".into()]),
        };
        let entries = match fs::read_dir("/proc") {
            Ok(e) => e,
            Err(_) => return (images, vec!["cannot enumerate processes".into()]),
        };
        for entry in entries.flatten() {
            if !entry
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|c| c.is_ascii_digit())
            {
                continue;
            }
            let path = entry.path();
            match fs::metadata(&path) {
                Ok(m) if m.uid() == uid => {}
                _ => continue,
            }
            match fs::read_link(path.join("exe")) {
                Ok(image) => images.push(image),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => issues.push(format!(
                    "cannot read same-user executable: {}",
                    path.display()
                )),
            }
        }
        images.sort();
        images.dedup();
        (images, issues)
    }
}
fn plain(path: &Path) -> bool {
    path.is_absolute()
        && path.to_str().is_some_and(|s| {
            s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b))
        })
        && !path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
}
fn no_links(path: &Path) -> Result<()> {
    let mut prefix = PathBuf::new();
    for part in path.components() {
        prefix.push(part.as_os_str());
        let m = fs::symlink_metadata(&prefix).map_err(|e| format!("{}: {e}", prefix.display()))?;
        if m.file_type().is_symlink() {
            return Err(format!("symlink: {}", prefix.display()));
        }
    }
    Ok(())
}
fn read(path: &Path, limit: u64) -> Result<Vec<u8>> {
    no_links(path)?;
    if !fs::symlink_metadata(path)
        .map_err(|e| e.to_string())?
        .is_file()
    {
        return Err(format!("not regular: {}", path.display()));
    }
    let mut f = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    if !f.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err(format!("not regular: {}", path.display()));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut f)
        .take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(format!("oversized metadata/artifact: {}", path.display()));
    }
    Ok(bytes)
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn hex(s: &str, n: usize) -> bool {
    s.len() == n
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd)]
struct Reference {
    kind: String,
    source: String,
    unit: String,
}
struct Scan<'a> {
    root: &'a Path,
    refs: BTreeMap<PathBuf, BTreeSet<Reference>>,
    issues: Vec<String>,
}
impl Scan<'_> {
    fn reference(&mut self, path: &Path, kind: &str, source: &str, unit: &str) {
        let Ok(relative) = path.strip_prefix(self.root) else {
            return;
        };
        if !plain(path) {
            self.issues.push(format!("ambiguous reference in {source}"));
            return;
        }
        if let Some(Component::Normal(first)) = relative.components().next() {
            self.refs
                .entry(self.root.join(first))
                .or_default()
                .insert(Reference {
                    kind: kind.into(),
                    source: source.into(),
                    unit: unit.into(),
                });
        }
    }
    fn text_refs(&mut self, text: &str, kind: &str, source: &str, unit: &str) {
        let prefix = format!("{}/", self.root.display());
        for (start, _) in text.match_indices(&prefix) {
            let tail = &text[start + prefix.len()..];
            let length = tail
                .bytes()
                .take_while(|b| b.is_ascii_alphanumeric() || b"/._-".contains(b))
                .count();
            if length == 0 {
                self.issues
                    .push(format!("ambiguous release reference in {source}"));
                continue;
            }
            self.reference(
                Path::new(&text[start..start + prefix.len() + length]),
                kind,
                source,
                unit,
            );
        }
    }
    fn unit(&mut self, key: &str, service: &str, show: &str, cat: &str) -> Value {
        let fields: BTreeMap<_, _> = show
            .lines()
            .filter_map(|line| line.split_once('='))
            .collect();
        let fragment = fields.get("FragmentPath").copied().unwrap_or_default();
        let drops = fields.get("DropInPaths").copied().unwrap_or_default();
        let mut expected = vec![fragment.to_string()];
        expected.extend(drops.split_whitespace().map(str::to_owned));
        let mut sections: Vec<(String, String)> = Vec::new();
        let mut current = None;
        for line in cat.lines() {
            if let Some(path) = line.strip_prefix("# /") {
                sections.push((format!("/{path}"), String::new()));
                current = Some(sections.len() - 1);
            } else if let Some(index) = current {
                sections[index].1.push_str(line);
                sections[index].1.push('\n');
            }
        }
        let actual: Vec<_> = sections.iter().map(|(p, _)| p.clone()).collect();
        let mut unknown = Vec::new();
        let chain_matches = !fragment.is_empty() && expected == actual;
        if !chain_matches {
            unknown.push("show/cat chain mismatch or incomplete".to_string());
        }
        let mut starts: Vec<(String, String)> = Vec::new();
        let mut unsupported_syntax = false;
        let mut chain = Vec::new();
        for (path, text) in &sections {
            self.text_refs(text, "overlay", path, key);
            let mut in_service = false;
            let mut assigns = false;
            for line in text.lines().map(str::trim) {
                if line.starts_with('#') || line.starts_with(';') || line.is_empty() {
                    continue;
                }
                if line.ends_with('\\') {
                    unsupported_syntax = true;
                }
                if line.starts_with('[') {
                    in_service = line == "[Service]";
                    continue;
                }
                if in_service {
                    if let Some(value) = line
                        .split_once('=')
                        .and_then(|(k, v)| (k.trim() == "ExecStart").then_some(v.trim()))
                    {
                        assigns = true;
                        if value.is_empty() {
                            starts.clear();
                        } else {
                            starts.push((path.clone(), value.to_string()));
                        }
                    }
                }
            }
            chain.push(
                json!({"path":path,"sha256":hash(text.as_bytes()),"assigns_exec_start":assigns}),
            );
        }
        let effective = fields
            .get("ExecStart")
            .and_then(|s| s.split_once("path="))
            .and_then(|(_, s)| s.split(|c: char| c == ';' || c.is_whitespace()).next())
            .unwrap_or_default();
        if unsupported_syntax {
            unknown.push("continued unit syntax: ExecStart origin not inferred".into());
        }
        let winner = if !chain_matches || unsupported_syntax {
            None
        } else if starts.len() == 1 {
            let (source, value) = &starts[0];
            let declared = value
                .trim_start_matches(['-', '@', ':', '+', '!'])
                .split_whitespace()
                .next()
                .unwrap_or_default();
            if declared == effective && effective.starts_with('/') {
                Some(source.clone())
            } else {
                unknown.push("ExecStart origin could not be proven".into());
                None
            }
        } else {
            unknown.push("multiple/missing ExecStart assignments".into());
            None
        };
        if effective.starts_with('/') {
            self.reference(Path::new(effective), "current", service, key);
        }
        self.issues
            .extend(unknown.iter().map(|s| format!("{service}: {s}")));
        json!({"unit":key,"service":service,"main_pid":fields.get("MainPID"),"effective_exec_start":effective,"exec_start_winner":winner,"chain":chain,"unknown":unknown})
    }
    fn record(&mut self, path: &Path) -> Option<Value> {
        let result = (|| -> Result<(Vec<u8>, Value)> {
            let raw = read(path, LIMIT)?;
            let v = serde_json::from_slice(&raw).map_err(|e| e.to_string())?;
            Ok((raw, v))
        })();
        let (raw, v) = match result {
            Ok(v) => v,
            Err(e) => {
                self.issues.push(format!("record {}: {e}", path.display()));
                return None;
            }
        };
        let source = path.display().to_string();
        let Some(units) = v.get("units").and_then(Value::as_object) else {
            self.issues
                .push(format!("unsupported record schema: {source}"));
            return None;
        };
        if units.is_empty() {
            self.issues.push(format!("empty record units: {source}"));
        }
        let state = path.parent().unwrap().join("state");
        let mut phase = "pending-or-partial";
        for (name, label) in [
            ("ACTIVATION-RESULT.json", "activation-recorded"),
            ("ROLLBACK-RESULT.json", "rollback-recorded"),
        ] {
            let result_path = state.join(name);
            match fs::symlink_metadata(&result_path) {
                Ok(_) => match read(&result_path, LIMIT)
                    .and_then(|b| serde_json::from_slice::<Value>(&b).map_err(|e| e.to_string()))
                {
                    Ok(data) if data["exit_code"] == 0 => phase = label,
                    _ => {
                        self.issues
                            .push(format!("unknown packet state: {}", result_path.display()));
                        phase = "unknown-state";
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => {
                    self.issues.push(format!(
                        "unreadable packet state: {}",
                        result_path.display()
                    ));
                    phase = "unknown-state";
                }
            }
        }
        for (key, u) in units {
            if !UNITS.iter().any(|(name, _)| name == key) {
                self.issues.push(format!("unknown unit in {source}"));
            }
            match u.get("release").and_then(Value::as_str) {
                Some(p) => self.reference(
                    Path::new(p),
                    if phase == "pending-or-partial" {
                        "pending-stage"
                    } else {
                        "recorded-candidate"
                    },
                    &source,
                    key,
                ),
                None => self
                    .issues
                    .push(format!("missing candidate path: {source}/{key}")),
            }
            match u.pointer("/baseline/executable").and_then(Value::as_str) {
                Some(p) => self.reference(Path::new(p), "recorded-rollback", &source, key),
                None => self
                    .issues
                    .push(format!("missing rollback path: {source}/{key}")),
            }
        }
        if let Some(rows) = v.get("rows").and_then(Value::as_array) {
            for row in rows {
                for field in ["source", "stage", "release"] {
                    if let Some(p) = row.get(field).and_then(Value::as_str) {
                        let p = Path::new(p);
                        let full = if p.is_absolute() {
                            p.to_path_buf()
                        } else {
                            path.parent().unwrap().join(p)
                        };
                        self.reference(
                            &full,
                            "recorded-artifact",
                            &source,
                            row.get("unit").and_then(Value::as_str).unwrap_or("unknown"),
                        );
                    }
                }
            }
        }
        if let Some(poll) = v.get("poll_unchanged").and_then(Value::as_object) {
            for p in poll.keys() {
                self.reference(Path::new(p), "poll", &source, "poll");
            }
        }
        Some(
            json!({"path":path,"sha256":hash(&raw),"state":phase,"note":"References only; not a usable rollback or activation authorization."}),
        )
    }
}
fn discover(options: &Options, issues: &mut Vec<String>) -> BTreeSet<PathBuf> {
    let mut records: BTreeSet<_> = options.records.iter().cloned().collect();
    if records.is_empty() && options.record_roots.is_empty() {
        issues.push("no record roots supplied: external rollback/pending coverage unknown".into());
    }
    for root in &options.record_roots {
        if let Err(e) = no_links(root) {
            issues.push(format!("record root: {e}"));
            continue;
        }
        let entries = match fs::read_dir(root) {
            Ok(e) => e,
            Err(e) => {
                issues.push(format!("record root {}: {e}", root.display()));
                continue;
            }
        };
        if root.join("STAGING-MANIFEST.json").exists() {
            records.insert(root.join("STAGING-MANIFEST.json"));
        }
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(e) => {
                    issues.push(format!("record root entry: {e}"));
                    continue;
                }
            };
            let meta = match entry.file_type() {
                Ok(m) => m,
                Err(e) => {
                    issues.push(format!("record entry: {e}"));
                    continue;
                }
            };
            if meta.is_symlink() {
                issues.push(format!(
                    "record directory symlink skipped: {}",
                    entry.path().display()
                ));
                continue;
            }
            if meta.is_dir() {
                let p = entry.path().join("STAGING-MANIFEST.json");
                match fs::symlink_metadata(&p) {
                    Ok(_) => {
                        records.insert(p);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => issues.push(format!("record metadata: {e}")),
                }
            }
        }
    }
    records
}
fn logical_bytes(root: &Path, issues: &mut Vec<String>) -> u64 {
    let mut queue = vec![(root.to_path_buf(), 0)];
    let mut bytes = 0u64;
    let mut count = 0;
    while let Some((path, depth)) = queue.pop() {
        count += 1;
        if count > 100_000 || depth > 64 {
            issues.push("release tree limit exceeded".into());
            break;
        }
        match fs::symlink_metadata(&path) {
            Ok(m) if m.is_file() => bytes = bytes.saturating_add(m.len()),
            Ok(m) if m.is_dir() => match fs::read_dir(&path) {
                Ok(es) => {
                    for e in es {
                        match e {
                            Ok(e) => queue.push((e.path(), depth + 1)),
                            Err(_) => issues.push("unreadable release entry".into()),
                        }
                    }
                }
                Err(_) => issues.push("unreadable release directory".into()),
            },
            Ok(_) => issues.push("symlink or unexpected release entry: keep".into()),
            Err(_) => issues.push("unreadable release metadata".into()),
        }
    }
    bytes
}
fn plain_launcher(text: &str) -> bool {
    if !text.starts_with("#!/bin/sh\n") {
        return false;
    }
    let lines: Vec<_> = text
        .lines()
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with('#'))
        .collect();
    lines.len() == 1
        && lines[0]
            .strip_prefix("exec ")
            .and_then(|s| s.strip_suffix(" \"$@\""))
            .is_some_and(|s| plain(Path::new(s)))
}
fn floors(path: Option<&Path>, issues: &mut Vec<String>) -> Vec<Value> {
    let Some(path) = path else {
        issues.push("no constraint data supplied: rollback floors unknown".into());
        return vec![];
    };
    let parsed = (|| -> Result<Vec<Value>> {
        let v: Value = serde_json::from_slice(&read(path, LIMIT)?).map_err(|e| e.to_string())?;
        if v["format"] != 1 {
            return Err("unsupported constraint format".into());
        }
        let list = v["constraints"]
            .as_array()
            .ok_or("constraints must be an array")?;
        for c in list {
            if !c["minimum_revision"].as_str().is_some_and(|s| hex(s, 40))
                || !c["reason"].as_str().is_some_and(|s| !s.trim().is_empty())
                || !c["source"].as_str().is_some_and(|s| !s.trim().is_empty())
                || !c["units"].as_array().is_some_and(|u| {
                    !u.is_empty()
                        && u.iter()
                            .all(|s| matches!(s.as_str(), Some("daemon" | "web" | "telegram")))
                })
            {
                return Err("constraint requires unit, full revision, reason and source".into());
            }
        }
        Ok(list.clone())
    })();
    match parsed {
        Ok(v) => v,
        Err(e) => {
            issues.push(format!("constraint {}: {e}", path.display()));
            vec![]
        }
    }
}
/// Inspect once. No filesystem mutation API or daemon client is used.
pub fn collect(options: &Options) -> Result<Value> {
    collect_with_host(options, &mut System)
}
pub fn collect_with_host(options: &Options, host: &mut impl Host) -> Result<Value> {
    if !plain(&options.releases) {
        return Err("release root must be an absolute plain path".into());
    }
    no_links(&options.releases)?;
    if !fs::metadata(&options.releases)
        .map_err(|e| e.to_string())?
        .is_dir()
    {
        return Err("release root is not a directory".into());
    }
    let root = options.releases.as_path();
    let mut scan = Scan {
        root,
        refs: BTreeMap::new(),
        issues: Vec::new(),
    };
    let mut units = Vec::new();
    for (key, service) in UNITS {
        match host.systemd(service) {
            Ok((show, cat)) => units.push(scan.unit(key, service, &show, &cat)),
            Err(e) => {
                scan.issues.push(e.clone());
                units.push(json!({"unit":key,"service":service,"unknown":[e]}));
            }
        }
    }
    let (images, issues) = host.images();
    scan.issues.extend(issues);
    for path in images {
        scan.reference(
            &path,
            "running-process",
            "same-user /proc executable",
            "unknown",
        );
    }
    for path in &options.launchers {
        scan.reference(path, "launcher", &path.display().to_string(), "launcher");
        match fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => match fs::read_link(path) {
                Ok(target) => {
                    let target = if target.is_absolute() {
                        target
                    } else {
                        path.parent().unwrap().join(target)
                    };
                    scan.reference(&target, "launcher", &path.display().to_string(), "launcher");
                    scan.issues.push(format!(
                        "launcher symlink not evaluated transitively: {}",
                        path.display()
                    ));
                }
                Err(e) => scan
                    .issues
                    .push(format!("launcher {}: {e}", path.display())),
            },
            Ok(_) => match read(path, LIMIT) {
                Ok(bytes) => {
                    let text = String::from_utf8_lossy(&bytes);
                    scan.text_refs(&text, "launcher", &path.display().to_string(), "launcher");
                    if !plain_launcher(&text) {
                        scan.issues
                            .push(format!("unsupported launcher syntax: {}", path.display()));
                    }
                }
                Err(e) => scan.issues.push(e),
            },
            Err(e) => scan
                .issues
                .push(format!("launcher {}: {e}", path.display())),
        }
    }
    let record_paths = discover(options, &mut scan.issues);
    let mut records = Vec::new();
    for p in record_paths {
        if let Some(record) = scan.record(&p) {
            records.push(record);
        }
    }
    let constraints = floors(options.constraints.as_deref(), &mut scan.issues);
    let mut entries: Vec<_> = fs::read_dir(root)
        .map_err(|e| e.to_string())?
        .collect::<std::io::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    entries.sort_by_key(|e| e.file_name());
    let mut rows = Vec::new();
    for entry in entries {
        let path = entry.path();
        let mut unknown = Vec::new();
        let mut manifest = Value::Null;
        let meta = fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        let bytes = if meta.is_dir() {
            logical_bytes(&path, &mut unknown)
        } else {
            unknown.push("not a real release directory: keep".into());
            0
        };
        if !plain(&path) {
            unknown.push("unsupported directory name: keep".into());
        }
        if meta.is_dir() {
            let parsed = (|| -> Result<Value> {
                let raw = read(&path.join("MANIFEST"), LIMIT)?;
                let text = std::str::from_utf8(&raw).map_err(|e| e.to_string())?;
                let mut fields = BTreeMap::new();
                for line in text.lines() {
                    let (k, v) = line.split_once('=').ok_or("unsupported MANIFEST line")?;
                    if fields.insert(k, v).is_some() {
                        return Err("duplicate MANIFEST field".into());
                    }
                }
                let unit = fields.get("unit").copied().ok_or("missing unit")?;
                let revision = fields.get("revision").copied().ok_or("missing revision")?;
                let binary = fields.get("binary").copied().ok_or("missing binary")?;
                if fields.get("format") != Some(&"1")
                    || !hex(revision, 40)
                    || !matches!(unit, "daemon" | "web" | "telegram" | "poll")
                    || binary.is_empty()
                    || Path::new(binary).components().count() != 1
                    || !binary
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                {
                    return Err("unknown legacy MANIFEST: keep".into());
                }
                let actual = hash(&read(&path.join(binary), 256 * 1024 * 1024)?);
                if !fields
                    .get("binary_sha256")
                    .is_some_and(|s| hex(s, 64) && *s == actual)
                {
                    return Err("binary digest mismatch: keep".into());
                }
                Ok(
                    json!({"format":1,"unit":unit,"revision":revision,"binary":binary,"manifest_sha256":hash(&raw),"binary_sha256":actual}),
                )
            })();
            match parsed {
                Ok(v) => manifest = v,
                Err(e) => unknown.push(e),
            }
        }
        for c in &constraints {
            if c["units"]
                .as_array()
                .unwrap()
                .iter()
                .any(|u| u == &manifest["unit"])
            {
                if manifest["revision"] == c["minimum_revision"] {
                    scan.reference(
                        &path,
                        "floor",
                        c["source"].as_str().unwrap(),
                        manifest["unit"].as_str().unwrap(),
                    );
                } else {
                    unknown.push("unit floor applies; revision ancestry not inferred; recorded rollback is not authorization".into());
                }
            }
        }
        let refs = scan.refs.get(&path).cloned().unwrap_or_default();
        let roles: BTreeSet<_> = refs.iter().map(|r| r.kind.clone()).collect();
        let status = if roles.contains("current") {
            "current"
        } else if roles.contains("recorded-rollback") {
            "recorded rollback reference"
        } else if !refs.is_empty() {
            "referenced"
        } else {
            "unreferenced in observed inputs"
        };
        if refs.is_empty() && !scan.issues.is_empty() {
            unknown.push("incomplete reference discovery: keep".into());
        }
        unknown.sort();
        unknown.dedup();
        rows.push(json!({"path":path,"logical_bytes":bytes,"manifest":manifest,"reference_status":status,"roles":roles,"references":refs.into_iter().map(|r|json!({"kind":r.kind,"source":r.source,"unit":r.unit})).collect::<Vec<_>>(),"disposition":if unknown.is_empty(){"keep (inventory only)"}else{"unknown: keep"},"unknown":unknown}));
    }
    scan.issues.sort();
    scan.issues.dedup();
    let logical: u64 = rows
        .iter()
        .filter_map(|r| r["logical_bytes"].as_u64())
        .sum();
    Ok(
        json!({"format":1,"read_only":true,"release_root":root,"scope":"All three units, same-user executable links, supplied launchers and explicitly supplied packet roots; no registered global state index exists.","units":units,"records":records,"constraints":constraints,"releases":rows,"issues":scan.issues,"summary":{"release_entries":rows.len(),"logical_bytes":logical,"reference_discovery_incomplete":!scan.issues.is_empty(),"removal_authorized":false},"limits":["Unreferenced means no reference in observed inputs, not safe to remove.","Recorded rollback references are historical metadata, not rollback permission; floor compatibility needs independent review.","SHA order/mtime is not release order. Logical bytes are not physical reclaim.","Read-only snapshot; no lock, registered index, legacy migration or removal implementation."]}),
    )
}
/// Text is a compact rendering of the same inventory rows, not a second host scan.
pub fn text(report: &Value) -> String {
    let mut out = format!(
        "Release inventory (READ ONLY) {}\n",
        report["release_root"].as_str().unwrap_or("?")
    );
    for u in report["units"].as_array().unwrap() {
        out.push_str(&format!(
            "UNIT {}: ExecStart={} winner={}\n",
            u["service"].as_str().unwrap_or("?"),
            u["effective_exec_start"].as_str().unwrap_or("unknown"),
            u["exec_start_winner"].as_str().unwrap_or("unknown: keep")
        ));
        if let Some(chain) = u["chain"].as_array() {
            for (i, f) in chain.iter().enumerate() {
                out.push_str(&format!(
                    "  {} {}{}\n",
                    i,
                    f["path"].as_str().unwrap(),
                    if f["assigns_exec_start"] == true {
                        " [sets/resets ExecStart]"
                    } else {
                        ""
                    }
                ));
            }
        }
    }
    for c in report["constraints"].as_array().unwrap() {
        out.push_str(&format!(
            "FLOOR {} >= {}: {} (source {})\n",
            c["units"],
            c["minimum_revision"].as_str().unwrap(),
            c["reason"].as_str().unwrap(),
            c["source"].as_str().unwrap()
        ));
    }
    for r in report["releases"].as_array().unwrap() {
        out.push_str(&format!(
            "{} | {} | {} | roles={} refs={} bytes={}{}\n",
            r["path"].as_str().unwrap(),
            r["reference_status"].as_str().unwrap(),
            r["disposition"].as_str().unwrap(),
            r["roles"],
            r["references"].as_array().unwrap().len(),
            r["logical_bytes"],
            if r["unknown"].as_array().unwrap().is_empty() {
                String::new()
            } else {
                format!(" | {}", r["unknown"])
            }
        ));
    }
    for issue in report["issues"].as_array().unwrap() {
        out.push_str(&format!("UNKNOWN: KEEP {issue}\n"));
    }
    out.push_str(&format!("SUMMARY {}\n", report["summary"]));
    for limit in report["limits"].as_array().unwrap() {
        out.push_str(&format!("NOTE {}\n", limit.as_str().unwrap()));
    }
    out
}
#[cfg(test)]
mod tests;
