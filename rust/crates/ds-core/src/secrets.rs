//! Refuse to publish the desk's own secrets (t756; `dunk_secrets.py`).
//!
//! On 2026-10-29 two commits on two public PRs added `data/` (`data/auth.json`,
//! `data/owner-credentials`, `data/messages.sqlite3`); the owner rotated the password and the
//! token. `.gitignore` alone cannot stop a `git add -A`, so there are two defences and this
//! module is the second: anything that would PUBLISH those names is refused by name, at the
//! credential handshake `git push` performs and by the `gh pr create` wrapper.
//!
//! What this does NOT do, stated because a guard that overclaims is worse than none: it does
//! not rewrite history (a leaked SHA stays fetchable); the credential-helper path refuses the
//! lane's App token but an owner helper configured behind it may still authenticate; it only
//! looks at ADDED paths in the range it can see, and a range it cannot determine is a
//! warning, not a refusal - a guard that fails closed on a detached worktree would block
//! every lane.
//!
//! NOT ported here: the CI scanner (`tools/check_public_secrets.py`, Python) and the
//! Python tests that read `.gitignore` / the script text (`WiringTests`) or compare with the
//! scanner (`AgreementWithTheCiScanner`).

use std::path::Path;
use std::process::Command;

use regex::Regex;

/// The names that must never be published, as globs so the message names the rule.
pub const SECRET_PATTERNS: [&str; 10] = [
    "data/",
    "data",
    "*.sqlite3",
    "*-credentials",
    "owner-credentials",
    "*.pem",
    "*token*",
    "*token*/*",
    ".env",
    "*.key",
];

/// Python `fnmatch.fnmatch` on POSIX: `*` matches anything INCLUDING `/`, `?` one character,
/// `[seq]` / `[!seq]` a class; the whole string must match.
fn fnmatch(name: &str, pattern: &str) -> bool {
    let mut re = String::from("(?s)^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        match c {
            '*' => re.push_str(".*"),
            '?' => re.push('.'),
            '[' => {
                let mut j = i;
                if j < chars.len() && chars[j] == '!' {
                    j += 1;
                }
                if j < chars.len() && chars[j] == ']' {
                    j += 1;
                }
                while j < chars.len() && chars[j] != ']' {
                    j += 1;
                }
                if j >= chars.len() {
                    re.push_str("\\[");
                } else {
                    let mut stuff: String = chars[i..j].iter().collect();
                    stuff = stuff.replace('\\', "\\\\");
                    let negate = stuff.starts_with('!');
                    if negate {
                        stuff = stuff[1..].to_string();
                    }
                    re.push('[');
                    if negate {
                        re.push('^');
                    }
                    re.push_str(&stuff.replace('[', "\\["));
                    re.push(']');
                    i = j + 1;
                }
            }
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).map(|r| r.is_match(name)).unwrap_or(false)
}

/// `matches`: the pattern that makes `path` unpublishable, or `None`.
pub fn matches(path: &str) -> Option<&'static str> {
    let mut path = path;
    // Not lstrip("./"), which eats the dot of ".env".
    while let Some(rest) = path.strip_prefix("./") {
        path = rest;
    }
    for pattern in SECRET_PATTERNS {
        if let Some(dir) = pattern.strip_suffix('/') {
            if path == dir || path.starts_with(pattern) {
                return Some(pattern);
            }
            continue;
        }
        if fnmatch(path, pattern) {
            return Some(pattern);
        }
        // A directory component: 'data/auth.json' matches 'data/'.
        if path.split('/').any(|part| fnmatch(part, pattern)) {
            return Some(pattern);
        }
    }
    None
}

/// `leaks`: `(path, pattern)` for every path that must not be published.
pub fn leaks<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<(String, &'static str)> {
    paths
        .into_iter()
        .filter_map(|p| matches(p).map(|pattern| (p.to_string(), pattern)))
        .collect()
}

fn git(args: &[&str], cwd: &Path) -> (i32, String, String) {
    match Command::new("git").arg("-C").arg(cwd).args(args).output() {
        Ok(done) => (
            done.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&done.stdout).into_owned(),
            String::from_utf8_lossy(&done.stderr).into_owned(),
        ),
        Err(error) => (-1, String::new(), error.to_string()),
    }
}

/// `added_paths`: paths this branch ADDS relative to `base`, plus anything staged. Returns
/// `(paths, error)`; `error` is a sentence when the range could not be determined, so the
/// caller can warn instead of pretending it checked.
pub fn added_paths(cwd: &Path, base: Option<&str>) -> (Vec<String>, Option<String>) {
    let mut base = base.map(str::to_string);
    let mut paths: Vec<String> = Vec::new();
    let mut error = None;
    if base.is_none() {
        for candidate in ["origin/main", "main", "@{upstream}"] {
            if git(&["rev-parse", "--verify", "--quiet", candidate], cwd).0 == 0 {
                base = Some(candidate.to_string());
                break;
            }
        }
    }
    match &base {
        None => {
            error = Some("no base ref (origin/main, main, upstream) to diff against".to_string())
        }
        Some(base) => {
            let range = format!("{base}...HEAD");
            let (code, out, err) = git(&["diff", "--name-only", "--diff-filter=A", &range], cwd);
            if code != 0 {
                let detail: String = err.trim().chars().take(160).collect();
                error = Some(format!("cannot diff against {base}: {detail}"));
            } else {
                paths.extend(
                    out.lines()
                        .filter(|l| !l.trim().is_empty())
                        .map(str::to_string),
                );
            }
        }
    }
    // Staged additions matter even when the branch has no commits yet: this is the exact
    // moment `git add -A` created the incident.
    let (code, out, _err) = git(&["diff", "--cached", "--name-only", "--diff-filter=A"], cwd);
    if code == 0 {
        paths.extend(
            out.lines()
                .filter(|l| !l.trim().is_empty())
                .map(str::to_string),
        );
    }
    paths.sort();
    paths.dedup();
    (paths, error)
}

/// `refusal`: `(message, error)` - the named refusal, or `(None, error-or-None)`.
pub fn refusal(cwd: &Path, base: Option<&str>) -> (Option<String>, Option<String>) {
    let (paths, error) = added_paths(cwd, base);
    let found = leaks(paths.iter().map(String::as_str));
    if found.is_empty() {
        return (None, error);
    }
    let mut lines = vec![format!(
        "REFUSED: this push would publish {} file(s) that must never leave this host:",
        found.len()
    )];
    for (path, pattern) in &found {
        lines.push(format!(
            "  {path}   (matches {})",
            crate::context::store::py_repr(pattern)
        ));
    }
    lines.push(
        "These are the desk's own secrets - the owner's password, API token, cookie store and \
         conversation. They were leaked twice on 2026-10-29 (PR #143 and PR #146) and the owner \
         rotated the credentials both times."
            .to_string(),
    );
    lines.push(
        "Fix: `git rm --cached <path>`, commit, and add the name to .gitignore. If it is already \
         in a pushed commit, the commit must be rewritten and the credential rotated - a \
         force-push does not unpublish the SHA."
            .to_string(),
    );
    (Some(lines.join("\n")), error)
}

/// `main`: `dunkingsheep secrets-check [--base REF] [--cwd DIR] [--quiet]`; returns
/// `(exit code, stderr text)` - 1 on a refusal, 0 otherwise (a warning only prints).
pub fn check(cwd: &Path, base: Option<&str>, quiet: bool) -> (i32, String) {
    let (message, error) = refusal(cwd, base);
    if let Some(message) = message {
        return (1, format!("{message}\n"));
    }
    match error {
        Some(error) if !quiet => (
            0,
            format!("secrets-check: WARNING could not fully check: {error}\n"),
        ),
        _ => (0, String::new()),
    }
}

#[cfg(test)]
#[path = "secrets_tests.rs"]
mod tests;
