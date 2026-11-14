//! Owner-session summary browsing; never read/ack or retrieve message bodies.
use super::*;
use axum::extract::Query;
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SummaryQuery {
    mailbox: String,
    limit: Option<usize>,
    before: Option<String>,
}

// Typed allowlist: future daemon fields, including body, cannot reach the browser.
#[derive(Deserialize, Serialize)]
struct Summary {
    message_id: String,
    mailbox: String,
    destination: String,
    sender: Option<String>,
    subject: Option<String>,
    state: String,
    created_at: f64,
    body_bytes: u64,
    transport: Option<String>,
    reply_to: Option<String>,
    attachment_count: u64,
    audience: Option<String>,
    waiting_s: Option<i64>,
}

#[derive(Deserialize, Serialize)]
struct SummaryPage {
    mailbox: String,
    messages: Vec<Summary>,
    order: String,
    next_before: Option<String>,
}

fn valid_key(key: &str) -> bool {
    !key.trim().is_empty() && key.len() <= 1024 && !key.chars().any(char::is_control)
}

pub(super) async fn api_mailbox_messages(
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
    let Ok(Query(query)) = Query::<SummaryQuery>::try_from_uri(request.uri()) else {
        return detail(
            StatusCode::BAD_REQUEST,
            "Use mailbox, limit (1..100), and optional before",
        );
    };
    let limit = query.limit.unwrap_or(50);
    if !(1..=100).contains(&limit)
        || !valid_key(&query.mailbox)
        || query.before.as_deref().is_some_and(|key| !valid_key(key))
    {
        return detail(StatusCode::BAD_REQUEST, "Invalid mailbox page");
    }
    let args = json!({"mailbox":query.mailbox,"limit":limit,"before":query.before,
        "newest_first":true,"literal_mailbox":true,"unread_only":false});
    let result = match state.rpc.rpc("inbox_list", args).await {
        Ok(value) => serde_json::from_value::<SummaryPage>(value).ok(),
        Err(_) => None,
    };
    match result {
        Some(page) if page.mailbox == query.mailbox && page.order == "newest_first" => {
            Json(page).into_response()
        }
        _ => detail(
            StatusCode::BAD_GATEWAY,
            "Mailbox summaries unavailable; refresh the mailbox",
        ),
    }
}
