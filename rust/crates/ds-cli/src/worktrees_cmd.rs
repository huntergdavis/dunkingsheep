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
    max_seconds: Option<f64>,
    max_dirs: Option<i64>,
    json: bool,
) -> Outcome {
    let root = default_root(root);
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
    match ds_core::worktree_clean::clean(&root, &wall_now, &link, &busy, &budget, apply, &mut log) {
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
        let outcome = clean_command(Some("/nonexistent/worktree/root"), false, None, None, true);
        assert_eq!(outcome.code, 2);
        assert!(
            outcome.stderr.starts_with("worktrees clean: "),
            "{}",
            outcome.stderr
        );
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
