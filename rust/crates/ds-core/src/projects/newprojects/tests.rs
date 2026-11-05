use super::*;
use crate::error::DunkError;
use crate::projects::{Authority, ProjectAgentRegistrar, Projects, Request, State, Usage};
use crate::terminal::{fakes::FakeTerminal, Pane};
use serde_json::{json, Value};
use std::fs;
use std::path::Path;

fn configured(credit_fallback: Value) -> String {
    json!({
        "credit_ceiling": null,
        "providers": {
            "codex": {
                "subscription": {
                    "window": "weekly",
                    "max_used_percent": 70.0,
                    "max_age_seconds": 60.0
                },
                "credit_fallback": credit_fallback
            }
        }
    })
    .to_string()
}

fn codex_fixture(home: &std::path::Path, timestamp: f64, used_percent: Value) -> Value {
    let sessions = home.join(".codex/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("session.jsonl"),
        json!({
            "timestamp": timestamp,
            "payload": { "rate_limits": { "primary": {
                "used_percent": used_percent,
                "window_minutes": 10080
            }}}
        })
        .to_string(),
    )
    .unwrap();
    crate::usage_providers::codex(home, 60.0, timestamp + 1.0)
}

fn request() -> Request {
    Request {
        name: "budget-app".into(),
        brief: "test project".into(),
        template: "standard".into(),
        soft_cap: None,
    }
}

#[test]
fn missing_five_hour_usage_never_becomes_zero_headroom() {
    let budget = ProjectBudgetConfig::from_json(&configured(Value::Null)).unwrap();
    let reading = json!({
        "provider":"codex",
        "status":"ok",
        "meaning":"percent_used",
        "source":"fixture",
        "measured_at":1000.0,
        "windows":[{"name":"300min","window_minutes":300}]
    });
    assert!(matches!(
        budget.five_hour_usage_from_reading("codex", &reading, 1001.0),
        Usage::Unknown(_)
    ));
}

#[test]
fn synchronous_spawn_without_budget_refuses_before_writes() {
    let root = tempfile::tempdir().unwrap();
    let mut terminal = FakeTerminal::new();
    let error = Projects::new()
        .spawn(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(1.0),
        )
        .unwrap_err()
        .to_string();
    assert_eq!(error, "project budget not configured");
    assert!(terminal.layout_calls.is_empty());
    assert!(!root.path().join("budget-app").exists());
}

fn authority() -> Authority {
    Authority::project_template(true)
}

#[derive(Debug, Clone, Default)]
struct DurableReporter {
    fail_next: std::sync::Arc<std::sync::atomic::AtomicBool>,
    calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl crate::projects::Reporter for DurableReporter {
    fn report(&self, _project: &str, _body: &str) -> Result<String, String> {
        if self
            .fail_next
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err("owner mailbox unavailable".into());
        }
        let id = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        Ok(format!("i-{id}"))
    }
}

async fn open_with_reporter(path: &Path) -> Result<Projects, DunkError> {
    Projects::open_durable_with_reporter(path, Box::new(DurableReporter::default())).await
}

#[test]
fn required_config_has_no_implicit_budget_or_policy_defaults() {
    assert_eq!(
        ProjectBudgetConfig::from_json(r#"{"providers":{}}"#)
            .unwrap_err()
            .to_string(),
        "project budget not configured"
    );
    assert_eq!(
        ProjectBudgetConfig::from_json(
            r#"{"credit_ceiling":null,"providers":{"codex":{"subscription":null}}}"#
        )
        .unwrap_err()
        .to_string(),
        "project budget not configured",
        "the explicit credit policy key is required, even when credits are prohibited"
    );
    assert!(ProjectBudgetConfig::from_json(&configured(Value::Null)).is_ok());
}

#[test]
fn subscription_admission_consumes_real_codex_provider_fixture_and_rejects_unknowns() {
    let home = tempfile::tempdir().unwrap();
    let config = ProjectBudgetConfig::from_json(&configured(Value::Null)).unwrap();
    let reading = codex_fixture(home.path(), 1000.0, json!(15.0));
    assert_eq!(reading["status"], "ok");
    assert!(config
        .admit(
            "budget-app",
            "codex",
            &Usage::Percent(69.9),
            &reading,
            1001.0
        )
        .is_ok());

    let malformed = codex_fixture(home.path(), 1000.0, json!("15"));
    assert_eq!(malformed["status"], "error");
    assert!(config
        .admit(
            "budget-app",
            "codex",
            &Usage::Percent(10.0),
            &malformed,
            1001.0
        )
        .is_err());
    let mut wrong_meaning = reading.clone();
    wrong_meaning["meaning"] = json!("unknown");
    assert!(config
        .admit(
            "budget-app",
            "codex",
            &Usage::Percent(10.0),
            &wrong_meaning,
            1001.0
        )
        .is_err());
    let mut unknown = reading.clone();
    unknown["status"] = json!("unknown");
    assert!(config
        .admit(
            "budget-app",
            "codex",
            &Usage::Percent(10.0),
            &unknown,
            1001.0
        )
        .is_err());
    assert!(config
        .admit(
            "budget-app",
            "codex",
            &Usage::Percent(70.0),
            &reading,
            1001.0
        )
        .unwrap_err()
        .to_string()
        .contains("five-hour usage is 70%"));
    assert!(
        config
            .admit(
                "budget-app",
                "codex",
                &Usage::Percent(10.0),
                &reading,
                1061.0
            )
            .is_err(),
        "stale provider evidence is not headroom"
    );
}

#[tokio::test]
async fn strict_codex_adapter_does_not_turn_unrepresentable_values_into_zero_headroom() {
    let home = tempfile::tempdir().unwrap();
    let sessions = home.path().join(".codex/sessions");
    fs::create_dir_all(&sessions).unwrap();
    fs::write(
        sessions.join("session.jsonl"),
        r#"{"timestamp":1000,"payload":{"rate_limits":{"primary":{"used_percent":1e1000,"window_minutes":10080}}}}"#,
    )
    .unwrap();
    let config = ProjectBudgetConfig::from_json(&configured(Value::Null)).unwrap();
    let reading = config
        .read_provider("codex", home.path(), 1001.0)
        .await
        .unwrap();
    assert_eq!(reading["status"], "unknown");
    assert!(config
        .admit(
            "budget-app",
            "codex",
            &Usage::Percent(10.0),
            &reading,
            1001.0
        )
        .is_err());
}

#[test]
fn credit_fallback_requires_explicit_enablement_and_project_attribution() {
    let denied = ProjectBudgetConfig::from_json(&configured(json!({
        "enabled": false,
        "limit": 10.0,
        "max_age_seconds": 60.0
    })))
    .unwrap();
    let account_balance = json!({
        "provider": "codex",
        "status": "ok",
        "credits": {"balance": 1000.0}
    });
    assert!(denied
        .admit(
            "budget-app",
            "codex",
            &Usage::Percent(10.0),
            &account_balance,
            1.0
        )
        .is_err());

    let enabled = ProjectBudgetConfig::from_json(&configured(json!({
        "enabled": true,
        "limit": 10.0,
        "max_age_seconds": 60.0
    })))
    .unwrap();
    assert!(
        enabled
            .admit(
                "budget-app",
                "codex",
                &Usage::Percent(10.0),
                &account_balance,
                1.0
            )
            .is_err(),
        "account balances/deltas cannot be attributed to a project"
    );
    // None of the current provider readers yields project-scoped credit usage. Enabling the
    // fallback therefore remains fail-closed until such a real source is added.
}

#[tokio::test]
async fn durable_spawn_recovers_events_and_live_retry_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("owned-projects.sqlite3");
    let provider_home = tempfile::tempdir().unwrap();
    let _ = codex_fixture(provider_home.path(), 1000.0, json!(15.0));
    let config = configured(Value::Null);
    let mut terminal = FakeTerminal::new();
    let mut projects = open_with_reporter(&db).await.unwrap();
    let live = projects
        .spawn_from_sources(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await
        .unwrap();
    assert_eq!(live.state, State::Live);
    let terminal_calls = terminal.layout_calls.len();
    let durable_events = projects
        .store
        .as_ref()
        .unwrap()
        .events("budget-app".into())
        .await
        .unwrap();
    assert!(durable_events
        .iter()
        .any(|event| event.step == "workspace_started"));
    assert!(durable_events
        .iter()
        .any(|event| event.step == "writer_agent_started"));
    assert!(durable_events
        .iter()
        .any(|event| event.step == "reviewer_agent_started"));
    projects.shutdown().await;

    let mut recovered = Projects::open_durable(&db).await.unwrap();
    assert_eq!(recovered.get("budget-app").unwrap().state, State::Live);
    let again = recovered
        .spawn_from_sources(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(99.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await
        .unwrap();
    assert_eq!(again, live);
    assert_eq!(terminal.layout_calls.len(), terminal_calls);
    recovered.shutdown().await;
}

#[tokio::test]
async fn durable_owner_report_failure_is_uncertain_and_never_replayed() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("owned-projects.sqlite3");
    let provider_home = tempfile::tempdir().unwrap();
    let _ = codex_fixture(provider_home.path(), 1000.0, json!(15.0));
    let config = configured(Value::Null);
    let reporter = DurableReporter {
        fail_next: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        ..DurableReporter::default()
    };
    let mut terminal = FakeTerminal::new();
    let mut projects = Projects::open_durable_with_reporter(&db, Box::new(reporter.clone()))
        .await
        .unwrap();
    let failed = projects
        .spawn_from_sources(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(failed.contains("owner mailbox unavailable"), "{failed}");
    assert_eq!(State::Partial, projects.get("budget-app").unwrap().state);
    let calls_before_retry = terminal.layout_calls.len();
    let events = projects
        .store
        .as_ref()
        .unwrap()
        .events("budget-app".into())
        .await
        .unwrap();
    assert!(events
        .iter()
        .any(|event| event.step == "reported_uncertain" && !event.ok));
    assert!(!events
        .iter()
        .any(|event| event.step == "reported" && event.ok));
    projects.shutdown().await;

    let mut recovered = Projects::open_durable_with_reporter(&db, Box::new(reporter.clone()))
        .await
        .unwrap();
    let retry = recovered
        .spawn_from_sources(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(retry.contains("outcome is uncertain"), "{retry}");
    assert_eq!(State::Partial, recovered.get("budget-app").unwrap().state);
    assert_eq!(2, recovered.get("budget-app").unwrap().agents.len());
    assert_eq!(calls_before_retry, terminal.layout_calls.len());
    assert_eq!(1, reporter.calls.load(std::sync::atomic::Ordering::SeqCst));
    let events = recovered
        .store
        .as_ref()
        .unwrap()
        .events("budget-app".into())
        .await
        .unwrap();
    assert_eq!(
        0,
        events
            .iter()
            .filter(|event| event.step == "reported" && event.ok)
            .count()
    );
    recovered.shutdown().await;
}

#[derive(Debug)]
struct FailedProjectRoleRegistration;

impl ProjectAgentRegistrar for FailedProjectRoleRegistration {
    fn register(
        &self,
        _role: &str,
        _pane: &Pane,
        _provider: &str,
        _model: &str,
        _label: &str,
    ) -> Result<(), String> {
        Err("role store refused the receipt".into())
    }
}

#[tokio::test]
async fn failed_project_role_registration_keeps_pane_and_never_restarts_it() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("owned-projects.sqlite3");
    let provider_home = tempfile::tempdir().unwrap();
    let reading = codex_fixture(provider_home.path(), 1000.0, json!(15.0));
    let config = configured(Value::Null);
    let registrar = FailedProjectRoleRegistration;
    let reporter = DurableReporter::default();
    let mut terminal = FakeTerminal::new();
    let mut projects = Projects::open_durable(&db).await.unwrap();
    let failure = projects
        .spawn_from_reading_with_reporter_and_registrar(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            &reading,
            1001.0,
            &reporter,
            &registrar,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(
        failure.contains("role registration is uncertain"),
        "{failure}"
    );
    let project = projects.get("budget-app").unwrap();
    assert_eq!(Some("writer_agent"), project.pending_step.as_deref());
    assert_eq!(1, project.agents.len());
    assert!(projects
        .store
        .as_ref()
        .unwrap()
        .events("budget-app".into())
        .await
        .unwrap()
        .iter()
        .any(|event| event.step == "writer_agent_uncertain" && !event.ok));
    let started_before_recovery = terminal
        .panes
        .iter()
        .filter(|pane| pane.label == "budget-app-writer")
        .count();
    assert_eq!(1, started_before_recovery);
    projects.shutdown().await;

    let mut recovered = Projects::open_durable(&db).await.unwrap();
    let retry = recovered
        .spawn_from_reading_with_reporter_and_registrar(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            &reading,
            1001.0,
            &reporter,
            &registrar,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(retry.contains("outcome is uncertain"), "{retry}");
    assert_eq!(
        started_before_recovery,
        terminal
            .panes
            .iter()
            .filter(|pane| pane.label == "budget-app-writer")
            .count()
    );
    recovered.shutdown().await;
}

#[tokio::test]
async fn failed_external_step_is_durable_and_retry_does_not_repeat_completed_steps() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("owned-projects.sqlite3");
    let provider_home = tempfile::tempdir().unwrap();
    let _ = codex_fixture(provider_home.path(), 1000.0, json!(15.0));
    let config = configured(Value::Null);
    let mut terminal = FakeTerminal::new();
    terminal.fail_layout = Some("workspace offline".into());
    let mut projects = open_with_reporter(&db).await.unwrap();
    let failure = projects
        .spawn_from_sources(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(failure.contains("step workspace failed"), "{failure}");
    let events = projects
        .store
        .as_ref()
        .unwrap()
        .events("budget-app".into())
        .await
        .unwrap();
    assert!(events
        .iter()
        .any(|event| event.step == "workspace" && !event.ok));
    assert_eq!(
        events
            .iter()
            .filter(|event| event.step == "scaffolded" && event.ok)
            .count(),
        1
    );
    projects.shutdown().await;

    let mut recovered = open_with_reporter(&db).await.unwrap();
    terminal.fail_layout = None;
    let live = recovered
        .spawn_from_sources(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await
        .unwrap();
    assert_eq!(live.state, State::Live);
    let events = recovered
        .store
        .as_ref()
        .unwrap()
        .events("budget-app".into())
        .await
        .unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.step == "scaffolded" && event.ok)
            .count(),
        1,
        "the completed filesystem step is not replayed"
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event.step == "workspace_started")
            .count(),
        2,
        "failed workspace step intent is recorded before both attempts"
    );
    recovered.shutdown().await;
}

#[tokio::test]
async fn failed_result_write_keeps_intent_uncertain_in_memory_and_prevents_replay() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("owned-projects.sqlite3");
    let provider_home = tempfile::tempdir().unwrap();
    let _ = codex_fixture(provider_home.path(), 1000.0, json!(15.0));
    let config = configured(Value::Null);
    let mut projects = open_with_reporter(&db).await.unwrap();
    let connection = rusqlite::Connection::open(&db).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER reject_workspace_result BEFORE INSERT ON project_events
             WHEN NEW.step='workspace'
             BEGIN SELECT RAISE(ABORT, 'injected event failure'); END;",
        )
        .unwrap();
    drop(connection);
    let mut terminal = FakeTerminal::new();

    let first = projects
        .spawn_from_sources(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(first.contains("injected event failure"), "{first}");
    assert_eq!(
        projects.get("budget-app").unwrap().pending_step.as_deref(),
        Some("workspace")
    );
    let calls_after_first = terminal.layout_calls.len();

    let second = projects
        .spawn_from_sources(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(second.contains("outcome is uncertain"), "{second}");
    assert_eq!(terminal.layout_calls.len(), calls_after_first);
    projects.shutdown().await;
}

#[tokio::test]
async fn restart_with_an_unresolved_external_intent_refuses_without_replay() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("owned-projects.sqlite3");
    let provider_home = tempfile::tempdir().unwrap();
    let _ = codex_fixture(provider_home.path(), 1000.0, json!(15.0));
    let config = configured(Value::Null);
    let projects = Projects::open_durable(&db).await.unwrap();
    let pending = crate::projects::Project {
        name: "budget-app".into(),
        dir: root.path().join("budget-app"),
        brief: "test project".into(),
        soft_cap: None,
        state: State::Partial,
        workspace_id: None,
        agents: Vec::new(),
        events: Vec::new(),
        pending_step: Some("writer_agent".into()),
    };
    projects
        .store
        .as_ref()
        .unwrap()
        .save(pending.clone(), None)
        .await
        .unwrap();
    projects.shutdown().await;

    let mut recovered = Projects::open_durable(&db).await.unwrap();
    let mut terminal = FakeTerminal::new();
    let result = recovered
        .spawn_from_sources(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await;
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("outcome is uncertain"));
    assert!(terminal.layout_calls.is_empty());
    recovered.shutdown().await;
}

#[tokio::test]
async fn missing_budget_refuses_before_project_state_or_directory_writes() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("owned-projects.sqlite3");
    let mut projects = Projects::open_durable(&db).await.unwrap();
    let error = projects
        .spawn_from_sources(
            &mut FakeTerminal::new(),
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(1.0),
            "{}",
            "codex",
            root.path(),
            1000.0,
        )
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "project budget not configured");
    assert!(projects.get("budget-app").is_none());
    assert!(!root.path().join("budget-app").exists());
    assert!(projects
        .store
        .as_ref()
        .unwrap()
        .load()
        .await
        .unwrap()
        .is_empty());
    projects.shutdown().await;
}

#[tokio::test]
async fn approved_durable_spawn_requires_owner_reporter_before_project_writes() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("owned-projects.sqlite3");
    let provider_home = tempfile::tempdir().unwrap();
    let _ = codex_fixture(provider_home.path(), 1000.0, json!(15.0));
    let config = configured(Value::Null);
    let mut projects = Projects::open_durable(&db).await.unwrap();
    let error = projects
        .spawn_from_sources(
            &mut FakeTerminal::new(),
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(error, "owner mailbox reporter is not wired");
    assert!(projects.get("budget-app").is_none());
    assert!(!root.path().join("budget-app").exists());
    assert!(projects
        .store
        .as_ref()
        .unwrap()
        .load()
        .await
        .unwrap()
        .is_empty());
    projects.shutdown().await;
}

#[tokio::test]
async fn a_foreign_project_directory_is_never_overwritten() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("owned-projects.sqlite3");
    let foreign = root.path().join("budget-app");
    fs::create_dir(&foreign).unwrap();
    fs::write(foreign.join("BRIEF.md"), "owner data").unwrap();
    let provider_home = tempfile::tempdir().unwrap();
    let _ = codex_fixture(provider_home.path(), 1000.0, json!(15.0));
    let config = configured(Value::Null);
    let mut projects = open_with_reporter(&db).await.unwrap();
    assert!(projects
        .spawn_from_sources(
            &mut FakeTerminal::new(),
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await
        .is_err());
    assert_eq!(
        fs::read_to_string(foreign.join("BRIEF.md")).unwrap(),
        "owner data"
    );
    assert!(projects.get("budget-app").is_none());
    projects.shutdown().await;
}

#[tokio::test]
async fn a_recovered_project_path_outside_the_approved_root_is_refused() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let db = root.path().join("owned-projects.sqlite3");
    let foreign = outside.path().join("foreign-project");
    fs::create_dir(&foreign).unwrap();
    fs::write(foreign.join("BRIEF.md"), "owner data").unwrap();
    let provider_home = tempfile::tempdir().unwrap();
    let _ = codex_fixture(provider_home.path(), 1000.0, json!(15.0));
    let config = configured(Value::Null);
    let projects = Projects::open_durable(&db).await.unwrap();
    projects
        .store
        .as_ref()
        .unwrap()
        .save(
            crate::projects::Project {
                name: "budget-app".into(),
                dir: foreign.clone(),
                brief: "test project".into(),
                soft_cap: None,
                state: State::Partial,
                workspace_id: None,
                agents: Vec::new(),
                events: Vec::new(),
                pending_step: None,
            },
            None,
        )
        .await
        .unwrap();
    projects.shutdown().await;

    let mut recovered = open_with_reporter(&db).await.unwrap();
    let mut terminal = FakeTerminal::new();
    let error = recovered
        .spawn_from_sources(
            &mut terminal,
            root.path(),
            &request(),
            &authority(),
            &Usage::Percent(10.0),
            &config,
            "codex",
            provider_home.path(),
            1001.0,
        )
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("does not match approved root"), "{error}");
    assert!(terminal.layout_calls.is_empty());
    assert_eq!(
        fs::read_to_string(foreign.join("BRIEF.md")).unwrap(),
        "owner data"
    );
    recovered.shutdown().await;
}
