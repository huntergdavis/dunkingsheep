//! Read-only project policy receipts from the versioned task-owner-answer path.
//!
//! A task ID must be supplied by daemon-owned project configuration. This module never searches
//! by title and never treats an arbitrary owner answer as project approval.

use rusqlite::{params, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::DunkError;
use crate::projects::newprojects::{CreditFallback, ProjectBudgetConfig, SubscriptionBudget};

use super::Tasks;

const POLICY_SCHEMA: &str = "project-policy/v1";
const POLICY_PROVIDER_NAMES: [&str; 4] = ["codex", "pi", "opencode-go", "claude"];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectPolicyManifest {
    schema: String,
    project: String,
    provider: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectPolicyReply {
    schema: String,
    project: String,
    provider: String,
    // Keep Value here so absence is distinguishable from an explicit null. Both keys are required.
    subscription: Value,
    credit_fallback: Value,
}

/// The explicit policy captured in one owner answer. `budget_json` is ready for the existing
/// ProjectBudgetConfig parser; its legacy account-wide credit_ceiling is always null.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectPolicyAnswer {
    pub project: String,
    pub provider: String,
    pub budget_json: String,
}

/// Durable evidence for the project policy answer. The role gate is applied by
/// api::h_task_owner_answer before this task event is written; actor and event identity remain
/// attached so downstream startup can retain the receipt provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectPolicyReceipt {
    pub task_id: String,
    pub answer_event_id: i64,
    pub question_version: i64,
    pub actor: String,
    pub answered_at: f64,
    pub answer: ProjectPolicyAnswer,
}

#[derive(Debug)]
struct StateEvent {
    id: i64,
    event: String,
    at: f64,
    actor: Option<String>,
    status_from: Option<String>,
    status_to: Option<String>,
    meta: Option<String>,
}

fn refused(message: &'static str) -> DunkError {
    DunkError::Refused(message.into())
}

fn canonical_project(project: &str) -> Result<String, DunkError> {
    let canonical = crate::projects::validate_name(project)
        .map_err(|_| refused("project policy project name is invalid"))?;
    if canonical != project {
        return Err(refused("project policy project name is not canonical"));
    }
    Ok(canonical)
}

fn canonical_provider(provider: &str) -> Result<(), DunkError> {
    if POLICY_PROVIDER_NAMES.contains(&provider) {
        Ok(())
    } else {
        Err(refused("project policy provider is unsupported"))
    }
}

fn parse_manifest(
    body: &str,
    expected_project: &str,
    expected_provider: &str,
) -> Result<(), DunkError> {
    let manifest: ProjectPolicyManifest = serde_json::from_str(body)
        .map_err(|_| refused("project policy task manifest is invalid"))?;
    if manifest.schema != POLICY_SCHEMA {
        return Err(refused(
            "project policy task manifest version is unsupported",
        ));
    }
    let project = canonical_project(&manifest.project)?;
    if project != expected_project {
        return Err(refused(
            "project policy task does not match the configured project",
        ));
    }
    canonical_provider(&manifest.provider)?;
    if manifest.provider != expected_provider {
        return Err(refused(
            "project policy task does not match the configured provider",
        ));
    }
    Ok(())
}

fn parse_reply(
    raw: &str,
    expected_project: &str,
    expected_provider: &str,
) -> Result<ProjectPolicyAnswer, DunkError> {
    let reply: ProjectPolicyReply =
        serde_json::from_str(raw).map_err(|_| refused("project policy owner answer is invalid"))?;
    if reply.schema != POLICY_SCHEMA {
        return Err(refused(
            "project policy owner answer version is unsupported",
        ));
    }
    let project = canonical_project(&reply.project)?;
    if project != expected_project {
        return Err(refused(
            "project policy owner answer names a different project",
        ));
    }
    canonical_provider(&reply.provider)?;
    if reply.provider != expected_provider {
        return Err(refused(
            "project policy owner answer names a different provider",
        ));
    }

    let subscription_value = reply.subscription;
    let credit_fallback_value = reply.credit_fallback;
    let subscription: Option<SubscriptionBudget> =
        serde_json::from_value(subscription_value.clone())
            .map_err(|_| refused("project policy subscription policy is invalid"))?;
    let credit_fallback: Option<CreditFallback> =
        serde_json::from_value(credit_fallback_value.clone())
            .map_err(|_| refused("project policy credit fallback is invalid"))?;
    if credit_fallback
        .as_ref()
        .is_some_and(|fallback| !fallback.enabled)
    {
        return Err(refused(
            "project policy must use null to disable credit fallback",
        ));
    }
    if subscription.is_none() && credit_fallback.is_none() {
        return Err(refused("project policy has no explicit budget source"));
    }

    let budget_json = json!({
        "credit_ceiling": Value::Null,
        "providers": {
            reply.provider: {
                "subscription": subscription_value,
                "credit_fallback": credit_fallback_value,
            }
        }
    })
    .to_string();
    ProjectBudgetConfig::from_json(&budget_json)
        .map_err(|_| refused("project policy budget is invalid"))?;

    Ok(ProjectPolicyAnswer {
        project,
        provider: reply.provider,
        budget_json,
    })
}

impl Tasks {
    /// Read an answered, explicitly mapped project policy task from the task actor.
    ///
    /// The caller must provide the task ID from daemon-owned configuration and the canonical
    /// project being configured. A valid receipt is only the latest state transition pair
    /// `owner_answer` followed by `unblocked`; later block/unblock transitions invalidate it.
    pub async fn project_policy_receipt(
        &self,
        approval_task_id: &str,
        expected_project: &str,
        expected_provider: &str,
    ) -> Result<ProjectPolicyReceipt, DunkError> {
        let task_id = approval_task_id.to_owned();
        let project = canonical_project(expected_project)?;
        canonical_provider(expected_provider)?;
        let provider = expected_provider.to_owned();
        if task_id.trim().is_empty() {
            return Err(refused("project policy approval task is not configured"));
        }

        self.writer
            .call(move |conn| {
                let (body, status): (String, String) = conn
                    .query_row(
                        "SELECT body, status FROM tasks WHERE id = ?",
                        params![task_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()
                    .map_err(crate::db::map_sqlite)?
                    .ok_or_else(|| refused("project policy approval task is missing"))?;
                parse_manifest(&body, &project, &provider)?;
                if !matches!(status.as_str(), "open" | "done") {
                    return Err(refused("project policy owner answer is not currently approved"));
                }

                let mut statement = conn
                    .prepare(
                        "SELECT id, event, at, actor, status_from, status_to, meta \
                         FROM task_events \
                         WHERE task_id = ? AND event IN ('owner_answer', 'blocked', 'unblocked') \
                         ORDER BY id DESC LIMIT 2",
                    )
                    .map_err(crate::db::map_sqlite)?;
                let events = statement
                    .query_map(params![task_id], |row| {
                        Ok(StateEvent {
                            id: row.get(0)?,
                            event: row.get(1)?,
                            at: row.get(2)?,
                            actor: row.get(3)?,
                            status_from: row.get(4)?,
                            status_to: row.get(5)?,
                            meta: row.get(6)?,
                        })
                    })
                    .map_err(crate::db::map_sqlite)?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(crate::db::map_sqlite)?;
                drop(statement);

                let [unblocked, answer] = events.as_slice() else {
                    return Err(refused("project policy owner answer receipt is missing"));
                };
                if unblocked.event != "unblocked"
                    || unblocked.status_from.as_deref() != Some("blocked")
                    || unblocked.status_to.as_deref() != Some("open")
                    || answer.event != "owner_answer"
                    || answer.status_from.as_deref() != Some("blocked")
                    || answer.status_to.as_deref() != Some("open")
                    || unblocked.actor.as_deref().is_none_or(str::is_empty)
                    || unblocked.actor != answer.actor
                {
                    return Err(refused("project policy owner answer receipt is stale"));
                }
                let actor = answer
                    .actor
                    .as_deref()
                    .filter(|actor| !actor.trim().is_empty())
                    .ok_or_else(|| refused("project policy owner answer actor is missing"))?
                    .to_owned();
                let meta: Value = serde_json::from_str(
                    answer
                        .meta
                        .as_deref()
                        .ok_or_else(|| refused("project policy owner answer metadata is missing"))?,
                )
                .map_err(|_| refused("project policy owner answer metadata is invalid"))?;
                let version = meta["version"]
                    .as_i64()
                    .filter(|version| *version > 0)
                    .ok_or_else(|| refused("project policy owner answer version is invalid"))?;
                let option_number = meta
                    .get("option_number")
                    .ok_or_else(|| refused("project policy owner answer option is missing"))?;
                if !option_number.is_null() {
                    return Err(refused("project policy owner answer must be a JSON reply"));
                }
                let raw_reply = meta["reply"]
                    .as_str()
                    .ok_or_else(|| refused("project policy owner answer reply is missing"))?;
                let answer_event_id = answer.id;
                let previous_event_id: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM task_events WHERE task_id = ? AND id < ? ORDER BY id DESC LIMIT 1",
                        params![task_id, answer_event_id],
                        |row| row.get(0),
                    )
                    .optional()
                    .map_err(crate::db::map_sqlite)?;
                if previous_event_id != Some(version) {
                    return Err(refused("project policy owner answer version is stale"));
                }

                Ok(ProjectPolicyReceipt {
                    task_id,
                    answer_event_id,
                    question_version: version,
                    actor,
                    answered_at: answer.at,
                    answer: parse_reply(raw_reply, &project, &provider)?,
                })
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn manifest(project: &str, provider: &str) -> String {
        json!({"schema": POLICY_SCHEMA, "project": project, "provider": provider}).to_string()
    }

    fn subscription_answer(project: &str) -> String {
        json!({
            "schema": POLICY_SCHEMA,
            "project": project,
            "provider": "codex",
            "subscription": {"window":"weekly", "max_used_percent":80.0, "max_age_seconds":300.0},
            "credit_fallback": null,
        })
        .to_string()
    }

    fn credit_answer(project: &str) -> String {
        json!({
            "schema": POLICY_SCHEMA,
            "project": project,
            "provider": "codex",
            "subscription": null,
            "credit_fallback": {"enabled":true, "limit":12.5, "max_age_seconds":60.0},
        })
        .to_string()
    }

    async fn answered_task(body: &str, reply: &str) -> (Tasks, String, tempfile::TempDir) {
        let dir = tempdir().unwrap();
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let task = tasks.add("global", "project policy", body).await.unwrap();
        tasks
            .block(&task.id, "needs-owner: reply with project-policy/v1 JSON")
            .await
            .unwrap();
        let version = tasks.owner_question(&task.id, None).await.unwrap()["version"]
            .as_str()
            .unwrap()
            .parse::<i64>()
            .unwrap();
        tasks
            .owner_answer(
                &task.id,
                version,
                None,
                Some(reply),
                Some("owner-pane"),
                None,
            )
            .await
            .unwrap();
        (tasks, task.id, dir)
    }

    #[tokio::test]
    async fn subscription_only_receipt_is_explicit_and_has_no_credit_ceiling() {
        let (tasks, id, _dir) =
            answered_task(&manifest("demo", "codex"), &subscription_answer("demo")).await;
        let receipt = tasks
            .project_policy_receipt(&id, "demo", "codex")
            .await
            .unwrap();
        let config = ProjectBudgetConfig::from_json(&receipt.answer.budget_json).unwrap();
        assert_eq!(receipt.answer.provider, "codex");
        assert!(config.credit_ceiling.is_null());
        assert!(config.providers["codex"].subscription.0.is_some());
        assert!(config.providers["codex"].credit_fallback.0.is_none());
        assert_eq!(receipt.task_id, id);
        assert!(receipt.answer_event_id > receipt.question_version);
        assert_eq!(receipt.actor, "owner-pane");
    }

    #[tokio::test]
    async fn explicit_credit_receipt_preserves_provider_limit_and_provenance() {
        let (tasks, id, _dir) =
            answered_task(&manifest("demo", "codex"), &credit_answer("demo")).await;
        let receipt = tasks
            .project_policy_receipt(&id, "demo", "codex")
            .await
            .unwrap();
        let config = ProjectBudgetConfig::from_json(&receipt.answer.budget_json).unwrap();
        let fallback = config.providers["codex"]
            .credit_fallback
            .0
            .as_ref()
            .unwrap();
        assert!(fallback.enabled);
        assert_eq!(fallback.limit, 12.5);
        assert_eq!(receipt.answer.project, "demo");
        assert_eq!(receipt.answer.provider, "codex");
        assert!(receipt.answered_at.is_finite());
    }

    #[tokio::test]
    async fn missing_mapping_unrelated_task_and_wrong_project_refuse() {
        let (tasks, id, _dir) =
            answered_task(&manifest("demo", "codex"), &subscription_answer("demo")).await;
        assert!(tasks
            .project_policy_receipt("", "demo", "codex")
            .await
            .is_err());
        assert!(tasks
            .project_policy_receipt("t999999", "demo", "codex")
            .await
            .is_err());
        assert!(tasks
            .project_policy_receipt(&id, "other", "codex")
            .await
            .is_err());
        assert!(tasks
            .project_policy_receipt(&id, "demo", "pi")
            .await
            .is_err());
        let (unrelated, unrelated_id, _unrelated_dir) =
            answered_task("ordinary task body", &subscription_answer("demo")).await;
        assert!(unrelated
            .project_policy_receipt(&unrelated_id, "demo", "codex")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn malformed_unknown_missing_and_invalid_policy_values_refuse() {
        let invalid = [
            "not-json".to_string(),
            json!({"schema":"project-policy/v2", "project":"demo", "provider":"codex", "subscription":null, "credit_fallback":null}).to_string(),
            json!({"schema":POLICY_SCHEMA, "project":"demo", "provider":"codex", "subscription":null, "credit_fallback":null, "extra":true}).to_string(),
            json!({"schema":POLICY_SCHEMA, "project":"demo", "subscription":null, "credit_fallback":null}).to_string(),
            json!({"schema":POLICY_SCHEMA, "project":"demo", "provider":"codex", "subscription":null, "credit_fallback":{"enabled":true,"limit":0.0,"max_age_seconds":60.0}}).to_string(),
            json!({"schema":POLICY_SCHEMA, "project":"demo", "provider":"codex", "subscription":null, "credit_fallback":{"enabled":false,"limit":10.0,"max_age_seconds":60.0}}).to_string(),
            json!({"schema":POLICY_SCHEMA, "project":"demo", "provider":"codex", "subscription":null, "credit_fallback":null}).to_string(),
            json!({"schema":POLICY_SCHEMA, "project":"demo", "provider":"codex", "subscription":null, "credit_fallback":{"enabled":true,"max_age_seconds":60.0}}).to_string(),
            json!({"schema":POLICY_SCHEMA, "project":"demo", "provider":"unknown", "subscription":{"window":"weekly", "max_used_percent":80.0, "max_age_seconds":300.0}, "credit_fallback":null}).to_string(),
            json!({"schema":POLICY_SCHEMA, "project":"demo", "provider":"pi", "subscription":{"window":"weekly", "max_used_percent":80.0, "max_age_seconds":300.0}, "credit_fallback":null}).to_string(),
        ];
        for reply in invalid {
            let (tasks, id, _dir) = answered_task(&manifest("demo", "codex"), &reply).await;
            assert!(
                tasks
                    .project_policy_receipt(&id, "demo", "codex")
                    .await
                    .is_err(),
                "accepted {reply}"
            );
        }
    }

    #[tokio::test]
    async fn changed_generation_and_later_block_unblock_invalidate_receipt() {
        let (tasks, id, _dir) =
            answered_task(&manifest("demo", "codex"), &subscription_answer("demo")).await;
        tasks
            .block(&id, "needs-owner: updated project policy")
            .await
            .unwrap();
        tasks.unblock(&id).await.unwrap();
        assert!(tasks
            .project_policy_receipt(&id, "demo", "codex")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn changed_owner_answer_version_invalidates_receipt() {
        let (tasks, id, _dir) =
            answered_task(&manifest("demo", "codex"), &subscription_answer("demo")).await;
        let task_id = id.clone();
        tasks
            .writer
            .call(move |conn| {
                let (event_id, meta): (i64, String) = conn
                    .query_row(
                        "SELECT id, meta FROM task_events WHERE task_id = ? AND event = 'owner_answer'",
                        params![task_id],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .map_err(crate::db::map_sqlite)?;
                let mut value: Value = serde_json::from_str(&meta)
                    .map_err(|_| refused("test owner answer metadata is invalid"))?;
                value["version"] = json!(i64::MAX);
                conn.execute(
                    "UPDATE task_events SET meta = ? WHERE id = ?",
                    params![value.to_string(), event_id],
                )
                .map_err(crate::db::map_sqlite)?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(tasks
            .project_policy_receipt(&id, "demo", "codex")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn an_unblock_without_owner_answer_cannot_forge_a_receipt() {
        let dir = tempdir().unwrap();
        let tasks = Tasks::open(dir.path().join("tasks.db")).await.unwrap();
        let task = tasks
            .add("global", "project policy", &manifest("demo", "codex"))
            .await
            .unwrap();
        tasks
            .block(&task.id, "needs-owner: reply with project-policy/v1 JSON")
            .await
            .unwrap();
        tasks.unblock(&task.id).await.unwrap();
        assert!(tasks
            .project_policy_receipt(&task.id, "demo", "codex")
            .await
            .is_err());
    }
}
