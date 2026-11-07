//! The bridge's state on disk, in the same files and shapes the Python bridge uses, so the switch
//! from Python to Rust keeps the pairing, the update offset and the forwarding cursor.
//!
//! - `telegram.json` (0600): the bot token and pairing state. Never printed, never logged.
//! - `telegram.lock`: the single-instance flock.
//! - `telegram-status.json` (0644): the forwarding watermark for the watchdog. No secret in it.
//! - `telegram-routing.json`: `{"channel": "..."}` only.
//! - `messages.sqlite3`: the legacy desk store. Read only from here.
//!
//! The data directory is `VP_DATA_DIR`, else `$DUNKINGSHEEP_DIR/desk`, else
//! `~/.config/dunkingsheep/desk`, as `web/app.py` resolves `DATA`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ds_core::atomic_json::python_json_indent2;
use serde_json::{Map, Value};

use crate::error::BridgeError;

/// The parsed `telegram.json`. Insertion order is kept so a save writes the same key order.
pub type Config = Map<String, Value>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub data: PathBuf,
}

impl Paths {
    pub fn new(data: impl Into<PathBuf>) -> Self {
        Paths { data: data.into() }
    }

    /// `VP_DATA_DIR`, else `<config dir>/desk`, where the config dir is `DUNKINGSHEEP_DIR` or
    /// `~/.config/dunkingsheep` (the same rule `ds_core::config::config_dir` applies).
    pub fn from_env() -> Result<Self, BridgeError> {
        let env = ds_core::config::Env::from_process();
        let data = match env
            .get("VP_DATA_DIR")
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            Some(explicit) => PathBuf::from(explicit),
            None => ds_core::config::config_dir(&env)?.join("desk"),
        };
        Ok(Paths::new(expand_user(&data)))
    }

    pub fn config(&self) -> PathBuf {
        self.data.join("telegram.json")
    }
    pub fn lock(&self) -> PathBuf {
        self.data.join("telegram.lock")
    }
    pub fn status(&self) -> PathBuf {
        self.data.join("telegram-status.json")
    }
    pub fn routing(&self) -> PathBuf {
        self.data.join("telegram-routing.json")
    }
    pub fn desk_db(&self) -> PathBuf {
        self.data.join("messages.sqlite3")
    }
}

/// `os.path.expanduser` for a leading `~/`; anything else is returned as given.
fn expand_user(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => match std::env::var("HOME") {
            Ok(home) if !home.is_empty() => PathBuf::from(home).join(rest),
            _ => path.to_path_buf(),
        },
        Err(_) => path.to_path_buf(),
    }
}

/// `load_config`: a missing file is the setup message; bad JSON is a ValueError.
pub fn load_config(paths: &Paths) -> Result<Config, BridgeError> {
    let path = paths.config();
    if !path.exists() {
        return Err(BridgeError::Runtime(
            "Run ./telegram-bridge setup first".into(),
        ));
    }
    let text = fs::read_to_string(&path).map_err(|error| BridgeError::Os(error.to_string()))?;
    match serde_json::from_str::<Value>(&text) {
        Ok(Value::Object(config)) => Ok(config),
        Ok(_) => Err(BridgeError::Value(
            "telegram.json must hold a JSON object".into(),
        )),
        Err(error) => Err(BridgeError::Value(error.to_string())),
    }
}

/// `save_config`: an atomic replace, mode 0600, the same JSON the Python writer produces.
pub fn save_config(paths: &Paths, config: &Config) -> Result<(), BridgeError> {
    let content = python_json_indent2(&Value::Object(config.clone())) + "\n";
    write_atomic(&paths.config(), content.as_bytes(), 0o600)
}

/// `_private_write` and its chmod: write a temporary file in the same directory with the mode set
/// at creation, fsync it, then rename over the target. A reader never sees a partial file.
pub fn write_atomic(path: &Path, content: &[u8], mode: u32) -> Result<(), BridgeError> {
    let dir = path
        .parent()
        .ok_or_else(|| BridgeError::Os(format!("no directory for {}", path.display())))?;
    fs::create_dir_all(dir).map_err(|error| BridgeError::Os(error.to_string()))?;
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".to_string());
    let temporary = dir.join(format!(".{name}.{}.{stamp}.tmp", std::process::id()));
    let result = (|| -> std::io::Result<()> {
        let mut file: File = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        drop(file);
        fs::set_permissions(&temporary, fs::Permissions::from_mode(mode))?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result.map_err(|error| BridgeError::Os(error.to_string()))
}

/// One desk channel from the registry: its id and its display name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Channel {
    pub id: String,
    pub name: Option<String>,
}

/// `_load_channels`: `VP_CHANNELS_FILE` must name a file. The Python fallback to the example
/// registry is not carried over: the units always set the variable (t735).
pub fn load_registry(path: &Path) -> Result<Vec<Channel>, BridgeError> {
    let text = fs::read_to_string(path).map_err(|error| {
        BridgeError::Os(format!("cannot read the desk channel registry: {error}"))
    })?;
    let rows = match serde_json::from_str::<Value>(&text) {
        Ok(Value::Array(rows)) => rows,
        _ => {
            return Err(BridgeError::Value(
                "Telegram desk channel registry is invalid".into(),
            ))
        }
    };
    let mut channels = Vec::with_capacity(rows.len());
    for row in rows {
        let Value::Object(fields) = row else {
            return Err(BridgeError::Value(
                "Telegram desk channel registry is invalid".into(),
            ));
        };
        let Some(Value::String(id)) = fields.get("id") else {
            return Err(BridgeError::Value(
                "Telegram desk channel registry is invalid".into(),
            ));
        };
        let name = match fields.get("name") {
            Some(Value::String(name)) => Some(name.clone()),
            _ => None,
        };
        channels.push(Channel {
            id: id.clone(),
            name,
        });
    }
    Ok(channels)
}

/// `resolve_telegram_channel`: environment > `telegram-routing.json` > `cos`, validated against
/// the registry. The routing file may carry only the `channel` key.
pub fn resolve_channel(
    paths: &Paths,
    env_channel: Option<&str>,
    registry: &[Channel],
) -> Result<String, BridgeError> {
    let routing = paths.routing();
    let mut configured = Value::String("cos".to_string());
    if routing.exists() {
        let text =
            fs::read_to_string(&routing).map_err(|error| BridgeError::Os(error.to_string()))?;
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(fields)) => {
                if fields.keys().any(|key| key != "channel") {
                    return Err(BridgeError::Value(
                        "Telegram routing config must contain only channel".into(),
                    ));
                }
                if let Some(channel) = fields.get("channel") {
                    configured = channel.clone();
                }
            }
            _ => {
                return Err(BridgeError::Value(
                    "Telegram routing config must contain only channel".into(),
                ))
            }
        }
    }
    let channel = match env_channel {
        Some(name) => Value::String(name.to_string()),
        None => configured,
    };
    let Value::String(channel) = channel else {
        return Err(BridgeError::Value(
            "Telegram channel must name a configured desk channel".into(),
        ));
    };
    if !registry.iter().any(|row| row.id == channel) {
        return Err(BridgeError::Value(
            "Telegram channel must name a configured desk channel".into(),
        ));
    }
    Ok(channel)
}

/// The display name for a channel id, as `_channel_name` reads `CHANNEL_INDEX[channel]["name"]`.
pub fn channel_name(registry: &[Channel], channel: &str) -> Result<String, BridgeError> {
    registry
        .iter()
        .find(|row| row.id == channel)
        .and_then(|row| row.name.clone())
        .ok_or_else(|| BridgeError::Value(format!("desk channel {channel} has no name")))
}
