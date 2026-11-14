//! `dunkingsheep worktrees disk` (t3912): the read-only worktree disk report.
//!
//! The scan and the rules are `ds_core::worktree_disk`. This file supplies what they need from
//! the machine: each directory's branch and origin from `git`, the pull requests of each origin
//! from one `gh pr list` call per repository, and the running processes from `/proc`. Nothing
//! is deleted, and a lookup that fails is reported as unknown.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;

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

fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
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
    let output = Command::new("gh")
        .args([
            "pr", "list", "--repo", repo, "--state", "all", "--limit", PR_LIMIT,
        ])
        .args(["--json", "number,state,headRefName,isCrossRepository"])
        .output()
        .map_err(|error| format!("gh could not be run: {error}"))?;
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
    let root = match root.filter(|root| !root.is_empty()) {
        Some(root) => PathBuf::from(root),
        None => PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
            .join("workspace")
            .join(".dunk-port-work"),
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let cache: RefCell<HashMap<String, PrLookup>> = RefCell::new(HashMap::new());
    let link = |dir: &Path| {
        if !own_checkout(dir) {
            return PrLink::unknown("not the top of a git checkout");
        }
        let branch = git(dir, &["symbolic-ref", "--short", "-q", "HEAD"]);
        let repo = git(dir, &["remote", "get-url", "origin"]).and_then(|url| github_repo(&url));
        link_for(branch, repo, &mut |repo| {
            cache
                .borrow_mut()
                .entry(repo.to_string())
                .or_insert_with(|| list_prs(repo))
                .clone()
        })
    };
    // This process's own /proc entry is owned by the user the scan runs as.
    let uid = std::fs::metadata("/proc/self")
        .map(|meta| std::os::unix::fs::MetadataExt::uid(&meta))
        .unwrap_or(u32::MAX);
    let scan = worktree_disk::busy_paths(Path::new("/proc"), &root, uid);
    let uninspected = scan.as_ref().ok().map(|scan| {
        serde_json::json!({"own": scan.uninspected_own, "other_users": scan.uninspected_other})
    });
    let busy = scan.map(|scan| scan.paths);
    let budget = worktree_disk::Budget::new(
        max_seconds.unwrap_or(DEFAULT_MAX_SECONDS),
        max_dirs.map_or(DEFAULT_MAX_DIRS, |dirs| dirs.max(0) as usize),
    );
    match worktree_disk::report_within(&root, now, &link, &busy, &budget) {
        Ok(mut report) => Outcome {
            stdout: if json {
                // Processes that refuse inspection and show no sign of building here.
                report["uninspected_processes"] = serde_json::json!(uninspected);
                format!("{}\n", sorted_json(&report))
            } else {
                worktree_disk::render_text(&report)
            },
            stderr: String::new(),
            code: 0,
        },
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
