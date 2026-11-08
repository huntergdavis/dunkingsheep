//! The owner escalation channel for the watchdog (t3624 / t3625 / t3700), the Rust mirror of
//! `OwnerMailboxChannel` in `dunk_watchdog.py`.
//!
//! A routine owner escalation is one daemon `inbox_send` to the owner mailbox, as the `watchdog`
//! system sender. It carries no telegram transport and is confirmed on post. A critical emergency
//! page (security, outage or money, t3700) is recorded with the daemon's `owner_page` verb, which
//! writes it to the Telegram outbox as well as the owner mailbox; it is confirmed only when the
//! outbox receipt arrives (`owner_page_confirmed`). The desk brief channel is unchanged and fails
//! closed.

use std::cell::RefCell;

use serde_json::{Map, Value};

use super::run::{inbox_sender, OwnerTransport, SendError};
use crate::client::{FlockClient, Transport};
use crate::error::DunkError;
use crate::owner::conversations::owner_page_number;

/// The owner's mailbox: the Front Desk thread.
pub const OWNER_MAILBOX: &str = "owner";
/// The subject every routine owner escalation carries.
pub const OWNER_MAILBOX_SUBJECT: &str = "Watchdog escalation";

/// The owner-mailbox channel over one lazily connected daemon client.
///
/// The connection is made on the first use, so a run with nothing to escalate never touches the
/// daemon. A connection or write failure is returned as a `SendError` and never retried here.
pub struct OwnerMailbox<T: Transport> {
    connect: Box<dyn Fn() -> Result<FlockClient<T>, DunkError>>,
    client: RefCell<Option<FlockClient<T>>>,
}

impl<T: Transport> OwnerMailbox<T> {
    pub fn new(connect: impl Fn() -> Result<FlockClient<T>, DunkError> + 'static) -> Self {
        Self {
            connect: Box::new(connect),
            client: RefCell::new(None),
        }
    }

    /// Runs one daemon operation on the shared client, connecting on first use.
    fn with_client<R>(
        &self,
        operation: impl FnOnce(&mut FlockClient<T>) -> Result<R, SendError>,
    ) -> Result<R, SendError> {
        let mut slot = self.client.borrow_mut();
        if slot.is_none() {
            let client = (self.connect)().map_err(|error| SendError {
                kind: "DaemonUnavailable".into(),
                message: error.to_string(),
            })?;
            *slot = Some(client);
        }
        operation(slot.as_mut().expect("connected above"))
    }
}

impl<T: Transport> OwnerTransport for OwnerMailbox<T> {
    fn desk_channel(&self) -> Result<String, DunkError> {
        Ok("owner-mailbox".to_owned())
    }

    fn post(&self, body: &str) -> Result<i64, SendError> {
        self.with_client(|client| {
            inbox_sender(client, OWNER_MAILBOX, OWNER_MAILBOX_SUBJECT, body).map_err(|error| {
                SendError {
                    kind: "ClientError".into(),
                    message: error.to_string(),
                }
            })
        })?;
        // The mailbox has no integer id to hand back; the write itself is the record.
        Ok(0)
    }

    fn post_page(&self, emergency: bool, class: &str, body: &str) -> Result<i64, SendError> {
        if !emergency {
            return self.post(body);
        }
        self.with_client(|client| {
            let mut args = Map::new();
            args.insert("body".into(), Value::String(body.to_owned()));
            args.insert("class".into(), Value::String(class.to_owned()));
            let reply = client.call("owner_page", args).map_err(|error| SendError {
                kind: "ClientError".into(),
                message: error.to_string(),
            })?;
            let message_id = reply["message"]["message_id"]
                .as_str()
                .ok_or_else(|| SendError {
                    kind: "BadReply".into(),
                    message: "owner_page returned no message id".into(),
                })?;
            owner_page_number(message_id).ok_or_else(|| SendError {
                kind: "BadReply".into(),
                message: format!("owner_page returned an unusable message id {message_id:?}"),
            })
        })
    }

    fn confirmed_through(&self) -> Result<Option<Value>, DunkError> {
        self.with_client(|client| {
            let reply = client
                .call("owner_page_confirmed", Map::new())
                .map_err(|error| SendError {
                    kind: "ClientError".into(),
                    message: error.to_string(),
                })?;
            Ok(reply
                .get("through")
                .filter(|value| !value.is_null())
                .cloned())
        })
        .map_err(|error| DunkError::Unavailable(error.message))
    }

    fn confirms_on_post(&self) -> bool {
        true
    }

    fn confirms_on_post_for(&self, emergency: bool) -> bool {
        !emergency
    }
}
