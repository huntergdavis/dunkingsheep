//! Persistent tmux ownership, matching dunk_terminal.TmuxBackend's registry.
use super::{Pane, TerminalError};
use crate::config::Env;
use serde_json::{json, Map, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Registry {
    path: Result<PathBuf, TerminalError>,
}
impl Registry {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: Ok(path.into()),
        }
    }
    pub fn configured(explicit: Option<&Path>, env: &Env) -> Self {
        if let Some(path) = explicit.filter(|p| !p.as_os_str().is_empty()) {
            return Self::new(path);
        }
        if let Some(path) = env
            .get("DUNKINGSHEEP_TMUX_REGISTRY")
            .filter(|p| !p.is_empty())
        {
            return Self::new(path);
        }
        let directory = env
            .get("DUNKINGSHEEP_DIR")
            .filter(|p| !p.is_empty())
            .map(|path| Ok(PathBuf::from(path)))
            .unwrap_or_else(|| expand_home("~/.config/dunkingsheep", env));
        Self {
            path: directory.map(|directory| directory.join("tmux-panes.json")),
        }
    }
    pub fn path(&self) -> Result<&Path, TerminalError> {
        self.path.as_deref().map_err(Clone::clone)
    }
    pub fn load(&self) -> Result<Map<String, Value>, TerminalError> {
        // Python swallows file/JSON errors, but not a valid non-object registry.
        let value = std::fs::read(self.path()?)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .unwrap_or_else(|| json!({}));
        let mut data = value
            .as_object()
            .cloned()
            .ok_or_else(|| TerminalError::refused("tmux registry must be an object"))?;
        for key in ["panes", "tabs", "workspaces"] {
            data.entry(key).or_insert_with(|| json!({}));
        }
        Ok(data)
    }
    pub fn save(&self, data: &Map<String, Value>) -> Result<(), TerminalError> {
        let path = self.path()?;
        let directory = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(directory).map_err(io_error)?;
        let mut temp = path.as_os_str().to_os_string();
        temp.push(".tmp");
        let temp = PathBuf::from(temp);
        let text = serde_json::to_string_pretty(&sorted(Value::Object(data.clone())))
            .map_err(|e| TerminalError::refused(e.to_string()))?;
        // json.dump defaults ensure_ascii=True, including surrogate pairs.
        let mut ascii = String::new();
        for c in text.chars() {
            if (c as u32) < 0x7f {
                ascii.push(c);
            } else {
                for unit in c.encode_utf16(&mut [0; 2]) {
                    ascii.push_str(&format!("\\u{unit:04x}"));
                }
            }
        }
        std::fs::write(&temp, ascii).map_err(io_error)?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .map_err(io_error)?;
        std::fs::rename(&temp, path).map_err(io_error)
    }
    pub fn remember(&self, kind: &str, id: &str, extra: Value) -> Result<(), TerminalError> {
        if id.is_empty() {
            return Ok(());
        }
        let mut data = self.load()?;
        let mut record = extra.as_object().cloned().unwrap_or_default();
        record.insert("at".into(), json!(crate::clock::now_s() as i64));
        collection_mut(&mut data, kind)?.insert(id.into(), Value::Object(record));
        self.save(&data)
    }
    pub fn forget(&self, kind: &str, id: &str) -> Result<(), TerminalError> {
        let mut data = self.load()?;
        if collection_mut(&mut data, kind)?
            .remove(id)
            .is_some_and(|v| !v.is_null())
        {
            self.save(&data)?;
        }
        Ok(())
    }
    pub fn managed(&self, kind: &str, id: &str, panes: &[Pane]) -> Result<bool, TerminalError> {
        let data = self.load()?;
        let own = collection(&data, kind)?;
        if !own.contains_key(id) {
            return Ok(false);
        }
        if kind == "pane" {
            return Ok(true);
        }
        let owned_panes = collection(&data, "pane")?;
        let mut children = panes
            .iter()
            .filter(|p| {
                if kind == "tab" {
                    p.window_id == id
                } else {
                    p.session_id == id
                }
            })
            .peekable();
        Ok(children.peek().is_some() && children.all(|p| owned_panes.contains_key(&p.pane_id)))
    }
    pub fn labels(&self) -> Result<std::collections::BTreeMap<String, String>, TerminalError> {
        let data = self.load()?;
        Ok(collection(&data, "pane")?
            .iter()
            .filter_map(|(id, record)| {
                record
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                    .map(|name| (id.clone(), name.into()))
            })
            .collect())
    }
}
fn key(kind: &str) -> Result<&'static str, TerminalError> {
    match kind {
        "pane" => Ok("panes"),
        "tab" => Ok("tabs"),
        "workspace" => Ok("workspaces"),
        _ => Err(TerminalError::refused(format!("unknown kind {kind:?}"))),
    }
}
fn collection<'a>(
    data: &'a Map<String, Value>,
    kind: &str,
) -> Result<&'a Map<String, Value>, TerminalError> {
    data.get(key(kind)?)
        .and_then(Value::as_object)
        .ok_or_else(|| TerminalError::refused("tmux registry collection must be an object"))
}
fn collection_mut<'a>(
    data: &'a mut Map<String, Value>,
    kind: &str,
) -> Result<&'a mut Map<String, Value>, TerminalError> {
    data.get_mut(key(kind)?)
        .and_then(Value::as_object_mut)
        .ok_or_else(|| TerminalError::refused("tmux registry collection must be an object"))
}
fn sorted(value: Value) -> Value {
    match value {
        Value::Object(map) => {
            let ordered: std::collections::BTreeMap<_, _> = map.into_iter().collect();
            Value::Object(ordered.into_iter().map(|(k, v)| (k, sorted(v))).collect())
        }
        Value::Array(values) => Value::Array(values.into_iter().map(sorted).collect()),
        other => other,
    }
}
fn io_error(error: std::io::Error) -> TerminalError {
    TerminalError::refused(error.to_string())
}
/// Python expanduser: HOME (even empty), then real-UID passwd; unresolved stays '~'.
pub fn expand_home(path: &str, env: &Env) -> Result<PathBuf, TerminalError> {
    expand_home_with(path, env, &|name| {
        let user = match name {
            Some(name) => nix::unistd::User::from_name(name),
            None => nix::unistd::User::from_uid(nix::unistd::Uid::current()),
        };
        user.map(|user| user.map(|user| user.dir))
            .map_err(|error| TerminalError::refused(error.to_string()))
    })
}
type AccountLookup<'a> = dyn Fn(Option<&str>) -> Result<Option<PathBuf>, TerminalError> + 'a;
fn expand_home_with(
    path: &str,
    env: &Env,
    lookup: &AccountLookup<'_>,
) -> Result<PathBuf, TerminalError> {
    if !path.starts_with('~') {
        return Ok(PathBuf::from(path));
    }
    let end = path.find('/').unwrap_or(path.len());
    let name = &path[1..end];
    let home = if name.is_empty() {
        match env.get("HOME") {
            Some(home) => Some(PathBuf::from(home)),
            None => lookup(None)?,
        }
    } else {
        lookup(Some(name))?
    };
    Ok(match home {
        Some(home) => {
            let expanded = format!(
                "{}{}",
                home.to_string_lossy().trim_end_matches('/'),
                &path[end..]
            );
            PathBuf::from(if expanded.is_empty() {
                "/".into()
            } else {
                expanded
            })
        }
        None => PathBuf::from(path),
    })
}

#[cfg(test)]
#[path = "tmux_registry_tests.rs"]
mod tests;
