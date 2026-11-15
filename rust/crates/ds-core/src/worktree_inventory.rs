//! The read-only worktree inventory (t4705, slice 1 of t3243): every git worktree of one
//! repository, and every directory under the worktree root that holds a build cache without
//! being a registered worktree.
//!
//! For each worktree it reports the branch or detached head, whether the head is already on
//! the main branch, uncommitted and untracked files, commits that are neither on the main branch
//! nor on any remote, the live panes working inside it, its build caches with their size and tag,
//! and every reason `worktrees clean` would keep each cache (the same rules as `worktrees disk`,
//! `worktree_disk::cache_keep_reasons`). It then gives the worktree one class:
//!
//! * `missing`: the registered directory is gone;
//! * `in-use`: a live pane or a running process works inside it;
//! * `unknown`: git could not answer for it;
//! * `dirty`: uncommitted or untracked files;
//! * `unmanaged-location`: clean, but not under the worktree root;
//! * `clean-merged`: clean, and its head is on the main branch;
//! * `clean-unmerged`: clean, with commits that are not on the main branch.
//!
//! The owner a worktree is listed under is a GUESS from its directory name or last commit author
//! (`likely_owner`). It is never authority for anything.
//!
//! Nothing here writes, deletes or moves. A class is a finding of this report, not a licence to
//! clean anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::worktree_disk::{self, real, Budget, PrLink};

/// One entry of `git worktree list --porcelain`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Registered {
    pub path: PathBuf,
    pub head: Option<String>,
    /// The branch without `refs/heads/`; `None` for a detached head.
    pub branch: Option<String>,
    pub bare: bool,
    /// git itself says the directory is gone.
    pub prunable: bool,
}

/// Parse `git worktree list --porcelain`: blocks separated by a blank line.
pub fn parse_worktree_list(porcelain: &str) -> Vec<Registered> {
    let mut found = Vec::new();
    let mut current: Option<Registered> = None;
    for line in porcelain.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            found.extend(current.take());
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            found.extend(current.take());
            current = Some(Registered {
                path: PathBuf::from(value),
                ..Registered::default()
            });
            continue;
        }
        let Some(entry) = current.as_mut() else {
            continue;
        };
        match key {
            "HEAD" => entry.head = Some(value.to_string()),
            "branch" => {
                entry.branch = Some(
                    value
                        .strip_prefix("refs/heads/")
                        .unwrap_or(value)
                        .to_string(),
                )
            }
            "bare" => entry.bare = true,
            "prunable" => entry.prunable = true,
            _ => {}
        }
    }
    found
}

/// What git says about one worktree. `None` means git could not answer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitState {
    /// The head is an ancestor of (or equal to) the main branch.
    pub merged: Option<bool>,
    /// Changed tracked files (staged or not).
    pub changed: Option<u64>,
    pub untracked: Option<u64>,
    /// Commits reachable from the head that are on neither the main branch nor any remote.
    pub unpushed: Option<u64>,
    /// The author name of the newest commit.
    pub last_author: Option<String>,
}

/// Count the lines of `git status --porcelain`: changed tracked files, untracked files.
pub fn count_status(porcelain: &str) -> (u64, u64) {
    porcelain
        .lines()
        .filter(|line| !line.is_empty())
        .fold((0, 0), |(changed, untracked), line| {
            if line.starts_with("??") {
                (changed, untracked + 1)
            } else {
                (changed + 1, untracked)
            }
        })
}

/// The pane each readable process runs in (its `TMUX_PANE`), with its working directory.
/// Processes that refuse inspection are left out: this answers "which pane", while being in use
/// at all is `busy_paths`'s fail-closed question.
pub fn pane_dirs(proc_root: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return found;
    };
    for entry in entries.flatten() {
        let numbered = entry
            .file_name()
            .to_str()
            .is_some_and(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()));
        if !numbered {
            continue;
        }
        let process = entry.path();
        let (Ok(cwd), Ok(environ)) = (
            std::fs::read_link(process.join("cwd")),
            std::fs::read(process.join("environ")),
        ) else {
            continue;
        };
        let pane = environ
            .split(|byte| *byte == 0)
            .find_map(|variable| variable.strip_prefix(b"TMUX_PANE="))
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .filter(|pane| !pane.is_empty());
        if let Some(pane) = pane {
            found.push((pane, cwd));
        }
    }
    found.sort();
    found.dedup();
    found
}

/// A GUESS at the lane a worktree belongs to: a `NNN-` directory name prefix (`%NNN`), else a
/// `(%NNN)` in the last commit's author name. Returns the guess and what it rests on.
pub fn likely_owner(dir_name: &str, last_author: Option<&str>) -> Option<(String, &'static str)> {
    let digits: String = dir_name.chars().take_while(char::is_ascii_digit).collect();
    // Lanes name worktrees `NNN-...` after their pane; a four-digit prefix is far more often a
    // year than a pane, so only one to three digits count.
    if (1..=3).contains(&digits.len()) && dir_name[digits.len()..].starts_with('-') {
        return Some((format!("%{digits}"), "directory name prefix"));
    }
    let author = last_author?;
    let start = author.find("(%")?;
    let digits: String = author[start + 2..]
        .chars()
        .take_while(char::is_ascii_digit)
        .collect();
    (!digits.is_empty() && author[start + 2 + digits.len()..].starts_with(')'))
        .then(|| (format!("%{digits}"), "last commit author"))
}

/// The class of one worktree, from the facts above (see the module documentation).
pub fn classify(exists: bool, in_use: bool, git: &GitState, under_root: bool) -> &'static str {
    if !exists {
        return "missing";
    }
    if in_use {
        return "in-use";
    }
    let (Some(changed), Some(untracked), Some(merged)) = (git.changed, git.untracked, git.merged)
    else {
        return "unknown";
    };
    if changed + untracked > 0 {
        return "dirty";
    }
    if !under_root {
        return "unmanaged-location";
    }
    if merged {
        "clean-merged"
    } else {
        "clean-unmerged"
    }
}

/// How long one build cache's size may take to read; past it the size is a lower bound.
pub const TARGET_SIZE_S: f64 = 20.0;

/// Everything the inventory reads from the machine, so tests can supply it.
pub struct Sources<'a> {
    pub git: &'a dyn Fn(&Path) -> GitState,
    /// The pull request of a worktree directly below the root (what the cleaner reads).
    pub link: &'a dyn Fn(&Path) -> PrLink,
    pub panes: &'a [(String, PathBuf)],
    /// `busy_paths`: where running processes work and build, or why it could not be read.
    pub busy: &'a Result<Vec<PathBuf>, String>,
}

fn size_of(target: &Path, budget: &Budget) -> worktree_disk::Usage {
    let remaining = budget.max.saturating_sub(budget.elapsed()).as_secs_f64();
    worktree_disk::usage(
        target,
        &[],
        &Budget::new(remaining.min(TARGET_SIZE_S), usize::MAX),
    )
}

/// The caches below `dir`, each with its size and the cleaner's keep reasons.
fn caches(
    root_real: &Path,
    dir: &Path,
    sources: &Sources,
    busy_real: &Result<Vec<PathBuf>, &str>,
    now: f64,
    budget: &Budget,
    pr: &mut Option<PrLink>,
) -> Vec<Value> {
    let dir_real = real(dir);
    let directly_below = dir_real.parent() == Some(root_real);
    let mut found = Vec::new();
    for (target, tagged) in worktree_disk::find_targets(dir) {
        let used = size_of(&target, budget);
        let lower_bound = used.truncated || used.unreadable > 0;
        let keep = if directly_below {
            let link = pr.get_or_insert_with(|| (sources.link)(dir));
            let target_real = real(&target);
            let busy_here = match busy_real {
                Err(error) => Err(*error),
                Ok(paths) => Ok(paths
                    .iter()
                    .any(|path| path.starts_with(&dir_real) || path.starts_with(&target_real))),
            };
            worktree_disk::cache_keep_reasons(tagged, &used, link, now, busy_here)
        } else {
            vec!["not directly below the worktree root: the cleaner never looks there".into()]
        };
        found.push(json!({
            "path": target,
            "bytes": used.bytes,
            "size_is_lower_bound": lower_bound,
            "cargo_tag": tagged,
            "newest_mtime": used.newest,
            "age_s": used.newest.map(|at| (now - at).max(0.0).floor() as i64),
            "cleaner_keep_reasons": keep,
        }));
    }
    found
}

/// The short form of a keep reason, for totals: the text before its first colon.
fn reason_key(reason: &str) -> String {
    reason
        .split(':')
        .next()
        .unwrap_or(reason)
        .trim()
        .to_string()
}

/// The inventory of `worktrees` (from `parse_worktree_list`) and of the unregistered directories
/// directly below `root` that hold a build cache. `main_branch` names the ref heads are
/// compared with. Stops at the budget: worktrees past it are counted, not inspected.
pub fn inventory(
    root: &Path,
    main_branch: &str,
    worktrees: &[Registered],
    sources: &Sources,
    now: f64,
    budget: &Budget,
) -> Value {
    let root_real = real(root);
    let busy_real: Result<Vec<PathBuf>, &str> = match sources.busy {
        Ok(paths) => Ok(paths.iter().map(|path| real(path)).collect()),
        Err(error) => Err(error.as_str()),
    };
    let panes: Vec<(&str, PathBuf)> = sources
        .panes
        .iter()
        .map(|(pane, cwd)| (pane.as_str(), real(cwd)))
        .collect();
    let mut rows = Vec::new();
    let mut registered_real = Vec::new();
    let mut not_inspected = 0usize;
    for (index, worktree) in worktrees.iter().enumerate() {
        let path_real = real(&worktree.path);
        registered_real.push(path_real.clone());
        if worktree.bare {
            continue;
        }
        if budget.spent() {
            not_inspected += 1;
            continue;
        }
        let exists = !worktree.prunable
            && std::fs::symlink_metadata(&worktree.path).is_ok_and(|meta| {
                meta.is_dir() || (meta.file_type().is_symlink() && worktree.path.is_dir())
            });
        let under_root = path_real.starts_with(&root_real) && path_real != root_real;
        let (git, targets, live_panes, process_inside) = if exists {
            let git = (sources.git)(&worktree.path);
            let mut pr = None;
            let targets = caches(
                &root_real,
                &worktree.path,
                sources,
                &busy_real,
                now,
                budget,
                &mut pr,
            );
            let live: Vec<&str> = panes
                .iter()
                .filter(|(_, cwd)| cwd.starts_with(&path_real))
                .map(|(pane, _)| *pane)
                .collect();
            let inside = busy_real
                .as_ref()
                .ok()
                .map(|paths| paths.iter().any(|path| path.starts_with(&path_real)));
            (git, targets, live, inside)
        } else {
            (GitState::default(), Vec::new(), Vec::new(), Some(false))
        };
        let in_use = !live_panes.is_empty() || process_inside == Some(true);
        let class = classify(exists, in_use, &git, under_root);
        let name = worktree
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let owner = likely_owner(name, git.last_author.as_deref());
        rows.push(json!({
            "path": worktree.path,
            "main_checkout": index == 0,
            "class": class,
            "branch": worktree.branch,
            "detached": worktree.branch.is_none(),
            "head": worktree.head,
            "exists": exists,
            "under_root": under_root,
            "merged_into_main": git.merged,
            "changed_files": git.changed,
            "untracked_files": git.untracked,
            "unpushed_commits": git.unpushed,
            "live_panes": live_panes,
            "process_inside": process_inside,
            "likely_owner": owner.as_ref().map(|(owner, _)| owner),
            "likely_owner_basis": owner.as_ref().map(|(_, basis)| *basis),
            "targets": targets,
        }));
    }

    // Directories directly below the root that hold a build cache but are not worktrees of this
    // repository: plain clones, other repositories' worktrees, copied trees.
    let mut unregistered = Vec::new();
    let mut children: Vec<PathBuf> = std::fs::read_dir(root)
        .map(|entries| {
            entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.is_dir()))
                .collect()
        })
        .unwrap_or_default();
    children.sort();
    let mut unregistered_not_inspected = 0usize;
    for dir in children {
        if registered_real.contains(&real(&dir)) {
            continue;
        }
        if budget.spent() {
            unregistered_not_inspected += 1;
            continue;
        }
        let mut pr = None;
        let targets = caches(&root_real, &dir, sources, &busy_real, now, budget, &mut pr);
        if targets.is_empty() {
            continue;
        }
        let kind = match std::fs::symlink_metadata(dir.join(".git")) {
            Ok(meta) if meta.is_dir() => "a separate git clone",
            Ok(_) => "a worktree of another repository",
            Err(_) => "not a git checkout",
        };
        unregistered.push(json!({"path": dir, "kind": kind, "targets": targets}));
    }

    // Totals: per class, and per reason the cleaner keeps a cache, in bytes.
    let mut by_class: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    let mut by_reason: BTreeMap<String, (u64, u64)> = BTreeMap::new();
    let mut lower_bound = false;
    let mut add_targets = |targets: &Value| {
        let mut bytes = 0;
        for target in targets.as_array().into_iter().flatten() {
            let size = target["bytes"].as_u64().unwrap_or(0);
            bytes += size;
            lower_bound |= target["size_is_lower_bound"] == true;
            let reasons = target["cleaner_keep_reasons"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            let keys: Vec<String> = if reasons.is_empty() {
                vec!["eligible for the cleaner".into()]
            } else {
                reasons
                    .iter()
                    .filter_map(Value::as_str)
                    .map(reason_key)
                    .collect()
            };
            for key in keys {
                let entry = by_reason.entry(key).or_default();
                entry.0 += 1;
                entry.1 += size;
            }
        }
        bytes
    };
    for row in &rows {
        let bytes = add_targets(&row["targets"]);
        let entry = by_class
            .entry(class_static(row["class"].as_str().unwrap_or("unknown")))
            .or_default();
        entry.0 += 1;
        entry.1 += bytes;
    }
    let mut unregistered_bytes = 0;
    for dir in &unregistered {
        unregistered_bytes += add_targets(&dir["targets"]);
    }
    let partial = not_inspected + unregistered_not_inspected > 0;
    json!({
        "root": root,
        "main_branch": main_branch,
        "generated_at": now,
        "read_only": true,
        "note": "likely_owner is a guess from a directory name or commit author, never authority; a class is a finding, not a licence to clean",
        "partial": partial,
        "not_inspected": {"worktrees": not_inspected, "unregistered_dirs": unregistered_not_inspected},
        "sizes_are_lower_bounds": lower_bound,
        "totals": {
            "worktrees": rows.len(),
            "by_class": by_class.iter().map(|(class, (count, bytes))| {
                ((*class).to_string(), json!({"worktrees": count, "target_bytes": bytes}))
            }).collect::<serde_json::Map<_, _>>(),
            "cleaner_keep_reasons": by_reason.iter().map(|(reason, (count, bytes))| {
                (reason.clone(), json!({"targets": count, "bytes": bytes}))
            }).collect::<serde_json::Map<_, _>>(),
            "dirty_untracked_only": rows
                .iter()
                .filter(|row| row["class"] == "dirty" && row["changed_files"] == 0)
                .count(),
            "unregistered_dirs": unregistered.len(),
            "unregistered_target_bytes": unregistered_bytes,
        },
        "worktrees": rows,
        "unregistered": unregistered,
    })
}

const CLASSES: [&str; 7] = [
    "missing",
    "in-use",
    "unknown",
    "dirty",
    "unmanaged-location",
    "clean-merged",
    "clean-unmerged",
];

fn class_static(class: &str) -> &'static str {
    CLASSES
        .iter()
        .find(|known| **known == class)
        .copied()
        .unwrap_or("unknown")
}

fn size_text(bytes: u64) -> String {
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gib >= 1.0 {
        format!("{gib:.1} GiB")
    } else {
        format!("{:.0} MiB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// The operator's view: totals per class and per keep reason, then the unregistered caches.
pub fn render_text(report: &Value) -> String {
    let totals = &report["totals"];
    let bound = if report["sizes_are_lower_bounds"] == true {
        " (some sizes are lower bounds: their scan hit the time cap)"
    } else {
        ""
    };
    let mut out = format!(
        "{}: {} worktrees of the repository; main branch {}{}\n",
        report["root"].as_str().unwrap_or("?"),
        totals["worktrees"].as_u64().unwrap_or(0),
        report["main_branch"].as_str().unwrap_or("?"),
        bound,
    );
    if report["partial"] == true {
        out.push_str(&format!(
            "PARTIAL: the time limit was reached; not inspected: {} worktrees, {} other directories\n",
            report["not_inspected"]["worktrees"].as_u64().unwrap_or(0),
            report["not_inspected"]["unregistered_dirs"].as_u64().unwrap_or(0),
        ));
    }
    out.push_str("by class (worktrees, build caches):\n");
    for (class, value) in totals["by_class"].as_object().into_iter().flatten() {
        out.push_str(&format!(
            "  {class:<20} {:>4}  {}\n",
            value["worktrees"].as_u64().unwrap_or(0),
            size_text(value["target_bytes"].as_u64().unwrap_or(0)),
        ));
    }
    out.push_str(&format!(
        "  ({} dirty worktrees have only untracked files)\n",
        totals["dirty_untracked_only"].as_u64().unwrap_or(0)
    ));
    out.push_str("why the cleaner keeps each cache (a cache can have several reasons):\n");
    for (reason, value) in totals["cleaner_keep_reasons"]
        .as_object()
        .into_iter()
        .flatten()
    {
        out.push_str(&format!(
            "  {:>4} caches  {:>9}  {reason}\n",
            value["targets"].as_u64().unwrap_or(0),
            size_text(value["bytes"].as_u64().unwrap_or(0)),
        ));
    }
    let unregistered = report["unregistered"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    if !unregistered.is_empty() {
        out.push_str(&format!(
            "not worktrees of this repository, with build caches ({}): \n",
            size_text(totals["unregistered_target_bytes"].as_u64().unwrap_or(0))
        ));
        for dir in unregistered {
            out.push_str(&format!(
                "  {} ({})\n",
                dir["path"].as_str().unwrap_or("?"),
                dir["kind"].as_str().unwrap_or("?")
            ));
        }
    }
    out.push_str("likely owners are guesses; nothing was changed.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_entries_keep_branch_detached_bare_and_prunable() {
        let parsed = parse_worktree_list(
            "worktree /r\nHEAD aaa\nbranch refs/heads/main\n\n\
             worktree /w/one\nHEAD bbb\ndetached\n\n\
             worktree /w/gone\nHEAD ccc\nbranch refs/heads/lane/t1\nprunable gitdir file points to non-existent location\n\n\
             worktree /bare\nbare\n",
        );
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed[0].branch.as_deref(), Some("main"));
        assert_eq!(
            (parsed[1].branch.clone(), parsed[1].head.as_deref()),
            (None, Some("bbb"))
        );
        assert!(parsed[2].prunable && parsed[2].branch.as_deref() == Some("lane/t1"));
        assert!(parsed[3].bare);
    }

    #[test]
    fn status_lines_count_changed_and_untracked_apart() {
        assert_eq!(count_status(" M a\nM  b\n?? c\n?? d\n?? e\n"), (2, 3));
        assert_eq!(count_status(""), (0, 0));
    }

    #[test]
    fn the_owner_is_a_guess_from_a_name_prefix_or_an_author_and_says_which() {
        assert_eq!(
            likely_owner("308-t4705-wt", Some("Someone (%152)")),
            Some(("%308".into(), "directory name prefix"))
        );
        assert_eq!(
            likely_owner("t4648-task-backlog", Some("DS Fable Writer (%308)")),
            Some(("%308".into(), "last commit author"))
        );
        assert_eq!(likely_owner("t1-x", Some("A Person")), None);
        assert_eq!(
            likely_owner("2026-notes", None),
            None,
            "a date is not a pane"
        );
        assert_eq!(likely_owner("x", Some("bad (%12x)")), None);
    }

    #[test]
    fn classes_follow_the_documented_order() {
        let clean = |merged| GitState {
            merged: Some(merged),
            changed: Some(0),
            untracked: Some(0),
            unpushed: Some(0),
            last_author: None,
        };
        assert_eq!(classify(false, true, &clean(true), true), "missing");
        assert_eq!(classify(true, true, &clean(true), true), "in-use");
        assert_eq!(classify(true, false, &GitState::default(), true), "unknown");
        let dirty = GitState {
            untracked: Some(1),
            ..clean(true)
        };
        assert_eq!(classify(true, false, &dirty, false), "dirty");
        assert_eq!(
            classify(true, false, &clean(true), false),
            "unmanaged-location"
        );
        assert_eq!(classify(true, false, &clean(true), true), "clean-merged");
        assert_eq!(classify(true, false, &clean(false), true), "clean-unmerged");
    }

    #[test]
    fn panes_come_from_readable_processes_with_a_tmux_pane() {
        let proc_root = crate::testsupport::dir("worktree-inventory-proc");
        let make = |pid: &str, cwd: &str, environ: &[u8]| {
            let dir = proc_root.path().join(pid);
            std::fs::create_dir_all(&dir).unwrap();
            std::os::unix::fs::symlink(cwd, dir.join("cwd")).unwrap();
            std::fs::write(dir.join("environ"), environ).unwrap();
        };
        make("10", "/w/a/sub", b"HOME=/h\0TMUX_PANE=%7\0");
        make("11", "/w/b", b"HOME=/h\0");
        make("12", "/w/c", b"TMUX_PANE=\0");
        std::fs::create_dir_all(proc_root.path().join("self")).unwrap();
        assert_eq!(
            pane_dirs(proc_root.path()),
            vec![("%7".to_string(), PathBuf::from("/w/a/sub"))]
        );
    }
}
