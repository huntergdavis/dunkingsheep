//! Explicit thread-page HTTP adapter; legacy owner-thread shaping is unchanged.
use super::*;

fn invalid() -> OwnerWebError {
    OwnerWebError::Http { status: StatusCode::BAD_GATEWAY, detail: "Threaded inbox unavailable; response scope could not be verified. A service upgrade may be needed.".into() }
}
fn id(value: &Value) -> bool {
    value.as_str().is_some_and(ds_core::inbox::valid_message_id)
}
fn scoped(row: &Value, filter: ConversationFilter, root: Option<&str>, fence: i64) -> bool {
    let Some(message) = row["id"].as_str().filter(|_| id(&row["id"])) else {
        return false;
    };
    if message[1..].parse::<i64>().ok().is_none_or(|n| n > fence)
        || !row["created_at"].as_f64().is_some_and(f64::is_finite)
        || !row
            .get("audience")
            .is_some_and(|a| a.is_null() || a == "agent")
    {
        return false;
    }
    if filter == ConversationFilter::Historical {
        return root.is_none_or(|r| r == message)
            && [
                "thread_version",
                "parent_id",
                "root_id",
                "conversation_kind",
                "sender_kind",
            ]
            .iter()
            .all(|k| row.get(*k).is_some_and(Value::is_null))
            && matches!(
                row["historical_reason"].as_str(),
                Some("prior_version" | "other" | "unknown")
            );
    }
    let kind = filter.thread_view().unwrap_or_default();
    row["thread_version"] == 1
        && id(&row["root_id"])
        && row.get("parent_id").is_some_and(|p| p.is_null() || id(p))
        && matches!(
            row["sender_kind"].as_str(),
            Some("human" | "agent" | "system")
        )
        && (row["conversation_kind"] == kind
            || kind == "human"
                && row["conversation_kind"] == "vp"
                && row["sender_kind"] == "agent"
                && row["recipient_route_kind"] == "human")
        && root.is_none_or(|r| row["root_id"] == r)
}
fn item(row: &Value, filter: ConversationFilter, preview: bool) -> Value {
    let mut result = owner_thread_item(row);
    for key in [
        "mailbox",
        "audience",
        "thread_version",
        "parent_id",
        "root_id",
        "conversation_kind",
        "sender_kind",
        "unresolved_parent",
        "sender_route_kind",
        "sender_route_mailbox",
        "recipient_route_kind",
        "recipient_route_mailbox",
        "subject",
        "state",
        "ambiguous_owner_reply",
    ] {
        result[key] = row[key].clone();
    }
    result["conversation_source"] = json!(filter.thread_view());
    // Preserve sub-second ordering separately from the human-readable timestamp.
    result["sort_at"] = row["created_at"].clone();
    let mut labels = Vec::new();
    if row["audience"] == "agent" {
        labels.push("Agent-only");
    }
    if filter == ConversationFilter::Historical {
        result["historical_reason"] = row["historical_reason"].clone();
        if ds_core::owner::conversations::is_routine_mail(row) {
            labels.push("Routine agent mail");
        }
        if row["digest_id"]
            .as_str()
            .is_some_and(|s| s.starts_with("dunk:"))
        {
            labels.push("Scheduled check-in");
        }
        result["historical_labels"] = json!(labels);
    } else {
        if row["sender_kind"] == "system" {
            labels.push("System");
        }
        if row["sender"] == ds_core::owner::conversations::OWNER_PAGE_SENDER
            && row["subject"] == ds_core::owner::conversations::OWNER_PAGE_SUBJECT
        {
            labels.push("Alert");
        }
        result["kind_labels"] = json!(labels);
        result["role"] = json!(if row["sender_kind"] == "human" {
            "owner"
        } else {
            "agent"
        });
    }
    if preview {
        result.as_object_mut().unwrap().remove("body");
        result["preview"] = row["preview"].clone();
    }
    result
}

pub(super) async fn get(
    state: &OwnerWebState,
    query: &HashMap<String, String>,
) -> Result<Value, OwnerWebError> {
    let bad = |detail: &str| OwnerWebError::Http {
        status: StatusCode::BAD_REQUEST,
        detail: detail.into(),
    };
    let mode = query
        .get("thread_mode")
        .map(String::as_str)
        .unwrap_or_default();
    if !matches!(mode, "heads" | "messages")
        || query.contains_key("before")
        || query.contains_key("before_id")
        || query.contains_key("paged")
        || query.get("include_internal").is_some_and(|v| v != "false")
    {
        return Err(bad("invalid thread-page query"));
    }
    let filter = ConversationFilter::parse(
        query.get("filter").map(String::as_str).unwrap_or_default(),
        false,
    )
    .map_err(|_| bad("invalid thread view"))?;
    if filter.thread_view().is_none() {
        return Err(bad("thread pages require an explicit thread view"));
    }
    let root = query.get("root_id");
    if (mode == "messages") != root.is_some()
        || root.is_some_and(|r| !ds_core::inbox::valid_message_id(r))
    {
        return Err(bad("invalid thread root"));
    }
    let limit = query
        .get("limit")
        .map(|s| s.parse::<i64>())
        .transpose()
        .map_err(|_| bad("invalid thread limit"))?
        .unwrap_or(if mode == "heads" { 25 } else { 50 });
    if !(1..=100).contains(&limit) {
        return Err(bad("thread limit must be 1..100"));
    }
    let cursor = query.get("thread_cursor");
    if cursor.is_some_and(|s| s.is_empty() || s.len() > 16384) {
        return Err(bad("invalid thread cursor"));
    }
    let mut args = json!({"thread_mode":mode,"source_filter":filter.name(),"include_internal":false,"limit":limit});
    if let Some(root) = root {
        args["root_id"] = json!(root);
    }
    if let Some(cursor) = cursor {
        args["thread_cursor"] = json!(cursor);
    }
    let data = state.backend.rpc("owner_conversation", args).await?;
    let key = if mode == "heads" {
        "threads"
    } else {
        "messages"
    };
    let rows = data[key].as_array().ok_or_else(invalid)?;
    let fence = data["fence_id"]
        .as_i64()
        .filter(|v| *v >= 0)
        .ok_or_else(invalid)?;
    if data["thread_mode"] != mode
        || data["source_filter"] != filter.name()
        || data["include_internal"] != false
        || !data["view_revision"].as_i64().is_some_and(|v| v >= 0)
        || !data["reload_from_top"].is_boolean()
        || !data.get("next_cursor").is_some_and(|v| {
            v.is_null()
                || v.as_str()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 16384)
        })
        || rows.len() > limit as usize
        || mode == "messages" && data["root_id"] != json!(root)
    {
        return Err(invalid());
    }
    let mut output = json!({"thread_mode":mode,"source_filter":filter.name(),"include_internal":false,"view_revision":data["view_revision"],"fence_id":fence,"next_cursor":data["next_cursor"],"reload_from_top":data["reload_from_top"]});
    if mode == "messages" {
        output["root_id"] = json!(root);
    }
    if data["reload_from_top"] == true {
        if !rows.is_empty() || !data["next_cursor"].is_null() {
            return Err(invalid());
        }
        output[key] = json!([]);
        output["message"] = json!("List changed, reload from top.");
        return Ok(output);
    }
    let mut seen = std::collections::HashSet::new();
    let mut selected = Vec::new();
    for row in rows {
        if mode == "heads" {
            let latest = &row["latest"];
            let thread = row["thread_id"]
                .as_str()
                .filter(|_| id(&row["thread_id"]))
                .ok_or_else(invalid)?;
            if !seen.insert(thread)
                || !scoped(latest, filter, Some(thread), fence)
                || !row["message_count"].as_i64().is_some_and(|c| c > 0)
                || !row["unread_count"]
                    .as_i64()
                    .is_some_and(|n| n >= 0 && Some(n) <= row["message_count"].as_i64())
                || !latest["preview"]
                    .as_str()
                    .is_some_and(|p| p.chars().count() <= 240)
            {
                return Err(invalid());
            }
            selected.push(json!({"thread_id":thread,"latest":item(latest,filter,true),"message_count":row["message_count"],"unread_count":row["unread_count"]}));
        } else {
            if !scoped(row, filter, root.map(String::as_str), fence)
                || !row["body"].is_string()
                || !seen.insert(row["id"].as_str().unwrap_or_default())
            {
                return Err(invalid());
            }
            selected.push(item(row, filter, false));
        }
    }
    output[key] = json!(selected);
    Ok(output)
}
