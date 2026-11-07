//! `run()`: the single-instance long-poll loop, as `telegram_bridge.py run` does it.
//!
//! The order is the Python order: take the lock (a second instance refuses), load the config,
//! prepare the route, refuse a bot that has a webhook (long polling cannot coexist with one), print
//! the running line, then loop. A failed tick is printed by type name only and retried after five
//! seconds; the token never reaches a log.

use std::io::Write;

use ds_core::lock::{Guard, Mode};

use crate::bot::Bot;
use crate::bridge::{Bridge, RETRY_DELAY};
use crate::config::load_config;
use crate::daemon::Daemon;
use crate::error::BridgeError;
use crate::pyval::truthy;

/// Run until the process is stopped. Returns only on a startup error.
pub fn run<D: Daemon, B: Bot>(bridge: &mut Bridge<D, B>) -> Result<(), BridgeError> {
    std::fs::create_dir_all(&bridge.paths.data)
        .map_err(|error| BridgeError::Os(error.to_string()))?;
    let _lock = match Guard::try_acquire(&bridge.paths.lock(), Mode::Exclusive)? {
        Some(guard) => guard,
        None => {
            return Err(BridgeError::Runtime(
                "Telegram bridge is already running".into(),
            ))
        }
    };
    let mut config = load_config(&bridge.paths)?;
    bridge.prepare_route(&mut config)?;
    let webhook = bridge.bot.call("getWebhookInfo", serde_json::Map::new())?;
    if truthy(webhook.get("url")) {
        return Err(BridgeError::Runtime(
            "This bot has a webhook; remove it before using long polling".into(),
        ));
    }
    let channel = bridge.channel(&config)?;
    println!("Telegram bridge running on {channel}; waiting for the paired private chat");
    let _ = std::io::stdout().flush();
    loop {
        if let Err(error) = bridge.tick(&mut config) {
            eprintln!("Telegram bridge retrying after {}", error.python_type());
            std::thread::sleep(RETRY_DELAY);
        }
    }
}
