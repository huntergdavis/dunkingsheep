//! The owner-mailbox escalation channel (t3624 / t3625): the Rust mirror of `OwnerMailboxChannel`
//! in `dunk_watchdog.py`.
//!
//! An owner escalation is one daemon `inbox_send` to the owner mailbox, as the `watchdog` system
//! sender. The row has no telegram transport and no owner-page sender, so the Telegram outbox never
//! carries it: this channel never pages the phone. It is confirmed on post, because the accepted
//! write is the delivery. The desk brief channel (`DeskBriefChannel`) is unchanged and still fails
//! closed.

use std::cell::RefCell;

use serde_json::Value;

use super::run::{inbox_sender, OwnerTransport, SendError};
use crate::client::{ClientError, FlockClient, Transport};
use crate::error::DunkError;

/// The owner's mailbox: the Front Desk thread.
pub const OWNER_MAILBOX: &str = "owner";
/// The subject every owner escalation carries.
pub const OWNER_MAILBOX_SUBJECT: &str = "Watchdog escalation";

/// The owner-mailbox channel over one lazily connected daemon client.
///
/// The connection is made on the first post, so a run with nothing to escalate never touches the
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
}

impl<T: Transport> OwnerTransport for OwnerMailbox<T> {
    fn desk_channel(&self) -> Result<String, DunkError> {
        Ok("owner-mailbox".to_owned())
    }

    fn post(&self, body: &str) -> Result<i64, SendError> {
        let mut slot = self.client.borrow_mut();
        if slot.is_none() {
            let client = (self.connect)().map_err(|error| SendError {
                kind: "DaemonUnavailable".into(),
                message: error.to_string(),
            })?;
            *slot = Some(client);
        }
        let client = slot.as_mut().expect("connected above");
        inbox_sender(client, OWNER_MAILBOX, OWNER_MAILBOX_SUBJECT, body).map_err(
            |error: ClientError| SendError {
                kind: "ClientError".into(),
                message: error.to_string(),
            },
        )?;
        // The mailbox has no integer id to hand back; the write itself is the record. A confirm-on-post
        // page is never compared against a forwarder id, so the stored `brief_id` is informational.
        Ok(0)
    }

    fn confirmed_through(&self) -> Result<Option<Value>, DunkError> {
        Ok(None)
    }

    fn confirms_on_post(&self) -> bool {
        true
    }
}
