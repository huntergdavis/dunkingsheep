//! Narrow read-only store projection used by the owner-conversations service.

use crate::clock::now_s;
use crate::db::ErrorContext;
use crate::error::DunkError;

use super::{attachments_of_context, full_from_row, refuse, Message, Store, FULL_COLUMNS};

impl Store {
    /// Read one complete message without changing its queued/read state.
    pub fn get_message(&self, message_id: &str) -> Result<Message, DunkError> {
        self.get_message_context(message_id, &ErrorContext::default())
    }
    pub(crate) fn get_message_context(
        &self,
        message_id: &str,
        context: &ErrorContext,
    ) -> Result<Message, DunkError> {
        let message_id = message_id.to_string();
        self.readers.with(move |connection| {
            let map_sqlite = |error| context.sqlite(error, self.readers.path(), "get_message");
            let mut message = {
                let mut statement = connection
                    .prepare(&format!("SELECT {FULL_COLUMNS} FROM messages WHERE id = ?"))
                    .map_err(map_sqlite)?;
                match statement.query_row([message_id.as_str()], |row| full_from_row(row, now_s()))
                {
                    Ok(message) => message,
                    Err(rusqlite::Error::QueryReturnedNoRows) => {
                        return Err(refuse(format!("no message with id {message_id:?}")));
                    }
                    Err(error) => return Err(map_sqlite(error)),
                }
            };
            message.attachments = attachments_of_context(connection, &message_id, &map_sqlite)?;
            Ok(message)
        })
    }
}
