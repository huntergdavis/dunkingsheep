use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use tokio::runtime::Runtime;

use super::{load_startup_policy, StartupPolicy, PROJECT_RUNTIME_CONFIG_FILE};
use crate::projects::newprojects::ProjectBudgetConfig;
use crate::tasks::Tasks;

const POLICY: &str = "project-policy/v1";

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn manifest(project: &str, provider: &str) -> String {
    json!({"schema": POLICY, "project": project, "provider": provider}).to_string()
}

fn subscription_reply(project: &str, provider: &str) -> String {
    json!({
        "schema": POLICY,
        "project": project,
        "provider": provider,
        "subscription": {"window": "weekly", "max_used_percent": 80.0, "max_age_seconds": 300.0},
        "credit_fallback": null,
    })
    .to_string()
}

fn credit_reply(project: &str, provider: &str, limit: f64) -> String {
    json!({
        "schema": POLICY,
        "project": project,
        "provider": provider,
        "subscription": null,
        "credit_fallback": {"enabled": true, "limit": limit, "max_age_seconds": 60.0},
    })
    .to_string()
}

fn open(runtime: &Runtime, dir: &Path) -> Tasks {
    runtime.block_on(Tasks::open(dir.join("tasks.db"))).unwrap()
}

/// One policy task blocked on its owner question and never answered.
fn blocked(runtime: &Runtime, tasks: &Tasks, body: &str) -> String {
    let task = runtime
        .block_on(tasks.add("global", "project policy", body))
        .unwrap();
    runtime
        .block_on(tasks.block(&task.id, "needs-owner: reply with project-policy/v1 JSON"))
        .unwrap();
    task.id
}

/// One policy task answered through the production owner-answer path.
fn answered(runtime: &Runtime, tasks: &Tasks, body: &str, reply: &str) -> String {
    let id = blocked(runtime, tasks, body);
    let version = runtime.block_on(tasks.owner_question(&id, None)).unwrap()["version"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    runtime
        .block_on(tasks.owner_answer(&id, version, None, Some(reply), Some("owner-pane"), None))
        .unwrap();
    id
}

fn runtime_config(dir: &Path, projects: Value) -> Value {
    json!({
        "schema": "project-runtime/v1",
        "project_root": dir.join("projects"),
        "store_path": dir.join("projects.sqlite3"),
        "provider_homes": {"codex": dir.join("codex-home")},
        "projects": projects,
    })
}

/// Write the startup file and the directories it names.
fn write_config(dir: &Path, projects: Value) -> PathBuf {
    std::fs::create_dir_all(dir.join("projects")).unwrap();
    std::fs::create_dir_all(dir.join("codex-home")).unwrap();
    let path = dir.join(PROJECT_RUNTIME_CONFIG_FILE);
    std::fs::write(&path, runtime_config(dir, projects).to_string()).unwrap();
    path
}

#[test]
fn a_current_subscription_receipt_admits_its_project_and_creates_no_store() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let tasks = open(&rt, dir.path());
    let id = answered(
        &rt,
        &tasks,
        &manifest("demo", "codex"),
        &subscription_reply("demo", "codex"),
    );
    let path = write_config(
        dir.path(),
        json!({"demo": {"provider": "codex", "approval_task": id}}),
    );

    let StartupPolicy::Attached { policy, refusal } =
        load_startup_policy(&path, &tasks, rt.handle())
    else {
        panic!("a current owner receipt must attach its project");
    };
    assert!(refusal.is_none());
    let entry = policy.entry("demo").expect("approved project");
    assert!(entry.approved);
    assert_eq!(entry.provider, "codex");
    let budget = ProjectBudgetConfig::from_json(&entry.budget_json).unwrap();
    assert!(budget.credit_ceiling.is_null());
    assert!(budget.providers["codex"].subscription.0.is_some());
    assert!(budget.providers["codex"].credit_fallback.0.is_none());
    assert!(!dir.path().join("projects.sqlite3").exists());
}

#[test]
fn an_explicit_credit_receipt_keeps_its_provider_and_ceiling() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let tasks = open(&rt, dir.path());
    let id = answered(
        &rt,
        &tasks,
        &manifest("demo", "codex"),
        &credit_reply("demo", "codex", 12.5),
    );
    let path = write_config(
        dir.path(),
        json!({"demo": {"provider": "codex", "approval_task": id}}),
    );

    let StartupPolicy::Attached { policy, .. } = load_startup_policy(&path, &tasks, rt.handle())
    else {
        panic!("an explicit credit ceiling must attach its project");
    };
    let budget =
        ProjectBudgetConfig::from_json(&policy.entry("demo").unwrap().budget_json).unwrap();
    let fallback = budget.providers["codex"]
        .credit_fallback
        .0
        .as_ref()
        .unwrap();
    assert!(fallback.enabled);
    assert_eq!(fallback.limit, 12.5);
    assert!(budget.providers["codex"].subscription.0.is_none());
}

#[test]
fn only_projects_with_current_receipts_are_admitted() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let tasks = open(&rt, dir.path());
    let demo = answered(
        &rt,
        &tasks,
        &manifest("demo", "codex"),
        &subscription_reply("demo", "codex"),
    );
    let waiting = blocked(&rt, &tasks, &manifest("other", "codex"));
    let path = write_config(
        dir.path(),
        json!({
            "demo": {"provider": "codex", "approval_task": demo},
            "other": {"provider": "codex", "approval_task": waiting},
        }),
    );

    let StartupPolicy::Attached { policy, refusal } =
        load_startup_policy(&path, &tasks, rt.handle())
    else {
        panic!("one current receipt must still attach");
    };
    assert!(policy.entry("demo").is_some());
    assert!(policy.entry("other").is_none());
    let refusal = refusal.expect("the unanswered project is named");
    assert!(refusal.contains("project other"), "{refusal}");
}

#[test]
fn missing_stale_mismatched_and_unattributed_evidence_admits_nothing() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let tasks = open(&rt, dir.path());
    let missing = blocked(&rt, &tasks, &manifest("missing", "codex"));
    let stale = answered(
        &rt,
        &tasks,
        &manifest("stale", "codex"),
        &subscription_reply("stale", "codex"),
    );
    rt.block_on(tasks.block(&stale, "needs-owner: changed policy"))
        .unwrap();
    rt.block_on(tasks.unblock(&stale)).unwrap();
    let forged = blocked(&rt, &tasks, &manifest("forged", "codex"));
    rt.block_on(tasks.unblock(&forged)).unwrap();
    let for_codex = answered(
        &rt,
        &tasks,
        &manifest("pi-project", "codex"),
        &subscription_reply("pi-project", "codex"),
    );
    let for_demo = answered(
        &rt,
        &tasks,
        &manifest("demo", "codex"),
        &subscription_reply("demo", "codex"),
    );
    let path = write_config(
        dir.path(),
        json!({
            "missing": {"provider": "codex", "approval_task": missing},
            "stale": {"provider": "codex", "approval_task": stale},
            "forged": {"provider": "codex", "approval_task": forged},
            "pi-project": {"provider": "pi", "approval_task": for_codex},
            "renamed": {"provider": "codex", "approval_task": for_demo},
        }),
    );

    let StartupPolicy::Refused(reason) = load_startup_policy(&path, &tasks, rt.handle()) else {
        panic!("no current receipt may attach a policy");
    };
    assert!(
        reason.starts_with("no project has a current owner policy"),
        "{reason}"
    );
    for name in ["missing", "stale", "forged", "pi-project", "renamed"] {
        assert!(reason.contains(&format!("project {name}:")), "{reason}");
    }
}

#[test]
fn a_credit_fallback_without_an_explicit_limit_is_refused() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let tasks = open(&rt, dir.path());
    let reply = json!({
        "schema": POLICY,
        "project": "demo",
        "provider": "codex",
        "subscription": null,
        "credit_fallback": {"enabled": true, "max_age_seconds": 60.0},
    })
    .to_string();
    // The answer path refuses this reply before any event is written, so the task stays blocked
    // and the startup load admits nothing.
    let id = blocked(&rt, &tasks, &manifest("demo", "codex"));
    let version = rt.block_on(tasks.owner_question(&id, None)).unwrap()["version"]
        .as_str()
        .unwrap()
        .parse::<i64>()
        .unwrap();
    let answer = rt.block_on(tasks.owner_answer(
        &id,
        version,
        None,
        Some(reply.as_str()),
        Some("owner-pane"),
        None,
    ));
    assert!(
        answer.is_err(),
        "a credit fallback without a limit was written as an answer"
    );
    assert_eq!(rt.block_on(tasks.get(&id)).unwrap().status, "blocked");
    let path = write_config(
        dir.path(),
        json!({"demo": {"provider": "codex", "approval_task": id}}),
    );
    assert!(matches!(
        load_startup_policy(&path, &tasks, rt.handle()),
        StartupPolicy::Refused(_)
    ));
}

#[test]
fn malformed_or_unknown_startup_files_admit_nothing() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let tasks = open(&rt, dir.path());
    let id = answered(
        &rt,
        &tasks,
        &manifest("demo", "codex"),
        &subscription_reply("demo", "codex"),
    );
    let valid = runtime_config(
        dir.path(),
        json!({"demo": {"provider": "codex", "approval_task": id}}),
    );
    let mut unknown_key = valid.clone();
    unknown_key["extra"] = json!(true);
    let mut wrong_schema = valid.clone();
    wrong_schema["schema"] = json!("project-runtime/v2");
    let mut unknown_project_key = valid.clone();
    unknown_project_key["projects"]["demo"]["limit"] = json!(10.0);
    let mut relative_root = valid.clone();
    relative_root["project_root"] = json!("relative/projects");
    let path = write_config(dir.path(), json!({}));
    let cases = [
        "not json".to_string(),
        unknown_key.to_string(),
        wrong_schema.to_string(),
        unknown_project_key.to_string(),
        relative_root.to_string(),
        json!({"schema": "project-runtime/v1", "project_root": "/", "store_path": "/x", "provider_homes": {}, "projects": {}}).to_string(),
    ];
    for raw in cases {
        std::fs::write(&path, &raw).unwrap();
        assert!(
            matches!(
                load_startup_policy(&path, &tasks, rt.handle()),
                StartupPolicy::Refused(_)
            ),
            "admitted {raw}"
        );
    }
}

#[test]
fn receipts_are_never_read_from_inside_a_running_runtime() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let tasks = open(&rt, dir.path());
    let id = answered(
        &rt,
        &tasks,
        &manifest("demo", "codex"),
        &subscription_reply("demo", "codex"),
    );
    let path = write_config(
        dir.path(),
        json!({"demo": {"provider": "codex", "approval_task": id}}),
    );
    let outcome = rt.block_on(async { load_startup_policy(&path, &tasks, rt.handle()) });
    let StartupPolicy::Refused(reason) = outcome else {
        panic!("a load inside a runtime must refuse instead of blocking");
    };
    assert!(reason.contains("synchronous startup context"), "{reason}");
    assert!(!dir.path().join("projects.sqlite3").exists());
}

#[test]
fn no_startup_file_means_no_policy_and_no_refusal() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let tasks = open(&rt, dir.path());
    let path = dir.path().join(PROJECT_RUNTIME_CONFIG_FILE);
    assert!(matches!(
        load_startup_policy(&path, &tasks, rt.handle()),
        StartupPolicy::NotConfigured
    ));
}

#[test]
fn the_receipt_and_its_policy_reload_identically_after_a_restart() {
    let rt = runtime();
    let dir = tempfile::tempdir().unwrap();
    let id;
    let first_receipt;
    let first_budget;
    {
        let tasks = open(&rt, dir.path());
        id = answered(
            &rt,
            &tasks,
            &manifest("demo", "codex"),
            &subscription_reply("demo", "codex"),
        );
        first_receipt = rt
            .block_on(tasks.project_policy_receipt(&id, "demo", "codex"))
            .unwrap();
        let path = write_config(
            dir.path(),
            json!({"demo": {"provider": "codex", "approval_task": id}}),
        );
        let StartupPolicy::Attached { policy, .. } =
            load_startup_policy(&path, &tasks, rt.handle())
        else {
            panic!("the first start must attach");
        };
        first_budget = policy.entry("demo").unwrap().budget_json.clone();
    }

    // A new task actor over the same file: nothing from the first process survives in memory.
    let reopened = open(&rt, dir.path());
    let second_receipt = rt
        .block_on(reopened.project_policy_receipt(&id, "demo", "codex"))
        .unwrap();
    assert_eq!(first_receipt, second_receipt);
    assert_eq!(second_receipt.actor, "owner-pane");
    assert!(second_receipt.answer_event_id > second_receipt.question_version);
    let path = dir.path().join(PROJECT_RUNTIME_CONFIG_FILE);
    let StartupPolicy::Attached { policy, .. } = load_startup_policy(&path, &reopened, rt.handle())
    else {
        panic!("the restart must reload the same approval");
    };
    assert_eq!(policy.entry("demo").unwrap().budget_json, first_budget);
}
