//! Narrow read-only store projection used by the owner-conversations service.

use crate::clock::now_s;
use crate::db::map_sqlite;
use crate::error::DunkError;

use super::{attachments_of, full_from_row, refuse, Message, Store, FULL_COLUMNS};

impl Store {
    /// Read one complete message without changing its queued/read state.
    pub fn get_message(&self, message_id: &str) -> Result<Message, DunkError> {
        let message_id = message_id.to_string();
        self.readers.with(move |connection| {
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
            message.attachments = attachments_of(connection, &message_id)?;
            Ok(message)
        })
    }
}
