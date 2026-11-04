//! The three direct-message API handlers; IDs retain their raw JSON values.
use super::{Args, HandlerContext};
use crate::error::DunkError;
use serde_json::Value;

pub trait OutboxCommandEnv {
    fn list_messages(&self) -> Result<Value, DunkError>;
    fn get_message(&self, id: &Value) -> Result<Value, DunkError>;
    fn cancel_message(&self, id: &Value) -> Result<Value, DunkError>;
}

pub fn h_list_messages(context: &HandlerContext<'_>, _args: Args) -> Result<Value, DunkError> {
    context
        .env
        .with_outbox_commands(&mut |env| env.list_messages())
}
pub fn h_get_message(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args
        .get("message_id")
        .ok_or_else(|| DunkError::Refused("'message_id'".into()))?;
    context
        .env
        .with_outbox_commands(&mut |env| env.get_message(id))
}
pub fn h_cancel_message(context: &HandlerContext<'_>, args: Args) -> Result<Value, DunkError> {
    let id = args
        .get("message_id")
        .ok_or_else(|| DunkError::Refused("'message_id'".into()))?;
    context
        .env
        .with_outbox_commands(&mut |env| env.cancel_message(id))
}
