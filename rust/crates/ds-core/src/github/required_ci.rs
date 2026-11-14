//! Fail-closed merge checks. Observed green checks never substitute for an unknown policy.
use super::{request, GitHubTransport, RequestOptions};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const UNKNOWN: &str = "merge held: required checks unknown";

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
pub async fn require_merge_ci(
    transport: &dyn GitHubTransport,
    root: &str,
    repo: &str,
    number: i64,
    head: &str,
    token: &str,
) -> Result<(), String> {
    let pull = get(
        transport,
        root,
        token,
        &format!("repos/{repo}/pulls/{number}"),
    )
    .await?;
    if pull["state"] != "open" || pull["head"]["sha"].as_str() != Some(head) {
        return Err("merge held: reviewed head is no longer the open PR head".into());
    }
    let branch = pull["base"]["ref"]
        .as_str()
        .filter(|s| !s.is_empty())
        .ok_or(UNKNOWN)?;
    let protection = get(
        transport,
        root,
        token,
        &format!("repos/{repo}/branches/{}/protection", segment(branch)),
    )
    .await?;
    let required = &protection["required_status_checks"];
    let mut names = BTreeSet::new();
    for context in required["contexts"].as_array().ok_or(UNKNOWN)? {
        let name = context.as_str().filter(|s| !s.is_empty()).ok_or(UNKNOWN)?;
        names.insert((name.to_owned(), None));
    }
    // GitHub returns both the legacy contexts list and app-bound checks. A bound entry
    // replaces its legacy duplicate, so a different App cannot satisfy the requirement.
    for check in required["checks"].as_array().ok_or(UNKNOWN)? {
        let name = check["context"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or(UNKNOWN)?;
        let app = match check.get("app_id") {
            Some(Value::Null) => None,
            Some(value) => match value.as_i64() {
                Some(-1) => None,
                Some(id) if id > 0 => Some(id),
                _ => return Err(UNKNOWN.into()),
            },
            None => return Err(UNKNOWN.into()),
        };
        names.remove(&(name.to_owned(), None));
        names.insert((name.to_owned(), app));
    }
    if names.is_empty() {
        return Err(UNKNOWN.into());
    }
    let runs = get(
        transport,
        root,
        token,
        &format!("repos/{repo}/commits/{head}/check-runs?per_page=100&filter=latest"),
    )
    .await?;
    let combined = get(
        transport,
        root,
        token,
        &format!("repos/{repo}/commits/{head}/status?per_page=100"),
    )
    .await?;
    let waiting = "merge held: waiting for CI";
    let checks = runs["check_runs"].as_array().ok_or(waiting)?;
    let statuses = combined["statuses"].as_array().ok_or(waiting)?;
    if runs["total_count"].as_u64() != Some(checks.len() as u64)
        || combined["total_count"].as_u64() != Some(statuses.len() as u64)
        || checks.len() > 100
        || statuses.len() > 100
        || combined["sha"].as_str() != Some(head)
    {
        return Err("merge held: incomplete or truncated CI results".into());
    }
    let mut status_apps = BTreeMap::new();
    for (name, app) in names {
        let mut latest = BTreeMap::new();
        for run in checks.iter().filter(|r| r["name"].as_str() == Some(&name)) {
            let run_app = run["app"]["id"].as_i64().ok_or(waiting)?;
            if app.is_some_and(|id| id != run_app) {
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
        let status = statuses
            .iter()
            .filter(|s| s["context"].as_str() == Some(&name))
            .max_by_key(|s| s["id"].as_u64().unwrap_or(0));
        let status = if let (Some(app_id), Some(status)) = (app, status) {
            // Commit statuses expose their creator, not an App id. Resolve a GitHub
            // App bot's slug via the read-only public App endpoint; never confuse a
            // creator/user id with an App id or accept a same-named user's status.
            let slug = status["creator"]["login"]
                .as_str()
                .filter(|_| status["creator"]["type"] == "Bot")
                .and_then(|login| login.strip_suffix("[bot]"))
                .filter(|slug| !slug.is_empty());
            let matches = if let Some(slug) = slug {
                if !status_apps.contains_key(slug) {
                    let info =
                        get(transport, root, token, &format!("apps/{}", segment(slug))).await?;
                    let id = info["id"].as_i64().filter(|id| *id > 0).ok_or(UNKNOWN)?;
                    if info["slug"].as_str() != Some(slug) {
                        return Err(UNKNOWN.into());
                    }
                    status_apps.insert(slug.to_owned(), id);
                }
                status_apps.get(slug) == Some(&app_id)
            } else {
                false
            };
            matches.then_some(status)
        } else {
            status
        };
        if latest.is_empty() && status.is_none() {
            return Err(format!("{waiting}: {name} missing"));
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
    Ok(())
}
