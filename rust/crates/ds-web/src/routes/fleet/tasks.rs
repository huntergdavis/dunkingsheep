//! Owner-session task summaries and metrics through the existing authenticated daemon client.
use super::*;
use axum::extract::Query;
use serde::{Deserialize, Serialize};

const PAGE_SIZE: usize = 50;
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskQuery {
    view: Option<String>,
    project: Option<String>,
    kind: Option<String>,
    needs_owner: Option<bool>,
    status: Option<String>,
    priority: Option<String>,
    assignee: Option<String>,
    cursor: Option<String>,
}

#[derive(Deserialize, Serialize)]
struct Summary {
    id: String,
    title: String,
    #[serde(default)]
    body: String,
    #[serde(default)]
    kind: String,
    status: String,
    priority: String,
    assignees: Vec<String>,
    created_at: f64,
    block_reason: Option<String>,
    #[serde(skip_serializing)]
    scope: String,
}

fn success(value: Value) -> Response {
    let mut response = Json(value).into_response();
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static("no-store"),
    );
    response
}
fn unavailable() -> Response {
    detail(StatusCode::BAD_GATEWAY, "Tasks unavailable; try refreshing")
}

// Strict older dispatchers reject the newly introduced names rather than ignoring
// them. Only that exact capability refusal permits legacy reads, never auth/I/O errors.
fn unsupported_paging(error: &PanesWebError) -> bool {
    let PanesWebError::Failure(text) = error else {
        return false;
    };
    let Some(names) = text.strip_prefix("task_list: unknown argument(s): ") else {
        return false;
    };
    let fields: Vec<_> = names.split(',').map(str::trim).collect();
    fields.contains(&"paged")
        && fields
            .iter()
            .all(|field| ["paged", "page_size", "cursor", "priority", "page_group"].contains(field))
}

pub(super) async fn task_page(
    Extension(state): Extension<Arc<FleetState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    match authenticate(auth.as_ref(), request.headers(), scheme).await {
        Ok(Identity::Owner) => {}
        Ok(_) => return detail(StatusCode::FORBIDDEN, "Owner session required"),
        Err(response) => return response.into_response(),
    }
    let Ok(Query(query)) = Query::<TaskQuery>::try_from_uri(request.uri()) else {
        return detail(StatusCode::BAD_REQUEST, "Invalid task filters");
    };
    let view = query.view.as_deref().unwrap_or("active");
    let statuses: &[&str] = match view {
        "active" => &["open", "in_progress", "blocked"],
        "closed" => &["done", "cancelled"],
        _ => return detail(StatusCode::BAD_REQUEST, "Invalid task view"),
    };
    if query
        .project
        .as_ref()
        .is_some_and(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
        || query
            .kind
            .as_deref()
            .is_some_and(|v| !["work", "review", "verify", "report"].contains(&v))
        || query
            .status
            .as_deref()
            .is_some_and(|v| !statuses.contains(&v))
        || query
            .priority
            .as_deref()
            .is_some_and(|v| !["urgent", "high", "medium", "low"].contains(&v))
        || query
            .assignee
            .as_deref()
            .is_some_and(|v| v.is_empty() || v.len() > 256 || v.chars().any(char::is_control))
        || query
            .cursor
            .as_deref()
            .is_some_and(|v| v.is_empty() || v.len() > 8192)
    {
        return detail(StatusCode::BAD_REQUEST, "Invalid task filters");
    }
    let mut args = json!({"scope":"global", "paged":true,"page_size":PAGE_SIZE,"limit":PAGE_SIZE,"include_done":view=="closed"});
    if let Some(project) = &query.project {
        args["project"] = json!(project);
    }
    if let Some(kind) = &query.kind {
        args["kind"] = json!(kind);
    }
    if query.needs_owner == Some(true) {
        args["needs_owner"] = json!(true);
    }
    if let Some(status) = &query.status {
        args["status"] = json!(status);
    } else {
        args["page_group"] = json!(view);
    }
    if let Some(priority) = &query.priority {
        args["priority"] = json!(priority);
    }
    if let Some(assignee) = &query.assignee {
        args["assignee"] = json!(assignee);
    }
    if let Some(cursor) = &query.cursor {
        args["cursor"] = json!(cursor);
    }
    let raw = match state.rpc.rpc("task_list", args).await {
        Ok(raw) => Some(raw),
        Err(error) if unsupported_paging(&error) => None,
        Err(_) => return unavailable(),
    };
    if raw
        .as_ref()
        .is_some_and(|raw| raw["scope"] != "global" || !raw["tasks"].is_array())
    {
        return unavailable();
    }
    if query.project.as_ref().is_some_and(|project| {
        raw.as_ref()
            .is_none_or(|value| value["project"].as_str() != Some(project.as_str()))
    }) {
        return unavailable();
    }
    let legacy = raw
        .as_ref()
        .is_none_or(|raw| raw.get("paging_version").is_none());
    if legacy
        && (query.project.is_some() || query.kind.is_some() || query.needs_owner == Some(true))
    {
        return unavailable();
    }
    let (candidates, total, next_cursor, checked_at) = if legacy {
        if query.cursor.is_some() {
            return detail(
                StatusCode::BAD_GATEWAY,
                "Paging is unavailable; refresh to the first page",
            );
        }
        // Closed needs two bounded legacy reads. Active/explicit-status queries reuse
        // an ignored-arguments response or make one clean legacy call after refusal.
        let mut rows = if view == "closed" && query.status.is_none() {
            let mut rows = Vec::new();
            for status in statuses {
                let mut args =
                    json!({"scope":"global","status":status,"limit":PAGE_SIZE,"include_done":true});
                if let Some(assignee) = &query.assignee {
                    args["assignee"] = json!(assignee);
                }
                let old = match state.rpc.rpc("task_list", args).await {
                    Ok(value) => value,
                    Err(_) => return unavailable(),
                };
                let Some(items) = old["tasks"]
                    .as_array()
                    .filter(|items| items.len() <= PAGE_SIZE)
                else {
                    return unavailable();
                };
                if old["scope"] != "global" {
                    return unavailable();
                }
                if items.iter().any(|row| row["status"] != *status) {
                    return unavailable();
                }
                rows.extend(items.iter().cloned());
            }
            rows
        } else {
            let old = match raw {
                Some(raw) => raw,
                None => {
                    let mut args =
                        json!({"scope":"global","include_done":view=="closed","limit":PAGE_SIZE});
                    if let Some(status) = &query.status {
                        args["status"] = json!(status);
                    }
                    if let Some(assignee) = &query.assignee {
                        args["assignee"] = json!(assignee);
                    }
                    match state.rpc.rpc("task_list", args).await {
                        Ok(value) => value,
                        Err(_) => return unavailable(),
                    }
                }
            };
            if old["scope"] != "global" {
                return unavailable();
            }
            let Some(rows) = old["tasks"]
                .as_array()
                .filter(|rows| rows.len() <= PAGE_SIZE)
            else {
                return unavailable();
            };
            rows.clone()
        };
        rows.sort_by(|a, b| a["id"].as_str().cmp(&b["id"].as_str()));
        (
            rows,
            Value::Null,
            Value::Null,
            json!(state.rpc.now_seconds()),
        )
    } else {
        let raw = raw.as_ref().expect("paged response exists");
        if raw["paging_version"] != 1
            || raw["order"] != "id_asc"
            || raw["page_size"] != PAGE_SIZE
            || raw["total"].as_u64().is_none()
            || raw["checked_at"].as_f64().is_none()
            || !raw.get("next_cursor").is_some_and(|v| {
                v.is_null() || v.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 8192)
            })
        {
            return unavailable();
        }
        let rows = raw["tasks"].as_array().unwrap();
        if rows.len() > PAGE_SIZE {
            return unavailable();
        }
        (
            rows.clone(),
            raw["total"].clone(),
            raw["next_cursor"].clone(),
            raw["checked_at"].clone(),
        )
    };
    let mut rows = Vec::new();
    for value in candidates {
        let Ok(row) = serde_json::from_value::<Summary>(value) else {
            return unavailable();
        };
        if query.kind.as_ref().is_some_and(|kind| kind != &row.kind)
            || (query.needs_owner == Some(true)
                && !row
                    .block_reason
                    .as_deref()
                    .is_some_and(|reason| reason.starts_with("needs-owner:")))
            || row.scope != "global"
            || !statuses.contains(&row.status.as_str())
            || query
                .status
                .as_ref()
                .is_some_and(|status| status != &row.status)
            || query
                .assignee
                .as_ref()
                .is_some_and(|a| !row.assignees.contains(a))
        {
            return unavailable();
        }
        if query.priority.as_ref().is_some_and(|p| p != &row.priority) {
            if legacy {
                continue;
            } else {
                return unavailable();
            }
        }
        rows.push(row);
    }
    if rows.windows(2).any(|pair| pair[0].id >= pair[1].id) {
        return unavailable();
    }
    if !legacy
        && (total.as_u64().unwrap_or(0) < rows.len() as u64
            || (!next_cursor.is_null() && rows.is_empty()))
    {
        return unavailable();
    }
    rows.truncate(PAGE_SIZE);
    success(
        json!({"tasks":rows,"total":total,"next_cursor":next_cursor,"checked_at":checked_at,
        "view":view,"legacy_limited":legacy,"scope":"global"}),
    )
}

pub(super) async fn task_metrics(
    Extension(state): Extension<Arc<FleetState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    match authenticate(auth.as_ref(), request.headers(), scheme).await {
        Ok(Identity::Owner) => {}
        Ok(_) => return detail(StatusCode::FORBIDDEN, "Owner session required"),
        Err(response) => return response.into_response(),
    }
    match state
        .rpc
        .rpc(
            "task_metrics",
            json!({"scope":"global","window_s":86400,"bucket":"hour"}),
        )
        .await
    {
        Ok(value)
            if value["scope"] == "global"
                && value["current"].is_object()
                && value["throughput"].is_object()
                && value["window"].is_object() =>
        {
            success(
                json!({"current":value["current"],"throughput":value["throughput"],"window":value["window"],"generated_at":value["generated_at"]}),
            )
        }
        _ => unavailable(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_explicit_old_argument_refusals_allow_fallback() {
        assert!(unsupported_paging(&PanesWebError::Failure(
            "task_list: unknown argument(s): page_group, page_size, paged".into()
        )));
        for text in [
            "task_list is not permitted for role web",
            "daemon unavailable",
            "task_list: unknown argument(s): attacker, paged",
            "task_list: unknown argument(s): cursor",
        ] {
            assert!(!unsupported_paging(&PanesWebError::Failure(text.into())));
        }
    }
}
