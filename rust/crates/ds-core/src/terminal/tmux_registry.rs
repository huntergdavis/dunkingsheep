//! Persistent tmux ownership, matching dunk_terminal.TmuxBackend's registry.
use super::{Pane, TerminalError};
use crate::config::Env;
use serde_json::{json, Map, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The filesystem operation that failed, retained without parsing an error message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistryOperation {
    CreateDirectory,
    Write,
    Chmod,
    Rename,
}

/// Exceptions from the raw Python registry boundary. Ordinary terminal refusals are
/// represented separately by the inner result of `RawCreateResult`.
#[derive(Debug)]
pub enum RegistryError {
    Io {
        operation: RegistryOperation,
        path: PathBuf,
        destination: Option<PathBuf>,
        error: std::io::Error,
    },
    Shape {
        category: &'static str,
        message: String,
    },
    Configuration(TerminalError),
}
impl RegistryError {
    pub fn category(&self) -> &'static str {
        match self {
            Self::Io { error, .. } => match error.kind() {
                std::io::ErrorKind::AlreadyExists => "FileExistsError",
                std::io::ErrorKind::NotFound => "FileNotFoundError",
                std::io::ErrorKind::NotADirectory => "NotADirectoryError",
                std::io::ErrorKind::IsADirectory => "IsADirectoryError",
                std::io::ErrorKind::PermissionDenied => "PermissionError",
                std::io::ErrorKind::Interrupted => "InterruptedError",
                _ => "OSError",
            },
            Self::Shape { category, .. } => category,
            Self::Configuration(_) => "DunkError",
        }
    }
    fn io(
        operation: RegistryOperation,
        path: &Path,
        destination: Option<&Path>,
        error: std::io::Error,
    ) -> Self {
        Self::Io {
            operation,
            path: path.to_owned(),
            destination: destination.map(Path::to_owned),
            error,
        }
    }
    fn legacy(self) -> TerminalError {
        match self {
            Self::Io { error, .. } => io_error(error),
            Self::Configuration(error) => error,
            Self::Shape { message, .. } => TerminalError::refused(message),
        }
    }
}
impl std::fmt::Display for RegistryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io {
                path,
                destination,
                error,
                ..
            } => {
                let quoted = crate::api::python_value_repr(&json!(path.to_string_lossy()));
                if let Some(errno) = error.raw_os_error() {
                    write!(
                        formatter,
                        "[Errno {errno}] {}: {quoted}",
                        nix::errno::Errno::from_raw(errno).desc()
                    )?;
                } else {
                    write!(formatter, "{error}: {quoted}")?;
                }
                if let Some(destination) = destination {
                    write!(
                        formatter,
                        " -> {}",
                        crate::api::python_value_repr(&json!(destination.to_string_lossy()))
                    )?;
                }
                Ok(())
            }
            Self::Shape { message, .. } => formatter.write_str(message),
            Self::Configuration(error) => write!(formatter, "{error}"),
        }
    }
}
impl std::error::Error for RegistryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { error, .. } => Some(error),
            _ => None,
        }
    }
}

fn python_type(value: &Value) -> &'static str {
    match value {
        Value::Null => "NoneType",
        Value::Bool(_) => "bool",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        Value::String(_) => "str",
        Value::Number(n) if n.to_string().contains(['.', 'e', 'E']) => "float",
        _ => "int",
    }
}

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
    pub fn load_raw(&self) -> Result<Map<String, Value>, RegistryError> {
        let path = self.path().map_err(RegistryError::Configuration)?;
        // Python catches OSError and JSON/Unicode decoding errors, not a valid wrong type.
        let value = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .unwrap_or_else(|| json!({}));
        let kind = python_type(&value);
        let mut data = value
            .as_object()
            .cloned()
            .ok_or_else(|| RegistryError::Shape {
                category: "AttributeError",
                message: format!("'{kind}' object has no attribute 'setdefault'"),
            })?;
        for key in ["panes", "tabs", "workspaces"] {
            data.entry(key).or_insert_with(|| json!({}));
        }
        Ok(data)
    }
    pub fn remember_raw(&self, kind: &str, id: &str, extra: Value) -> Result<(), RegistryError> {
        if id.is_empty() {
            return Ok(());
        }
        let mut data = self.load_raw()?;
        let key = key(kind).map_err(RegistryError::Configuration)?;
        let value = data
            .get_mut(key)
            .expect("load supplies all three collections");
        let kind = python_type(value);
        let collection = value.as_object_mut().ok_or_else(|| RegistryError::Shape {
            category: "TypeError",
            message: if kind == "list" {
                "list indices must be integers or slices, not str".into()
            } else {
                format!("'{kind}' object does not support item assignment")
            },
        })?;
        let mut record = extra.as_object().cloned().unwrap_or_default();
        record.insert("at".into(), json!(crate::clock::now_s() as i64));
        collection.insert(id.into(), Value::Object(record));
        self.save_raw(&data)
    }
    pub fn save(&self, data: &Map<String, Value>) -> Result<(), TerminalError> {
        self.save_raw(data).map_err(RegistryError::legacy)
    }
    pub fn save_raw(&self, data: &Map<String, Value>) -> Result<(), RegistryError> {
        let path = self.path().map_err(RegistryError::Configuration)?;
        let directory = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(directory).map_err(|error| {
            RegistryError::io(RegistryOperation::CreateDirectory, directory, None, error)
        })?;
        let mut temp = path.as_os_str().to_os_string();
        temp.push(".tmp");
        let temp = PathBuf::from(temp);
        let text = serde_json::to_string_pretty(&sorted(Value::Object(data.clone()))).map_err(
            |error| RegistryError::Shape {
                category: "ValueError",
                message: error.to_string(),
            },
        )?;
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
        std::fs::write(&temp, ascii)
            .map_err(|error| RegistryError::io(RegistryOperation::Write, &temp, None, error))?;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| RegistryError::io(RegistryOperation::Chmod, &temp, None, error))?;
        std::fs::rename(&temp, path)
            .map_err(|error| RegistryError::io(RegistryOperation::Rename, &temp, Some(path), error))
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
    pub fn pane_labels_raw(
        &self,
        panes: &[Pane],
    ) -> Result<std::collections::BTreeMap<String, String>, RegistryError> {
        let data = self.load_raw()?;
        Self::pane_labels_from_raw(&data, panes)
    }
    /// Interpret labels from one already captured registry snapshot.
    pub(crate) fn pane_labels_from_raw(
        data: &Map<String, Value>,
        panes: &[Pane],
    ) -> Result<std::collections::BTreeMap<String, String>, RegistryError> {
        let mut labels = std::collections::BTreeMap::new();
        for pane in panes {
            let collection = &data["panes"];
            let collection = collection.as_object().ok_or_else(|| RegistryError::Shape {
                category: "AttributeError",
                message: format!(
                    "'{}' object has no attribute 'get'",
                    python_type(collection)
                ),
            })?;
            if let Some(record) = collection
                .get(&pane.pane_id)
                .filter(|v| super::raw::truthy(v))
            {
                let record = record.as_object().ok_or_else(|| RegistryError::Shape {
                    category: "AttributeError",
                    message: format!("'{}' object has no attribute 'get'", python_type(record)),
                })?;
                if let Some(name) = record
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|v| !v.is_empty())
                {
                    labels.insert(pane.pane_id.clone(), name.to_owned());
                }
            }
        }
        Ok(labels)
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
