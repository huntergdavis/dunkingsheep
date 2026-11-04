//! Inbox command adapters using the request's existing inbox services.
use super::*;
use crate::flock::commands_inbox::SendArgs;
use crate::inbox_store::Transition;

impl Dispatcher {
    pub fn register_inbox_messages(&mut self) {
        for (name, handler) in [
            ("inbox_send", h_inbox_send as Handler),
            ("inbox_count", h_inbox_count),
            ("inbox_list", h_inbox_list),
            ("inbox_read", h_inbox_read),
            ("inbox_status", h_inbox_status),
            ("inbox_ack", h_inbox_ack),
            ("inbox_cancel", h_inbox_cancel),
            ("inbox_delete", h_inbox_delete),
            ("inbox_recover", h_inbox_recover),
            ("inbox_archive", h_inbox_archive),
            ("inbox_stats", h_inbox_stats),
            ("inbox_fetch", h_inbox_fetch),
        ] {
            self.register(name, handler);
        }
    }
}

fn mailbox(args: &Args) -> Option<String> {
    args.get("mailbox")
        .filter(|v| truthy(v))
        .map(python_value_string)
}
fn ids(args: &Args) -> Result<Vec<String>, DunkError> {
    let value = args.get("message_ids").unwrap_or(&Value::Null);
    let values: Vec<String> = match value {
        Value::String(s) => s.split(',').map(str::to_owned).collect(),
        Value::Array(v) => v.iter().map(python_value_string).collect(),
        Value::Object(v) => v.keys().cloned().collect(),
        _ => {
            return Err(DunkError::Refused(format!(
                "'{}' object is not iterable",
                python_value_type_name(value)
            )))
        }
    };
    Ok(values
        .into_iter()
        .filter(|s: &String| !s.trim().is_empty())
        .collect())
}
fn transition(t: Transition) -> Value {
    json!({"state":t.state,"changed":t.changed,"unchanged":t.unchanged.into_iter().map(|(id,state)|json!({"message_id":id,"state":state})).collect::<Vec<_>>()})
}
fn limit(args: &Args, default: i64) -> Result<OwnerOutboxLimit, DunkError> {
    owner_outbox_limit_arg(args, default)
}
fn bound_limit(limit: &OwnerOutboxLimit) -> Result<i64, DunkError> {
    match limit {
        OwnerOutboxLimit::Sqlite(n) => Ok(*n),
        OwnerOutboxLimit::Overflow => Err(DunkError::Refused(
            "Python int too large to convert to SQLite INTEGER".into(),
        )),
    }
}
pub fn h_inbox_count(c: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let m = mailbox(&args);
    if m.is_none() {
        let env = c.env.status_env()?;
        let view = c.env.view_cache()?.view(
            env,
            c.self_pane_id(),
            python_bool_arg(&args, "fresh", false),
            env.view_cache_ttl()?,
        );
        if let Some(count) = view.get("inbox_count") {
            return Ok(count.clone());
        }
    }
    c.env.with_inbox_env(&mut |env| {
        let v=c.flock.inbox_count(env,m.as_deref(),c.self_pane_id())?;
        Ok(json!({"mailbox":v.mailbox,"unread":v.unread,"total":v.total,"archived":v.archived,"undeliverable":v.undeliverable,"oldest_unread_s":v.oldest_unread_s,"attachments_unread":v.attachments_unread,"from_lanes":v.from_lanes}))
    })
}
pub fn h_inbox_list(c: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let m = mailbox(&args);
    let n = limit(&args, 50)?;
    c.env.with_inbox_env(&mut |env| {
        let key = c
            .flock
            .mailbox_or_self(env, m.as_deref(), c.self_pane_id(), false, true)?;
        let rows = crate::inbox::list_signed(
            env.pool(),
            &key,
            python_bool_arg(&args, "unread_only", true),
            bound_limit(&n)?,
            env.now(),
        )?;
        let messages = rows
            .iter()
            .map(|row| {
                let full = env.store().get_message(&row.message_id)?;
                let mut wire = owner_message_value(&full);
                wire.as_object_mut().unwrap().remove("body");
                wire.as_object_mut().unwrap().remove("read_at");
                Ok(wire)
            })
            .collect::<Result<Vec<Value>, DunkError>>()?;
        Ok(json!({"mailbox":key,"messages":messages}))
    })
}
pub fn h_inbox_read(c: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let m = mailbox(&args);
    let n = limit(&args, 10)?;
    c.env.with_inbox_env(&mut |env| {
        c.flock.mailbox_or_self(env,m.as_deref(),c.self_pane_id(),false,true)?;
        let r=c.flock.inbox_read_signed(env,m.as_deref(),bound_limit(&n)?,python_bool_arg(&args,"peek",false),python_bool_arg(&args,"include_tasks",true),c.self_pane_id())?;
        let mut out=json!({"mailbox":r.mailbox,"messages":r.messages.iter().map(owner_message_value).collect::<Vec<_>>()});
        if let Some(tasks)=r.tasks {out["tasks"]=tasks;}
        if let Some(marked)=r.marked_read {out["marked_read"]=json!(marked);}
        Ok(out)
    })
}
pub fn h_inbox_recover(c: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let m = mailbox(&args);
    c.env.with_inbox_env(&mut |env| {
        let (key,rows)=c.flock.inbox_recover(env,m.as_deref(),c.self_pane_id())?;
        Ok(json!({"mailbox":key,"messages":rows.iter().map(owner_message_value).collect::<Vec<_>>()}))
    })
}
pub fn h_inbox_ack(c: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let ids = ids(&args)?;
    let m = mailbox(&args);
    c.env.with_inbox_env(&mut |env| {
        let r = c
            .flock
            .inbox_ack(env, &ids, m.as_deref(), c.self_pane_id())?;
        let mut out = transition(r.transition);
        out["acked_in"] = json!(r.acked_in);
        if let Some(pending) = r.still_pending {
            out["still_pending"] = json!(pending);
        }
        Ok(out)
    })
}
pub fn h_inbox_cancel(c: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let ids = ids(&args)?;
    c.env
        .with_inbox_env(&mut |env| Ok(transition(c.flock.inbox_cancel(env, &ids)?)))
}
pub fn h_inbox_delete(c: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let ids = ids(&args)?;
    c.env
        .with_inbox_env(&mut |env| Ok(json!({"deleted":c.flock.inbox_delete(env,&ids)?})))
}
pub fn h_inbox_status(c: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = python_value_string(&args["message_id"]);
    c.env.with_inbox_env(&mut |env| {
        let s=c.flock.inbox_status(env,&id)?;
        Ok(json!({"message_id":s.message_id,"state":s.state,"mailbox":s.mailbox,"destination":s.destination,"pane_id":s.pane_id,"sender":s.sender,"created_at":s.created_at,"read_at":s.read_at,"acked_at":s.acked_at,"archived_at":s.archived_at,"cancelled_at":s.cancelled_at,"waiting_s":s.waiting_s,"body_bytes":s.body_bytes,"attachments":s.attachments,"attachment_bytes":s.attachment_bytes,"reason":s.reason,"notify":{"state":s.notify_state,"reason":s.notify_reason,"at":s.notify_at,"transport_id":s.notify_transport_id,"attempt":s.notify_attempt}}))
    })
}
pub fn h_inbox_stats(c: &HandlerContext<'_>, _args: Args) -> Result<Value, DunkError> {
    c.env.with_inbox_env(&mut |env| {
        let rows=c.flock.inbox_stats(env)?;
        Ok(json!({"mailboxes":rows.iter().map(|s|json!({"mailbox":s.mailbox,"destination":s.destination,"unread":s.unread,"total":s.total,"archived":s.archived,"undeliverable":s.undeliverable,"oldest_unread_s":s.oldest_unread_s})).collect::<Vec<_>>()}))
    })
}
pub fn h_inbox_fetch(c: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = python_value_string(&args["attachment_id"]);
    let dest = args
        .get("dest")
        .filter(|v| truthy(v))
        .map(python_value_string)
        .map(PathBuf::from);
    c.env.with_inbox_env(&mut |env| {
        let r=c.flock.inbox_fetch(env,&id,dest.as_deref())?;
        Ok(json!({"attachment_id":r.attachment_id,"path":r.path,"filename":r.filename,"size":r.size,"media_type":r.media_type,"message_id":r.message_id}))
    })
}

pub fn h_inbox_send(c: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let to = python_value_string(&args["to"]);
    let body = args
        .get("body")
        .filter(|v| !v.is_null())
        .map(python_value_string)
        .unwrap_or_default();
    let subject = args
        .get("subject")
        .filter(|v| !v.is_null())
        .map(python_value_string);
    let system = args
        .get("system_sender")
        .filter(|v| truthy(v))
        .map(python_value_string);
    c.env.with_inbox_env(&mut |env| {
        let r = c.flock.inbox_send_wire(
            env,
            &SendArgs {
                to: &to,
                body: &body,
                subject: subject.as_deref(),
                attach: &[],
                notify: python_bool_arg(&args, "notify", true),
                self_pane_id: c.self_pane_id(),
                system_sender: system.as_deref(),
            },
            args.get("subject"),
            args.get("attach"),
        )?;
        let mut out = owner_message_value(&r.snapshot);
        out["notified"] = json!(r.notified);
        if let Some(v) = r.redirected {
            out["redirected"] = v;
        }
        if let Some(v) = r.outcome {
            out["outcome"] = json!(v);
        }
        if let Some(v) = r.reason {
            out["reason"] = json!(v);
        }
        Ok(out)
    })
}
fn archive_before(v: Option<&Value>) -> Result<Option<f64>, DunkError> {
    let Some(v) = v.filter(|v| !v.is_null() && **v != json!("")) else {
        return Ok(None);
    };
    if let Value::Bool(b) = v {
        return Ok(Some(if *b { 1.0 } else { 0.0 }));
    }
    let text = python_value_string(v);
    let text = text.trim();
    if let Some(n) = crate::github::parse_python_float(text) {
        return Ok(Some(n));
    }
    if let Ok(date) = chrono::DateTime::parse_from_rfc3339(text) {
        return Ok(Some(
            date.timestamp() as f64 + f64::from(date.timestamp_subsec_nanos()) / 1e9,
        ));
    }
    let local = chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(text, "%Y-%m-%d %H:%M:%S%.f"))
        .or_else(|_| {
            chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d")
                .map(|d| d.and_hms_opt(0, 0, 0).unwrap())
        });
    if let Ok(date) = local {
        use chrono::TimeZone;
        if let Some(date) = chrono::Local.from_local_datetime(&date).earliest() {
            return Ok(Some(
                date.timestamp() as f64 + f64::from(date.timestamp_subsec_nanos()) / 1e9,
            ));
        }
    }
    Err(DunkError::Refused(format!(
        "before must be an ISO date/time or a unix timestamp: {}",
        python_value_repr(v)
    )))
}
pub fn h_inbox_archive(c: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let mailbox = python_value_string(&args["mailbox"]);
    let before = archive_before(args.get("before"))?;
    let states = match args.get("state").or_else(|| args.get("states")) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(
            s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        ),
        Some(Value::Array(a)) => Some(a.iter().map(python_value_string).collect()),
        Some(Value::Object(a)) => Some(a.keys().cloned().collect()),
        Some(v) => {
            return Err(DunkError::Refused(format!(
                "'{}' object is not iterable",
                python_value_type_name(v)
            )))
        }
    };
    c.env.with_inbox_env(&mut |env| {
        let r = c
            .flock
            .inbox_archive(env, &mailbox, before, states.as_deref())?;
        Ok(json!({"mailbox":r.mailbox,"archived":r.archived,"states":r.states,"before":r.before}))
    })
}
