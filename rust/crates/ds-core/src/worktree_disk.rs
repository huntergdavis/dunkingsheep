//! The worktree disk report (t3912): what each directory under the agents' worktree root
//! costs, and which Cargo build caches in it could be deleted without losing work.
//!
//! Read-only: nothing here deletes or writes. Each directory gets its size, its newest
//! modification time, the branch and pull request it belongs to, and its build caches. A build
//! cache is `eligible` only when ALL of these hold:
//!
//! * it carries Cargo's own `CACHEDIR.TAG` (so it is a build cache, not source or evidence);
//! * the directory's pull request is merged or closed (known, not guessed);
//! * nothing in the cache changed for `UNTOUCHED_S`;
//! * no running process works in the directory or builds into the cache.
//!
//! `eligible` is a finding of this report, not a licence to delete. Processes that refuse
//! inspection and show no sign of building here are counted, not proven idle, so "no running
//! process" holds only under that assumption (`eligibility_assumes`). A cleanup must check
//! every condition again, strictly, at the moment it deletes.
//!
//! Anything unknown keeps the cache: an unreadable pull-request state, a directory that is not
//! a checkout, a process table that cannot be read.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

/// A build cache untouched for less than this is kept.
pub const UNTOUCHED_S: f64 = 12.0 * 3600.0;
/// How deep below a worktree a build cache is looked for.
const TARGET_DEPTH: usize = 4;
const TAG_FILE: &str = "CACHEDIR.TAG";
const TAG_SIGNATURE: &str = "Signature: 8a477f597d28d172789f06886806bc55";
const TAG_CARGO: &str = "created by cargo";

/// The pull request a directory belongs to, as far as it is known.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PrLink {
    pub branch: Option<String>,
    pub pr: Option<i64>,
    /// `open`, `merged`, `closed`, `none` (a branch with no pull request) or `unknown`.
    pub state: String,
    /// Why the state is `unknown` or `none`.
    pub reason: Option<String>,
}

impl PrLink {
    pub fn unknown(reason: impl Into<String>) -> PrLink {
        PrLink {
            state: "unknown".into(),
            reason: Some(reason.into()),
            ..PrLink::default()
        }
    }
}

/// The task id in a branch name such as `lane/t3912-disk`, if it names one.
pub fn task_of_branch(branch: &str) -> Option<String> {
    branch
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find(|part| {
            part.len() > 1 && part.starts_with('t') && part[1..].chars().all(|c| c.is_ascii_digit())
        })
        .map(str::to_string)
}

/// Disk blocks and the newest modification time below a path.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Usage {
    pub bytes: u64,
    pub newest: Option<f64>,
    /// Entries that could not be read; their size is not in `bytes`.
    pub unreadable: u64,
    /// The scan ran out of its budget inside this path: `bytes` and `newest` are incomplete.
    pub truncated: bool,
}

/// How much one report may do. A worktree root only grows, so a report that reads all of it
/// takes longer every week; past the budget the report stops and says it is partial.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    started: std::time::Instant,
    /// Wall time for the whole scan.
    pub max: std::time::Duration,
    /// Directories directly below the root that are scanned.
    pub max_dirs: usize,
}

impl Budget {
    pub fn new(max_seconds: f64, max_dirs: usize) -> Budget {
        Budget {
            started: std::time::Instant::now(),
            // A limit that is not a number is no time at all, not a panic.
            max: std::time::Duration::from_secs_f64(if max_seconds.is_finite() {
                max_seconds.clamp(0.0, 86_400.0)
            } else {
                0.0
            }),
            max_dirs,
        }
    }

    pub fn unlimited() -> Budget {
        Budget::new(86_400.0, usize::MAX)
    }

    fn spent(&self) -> bool {
        self.started.elapsed() >= self.max
    }
}

impl Usage {
    fn add(&mut self, other: Usage) {
        self.bytes += other.bytes;
        self.unreadable += other.unreadable;
        self.truncated |= other.truncated;
        self.newest = match (self.newest, other.newest) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
}

/// Walk `path` without following symbolic links, leaving out the `skip` directories. Stops,
/// marked `truncated`, once the budget's time is spent.
pub fn usage(path: &Path, skip: &[PathBuf], budget: &Budget) -> Usage {
    let mut total = Usage::default();
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        total.unreadable = 1;
        return total;
    };
    total.bytes = meta.blocks() * 512;
    total.newest = Some(meta.mtime() as f64);
    if !meta.is_dir() {
        return total;
    }
    let Ok(entries) = std::fs::read_dir(path) else {
        total.unreadable += 1;
        return total;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            total.unreadable += 1;
            continue;
        };
        let child = entry.path();
        if skip.contains(&child) {
            continue;
        }
        if budget.spent() {
            total.truncated = true;
            break;
        }
        total.add(usage(&child, skip, budget));
    }
    total
}

/// True when `dir` holds the cache tag Cargo writes into its target directories.
pub fn has_cargo_tag(dir: &Path) -> bool {
    std::fs::read_to_string(dir.join(TAG_FILE))
        .is_ok_and(|text| text.starts_with(TAG_SIGNATURE) && text.contains(TAG_CARGO))
}

/// The build caches below `dir`: every directory with Cargo's tag, and every directory named
/// `target` without it (reported, never eligible). A cache is not searched for more caches.
pub fn find_targets(dir: &Path) -> Vec<(PathBuf, bool)> {
    fn walk(dir: &Path, depth: usize, found: &mut Vec<(PathBuf, bool)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut children: Vec<PathBuf> = entries.flatten().map(|entry| entry.path()).collect();
        children.sort();
        for child in children {
            let is_dir = std::fs::symlink_metadata(&child).is_ok_and(|meta| meta.is_dir());
            let name = child.file_name().and_then(|name| name.to_str());
            if !is_dir || matches!(name, Some(".git" | "node_modules")) {
                continue;
            }
            if has_cargo_tag(&child) {
                found.push((child, true));
            } else if name == Some("target") {
                found.push((child, false));
            } else if depth < TARGET_DEPTH {
                walk(&child, depth + 1, found);
            }
        }
    }
    let mut found = Vec::new();
    walk(dir, 1, &mut found);
    found
}

/// Programs that write into a Cargo build cache. `comm` holds at most 15 characters, so the
/// names are matched as prefixes of it.
const BUILD_TOOLS: [&str; 14] = [
    "cargo",
    "rustc",
    "rustdoc",
    "rust-analyzer",
    "build-script",
    "cc",
    "gcc",
    "clang",
    "ld",
    "lld",
    "mold",
    "make",
    "ninja",
    "sccache",
];

fn is_build_tool(comm: &str) -> bool {
    let comm = comm.trim();
    BUILD_TOOLS.iter().any(|tool| match *tool {
        // Short names must match whole, or `cc` would claim every `ccache`-like name.
        "cc" | "ld" | "gcc" | "lld" | "make" | "mold" => comm == *tool,
        tool => comm.starts_with(tool),
    })
}

/// What the process table says about where work is happening.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProcessScan {
    /// Every inspected process's current directory and (absolute) `CARGO_TARGET_DIR`.
    pub paths: Vec<PathBuf>,
    /// Processes of this user whose directory or environment may not be read (non-dumpable
    /// ones) and which show no sign of building under `root`.
    pub uninspected_own: u64,
    /// The same for other users' processes and kernel threads.
    pub uninspected_other: u64,
}

/// Where running processes work and build. Fails closed: `Err`, which keeps every cache, when
/// the process table cannot be read, or when a process that cannot be inspected
///
/// * is a build tool by its name,
/// * names a path under `root`, or a relative path through a `target` directory, on its
///   command line,
/// * belongs to this user (`uid`) and mentions `target` or the root's own name anywhere on
///   its command line, or
/// * cannot be identified at all (name or command line unreadable).
///
/// Other uninspectable processes are counted, not trusted blindly and not fatal: on an
/// ordinary host hundreds of other users' processes and kernel threads refuse these reads, and
/// treating each as a possible build would mean no cache is ever known to be idle. A relative
/// `CARGO_TARGET_DIR` is resolved against the directory of the process that has it. A process
/// that is gone by the time it is read is not running.
pub fn busy_paths(proc_root: &Path, root: &Path, uid: u32) -> Result<ProcessScan, String> {
    use std::io::ErrorKind::NotFound;
    let entries = std::fs::read_dir(proc_root)
        .map_err(|error| format!("{}: {error}", proc_root.display()))?;
    let root_real = real(root);
    let root_name = root
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let mut scan = ProcessScan::default();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .filter(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()))
        else {
            continue;
        };
        let process = entry.path();
        let cwd = std::fs::read_link(process.join("cwd"));
        let environ = std::fs::read(process.join("environ"));
        let gone = |error: &std::io::Error| error.kind() == NotFound;
        match (&cwd, &environ) {
            (Err(error), _) | (_, Err(error)) if gone(error) => continue,
            (Ok(cwd), Ok(environ)) => {
                scan.paths.push(cwd.clone());
                for variable in environ.split(|byte| *byte == 0) {
                    if let Some(value) = variable.strip_prefix(b"CARGO_TARGET_DIR=") {
                        if !value.is_empty() {
                            let value = PathBuf::from(String::from_utf8_lossy(value).into_owned());
                            // `Path::join` keeps an absolute value as it is.
                            scan.paths.push(cwd.join(value));
                        }
                    }
                }
            }
            _ => {
                let comm = std::fs::read_to_string(process.join("comm"));
                let cmdline = std::fs::read(process.join("cmdline"));
                let (comm, cmdline) = match (comm, cmdline) {
                    (Err(error), _) | (_, Err(error)) if gone(&error) => continue,
                    (Ok(comm), Ok(cmdline)) => (comm, cmdline),
                    _ => return Err(format!("process {pid} cannot be inspected or identified")),
                };
                if is_build_tool(&comm) {
                    return Err(format!(
                        "process {pid} ({}) is a build tool and cannot be inspected",
                        comm.trim()
                    ));
                }
                // An owner that cannot be read is treated as this user: the stricter rule.
                let own = std::fs::metadata(&process).map_or(true, |meta| meta.uid() == uid);
                for argument in cmdline.split(|byte| *byte == 0) {
                    let text = String::from_utf8_lossy(argument).into_owned();
                    let mentions = text.contains("target")
                        || root_name.as_deref().is_some_and(|name| text.contains(name));
                    if own && mentions {
                        return Err(format!(
                            "process {pid} is ours, cannot be inspected, and its command line mentions a build cache or the worktree root"
                        ));
                    }
                    let argument = PathBuf::from(text);
                    let under_root = argument.is_absolute()
                        && (argument.starts_with(root) || real(&argument).starts_with(&root_real));
                    let through_target = argument.is_relative()
                        && argument
                            .components()
                            .any(|part| part.as_os_str() == "target");
                    if under_root || through_target {
                        return Err(format!(
                            "process {pid} cannot be inspected and names a path that may be under the worktree root"
                        ));
                    }
                }
                if own {
                    scan.uninspected_own += 1;
                } else {
                    scan.uninspected_other += 1;
                }
            }
        }
    }
    Ok(scan)
}

pub(crate) fn real(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn age(now: f64, newest: Option<f64>) -> Option<i64> {
    newest.map(|at| (now - at).max(0.0).floor() as i64)
}

/// The report for every directory directly below `root`. `link` says which pull request a
/// directory belongs to; `busy` is `busy_paths`, or its error.
pub fn report(
    root: &Path,
    now: f64,
    link: &dyn Fn(&Path) -> PrLink,
    busy: &Result<Vec<PathBuf>, String>,
) -> Result<Value, String> {
    report_within(root, now, link, busy, &Budget::unlimited())
}

/// `report`, stopping at the budget: directories past it are not scanned and are counted in
/// `skipped_dirs`, and the report is marked `partial`. A cache whose own scan was cut short
/// is kept.
pub fn report_within(
    root: &Path,
    now: f64,
    link: &dyn Fn(&Path) -> PrLink,
    busy: &Result<Vec<PathBuf>, String>,
    budget: &Budget,
) -> Result<Value, String> {
    let entries =
        std::fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))?;
    let mut children: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()))
        .collect();
    children.sort();
    let busy_real: Result<Vec<PathBuf>, &str> = match busy {
        Ok(paths) => Ok(paths.iter().map(|path| real(path)).collect()),
        Err(error) => Err(error.as_str()),
    };

    let mut dirs = Vec::new();
    let (mut bytes, mut target_bytes, mut eligible_bytes, mut eligible_targets) = (0, 0, 0, 0u64);
    let found_dirs = children.len();
    let mut skipped_dirs = 0usize;
    let mut partial_reason: Option<&str> = None;
    for (index, dir) in children.into_iter().enumerate() {
        if index >= budget.max_dirs {
            partial_reason.get_or_insert("the directory limit was reached");
        } else if budget.spent() {
            partial_reason.get_or_insert("the time limit was reached");
        }
        if partial_reason.is_some() {
            skipped_dirs += 1;
            continue;
        }
        let found = find_targets(&dir);
        let skip: Vec<PathBuf> = found.iter().map(|(path, _)| path.clone()).collect();
        let mut total = usage(&dir, &skip, budget);
        let pr = link(&dir);
        let dir_real = real(&dir);
        let dir_busy = busy_real
            .as_ref()
            .is_ok_and(|paths| paths.iter().any(|path| path.starts_with(&dir_real)));
        let mut targets = Vec::new();
        for (target, tagged) in found {
            let used = usage(&target, &[], budget);
            total.add(used);
            target_bytes += used.bytes;
            let target_real = real(&target);
            let mut keep: Vec<String> = Vec::new();
            if !tagged {
                keep.push("no Cargo cache tag: not known to be a build cache".into());
            }
            if used.unreadable > 0 {
                keep.push("part of it could not be read, so its age is not known".into());
            }
            if used.truncated {
                keep.push("the scan ran out of time before reading it fully".into());
            }
            match pr.state.as_str() {
                "merged" | "closed" => {}
                "open" => keep.push("its pull request is open".into()),
                "none" => keep.push(format!(
                    "no pull request: {}",
                    pr.reason.as_deref().unwrap_or("none found for the branch")
                )),
                _ => keep.push(format!(
                    "pull request state unknown: {}",
                    pr.reason.as_deref().unwrap_or("not read")
                )),
            }
            match age(now, used.newest) {
                Some(seconds) if seconds as f64 > UNTOUCHED_S => {}
                Some(_) => keep.push("changed in the last 12 hours".into()),
                None => keep.push("its age could not be read".into()),
            }
            match &busy_real {
                Err(error) => keep.push(format!("running processes could not be read: {error}")),
                Ok(paths) => {
                    if dir_busy || paths.iter().any(|path| path.starts_with(&target_real)) {
                        keep.push("a running process works in this directory".into());
                    }
                }
            }
            let eligible = keep.is_empty();
            if eligible {
                eligible_bytes += used.bytes;
                eligible_targets += 1;
            }
            targets.push(json!({
                "path": target,
                "bytes": used.bytes,
                "newest_mtime": used.newest,
                "age_s": age(now, used.newest),
                "cargo_tag": tagged,
                "eligible": eligible,
                "keep_reasons": keep,
            }));
        }
        bytes += total.bytes;
        dirs.push(json!({
            "name": dir.file_name().and_then(|name| name.to_str()),
            "path": dir,
            "bytes": total.bytes,
            "unreadable": total.unreadable,
            "truncated": total.truncated,
            "newest_mtime": total.newest,
            "age_s": age(now, total.newest),
            "branch": pr.branch,
            "task": pr.branch.as_deref().and_then(task_of_branch),
            "pr": pr.pr,
            "pr_state": pr.state,
            "pr_reason": pr.reason,
            "in_use": busy_real.as_ref().ok().map(|_| dir_busy),
            "targets": targets,
        }));
    }
    dirs.sort_by_key(|dir| std::cmp::Reverse(dir["bytes"].as_u64().unwrap_or(0)));
    let filesystem = match nix::sys::statvfs::statvfs(root) {
        Ok(stat) => {
            let unit = stat.fragment_size();
            let total = stat.blocks() * unit;
            let free = stat.blocks_available() * unit;
            json!({
                "total_bytes": total,
                "free_bytes": free,
                "used_pct": ((total - free.min(total)) * 100).checked_div(total),
                "reason": null,
            })
        }
        Err(error) => json!({
            "total_bytes": null, "free_bytes": null, "used_pct": null,
            "reason": format!("statvfs failed: {error}"),
        }),
    };
    Ok(json!({
        "root": root,
        "generated_at": now,
        "untouched_s": UNTOUCHED_S,
        "read_only": true,
        "cleanup_note": "eligible is this report's finding, not a licence to delete: a cleanup must re-check every condition strictly at delete time",
        "partial": partial_reason.is_some() || dirs.iter().any(|dir| dir["truncated"] == true),
        "partial_reason": partial_reason.or_else(|| {
            dirs.iter()
                .any(|dir| dir["truncated"] == true)
                .then_some("the time limit was reached")
        }),
        "found_dirs": found_dirs,
        "skipped_dirs": skipped_dirs,
        "filesystem": filesystem,
        "totals": {
            "dirs": dirs.len(),
            "bytes": bytes,
            "target_bytes": target_bytes,
            "eligible_targets": eligible_targets,
            "eligible_bytes": eligible_bytes,
        },
        "dirs": dirs,
    }))
}

fn size_text(bytes: u64) -> String {
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    let gib = bytes as f64 / GIB;
    if gib >= 1.0 {
        format!("{gib:.1} GiB")
    } else {
        format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn age_text(seconds: Option<i64>) -> String {
    match seconds {
        None => "unknown".into(),
        Some(s) if s < 3600 => format!("{} min", s / 60),
        Some(s) if s < 48 * 3600 => format!("{} h", s / 3600),
        Some(s) => format!("{} d", s / 86_400),
    }
}

/// Record the processes that refused inspection in a report (`ProcessScan`'s two counts).
/// When there are any, every `eligible` in the report rests on the assumption written here.
pub fn note_uninspected(report: &mut Value, own: u64, other_users: u64) {
    report["uninspected_processes"] = json!({"own": own, "other_users": other_users});
    report["eligibility_assumes"] = if own + other_users > 0 {
        json!(format!(
            "{own} processes of this user and {other_users} of other users could not be inspected; \
             they show no sign of building here (not a build tool, no path under the root on the \
             command line) and are assumed not to use these caches"
        ))
    } else {
        Value::Null
    };
}

/// The operator's view of `report`: one line per directory, largest first, then its caches.
pub fn render_text(report: &Value) -> String {
    let mut out = String::new();
    let totals = &report["totals"];
    let filesystem = &report["filesystem"];
    out.push_str(&format!(
        "{}: {} in {} directories; build caches {}; eligible for cleanup {} in {} caches\n",
        report["root"].as_str().unwrap_or("?"),
        size_text(totals["bytes"].as_u64().unwrap_or(0)),
        totals["dirs"].as_u64().unwrap_or(0),
        size_text(totals["target_bytes"].as_u64().unwrap_or(0)),
        size_text(totals["eligible_bytes"].as_u64().unwrap_or(0)),
        totals["eligible_targets"].as_u64().unwrap_or(0),
    ));
    match filesystem["used_pct"].as_i64() {
        Some(used) => out.push_str(&format!(
            "filesystem: {used}% used, {} free\n",
            size_text(filesystem["free_bytes"].as_u64().unwrap_or(0))
        )),
        None => out.push_str(&format!(
            "filesystem: unknown ({})\n",
            filesystem["reason"].as_str().unwrap_or("not read")
        )),
    }
    out.push_str("read-only report: nothing was deleted\n");
    if let Some(assumes) = report["eligibility_assumes"].as_str() {
        out.push_str(&format!("ASSUMPTION: {assumes}\n"));
    }
    out.push_str(
        "eligible is a finding, not a licence to delete: a cleanup must re-check at delete time\n",
    );
    if report["partial"] == true {
        out.push_str(&format!(
            "PARTIAL: {}; {} of {} directories were not scanned, and the totals cover only the rest\n",
            report["partial_reason"].as_str().unwrap_or("a limit was reached"),
            report["skipped_dirs"].as_u64().unwrap_or(0),
            report["found_dirs"].as_u64().unwrap_or(0),
        ));
    }
    for dir in report["dirs"].as_array().into_iter().flatten() {
        let pr = match (
            dir["pr"].as_i64(),
            dir["pr_state"].as_str().unwrap_or("unknown"),
        ) {
            (Some(number), state) => format!("PR {number} {state}"),
            (None, state) => format!("PR {state}"),
        };
        out.push_str(&format!(
            "\n{:>10}  {}  last change {} ago  {}  {}{}\n",
            size_text(dir["bytes"].as_u64().unwrap_or(0)),
            dir["name"].as_str().unwrap_or("?"),
            age_text(dir["age_s"].as_i64()),
            dir["branch"].as_str().unwrap_or("no branch"),
            pr,
            match dir["in_use"].as_bool() {
                Some(true) => "  IN USE",
                Some(false) => "",
                None => "  use unknown",
            },
        ));
        for target in dir["targets"].as_array().into_iter().flatten() {
            let verdict = if target["eligible"].as_bool() == Some(true) {
                if report["eligibility_assumes"].is_string() {
                    "eligible under the assumption above".to_string()
                } else {
                    "eligible".to_string()
                }
            } else {
                let reasons: Vec<&str> = target["keep_reasons"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                format!("keep: {}", reasons.join("; "))
            };
            out.push_str(&format!(
                "{:>10}    cache {}  untouched {}  {}\n",
                size_text(target["bytes"].as_u64().unwrap_or(0)),
                target["path"].as_str().unwrap_or("?"),
                age_text(target["age_s"].as_i64()),
                verdict,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    const CARGO_TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag created by cargo.\n";
    const OLD: i64 = 1_000_000;
    const NOW: f64 = 1_000_000.0 + UNTOUCHED_S + 60.0;

    fn touch(path: &Path, at: i64) {
        let time = nix::sys::time::TimeSpec::new(at, 0);
        nix::sys::stat::utimensat(
            None,
            path,
            &time,
            &time,
            nix::sys::stat::UtimensatFlags::NoFollowSymlink,
        )
        .unwrap();
    }

    /// A worktree with source, and a build cache whose every entry is `at` old.
    fn worktree(root: &Path, name: &str, tagged: bool, at: i64) -> PathBuf {
        let dir = root.join(name);
        let target = dir.join("rust").join("target");
        fs::create_dir_all(target.join("debug")).unwrap();
        fs::write(dir.join("source.rs"), "fn main() {}").unwrap();
        fs::write(target.join("debug").join("artifact"), vec![0u8; 8192]).unwrap();
        if tagged {
            fs::write(target.join(TAG_FILE), CARGO_TAG).unwrap();
            touch(&target.join(TAG_FILE), at);
        }
        touch(&target.join("debug").join("artifact"), at);
        touch(&target.join("debug"), at);
        touch(&target, at);
        dir
    }

    fn link(state: &'static str) -> impl Fn(&Path) -> PrLink {
        move |_| PrLink {
            branch: Some("lane/t3912-disk".into()),
            pr: (state != "none" && state != "unknown").then_some(7),
            state: state.into(),
            reason: (state == "unknown").then(|| "gh failed".to_string()),
        }
    }

    fn me() -> u32 {
        nix::unistd::Uid::effective().as_raw()
    }

    fn only_target(report: &Value) -> Value {
        report["dirs"][0]["targets"][0].clone()
    }

    #[test]
    fn an_old_tagged_cache_of_a_merged_pr_with_no_process_is_eligible() {
        let home = crate::testsupport::dir("worktree-disk-eligible");
        worktree(home.path(), "a-wt", true, OLD);
        for state in ["merged", "closed"] {
            let report = report(home.path(), NOW, &link(state), &Ok(Vec::new())).unwrap();
            let target = only_target(&report);
            assert_eq!(target["eligible"], true, "{report}");
            assert_eq!(target["keep_reasons"], json!([]));
            assert_eq!(target["cargo_tag"], true);
            assert_eq!(report["totals"]["eligible_targets"], 1);
            assert_eq!(report["totals"]["eligible_bytes"], target["bytes"]);
            assert_eq!(report["dirs"][0]["task"], "t3912");
            assert_eq!(report["dirs"][0]["pr"], 7);
            assert_eq!(report["read_only"], true);
        }
    }

    #[test]
    fn each_missing_condition_keeps_the_cache_and_says_why() {
        let home = crate::testsupport::dir("worktree-disk-keep");
        let dir = worktree(home.path(), "a-wt", true, OLD);
        let reasons = |report: Value| -> Vec<String> {
            let target = only_target(&report);
            assert_eq!(target["eligible"], false, "{report}");
            assert_eq!(report["totals"]["eligible_targets"], 0);
            target["keep_reasons"]
                .as_array()
                .unwrap()
                .iter()
                .map(|reason| reason.as_str().unwrap().to_string())
                .collect()
        };
        let idle = Ok(Vec::new());
        assert_eq!(
            reasons(report(home.path(), NOW, &link("open"), &idle).unwrap()),
            ["its pull request is open"]
        );
        assert_eq!(
            reasons(report(home.path(), NOW, &link("none"), &idle).unwrap()),
            ["no pull request: none found for the branch"]
        );
        assert_eq!(
            reasons(report(home.path(), NOW, &link("unknown"), &idle).unwrap()),
            ["pull request state unknown: gh failed"]
        );
        // Changed 12 hours ago to the second: not yet "more than".
        assert_eq!(
            reasons(
                report(
                    home.path(),
                    OLD as f64 + UNTOUCHED_S,
                    &link("merged"),
                    &idle
                )
                .unwrap()
            ),
            ["changed in the last 12 hours"]
        );
        // A process whose current directory is the worktree, or one building into the cache.
        for busy in [
            dir.join("rust"),
            dir.join("rust").join("target").join("debug"),
        ] {
            assert_eq!(
                reasons(report(home.path(), NOW, &link("merged"), &Ok(vec![busy])).unwrap()),
                ["a running process works in this directory"]
            );
        }
        assert_eq!(
            reasons(report(home.path(), NOW, &link("merged"), &Err("no /proc".into())).unwrap()),
            ["running processes could not be read: no /proc"]
        );
    }

    #[test]
    fn an_untagged_target_is_reported_and_never_eligible() {
        let home = crate::testsupport::dir("worktree-disk-untagged");
        worktree(home.path(), "a-wt", false, OLD);
        let report = report(home.path(), NOW, &link("merged"), &Ok(Vec::new())).unwrap();
        let target = only_target(&report);
        assert_eq!(target["cargo_tag"], false);
        assert_eq!(target["eligible"], false);
        assert_eq!(
            target["keep_reasons"],
            json!(["no Cargo cache tag: not known to be a build cache"])
        );
        // Another tool's cache tag is not Cargo's.
        fs::write(
            home.path().join("a-wt/rust/target").join(TAG_FILE),
            "Signature: 8a477f597d28d172789f06886806bc55\n# some other tool\n",
        )
        .unwrap();
        assert!(!has_cargo_tag(&home.path().join("a-wt/rust/target")));
    }

    #[test]
    fn sizes_cover_source_and_caches_once_and_the_largest_directory_is_first() {
        let home = crate::testsupport::dir("worktree-disk-size");
        worktree(home.path(), "small-wt", true, OLD);
        let big = worktree(home.path(), "big-wt", true, OLD);
        fs::write(big.join("evidence.bin"), vec![1u8; 300_000]).unwrap();
        fs::write(home.path().join("a-file-not-a-directory"), "x").unwrap();
        let report = report(home.path(), NOW, &link("merged"), &Ok(Vec::new())).unwrap();
        assert_eq!(report["totals"]["dirs"], 2);
        assert_eq!(report["dirs"][0]["name"], "big-wt");
        let dir_bytes = report["dirs"][0]["bytes"].as_u64().unwrap();
        let target_bytes = report["dirs"][0]["targets"][0]["bytes"].as_u64().unwrap();
        assert!(
            target_bytes >= 8192 && dir_bytes >= target_bytes + 300_000,
            "{report}"
        );
        assert_eq!(
            usage(&big, &[], &Budget::unlimited()).bytes,
            dir_bytes,
            "the cache is counted once"
        );
        let text = render_text(&report);
        assert!(
            text.contains("read-only report: nothing was deleted"),
            "{text}"
        );
        assert!(
            text.contains("\n") && text.contains("  eligible\n"),
            "{text}"
        );
        assert!(!text.contains("safe to delete"), "{text}");
        assert!(!text.contains("ASSUMPTION"), "{text}");
        assert!(
            text.contains("a cleanup must re-check at delete time"),
            "{text}"
        );
        let mut assumed = report.clone();
        note_uninspected(&mut assumed, 11, 227);
        let text = render_text(&assumed);
        assert!(
            text.contains("ASSUMPTION: 11 processes of this user and 227 of other users could not be inspected"),
            "{text}"
        );
        assert!(
            text.contains("eligible under the assumption above"),
            "{text}"
        );
        assert_eq!(
            assumed["uninspected_processes"],
            json!({"own": 11, "other_users": 227})
        );
        let mut none = report.clone();
        note_uninspected(&mut none, 0, 0);
        assert_eq!(none["eligibility_assumes"], Value::Null);
        assert!(report["cleanup_note"]
            .as_str()
            .unwrap()
            .contains("re-check every condition strictly at delete time"));
        assert!(text.contains("PR 7 merged"), "{text}");
        assert!(
            big.join("rust/target/debug/artifact").exists(),
            "nothing is deleted"
        );
    }

    #[test]
    fn a_report_past_its_budget_is_partial_and_says_what_it_left_out() {
        let home = crate::testsupport::dir("worktree-disk-budget");
        worktree(home.path(), "a-wt", true, OLD);
        worktree(home.path(), "b-wt", true, OLD);
        let idle = Ok(Vec::new());
        let full = report(home.path(), NOW, &link("merged"), &idle).unwrap();
        assert_eq!(
            (&full["partial"], &full["skipped_dirs"]),
            (&json!(false), &json!(0))
        );
        assert_eq!(full["partial_reason"], Value::Null);

        let one = report_within(
            home.path(),
            NOW,
            &link("merged"),
            &idle,
            &Budget::new(3600.0, 1),
        )
        .unwrap();
        assert_eq!(one["partial"], true);
        assert_eq!(one["partial_reason"], "the directory limit was reached");
        assert_eq!(
            (&one["found_dirs"], &one["skipped_dirs"]),
            (&json!(2), &json!(1))
        );
        assert_eq!(one["totals"]["dirs"], 1);
        assert_eq!(
            one["totals"]["eligible_targets"], 1,
            "only what was scanned is counted"
        );
        assert!(render_text(&one).contains("PARTIAL: the directory limit was reached; 1 of 2"));

        let none = report_within(
            home.path(),
            NOW,
            &link("merged"),
            &idle,
            &Budget::new(0.0, 100),
        )
        .unwrap();
        assert_eq!(none["partial_reason"], "the time limit was reached");
        assert_eq!(
            (&none["skipped_dirs"], &none["totals"]["dirs"]),
            (&json!(2), &json!(0))
        );
        assert_eq!(none["totals"]["eligible_targets"], 0);
    }

    #[test]
    fn a_walk_cut_short_is_marked_and_its_cache_is_kept() {
        let home = crate::testsupport::dir("worktree-disk-truncated");
        let dir = worktree(home.path(), "a-wt", true, OLD);
        let cut = usage(&dir, &[], &Budget::new(0.0, 100));
        assert!(cut.truncated);
        assert!(cut.bytes < usage(&dir, &[], &Budget::unlimited()).bytes);
        assert!(!usage(&dir, &[], &Budget::unlimited()).truncated);
    }

    /// A fake process: `cwd` as a link (inspectable) or as a plain file (reading the link
    /// fails, as it does for another user's process).
    fn process(
        proc_root: &Path,
        pid: &str,
        cwd: Option<&str>,
        environ: &[u8],
        comm: &str,
        cmdline: &[u8],
    ) {
        let dir = proc_root.join(pid);
        fs::create_dir_all(&dir).unwrap();
        match cwd {
            Some(cwd) => std::os::unix::fs::symlink(cwd, dir.join("cwd")).unwrap(),
            None => fs::write(dir.join("cwd"), "").unwrap(),
        }
        fs::write(dir.join("environ"), environ).unwrap();
        fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
        fs::write(dir.join("cmdline"), cmdline).unwrap();
    }

    #[test]
    fn busy_paths_reads_cwd_and_cargo_target_dir_of_numbered_processes() {
        let proc_root = crate::testsupport::dir("worktree-disk-proc");
        process(
            proc_root.path(),
            "101",
            Some("/work/a-wt/rust"),
            b"HOME=/h\0CARGO_TARGET_DIR=/work/b-wt/target\0",
            "cargo",
            b"cargo\0build\0",
        );
        // A relative target directory belongs under the directory of the process that has it.
        process(
            proc_root.path(),
            "102",
            Some("/work/c-wt/rust"),
            b"CARGO_TARGET_DIR=target\0",
            "cargo",
            b"cargo\0",
        );
        fs::create_dir_all(proc_root.path().join("self")).unwrap();
        // A process that exited between the listing and the read is not running.
        fs::create_dir_all(proc_root.path().join("103")).unwrap();
        let mut scan = busy_paths(proc_root.path(), Path::new("/work"), me()).unwrap();
        scan.paths.sort();
        assert_eq!(
            scan.paths,
            [
                PathBuf::from("/work/a-wt/rust"),
                PathBuf::from("/work/b-wt/target"),
                PathBuf::from("/work/c-wt/rust"),
                PathBuf::from("/work/c-wt/rust/target"),
            ]
        );
        assert_eq!((scan.uninspected_own, scan.uninspected_other), (0, 0));
        assert!(busy_paths(&proc_root.path().join("missing"), Path::new("/work"), me()).is_err());
    }

    /// The scan of one uninspectable process, as `uid`, with the root `/work/.agent-trees`.
    fn scan_one(comm: &str, cmdline: &[u8], uid: u32) -> Result<ProcessScan, String> {
        let proc_root = crate::testsupport::dir("worktree-disk-proc-closed");
        process(proc_root.path(), "7", None, b"", comm, cmdline);
        busy_paths(proc_root.path(), Path::new("/work/.agent-trees"), uid)
    }

    #[test]
    fn an_uninspectable_process_of_ours_is_counted_unless_it_could_be_building_here() {
        // A non-dumpable helper of ours: counted, and the scan still answers.
        for (comm, cmdline) in [
            ("agent-helper", &b""[..]),
            ("sshd", b"/usr/sbin/sshd\0-D\0"),
            ("ccache-daemon", b"ccache-daemon\0"),
        ] {
            let scan = scan_one(comm, cmdline, me()).unwrap();
            assert_eq!(
                (scan.uninspected_own, scan.uninspected_other),
                (1, 0),
                "{comm}"
            );
        }
        // A build tool, by its name alone.
        for tool in [
            "cargo",
            "rustc",
            "build-script-bu",
            "rust-analyzer",
            "cc",
            "ld",
        ] {
            let error = scan_one(tool, b"x\0", me()).unwrap_err();
            assert!(
                error.contains("is a build tool and cannot be inspected"),
                "{tool}: {error}"
            );
        }
        // One of ours that mentions a build cache or the root anywhere on its command line.
        for cmdline in [
            &b"sh\0-c\0cd somewhere && ls target\0"[..],
            b"tool\0--dir=.agent-trees/x\0",
            b"./target/debug/app\0",
            b"python3\0/work/.agent-trees/a-wt/tool.py\0",
        ] {
            let error = scan_one("tool", cmdline, me()).unwrap_err();
            assert!(
                error.contains("command line mentions a build cache or the worktree root"),
                "{error}"
            );
        }
    }

    #[test]
    fn another_users_uninspectable_process_blocks_only_on_a_path_that_may_be_under_the_root() {
        // The fixture's process belongs to the test's user; scanning as a different uid makes
        // it "another user's".
        let other = me().wrapping_add(1);
        let counted = scan_one("daemon", b"daemon\0--log=/var/log/target.log\0", other).unwrap();
        assert_eq!((counted.uninspected_own, counted.uninspected_other), (0, 1));
        assert_eq!(
            scan_one("kworker/0:1", b"", other)
                .unwrap()
                .uninspected_other,
            1
        );
        for cmdline in [
            &b"/work/.agent-trees/a-wt/rust/target/debug/deps/unit-1\0"[..],
            b"./target/debug/app\0",
        ] {
            let error = scan_one("unit-1", cmdline, other).unwrap_err();
            assert!(
                error.contains("names a path that may be under the worktree root"),
                "{error}"
            );
        }
        assert!(
            scan_one("cargo", b"cargo\0", other).is_err(),
            "a build tool of any user"
        );
    }

    #[test]
    fn a_process_that_cannot_even_be_identified_fails_the_scan() {
        let proc_root = crate::testsupport::dir("worktree-disk-proc-blind");
        process(proc_root.path(), "7", None, b"", "x", b"");
        // The name cannot be read either (a directory where the file should be).
        fs::remove_file(proc_root.path().join("7/comm")).unwrap();
        fs::create_dir(proc_root.path().join("7/comm")).unwrap();
        let error = busy_paths(proc_root.path(), Path::new("/work"), me()).unwrap_err();
        assert_eq!(error, "process 7 cannot be inspected or identified");
        // An unreadable environment alone is enough to be uninspectable.
        let other = crate::testsupport::dir("worktree-disk-proc-env");
        process(
            other.path(),
            "8",
            Some("/elsewhere"),
            b"",
            "cargo",
            b"cargo\0",
        );
        fs::remove_file(other.path().join("8/environ")).unwrap();
        fs::create_dir(other.path().join("8/environ")).unwrap();
        assert!(busy_paths(other.path(), Path::new("/work"), me()).is_err());
    }

    #[test]
    fn a_cache_with_unreadable_content_is_kept() {
        use std::os::unix::fs::PermissionsExt;
        if nix::unistd::Uid::effective().is_root() {
            return; // root reads everything; the case cannot be built
        }
        let home = crate::testsupport::dir("worktree-disk-unreadable");
        let dir = worktree(home.path(), "a-wt", true, OLD);
        let locked = dir.join("rust/target/locked");
        fs::create_dir(&locked).unwrap();
        touch(&locked, OLD);
        touch(&dir.join("rust/target"), OLD);
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o000)).unwrap();
        let report = report(home.path(), NOW, &link("merged"), &Ok(Vec::new()));
        fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
        let target = only_target(&report.unwrap());
        assert_eq!(target["eligible"], false);
        assert_eq!(
            target["keep_reasons"],
            json!(["part of it could not be read, so its age is not known"])
        );
    }

    #[test]
    fn the_task_is_read_from_the_branch_name_only_when_it_names_one() {
        assert_eq!(
            task_of_branch("fable/t4706-command-counters").as_deref(),
            Some("t4706")
        );
        assert_eq!(task_of_branch("task/t12").as_deref(), Some("t12"));
        assert_eq!(task_of_branch("main"), None);
        assert_eq!(task_of_branch("feature/tidy-up"), None);
    }
}
