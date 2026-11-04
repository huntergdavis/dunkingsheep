//! Health and state commands, with Python's conversion and store-open ordering.
use super::{Args, HandlerContext, OwnerOutboxLimit};
use crate::{error::DunkError, state::StateStore};
use serde_json::{json, Value};

pub trait HealthStateEnv {
    fn health(&self, repair: bool, deep: bool) -> Result<Value, DunkError>;
    fn capture(&self, source: &Value) -> Result<Value, DunkError>;
    fn with_state_store(
        &self,
        operation: &mut dyn FnMut(&StateStore) -> Result<Value, DunkError>,
    ) -> Result<Value, DunkError>;
}

pub fn h_health(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    context.env.health_state_env()?.health(
        args.get("repair").is_some_and(super::truthy),
        args.get("deep").is_some_and(super::truthy),
    )
}

pub fn h_state_capture(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let source = args
        .get("source")
        .filter(|v| super::truthy(v))
        .cloned()
        .unwrap_or_else(|| json!("periodic"));
    context.env.health_state_env()?.capture(&source)
}

pub fn h_state_latest(context: &HandlerContext<'_>, mut args: Args) -> Result<Value, DunkError> {
    // Python evaluates StateStore().latest before evaluating int(limit or 50).
    context
        .env
        .health_state_env()?
        .with_state_store(&mut |store| {
            if !args.get("limit").is_some_and(super::truthy) {
                args.insert("limit".into(), json!(50));
            }
            if let Some(Value::Number(number)) = args.get("limit") {
                let token = number.to_string();
                if token.contains(['.', 'e', 'E'])
                    && number.as_f64().is_none_or(|value| !value.is_finite())
                {
                    return Err(DunkError::Refused(
                        "internal error: cannot convert float infinity to integer".into(),
                    ));
                }
            }
            let limit = match super::owner_outbox_limit_arg(&args, 50)
                .map_err(crate::error::internal_error)?
            {
                OwnerOutboxLimit::Sqlite(value) => Some(value),
                OwnerOutboxLimit::Overflow => None,
            };
            let rows = store
                .latest_raw(args.get("pane_id"), args.get("project"), limit)
                .map_err(crate::error::internal_error)?;
            Ok(json!({"count": rows.len(), "rows": rows}))
        })
}
