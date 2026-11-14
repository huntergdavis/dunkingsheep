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
    include_agent: Option<bool>,
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
    #[serde(deserialize_with = "required_audience")]
    audience: Option<String>,
    waiting_s: Option<i64>,
}

fn required_audience<'de, D: serde::Deserializer<'de>>(
    value: D,
) -> Result<Option<String>, D::Error> {
    Option::<String>::deserialize(value)
}

#[derive(Deserialize, Serialize)]
struct SummaryPage {
    mailbox: String,
    messages: Vec<Summary>,
    order: String,
    next_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    include_agent: Option<bool>,
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
    let mut args = json!({"mailbox":query.mailbox,"limit":limit,"before":query.before,
        "newest_first":true,"literal_mailbox":true,"unread_only":false});
    if let Some(include) = query.include_agent {
        args["include_agent"] = json!(include);
    }
    let result = match state.rpc.rpc("inbox_list", args).await {
        Ok(value) => serde_json::from_value::<SummaryPage>(value).ok(),
        Err(_) => None,
    };
    match result {
        Some(page)
            if page.mailbox == query.mailbox
                && page.order == "newest_first"
                && (query.include_agent.is_none()
                    || (page.include_agent == query.include_agent
                        && page.messages.iter().all(|row| {
                            ds_core::inbox::normalise_mailbox(&row.mailbox)
                                == ds_core::inbox::normalise_mailbox(&query.mailbox)
                                && match row.audience.as_deref() {
                                    None => true,
                                    Some("agent") => query.include_agent == Some(true),
                                    _ => false,
                                }
                        }))) =>
        {
            Json(page).into_response()
        }
        _ => detail(
            StatusCode::BAD_GATEWAY,
            "Mailbox summaries unavailable; refresh the mailbox",
        ),
    }
}
