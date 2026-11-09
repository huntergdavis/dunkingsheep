//! T1594: preserve raw arguments until the reached Flock/Store conversion boundary.
use super::*;

pub fn h_context_add(context: &HandlerContext<'_>, mut args: Args) -> Result<Value, DunkError> {
    for (key, default) in [
        ("type", "reference"),
        ("authority", "agent"),
        ("source", ""),
    ] {
        if !args.get(key).is_some_and(truthy) {
            args.insert(key.into(), Value::String(default.into()));
        }
    }
    let global = python_bool_arg(&args, "global_store", true);
    context
        .flock
        .context_add_raw(&args, global, context.self_pane_id(), context.env)
}

pub fn h_context_promote(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let global = python_bool_arg(&args, "global_store", true);
    context
        .flock
        .context_promote_raw(&args, global, context.self_pane_id(), context.env)
}

pub fn h_memory_backfill(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    if let Some(action @ ("quarantine" | "restore")) = args.get("action").and_then(Value::as_str) {
        let id = args
            .get("queue_id")
            .and_then(Value::as_i64)
            .ok_or_else(|| DunkError::Refused("queue_id is required".into()))?;
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| DunkError::Refused("exact source path is required".into()))?;
        let reason = args
            .get("reason")
            .and_then(Value::as_str)
            .ok_or_else(|| DunkError::Refused("reason is required".into()))?;
        let actor = match context.self_pane_id().filter(|pane| !pane.is_empty()) {
            Some(pane) => pane.to_owned(),
            None => context
                .flock
                .owner_token_audit_actor(context.request.service_token.as_deref())?
                .ok_or_else(|| DunkError::Refused("operator identity is required".into()))?,
        };
        let service = context
            .flock
            .memory_service()
            .ok_or_else(|| DunkError::Refused("memory service is not enabled".into()))?;
        service.queue_disposition(id, path, reason, &actor, action == "restore")?;
        return Ok(json!({"action":action,"queue_id":id,"path":path}));
    }
    context.flock.memory_backfill_raw(
        args.get("action").unwrap_or(&Value::Null),
        args.get("limit").unwrap_or(&Value::Null),
    )
}
