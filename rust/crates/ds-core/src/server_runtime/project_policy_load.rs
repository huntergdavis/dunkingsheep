//! Startup composition of the owner-approved project policy (t1692).
//!
//! `project-runtime.json` in the captured config directory is the only source of the project
//! root, the projects store, the provider homes, and each project's provider and approval task.
//! Nothing is inferred or defaulted. A project enters the policy only when
//! `Tasks::project_policy_receipt` returns a current, attributed owner receipt for that exact
//! project and provider, and its budget is taken from that receipt. `RuntimeServices` performs
//! this once while it is built and attaches the result to the canonical Flock before serving.
//!
//! An absent file leaves the policy unattached. An unreadable or invalid file, or a receipt that
//! is missing, stale, malformed, ambiguous or unattributed, never fails daemon startup: the
//! affected project, or the whole policy, stays unattached and the reason is recorded so that
//! `project_spawn` can name it. Nothing here opens the projects store or starts an agent.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use tokio::runtime::Handle;

use super::{ProjectRuntimeEntry, ProjectRuntimePolicy};
use crate::config::{self, Env};
use crate::error::{refusal, DunkError};
use crate::flock::Flock;
use crate::tasks::Tasks;

/// The daemon-owned startup file, inside the captured config directory.
pub const PROJECT_RUNTIME_CONFIG_FILE: &str = "project-runtime.json";
const CONFIG_SCHEMA: &str = "project-runtime/v1";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeConfig {
    schema: String,
    project_root: PathBuf,
    store_path: PathBuf,
    provider_homes: BTreeMap<String, PathBuf>,
    projects: BTreeMap<String, ProjectConfig>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProjectConfig {
    provider: String,
    approval_task: String,
}

/// The outcome of reading the startup file once.
pub(crate) enum StartupPolicy {
    /// No startup file exists, so project_spawn stays unattached and refuses.
    NotConfigured,
    /// At least one project has a current owner receipt. `refusal` names every project that did
    /// not, so that a partial policy is never silently partial.
    Attached {
        policy: ProjectRuntimePolicy,
        refusal: Option<String>,
    },
    /// Nothing may be attached. The reason is what project_spawn reports.
    Refused(String),
}

/// Read the startup file and admit only the projects whose owner receipts are current.
pub(crate) fn load_startup_policy(
    config_path: &Path,
    tasks: &Tasks,
    runtime: &Handle,
) -> StartupPolicy {
    let raw = match std::fs::read_to_string(config_path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return StartupPolicy::NotConfigured;
        }
        Err(_) => return StartupPolicy::Refused("project runtime config cannot be read".into()),
    };
    let config: RuntimeConfig = match serde_json::from_str(&raw) {
        Ok(config) => config,
        Err(_) => return StartupPolicy::Refused("project runtime config is invalid".into()),
    };
    if config.schema != CONFIG_SCHEMA {
        return StartupPolicy::Refused("project runtime config version is unsupported".into());
    }
    if config.projects.is_empty() {
        return StartupPolicy::Refused("project runtime config names no projects".into());
    }
    // Receipts are read by blocking on the daemon's own runtime. That is sound only from a
    // synchronous startup thread; inside a runtime it would panic, so refuse instead.
    if tokio::runtime::Handle::try_current().is_ok() {
        return StartupPolicy::Refused(
            "project runtime policy must be loaded from a synchronous startup context".into(),
        );
    }

    let mut approved = BTreeMap::new();
    let mut refusals = Vec::new();
    for (name, project) in &config.projects {
        let receipt = runtime.block_on(tasks.project_policy_receipt(
            &project.approval_task,
            name,
            &project.provider,
        ));
        match receipt {
            Ok(receipt) => {
                // `approved` is true only because this exact project and provider has a receipt
                // that the task store currently attributes to an owner answer.
                approved.insert(
                    name.clone(),
                    ProjectRuntimeEntry {
                        provider: project.provider.clone(),
                        budget_json: receipt.answer.budget_json,
                        approved: true,
                    },
                );
            }
            Err(error) => refusals.push(format!("project {name}: {error}")),
        }
    }
    if approved.is_empty() {
        return StartupPolicy::Refused(format!(
            "no project has a current owner policy: {}",
            refusals.join("; ")
        ));
    }
    let refusal = (!refusals.is_empty()).then(|| refusals.join("; "));
    match ProjectRuntimePolicy::new(
        config.project_root,
        config.store_path,
        config.provider_homes,
        approved,
    ) {
        Ok(policy) => StartupPolicy::Attached { policy, refusal },
        Err(error) => StartupPolicy::Refused(error.to_string()),
    }
}

/// Attach the startup policy to the canonical Flock. Called exactly once while `RuntimeServices`
/// is built, so no request can observe a policy that was not loaded from the startup file.
pub(crate) fn attach_startup_policy(
    flock: &Flock,
    tasks: &Tasks,
    env: &Env,
    runtime: &Handle,
) -> Result<(), DunkError> {
    let config_path = match config::config_dir(env) {
        Ok(dir) => dir.join(PROJECT_RUNTIME_CONFIG_FILE),
        Err(error) => {
            flock.attach_project_runtime_refusal(format!(
                "project runtime config directory is unavailable: {error}"
            ));
            return Ok(());
        }
    };
    match load_startup_policy(&config_path, tasks, runtime) {
        StartupPolicy::NotConfigured => Ok(()),
        StartupPolicy::Refused(reason) => {
            flock.attach_project_runtime_refusal(reason);
            Ok(())
        }
        StartupPolicy::Attached { policy, refusal } => {
            flock.attach_project_runtime_policy(Arc::new(policy))?;
            if let Some(reason) = refusal {
                flock.attach_project_runtime_refusal(reason);
            }
            Ok(())
        }
    }
}

/// The refusal for a project_spawn request that needs the attached policy and finds none. It
/// names the startup reason when one was recorded.
pub(crate) fn not_attached_refusal(flock: &Flock) -> DunkError {
    match flock.project_runtime_policy_refusal() {
        Some(reason) => refusal(format!("project runtime policy is not attached: {reason}")),
        None => refusal("project runtime policy is not attached"),
    }
}

#[cfg(test)]
#[path = "project_policy_load_tests.rs"]
mod tests;
