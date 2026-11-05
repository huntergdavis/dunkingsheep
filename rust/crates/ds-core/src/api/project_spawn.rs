use serde_json::{Map, Value};

use crate::error::DunkError;
use crate::flock::commands_inbox::OwnerMailboxReporter;
use crate::projects::newprojects::ProjectBudgetConfig;
use crate::projects::{
    Authority, ProjectAgentRegistrar, ProjectTerminal, Projects, Request, Usage,
};
use crate::terminal::Pane;

use super::{Args, HandlerContext};

fn required_string(args: &Map<String, Value>, key: &str) -> Result<String, DunkError> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| DunkError::Refused(format!("project_spawn {key} must be a string")))
}

struct RuntimeProjectRegistrar<'a> {
    flock: &'a crate::flock::Flock,
}

impl ProjectAgentRegistrar for RuntimeProjectRegistrar<'_> {
    fn register(
        &self,
        role: &str,
        pane: &Pane,
        provider: &str,
        model: &str,
        label: &str,
    ) -> Result<(), String> {
        let pane_id = pane.pane_id.trim();
        if pane_id.is_empty() {
            return Err("started project pane has no pane id".into());
        }
        let pid = pane
            .pane_pid
            .ok_or_else(|| format!("started project pane {pane_id} has no pane PID"))?;
        let start = crate::proc::process_start(pid).ok_or_else(|| {
            format!("started project pane {pane_id} has no process start identity")
        })?;
        if !crate::proc::process_alive(pid, Some(&start)) {
            return Err(format!(
                "started project pane {pane_id} PID/start identity is not live"
            ));
        }
        let registered_role = match role {
            // `Roles` uses `worker` for the worker lane; the durable label retains the narrower
            // project-template role so writer and reviewer remain distinguishable.
            "writer" => "worker",
            "reviewer" => "reviewer",
            _ => return Err(format!("unsupported project agent role {role}")),
        };
        self.flock
            .with_roles(|roles, _| {
                let entry = roles.register(
                    pane_id,
                    registered_role,
                    Some(pid),
                    Some(&start),
                    Some(provider),
                    Some(model),
                    Some("project_spawn"),
                    Some(label),
                )?;
                let matches = entry.get("role").and_then(Value::as_str)
                    == Some(registered_role)
                    && entry.get("pane_pid").and_then(Value::as_i64) == Some(pid)
                    && entry.get("start").and_then(Value::as_str) == Some(start.as_str())
                    && entry.get("provider").and_then(Value::as_str) == Some(provider)
                    && entry.get("model").and_then(Value::as_str) == Some(model)
                    && entry.get("label").and_then(Value::as_str) == Some(label);
                if !matches {
                    return Err(DunkError::Refused(format!(
                        "project pane {pane_id} role registration receipt did not match requested identity"
                    )));
                }
                Ok(())
            })
            .map_err(|error| error.to_string())
    }
}

/// Rust command adapter for `project_spawn`. The request selects only an explicitly configured
/// name and the bounded template; authority, budget, provider paths, argv and approval are
/// daemon-owned. All policy and usage checks happen before opening the durable store.
pub fn h_project_spawn(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    const FIELDS: [&str; 4] = ["name", "brief", "template", "soft_cap"];
    if let Some(field) = args.keys().find(|field| !FIELDS.contains(&field.as_str())) {
        return Err(DunkError::Refused(format!(
            "project_spawn does not accept caller-controlled field {field:?}"
        )));
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
    if let Some(cap) = soft_cap {
        if !cap.is_finite() || cap <= 0.0 {
            return Err(DunkError::Refused(format!(
                "soft_cap must be a positive number; got {cap}"
            )));
        }
    }
    let request = Request {
        name: crate::projects::validate_name(&required_string(&args, "name")?)?,
        brief: required_string(&args, "brief")?,
        template: template.clone(),
        soft_cap,
    };
    let mut response = None;
    context
        .env
        .with_project_spawn_lock(&request.name, &mut || {
            let config = context
                .env
                .project_spawn_config_for(&request.name, &request.template)?;
            if !config.approved {
                return Err(DunkError::Refused(format!(
                    "project {}: owner approval is not configured",
                    request.name
                )));
            }
            if config.provider != "codex" {
                return Err(DunkError::Refused(
                    "project template requires an explicitly configured Codex usage policy".into(),
                ));
            }
            let budget = ProjectBudgetConfig::from_json(&config.budget_json)?;
            if !budget.providers.contains_key(&config.provider) {
                return Err(DunkError::Refused(format!(
                    "no budget policy for provider {}",
                    config.provider
                )));
            }
            if !config.project_root.is_absolute()
                || !config.store_path.is_absolute()
                || !config.provider_home.is_absolute()
                || !config.project_root.is_dir()
                || !config.provider_home.is_dir()
                || !config.now.is_finite()
            {
                return Err(DunkError::Refused(
                    "project runtime paths and request time must be explicitly valid".into(),
                ));
            }
            let reading = match config.provider_reading.clone() {
                Some(reading) => reading,
                None => context.block_on(budget.read_provider(
                    &config.provider,
                    &config.provider_home,
                    config.now,
                ))??,
            };
            let five_hour_usage = if config.provider_reading.is_some() {
                budget.five_hour_usage_from_reading(&config.provider, &reading, config.now)
            } else {
                // This branch exists for command fixtures and legacy typed callers. The
                // production RequestRuntimeAdapter always supplies a fresh source reading.
                config.five_hour_usage.clone()
            };
            budget.admit(
                &request.name,
                &config.provider,
                &five_hour_usage,
                &reading,
                config.now,
            )?;

            // Verify canonical runtime services before SQLite creates its schema.
            context.flock.with_roles(|_, _| Ok(()))?;
            context.env.with_inbox_env(&mut |_| Ok(Value::Null))?;

            let authority = Authority::project_template(config.approved);
            let registrar = RuntimeProjectRegistrar {
                flock: context.flock,
            };
            let mut run = |terminal: &mut dyn ProjectTerminal| {
                let mut with_inbox = |inbox_env: &dyn crate::flock::commands_inbox::InboxEnv| {
                    let reporter = OwnerMailboxReporter {
                        flock: context.flock,
                        env: inbox_env,
                        self_pane_id: context.self_pane_id(),
                    };
                    let project = context.block_on(async {
                        let mut projects = Projects::open_durable(&config.store_path).await?;
                        let result = projects
                            .spawn_from_reading_with_reporter_and_registrar(
                                terminal,
                                &config.project_root,
                                &request,
                                &authority,
                                &five_hour_usage,
                                &config.budget_json,
                                &config.provider,
                                &reading,
                                config.now,
                                &reporter,
                                &registrar,
                            )
                            .await;
                        projects.shutdown().await;
                        result
                    })??;
                    response = Some(serde_json::to_value(project)?);
                    Ok(Value::Null)
                };
                context.env.with_inbox_env(&mut with_inbox)?;
                Ok(())
            };
            context.env.with_project_terminal(&mut run)?;
            Ok(())
        })?;
    response
        .ok_or_else(|| DunkError::Refused("project spawn runtime returned without a result".into()))
}
