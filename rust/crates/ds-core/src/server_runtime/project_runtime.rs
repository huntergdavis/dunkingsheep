//! Explicit project policy attached to the canonical Flock by its startup owner.
//!
//! This type has no environment/config-file defaults and does not select live values. A missing
//! attachment or missing project entry is a refusal before the project database is opened.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use crate::api::ProjectSpawnConfig;
use crate::error::{refusal, DunkError};
use crate::projects::newprojects::ProjectBudgetConfig;

#[derive(Debug, Clone)]
pub struct ProjectRuntimeEntry {
    /// Provider is selected by daemon policy, never by request JSON.
    pub provider: String,
    /// Full explicit project/provider budget object; required keys are validated at construction.
    pub budget_json: String,
    /// Explicit owner policy. `false` is retained and rejected at request time.
    pub approved: bool,
}

/// Typed runtime paths and project/provider selection. Construction validates every entry before
/// any durable store is opened. A single request gate serializes project admission against live
/// counts and prevents concurrent identical RPCs from starting duplicate workspaces or agents.
#[derive(Debug)]
pub struct ProjectRuntimePolicy {
    project_root: PathBuf,
    store_path: PathBuf,
    provider_homes: BTreeMap<String, PathBuf>,
    projects: BTreeMap<String, ProjectRuntimeEntry>,
    spawn_gate: Mutex<()>,
}

impl ProjectRuntimePolicy {
    pub fn new(
        project_root: PathBuf,
        store_path: PathBuf,
        provider_homes: BTreeMap<String, PathBuf>,
        projects: BTreeMap<String, ProjectRuntimeEntry>,
    ) -> Result<Self, DunkError> {
        if !project_root.is_absolute() || !project_root.is_dir() {
            return Err(refusal(
                "project runtime root must be an existing absolute directory",
            ));
        }
        let root = std::fs::canonicalize(&project_root)
            .map_err(|_| refusal("project runtime root is unavailable"))?;
        if !store_path.is_absolute() || store_path.as_os_str().is_empty() {
            return Err(refusal(
                "project runtime store path must be explicit and absolute",
            ));
        }
        let store_parent = store_path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .ok_or_else(|| refusal("project runtime store path has no parent"))?;
        if !store_parent.is_dir() {
            return Err(refusal("project runtime store parent must already exist"));
        }
        let store_name = store_path
            .file_name()
            .ok_or_else(|| refusal("project runtime store path has no file name"))?;
        let canonical_store_parent = std::fs::canonicalize(store_parent)
            .map_err(|_| refusal("project runtime store parent is unavailable"))?;
        let store_path = canonical_store_parent.join(store_name);
        if store_path.starts_with(&root) || root.starts_with(&store_path) {
            return Err(refusal(
                "project runtime store path must be separate from the project root",
            ));
        }
        match std::fs::symlink_metadata(&store_path) {
            Ok(metadata) if !metadata.file_type().is_file() => {
                return Err(refusal("project runtime store path must be a regular file"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(refusal("project runtime store path cannot be inspected")),
        }
        if provider_homes.is_empty() || projects.is_empty() {
            return Err(refusal(
                "project runtime requires explicit provider homes and project policies",
            ));
        }
        let mut canonical_homes = BTreeMap::new();
        for (provider, home) in provider_homes {
            if provider.trim().is_empty() || !home.is_absolute() || !home.is_dir() {
                return Err(refusal(format!(
                    "project runtime provider home for {provider:?} must be an existing absolute directory"
                )));
            }
            let canonical = std::fs::canonicalize(&home).map_err(|_| {
                refusal(format!(
                    "project runtime provider home for {provider} is unavailable"
                ))
            })?;
            canonical_homes.insert(provider, canonical);
        }
        for (name, entry) in &projects {
            let canonical_name = crate::projects::validate_name(name)?;
            if canonical_name.as_str() != name.as_str() {
                return Err(refusal(format!(
                    "project runtime policy key {name:?} must use its canonical project name"
                )));
            }
            if !canonical_homes.contains_key(&entry.provider) {
                return Err(refusal(format!(
                    "project {name}: provider home is not explicitly configured for {}",
                    entry.provider
                )));
            }
            if !matches!(
                entry.provider.as_str(),
                "codex" | "pi" | "opencode-go" | "claude"
            ) {
                return Err(refusal(format!(
                    "project {name}: no provider-specific usage reader for {}",
                    entry.provider
                )));
            }
            let budget = ProjectBudgetConfig::from_json(&entry.budget_json)?;
            if !budget.providers.contains_key(&entry.provider) {
                return Err(refusal(format!(
                    "project {name}: no budget policy for provider {}",
                    entry.provider
                )));
            }
        }
        if canonical_homes
            .values()
            .any(|home| store_path.starts_with(home) || home.starts_with(&store_path))
        {
            return Err(refusal(
                "project runtime store path must be separate from provider homes",
            ));
        }
        Ok(Self {
            project_root: root,
            store_path,
            provider_homes: canonical_homes,
            projects,
            spawn_gate: Mutex::new(()),
        })
    }

    pub(crate) fn with_spawn_gate<T>(
        &self,
        operation: impl FnOnce() -> Result<T, DunkError>,
    ) -> Result<T, DunkError> {
        let _guard = self
            .spawn_gate
            .lock()
            .map_err(|_| refusal("project runtime spawn gate is poisoned"))?;
        operation()
    }

    pub(crate) fn request_config(
        &self,
        project_name: &str,
        template: &str,
        runtime: &tokio::runtime::Handle,
    ) -> Result<ProjectSpawnConfig, DunkError> {
        if template != "standard" {
            return Err(refusal(format!(
                "unknown project template {template:?}; only 'standard' exists"
            )));
        }
        let entry = self.projects.get(project_name).ok_or_else(|| {
            refusal(format!(
                "project {project_name}: no explicit runtime policy is configured"
            ))
        })?;
        if !entry.approved {
            return Err(refusal(format!(
                "project {project_name}: owner approval is not configured"
            )));
        }
        let budget = ProjectBudgetConfig::from_json(&entry.budget_json)?;
        let provider_home = self.provider_homes.get(&entry.provider).ok_or_else(|| {
            refusal(format!(
                "project {project_name}: provider home is not configured for {}",
                entry.provider
            ))
        })?;
        let now = crate::clock::now_s();
        let reading =
            runtime.block_on(budget.read_provider(&entry.provider, provider_home, now))?;
        let five_hour_usage = budget.five_hour_usage_from_reading(&entry.provider, &reading, now);
        Ok(ProjectSpawnConfig {
            project_root: self.project_root.clone(),
            store_path: self.store_path.clone(),
            budget_json: entry.budget_json.clone(),
            provider: entry.provider.clone(),
            provider_home: provider_home.clone(),
            five_hour_usage,
            provider_reading: Some(reading),
            approved: entry.approved,
            now,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn entry(credit_enabled: bool) -> ProjectRuntimeEntry {
        ProjectRuntimeEntry {
            provider: "codex".into(),
            budget_json: json!({
                "credit_ceiling": null,
                "providers": {
                    "codex": {
                        "subscription": {
                            "window": "300min",
                            "max_used_percent": 70.0,
                            "max_age_seconds": 3600.0
                        },
                        "credit_fallback": {
                            "enabled": credit_enabled,
                            "limit": 25.0,
                            "max_age_seconds": 3600.0
                        }
                    }
                }
            })
            .to_string(),
            approved: true,
        }
    }

    #[test]
    fn project_selection_keeps_per_project_credit_policy_and_reads_usage_per_request() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("projects");
        let provider_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&project_root).unwrap();
        let sessions = provider_home.join(".codex/sessions");
        std::fs::create_dir_all(&sessions).unwrap();
        let measured_at = crate::clock::now_s();
        std::fs::write(
            sessions.join("session.jsonl"),
            json!({
                "timestamp": measured_at,
                "payload": {"rate_limits": {"primary": {
                    "used_percent": 20.0,
                    "window_minutes": 300
                }}}
            })
            .to_string(),
        )
        .unwrap();
        let store_path = dir.path().join("projects.sqlite3");
        let policy = ProjectRuntimePolicy::new(
            project_root,
            store_path.clone(),
            BTreeMap::from([("codex".into(), provider_home)]),
            BTreeMap::from([
                ("project-one".into(), entry(false)),
                ("project-two".into(), entry(true)),
            ]),
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let first = policy
            .request_config("project-one", "standard", runtime.handle())
            .unwrap();
        let second = policy
            .request_config("project-two", "standard", runtime.handle())
            .unwrap();
        assert_eq!(first.five_hour_usage, crate::projects::Usage::Percent(20.0));
        assert!(first.provider_reading.is_some());
        assert!(
            !ProjectBudgetConfig::from_json(&first.budget_json)
                .unwrap()
                .providers["codex"]
                .credit_fallback
                .0
                .as_ref()
                .unwrap()
                .enabled
        );
        assert!(
            ProjectBudgetConfig::from_json(&second.budget_json)
                .unwrap()
                .providers["codex"]
                .credit_fallback
                .0
                .as_ref()
                .unwrap()
                .enabled
        );
        let unknown = policy
            .request_config("unknown-project", "standard", runtime.handle())
            .unwrap_err();
        assert!(unknown.to_string().contains("no explicit runtime policy"));
        assert!(!store_path.exists());
        runtime.shutdown_background();
    }

    #[test]
    fn policy_rejects_missing_budget_and_unapproved_project_before_store_creation() {
        let dir = tempfile::tempdir().unwrap();
        let project_root = dir.path().join("projects");
        let provider_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::create_dir_all(&provider_home).unwrap();
        let store_path = dir.path().join("projects.sqlite3");
        let mut invalid = entry(true);
        invalid.budget_json = r#"{"providers":{}}"#.into();
        let error = ProjectRuntimePolicy::new(
            project_root.clone(),
            store_path.clone(),
            BTreeMap::from([("codex".into(), provider_home.clone())]),
            BTreeMap::from([("project-one".into(), invalid)]),
        )
        .unwrap_err();
        assert_eq!(error.to_string(), "project budget not configured");
        assert!(!store_path.exists());

        let mut unapproved = entry(true);
        unapproved.approved = false;
        let policy = ProjectRuntimePolicy::new(
            project_root,
            store_path.clone(),
            BTreeMap::from([("codex".into(), provider_home)]),
            BTreeMap::from([("project-one".into(), unapproved)]),
        )
        .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        let error = policy
            .request_config("project-one", "standard", runtime.handle())
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("owner approval is not configured"));
        assert!(!store_path.exists());
        runtime.shutdown_background();
    }
}
