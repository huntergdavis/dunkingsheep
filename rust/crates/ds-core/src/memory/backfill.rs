//! Python memory_sources discovery; injected captured home, never a live-home fallback.
use crate::error::DunkError;
use serde_json::Value;
use std::path::{Component, Path, PathBuf};

pub fn memory_sources(home: &Path) -> Result<Vec<PathBuf>, DunkError> {
    let mut paths = Vec::new();
    for root in [".codex/sessions", ".claude/projects", ".pi/agent/sessions"] {
        jsonl_tree(&home.join(root), &mut paths);
    }
    for extra in [
        ".codex/thread_history_1.sqlite",
        ".local/share/opencode/opencode.db",
    ] {
        let path = home.join(extra);
        if path.exists() {
            paths.push(path);
        }
    }
    let mut unique = std::collections::HashSet::new();
    let mut sources = Vec::new();
    for path in paths {
        if !path.is_file() {
            continue;
        }
        let path = absolute(&path)?;
        if unique.insert(path.clone()) {
            // Unlike glob's directory scans, a getmtime error is not swallowed.
            use std::os::unix::fs::MetadataExt;
            let metadata = std::fs::metadata(&path)?;
            let mtime = metadata.mtime() as f64 + metadata.mtime_nsec() as f64 / 1e9;
            sources.push((path, mtime));
        }
    }
    sources.sort_by(|a, b| b.1.partial_cmp(&a.1).expect("finite filesystem mtime"));
    Ok(sources.into_iter().map(|(path, _)| path).collect())
}

fn jsonl_tree(root: &Path, paths: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let entries: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .collect();
    for entry in &entries {
        if entry.file_name().to_string_lossy().ends_with(".jsonl") {
            paths.push(entry.path());
        }
    }
    for entry in entries {
        if entry.path().is_dir() {
            jsonl_tree(&entry.path(), paths);
        }
    }
}

fn absolute(path: &Path) -> Result<PathBuf, DunkError> {
    let path = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            _ => out.push(part.as_os_str()),
        }
    }
    Ok(out)
}

pub(crate) fn limit_reached(limit: &Value, count: usize) -> Result<bool, DunkError> {
    use super::service::QueryInput;
    let integer = QueryInput::from(limit.clone())
        .integer()
        .map_err(|error| match limit {
            Value::String(text) if text.chars().count() <= 4300 => DunkError::Refused(format!(
                "invalid literal for int() with base 10: {}",
                crate::context::store::py_repr(text)
            )),
            Value::Array(_) | Value::Object(_) | Value::Null => DunkError::Refused(format!(
                "int() argument must be a string, a bytes-like object or a real number, not '{}'",
                crate::flock::context_mutations::raw_type(limit)
            )),
            _ => error,
        })?;
    if integer.starts_with('-') {
        return Ok(true);
    }
    let count = count.to_string();
    Ok(count.len() > integer.len() || (count.len() == integer.len() && count >= integer))
}
