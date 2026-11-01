//! T44, slice 5: `dunkingsheep github wrappers` (`dunk_github_wrappers_install` and the
//! `wrappers` branch of `_github_local`): put the `gh` wrapper script in `--bin-dir` and make
//! `!dunkingsheep git-credential` the FIRST helper of `credential.https://github.com.helper`
//! in the user's global git config, keeping every helper already there, in order (t599: the
//! first version used --replace-all and dropped the owner's own `gh auth git-credential`).
//!
//! The wrapper script text is `gh_wrapper.sh`, written FROM the Python's `GH_WRAPPER` so the
//! bytes are the Python's. `github protect` loads only the configured review App metadata and
//! uses the user's `gh` CLI for the one-shot branch protection update.

use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::{env, io::Write};

/// `GH_WRAPPER`, byte for byte, with `__REAL_GH__` to be replaced.
pub const GH_WRAPPER: &str = include_str!("gh_wrapper.sh");

const HELPER: &str = "!dunkingsheep git-credential";
const KEY: &str = "credential.https://github.com.helper";
const NOTE: &str = "lanes use https remotes; our helper is first and any helper already configured (the owner's gh login) is kept as the fallback (gh-token prints nothing for the owner role)";
const REVIEW_CONTEXT: &str = "dunkingsheep/review";

fn load_reviewer_app() -> Result<(i64, String), String> {
    let config_dir = env::var_os("DUNKINGSHEEP_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env::var_os("HOME").unwrap_or_default())
                .join(".config")
                .join("dunkingsheep")
        });
    let app_path = config_dir.join("github-app-reviewer.json");
    let raw = std::fs::read(&app_path).map_err(|_| {
        format!(
            "no GitHub App registered as 'reviewer': {} is missing (create it with the manifest page, docs/FLAT_FLEET.md 6.1)",
            app_path.display()
        )
    })?;
    let app: Value = serde_json::from_slice(&raw).map_err(|_| {
        format!(
            "no GitHub App registered as 'reviewer': {} is missing (create it with the manifest page, docs/FLAT_FLEET.md 6.1)",
            app_path.display()
        )
    })?;
    let id = app
        .get("id")
        .and_then(Value::as_i64)
        .ok_or_else(|| "GitHub App 'reviewer' is missing a numeric id".to_string())?;
    let slug = app
        .get("slug")
        .and_then(Value::as_str)
        .filter(|slug| !slug.is_empty())
        .unwrap_or("dunkingsheep-reviewer")
        .to_string();
    let pem = app
        .get("pem")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .unwrap_or_else(|| config_dir.join("github-app-reviewer.pem"));
    if !pem.exists() {
        return Err(format!(
            "GitHub App 'reviewer' private key missing: {}",
            pem.display()
        ));
    }
    Ok((id, slug))
}

/// The branch-protection request body used by Python's `dunk_github.protection_payload`.
pub fn protection_payload(
    approvals: i64,
    reviewer_app_id: i64,
    bypass_app_slug: Option<&str>,
) -> Value {
    let mut payload = json!({
        "required_status_checks": {
            "strict": false,
            "checks": [{"context": REVIEW_CONTEXT, "app_id": reviewer_app_id}]
        },
        "enforce_admins": true,
        "required_pull_request_reviews": {
            "dismiss_stale_reviews": true,
            "require_code_owner_reviews": false,
            "required_approving_review_count": approvals
        },
        "restrictions": null,
        "allow_force_pushes": false,
        "allow_deletions": false,
        "required_linear_history": false
    });
    if let Some(slug) = bypass_app_slug {
        payload["required_pull_request_reviews"]["bypass_pull_request_allowances"] =
            json!({"apps": [slug]});
    }
    payload
}

fn gh_api(args: &[&str], payload: &Value) -> Result<Output, String> {
    let mut child = Command::new("gh")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| error.to_string())?;
    let body = serde_json::to_vec(payload).map_err(|error| error.to_string())?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(&body)
        .map_err(|error| error.to_string())?;
    child.wait_with_output().map_err(|error| error.to_string())
}

fn trimmed(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_string()
}

/// `dunkingsheep github protect`: dry-run prints the payload; apply delegates to `gh api`.
pub fn protect(
    repo: &str,
    approvals: i64,
    bypass_reviewer_app: bool,
    dry_run: bool,
) -> crate::usage_cmd::Outcome {
    let (reviewer_id, reviewer_slug) = match load_reviewer_app() {
        Ok(app) => app,
        Err(error) => {
            return crate::usage_cmd::Outcome {
                stdout: String::new(),
                stderr: format!("{error}\n"),
                code: 1,
            }
        }
    };
    let payload = protection_payload(
        approvals,
        reviewer_id,
        bypass_reviewer_app.then_some(reviewer_slug.as_str()),
    );
    if dry_run {
        return crate::usage_cmd::Outcome {
            stdout: format!("{}\n", crate::usage_cmd::ordered_json(&payload)),
            stderr: String::new(),
            code: 0,
        };
    }

    let protection_path = format!("repos/{repo}/branches/main/protection");
    let put_args = ["api", "-X", "PUT", &protection_path, "--input", "-"];
    let put = match gh_api(&put_args, &payload) {
        Ok(output) => output,
        Err(error) => {
            return crate::usage_cmd::Outcome {
                stdout: String::new(),
                stderr: format!("{error}\n"),
                code: 1,
            }
        }
    };
    if !put.status.success() {
        let error = trimmed(&put.stderr);
        let error = if error.is_empty() {
            trimmed(&put.stdout)
        } else {
            error
        };
        return crate::usage_cmd::Outcome {
            stdout: String::new(),
            stderr: format!("{error}\n"),
            code: put.status.code().unwrap_or(1),
        };
    }

    let auto_merge = json!({"allow_auto_merge": true, "delete_branch_on_merge": true});
    let repo_path = format!("repos/{repo}");
    let patch_args = ["api", "-X", "PATCH", &repo_path, "--input", "-"];
    let auto_merge_status = match gh_api(&patch_args, &auto_merge) {
        Ok(output) if output.status.success() => "on".to_string(),
        Ok(output) => format!("NOT enabled: {}", trimmed(&output.stderr)),
        Err(error) => format!("NOT enabled: {error}"),
    };
    crate::usage_cmd::Outcome {
        stdout: format!(
            "protected {repo}: {approvals} approval(s), {REVIEW_CONTEXT} required (app {reviewer_id}), auto-merge {auto_merge_status}\n"
        ),
        stderr: String::new(),
        code: 0,
    }
}

/// What the install did (the Python's result dict, in its key order).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapperInstall {
    pub gh_wrapper: String,
    pub real_gh: String,
    pub git_credential_helper: String,
    pub helpers: Vec<String>,
    pub note: String,
}

fn expand_user(path: &str) -> PathBuf {
    match (path.strip_prefix("~/"), std::env::var("HOME")) {
        (Some(rest), Ok(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(path),
    }
}

fn real_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

/// The first `gh` on `path_env` that is not in `bin_dir` and is not already our wrapper.
fn find_real_gh(path_env: &str, bin_dir: &Path) -> Option<PathBuf> {
    let bin_real = real_path(bin_dir);
    for directory in path_env.split(':') {
        let candidate = Path::new(directory).join("gh");
        if real_path(Path::new(directory)) != bin_real && is_executable_file(&candidate) {
            if let Ok(bytes) = std::fs::read(&candidate) {
                let head = &bytes[..bytes.len().min(300)];
                if head.windows(23).any(|w| w == b"dunkingsheep gh wrapper") {
                    continue;
                }
            }
            return Some(candidate);
        }
    }
    None
}

fn git_config(args: &[&str]) -> String {
    Command::new("git")
        .args(["config", "--global"])
        .args(args)
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// `dunk_github_wrappers_install(bin_dir)`. `path_env` is `$PATH`. `Err` is the Python's
/// `SystemExit` text.
pub fn wrappers_install(bin_dir: &str, path_env: &str) -> Result<WrapperInstall, String> {
    let bin_dir = expand_user(bin_dir);
    std::fs::create_dir_all(&bin_dir)
        .map_err(|e| format!("cannot create {}: {e}", bin_dir.display()))?;
    let real = find_real_gh(path_env, &bin_dir)
        .ok_or_else(|| "no real `gh` found on PATH to wrap".to_string())?;
    let target = bin_dir.join("gh");
    let script = GH_WRAPPER.replace("__REAL_GH__", &real.display().to_string());
    std::fs::write(&target, script)
        .map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    let mode = std::fs::metadata(&target)
        .map_err(|e| e.to_string())?
        .permissions()
        .mode();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode | 0o111))
        .map_err(|e| e.to_string())?;
    // Ours goes FIRST (it is the lane credential) and every helper already there stays behind
    // it, in the order it was in.
    let existing: Vec<String> = git_config(&["--get-all", KEY])
        .lines()
        .filter(|row| !row.trim().is_empty())
        .map(str::to_string)
        .collect();
    let mut wanted = vec![HELPER.to_string()];
    wanted.extend(
        existing
            .iter()
            .filter(|row| row.as_str() != HELPER)
            .cloned(),
    );
    if existing != wanted {
        git_config(&["--unset-all", KEY]);
        for row in &wanted {
            git_config(&["--add", KEY, row]);
        }
    }
    Ok(WrapperInstall {
        gh_wrapper: target.display().to_string(),
        real_gh: real.display().to_string(),
        git_credential_helper: HELPER.to_string(),
        helpers: wanted,
        note: NOTE.to_string(),
    })
}

/// The printer: `{key:<20} {value}` per key; the helpers list prints as a Python list.
pub fn render_wrappers(result: &WrapperInstall) -> String {
    let list = format!(
        "[{}]",
        result
            .helpers
            .iter()
            .map(|h| ds_core::context::store::py_repr(h))
            .collect::<Vec<_>>()
            .join(", ")
    );
    let rows: [(&str, &str); 5] = [
        ("gh_wrapper", &result.gh_wrapper),
        ("real_gh", &result.real_gh),
        ("git_credential_helper", &result.git_credential_helper),
        ("helpers", &list),
        ("note", &result.note),
    ];
    rows.iter()
        .map(|(key, value)| format!("{key:<20} {value}\n"))
        .collect()
}
