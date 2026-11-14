//! Owner-session body view with explicit agent-mail consent. No read/ack operation.
use super::*;
use axum::{extract::Query, http::HeaderValue};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewQuery {
    mailbox: String,
    message_id: String,
    include_agent: Option<bool>,
}

#[derive(Deserialize, Serialize)]
struct Attachment {
    id: String,
    filename: Option<String>,
    media_type: Option<String>,
    size: u64,
}
#[derive(Deserialize, Serialize)]
struct MessageView {
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
    body: String,
    read_at: Option<f64>,
    audience: Value,
    attachments: Vec<Attachment>,
}
#[derive(Deserialize)]
struct ViewResult {
    status: String,
    message: Option<MessageView>,
}

fn private(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

pub(super) async fn api_mailbox_message(
    Extension(state): Extension<Arc<FleetState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    match authenticate(auth.as_ref(), request.headers(), scheme).await {
        Ok(Identity::Owner) => {}
        Ok(_) => return private(detail(StatusCode::FORBIDDEN, "Owner session required")),
        Err(response) => return private(response.into_response()),
    }
    let Ok(Query(query)) = Query::<ViewQuery>::try_from_uri(request.uri()) else {
        return private(detail(
            StatusCode::BAD_REQUEST,
            "Use mailbox, message_id, and optional include_agent",
        ));
    };
    if query.mailbox.trim().is_empty()
        || query.mailbox.len() > 1024
        || query.mailbox.chars().any(char::is_control)
        || !ds_core::inbox::valid_message_id(&query.message_id)
    {
        return private(detail(
            StatusCode::BAD_REQUEST,
            "Invalid mailbox or message id",
        ));
    }
    let include_agent = query.include_agent.unwrap_or(false);
    let result=match state.rpc.rpc("inbox_get",json!({"mailbox":query.mailbox,"message_id":query.message_id,"include_agent":include_agent})).await {
        Ok(value)=>serde_json::from_value::<ViewResult>(value).ok(),Err(_)=>None,
    };
    let response = match result {
        Some(ViewResult { status, message }) if status == "ok" => match message {
            Some(message)
                if message.message_id == query.message_id && message.mailbox == query.mailbox =>
            {
                match &message.audience {
                    Value::String(tag) if tag == "agent" && !include_agent => detail(
                        StatusCode::FORBIDDEN,
                        "Enable agent-only mail to read this message",
                    ),
                    Value::Null => Json(json!({"message":message})).into_response(),
                    Value::String(tag) if tag == "agent" => {
                        Json(json!({"message":message})).into_response()
                    }
                    _ => detail(StatusCode::BAD_GATEWAY, "Message unavailable; please retry"),
                }
            }
            _ => detail(StatusCode::BAD_GATEWAY, "Message unavailable; please retry"),
        },
        Some(result) if result.status == "not_found" => {
            detail(StatusCode::NOT_FOUND, "Message not found in this mailbox")
        }
        Some(result) if result.status == "agent_opt_in_required" => detail(
            StatusCode::FORBIDDEN,
            "Enable agent-only mail to read this message",
        ),
        _ => detail(StatusCode::BAD_GATEWAY, "Message unavailable; please retry"),
    };
    private(response)
}
