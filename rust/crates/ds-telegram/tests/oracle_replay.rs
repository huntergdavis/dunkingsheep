//! Golden replay against the Python bridge (t3231, the parity condition).
//!
//! `tests/fixtures/oracle_replay.json` was written by `tools/telegram_oracle_replay.py`, which ran the
//! Python `bridges/telegram_bridge.py` on fakes (stub desk, fake Telegram, fake daemon, fixed clock).
//! Each scenario holds the INPUTS and the OBSERVED Python outputs. This test replays the same inputs
//! through the Rust port and requires the same outputs: every Telegram call with its payload, every
//! daemon call with its arguments, each step's result or exception, and the final telegram.json and
//! status file.
//!
//! Intentional differences, recorded in the fixture instead of normalised away: `/status` reads
//! the Rust owner mailbox rather than Python's legacy desk delivery column (t3671/t3672), and Rust
//! sends attachment blobs through `owner_ingest` so they reach the desk's guarded mailbox and reply
//! outbox. Attachment scenarios carry `input.port` with those Rust calls; those keys replace only
//! their Python counterparts, so each divergence stays explicit and reviewable.
//!
//! Numbers are compared as floats, so an int in one language and a float in the other never decides
//! a comparison. Attachment cases include the stored desk metadata and file contents.

mod common;

use std::path::Path;

use common::{FakeBot, FakeDaemon, NOW};
use ds_telegram::bridge::status_line;
use ds_telegram::config::{load_config, Paths};
use ds_telegram::pyval::{present, truthy};
use ds_telegram::{Bot, Bridge, BridgeError, Config};
use serde_json::{json, Map, Value};

const FIXTURE: &str = "tests/fixtures/oracle_replay.json";

/// The outputs a scenario requires of the port.
///
/// By default that is exactly what the Python bridge did. A scenario with `input.port` records an
/// intentional difference (`/status`, owner-accepted 2026-11-08): its `bot_log` and `daemon_log` are
/// the port's, and they are the ONLY keys it may override. Everything else — steps, config, the
/// status file — stays Python's, so the difference is as narrow as the fixture says it is.
fn expected_outputs(scenario: &Value) -> Value {
    let mut expected = scenario["expected"].clone();
    let Some(port) = scenario["input"]
        .get("port")
        .filter(|value| value.is_object())
    else {
        return expected;
    };
    for key in ["bot_log", "daemon_log", "desk_attachments"] {
        if let Some(value) = port.get(key) {
            expected[key] = value.clone();
        }
    }
    for key in ["steps", "config", "status"] {
        assert!(
            port.get(key).is_none(),
            "{key} may not be overridden by a recorded difference"
        );
    }
    expected
}

/// Every number as an f64, the one spelling both sides are compared in.
fn normalize(value: &Value) -> Value {
    match value {
        Value::Number(number) => json!(number.as_f64().unwrap_or(0.0)),
        Value::Array(items) => Value::Array(items.iter().map(normalize).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), normalize(item)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn error_record(error: &BridgeError) -> Value {
    json!([error.python_type(), error.to_string()])
}

/// The scripted run loop, as the Python `run()` executes it: the webhook check, then one tick per
/// scripted batch, then the getUpdates that finds no batch (recorded, then ends the loop).
fn run_loop(
    bridge: &mut Bridge<FakeDaemon, FakeBot>,
    config: &mut Config,
    bot: &FakeBot,
    batches: Vec<Vec<Value>>,
) -> Result<(), BridgeError> {
    bot.0.borrow_mut().batches = batches.into_iter().collect();
    bridge.prepare_route(config)?;
    let webhook = bridge.bot.call("getWebhookInfo", Map::new())?;
    if truthy(webhook.get("url")) {
        return Err(BridgeError::Runtime(
            "This bot has a webhook; remove it before using long polling".into(),
        ));
    }
    loop {
        let pending = bot.0.borrow().batches.len();
        if pending == 0 {
            let mut params = Map::new();
            if let Some(offset) = present(config.get("update_offset")) {
                params.insert("offset".into(), offset.clone());
            }
            params.insert("timeout".into(), json!(10));
            params.insert("allowed_updates".into(), json!(["message"]));
            // The Python call that raises StopLoop is still a call: it is in the log.
            let _ = bridge.bot.call("getUpdates", params);
            return Ok(());
        }
        // The Python loop prints a failed tick and retries; the step records nothing for it.
        let _ = bridge.tick(config);
    }
}

/// Replay one scenario and return its observed outputs, in the fixture's shape.
fn replay(input: &Value) -> Value {
    let dir = tempfile::TempDir::new().expect("temp dir");
    let data = dir.path().join("desk");
    std::fs::create_dir_all(&data).expect("data dir");
    let paths = Paths::new(&data);
    let registry = dir.path().join("channels.json");
    std::fs::write(&registry, input["registry"].to_string()).expect("registry");

    if !input["routing"].is_null() {
        std::fs::write(paths.routing(), input["routing"].to_string()).expect("routing");
    }
    if !input["config"].is_null() {
        std::fs::write(paths.config(), input["config"].to_string()).expect("config");
    }
    {
        // The desk store always has its table, as the real desk does. `delivery` and
        // `external_id` are the two columns `/status` reads.
        let conn = rusqlite::Connection::open(paths.desk_db()).expect("desk db");
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS messages (id INTEGER PRIMARY KEY, channel TEXT, role TEXT, \
             body TEXT, delivery TEXT NOT NULL DEFAULT 'posted', external_id TEXT);\
             CREATE TABLE IF NOT EXISTS attachments (id TEXT PRIMARY KEY, filename TEXT NOT NULL, \
             mime TEXT NOT NULL, size INTEGER NOT NULL, path TEXT NOT NULL, message_id INTEGER);",
        )
        .expect("schema");
        for row in input["desk_rows"].as_array().cloned().unwrap_or_default() {
            // `(id, channel, role, body[, external_id])`: a row of any role.
            conn.execute(
                "INSERT INTO messages (id, channel, role, body, external_id) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                rusqlite::params![
                    row[0].as_i64().unwrap(),
                    row[1].as_str().unwrap(),
                    row[2].as_str().unwrap(),
                    row[3].as_str().unwrap(),
                    row.get(4).and_then(Value::as_str)
                ],
            )
            .expect("row");
        }
        for row in input["owner_rows"].as_array().cloned().unwrap_or_default() {
            conn.execute(
                "INSERT INTO messages (id, channel, role, body, delivery, external_id) \
                 VALUES (?1, ?2, 'owner', '', ?3, ?4)",
                rusqlite::params![
                    row[0].as_i64().unwrap(),
                    row[1].as_str().unwrap(),
                    row[2].as_str().unwrap(),
                    row[3].as_str().unwrap()
                ],
            )
            .expect("owner row");
        }
    }

    let bot = FakeBot::default();
    {
        let mut state = bot.0.borrow_mut();
        state.webhook_url = input["webhook_url"].as_str().unwrap_or("").to_string();
        state.fail_on = input["fail_on"].as_str().map(str::to_string);
        state.files = input["files"]
            .as_object()
            .into_iter()
            .flat_map(|files| files.iter())
            .map(|(file_id, file)| (file_id.clone(), file.clone()))
            .collect();
    }
    let daemon = FakeDaemon::default();
    {
        let mut state = daemon.0.borrow_mut();
        state.outbox = input["daemon"]["outbox"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        state.ingest = input["daemon"]["ingest"].clone();
        state.conversation = input["daemon"]["conversation"]
            .as_array()
            .cloned()
            .unwrap_or_default();
    }
    let mut bridge = Bridge {
        paths: paths.clone(),
        registry_path: registry.clone(),
        env_channel: None,
        daemon: daemon.clone(),
        bot: bot.clone(),
        clock: Box::new(|| NOW),
    };
    let mut config: Option<Config> = if input["config"].is_null() {
        None
    } else {
        Some(load_config(&paths).expect("config"))
    };

    let mut steps = Vec::new();
    for step in input["steps"].as_array().cloned().unwrap_or_default() {
        let op = step["op"].as_str().unwrap_or("");
        let mut record = Map::new();
        record.insert("op".into(), json!(op));
        record.insert("error".into(), Value::Null);
        match op {
            "process_update" => {
                let cfg = config.as_mut().expect("a config for process_update");
                if let Err(error) = bridge.process_update(&step["update"], cfg) {
                    record.insert("error".into(), error_record(&error));
                }
            }
            "send_owner_outbox" => {
                let cfg = config.as_mut().expect("a config for send_owner_outbox");
                match bridge.send_owner_outbox(cfg) {
                    Ok(sent) => {
                        let sent: Vec<Value> = sent
                            .iter()
                            .map(|item| {
                                json!({"message_id": item.message_id, "kind": item.kind, "reason": item.reason})
                            })
                            .collect();
                        record.insert("sent".into(), Value::Array(sent));
                    }
                    Err(error) => {
                        record.insert("error".into(), error_record(&error));
                    }
                }
            }
            "run" => {
                let cfg = config.as_mut().expect("a config for run");
                let batches = step["batches"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|batch| batch.as_array().cloned().unwrap_or_default())
                    .collect();
                match run_loop(&mut bridge, cfg, &bot, batches) {
                    Ok(()) => {
                        record.insert("stopped".into(), json!(true));
                    }
                    Err(error) => {
                        record.insert("error".into(), error_record(&error));
                    }
                }
            }
            "status" => {
                let (stdout, stderr) = match status_line(&paths, &registry, None) {
                    Ok(line) => (format!("{line}\n"), String::new()),
                    Err(error) => (String::new(), format!("{error}\n")),
                };
                record.insert("stdout".into(), json!(stdout));
                record.insert("stderr".into(), json!(stderr));
            }
            "write_config" => {
                std::fs::write(paths.config(), step["config"].to_string()).expect("config");
                config = Some(load_config(&paths).expect("config"));
            }
            "set_fail_on" => {
                bot.0.borrow_mut().fail_on = step["value"].as_str().map(str::to_string);
            }
            other => panic!("unknown op in the fixture: {other}"),
        }
        steps.push(Value::Object(record));
        // The config the next step sees is the one on disk, as the Python replay reloads it.
        if matches!(op, "process_update" | "send_owner_outbox") && paths.config().exists() {
            config = Some(load_config(&paths).expect("config"));
        }
    }

    let bot_log: Vec<Value> = bot
        .0
        .borrow()
        .log
        .iter()
        .map(|(method, payload)| json!([method, Value::Object(payload.clone())]))
        .collect();
    let daemon_log: Vec<Value> = daemon
        .0
        .borrow()
        .calls
        .iter()
        .map(|(name, kwargs)| {
            let mut kwargs = kwargs.clone();
            if let Some(paths) = kwargs.get_mut("attach").and_then(Value::as_array_mut) {
                for path in paths {
                    if let Some(value) = path.as_str() {
                        *path = json!(Path::new(value).file_name().unwrap().to_string_lossy());
                    }
                }
            }
            json!([name, Value::Object(kwargs)])
        })
        .collect();
    let config_on_disk = std::fs::read_to_string(paths.config())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    let status = std::fs::read_to_string(paths.status())
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .map(|mut value| {
            if let Some(object) = value.as_object_mut() {
                object.remove("updated_at");
            }
            value
        })
        .unwrap_or(Value::Null);
    let desk_attachments = desk_attachment_snapshot(&paths.desk_db());
    json!({
        "steps": steps,
        "bot_log": bot_log,
        "daemon_log": daemon_log,
        "config": config_on_disk,
        "status": status,
        "desk_attachments": desk_attachments,
    })
}

fn desk_attachment_snapshot(database: &Path) -> Value {
    use std::collections::BTreeMap;

    let conn = rusqlite::Connection::open(database).expect("desk db");
    let mut statement = conn
        .prepare(concat!(
            "SELECT m.id,m.channel,m.body,m.delivery,m.external_id,",
            "a.filename,a.mime,a.size,a.path FROM messages m ",
            "JOIN attachments a ON a.message_id=m.id ORDER BY m.id,a.rowid"
        ))
        .expect("attachment snapshot query");
    let mut rows = statement.query([]).expect("attachment snapshot rows");
    let mut messages: BTreeMap<i64, Value> = BTreeMap::new();
    while let Some(row) = rows.next().expect("attachment snapshot row") {
        let message_id: i64 = row.get(0).expect("message id");
        let channel: String = row.get(1).expect("channel");
        let body: String = row.get(2).expect("body");
        let delivery: String = row.get(3).expect("delivery");
        let external_id: Option<String> = row.get(4).expect("external id");
        let filename: String = row.get(5).expect("filename");
        let mime: String = row.get(6).expect("mime");
        let size: i64 = row.get(7).expect("size");
        let path: String = row.get(8).expect("attachment path");
        let content = std::fs::read(path).expect("stored attachment bytes");
        let message = messages.entry(message_id).or_insert_with(|| {
            json!({
                "message_id": message_id,
                "channel": channel,
                "body": body,
                "delivery": delivery,
                "external_id": external_id,
                "attachments": [],
            })
        });
        message["attachments"]
            .as_array_mut()
            .expect("attachment list")
            .push(json!({
                "filename": filename,
                "mime": mime,
                "size": size,
                "content": String::from_utf8_lossy(&content),
            }));
    }
    Value::Array(messages.into_values().collect())
}

#[test]
fn every_scenario_matches_the_python_oracle() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    let text = std::fs::read_to_string(&path).expect("the oracle fixture");
    let document: Value = serde_json::from_str(&text).expect("fixture JSON");
    assert_eq!(
        document["now"].as_f64(),
        Some(NOW),
        "the fixture was recorded at the pinned clock"
    );
    let scenarios = document["scenarios"].as_array().expect("scenarios");
    assert!(
        scenarios.len() >= 30,
        "only {} scenarios in the fixture",
        scenarios.len()
    );

    let mut failures = Vec::new();
    for scenario in scenarios {
        let name = scenario["name"].as_str().unwrap_or("?").to_string();
        let expected = normalize(&expected_outputs(scenario));
        let actual = normalize(&replay(&scenario["input"]));
        if expected != actual {
            let keys: Vec<&str> = [
                "steps",
                "bot_log",
                "daemon_log",
                "config",
                "status",
                "desk_attachments",
            ]
            .into_iter()
            .filter(|key| expected[*key] != actual[*key])
            .collect();
            failures.push(format!("{name}: differs in {keys:?}"));
            if failures.len() == 1 {
                eprintln!(
                    "first difference, {name}\nexpected: {}\nactual:   {}",
                    serde_json::to_string_pretty(&expected).unwrap(),
                    serde_json::to_string_pretty(&actual).unwrap()
                );
            }
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} scenarios differ from the Python oracle:\n{}",
        failures.len(),
        scenarios.len(),
        failures.join("\n")
    );
}
