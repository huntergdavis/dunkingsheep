//! `bridges/telegram_bridge.py`: pairing, inbound text, the outbox loop and the run loop.
//!
//! The bridge keeps no store of its own. An inbound question is recorded in the one owner mailbox
//! with `transport=telegram` and `external_id=telegram:<update_id>`; a reply or a page is read from
//! the daemon's outbox and confirmed with `owner_delivered`. The only desk-side state it touches is
//! the legacy store's page rows and the forwarding cursor, read through `desk_store`.
//!
//! Not in this build (each is a visible reply or a named error, never a silent drop):
//! attachments and the watchdog's own status file beyond the watermark write. See the PR body.
//!
//! `/status` is a deliberate, owner-accepted difference from Python: the Python bridge reads the
//! legacy desk store's `delivery` column, while this bridge records its questions in the one owner
//! mailbox (t594) and so answers from that mailbox's state (t3671/t3672, owner 2026-11-08:
//! "b I want it to be useful"). See `message_status`.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::bot::Bot;
use crate::config::{
    channel_name, load_config, load_registry, resolve_channel, save_config, write_atomic, Config,
    Paths,
};
use crate::daemon::Daemon;
use crate::desk_store;
use crate::error::BridgeError;
use crate::policy::{
    chunks, deliver, is_page, outbox_items, Row, Sent, CHUNK_LIMIT, KIND_MAILBOX_PAGE, KIND_PAGE,
    KIND_REPLY, OWNER_PAGE_SENDER, PAGE_MAX_PER_HOUR, PAGE_WINDOW_S,
};
use crate::pyval::{as_int, display, present, truthy};

/// Seconds the loop waits after a failed tick, as the Python `time.sleep(5)`.
pub const RETRY_DELAY: Duration = Duration::from_secs(5);
/// The long-poll window for `getUpdates`, as the Python `timeout=10`.
pub const POLL_TIMEOUT_S: i64 = 10;
/// How many owner-mailbox rows `/status` reads per `owner_conversation` page.
pub const STATUS_PAGE: i64 = 100;
/// How many pages it walks back before it reports uncertainty instead of an answer. The walk stops
/// early at the end of the mailbox, so this is a ceiling, not a cost per request.
pub const STATUS_MAX_PAGES: usize = 10;

/// Is this owner-mailbox row a question THIS bridge recorded?
///
/// Two conditions, both load-bearing:
///
/// - `owner_ingest` writes the transport's own id, `telegram:<update_id>`, as the dedupe key. A
///   daemon-written page or a lane message carries no such id, and a web question carries `web:*`.
/// - a question is a row nothing answers. `owner_reply` writes `reply_to`, and `owner_delivered`
///   MAY stamp the sent Telegram message's id onto that reply row as its `external_id` (the verb
///   accepts one; this bridge passes none today), so the id alone is not enough to exclude a
///   reply.
fn is_ingested_question(row: &Value) -> bool {
    if present(row.get("reply_to")).is_some() {
        return false;
    }
    row.get("external_id")
        .and_then(Value::as_str)
        .is_some_and(|id| id.starts_with("telegram:"))
}

/// A row's `created_at`, the only field the daemon's page cursor reads.
fn created_at(row: &Value) -> Option<f64> {
    row.get("created_at").and_then(Value::as_f64)
}

/// The newest question in a page (the daemon returns rows oldest first).
fn newest_question(rows: &[Value]) -> Option<&Value> {
    rows.iter().rev().find(|row| is_ingested_question(row))
}

/// Where the bridge reads its channel registry and routing from, and how it talks to the world.
pub struct Bridge<D: Daemon, B: Bot> {
    pub paths: Paths,
    /// `VP_CHANNELS_FILE`: the desk channel registry.
    pub registry_path: PathBuf,
    /// `VP_TELEGRAM_CHANNEL`, when set; it overrides `telegram-routing.json`.
    pub env_channel: Option<String>,
    pub daemon: D,
    pub bot: B,
    /// Wall-clock seconds. Production uses `SystemTime`; tests pin it.
    pub clock: Box<dyn Fn() -> f64>,
}

/// `time.time()` as seconds since the epoch.
pub fn wall_clock() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0)
}

fn args(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

/// `message.get("text") or message.get("caption") or ""`.
fn message_text(message: &Value) -> String {
    for key in ["text", "caption"] {
        if let Some(text) = message.get(key).and_then(Value::as_str) {
            if !text.is_empty() {
                return text.to_string();
            }
        }
    }
    String::new()
}

/// Does the message carry a file the bridge would have to download? Any of the Python keys.
fn has_attachment(message: &Value) -> bool {
    truthy(message.get("document"))
        || truthy(message.get("photo"))
        || truthy(message.get("video"))
        || truthy(message.get("audio"))
        || truthy(message.get("voice"))
}

/// `secrets.compare_digest` for two strings: the same answer, without an early exit.
fn constant_time_eq(left: &str, right: &str) -> bool {
    let (left, right) = (left.as_bytes(), right.as_bytes());
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right)
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

const URL_SAFE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// `secrets.token_urlsafe(n)`: n random bytes, base64url without padding, from the kernel.
pub fn token_urlsafe(bytes: usize) -> Result<String, BridgeError> {
    use std::io::Read;
    let mut random = vec![0u8; bytes];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut file| file.read_exact(&mut random))
        .map_err(|error| BridgeError::Os(format!("random source: {error}")))?;
    let mut out = String::new();
    for chunk in random.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, byte)| {
            acc | (u32::from(*byte) << (16 - 8 * i))
        });
        let symbols = chunk.len() + 1;
        for i in 0..symbols {
            out.push(URL_SAFE[((n >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
    }
    Ok(out)
}

impl<D: Daemon, B: Bot> Bridge<D, B> {
    /// `_channel`: the channel this bridge serves, from the routing file and the environment.
    pub fn channel(&self, config: &Config) -> Result<String, BridgeError> {
        if config.contains_key("channel") {
            return Err(BridgeError::Value(
                "Move Telegram channel from credential config to telegram-routing.json".into(),
            ));
        }
        let registry = load_registry(&self.registry_path)?;
        resolve_channel(&self.paths, self.env_channel.as_deref(), &registry)
    }

    /// `_channel_name`: the desk's display name for a channel id.
    pub fn channel_name(&self, channel: &str) -> Result<String, BridgeError> {
        let registry = load_registry(&self.registry_path)?;
        channel_name(&registry, channel)
    }

    /// `_message_status`, answered from the owner mailbox instead of the desk store.
    ///
    /// Python reconciles the desk store and reports the newest `role='owner'` row whose
    /// `external_id` is `telegram:*`; this bridge never writes such a row for a text question (t594
    /// moved it into the one owner mailbox), so the honest answer is the mailbox row the bridge
    /// itself recorded. The owner accepted that difference and its wording on 2026-11-08 (t3672
    /// acceptance, "b I want it to be useful"):
    ///
    /// - a question this bridge ingested (the `telegram:<update_id>` `external_id` that
    ///   `owner_ingest` deduplicates on, and no `reply_to`) is the row reported;
    /// - the reply is `"<desk name> message #<id>: <state>"` with the mailbox state the daemon
    ///   reports, or `"No Telegram message has reached <desk name> yet."` when there is none.
    ///
    /// A reply is not a question, and a delivered page is not either, so both are skipped.
    ///
    /// The mailbox is walked backwards in pages (`before` = the oldest `created_at` seen), because
    /// the conversation also carries owner-role lane mailboxes and unrelated traffic can fill any
    /// fixed window (t3535; reviewer t3745).
    ///
    /// The strict cursor hides same-timestamp rows (reviewer t3762): `created_at < before` cannot
    /// return the siblings of the row it stops at, so a page that ends on a timestamp shared by
    /// more rows than fit leaves them invisible. That is not only a missing answer, it can make an
    /// OLDER question look like the latest one. So each boundary is PROVED before the walk goes
    /// below it (`boundary_cluster`), and two outcomes are never confused:
    ///
    /// - a page shorter than `STATUS_PAGE`, with every boundary above it proved, ends the mailbox:
    ///   "No Telegram message has reached … yet" is then a fact;
    /// - a boundary whose cluster will not fit in one page, `STATUS_MAX_PAGES` full pages, or a
    ///   cursor that will not advance is NOT a fact: the reply says the message could not be found
    ///   and points at the web desk.
    fn message_status(&mut self, channel: &str) -> Result<String, BridgeError> {
        let name = self.channel_name(channel)?;
        let mut before: Option<f64> = None;
        for _ in 0..STATUS_MAX_PAGES {
            let rows = self.mailbox_page(before)?;
            let full = rows.len() >= STATUS_PAGE as usize;
            let oldest = rows.first().and_then(created_at);
            if let Some(row) = newest_question(&rows) {
                // A question strictly newer than the page's oldest timestamp has no same-timestamp
                // sibling above it, and every boundary above was already proved: it is the answer.
                if !full || created_at(row) != oldest {
                    return Ok(message_line(&name, row));
                }
                // A question ON the boundary may have newer siblings the cursor hid. The cluster
                // decides, and if the cluster will not fit, the answer is uncertainty.
                let Some(boundary) = oldest else {
                    return Ok(uncertain_line(&name));
                };
                let Some(cluster) = self.boundary_cluster(boundary)? else {
                    return Ok(uncertain_line(&name));
                };
                return Ok(message_line(
                    &name,
                    newest_question(&cluster).unwrap_or(row),
                ));
            }
            if !full {
                // Every boundary above was proved complete, so this short page is the end of the
                // mailbox and the absence is real.
                return Ok(format!("No Telegram message has reached {name} yet."));
            }
            let Some(boundary) = oldest else {
                return Ok(uncertain_line(&name));
            };
            // Prove this boundary before looking below it, and scan the cluster too: a hidden
            // sibling at the boundary may be the question the page could not show.
            let Some(cluster) = self.boundary_cluster(boundary)? else {
                return Ok(uncertain_line(&name));
            };
            if let Some(row) = newest_question(&cluster) {
                return Ok(message_line(&name, row));
            }
            // A cursor that does not move would re-read the same page forever.
            if before.is_some_and(|previous| boundary >= previous) {
                return Ok(uncertain_line(&name));
            }
            before = Some(boundary);
        }
        // The walk ran out of pages: never claim absence.
        Ok(uncertain_line(&name))
    }

    /// One `owner_conversation` page: at most `STATUS_PAGE` rows strictly older than `before`,
    /// oldest first.
    fn mailbox_page(&mut self, before: Option<f64>) -> Result<Vec<Value>, BridgeError> {
        let mut params = json!({"limit": STATUS_PAGE});
        if let Some(cursor) = before {
            params["before"] = json!(cursor);
        }
        let answer = self.daemon.call("owner_conversation", args(params))?;
        Ok(answer
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// Every row the daemon will show at `boundary`, or `None` when it cannot be proved complete.
    ///
    /// `created_at < next_up(boundary)` accepts exactly the rows at or below the boundary (no
    /// representable float lies between), so asking for the newest page of those returns the whole
    /// boundary cluster whenever the cluster is smaller than a page. A cluster that fills the page
    /// may have more rows behind it that nothing can enumerate: that is `None`, and the caller says
    /// so instead of guessing.
    fn boundary_cluster(&mut self, boundary: f64) -> Result<Option<Vec<Value>>, BridgeError> {
        let rows = self.mailbox_page(Some(boundary.next_up()))?;
        let cluster: Vec<Value> = rows
            .into_iter()
            .filter(|row| created_at(row) == Some(boundary))
            .collect();
        Ok((cluster.len() < STATUS_PAGE as usize).then_some(cluster))
    }

    /// `_prepare_route`: scope the forwarding cursor to its channel. A switch starts at the
    /// destination's current tail, without replaying history or reusing a partial send.
    pub fn prepare_route(&mut self, config: &mut Config) -> Result<String, BridgeError> {
        let channel = self.channel(config)?;
        let previous = config
            .get("brief_channel")
            .cloned()
            .unwrap_or_else(|| json!("vp"));
        let wanted = json!(channel);
        if previous != wanted {
            let latest = desk_store::latest_agent_row(&self.paths.desk_db(), &channel)?;
            config.insert("last_brief_id".into(), json!(latest));
            config.remove("outbound_message_id");
            config.remove("outbound_part");
        }
        if previous != wanted || !config.contains_key("brief_channel") {
            config.insert("brief_channel".into(), wanted);
            save_config(&self.paths, config)?;
        }
        Ok(channel)
    }

    fn send_text(&mut self, chat_id: &Value, text: &str) -> Result<(), BridgeError> {
        self.bot
            .call(
                "sendMessage",
                args(json!({"chat_id": chat_id, "text": text})),
            )
            .map(|_| ())
    }

    /// `_page_watermark`: where the legacy page scan starts. A first run carries the old cursor
    /// over; a fresh config starts at the newest row, never at history (t667).
    fn page_watermark(&self, config: &mut Config, channel: &str) -> Result<i64, BridgeError> {
        if let Some(stored) = present(config.get("last_page_id")) {
            return as_int(stored)
                .ok_or_else(|| BridgeError::Value("last_page_id must be an integer".into()));
        }
        if let Some(carried) = present(config.get("last_brief_id")) {
            let value = as_int(carried)
                .ok_or_else(|| BridgeError::Value("last_brief_id must be an integer".into()))?;
            config.insert("last_page_id".into(), json!(value));
            save_config(&self.paths, config)?;
            return Ok(value);
        }
        let newest = desk_store::newest_row(&self.paths.desk_db(), channel)?;
        config.insert("last_page_id".into(), json!(newest));
        save_config(&self.paths, config)?;
        Ok(newest)
    }

    /// `_legacy_pages`: watchdog pages still written into the desk store, by their marker.
    fn legacy_pages(&self, config: &mut Config, channel: &str) -> Result<Vec<Row>, BridgeError> {
        let watermark = self.page_watermark(config, channel)?;
        let candidates = desk_store::page_candidates(&self.paths.desk_db(), channel, watermark)?;
        Ok(candidates
            .into_iter()
            .filter(|(_, body)| is_page(body))
            .map(|(id, body)| Row {
                message_id: format!("page-{id}"),
                body,
                kind: KIND_PAGE.to_string(),
                id: Some(id),
            })
            .collect())
    }

    /// `process_update`: one Telegram update, in its order of checks.
    pub fn process_update(
        &mut self,
        update: &Value,
        config: &mut Config,
    ) -> Result<(), BridgeError> {
        let Some(message) = update.get("message").filter(|message| message.is_object()) else {
            return Ok(());
        };
        if message.pointer("/chat/type").and_then(Value::as_str) != Some("private") {
            return Ok(());
        }
        let channel = self.channel(config)?;
        let name = self.channel_name(&channel)?;
        let chat_id = message.pointer("/chat/id").cloned().unwrap_or(Value::Null);
        let text = message_text(message);
        let stripped = text.trim();

        if present(config.get("chat_id")).is_none() {
            let parts: Vec<&str> = stripped.split_whitespace().collect();
            let pair_code = config
                .get("pair_code")
                .and_then(Value::as_str)
                .unwrap_or("");
            if parts.len() == 2 && parts[0] == "/pair" && constant_time_eq(parts[1], pair_code) {
                config.insert("chat_id".into(), chat_id.clone());
                let latest = desk_store::latest_agent_row(&self.paths.desk_db(), &channel)?;
                config.insert("last_brief_id".into(), json!(latest));
                config.insert("brief_channel".into(), json!(channel));
                save_config(&self.paths, config)?;
                self.send_text(
                    &chat_id,
                    &format!("{name} connected. Send text, a photo, or a file. Replies will appear here. /status checks your last send."),
                )?;
            }
            return Ok(());
        }
        if config.get("chat_id") != Some(&chat_id) {
            return Ok(());
        }
        self.prepare_route(config)?;
        if stripped == "/start" || stripped == "/help" {
            return self.send_text(
                &chat_id,
                &format!("Send a message, photo, or file directly to {name}. /status checks your last send. Files over 20 MB use the web desk."),
            );
        }
        if stripped == "/status" {
            let reply = self.message_status(&channel)?;
            return self.send_text(&chat_id, &reply);
        }
        let external_id = format!(
            "telegram:{}",
            update.get("update_id").map(display).unwrap_or_default()
        );
        if has_attachment(message) {
            return self.send_text(
                &chat_id,
                "Telegram attachments are not in the Rust bridge yet. Send the text, or use the web desk.",
            );
        }
        if stripped.is_empty() {
            return self.send_text(
                &chat_id,
                "I can receive text, photos, and files. Other Telegram items need the web desk.",
            );
        }
        // t594: the question is a row in the ONE owner mailbox, recorded with its transport. The
        // daemon dedupes on external_id, so a redelivered update returns the row it produced.
        let ingested = self.daemon.call(
            "owner_ingest",
            args(json!({
                "body": text,
                "transport": "telegram",
                "external_id": external_id,
                "subject": Value::Null,
            })),
        )?;
        let message_id = ingested
            .pointer("/message/message_id")
            .map(display)
            .unwrap_or_default();
        let again = if truthy(ingested.get("duplicate")) {
            " (already recorded)"
        } else {
            ""
        };
        self.send_text(
            &chat_id,
            &format!("{name} message #{message_id}{again}. I'll reply here when ready."),
        )
    }

    /// `send_owner_outbox`: deliver replies and pages, confirm each with its receipt.
    pub fn send_owner_outbox(&mut self, config: &mut Config) -> Result<Vec<Sent>, BridgeError> {
        let Some(chat_id) = present(config.get("chat_id")).cloned() else {
            return Ok(Vec::new());
        };
        let channel = self.prepare_route(config)?;
        let answer = self
            .daemon
            .call("owner_outbox", args(json!({"transport": "telegram"})))?;
        let rows = answer
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut replies = Vec::new();
        let mut mailbox_pages = Vec::new();
        for row in &rows {
            let message_id = row.get("message_id").map(display).unwrap_or_default();
            let body = row
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if row.get("sender") == Some(&json!(OWNER_PAGE_SENDER)) {
                mailbox_pages.push(Row {
                    message_id,
                    body,
                    kind: KIND_MAILBOX_PAGE.to_string(),
                    id: None,
                });
            } else {
                replies.push(Row {
                    message_id,
                    body,
                    kind: KIND_REPLY.to_string(),
                    id: None,
                });
            }
        }
        let mut pages = self.legacy_pages(config, &channel)?;
        pages.extend(mailbox_pages);
        let now = (self.clock)();
        let mut sent_page_times: Vec<f64> = config
            .get("page_sent_at")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(as_f64).collect())
            .unwrap_or_default();
        let items = outbox_items(
            &replies,
            &pages,
            &sent_page_times,
            now,
            PAGE_MAX_PER_HOUR,
            PAGE_WINDOW_S,
        );

        let Bridge {
            paths, daemon, bot, ..
        } = self;
        let send = |body: &str| -> Result<(), BridgeError> {
            for part in chunks(body, CHUNK_LIMIT) {
                bot.call(
                    "sendMessage",
                    args(json!({"chat_id": chat_id, "text": part})),
                )?;
            }
            Ok(())
        };
        let mark = |message_id: &str, kind: &str| -> Result<(), BridgeError> {
            if kind == KIND_REPLY || kind == KIND_MAILBOX_PAGE {
                daemon.call(
                    "owner_delivered",
                    args(json!({"message_id": message_id, "transport": "telegram"})),
                )?;
                if kind == KIND_REPLY {
                    return Ok(());
                }
            }
            // Every delivered page spends the budget, on the live list (t685).
            sent_page_times.retain(|at| now - at < PAGE_WINDOW_S);
            sent_page_times.push(now);
            config.insert("page_sent_at".into(), json!(sent_page_times));
            // Only a page that reached the phone moves the watermark (t667).
            for item in items.iter().filter(|item| item.message_id == message_id) {
                if let Some(id) = item.id {
                    let current = config.get("last_page_id").and_then(as_int).unwrap_or(0);
                    config.insert("last_page_id".into(), json!(current.max(id)));
                }
            }
            save_config(paths, config)?;
            let last = config.get("last_page_id").and_then(as_int).unwrap_or(0);
            write_status_file(paths, last, &channel, now)
        };
        let sent = deliver(&items, send, mark)?;
        for item in items.iter().filter(|item| item.hold) {
            eprintln!("telegram held {}: {}", item.message_id, item.reason);
        }
        Ok(sent)
    }

    /// One pass of the loop: poll, handle each update in order (saving the offset after each),
    /// then send the outbox.
    pub fn tick(&mut self, config: &mut Config) -> Result<(), BridgeError> {
        let mut params = Map::new();
        if let Some(offset) = present(config.get("update_offset")) {
            params.insert("offset".into(), offset.clone());
        }
        params.insert("timeout".into(), json!(POLL_TIMEOUT_S));
        params.insert("allowed_updates".into(), json!(["message"]));
        let updates = self.bot.call("getUpdates", params)?;
        for update in updates.as_array().cloned().unwrap_or_default() {
            self.process_update(&update, config)?;
            let update_id = update
                .get("update_id")
                .and_then(as_int)
                .ok_or_else(|| BridgeError::Value("update_id must be an integer".into()))?;
            config.insert("update_offset".into(), json!(update_id + 1));
            save_config(&self.paths, config)?;
        }
        self.send_owner_outbox(config)?;
        Ok(())
    }
}

fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    }
}

/// `write_status_file`: the forwarding watermark for the watchdog. Holds no secret, so 0644.
fn write_status_file(
    paths: &Paths,
    last_page_id: i64,
    channel: &str,
    now: f64,
) -> Result<(), BridgeError> {
    let payload = json!({"last_brief_id": last_page_id, "channel": channel, "updated_at": now});
    write_atomic(&paths.status(), format!("{payload}\n").as_bytes(), 0o644)
}

/// `status`: one line, the same fields the Python prints.
pub fn status_line(
    paths: &Paths,
    registry_path: &Path,
    env_channel: Option<&str>,
) -> Result<String, BridgeError> {
    let config = load_config(paths)?;
    if config.contains_key("channel") {
        return Err(BridgeError::Value(
            "Move Telegram channel from credential config to telegram-routing.json".into(),
        ));
    }
    let registry = load_registry(registry_path)?;
    let channel = resolve_channel(paths, env_channel, &registry)?;
    let brief_channel = config
        .get("brief_channel")
        .cloned()
        .unwrap_or_else(|| json!("vp"));
    let paired = if present(config.get("chat_id")).is_some() {
        "yes"
    } else {
        "no"
    };
    Ok(format!(
        "channel={channel}; brief_channel={}; paired={paired}; last_brief_id={}; update_offset={}",
        display(&brief_channel),
        display(&config.get("last_brief_id").cloned().unwrap_or(Value::Null)),
        display(&config.get("update_offset").cloned().unwrap_or(Value::Null)),
    ))
}

/// `setup`: refuse when configured; otherwise read the token, check it with `getMe`, and save a
/// fresh pairing code. Returns the text the Python prints. The token is never part of it.
pub fn setup<B: Bot>(
    paths: &Paths,
    read_token: impl FnOnce() -> Result<String, BridgeError>,
    make_bot: impl FnOnce(&str) -> Result<B, BridgeError>,
) -> Result<String, BridgeError> {
    if paths.config().exists() {
        return Err(BridgeError::Runtime(
            "Telegram is already configured; inspect ./telegram-bridge status".into(),
        ));
    }
    let token = read_token()?;
    let token = token.trim();
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        return Err(BridgeError::Runtime(
            "Token must be a single nonempty value".into(),
        ));
    }
    let bot = make_bot(token)?;
    let info = bot.call("getMe", Map::new())?;
    let username = info.get("username").map(display).unwrap_or_default();
    let pair_code = token_urlsafe(18)?;
    let mut config = Config::new();
    config.insert("bot_token".into(), json!(token));
    config.insert("pair_code".into(), json!(pair_code));
    config.insert("chat_id".into(), Value::Null);
    config.insert("update_offset".into(), Value::Null);
    config.insert("last_brief_id".into(), Value::Null);
    save_config(paths, &config)?;
    Ok(format!(
        "Bot @{username} saved in private local data.\nStart ./install-telegram-service.sh, then DM the bot: /pair {pair_code}"
    ))
}

/// `"<desk name> message #<id>: <state>"`: one answer from one mailbox row.
fn message_line(name: &str, row: &Value) -> String {
    format!(
        "{name} message #{}: {}",
        row.get("id").map(display).unwrap_or_default(),
        row.get("state")
            .map(display)
            .unwrap_or_else(|| "None".to_string())
    )
}

/// The reply when the walk stopped on an ambiguity: never a claim of absence.
fn uncertain_line(name: &str) -> String {
    format!(
    "Could not find a Telegram message for {name} in the {} most recent mailbox rows; check the web desk.",
    (STATUS_MAX_PAGES as i64) * STATUS_PAGE
)
}
