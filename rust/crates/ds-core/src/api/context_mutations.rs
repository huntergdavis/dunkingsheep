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
    context.flock.memory_backfill_raw(
        args.get("action").unwrap_or(&Value::Null),
        args.get("limit").unwrap_or(&Value::Null),
    )
}
