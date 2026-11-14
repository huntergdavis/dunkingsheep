//! The guarded cleanup of Cargo build caches under the worktree root (t3912).
//!
//! `worktree_disk::report` says which caches look eligible. That is a finding, and it is old by
//! the time anything is deleted. So this module never deletes on the report's word: for every
//! candidate it checks every condition AGAIN, from the machine, immediately before deleting:
//!
//! * the path is a real directory (not a link), directly inside a worktree that is directly
//!   below the root, and it is not the worktree itself and holds no `.git`;
//! * it carries Cargo's own cache tag;
//! * the worktree's pull request, read again, is merged or closed;
//! * nothing in it changed for `UNTOUCHED_S`, and all of it could be read;
//! * the process table, read again, shows no process working in the worktree or building into
//!   the cache, under the same fail-closed rule for processes that refuse inspection.
//!
//! Any condition that fails, or cannot be established, keeps the cache. Only a tagged build
//! cache is ever removed: never source, a stage or release directory, or evidence. Without
//! `apply` nothing is deleted at all.
//!
//! The check and the delete cannot be one atomic step. The cache is first renamed aside (one
//! atomic step), so a build that starts afterwards makes a fresh cache and loses nothing, and
//! the renamed directory is then removed.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::worktree_disk::{self, real, Budget, PrLink, UNTOUCHED_S};

/// How long the re-check may spend reading one cache before it gives up and keeps it.
const RECHECK_WALK_S: f64 = 120.0;

/// Every reason `target` must be kept, established now. Empty means it may be deleted.
///
/// `link` and `busy` must be fresh readings taken for this call, not the report's.
pub fn keep_reasons_now(
    root: &Path,
    dir: &Path,
    target: &Path,
    now: f64,
    link: &PrLink,
    busy: &Result<Vec<PathBuf>, String>,
) -> Vec<String> {
    let mut keep: Vec<String> = Vec::new();
    // Structure first: a path that is not where a build cache lives is never looked at further.
    let is_real_dir =
        |path: &Path| std::fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_dir());
    if !is_real_dir(target) {
        return vec!["it is no longer a real directory".into()];
    }
    if !is_real_dir(dir) {
        return vec!["its worktree is no longer a real directory".into()];
    }
    let (root_real, dir_real, target_real) = (real(root), real(dir), real(target));
    if dir_real.parent() != Some(root_real.as_path()) {
        return vec!["its worktree is not directly below the worktree root".into()];
    }
    if target_real == dir_real || !target_real.starts_with(&dir_real) {
        return vec!["it is not inside its worktree".into()];
    }
    if std::fs::symlink_metadata(target.join(".git")).is_ok() {
        return vec!["it holds a git checkout".into()];
    }
    if !worktree_disk::has_cargo_tag(target) {
        keep.push("no Cargo cache tag: not known to be a build cache".into());
    }
    match link.state.as_str() {
        "merged" | "closed" => {}
        state => keep.push(format!(
            "its pull request is not merged or closed now ({state}{})",
            link.reason
                .as_deref()
                .map(|reason| format!(": {reason}"))
                .unwrap_or_default()
        )),
    }
    let used = worktree_disk::usage(target, &[], &Budget::new(RECHECK_WALK_S, usize::MAX));
    if used.unreadable > 0 {
        keep.push("part of it could not be read, so its age is not known".into());
    }
    if used.truncated {
        keep.push("it could not be read fully in time".into());
    }
    match used.newest {
        Some(newest) if now - newest > UNTOUCHED_S => {}
        Some(_) => keep.push("changed in the last 12 hours".into()),
        None => keep.push("its age could not be read".into()),
    }
    match busy {
        Err(error) => keep.push(format!("running processes could not be read: {error}")),
        Ok(paths) => {
            let working = paths
                .iter()
                .map(|path| real(path))
                .any(|path| path.starts_with(&dir_real) || path.starts_with(&target_real));
            if working {
                keep.push("a running process works in this directory".into());
            }
        }
    }
    keep
}

/// Remove one build cache: rename it aside in one step, then delete the renamed directory.
/// `Err` says how far it got.
pub fn delete_cache(target: &Path) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or_else(|| "it has no parent directory".to_string())?;
    let name = target
        .file_name()
        .ok_or_else(|| "it has no name".to_string())?
        .to_string_lossy();
    let aside = parent.join(format!(".{name}.removing-{}", std::process::id()));
    if std::fs::symlink_metadata(&aside).is_ok() {
        return Err(format!("{} is in the way", aside.display()));
    }
    std::fs::rename(target, &aside).map_err(|error| format!("rename aside failed: {error}"))?;
    std::fs::remove_dir_all(&aside).map_err(|error| {
        format!(
            "renamed to {} but removing it failed: {error}",
            aside.display()
        )
    })
}

/// The cleanup. Scans like the report, then handles every cache the scan found eligible:
/// without `apply` it is listed as `would_delete`; with `apply` it is re-checked with fresh
/// readings (`link` and `busy` are called again for it) and deleted only if every condition
/// still holds. `log` receives one line per deleted cache, with its size.
pub fn clean(
    root: &Path,
    now: &dyn Fn() -> f64,
    link: &dyn Fn(&Path) -> PrLink,
    busy: &dyn Fn() -> Result<Vec<PathBuf>, String>,
    budget: &Budget,
    apply: bool,
    log: &mut dyn FnMut(&str),
) -> Result<Value, String> {
    let report = worktree_disk::report_within(root, now(), link, &busy(), budget)?;
    let (mut deleted, mut would_delete, mut kept, mut failed) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut freed: u64 = 0;
    for dir in report["dirs"].as_array().into_iter().flatten() {
        let Some(dir_path) = dir["path"].as_str().map(PathBuf::from) else {
            continue;
        };
        for target in dir["targets"].as_array().into_iter().flatten() {
            if target["eligible"] != true {
                continue;
            }
            let Some(path) = target["path"].as_str().map(PathBuf::from) else {
                continue;
            };
            let bytes = target["bytes"].as_u64().unwrap_or(0);
            if !apply {
                would_delete.push(json!({"path": path, "bytes": bytes}));
                continue;
            }
            // Fresh readings for this one cache, taken now.
            let reasons =
                keep_reasons_now(root, &dir_path, &path, now(), &link(&dir_path), &busy());
            if !reasons.is_empty() {
                kept.push(json!({"path": path, "reasons": reasons}));
                continue;
            }
            match delete_cache(&path) {
                Ok(()) => {
                    freed += bytes;
                    log(&format!("deleted {} ({bytes} bytes)", path.display()));
                    deleted.push(json!({"path": path, "bytes": bytes}));
                }
                Err(error) => failed.push(json!({"path": path, "error": error})),
            }
        }
    }
    Ok(json!({
        "root": root,
        "apply": apply,
        "partial": report["partial"],
        "partial_reason": report["partial_reason"],
        "skipped_dirs": report["skipped_dirs"],
        "scanned_dirs": report["totals"]["dirs"],
        "deleted": deleted,
        "freed_bytes": freed,
        "would_delete": would_delete,
        "kept_at_recheck": kept,
        "failed": failed,
    }))
}

/// The operator's view of `clean`.
pub fn render_text(result: &Value) -> String {
    let list = |key: &str| result[key].as_array().cloned().unwrap_or_default();
    let mut out = String::new();
    // What "no process works there" rests on, when some processes refused inspection
    // (`worktree_disk::note_uninspected`). Said before anything else, for a dry run and for a
    // run that deleted.
    if let Some(assumes) = result["eligibility_assumes"].as_str() {
        out.push_str(&format!("ASSUMPTION: {assumes}\n"));
    }
    if result["apply"] == true {
        for item in list("deleted") {
            out.push_str(&format!(
                "deleted {} ({} bytes)\n",
                item["path"].as_str().unwrap_or("?"),
                item["bytes"].as_u64().unwrap_or(0)
            ));
        }
        for item in list("kept_at_recheck") {
            let reasons: Vec<&str> = item["reasons"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            out.push_str(&format!(
                "kept {} (re-check at delete time: {})\n",
                item["path"].as_str().unwrap_or("?"),
                reasons.join("; ")
            ));
        }
        for item in list("failed") {
            out.push_str(&format!(
                "FAILED {}: {}\n",
                item["path"].as_str().unwrap_or("?"),
                item["error"].as_str().unwrap_or("?")
            ));
        }
        out.push_str(&format!(
            "deleted {} build caches, {} bytes; kept {} at the re-check; {} failed\n",
            list("deleted").len(),
            result["freed_bytes"].as_u64().unwrap_or(0),
            list("kept_at_recheck").len(),
            list("failed").len(),
        ));
    } else {
        let mut bytes = 0;
        for item in list("would_delete") {
            bytes += item["bytes"].as_u64().unwrap_or(0);
            out.push_str(&format!(
                "would delete {} ({} bytes)\n",
                item["path"].as_str().unwrap_or("?"),
                item["bytes"].as_u64().unwrap_or(0)
            ));
        }
        out.push_str(&format!(
            "dry run: nothing was deleted; {} build caches, {bytes} bytes, look eligible. \
             --apply re-checks each one and deletes those that still qualify\n",
            list("would_delete").len(),
        ));
    }
    if result["partial"] == true {
        out.push_str(&format!(
            "PARTIAL: {}; {} directories were not scanned\n",
            result["partial_reason"]
                .as_str()
                .unwrap_or("a limit was reached"),
            result["skipped_dirs"].as_u64().unwrap_or(0),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
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

    /// A worktree with source, evidence and an old tagged build cache.
    fn worktree(root: &Path, name: &str) -> PathBuf {
        let dir = root.join(name);
        let target = dir.join("rust").join("target");
        fs::create_dir_all(target.join("debug")).unwrap();
        fs::create_dir_all(dir.join(".git")).unwrap();
        fs::write(dir.join("source.rs"), "fn main() {}").unwrap();
        fs::write(dir.join("evidence.log"), "kept").unwrap();
        fs::write(target.join("debug").join("artifact"), vec![0u8; 8192]).unwrap();
        fs::write(target.join("CACHEDIR.TAG"), CARGO_TAG).unwrap();
        for path in [
            target.join("CACHEDIR.TAG"),
            target.join("debug").join("artifact"),
            target.join("debug"),
            target.clone(),
        ] {
            touch(&path, OLD);
        }
        dir
    }

    fn merged(_: &Path) -> PrLink {
        PrLink {
            branch: Some("lane/t1-x".into()),
            pr: Some(7),
            state: "merged".into(),
            reason: None,
        }
    }

    fn idle() -> Result<Vec<PathBuf>, String> {
        Ok(Vec::new())
    }

    fn run(
        root: &Path,
        link: &dyn Fn(&Path) -> PrLink,
        busy: &dyn Fn() -> Result<Vec<PathBuf>, String>,
        apply: bool,
    ) -> (Value, Vec<String>) {
        let mut lines = Vec::new();
        let result = clean(
            root,
            &|| NOW,
            link,
            busy,
            &Budget::unlimited(),
            apply,
            &mut |line| lines.push(line.to_string()),
        )
        .unwrap();
        (result, lines)
    }

    #[test]
    fn a_dry_run_lists_the_cache_and_deletes_nothing() {
        let home = crate::testsupport::dir("worktree-clean-dry");
        let dir = worktree(home.path(), "a-wt");
        let (result, lines) = run(home.path(), &merged, &idle, false);
        assert_eq!(result["apply"], false);
        assert_eq!(result["would_delete"].as_array().unwrap().len(), 1);
        assert_eq!(result["deleted"], json!([]));
        assert!(lines.is_empty());
        assert!(dir.join("rust/target/debug/artifact").exists());
        assert!(render_text(&result).contains("dry run: nothing was deleted; 1 build caches"));
        assert!(!render_text(&result).contains("ASSUMPTION"));
        // With processes that refused inspection, the dry run says what it assumes.
        let mut assumed = result.clone();
        worktree_disk::note_uninspected(&mut assumed, 11, 227);
        let text = render_text(&assumed);
        assert!(
            text.starts_with("ASSUMPTION: 11 processes of this user and 227 of other users could not be inspected"),
            "{text}"
        );
        assert_eq!(
            assumed["uninspected_processes"],
            json!({"own": 11, "other_users": 227})
        );
    }

    #[test]
    fn apply_deletes_only_the_tagged_cache_and_logs_it_with_its_size() {
        let home = crate::testsupport::dir("worktree-clean-apply");
        let dir = worktree(home.path(), "a-wt");
        // An untagged `target`, and a sibling worktree whose pull request is open.
        fs::create_dir_all(dir.join("other/target")).unwrap();
        fs::write(dir.join("other/target/keep"), "x").unwrap();
        let open_dir = worktree(home.path(), "b-wt");
        let link = |path: &Path| {
            if path.ends_with("b-wt") {
                PrLink {
                    state: "open".into(),
                    ..merged(path)
                }
            } else {
                merged(path)
            }
        };
        let (result, lines) = run(home.path(), &link, &idle, true);
        let target = dir.join("rust/target");
        assert!(!target.exists(), "the cache is gone");
        assert_eq!(result["deleted"].as_array().unwrap().len(), 1, "{result}");
        let bytes = result["deleted"][0]["bytes"].as_u64().unwrap();
        assert!(bytes >= 8192);
        assert_eq!(result["freed_bytes"], bytes);
        assert_eq!(
            lines,
            [format!("deleted {} ({bytes} bytes)", target.display())]
        );
        // Everything that is not that cache is still there.
        for kept in [
            dir.join("source.rs"),
            dir.join("evidence.log"),
            dir.join(".git"),
            dir.join("rust"),
            dir.join("other/target/keep"),
            open_dir.join("rust/target/debug/artifact"),
        ] {
            assert!(kept.exists(), "{}", kept.display());
        }
        let leftovers: Vec<_> = fs::read_dir(dir.join("rust")).unwrap().flatten().collect();
        assert!(leftovers.is_empty(), "no renamed directory is left behind");
        assert!(render_text(&result).contains("deleted 1 build caches"));
        // And a run that deleted says the same.
        let mut assumed = result.clone();
        worktree_disk::note_uninspected(&mut assumed, 0, 3);
        let text = render_text(&assumed);
        assert!(
            text.starts_with("ASSUMPTION: 0 processes of this user and 3 of other users"),
            "{text}"
        );
        assert!(text.contains("deleted 1 build caches"), "{text}");
        assert!(assumed["eligibility_assumes"].is_string());
    }

    #[test]
    fn a_cache_that_stopped_qualifying_after_the_scan_is_kept() {
        // Each case changes one thing between the scan (first reading) and the re-check.
        let case = |change: &dyn Fn(&Path), link_second: Option<&str>, busy_second: bool| {
            let home = crate::testsupport::dir("worktree-clean-recheck");
            let dir = worktree(home.path(), "a-wt");
            let links = Cell::new(0);
            let scans = Cell::new(0);
            let link = |path: &Path| {
                links.set(links.get() + 1);
                if links.get() > 1 {
                    change(&dir);
                    if let Some(state) = link_second {
                        return PrLink {
                            state: state.into(),
                            reason: (state == "unknown").then(|| "gh timed out".to_string()),
                            ..merged(path)
                        };
                    }
                }
                merged(path)
            };
            let busy = || {
                scans.set(scans.get() + 1);
                if busy_second && scans.get() > 1 {
                    return Ok(vec![dir.join("rust")]);
                }
                idle()
            };
            let (result, lines) = run(home.path(), &link, &busy, true);
            assert_eq!(result["deleted"], json!([]), "{result}");
            assert!(lines.is_empty());
            assert!(dir.join("rust/target/debug/artifact").exists(), "{result}");
            assert_eq!(
                (links.get(), scans.get()),
                (2, 2),
                "fresh readings were taken"
            );
            result["kept_at_recheck"][0]["reasons"].clone()
        };
        let nothing = |_: &Path| {};
        assert_eq!(
            case(&nothing, Some("open"), false),
            json!(["its pull request is not merged or closed now (open)"])
        );
        assert_eq!(
            case(&nothing, Some("unknown"), false),
            json!(["its pull request is not merged or closed now (unknown: gh timed out)"])
        );
        assert_eq!(
            case(&nothing, None, true),
            json!(["a running process works in this directory"])
        );
        assert_eq!(
            case(
                &|dir| fs::remove_file(dir.join("rust/target/CACHEDIR.TAG")).unwrap(),
                None,
                false
            ),
            // Removing the tag also changes the cache, which is a second reason.
            json!([
                "no Cargo cache tag: not known to be a build cache",
                "changed in the last 12 hours"
            ])
        );
        assert_eq!(
            case(
                &|dir| fs::write(dir.join("rust/target/debug/new"), "x").unwrap(),
                None,
                false
            ),
            json!(["changed in the last 12 hours"])
        );
    }

    #[test]
    fn the_recheck_refuses_paths_that_are_not_a_cache_inside_a_worktree_below_the_root() {
        let home = crate::testsupport::dir("worktree-clean-structure");
        let dir = worktree(home.path(), "a-wt");
        let target = dir.join("rust/target");
        let check = |root: &Path, dir: &Path, target: &Path| {
            keep_reasons_now(root, dir, target, NOW, &merged(dir), &idle())
        };
        assert_eq!(check(home.path(), &dir, &target), Vec::<String>::new());
        // The worktree itself, a path outside it, and a root that is not the worktree's parent.
        assert_eq!(
            check(home.path(), &dir, &dir),
            ["it is not inside its worktree"]
        );
        let outside = worktree(home.path(), "b-wt").join("rust/target");
        assert_eq!(
            check(home.path(), &dir, &outside),
            ["it is not inside its worktree"]
        );
        assert_eq!(
            check(&dir, &dir, &target),
            ["its worktree is not directly below the worktree root"]
        );
        // A link to a cache is not a cache.
        let link = dir.join("linked-target");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert_eq!(
            check(home.path(), &dir, &link),
            ["it is no longer a real directory"]
        );
        // A directory that holds a checkout is never a cache, tag or not.
        fs::create_dir_all(target.join(".git")).unwrap();
        assert_eq!(
            check(home.path(), &dir, &target),
            ["it holds a git checkout"]
        );
        assert!(target.join("debug/artifact").exists());
    }

    #[test]
    fn an_unreadable_process_table_at_the_recheck_keeps_the_cache() {
        let home = crate::testsupport::dir("worktree-clean-proc");
        let dir = worktree(home.path(), "a-wt");
        let scans = Cell::new(0);
        let busy = || {
            scans.set(scans.get() + 1);
            if scans.get() > 1 {
                Err("process 7 (cargo) is a build tool and cannot be inspected".to_string())
            } else {
                idle()
            }
        };
        let (result, _) = run(home.path(), &merged, &busy, true);
        assert_eq!(result["deleted"], json!([]));
        assert!(dir.join("rust/target").exists());
        assert_eq!(
            result["kept_at_recheck"][0]["reasons"],
            json!(["running processes could not be read: process 7 (cargo) is a build tool and cannot be inspected"])
        );
    }
}
