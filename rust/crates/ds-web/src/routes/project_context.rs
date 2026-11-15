//! Project information browser (t3250): read-only views of each project's context store and of
//! the global store, for the owner.
//!
//! Where a store is: only the owner-written map `$DUNKINGSHEEP_DIR/project-stores.json`
//! (`{"project name": "/absolute/project/dir"}`). The map must be a regular file owned by the
//! desk's user and not writable by group or others; each path must be absolute and, once
//! canonicalised, contain a `.dunkingsheep` directory. A bad map or a bad entry is shown as
//! such and never guessed; a project that is not mapped shows "no information store mapped".
//! The global store is always listed, apart from every project.
//!
//! Read-only: nothing is created, migrated or indexed. Search is a substring match over the
//! entries read for this request (the store's own search rebuilds an index file, which is a
//! write). A file that is a symbolic link, or that resolves outside the store, is skipped.
//! Bodies are returned as text, truncated past `MAX_BODY_CHARS`, and the desk renders them as
//! text.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::{Extension, Path as UrlPath, Query},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use ds_core::context::store::{Entry, Store, CLASSES};
use serde_json::{json, Value};

use super::auth::{AuthBackend, Identity, RequestScheme};

pub const MAP_FILE: &str = "project-stores.json";
const MAX_BODY_CHARS: usize = 4_000;
const MAX_ENTRIES: usize = 500;

/// Where the browser looks.
#[derive(Clone, Debug)]
pub struct ProjectContextConfig {
    /// `$DUNKINGSHEEP_DIR`, which holds the map.
    pub config_dir: PathBuf,
    /// The global context store.
    pub global_store: PathBuf,
    /// The user the map must belong to.
    pub uid: u32,
}

/// One mapped project: its store, or why it cannot be used.
#[derive(Debug, Clone, PartialEq)]
pub struct Mapped {
    pub name: String,
    pub store: Result<PathBuf, String>,
}

/// Read and check the map. `Err` when the map itself cannot be used; entries that cannot be
/// used are returned with their reason.
pub fn read_map(config: &ProjectContextConfig) -> Result<Option<Vec<Mapped>>, String> {
    let path = config.config_dir.join(MAP_FILE);
    let meta = match std::fs::symlink_metadata(&path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("the map cannot be read: {error}")),
    };
    if !meta.file_type().is_file() {
        return Err("the map is not a regular file".into());
    }
    if meta.uid() != config.uid {
        return Err("the map is not owned by the desk's user".into());
    }
    if meta.permissions().mode() & 0o022 != 0 {
        return Err("the map is writable by group or others".into());
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("the map cannot be read: {e}"))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|e| format!("the map is not JSON: {e}"))?;
    let Some(object) = value.as_object() else {
        return Err("the map must be a JSON object of project name to directory".into());
    };
    Ok(Some(
        object
            .iter()
            .map(|(name, dir)| Mapped {
                name: name.clone(),
                store: dir
                    .as_str()
                    .ok_or_else(|| "the directory is not a string".to_string())
                    .and_then(project_store),
            })
            .collect(),
    ))
}

/// The canonical `.dunkingsheep` store of an absolute project directory.
fn project_store(dir: &str) -> Result<PathBuf, String> {
    let dir = Path::new(dir);
    if !dir.is_absolute() {
        return Err("the directory is not an absolute path".into());
    }
    let dir = std::fs::canonicalize(dir).map_err(|_| "the directory does not exist".to_string())?;
    let store = std::fs::canonicalize(dir.join(".dunkingsheep"))
        .map_err(|_| "the directory has no .dunkingsheep store".to_string())?;
    if !store.starts_with(&dir) || !store.is_dir() {
        return Err("the .dunkingsheep store is not inside the directory".into());
    }
    Ok(store)
}

/// The entries of one store, read-only: files that are links or resolve outside the store are
/// left out and counted.
fn read_store(root: &Path) -> (Vec<Entry>, usize) {
    let Ok(root) = std::fs::canonicalize(root) else {
        return (Vec::new(), 0);
    };
    // Links, FIFOs and devices are never opened (see `Store::entries_no_follow`).
    Store::new(Some(&root), false).entries_no_follow()
}

fn class_rank(class: Option<&str>) -> usize {
    class
        .and_then(|class| CLASSES.iter().position(|known| *known == class))
        .unwrap_or(CLASSES.len())
}

/// An entry's class: its front matter, else the class directory it lives in (an entry whose
/// front matter cannot be read still belongs to its directory).
fn class_of(entry: &Entry) -> Option<&str> {
    entry.get("class").or_else(|| {
        entry
            .path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .filter(|name| CLASSES.contains(name))
    })
}

fn entry_json(entry: &Entry, now: chrono::NaiveDateTime) -> Value {
    let body: String = entry.body.chars().take(MAX_BODY_CHARS).collect();
    let truncated = entry.body.chars().count() > MAX_BODY_CHARS;
    json!({
        "slug": entry.slug,
        "class": class_of(entry),
        "ground_truth": class_of(entry) == Some("ground-truth"),
        "type": entry.get("type"),
        "authority": entry.get("authority"),
        "source": entry.get("source"),
        "subject": entry.get("subject"),
        "stated": entry.get("stated"),
        "recheck_after": entry.get("recheck_after"),
        "stale": !ds_core::context::digest::stale_mark(entry, now).is_empty(),
        "superseded_by": entry.get("superseded_by"),
        "retired": Store::is_retired(entry),
        "error": entry.error,
        "body": body,
        "body_truncated": truncated,
    })
}

/// The view of one store: ground truths first, then facts, provisional and history entries.
pub fn store_view(root: &Path, query: Option<&str>) -> Value {
    let (mut entries, skipped) = read_store(root);
    let needle = query
        .map(str::trim)
        .filter(|q| !q.is_empty())
        .map(str::to_lowercase);
    if let Some(needle) = &needle {
        entries.retain(|entry| {
            [
                entry.slug.as_str(),
                entry.get("subject").unwrap_or(""),
                entry.body.as_str(),
            ]
            .iter()
            .any(|text| text.to_lowercase().contains(needle.as_str()))
        });
    }
    entries.sort_by(|a, b| {
        (
            class_rank(a.get("class")),
            a.get("subject").unwrap_or(""),
            &a.slug,
        )
            .cmp(&(
                class_rank(b.get("class")),
                b.get("subject").unwrap_or(""),
                &b.slug,
            ))
    });
    let total = entries.len();
    let now = chrono::Local::now().naive_local();
    let list: Vec<Value> = entries
        .iter()
        .take(MAX_ENTRIES)
        .map(|e| entry_json(e, now))
        .collect();
    json!({
        "state": if total == 0 { if needle.is_some() { "no_match" } else { "empty" } } else { "ok" },
        "query": needle,
        "total": total,
        "shown": list.len(),
        "truncated": total > MAX_ENTRIES,
        "skipped_links": skipped,
        "ground_truths": entries.iter().filter(|e| class_of(e) == Some("ground-truth")).count(),
        "entries": list,
    })
}

#[derive(serde::Deserialize, Default)]
pub struct SearchQuery {
    q: Option<String>,
}

fn refusal(status: StatusCode, detail: &str) -> Response {
    (status, Json(json!({"detail": detail}))).into_response()
}

/// `None` for the owner; otherwise the response to send.
async fn not_owner(
    auth: &dyn AuthBackend,
    headers: &HeaderMap,
    scheme: RequestScheme,
) -> Option<Response> {
    match super::fleet::authenticate_identity(auth, headers, scheme).await {
        Ok(Identity::Owner) => None,
        Ok(_) => Some(refusal(
            StatusCode::FORBIDDEN,
            "the information browser is for the owner",
        )),
        Err(error) => Some(error.into_response()),
    }
}

async fn api_list(
    Extension(config): Extension<Arc<ProjectContextConfig>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = not_owner(auth.as_ref(), &headers, scheme).await {
        return response;
    }
    let (map_state, projects) = match read_map(&config) {
        Ok(None) => (json!({"state": "absent"}), Vec::new()),
        Ok(Some(mapped)) => (
            json!({"state": "ok"}),
            mapped
                .into_iter()
                .map(|m| match m.store {
                    Ok(_) => json!({"name": m.name, "state": "ok"}),
                    Err(reason) => {
                        json!({"name": m.name, "state": "mapping_invalid", "reason": reason})
                    }
                })
                .collect(),
        ),
        Err(reason) => (json!({"state": "invalid", "reason": reason}), Vec::new()),
    };
    let global = if Store::new(Some(&config.global_store), false).exists() {
        "ok"
    } else {
        "no_store"
    };
    Json(json!({"map": map_state, "projects": projects, "global": {"state": global}}))
        .into_response()
}

async fn api_project(
    Extension(config): Extension<Arc<ProjectContextConfig>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    UrlPath(name): UrlPath<String>,
    Query(query): Query<SearchQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = not_owner(auth.as_ref(), &headers, scheme).await {
        return response;
    }
    let mapped = match read_map(&config) {
        Ok(Some(mapped)) => mapped,
        Ok(None) => return Json(json!({"project": name, "state": "not_mapped"})).into_response(),
        Err(reason) => {
            return Json(json!({"project": name, "state": "map_invalid", "reason": reason}))
                .into_response()
        }
    };
    let Some(entry) = mapped.into_iter().find(|m| m.name == name) else {
        return Json(json!({"project": name, "state": "not_mapped"})).into_response();
    };
    match entry.store {
        Ok(store) => {
            let mut view = store_view(&store, query.q.as_deref());
            view["project"] = json!(name);
            Json(view).into_response()
        }
        Err(reason) => Json(json!({"project": name, "state": "mapping_invalid", "reason": reason}))
            .into_response(),
    }
}

async fn api_global(
    Extension(config): Extension<Arc<ProjectContextConfig>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    Query(query): Query<SearchQuery>,
    headers: HeaderMap,
) -> Response {
    if let Some(response) = not_owner(auth.as_ref(), &headers, scheme).await {
        return response;
    }
    if !Store::new(Some(&config.global_store), false).exists() {
        return Json(json!({"scope": "global", "state": "no_store"})).into_response();
    }
    let mut view = store_view(&config.global_store, query.q.as_deref());
    view["scope"] = json!("global");
    Json(view).into_response()
}

pub fn router(
    config: Arc<ProjectContextConfig>,
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
) -> Router {
    Router::new()
        .route("/api/project-context", get(api_list))
        .route("/api/project-context/global", get(api_global))
        .route("/api/project-context/project/{name}", get(api_project))
        .layer(Extension(config))
        .layer(Extension(auth))
        .layer(Extension(scheme))
}

#[cfg(test)]
#[path = "project_context_tests.rs"]
mod tests;
