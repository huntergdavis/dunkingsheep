//! `dunkingsheep-telegram`: the Telegram bridge process (t3231). Subcommands match the Python
//! `bridges/telegram_bridge.py`: `setup`, `run`, `status`.
//!
//! Deliberately a separate binary, not a `dunkingsheep` subcommand: the CLI table is golden-tested
//! against the Python argparse output and the root help lists every command, so a new top-level
//! command would move both. The unit's ExecStart is switched to this binary at the owner's go.

#![forbid(unsafe_code)]

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use ds_telegram::bridge::{setup, status_line, wall_clock};
use ds_telegram::config::load_config;
use ds_telegram::daemon::system_client;
use ds_telegram::{run, Bridge, BridgeError, HttpBot, Paths};

fn usage() -> ExitCode {
    eprintln!("usage: dunkingsheep-telegram {{setup,run,status}}");
    ExitCode::from(2)
}

fn fail(error: BridgeError) -> ExitCode {
    // Python prints `str(exc)` for these three kinds and exits 1. Network text is never reached here:
    // the bot layer maps every transport failure to a fixed message with no URL in it.
    eprintln!("{error}");
    ExitCode::from(1)
}

/// `VP_CHANNELS_FILE`, with a leading `~/` expanded. The Rust bridge requires it; the units set it.
fn channels_file() -> Result<PathBuf, BridgeError> {
    match std::env::var("VP_CHANNELS_FILE")
        .ok()
        .filter(|value| !value.is_empty())
    {
        Some(path) => match path.strip_prefix("~/") {
            Some(rest) => match std::env::var("HOME") {
                Ok(home) if !home.is_empty() => Ok(PathBuf::from(home).join(rest)),
                _ => Ok(PathBuf::from(path)),
            },
            None => Ok(PathBuf::from(path)),
        },
        None => Err(BridgeError::Value(
            "VP_CHANNELS_FILE is not set; the channel registry path is required".into(),
        )),
    }
}

/// Read one line from the terminal with echo off, as `getpass` does. Off a terminal, read a line.
fn read_token() -> Result<String, BridgeError> {
    eprint!("BotFather token (hidden): ");
    let _ = std::io::stderr().flush();
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.is_terminal() {
        use nix::sys::termios::{tcgetattr, tcsetattr, LocalFlags, SetArg};
        let original = tcgetattr(&stdin).map_err(|error| BridgeError::Os(error.to_string()))?;
        let mut hidden = original.clone();
        hidden.local_flags.remove(LocalFlags::ECHO);
        tcsetattr(&stdin, SetArg::TCSANOW, &hidden)
            .map_err(|error| BridgeError::Os(error.to_string()))?;
        let read = stdin.lock().read_line(&mut line);
        let _ = tcsetattr(&stdin, SetArg::TCSANOW, &original);
        eprintln!();
        read.map_err(|error| BridgeError::Os(error.to_string()))?;
    } else {
        stdin
            .lock()
            .read_line(&mut line)
            .map_err(|error| BridgeError::Os(error.to_string()))?;
    }
    Ok(line)
}

fn main() -> ExitCode {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let command = match words.as_slice() {
        [command] if matches!(command.as_str(), "setup" | "run" | "status") => command.clone(),
        _ => return usage(),
    };
    let paths = match Paths::from_env() {
        Ok(paths) => paths,
        Err(error) => return fail(error),
    };
    let env_channel = std::env::var("VP_TELEGRAM_CHANNEL").ok();
    match command.as_str() {
        "setup" => match setup(&paths, read_token, HttpBot::new) {
            Ok(text) => {
                println!("{text}");
                ExitCode::SUCCESS
            }
            Err(error) => fail(error),
        },
        "status" => {
            let registry = match channels_file() {
                Ok(path) => path,
                Err(error) => return fail(error),
            };
            match status_line(&paths, &registry, env_channel.as_deref()) {
                Ok(line) => {
                    println!("{line}");
                    ExitCode::SUCCESS
                }
                Err(error) => fail(error),
            }
        }
        _ => {
            let result = (|| -> Result<(), BridgeError> {
                let registry_path = channels_file()?;
                let config = load_config(&paths)?;
                let token = config
                    .get("bot_token")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| BridgeError::Value("telegram.json has no bot_token".into()))?
                    .to_string();
                let mut bridge = Bridge {
                    paths,
                    registry_path,
                    env_channel,
                    daemon: system_client()?,
                    bot: HttpBot::new(&token)?,
                    clock: Box::new(wall_clock),
                };
                run(&mut bridge)
            })();
            match result {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => fail(error),
            }
        }
    }
}
