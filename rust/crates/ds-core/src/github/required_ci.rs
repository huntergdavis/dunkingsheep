//! Fail-closed merge checks. Observed green checks never substitute for an unknown policy.
use super::{request, GitHubRequest, GitHubTransport, RequestOptions};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const UNKNOWN: &str = "merge held: required checks unknown";

/// Trusted, non-secret identity from the review App registration used to mint its token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusApp {
    pub id: i64,
    pub bot_login: String,
}

/// Optional owner policy. Read on every gate evaluation, never cached or written by the daemon.
/// An absent, linked, non-0600, foreign-owned, oversized or malformed file supplies no policy.
pub fn read_required_checks(path: &std::path::Path) -> Result<Value, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(path)
        .map_err(|_| UNKNOWN)?;
    let metadata = file.metadata().map_err(|_| UNKNOWN)?;
    if !metadata.is_file()
        || metadata.mode() & 0o7777 != 0o600
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.len() > 65536
    {
        return Err(UNKNOWN.into());
    }
    let mut raw = String::new();
    file.take(65537)
        .read_to_string(&mut raw)
        .map_err(|_| UNKNOWN)?;
    if raw.len() > 65536 {
        return Err(UNKNOWN.into());
    }
    let value: Value = serde_json::from_str(&raw).map_err(|_| UNKNOWN)?;
    if value["version"] != 1 || !value["repos"].is_object() {
        return Err(UNKNOWN.into());
    }
    Ok(value)
}

fn check_names(checks: &Value) -> Result<BTreeSet<(String, i64)>, String> {
    let mut names = BTreeSet::new();
    for check in checks.as_array().ok_or(UNKNOWN)? {
        let name = check["context"]
            .as_str()
            .filter(|name| !name.is_empty() && name.trim() == *name)
            .ok_or(UNKNOWN)?;
        let app = check["app_id"]
            .as_i64()
            .filter(|id| *id > 0)
            .ok_or(UNKNOWN)?;
        names.insert((name.to_string(), app));
    }
    if names.is_empty() {
        return Err(UNKNOWN.into());
    }
    Ok(names)
}

async fn protection_response(
    transport: &dyn GitHubTransport,
    root: &str,
    repo: &str,
    branch: &str,
    token: &str,
) -> Result<(u16, Value), String> {
    // Preserve the actual HTTP status: only a protection 403/404 may use owner policy.
    let response = transport
        .send(GitHubRequest {
            method: "GET".into(),
            url: format!(
                "{root}/repos/{repo}/branches/{}/protection",
                segment(branch)
            ),
            headers: BTreeMap::from([
                ("Accept".into(), "application/vnd.github+json".into()),
                ("User-Agent".into(), "dunkingsheep".into()),
                ("X-GitHub-Api-Version".into(), "2022-11-28".into()),
                ("Authorization".into(), format!("token {token}")),
            ]),
            body: None,
            timeout_s: 20,
        })
        .await
        .map_err(|_| UNKNOWN)?;
    let value = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
    Ok((response.status, value))
}

fn segment(value: &str) -> String {
    value
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

async fn get(
    transport: &dyn GitHubTransport,
    root: &str,
    token: &str,
    path: &str,
) -> Result<Value, String> {
    request(
        transport,
        RequestOptions {
            method: "GET",
            url: &format!("{root}/{path}"),
            token: Some(token),
            bearer: None,
            payload: None,
            timeout_s: 20,
            api_root: root,
        },
    )
    .await
    .map_err(|_| UNKNOWN.to_string())?
    .1
    .ok_or_else(|| UNKNOWN.into())
}

/// Both callers still enforce distinct exact-head reviews. This gate adds a known nonempty
/// branch-protection policy plus successful results for each required context on that head.
/// Every unavailable, malformed, incomplete or truncated response holds the merge.
#[allow(clippy::too_many_arguments)]
pub async fn require_merge_ci(
    transport: &dyn GitHubTransport,
    root: &str,
    repo: &str,
    number: i64,
    head: &str,
    token: &str,
    reviewer: Option<&StatusApp>,
    trusted: Option<&Value>,
) -> Result<Value, String> {
    let pull = get(
        transport,
        root,
        token,
        &format!("repos/{repo}/pulls/{number}"),
    )
    .await?;
    if pull["state"] != "open"
        || pull["head"]["sha"].as_str() != Some(head)
        || pull["draft"] == true
    {
        return Err("merge held: reviewed head is no longer the open PR head".into());
    }
    let branch = pull["base"]["ref"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(UNKNOWN)?;
    let (status, protection) = protection_response(transport, root, repo, branch, token).await?;
    let names = if status == 200 {
        let required = &protection["required_status_checks"];
        let names = check_names(&required["checks"])?;
        for context in required["contexts"].as_array().ok_or(UNKNOWN)? {
            let name = context.as_str().filter(|s| !s.is_empty()).ok_or(UNKNOWN)?;
            if !names.iter().any(|(bound, _)| bound == name) {
                return Err(UNKNOWN.into());
            }
        }
        names
    } else if matches!(status, 403 | 404) {
        // No observed-check inference. This exact repo/branch must have a nonempty owner policy.
        let policy = trusted.filter(|p| p["version"] == 1).ok_or(UNKNOWN)?;
        check_names(&policy["repos"][repo][branch])?
    } else {
        return Err(UNKNOWN.into());
    };
    // Neither source can waive the review gate or its configured producer. Owner policy is
    // only a fallback for an unreadable protection policy, never a replacement for readable API checks.
    let reviewer = reviewer
        .filter(|app| app.id > 0 && !app.bot_login.is_empty())
        .ok_or(UNKNOWN)?;
    if !names.contains(&(super::REVIEW_CONTEXT.to_string(), reviewer.id)) {
        return Err(UNKNOWN.into());
    }
    if names
        .iter()
        .any(|(name, id)| name == super::REVIEW_CONTEXT && *id != reviewer.id)
    {
        return Err(UNKNOWN.into());
    }
    let runs = get(
        transport,
        root,
        token,
        &format!("repos/{repo}/commits/{head}/check-runs?per_page=100&filter=latest"),
    )
    .await?;
    let listed = get(
        transport,
        root,
        token,
        &format!("repos/{repo}/commits/{head}/statuses?per_page=100"),
    )
    .await?;
    let waiting = "merge held: waiting for CI";
    let checks = runs["check_runs"].as_array().ok_or(waiting)?;
    // The list endpoint includes creator; combined status entries do not. A full
    // page may have more history beyond it, so refuse rather than miss a latest result.
    let statuses = listed.as_array().ok_or(waiting)?;
    if runs["total_count"].as_u64() != Some(checks.len() as u64)
        || checks.len() > 100
        || statuses.len() >= 100
    {
        return Err("merge held: incomplete or truncated CI results".into());
    }
    for (name, app) in names {
        let mut latest = BTreeMap::new();
        for run in checks.iter().filter(|r| r["name"].as_str() == Some(&name)) {
            let run_app = run["app"]["id"].as_i64().ok_or(waiting)?;
            if app != run_app {
                continue;
            }
            let id = run["id"].as_u64().ok_or(waiting)?;
            if latest
                .get(&run_app)
                .is_none_or(|old: &&Value| old["id"].as_u64().unwrap_or(0) < id)
            {
                latest.insert(run_app, run);
            }
        }
        if statuses
            .iter()
            .filter(|s| s["context"].as_str() == Some(&name))
            .any(|s| s["id"].as_u64().is_none())
        {
            return Err(waiting.into());
        }
        let status = statuses
            .iter()
            .filter(|s| s["context"].as_str() == Some(&name))
            .max_by_key(|s| s["id"].as_u64().unwrap_or(0));
        let bound_status = status.filter(|status| {
            reviewer.id == app
                && status["creator"]["type"] == "Bot"
                && status["creator"]["login"].as_str() == Some(reviewer.bot_login.as_str())
        });
        if latest.is_empty() && bound_status.is_none() {
            return Err(format!(
                "{waiting}: {name} missing or required App unverified"
            ));
        }
        for run in latest.values() {
            if run["head_sha"].as_str() != Some(head)
                || run["status"] != "completed"
                || run["conclusion"] != "success"
            {
                return Err(format!(
                    "{waiting}: {name} is not successful on the reviewed head"
                ));
            }
        }
        if status.is_some_and(|s| s["id"].as_u64().is_none() || s["state"] != "success") {
            return Err(format!("{waiting}: {name} status is not successful"));
        }
    }
    Ok(pull)
}
