//! Dunking Sheep Telegram bridge (Rust port, t3231).
//!
//! Ports `bridges/telegram_bridge.py` (setup, run, status, the outbox loop), `bridges/telegram_transport.py`
//! (the delivery policy) and `dunk_telegram_routing.py` (channel resolution) with the same state files
//! and the same owner-mailbox verbs. The daemon owns the mailbox; this crate asks it for the outbox,
//! records receipts, and never writes a store of its own beyond the cursor and status file.
//!
//! Layout: `policy` is pure (no I/O); `bot` is the Telegram Bot API client behind a trait; `daemon` is
//! the owner-mailbox client behind a trait; `bridge` holds the behaviour; `runner` is the loop. Tests
//! use fakes for both traits, so none of them needs a network, a token or a running daemon.

#![forbid(unsafe_code)]

pub mod bot;
pub mod bridge;
pub mod config;
pub mod daemon;
pub mod desk_store;
pub mod error;
pub mod policy;
pub mod pyval;
pub mod runner;

pub use bot::{Bot, HttpBot};
pub use bridge::{setup, status_line, wall_clock, Bridge};
pub use config::{Config, Paths};
pub use daemon::{system_client, Daemon};
pub use error::BridgeError;
pub use runner::run;
