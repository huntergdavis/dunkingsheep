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
}

impl Usage {
    fn add(&mut self, other: Usage) {
        self.bytes += other.bytes;
        self.unreadable += other.unreadable;
        self.newest = match (self.newest, other.newest) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
    }
}

/// Walk `path` without following symbolic links, leaving out the `skip` directories.
pub fn usage(path: &Path, skip: &[PathBuf]) -> Usage {
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
        total.add(usage(&child, skip));
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

/// Where running processes work and build: every readable process's current directory and its
/// `CARGO_TARGET_DIR`. `Err` when the process table itself cannot be read.
pub fn busy_paths(proc_root: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = std::fs::read_dir(proc_root)
        .map_err(|error| format!("{}: {error}", proc_root.display()))?;
    let mut paths = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        if !name
            .to_str()
            .is_some_and(|name| name.chars().all(|c| c.is_ascii_digit()))
        {
            continue;
        }
        let process = entry.path();
        if let Ok(cwd) = std::fs::read_link(process.join("cwd")) {
            paths.push(cwd);
        }
        if let Ok(environ) = std::fs::read(process.join("environ")) {
            for variable in environ.split(|byte| *byte == 0) {
                if let Some(value) = variable.strip_prefix(b"CARGO_TARGET_DIR=") {
                    if !value.is_empty() {
                        paths.push(PathBuf::from(String::from_utf8_lossy(value).into_owned()));
                    }
                }
            }
        }
    }
    Ok(paths)
}

fn real(path: &Path) -> PathBuf {
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
    for dir in children {
        let found = find_targets(&dir);
        let skip: Vec<PathBuf> = found.iter().map(|(path, _)| path.clone()).collect();
        let mut total = usage(&dir, &skip);
        let pr = link(&dir);
        let dir_real = real(&dir);
        let dir_busy = busy_real
            .as_ref()
            .is_ok_and(|paths| paths.iter().any(|path| path.starts_with(&dir_real)));
        let mut targets = Vec::new();
        for (target, tagged) in found {
            let used = usage(&target, &[]);
            total.add(used);
            target_bytes += used.bytes;
            let target_real = real(&target);
            let mut keep: Vec<String> = Vec::new();
            if !tagged {
                keep.push("no Cargo cache tag: not known to be a build cache".into());
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

/// The operator's view of `report`: one line per directory, largest first, then its caches.
pub fn render_text(report: &Value) -> String {
    let mut out = String::new();
    let totals = &report["totals"];
    let filesystem = &report["filesystem"];
    out.push_str(&format!(
        "{}: {} in {} directories; build caches {}; safe to delete {} in {} caches\n",
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
                "safe to delete".to_string()
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
            usage(&big, &[]).bytes,
            dir_bytes,
            "the cache is counted once"
        );
        let text = render_text(&report);
        assert!(
            text.contains("read-only report: nothing was deleted"),
            "{text}"
        );
        assert!(text.contains("safe to delete"), "{text}");
        assert!(text.contains("PR 7 merged"), "{text}");
        assert!(
            big.join("rust/target/debug/artifact").exists(),
            "nothing is deleted"
        );
    }

    #[test]
    fn busy_paths_reads_cwd_and_cargo_target_dir_of_numbered_processes() {
        let proc_root = crate::testsupport::dir("worktree-disk-proc");
        let one = proc_root.path().join("101");
        fs::create_dir_all(&one).unwrap();
        std::os::unix::fs::symlink("/work/a-wt/rust", one.join("cwd")).unwrap();
        fs::write(
            one.join("environ"),
            b"HOME=/h\0CARGO_TARGET_DIR=/work/b-wt/target\0",
        )
        .unwrap();
        fs::create_dir_all(proc_root.path().join("self")).unwrap();
        let mut paths = busy_paths(proc_root.path()).unwrap();
        paths.sort();
        assert_eq!(
            paths,
            [
                PathBuf::from("/work/a-wt/rust"),
                PathBuf::from("/work/b-wt/target")
            ]
        );
        assert!(busy_paths(&proc_root.path().join("missing")).is_err());
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
