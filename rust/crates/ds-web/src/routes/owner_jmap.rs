//! Internal-only JMAP projection. Presentation flags never mutate daemon lifecycle state.
use super::*;
use rusqlite::{params, Connection};
use std::path::PathBuf;

const CORE: &str = "urn:ietf:params:jmap:core";
const SUBMISSION: &str = "urn:ietf:params:jmap:submission";
const MAIL: &str = "urn:ietf:params:jmap:mail";
#[derive(Clone)]
pub struct MailStore(pub PathBuf);

fn connection(store: &MailStore) -> Result<Connection, String> {
    let db = Connection::open(&store.0).map_err(|e| e.to_string())?;
    db.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    db.execute_batch("CREATE TABLE IF NOT EXISTS jmap_flags (id TEXT PRIMARY KEY, mailboxes TEXT NOT NULL, keywords TEXT NOT NULL); CREATE TABLE IF NOT EXISTS jmap_drafts (id INTEGER PRIMARY KEY AUTOINCREMENT, email TEXT NOT NULL, submitted TEXT);").map_err(|e| e.to_string())?;
    Ok(db)
}

pub async fn session(
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    request: Request,
) -> Response {
    match authenticate(auth.as_ref(), request.headers(), scheme).await {
        Ok(Identity::Owner) => (),
        Ok(_) => return detail_response(StatusCode::FORBIDDEN, "Owner session required".into()),
        Err(error) => return error.into_response(),
    }
    Json(json!({"capabilities": {CORE: {"maxSizeUpload":0,"maxConcurrentUpload":1,"maxSizeRequest":1048576,"maxConcurrentRequests":4,"maxCallsInRequest":32,"maxObjectsInGet":500,"maxObjectsInSet":500,"collationAlgorithms":["i;unicode-casemap"]}, MAIL:{}, SUBMISSION:{}},
        "accounts":{"owner":{"name":"Dunking Sheep","isPersonal":true,"isReadOnly":false,"accountCapabilities":{SUBMISSION:{"maxDelayedSend":0,"submissionExtensions":{}},MAIL:{"maxMailboxesPerEmail":2,"maxMailboxDepth":1,"maxSizeMailboxName":100,"maxSizeAttachmentsPerEmail":0,"emailQuerySortOptions":["receivedAt"],"mayCreateTopLevelMailbox":false}}}},
        "primaryAccounts":{MAIL:"owner",SUBMISSION:"owner"},"username":"owner@dunkingsheep.invalid","apiUrl":"/api/jmap","downloadUrl":"/api/jmap/download/{accountId}/{blobId}/{name}?type={type}","uploadUrl":"/api/jmap/upload/{accountId}","eventSourceUrl":"/api/jmap/events","state":"1"})).into_response()
}

async fn emails(state: &OwnerWebState, store: &MailStore) -> Result<Vec<Value>, String> {
    let mut rows = Vec::new();
    let mut cursor = None;
    let mut view_revision = None;
    loop {
        let page = owner_thread(
            state,
            100,
            Some(cursor.as_deref()),
            ConversationFilter::HumanThreads,
        )
        .await
        .map_err(|e| e.detail().to_owned())?;
        if let Some(previous) = &view_revision {
            if *previous != page["view_revision"] {
                return Err("Conversation changed while loading; retry".into());
            }
        } else {
            view_revision = Some(page["view_revision"].clone());
        }
        rows.extend(page["items"].as_array().ok_or("Invalid history")?.clone());
        let next = page["next_before"].as_str().map(str::to_owned);
        if next.is_none() {
            break;
        }
        if next == cursor || rows.len() > 100_000 {
            return Err("History exceeds projection limit".into());
        }
        cursor = next;
    }
    // Open SQLite only after awaits: no connection is held across daemon I/O.
    let db = connection(store)?;
    rows.into_iter().map(|row| {
        let id = row["message_id"].as_str().ok_or("Invalid message id")?;
        let flags = db.query_row("SELECT mailboxes, keywords FROM jmap_flags WHERE id=?", [id], |r| Ok((r.get::<_, String>(0)?,r.get::<_, String>(1)?))).optional().map_err(|e| e.to_string())?;
        let (boxes, keywords) = match flags {
            Some((b,k)) => (serde_json::from_str::<Value>(&b).map_err(|e| e.to_string())?, serde_json::from_str::<Value>(&k).map_err(|e| e.to_string())?),
            None => (json!({"inbox":true}),json!({})),
        };
        let body = row["body"].as_str().unwrap_or_default();
        Ok(json!({"id":id,"blobId":id,"threadId":row["root_id"],"mailboxIds":boxes,"keywords":keywords,"size":body.len(),"receivedAt":row["created_at"],"sentAt":row["created_at"],"messageId":[format!("{id}@dunkingsheep.invalid")],"from":[{"name":if row["sender_kind"]=="human" {json!("You")}else{row["sender"].clone()},"email":if row["sender_kind"]=="human" {"owner@dunkingsheep.invalid"}else{"desk@dunkingsheep.invalid"}}],"to":[{"name":if row["sender_kind"]=="human" {"Front Desk"}else{"You"},"email":if row["sender_kind"]=="human" {"desk@dunkingsheep.invalid"}else{"owner@dunkingsheep.invalid"}}],"subject":body.lines().next().unwrap_or("Message").chars().take(100).collect::<String>(),"preview":body.chars().take(240).collect::<String>(),"hasAttachment":false,"attachments":[],"textBody":[{"partId":"text","type":"text/plain","size":body.len()}],"htmlBody":[],"bodyValues":{"text":{"value":body,"isEncodingProblem":false,"isTruncated":false}}}))
    }).collect()
}
use rusqlite::OptionalExtension;

pub async fn api(
    Extension(state): Extension<Arc<OwnerWebState>>,
    Extension(auth): Extension<Arc<dyn AuthBackend>>,
    Extension(scheme): Extension<RequestScheme>,
    Extension(store): Extension<MailStore>,
    request: Request,
) -> Response {
    match authenticate(auth.as_ref(), request.headers(), scheme).await {
        Ok(Identity::Owner) => (),
        Ok(_) => return detail_response(StatusCode::FORBIDDEN, "Owner session required".into()),
        Err(error) => return error.into_response(),
    }
    let headers = request.headers().clone();
    let bytes = match to_bytes(request.into_body(), 1_048_576).await {
        Ok(b) => b,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let body: Value = match serde_json::from_slice(&bytes) {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let Some(calls) = body["methodCalls"].as_array().filter(|c| c.len() <= 32) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    let needs_history = calls.iter().any(|c| {
        matches!(
            c[0].as_str(),
            Some("Mailbox/get" | "Email/get" | "Email/query" | "Thread/get")
        ) || c[0] == "Email/set" && c[1]["update"].is_object()
    });
    let mut rows = if needs_history {
        match emails(&state, &store).await {
            Ok(r) => r,
            Err(e) => return detail_response(StatusCode::BAD_GATEWAY, e),
        }
    } else {
        Vec::new()
    };
    if needs_history {
        if let Err(error) = append_drafts(&store, &mut rows) {
            return detail_response(StatusCode::INTERNAL_SERVER_ERROR, error);
        }
    }
    let mut results = Vec::<Value>::new();
    let mut created_ids = HashMap::<String, String>::new();
    for call in calls {
        let Some(parts) = call
            .as_array()
            .filter(|p| p.len() == 3 && p[0].is_string() && p[1].is_object() && p[2].is_string())
        else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        let name = parts[0].as_str().unwrap();
        let mut args = parts[1].clone();
        if let Some(reference) = args.get("#ids").cloned() {
            let result = results
                .iter()
                .find(|r| r[2] == reference["resultOf"] && r[0] == reference["name"]);
            args["ids"] = match (result, reference["path"].as_str()) {
                (Some(r), Some("/ids")) => r[1]["ids"].clone(),
                (Some(r), Some("/list/*/threadId")) => json!(r[1]["list"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .map(|v| v["threadId"].clone())
                    .collect::<Vec<_>>()),
                _ => {
                    results.push(json!(["error",{"type":"invalidResultReference"},parts[2]]));
                    continue;
                }
            };
        }
        let result = if name == "Core/echo" {
            Ok(args.clone())
        } else if args["accountId"] != "owner" {
            Err(json!({"type":"accountNotFound"}))
        } else if name == "EmailSubmission/set" {
            submit(
                &args,
                &created_ids,
                &store,
                state.clone(),
                auth.clone(),
                scheme,
                &headers,
            )
            .await
        } else {
            method(name, &args, &mut rows, &store)
        };
        if name == "Email/set" {
            if let Ok(value) = &result {
                if let Some(created) = value["created"].as_object() {
                    for (key, item) in created {
                        if let Some(id) = item["id"].as_str() {
                            created_ids.insert(key.clone(), id.into());
                        }
                    }
                }
            }
        }
        if matches!(name, "Email/set" | "EmailSubmission/set") && result.is_ok() {
            rows.retain(|r| !r["id"].as_str().is_some_and(|id| id.starts_with('d')));
            if let Err(error) = append_drafts(&store, &mut rows) {
                return detail_response(StatusCode::INTERNAL_SERVER_ERROR, error);
            }
        }
        match result {
            Ok(value) => results.push(json!([name, value, parts[2]])),
            Err(error) => results.push(json!(["error", error, parts[2]])),
        };
    }
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"methodResponses":results,"sessionState":"1"})),
    )
        .into_response()
}
fn revision(rows: &[Value]) -> String {
    let digest = ring::digest::digest(
        &ring::digest::SHA256,
        serde_json::to_string(rows).unwrap().as_bytes(),
    );
    digest.as_ref().iter().map(|b| format!("{b:02x}")).collect()
}
fn method(name: &str, args: &Value, rows: &mut [Value], store: &MailStore) -> Result<Value, Value> {
    let state = revision(rows);
    match name {
        "Identity/get" => Ok(
            json!({"accountId":"owner","state":"1","list":[{"id":"owner","name":"You","email":"owner@dunkingsheep.invalid","mayDelete":false,"textSignature":"","htmlSignature":""}],"notFound":[]}),
        ),
        "Mailbox/get" => {
            let list = ["inbox","archive","drafts","sent"].iter().map(|id| {
                let members: Vec<_> = rows.iter().filter(|r|r["mailboxIds"][*id]==true).collect();
                json!({"id":id,"name":id,"parentId":null,"role":id,"sortOrder":0,"totalEmails":members.len(),"unreadEmails":members.iter().filter(|r|r["keywords"]["$seen"]!=true).count(),"totalThreads":members.iter().filter_map(|r|r["threadId"].as_str()).collect::<std::collections::HashSet<_>>().len(),"unreadThreads":members.iter().filter(|r|r["keywords"]["$seen"]!=true).filter_map(|r|r["threadId"].as_str()).collect::<std::collections::HashSet<_>>().len(),"isSubscribed":true,"myRights":{"mayReadItems":true,"mayAddItems":true,"mayRemoveItems":true,"maySetSeen":true,"maySetKeywords":true,"mayCreateChild":false,"mayRename":false,"mayDelete":false,"maySubmit":*id=="drafts"}})
            }).collect::<Vec<_>>();
            Ok(json!({"accountId":"owner","state":state,"list":list,"notFound":[]}))
        }
        "Email/query" => {
            if args
                .get("sort")
                .and_then(Value::as_array)
                .is_some_and(|s| s.iter().any(|v| v["property"] != "receivedAt"))
            {
                return Err(json!({"type":"unsupportedSort"}));
            }
            if !valid_filter(&args["filter"], 0) {
                return Err(json!({"type":"unsupportedFilter"}));
            }
            let mut selected = rows
                .iter()
                .filter(|r| matches_filter(r, &args["filter"]))
                .collect::<Vec<_>>();
            selected.sort_by(|a, b| {
                b["receivedAt"]
                    .as_str()
                    .cmp(&a["receivedAt"].as_str())
                    .then_with(|| b["id"].as_str().cmp(&a["id"].as_str()))
            });
            if args["sort"][0]["isAscending"] == true {
                selected.reverse();
            }
            let position = args["position"].as_u64().unwrap_or(0) as usize;
            Ok(
                json!({"accountId":"owner","queryState":state,"canCalculateChanges":false,"position":position,"ids":selected.iter().skip(position).take(args["limit"].as_u64().unwrap_or(50).min(500) as usize).map(|r|r["id"].clone()).collect::<Vec<_>>(),"total":selected.len()}),
            )
        }
        "Email/get" | "Thread/get" => {
            let ids = args["ids"]
                .as_array()
                .ok_or(json!({"type":"invalidArguments"}))?;
            if ids.len() > 500 {
                return Err(json!({"type":"tooManyObjects"}));
            }
            let mut list = Vec::new();
            let mut missing = Vec::new();
            let mut seen = std::collections::HashSet::new();
            for id in ids {
                if !id.is_string() {
                    return Err(json!({"type":"invalidArguments"}));
                }
                if !seen.insert(id.as_str().unwrap()) {
                    continue;
                }
                if name == "Thread/get" {
                    let emails = rows
                        .iter()
                        .filter(|r| r["threadId"] == *id)
                        .map(|r| r["id"].clone())
                        .collect::<Vec<_>>();
                    if emails.is_empty() {
                        missing.push(id.clone())
                    } else {
                        list.push(json!({"id":id,"emailIds":emails}));
                    }
                } else if let Some(row) = rows.iter().find(|r| r["id"] == *id) {
                    list.push(row.clone())
                } else {
                    missing.push(id.clone());
                }
            }
            Ok(json!({"accountId":"owner","state":state,"list":list,"notFound":missing}))
        }
        "Email/set" => {
            let operations = ["create", "update", "destroy"]
                .iter()
                .filter(|k| args.get(**k).is_some_and(|v| !v.is_null()))
                .count();
            if operations != 1 {
                return Err(
                    json!({"type":"invalidArguments","description":"Use one create, update or destroy operation per call"}),
                );
            }
            if args["create"].as_object().is_some_and(|v| !v.is_empty()) {
                return create_drafts(args, store);
            }
            if let Some(ids) = args["destroy"].as_array() {
                let db = connection(store).map_err(|_| json!({"type":"serverFail"}))?;
                let mut destroyed = Vec::new();
                let mut refused = Map::new();
                for id in ids {
                    let key = id.as_str().unwrap_or_default();
                    if let Some(n) = key.strip_prefix('d').and_then(|v| v.parse::<i64>().ok()) {
                        if db
                            .execute(
                                "DELETE FROM jmap_drafts WHERE id=? AND submitted IS NULL",
                                [n],
                            )
                            .map_err(|_| json!({"type":"serverFail"}))?
                            > 0
                        {
                            destroyed.push(id.clone());
                            continue;
                        }
                    }
                    refused.insert(key.into(),json!({"type":"forbidden","description":"Only unsent drafts can be deleted"}));
                }
                return Ok(
                    json!({"accountId":"owner","oldState":state,"newState":state,"destroyed":destroyed,"notDestroyed":refused}),
                );
            }
            if args["ifInState"].is_string() && args["ifInState"] != state {
                return Err(json!({"type":"stateMismatch"}));
            }
            let updates = args["update"]
                .as_object()
                .ok_or(json!({"type":"invalidArguments"}))?;
            let mut db = connection(store).map_err(|_| json!({"type":"serverFail"}))?;
            let tx = db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|_| json!({"type":"serverFail"}))?;
            for row in rows.iter_mut() {
                let flags = tx
                    .query_row(
                        "SELECT mailboxes,keywords FROM jmap_flags WHERE id=?",
                        [row["id"].as_str().unwrap_or_default()],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
                    )
                    .optional()
                    .map_err(|_| json!({"type":"serverFail"}))?;
                if let Some((b, k)) = flags {
                    row["mailboxIds"] =
                        serde_json::from_str(&b).map_err(|_| json!({"type":"serverFail"}))?;
                    row["keywords"] =
                        serde_json::from_str(&k).map_err(|_| json!({"type":"serverFail"}))?;
                }
            }
            let state = revision(rows);
            if args["ifInState"].is_string() && args["ifInState"] != state {
                return Err(json!({"type":"stateMismatch"}));
            }
            let mut updated = Map::new();
            let mut failed = Map::new();
            for (id, patch) in updates {
                let Some(row) = rows.iter_mut().find(|r| r["id"] == *id) else {
                    failed.insert(id.clone(), json!({"type":"notFound"}));
                    continue;
                };
                let draft_editable =
                    if let Some(n) = id.strip_prefix('d').and_then(|v| v.parse::<i64>().ok()) {
                        tx.query_row(
                            "SELECT submitted IS NULL FROM jmap_drafts WHERE id=?",
                            [n],
                            |r| r.get::<_, bool>(0),
                        )
                        .map_err(|_| json!({"type":"serverFail"}))?
                    } else {
                        false
                    };
                if let Some(n) = id
                    .strip_prefix('d')
                    .and_then(|v| v.parse::<i64>().ok())
                    .filter(|_| draft_editable)
                {
                    let Some(fields) = patch.as_object() else {
                        failed.insert(id.clone(), json!({"type":"invalidProperties"}));
                        continue;
                    };
                    if fields.keys().any(|k| {
                        !matches!(
                            k.as_str(),
                            "to" | "cc"
                                | "bcc"
                                | "from"
                                | "subject"
                                | "bodyValues"
                                | "textBody"
                                | "htmlBody"
                                | "inReplyTo"
                                | "references"
                                | "keywords"
                        )
                    }) {
                        failed.insert(id.clone(), json!({"type":"invalidProperties"}));
                        continue;
                    }
                    let stored: String = tx
                        .query_row("SELECT email FROM jmap_drafts WHERE id=?", [n], |r| {
                            r.get(0)
                        })
                        .map_err(|_| json!({"type":"serverFail"}))?;
                    let stored: Value =
                        serde_json::from_str(&stored).map_err(|_| json!({"type":"serverFail"}))?;
                    let mut candidate = row.clone();
                    candidate["_dsExternalId"] = stored["_dsExternalId"].clone();
                    for (k, v) in fields {
                        candidate[k] = v.clone();
                    }
                    let count = tx
                        .execute(
                            "UPDATE jmap_drafts SET email=? WHERE id=? AND submitted IS NULL",
                            params![candidate.to_string(), n],
                        )
                        .map_err(|_| json!({"type":"serverFail"}))?;
                    if count == 0 {
                        failed.insert(id.clone(), json!({"type":"forbidden"}));
                    } else {
                        candidate.as_object_mut().unwrap().remove("_dsExternalId");
                        *row = candidate;
                        updated.insert(id.clone(), Value::Null);
                    }
                    continue;
                }
                let mut candidate = row.clone();
                if !patch_flags(&mut candidate, patch) {
                    failed.insert(
                        id.clone(),
                        json!({"type":"invalidProperties","properties":["mailboxIds","keywords"]}),
                    );
                    continue;
                }
                tx.execute("INSERT INTO jmap_flags VALUES (?,?,?) ON CONFLICT(id) DO UPDATE SET mailboxes=excluded.mailboxes,keywords=excluded.keywords",params![id,candidate["mailboxIds"].to_string(),candidate["keywords"].to_string()]).map_err(|_|json!({"type":"serverFail"}))?;
                *row = candidate;
                updated.insert(id.clone(), Value::Null);
            }
            tx.commit().map_err(|_| json!({"type":"serverFail"}))?;
            Ok(
                json!({"accountId":"owner","oldState":state,"newState":revision(rows),"updated":updated,"notUpdated":failed}),
            )
        }
        "Email/changes" | "Mailbox/changes" | "Thread/changes" => {
            Err(json!({"type":"cannotCalculateChanges"}))
        }
        _ => Err(json!({"type":"unknownMethod"})),
    }
}
fn valid_filter(filter: &Value, depth: usize) -> bool {
    if depth > 16 {
        return false;
    }
    if filter.is_null() {
        return true;
    }
    let Some(fields) = filter.as_object() else {
        return false;
    };
    if let Some(op) = fields.get("operator") {
        return matches!(op.as_str(), Some("AND" | "OR" | "NOT"))
            && fields.len() == 2
            && fields
                .get("conditions")
                .and_then(Value::as_array)
                .is_some_and(|a| a.iter().all(|f| valid_filter(f, depth + 1)));
    }
    fields.iter().all(|(key, value)| {
        matches!(
            key.as_str(),
            "inMailbox" | "hasKeyword" | "notKeyword" | "text" | "subject" | "from" | "to"
        ) && value.is_string()
    })
}
fn matches_filter(row: &Value, filter: &Value) -> bool {
    if filter.is_null() {
        return true;
    }
    if let Some(op) = filter["operator"].as_str() {
        let conditions = filter["conditions"].as_array().unwrap();
        return match op {
            "AND" => conditions.iter().all(|f| matches_filter(row, f)),
            "OR" => conditions.iter().any(|f| matches_filter(row, f)),
            "NOT" => !conditions.iter().any(|f| matches_filter(row, f)),
            _ => false,
        };
    }
    filter.as_object().is_some_and(|fields| {
        fields.iter().all(|(key, value)| {
            let needle = value.as_str().unwrap_or_default();
            match key.as_str() {
                "inMailbox" => row["mailboxIds"][needle] == true,
                "hasKeyword" => row["keywords"][needle] == true,
                "notKeyword" => row["keywords"][needle] != true,
                "text" => format!("{} {}", row["subject"], row["bodyValues"])
                    .to_lowercase()
                    .contains(&needle.to_lowercase()),
                "subject" | "from" | "to" => row[key]
                    .to_string()
                    .to_lowercase()
                    .contains(&needle.to_lowercase()),
                _ => false,
            }
        })
    })
}
fn patch_flags(row: &mut Value, patch: &Value) -> bool {
    let Some(fields) = patch.as_object() else {
        return false;
    };
    for (key, value) in fields {
        if matches!(key.as_str(), "mailboxIds" | "keywords") {
            row[key] = value.clone();
        } else if let Some((field, item)) = key.split_once('/') {
            if !matches!(field, "mailboxIds" | "keywords") || item.contains('/') {
                return false;
            }
            if !row[field].is_object() {
                return false;
            }
            if value.is_null() {
                row[field].as_object_mut().unwrap().remove(item);
            } else {
                row[field][item] = value.clone();
            }
        } else {
            return false;
        }
    }
    row["mailboxIds"].as_object().is_some_and(|m| {
        !m.is_empty()
            && m.iter()
                .all(|(k, v)| matches!(k.as_str(), "inbox" | "archive" | "sent") && *v == true)
    }) && row["keywords"].as_object().is_some_and(|m| {
        m.iter()
            .all(|(k, v)| matches!(k.as_str(), "$seen" | "$flagged" | "$answered") && *v == true)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn archive_restore_and_seen_preserve_lifecycle() {
        let dir = tempfile::tempdir().unwrap();
        let store = MailStore(dir.path().join("mail.sqlite3"));
        let mut rows = vec![
            json!({"id":"i1","threadId":"i1","mailboxIds":{"inbox":true},"keywords":{},"delivery_state":"queued","acked_at":null}),
        ];
        let result=method("Email/set",&json!({"accountId":"owner","update":{"i1":{"mailboxIds":{"archive":true},"keywords/$seen":true}}}),&mut rows,&store).unwrap();
        assert!(result["updated"].get("i1").is_some());
        assert_eq!(rows[0]["delivery_state"], "queued");
        assert!(rows[0]["acked_at"].is_null());
        method(
            "Email/set",
            &json!({"update":{"i1":{"mailboxIds":{"inbox":true},"keywords/$seen":null}}}),
            &mut rows,
            &store,
        )
        .unwrap();
        assert_eq!(rows[0]["mailboxIds"], json!({"inbox":true}));
        let db = connection(&store).unwrap();
        assert_eq!(
            db.query_row("SELECT keywords FROM jmap_flags WHERE id='i1'", [], |r| {
                r.get::<_, String>(0)
            })
            .unwrap(),
            "{}"
        );
    }
    #[test]
    fn rejects_unknown_message_and_lifecycle_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let store = MailStore(dir.path().join("mail.sqlite3"));
        let mut rows = vec![json!({"id":"i1","mailboxIds":{"inbox":true},"keywords":{}})];
        let result = method(
            "Email/set",
            &json!({"update":{"i2":{"keywords/$seen":true},"i1":{"state":"acked"}}}),
            &mut rows,
            &store,
        )
        .unwrap();
        assert_eq!(result["notUpdated"]["i2"]["type"], "notFound");
        assert_eq!(result["notUpdated"]["i1"]["type"], "invalidProperties");
        assert_eq!(rows[0]["mailboxIds"], json!({"inbox":true}));
    }
    #[test]
    fn rejects_stale_mutation() {
        let dir = tempfile::tempdir().unwrap();
        let store = MailStore(dir.path().join("mail.sqlite3"));
        assert_eq!(
            method(
                "Email/set",
                &json!({"ifInState":"old","update":{}}),
                &mut [],
                &store
            )
            .unwrap_err()["type"],
            "stateMismatch"
        );
    }
}

fn create_drafts(args: &Value, store: &MailStore) -> Result<Value, Value> {
    let creates = args["create"]
        .as_object()
        .ok_or(json!({"type":"invalidArguments"}))?;
    let db = connection(store).map_err(|_| json!({"type":"serverFail"}))?;
    let mut created = Map::new();
    let mut refused = Map::new();
    for (key, email) in creates {
        if !email.is_object()
            || email.get("bodyStructure").is_some()
            || email["attachments"]
                .as_array()
                .is_some_and(|a| !a.is_empty())
        {
            refused.insert(key.clone(),json!({"type":"invalidEmail","description":"Attachments are not supported by internal owner delivery"}));
            continue;
        }
        let mut email = email.clone();
        let mut random = [0u8; 16];
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut random)
            .map_err(|_| json!({"type":"serverFail"}))?;
        email["_dsExternalId"] = json!(format!(
            "jmap-{}",
            random
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        email["receivedAt"] = json!(chrono::Utc::now().to_rfc3339());
        db.execute(
            "INSERT INTO jmap_drafts(email) VALUES (?)",
            [email.to_string()],
        )
        .map_err(|_| json!({"type":"serverFail"}))?;
        created.insert(
            key.clone(),
            json!({"id":format!("d{}",db.last_insert_rowid())}),
        );
    }
    Ok(
        json!({"accountId":"owner","oldState":"0","newState":"1","created":created,"notCreated":refused}),
    )
}
async fn submit(
    args: &Value,
    ids: &HashMap<String, String>,
    store: &MailStore,
    state: Arc<OwnerWebState>,
    auth: Arc<dyn AuthBackend>,
    scheme: RequestScheme,
    headers: &HeaderMap,
) -> Result<Value, Value> {
    let creates = args["create"]
        .as_object()
        .ok_or(json!({"type":"invalidArguments"}))?;
    if args["onSuccessDestroyEmail"]
        .as_array()
        .is_some_and(|v| !v.is_empty())
    {
        return Err(
            json!({"type":"invalidArguments","description":"Removing the Sent copy is not supported"}),
        );
    }
    if let Some(patches) = args["onSuccessUpdateEmail"].as_object() {
        for (reference, patch) in patches {
            if !reference
                .strip_prefix('#')
                .is_some_and(|key| creates.contains_key(key))
                || !patch.as_object().is_some_and(|fields| {
                    fields.iter().all(|(key, value)| match key.as_str() {
                        "mailboxIds" => *value == json!({"sent":true}),
                        "keywords/$draft" => value.is_null(),
                        _ => false,
                    })
                })
            {
                return Err(
                    json!({"type":"invalidArguments","description":"Only filing in Sent and clearing the draft flag are supported"}),
                );
            }
        }
    }
    let mut created = Map::new();
    let mut refused = Map::new();
    for (key, submission) in creates {
        let raw = submission["emailId"].as_str().unwrap_or("");
        let id = raw
            .strip_prefix('#')
            .and_then(|r| ids.get(r).map(String::as_str))
            .unwrap_or(raw);
        let Some(number) = id.strip_prefix('d').and_then(|n| n.parse::<i64>().ok()) else {
            refused.insert(key.clone(), json!({"type":"notFound"}));
            continue;
        };
        let draft = {
            let db = connection(store).map_err(|_| json!({"type":"serverFail"}))?;
            db.query_row(
                "SELECT email,submitted FROM jmap_drafts WHERE id=?",
                [number],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(|_| json!({"type":"serverFail"}))?
        };
        let Some((email_json, receipt)) = draft else {
            refused.insert(key.clone(), json!({"type":"notFound"}));
            continue;
        };
        let email: Value =
            serde_json::from_str(&email_json).map_err(|_| json!({"type":"serverFail"}))?;
        if let Some(reason) = submission_refusal(&email, &submission) {
            refused.insert(
                key.clone(),
                json!({"type":"forbiddenToSend","description":reason}),
            );
            continue;
        }
        let part = email["textBody"][0]["partId"].as_str().unwrap_or("text");
        let body_text = email["bodyValues"][part]["value"]
            .as_str()
            .unwrap_or_default();
        let subject = email["subject"].as_str().unwrap_or_default();
        let delivery_text = if subject.is_empty() {
            body_text.to_owned()
        } else {
            format!("Subject: {subject}\n\n{body_text}")
        };
        if body_text.trim().is_empty()
            || delivery_text.chars().count() > OWNER_SEND_LIMIT
            || subject.contains(['\r', '\n'])
        {
            refused.insert(key.clone(),json!({"type":"invalidEmail","description":"Provide a plain-text body and a single-line subject within the internal message limit"}));
            continue;
        }
        if receipt.as_deref() == Some("pending") {
            refused.insert(key.clone(),json!({"type":"serverFail","description":"Submission is in progress or its outcome is unknown. Check the existing Inbox before sending again."}));
            continue;
        }
        if receipt.is_none() {
            let reserved = reserve_draft(store, number, &email_json)?;
            if !reserved {
                refused.insert(key.clone(),json!({"type":"stateMismatch","description":"Draft changed or is already being submitted; reload before retrying"}));
                continue;
            }
            let parent = email["inReplyTo"][0]
                .as_str()
                .and_then(|v| v.strip_suffix("@dunkingsheep.invalid"));
            let mut request = Request::new(axum::body::Body::from(
                json!({"text":delivery_text,"external_id":email["_dsExternalId"],"parent_id":parent})
                    .to_string(),
            ));
            *request.headers_mut() = headers.clone();
            let response = api_owner_send(
                Extension(state.clone()),
                Extension(auth.clone()),
                Extension(scheme),
                request,
            )
            .await;
            let status = response.status();
            let bytes = to_bytes(response.into_body(), 1048576)
                .await
                .map_err(|_| json!({"type":"serverFail"}))?;
            if !status.is_success() {
                refused.insert(
                    key.clone(),
                    json!({"type":"forbiddenToSend","description":String::from_utf8_lossy(&bytes)}),
                );
                continue;
            }
            let db = connection(store).map_err(|_| json!({"type":"serverFail"}))?;
            db.execute(
                "UPDATE jmap_drafts SET submitted=? WHERE id=?",
                params![String::from_utf8_lossy(&bytes), number],
            )
            .map_err(|_| json!({"type":"serverFail"}))?;
        }
        created.insert(
            key.clone(),
            json!({"id":format!("s{number}"),"emailId":id,"undoStatus":"final"}),
        );
    }
    Ok(
        json!({"accountId":"owner","oldState":"0","newState":"1","created":created,"notCreated":refused}),
    )
}

fn append_drafts(store: &MailStore, rows: &mut Vec<Value>) -> Result<(), String> {
    let db = connection(store)?;
    let mut stmt = db
        .prepare("SELECT id,email,submitted FROM jmap_drafts")
        .map_err(|e| e.to_string())?;
    let drafts = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    for draft in drafts {
        let (number, text, sent) = draft.map_err(|e| e.to_string())?;
        let mut email: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        let id = format!("d{number}");
        email["id"] = json!(id);
        email["threadId"] = json!(id);
        email["blobId"] = json!(id);
        let delivered = sent.as_deref().is_some_and(|v| v != "pending");
        if delivered {
            if let Some(keywords) = email["keywords"].as_object_mut() {
                keywords.remove("$draft");
            }
        }
        email["mailboxIds"] = if delivered {
            json!({"sent":true})
        } else {
            json!({"drafts":true})
        };

        email["size"] = json!(text.len());
        email["hasAttachment"] = json!(false);
        email["preview"] = json!("");
        let flags = db
            .query_row(
                "SELECT mailboxes,keywords FROM jmap_flags WHERE id=?",
                [&id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some((b, k)) = flags {
            email["mailboxIds"] = serde_json::from_str(&b).map_err(|e| e.to_string())?;
            email["keywords"] = serde_json::from_str(&k).map_err(|e| e.to_string())?;
        }
        email.as_object_mut().unwrap().remove("_dsExternalId");
        rows.push(email);
    }
    Ok(())
}

fn reserve_draft(store: &MailStore, id: i64, email: &str) -> Result<bool, Value> {
    let db = connection(store).map_err(|_| json!({"type":"serverFail"}))?;
    db.execute(
        "UPDATE jmap_drafts SET submitted='pending' WHERE id=? AND submitted IS NULL AND email=?",
        params![id, email],
    )
    .map(|n| n == 1)
    .map_err(|_| json!({"type":"serverFail"}))
}
#[cfg(test)]
mod submission_tests {
    use super::*;
    #[test]
    fn only_one_submission_reserves_a_draft_and_sent_is_not_a_draft() {
        let dir = tempfile::tempdir().unwrap();
        let store = MailStore(dir.path().join("mail.sqlite3"));
        let email=json!({"keywords":{"$draft":true},"bodyValues":{"text":{"value":"hello"}},"receivedAt":"2026-01-01T00:00:00Z"}).to_string();
        let db = connection(&store).unwrap();
        db.execute("INSERT INTO jmap_drafts(email) VALUES (?)", [&email])
            .unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let workers = (0..2)
            .map(|_| {
                let store = store.clone();
                let email = email.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    reserve_draft(&store, 1, &email).unwrap()
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(
            workers
                .into_iter()
                .filter_map(|w| w.join().unwrap().then_some(()))
                .count(),
            1
        );
        assert_eq!(
            db.execute(
                "DELETE FROM jmap_drafts WHERE id=1 AND submitted IS NULL",
                []
            )
            .unwrap(),
            0
        );
        assert_eq!(
            db.execute(
                "UPDATE jmap_drafts SET email='{}' WHERE id=1 AND submitted IS NULL",
                []
            )
            .unwrap(),
            0
        );
        let mut rows = Vec::new();
        append_drafts(&store, &mut rows).unwrap();
        assert_eq!(rows[0]["mailboxIds"], json!({"drafts":true}));
        db.execute("UPDATE jmap_drafts SET submitted='{}' WHERE id=1", [])
            .unwrap();
        rows.clear();
        append_drafts(&store, &mut rows).unwrap();
        assert_eq!(rows[0]["mailboxIds"], json!({"sent":true}));
        assert!(rows[0]["keywords"].get("$draft").is_none());
    }
}

/// Why a submission cannot be delivered internally, or `None` when it can. Only immediate
/// delivery from the owner identity to exactly one recipient, the desk, is supported; the reason
/// names what the owner has to change.
pub(crate) fn submission_refusal(email: &Value, submission: &Value) -> Option<String> {
    const DESK: &str = "desk@dunkingsheep.invalid";
    let to: Vec<&str> = email["to"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|r| r["email"].as_str().unwrap_or(""))
                .collect()
        })
        .unwrap_or_default();
    if to.is_empty() {
        return Some(format!("Add the recipient: messages can only go to {DESK}"));
    }
    if to != [DESK] {
        return Some(format!(
            "Messages can only go to {DESK}, not to {}",
            to.join(", ")
        ));
    }
    if ["cc", "bcc"]
        .iter()
        .any(|k| email[*k].as_array().is_some_and(|a| !a.is_empty()))
    {
        return Some(format!("Remove Cc and Bcc: messages can only go to {DESK}"));
    }
    if submission["identityId"] != "owner" {
        return Some("Send from your own identity (owner)".to_owned());
    }
    if submission.get("sendAt").is_some() {
        return Some("Scheduled sending is not supported; send now".to_owned());
    }
    if submission["envelope"]["rcptTo"]
        .as_array()
        .is_some_and(|a| a.len() != 1 || a[0]["email"] != DESK)
    {
        return Some(format!("The envelope can only name {DESK}"));
    }
    None
}

#[cfg(test)]
mod submission_refusal_tests {
    use super::submission_refusal;
    use serde_json::json;

    #[test]
    fn each_refusal_names_what_to_change() {
        let ok = json!({"to":[{"email":"desk@dunkingsheep.invalid"}]});
        let owner = json!({"identityId":"owner"});
        assert_eq!(submission_refusal(&ok, &owner), None);
        let cases = [
            (json!({"to":[]}), owner.clone(), "Add the recipient"),
            (json!({}), owner.clone(), "Add the recipient"),
            (
                json!({"to":[{"email":"a@example.com"}]}),
                owner.clone(),
                "not to a@example.com",
            ),
            (
                json!({"to":[{"email":"desk@dunkingsheep.invalid"},{"email":"b@example.com"}]}),
                owner.clone(),
                "b@example.com",
            ),
            (
                json!({"to":[{"email":"desk@dunkingsheep.invalid"}],"cc":[{"email":"c@example.com"}]}),
                owner.clone(),
                "Remove Cc",
            ),
            (ok.clone(), json!({"identityId":"x"}), "own identity"),
            (
                ok.clone(),
                json!({"identityId":"owner","sendAt":"2026-01-01T00:00:00Z"}),
                "Scheduled",
            ),
            (
                ok.clone(),
                json!({"identityId":"owner","envelope":{"rcptTo":[{"email":"a@example.com"}]}}),
                "envelope",
            ),
        ];
        for (email, submission, expected) in cases {
            let reason = submission_refusal(&email, &submission).expect("refused");
            assert!(reason.contains(expected), "{reason} lacks {expected}");
        }
    }
}
