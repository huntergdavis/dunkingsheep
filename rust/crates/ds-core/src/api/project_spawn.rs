use serde_json::{Map, Value};

use crate::error::DunkError;
use crate::flock::commands_inbox::OwnerMailboxReporter;
use crate::projects::newprojects::ProjectBudgetConfig;
use crate::projects::{Authority, Projects, Request};

use super::{Args, HandlerContext};

fn required_string(args: &Map<String, Value>, key: &str) -> Result<String, DunkError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| DunkError::Refused(format!("project_spawn {key} must be a string")))
}

/// Rust command adapter for `project_spawn`. All authority, budget, provider paths, and owner
/// approval come from the daemon adapter; the desk controls only the documented request fields.
pub fn h_project_spawn(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let config = context.env.project_spawn_config()?;
    if config.provider != "codex" {
        return Err(DunkError::Refused(
            "project template requires explicit Codex usage evidence".into(),
        ));
    }
    let template = match args.get("template") {
        None => "standard".to_string(),
        Some(Value::String(value)) => value.clone(),
        Some(_) => {
            return Err(DunkError::Refused(
                "project_spawn template must be a string".into(),
            ));
        }
    };
    let soft_cap =
        match args.get("soft_cap") {
            None | Some(Value::Null) => None,
            Some(Value::Number(value)) => Some(value.as_f64().ok_or_else(|| {
                DunkError::Refused("project_spawn soft_cap must be a number".into())
            })?),
            Some(_) => {
                return Err(DunkError::Refused(
                    "project_spawn soft_cap must be a number or null".into(),
                ));
            }
        };
    let request = Request {
        name: required_string(&args, "name")?,
        brief: required_string(&args, "brief")?,
        template,
        soft_cap,
    };
    // Reject missing or malformed budget configuration before opening the project store: opening
    // its SQLite writer creates the schema and is itself a durable write.
    let budget = ProjectBudgetConfig::from_json(&config.budget_json)?;
    if !budget.providers.contains_key(&config.provider) {
        return Err(DunkError::Refused(format!(
            "no budget policy for provider {}",
            config.provider
        )));
    }
    let authority = Authority::project_template(config.approved);
    let terminal_mutex = context.env.terminal();
    let mut terminal = terminal_mutex
        .lock()
        .map_err(|_| DunkError::Refused("terminal lock poisoned".into()))?;
    let mut response = None;
    context.env.with_inbox_env(&mut |inbox_env| {
        let reporter = OwnerMailboxReporter {
            flock: context.flock,
            env: inbox_env,
            self_pane_id: context.self_pane_id(),
        };
        let project = context.block_on(async {
            let mut projects = Projects::open_durable(&config.store_path).await?;
            let result = projects
                .spawn_from_sources_with_reporter(
                    &mut **terminal,
                    &config.project_root,
                    &request,
                    &authority,
                    &config.five_hour_usage,
                    &config.budget_json,
                    &config.provider,
                    &config.provider_home,
                    config.now,
                    &reporter,
                )
                .await;
            projects.shutdown().await;
            result
        })??;
        response = Some(serde_json::to_value(project)?);
        Ok(Value::Null)
    })?;
    response.ok_or_else(|| {
        DunkError::Refused("project spawn inbox adapter returned without a result".into())
    })
}
