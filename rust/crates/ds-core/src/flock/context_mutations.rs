//! Concrete context/backfill commands using startup config and the canonical memory service.
use super::Flock;
use crate::api::{python_value_repr, python_value_string, truthy, Args};
use crate::context::{
    ledger::Ledger,
    store::{Store, OWNER_ONLY_TYPES},
};
use crate::error::DunkError;
use crate::flock::panes::PaneInfoEnv;
use serde_json::{json, Value};
use std::path::PathBuf;

impl Flock {
    fn mutation_context_store(
        &self,
        global: bool,
        project: &Value,
    ) -> Result<(Store, Ledger), DunkError> {
        let state = self.status_state()?;
        let base = state
            .python_home
            .config_dir(&state.env)
            .map_err(DunkError::Refused)?;
        let global = global || !truthy(project);
        let path = if global {
            base.join("context")
        } else {
            PathBuf::from(project.as_str().ok_or_else(|| DunkError::Refused(format!(
                "argument should be a str or an os.PathLike object where __fspath__ returns a str, not '{}'", raw_type(project)
            )))?)
        };
        let path = expand_context_path(path, &state.python_home)?;
        Ok((
            Store {
                root: path,
                scope: if global { "global" } else { "project" },
            },
            Ledger::new(Some(&base.join("context-ledger.db"))),
        ))
    }

    pub fn context_add_raw(
        &self,
        args: &Args,
        global: bool,
        pane: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        let get = |key: &str| args.get(key).unwrap_or(&Value::Null);
        let role = self.caller_role(pane, env)?;
        let cls = get("cls");
        let type_ = get("type");
        let authority = get("authority");
        let wants_rule = cls == "ground-truth"
            || type_
                .as_str()
                .is_some_and(|v| OWNER_ONLY_TYPES.contains(&v))
            || authority == "owner";
        if wants_rule && role != "owner" {
            return Err(DunkError::Refused(format!("only the owner writes rules (class {}, type {}, authority {}); role {role} records a fact with --authority agent or measured and the owner promotes it", python_value_string(cls), python_value_string(type_), python_value_string(authority))));
        }
        if role != "owner" && role != "system" && authority == "measured" && !truthy(get("source"))
        {
            return Err(DunkError::Refused(
                "a measured fact needs --source (the command or file it came from)".into(),
            ));
        }
        let (store, mut ledger) = self.mutation_context_store(global, get("project_path"))?;
        if truthy(get("supersedes")) && role != "owner" {
            if let Some(old) = store
                .resolve_raw(get("supersedes"))
                .map_err(|e| DunkError::Refused(e.0))?
            {
                if old.get("class") == Some("ground-truth") || old.get("authority") == Some("owner")
                {
                    return Err(DunkError::Refused(
                        "only the owner supersedes a rule".into(),
                    ));
                }
            }
        }
        let added = store.add_raw(
            args,
            pane.filter(|p| !p.is_empty()).or(Some(&role)),
            &mut ledger,
        )?;
        Ok(
            json!({"slug":added.slug,"path":crate::context::ledger::path_key(&added.path),"class":added.cls}),
        )
    }

    pub fn context_promote_raw(
        &self,
        args: &Args,
        global: bool,
        pane: Option<&str>,
        env: &dyn PaneInfoEnv,
    ) -> Result<Value, DunkError> {
        self.require_superuser(pane, "context promote", env)?;
        let (store, mut ledger) =
            self.mutation_context_store(global, args.get("project_path").unwrap_or(&Value::Null))?;
        let slug = args.get("slug").unwrap_or(&Value::Null);
        let entry = store
            .find_raw(slug)
            .ok_or_else(|| DunkError::Refused(format!("no entry {}", python_value_repr(slug))))?;
        if entry.error.is_some() {
            return Err(DunkError::Refused("'body'".into()));
        }
        let type_ = entry
            .get("type")
            .filter(|v| OWNER_ONLY_TYPES.contains(v) || *v == "decision")
            .unwrap_or("principle");
        let input = json!({"body":entry.body,"cls":"ground-truth","subject":entry.get("subject"),"type":type_,"authority":"owner","source":entry.get("source").unwrap_or(""),"recheck_after":entry.get("recheck_after").filter(|v| !v.is_empty()).unwrap_or("90d"),"supersedes":entry.slug});
        let added = store.add_raw(input.as_object().expect("object"), pane, &mut ledger)?;
        Ok(
            json!({"slug":added.slug,"path":crate::context::ledger::path_key(&added.path),"class":added.cls}),
        )
    }

    pub fn memory_backfill_raw(&self, action: &Value, limit: &Value) -> Result<Value, DunkError> {
        let service = self
            .memory_service()
            .ok_or_else(|| DunkError::Refused("memory service is not enabled".into()))?;
        let action = if truthy(action) {
            action
                .as_str()
                .ok_or_else(|| {
                    DunkError::Refused(format!(
                        "'{}' object has no attribute 'lower'",
                        raw_type(action)
                    ))
                })?
                .to_lowercase()
        } else {
            "start".into()
        };
        match action.as_str() {
            "pause" => {
                service.pause();
                return Ok(json!({"paused":true,"status":service.status()?}));
            }
            "resume" => {
                service.resume();
                return Ok(json!({"paused":false,"status":service.status()?}));
            }
            "status" => return service.status(),
            _ => {}
        }
        let state = self.status_state()?;
        let home = state
            .python_home
            .expanduser_home()
            .map_err(DunkError::Refused)?
            .unwrap_or(std::path::Path::new("~"));
        let sources = crate::memory::backfill::memory_sources(home)?;
        let mut enqueued = 0usize;
        for path in &sources {
            if truthy(limit) && crate::memory::backfill::limit_reached(limit, enqueued)? {
                break;
            }
            if service.enqueue_discovered(&path.to_string_lossy())? {
                enqueued += 1;
            }
        }
        // Python only changes the message after the loop, before status can fail.
        *self
            .last_message
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) =
            format!("Queued {enqueued} memory source(s)");
        Ok(
            json!({"enqueued":enqueued,"discovered":sources.len(),"limit":limit,"paused":service.paused(),"status":service.status()?}),
        )
    }
}

pub(crate) fn raw_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::String(_) => "str",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        Value::Number(n) if n.to_string().contains(['.', 'e', 'E']) => "float",
        Value::Number(_) => "int",
    }
}

fn expand_context_path(
    path: PathBuf,
    home: &crate::config::CapturedPythonHome,
) -> Result<PathBuf, DunkError> {
    let text = path.to_string_lossy();
    if !text.starts_with('~') {
        return Ok(path);
    }
    let (user, tail) = text.split_once('/').unwrap_or((&text, ""));
    let base = if user == "~" {
        home.expanduser_home()
            .map_err(DunkError::Refused)?
            .map(std::path::Path::to_path_buf)
    } else {
        nix::unistd::User::from_name(&user[1..])
            .map_err(|e| DunkError::Refused(e.to_string()))?
            .map(|u| u.dir)
    }
    .filter(|p| !p.to_string_lossy().starts_with('~'))
    .ok_or_else(|| DunkError::Refused("Could not determine home directory.".into()))?;
    Ok(base.join(tail))
}
