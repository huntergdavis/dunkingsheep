//! `dunkingsheep worktrees disk` (t3912): the read-only worktree disk report.
//!
//! The scan and the rules are `ds_core::worktree_disk`. This file supplies what they need from
//! the machine: each directory's branch and origin from `git`, the pull requests of each origin
//! from one `gh pr list` call per repository, and the running processes from `/proc`. Nothing
//! is deleted, and a lookup that fails is reported as unknown.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use ds_core::worktree_disk::{self, PrLink};
use serde_json::Value;

use crate::usage_cmd::{sorted_json, Outcome};

/// The most pull requests read per repository. A branch whose pull request is older than
/// these is reported as having none, which keeps its cache.
const PR_LIMIT: &str = "3000";

/// Branch name to its newest pull request: `(number, state)`.
pub type BranchPrs = HashMap<String, (i64, String)>;
/// The pull requests of one repository, or why they could not be read.
pub type PrLookup = Result<BranchPrs, String>;

/// How long one `git` call may take, and one `gh` call. A call that takes longer is killed
/// and counts as failed, which leaves the pull request state unknown and keeps the cache.
const GIT_TIMEOUT: Duration = Duration::from_secs(10);
const GH_TIMEOUT: Duration = Duration::from_secs(60);

/// Run a command to its end, or kill it at `timeout`. Its output is read on two threads while
/// it runs, so a large answer cannot block it.
pub fn run_bounded(command: &mut Command, timeout: Duration) -> Result<Output, String> {
    use std::io::Read;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("could not be run: {error}"))?;
    let drain = |pipe: Option<Box<dyn Read + Send>>| {
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            if let Some(mut pipe) = pipe {
                let _ = pipe.read_to_end(&mut bytes);
            }
            bytes
        })
    };
    let stdout = drain(
        child
            .stdout
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    let stderr = drain(
        child
            .stderr
            .take()
            .map(|pipe| Box::new(pipe) as Box<dyn Read + Send>),
    );
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("timed out after {} s", timeout.as_secs()));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("could not be waited for: {error}"));
            }
        }
    };
    Ok(Output {
        status,
        stdout: stdout.join().unwrap_or_default(),
        stderr: stderr.join().unwrap_or_default(),
    })
}

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = run_bounded(
        Command::new("git").arg("-C").arg(dir).args(args),
        GIT_TIMEOUT,
    )
    .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
        .filter(|text| !text.is_empty())
}

/// True when `dir` is itself the top of a git checkout. A plain directory inside some other
/// checkout answers git with that checkout's branch and origin; it must not borrow them.
pub fn own_checkout(dir: &Path) -> bool {
    let Some(top) = git(dir, &["rev-parse", "--show-toplevel"]) else {
        return false;
    };
    match (std::fs::canonicalize(top), std::fs::canonicalize(dir)) {
        (Ok(top), Ok(dir)) => top == dir,
        _ => false,
    }
}

/// `owner/repo` of a GitHub remote URL (`https://github.com/o/r.git`, `git@github.com:o/r.git`).
pub fn github_repo(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://github.com/")
        .or_else(|| url.strip_prefix("git@github.com:"))
        .or_else(|| url.strip_prefix("ssh://git@github.com/"))?;
    let rest = rest
        .strip_suffix(".git")
        .unwrap_or(rest)
        .trim_end_matches('/');
    let mut parts = rest.split('/');
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(repo), None) if !owner.is_empty() && !repo.is_empty() => {
            Some(format!("{owner}/{repo}"))
        }
        _ => None,
    }
}

/// Branch name to the newest pull request from that branch of the same repository:
/// `(number, state)` with the state in lower case. Pull requests from forks are left out: a
/// fork's branch name says nothing about this checkout.
pub fn prs_by_branch(listing: &Value) -> BranchPrs {
    let mut found: BranchPrs = HashMap::new();
    for pr in listing.as_array().into_iter().flatten() {
        if pr["isCrossRepository"].as_bool() != Some(false) {
            continue;
        }
        let (Some(branch), Some(number), Some(state)) = (
            pr["headRefName"].as_str(),
            pr["number"].as_i64(),
            pr["state"].as_str(),
        ) else {
            continue;
        };
        let newer = found.get(branch).is_none_or(|(known, _)| number > *known);
        if newer {
            found.insert(branch.to_string(), (number, state.to_lowercase()));
        }
    }
    found
}

fn list_prs(repo: &str) -> PrLookup {
    let output = run_bounded(
        Command::new("gh")
            .args([
                "pr", "list", "--repo", repo, "--state", "all", "--limit", PR_LIMIT,
            ])
            .args(["--json", "number,state,headRefName,isCrossRepository"]),
        GH_TIMEOUT,
    )
    .map_err(|error| format!("gh pr list for {repo} {error}"))?;
    if !output.status.success() {
        let first = String::from_utf8_lossy(&output.stderr)
            .lines()
            .next()
            .unwrap_or("no error text")
            .to_string();
        return Err(format!("gh pr list failed for {repo}: {first}"));
    }
    let listing: Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("gh pr list output for {repo} is not JSON: {error}"))?;
    Ok(prs_by_branch(&listing))
}

/// The pull request of one directory, from its branch and what `prs` knows of its repository.
pub fn link_for(
    branch: Option<String>,
    repo: Option<String>,
    prs: &mut dyn FnMut(&str) -> PrLookup,
) -> PrLink {
    let Some(branch) = branch else {
        return PrLink::unknown("not a git checkout, or no branch is checked out");
    };
    let Some(repo) = repo else {
        return PrLink {
            branch: Some(branch),
            ..PrLink::unknown("the checkout has no GitHub origin")
        };
    };
    match prs(&repo) {
        Err(error) => PrLink {
            branch: Some(branch),
            ..PrLink::unknown(error)
        },
        Ok(known) => match known.get(&branch) {
            Some((number, state)) if matches!(state.as_str(), "open" | "merged" | "closed") => {
                PrLink {
                    branch: Some(branch),
                    pr: Some(*number),
                    state: state.clone(),
                    reason: None,
                }
            }
            Some((number, state)) => PrLink {
                branch: Some(branch),
                pr: Some(*number),
                ..PrLink::unknown(format!("unexpected pull request state {state:?}"))
            },
            None => PrLink {
                branch: Some(branch),
                pr: None,
                state: "none".into(),
                reason: Some(format!("none found for the branch in {repo}")),
            },
        },
    }
}

/// One repository's pull requests and when they were read.
type PrCache = RefCell<HashMap<String, (Instant, PrLookup)>>;

/// The pull request of `dir`, from `git` and `gh`. A repository's pull requests are read once
/// and reused; with `max_age` a reading older than that is taken again, which is what a
/// delete-time check needs.
fn live_link(dir: &Path, cache: &PrCache, max_age: Option<Duration>) -> PrLink {
    if !own_checkout(dir) {
        return PrLink::unknown("not the top of a git checkout");
    }
    let branch = git(dir, &["symbolic-ref", "--short", "-q", "HEAD"]);
    let repo = git(dir, &["remote", "get-url", "origin"]).and_then(|url| github_repo(&url));
    link_for(branch, repo, &mut |repo| {
        let mut cache = cache.borrow_mut();
        let stale = cache
            .get(repo)
            .is_none_or(|(read_at, _)| max_age.is_some_and(|max_age| read_at.elapsed() > max_age));
        if stale {
            cache.insert(repo.to_string(), (Instant::now(), list_prs(repo)));
        }
        cache[repo].1.clone()
    })
}

/// A delete-time check reads a repository's pull requests again when its last reading is
/// older than this.
const RECHECK_PR_MAX_AGE: Duration = Duration::from_secs(30);

fn default_root(root: Option<&str>) -> PathBuf {
    match root.filter(|root| !root.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("workspace")
            .join(".dunk-port-work"),
    }
}

fn wall_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// `worktrees clean [--root DIR] [--apply] [--max-seconds S] [--max-dirs N]` (t3912): the
/// guarded cleanup of build caches. Without `--apply` it only lists what looks eligible. With
/// it, every cache is checked again right before it is deleted, with a fresh reading of its
/// pull request (at most `RECHECK_PR_MAX_AGE` old) and of the running processes. One line per
/// deleted cache goes to standard error as it happens. Exit 1 when a delete failed, 2 when
/// the root cannot be read.
pub fn clean_command(
    root: Option<&str>,
    apply: bool,
    from: Option<&str>,
    max_seconds: Option<f64>,
    max_dirs: Option<i64>,
    json: bool,
) -> Outcome {
    let root = default_root(root);
    let usage = |text: String| Outcome {
        stdout: String::new(),
        stderr: format!("worktrees clean: {text}\n"),
        code: 2,
    };
    // Deleting needs the list of a dry run somebody reviewed; a list without --apply is a
    // mistake too, not a silent dry run.
    let reviewed = match (apply, from.filter(|from| !from.is_empty())) {
        (false, None) => None,
        (false, Some(_)) => return usage("--from is only used with --apply".into()),
        (true, None) => {
            return usage(
                "--apply needs --from FILE, the saved --json output of a reviewed dry run".into(),
            )
        }
        (true, Some(file)) => {
            let listed = std::fs::read_to_string(file)
                .map_err(|error| error.to_string())
                .and_then(|text| ds_core::worktree_clean::reviewed_list(&text, &root));
            match listed {
                Ok(listed) => Some(listed),
                Err(error) => return usage(format!("--from {file}: {error}")),
            }
        }
    };
    let cache = PrCache::default();
    let link = |dir: &Path| live_link(dir, &cache, Some(RECHECK_PR_MAX_AGE));
    let uid = std::fs::metadata("/proc/self")
        .map(|meta| std::os::unix::fs::MetadataExt::uid(&meta))
        .unwrap_or(u32::MAX);
    // Processes that refused inspection, at the scan and at every re-check: the largest count
    // seen is what the result reports, because each deletion rested on that assumption.
    let uninspected = std::cell::Cell::new((0u64, 0u64));
    let busy = || {
        worktree_disk::busy_paths(Path::new("/proc"), &root, uid).map(|scan| {
            let (own, other) = uninspected.get();
            uninspected.set((
                own.max(scan.uninspected_own),
                other.max(scan.uninspected_other),
            ));
            scan.paths
        })
    };
    let budget = worktree_disk::Budget::new(
        max_seconds.unwrap_or(DEFAULT_MAX_SECONDS),
        max_dirs.map_or(DEFAULT_MAX_DIRS, |dirs| dirs.max(0) as usize),
    );
    let mut log = |line: &str| eprintln!("worktrees clean: {line}");
    match ds_core::worktree_clean::clean(
        &root,
        &wall_now,
        &link,
        &busy,
        &budget,
        reviewed.as_deref(),
        &mut log,
    ) {
        Ok(mut result) => Outcome {
            code: i32::from(
                result["failed"]
                    .as_array()
                    .is_some_and(|failed| !failed.is_empty()),
            ),
            stdout: {
                let (own, other) = uninspected.get();
                worktree_disk::note_uninspected(&mut result, own, other);
                if json {
                    format!("{}\n", sorted_json(&result))
                } else {
                    ds_core::worktree_clean::render_text(&result)
                }
            },
            stderr: String::new(),
        },
        Err(error) => Outcome {
            stdout: String::new(),
            stderr: format!("worktrees clean: {error}\n"),
            code: 2,
        },
    }
}

/// The default limits of one run; past either the report is marked partial.
pub const DEFAULT_MAX_SECONDS: f64 = 300.0;
pub const DEFAULT_MAX_DIRS: usize = 2000;

/// `worktrees disk [--root DIR] [--max-seconds S] [--max-dirs N]`, `json` being the global `--json` flag. Exit 2 when the root
/// cannot be read.
pub fn disk_command(
    root: Option<&str>,
    max_seconds: Option<f64>,
    max_dirs: Option<i64>,
    json: bool,
) -> Outcome {
    let root = default_root(root);
    let now = wall_now();
    let cache = PrCache::default();
    let link = |dir: &Path| live_link(dir, &cache, None);
    // This process's own /proc entry is owned by the user the scan runs as.
    let uid = std::fs::metadata("/proc/self")
        .map(|meta| std::os::unix::fs::MetadataExt::uid(&meta))
        .unwrap_or(u32::MAX);
    let scan = worktree_disk::busy_paths(Path::new("/proc"), &root, uid);
    let uninspected = scan
        .as_ref()
        .ok()
        .map(|scan| (scan.uninspected_own, scan.uninspected_other));
    let busy = scan.map(|scan| scan.paths);
    let budget = worktree_disk::Budget::new(
        max_seconds.unwrap_or(DEFAULT_MAX_SECONDS),
        max_dirs.map_or(DEFAULT_MAX_DIRS, |dirs| dirs.max(0) as usize),
    );
    match worktree_disk::report_within(&root, now, &link, &busy, &budget) {
        Ok(mut report) => {
            if let Some((own, other_users)) = uninspected {
                worktree_disk::note_uninspected(&mut report, own, other_users);
            }
            Outcome {
                stdout: if json {
                    format!("{}\n", sorted_json(&report))
                } else {
                    worktree_disk::render_text(&report)
                },
                stderr: String::new(),
                code: 0,
            }
        }
        Err(error) => Outcome {
            stdout: String::new(),
            stderr: format!("worktrees disk: {error}\n"),
            code: 2,
        },
    }
}

/// `worktrees inventory` reads every worktree within this time unless `--max-seconds` says
/// otherwise; past it the report is partial and says how much it left out.
// 120 s left 95 of 324 live worktrees uninspected on 2026-11-15; a full run took about 4 min.
pub const INVENTORY_MAX_SECONDS: f64 = 300.0;
const MAIN_REF: &str = "refs/remotes/origin/main";

/// A read-only git call: `GIT_OPTIONAL_LOCKS=0` stops `git status` refreshing the index, so not
/// even a lock file is written.
fn git_read(dir: &Path, args: &[&str]) -> Option<Output> {
    run_bounded(
        Command::new("git")
            .env("GIT_OPTIONAL_LOCKS", "0")
            .arg("-C")
            .arg(dir)
            .args(args),
        GIT_TIMEOUT,
    )
    .ok()
}

fn git_text(dir: &Path, args: &[&str]) -> Option<String> {
    git_read(dir, args)
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// What git says about one worktree, compared with `main_ref`. No fetch: the main branch is
/// the local copy of the remote ref, as old as the last fetch.
pub fn git_state(dir: &Path, main_ref: &str) -> ds_core::worktree_inventory::GitState {
    use ds_core::worktree_inventory::{count_status, GitState};
    let status = git_text(dir, &["status", "--porcelain", "--untracked-files=normal"]);
    let (changed, untracked) = match status.as_deref().map(count_status) {
        Some((changed, untracked)) => (Some(changed), Some(untracked)),
        None => (None, None),
    };
    // `merge-base --is-ancestor` answers with its exit code: 0 yes, 1 no, anything else unknown.
    let merged =
        git_read(dir, &["merge-base", "--is-ancestor", "HEAD", main_ref]).and_then(|output| {
            match output.status.code() {
                Some(0) => Some(true),
                Some(1) => Some(false),
                _ => None,
            }
        });
    let unpushed = git_text(
        dir,
        &[
            "rev-list",
            "--count",
            "HEAD",
            "--not",
            main_ref,
            "--remotes",
        ],
    )
    .and_then(|text| text.trim().parse().ok());
    let last_author = git_text(dir, &["log", "-1", "--format=%an"])
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());
    GitState {
        merged,
        changed,
        untracked,
        unpushed,
        last_author,
    }
}

/// The inventory for `repo` and `root`, with the machine's readings supplied: everything but
/// the process table, which the caller reads. Exit 2 when the repository's worktrees cannot be
/// listed.
pub fn inventory_for(
    repo: &Path,
    root: &Path,
    panes: &[(String, PathBuf)],
    busy: &Result<Vec<PathBuf>, String>,
    link: &dyn Fn(&Path) -> PrLink,
    max_seconds: f64,
) -> Result<Value, String> {
    let listing = git_text(repo, &["worktree", "list", "--porcelain"])
        .ok_or_else(|| format!("{}: git worktree list failed", repo.display()))?;
    let worktrees = ds_core::worktree_inventory::parse_worktree_list(&listing);
    let git = |dir: &Path| git_state(dir, MAIN_REF);
    let sources = ds_core::worktree_inventory::Sources {
        git: &git,
        link,
        panes,
        busy,
    };
    let budget = worktree_disk::Budget::new(max_seconds, usize::MAX);
    Ok(ds_core::worktree_inventory::inventory(
        root,
        MAIN_REF,
        &worktrees,
        &sources,
        wall_now(),
        &budget,
    ))
}

/// `worktrees inventory [--repo DIR] [--root DIR] [--max-seconds S]` (t4705): the read-only
/// inventory of every worktree of the repository. It runs no git write (no prune, fetch or
/// remove) and deletes nothing. Exit 2 when the repository cannot be read.
pub fn inventory_command(
    repo: Option<&str>,
    root: Option<&str>,
    max_seconds: Option<f64>,
    json: bool,
) -> Outcome {
    let root = default_root(root);
    let repo = match repo.filter(|repo| !repo.is_empty()) {
        Some(repo) => PathBuf::from(repo),
        None => std::env::current_dir().unwrap_or_default(),
    };
    let uid = std::fs::metadata("/proc/self")
        .map(|meta| std::os::unix::fs::MetadataExt::uid(&meta))
        .unwrap_or(u32::MAX);
    let scan = worktree_disk::busy_paths(Path::new("/proc"), &root, uid);
    let busy = scan
        .as_ref()
        .map(|scan| scan.paths.clone())
        .map_err(Clone::clone);
    let panes = ds_core::worktree_inventory::pane_dirs(Path::new("/proc"));
    let cache = PrCache::default();
    let link = |dir: &Path| live_link(dir, &cache, None);
    match inventory_for(
        &repo,
        &root,
        &panes,
        &busy,
        &link,
        max_seconds.unwrap_or(INVENTORY_MAX_SECONDS),
    ) {
        Ok(mut report) => {
            if let Ok(scan) = &scan {
                worktree_disk::note_uninspected(
                    &mut report,
                    scan.uninspected_own,
                    scan.uninspected_other,
                );
            }
            Outcome {
                stdout: if json {
                    format!("{}\n", sorted_json(&report))
                } else {
                    ds_core::worktree_inventory::render_text(&report)
                },
                stderr: String::new(),
                code: 0,
            }
        }
        Err(error) => Outcome {
            stdout: String::new(),
            stderr: format!("worktrees inventory: {error}\n"),
            code: 2,
        },
    }
}

#[cfg(test)]
mod inventory_tests {
    use super::*;
    use serde_json::json;

    fn run(dir: &Path, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Fixture Lane (%42)")
            .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Fixture Lane (%42)")
            .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn tagged_target(dir: &Path) {
        let target = dir.join("rust/target/debug");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("blob"), vec![0u8; 8192]).unwrap();
        std::fs::write(
            dir.join("rust/target/CACHEDIR.TAG"),
            "Signature: 8a477f597d28d172789f06886806bc55\n# created by cargo\n",
        )
        .unwrap();
    }

    /// A repository with a fake `origin/main`, and one worktree per class under a root, plus
    /// one outside it, one deleted, and a plain directory with a cache that is not a worktree.
    #[test]
    fn every_class_a_symlinked_root_a_missing_dir_and_a_pane_in_a_subdirectory() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        std::fs::write(repo.join("file"), "one\n").unwrap();
        // As in the real repository, build caches are ignored, so they do not make a tree dirty.
        std::fs::write(repo.join(".gitignore"), "target/\n").unwrap();
        run(&repo, &["add", "file", ".gitignore"]);
        run(&repo, &["commit", "-q", "-m", "one"]);
        run(&repo, &["update-ref", MAIN_REF, "HEAD"]);
        let real_root = home.path().join("work");
        std::fs::create_dir_all(&real_root).unwrap();
        // The root is given through a symbolic link; worktrees are registered by real path.
        let root = home.path().join("work-link");
        std::os::unix::fs::symlink(&real_root, &root).unwrap();
        let add = |name: &str, base: &Path| {
            let path = base.join(name);
            run(
                &repo,
                &[
                    "worktree",
                    "add",
                    "-q",
                    "-b",
                    name,
                    path.to_str().unwrap(),
                    "main",
                ],
            );
            path
        };
        let merged = add("308-merged", &real_root);
        tagged_target(&merged);
        let unmerged = add("unmerged", &real_root);
        std::fs::write(unmerged.join("file"), "two\n").unwrap();
        run(&unmerged, &["commit", "-q", "-am", "two"]);
        let dirty = add("dirty", &real_root);
        std::fs::write(dirty.join("scratch"), "x").unwrap();
        let busy = add("busy", &real_root);
        std::fs::create_dir_all(busy.join("deep/er")).unwrap();
        let outside = add("outside", home.path());
        let gone = add("gone", &real_root);
        std::fs::remove_dir_all(&gone).unwrap();
        let clone = real_root.join("plain-copy");
        std::fs::create_dir_all(clone.join(".git")).unwrap();
        tagged_target(&clone);

        let panes = vec![("%9".to_string(), busy.join("deep/er"))];
        let link = |_: &Path| PrLink {
            state: "merged".into(),
            ..PrLink::default()
        };
        let report = inventory_for(&repo, &root, &panes, &Ok(vec![]), &link, 60.0).unwrap();
        let class_of = |path: &Path| {
            report["worktrees"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["path"].as_str() == path.to_str())
                .unwrap_or_else(|| panic!("{} not listed: {report:#}", path.display()))
                .clone()
        };
        assert_eq!(class_of(&merged)["class"], "clean-merged");
        assert_eq!(class_of(&merged)["likely_owner"], "%308");
        assert_eq!(
            class_of(&merged)["likely_owner_basis"],
            "directory name prefix"
        );
        assert_eq!(class_of(&unmerged)["class"], "clean-unmerged");
        assert_eq!(class_of(&unmerged)["unpushed_commits"], 1);
        assert_eq!(class_of(&unmerged)["likely_owner"], "%42");
        assert_eq!(class_of(&dirty)["class"], "dirty");
        assert_eq!(class_of(&dirty)["untracked_files"], 1);
        assert_eq!(report["totals"]["dirty_untracked_only"], 1);
        assert_eq!(class_of(&busy)["class"], "in-use");
        assert_eq!(class_of(&busy)["live_panes"], json!(["%9"]));
        assert_eq!(class_of(&outside)["class"], "unmanaged-location");
        assert_eq!(class_of(&gone)["class"], "missing");
        assert_eq!(class_of(&repo)["main_checkout"], true);

        // The merged worktree's cache: tagged, under 12 h old, so the cleaner keeps it for that.
        let target = &class_of(&merged)["targets"][0];
        assert_eq!(target["cargo_tag"], true);
        assert_eq!(target["size_is_lower_bound"], false);
        assert_eq!(
            target["cleaner_keep_reasons"],
            json!(["changed in the last 12 hours"])
        );
        // The plain directory with a cache is reported, not lost.
        assert_eq!(
            report["unregistered"][0]["path"],
            json!(root.join("plain-copy")),
            "listed under the root as given"
        );
        assert_eq!(
            report["unregistered"].as_array().unwrap().len(),
            1,
            "registered worktrees are not repeated"
        );
        assert_eq!(report["unregistered"][0]["kind"], "a separate git clone");
        assert_eq!(report["totals"]["by_class"]["missing"]["worktrees"], 1);
        assert_eq!(report["partial"], false);

        // Read-only: no worktree was pruned and no lock or index change was left behind.
        let listing = git_text(&repo, &["worktree", "list", "--porcelain"]).unwrap();
        assert!(
            listing.contains("gone"),
            "the missing worktree is still registered"
        );
        assert!(!repo.join(".git/index.lock").exists());
    }

    #[test]
    fn a_budget_of_zero_inspects_nothing_and_says_so() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run(&repo, &["init", "-q", "-b", "main"]);
        let link = |_: &Path| PrLink::default();
        let report = inventory_for(&repo, home.path(), &[], &Ok(vec![]), &link, 0.0).unwrap();
        assert_eq!(report["partial"], true);
        assert_eq!(report["not_inspected"]["worktrees"], 1);
    }

    #[test]
    fn a_directory_that_is_not_a_repository_is_a_usage_error() {
        let home = tempfile::tempdir().unwrap();
        let outcome = inventory_command(home.path().to_str(), home.path().to_str(), None, true);
        assert_eq!(outcome.code, 2, "{}", outcome.stderr);
        assert!(outcome.stderr.starts_with("worktrees inventory: "));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn github_repo_reads_the_usual_remote_forms_and_nothing_else() {
        for url in [
            "https://github.com/owner/repo.git",
            "https://github.com/owner/repo",
            "git@github.com:owner/repo.git",
            "ssh://git@github.com/owner/repo.git",
        ] {
            assert_eq!(github_repo(url).as_deref(), Some("owner/repo"), "{url}");
        }
        for url in [
            "https://example.test/owner/repo.git",
            "/srv/git/repo",
            "https://github.com/owner",
        ] {
            assert_eq!(github_repo(url), None, "{url}");
        }
    }

    #[test]
    fn the_newest_same_repository_pull_request_of_a_branch_wins() {
        let known = prs_by_branch(&json!([
            {"number": 5, "state": "CLOSED", "headRefName": "lane/a", "isCrossRepository": false},
            {"number": 9, "state": "MERGED", "headRefName": "lane/a", "isCrossRepository": false},
            {"number": 12, "state": "OPEN", "headRefName": "lane/a", "isCrossRepository": true},
            {"number": 3, "state": "OPEN", "headRefName": "lane/b", "isCrossRepository": false},
            {"number": 4, "state": "OPEN", "headRefName": "lane/c"},
        ]));
        assert_eq!(known.get("lane/a"), Some(&(9, "merged".to_string())));
        assert_eq!(known.get("lane/b"), Some(&(3, "open".to_string())));
        assert_eq!(
            known.get("lane/c"),
            None,
            "an entry that does not say it is same-repository is left out"
        );
    }

    #[test]
    fn a_lookup_that_cannot_answer_is_unknown_and_a_missing_pull_request_is_none() {
        let calls = std::cell::Cell::new(0);
        let mut prs = |_: &str| {
            calls.set(calls.get() + 1);
            Ok(HashMap::from([(
                "lane/a".to_string(),
                (9, "merged".to_string()),
            )]))
        };
        let merged = link_for(Some("lane/a".into()), Some("o/r".into()), &mut prs);
        assert_eq!(
            (merged.pr, merged.state.as_str(), merged.reason),
            (Some(9), "merged", None)
        );
        let none = link_for(Some("lane/z".into()), Some("o/r".into()), &mut prs);
        assert_eq!((none.pr, none.state.as_str()), (None, "none"));
        assert_eq!(calls.get(), 2);

        let no_branch = link_for(None, Some("o/r".into()), &mut prs);
        assert_eq!(no_branch.state, "unknown");
        let no_origin = link_for(Some("lane/a".into()), None, &mut prs);
        assert_eq!(
            (no_origin.state.as_str(), no_origin.branch.as_deref()),
            ("unknown", Some("lane/a"))
        );
        assert_eq!(calls.get(), 2, "no lookup without a branch and an origin");

        let failed = link_for(Some("lane/a".into()), Some("o/r".into()), &mut |_| {
            Err("gh failed".into())
        });
        assert_eq!(
            (failed.state.as_str(), failed.reason.as_deref()),
            ("unknown", Some("gh failed"))
        );
        let odd = link_for(Some("lane/a".into()), Some("o/r".into()), &mut |_| {
            Ok(HashMap::from([(
                "lane/a".to_string(),
                (9, "draft".to_string()),
            )]))
        });
        assert_eq!(odd.state, "unknown");
    }

    #[test]
    fn a_directory_inside_another_checkout_is_not_its_own_checkout() {
        let home = tempfile::tempdir().unwrap();
        let repo = home.path().join("repo");
        let inner = repo.join("scratch");
        std::fs::create_dir_all(&inner).unwrap();
        let made = Command::new("git")
            .arg("init")
            .arg("-q")
            .arg(&repo)
            .status()
            .unwrap();
        assert!(made.success());
        assert!(own_checkout(&repo));
        assert!(
            !own_checkout(&inner),
            "it would answer with the parent's branch"
        );
        assert!(!own_checkout(home.path()), "not in a checkout at all");
    }

    #[test]
    fn a_command_that_outlives_its_timeout_is_killed_and_a_fast_one_returns_its_output() {
        let started = Instant::now();
        let error =
            run_bounded(Command::new("sleep").arg("30"), Duration::from_millis(200)).unwrap_err();
        assert!(error.starts_with("timed out after"), "{error}");
        assert!(started.elapsed() < Duration::from_secs(10));
        let output = run_bounded(
            Command::new("sh").args(["-c", "printf out; printf err >&2; exit 3"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(output.status.code(), Some(3));
        assert_eq!(
            (output.stdout.as_slice(), output.stderr.as_slice()),
            (&b"out"[..], &b"err"[..])
        );
        // More output than a pipe holds must not stall the command.
        let big = run_bounded(
            Command::new("sh").args(["-c", "head -c 300000 /dev/zero"]),
            Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(big.stdout.len(), 300_000);
        assert!(run_bounded(
            &mut Command::new("/nonexistent/program"),
            Duration::from_secs(1)
        )
        .is_err());
    }

    #[test]
    fn clean_without_apply_on_an_unreadable_root_is_a_usage_error_and_deletes_nothing() {
        let outcome = clean_command(
            Some("/nonexistent/worktree/root"),
            false,
            None,
            None,
            None,
            true,
        );
        assert_eq!(outcome.code, 2);
        assert!(
            outcome.stderr.starts_with("worktrees clean: "),
            "{}",
            outcome.stderr
        );
    }

    #[test]
    fn apply_is_refused_without_a_reviewed_dry_run_list_of_the_same_root() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().to_str().unwrap();
        let refused = |apply: bool, from: Option<&str>| {
            let outcome = clean_command(Some(root), apply, from, None, None, true);
            assert_eq!(
                (outcome.code, outcome.stdout.as_str()),
                (2, ""),
                "{}",
                outcome.stderr
            );
            outcome.stderr
        };
        assert!(refused(true, None).contains("--apply needs --from FILE"));
        assert!(refused(false, Some("x.json")).contains("--from is only used with --apply"));
        assert!(refused(true, Some("/nonexistent/dry-run.json"))
            .contains("--from /nonexistent/dry-run.json: "));
        let other = home.path().join("other-root.json");
        std::fs::write(
            &other,
            r#"{"apply": false, "root": "/elsewhere", "would_delete": []}"#,
        )
        .unwrap();
        assert!(refused(true, other.to_str()).contains("it was made for another root (/elsewhere)"));
        let applied = home.path().join("applied.json");
        std::fs::write(
            &applied,
            format!(r#"{{"apply": true, "root": "{root}", "would_delete": []}}"#),
        )
        .unwrap();
        assert!(refused(true, applied.to_str()).contains("it is not the output of a dry run"));
    }

    #[test]
    fn a_root_that_cannot_be_read_is_a_usage_error() {
        let outcome = disk_command(Some("/nonexistent/worktree/root"), None, None, true);
        assert_eq!(outcome.code, 2);
        assert!(
            outcome.stderr.starts_with("worktrees disk: "),
            "{}",
            outcome.stderr
        );
        assert_eq!(outcome.stdout, "");
    }
}
