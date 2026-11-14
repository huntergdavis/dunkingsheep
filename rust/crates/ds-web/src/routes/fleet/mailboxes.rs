//! t4684 S-B: counts-only mailbox directory. No read, ack, notification or label resolution.
use super::*;
use axum::extract::Query;
use ds_core::inbox::normalise_mailbox;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MailboxPage {
    limit: Option<usize>,
    after: Option<String>,
}

pub(super) async fn api_mailboxes(
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
    let Ok(Query(page)) = Query::<MailboxPage>::try_from_uri(request.uri()) else {
        return detail(
            StatusCode::BAD_REQUEST,
            "Use limit (1..100) and an optional after mailbox",
        );
    };
    let limit = page.limit.unwrap_or(50);
    if !(1..=100).contains(&limit)
        || page.after.as_ref().is_some_and(|name| {
            name.is_empty() || name.len() > 1024 || name.chars().any(char::is_control)
        })
    {
        return detail(StatusCode::BAD_REQUEST, "Invalid mailbox page");
    }
    let stats = match state.rpc.rpc("inbox_stats", json!({})).await {
        Ok(value) if value["mailboxes"].is_array() => value,
        _ => return detail(StatusCode::BAD_GATEWAY, "Mailbox directory unavailable"),
    };
    let mut mailboxes = stats["mailboxes"]
        .as_array()
        .unwrap()
        .iter()
        .collect::<Vec<_>>();
    if mailboxes
        .iter()
        .any(|row| row["mailbox"].as_str().is_none_or(str::is_empty))
    {
        return detail(StatusCode::BAD_GATEWAY, "Mailbox directory unavailable");
    }
    // Stable lexical cursor: changing unread counts must not reshuffle pages.
    mailboxes.sort_by_key(|row| row["mailbox"].as_str().unwrap());
    let total = mailboxes.len();
    let roster = state.rpc.rpc("roster", json!({})).await.ok();
    let lanes = roster.as_ref().and_then(|value| value["lanes"].as_array());
    let mut page_rows = mailboxes
        .into_iter()
        .filter(|row| {
            page.after
                .as_deref()
                .is_none_or(|after| row["mailbox"].as_str().unwrap() > after)
        })
        .take(limit + 1)
        .collect::<Vec<_>>();
    let has_more = page_rows.len() > limit;
    page_rows.truncate(limit);
    let next_after = has_more.then(|| page_rows.last().unwrap()["mailbox"].clone());
    let rows = page_rows.into_iter().map(|row| {
        let mailbox = row["mailbox"].as_str().unwrap();
        // Join only reported names or pane IDs. Never infer a project from cwd, use labels to
        // resolve a target, or claim an unmatched historical mailbox belongs to a live lane.
        let matches = lanes.into_iter().flatten().filter(|lane| {
            ["name", "pane_id"].iter().any(|key| lane[*key].as_str().is_some_and(|name| {
                normalise_mailbox(name) == normalise_mailbox(mailbox)
            }))
        }).map(|lane| json!({
            "pane_id":lane["pane_id"], "name":lane["name"], "harness":lane["harness"],
            "membership":{"scope":lane["membership"]["scope"].as_str().unwrap_or("unknown"),"project":lane["membership"]["project"]}
        })).collect::<Vec<_>>();
        // Explicit allowlist: the list never forwards bodies/subjects or future RPC fields.
        json!({
            "mailbox":mailbox, "destination":row["destination"], "unread":row["unread"],
            "total":row["total"], "archived":row["archived"], "undeliverable":row["undeliverable"],
            "oldest_unread_s":row["oldest_unread_s"],
            "mapping":if lanes.is_none(){"unavailable"}else if matches.is_empty(){"unmapped"}else{"live"},
            "lanes":matches
        })
    }).collect::<Vec<_>>();
    Json(
        json!({"mailboxes":rows,"total":total,"limit":limit,"next_after":next_after,
        "roster_state":if lanes.is_some(){"ok"}else{"unavailable"}}),
    )
    .into_response()
}
