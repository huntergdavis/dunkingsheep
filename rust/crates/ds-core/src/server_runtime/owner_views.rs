//! Production `OwnerViewEnv` for the owner `message_state` and `reply_stats` views (t1605).
//!
//! The application-log path comes from `DUNKINGSHEEP_APP_DB`, with Python's default. Python reads
//! it once at import; this reads the process environment on each call, which gives the same path
//! for a daemon whose environment does not change while it runs. Inbox reads go through the inbox
//! store attached to this request. A request without that store is refused by name, never answered
//! with zero counts.

use std::path::PathBuf;

use serde_json::{json, Value};

use super::RequestRuntimeAdapter;
use crate::api::OwnerViewEnv;
use crate::config::{CapturedPythonHome, Env};
use crate::error::{refusal, DunkError};
use crate::inbox::{self, MailboxStats, Status};
use crate::inbox_store::Store;

/// `dunk_views.APP_DB`'s default, below the home directory.
const APP_DB_RELATIVE: &str = "workspace/vp-of-agentry/data/messages.sqlite3";

impl OwnerViewEnv for RequestRuntimeAdapter<'_> {
    fn app_db_path(&self) -> Result<PathBuf, DunkError> {
        app_db_path_from(&view_env()?)
    }

    fn inbox_status(&self, message_id: &str, now: f64) -> Result<Option<Value>, DunkError> {
        let store = attached_inbox(self)?;
        match inbox::status(store.read_pool(), message_id, now) {
            Ok(status) => Ok(Some(status_row(&status))),
            // `inbox::status` refuses an unknown id. For the owner view that is a missing row.
            Err(DunkError::Refused(text)) if text.starts_with("no message with id") => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn inbox_stats(&self, now: f64) -> Result<Vec<Value>, DunkError> {
        let store = attached_inbox(self)?;
        Ok(inbox::stats(store.read_pool(), now)?
            .iter()
            .map(stats_row)
            .collect())
    }

    fn seen_latencies(&self, mailbox: &str, limit: usize) -> Result<Vec<f64>, DunkError> {
        let store = attached_inbox(self)?;
        inbox::seen_latencies(store.read_pool(), mailbox, limit)
    }

    fn now(&self) -> f64 {
        crate::clock::now_s()
    }
}

fn attached_inbox<'a>(adapter: &'a RequestRuntimeAdapter<'_>) -> Result<&'a Store, DunkError> {
    adapter
        .inbox
        .as_deref()
        .ok_or_else(|| refusal("owner view inbox store is not attached"))
}

/// The environment the PM and owner views read, captured without panicking. `Env::from_process`
/// panics on a non-UTF-8 variable. `Env::try_from_process` would drop `POOLRAD_CHECKPOINT`, because
/// every name in `ENV_VARS` must start with DUNKINGSHEEP_, and Python's dunk_pm reads it. So the
/// names the views look up are read here, and a non-UTF-8 value is a named refusal.
pub(super) fn view_env() -> Result<Env, DunkError> {
    const NAMES: [&str; 5] = [
        "DUNKINGSHEEP_APP_DB",
        "DUNKINGSHEEP_PM_ROUTING",
        "DUNKINGSHEEP_VP_PANE",
        "POOLRAD_CHECKPOINT",
        "HOME",
    ];
    let mut values = Vec::new();
    for name in NAMES {
        if let Some(value) = std::env::var_os(name) {
            let value = value.into_string().map_err(|_| {
                refusal(format!("environment variable {name:?} is not valid UTF-8"))
            })?;
            values.push((name, value));
        }
    }
    let pairs: Vec<(&str, &str)> = values
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect();
    Ok(Env::from_pairs(&pairs))
}

/// `dunk_views.APP_DB`: `DUNKINGSHEEP_APP_DB` when it is set, else `os.path.expanduser` of the
/// default. `expanduser` keeps a literal `~` when the home directory is unknown, and so does this.
fn app_db_path_from(env: &Env) -> Result<PathBuf, DunkError> {
    if let Some(path) = env.get("DUNKINGSHEEP_APP_DB") {
        return Ok(PathBuf::from(path));
    }
    match CapturedPythonHome::capture(env).expanduser_home() {
        Ok(Some(home)) => Ok(home.join(APP_DB_RELATIVE)),
        Ok(None) => Ok(PathBuf::from(format!("~/{APP_DB_RELATIVE}"))),
        Err(text) => Err(refusal(text)),
    }
}

/// The flat row `flock::views::message_state` reads. It is not the nested `h_inbox_status` shape.
fn status_row(status: &Status) -> Value {
    json!({
        "id": status.message_id,
        "state": status.state,
        "mailbox": status.mailbox,
        "sender": status.sender,
        "created_at": status.created_at,
        "read_at": status.read_at,
        "acked_at": status.acked_at,
        "notify_state": status.notify_state,
        "notify_reason": status.notify_reason,
    })
}

/// One `inbox.stats()` row, with the keys the Python rows carry.
fn stats_row(row: &MailboxStats) -> Value {
    json!({
        "mailbox": row.mailbox,
        "destination": row.destination,
        "unread": row.unread,
        "total": row.total,
        "archived": row.archived,
        "undeliverable": row.undeliverable,
        "oldest_unread_s": row.oldest_unread_s,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flock::panes::Stores;
    use crate::inbox_store::Outgoing;
    use crate::roles::Roles;
    use crate::server::RequestCtx;
    use crate::server_runtime::{RuntimeObservability, RuntimeServiceInputs, RuntimeServices};
    use crate::tasks::Tasks;
    use crate::terminal::fakes::FakeTerminal;
    use crate::terminal::Terminal;
    use std::sync::{Arc, Mutex};

    /// Serialises the tests that set `DUNKINGSHEEP_APP_DB`, as `api::tests` does for its ledger.
    static APP_DB_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// Points `DUNKINGSHEEP_APP_DB` at one path for a test and restores the previous value.
    struct AppDbEnv(Option<std::ffi::OsString>);

    impl AppDbEnv {
        fn set(path: &std::path::Path) -> Self {
            let previous = std::env::var_os("DUNKINGSHEEP_APP_DB");
            std::env::set_var("DUNKINGSHEEP_APP_DB", path);
            Self(previous)
        }
    }

    impl Drop for AppDbEnv {
        fn drop(&mut self) {
            match self.0.take() {
                Some(previous) => std::env::set_var("DUNKINGSHEEP_APP_DB", previous),
                None => std::env::remove_var("DUNKINGSHEEP_APP_DB"),
            }
        }
    }

    fn lock_env() -> std::sync::MutexGuard<'static, ()> {
        APP_DB_ENV_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Real services over temporary stores: one canonical Flock with an attached inbox store.
    fn fixture() -> (tempfile::TempDir, Arc<RuntimeServices>, Arc<Store>) {
        let dir = tempfile::tempdir().unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let tasks = Arc::new(
            runtime
                .block_on(Tasks::open(dir.path().join("tasks.db")))
                .unwrap(),
        );
        runtime.block_on(tasks.allocation_init()).unwrap();
        let inbox =
            Arc::new(Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap());
        let env = Env::default();
        let mut roles = Roles::new(
            &env,
            Some(dir.path().join("roles.json")),
            Some(dir.path().join("tokens.json")),
            None,
        )
        .unwrap();
        roles
            .register("w3:p3", "desk", None, None, None, None, None, None)
            .unwrap();
        let executable = dir.path().join("test-daemon");
        std::fs::write(&executable, b"fixture identity").unwrap();
        let services = RuntimeServices::new(RuntimeServiceInputs {
            launches_file: dir.path().join("launches.json"),
            lane_registry_file: dir.path().join("lanes.json"),
            persist_enabled: false,
            stores: Stores {
                tasks: Some(tasks),
                inbox: Some(Arc::clone(&inbox)),
                ..Stores::default()
            },
            roles,
            legacy_superusers: None,
            release_version: "fixture".into(),
            state_file: dir.path().join("state.json"),
            executable,
            env,
            observability: Some(RuntimeObservability {
                state_dir: dir.path().join("metrics"),
                memory_db: None,
            }),
            runtime,
            terminal_factory: Arc::new(|| Box::new(FakeTerminal::default())),
        })
        .unwrap();
        (dir, services, inbox)
    }

    fn desk() -> RequestCtx {
        RequestCtx {
            self_pane_id: Some("w3:p3".into()),
            socket_peer: Some(true),
            ..RequestCtx::default()
        }
    }

    /// One application row (id 1). It has no answer, so the replied field stays null.
    fn write_app_db(path: &std::path::Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE messages (id INTEGER PRIMARY KEY, role TEXT, channel TEXT, body TEXT, \
             delivery TEXT, delivery_detail TEXT, created_at TEXT, answer_id INTEGER, answered_at TEXT);
             INSERT INTO messages VALUES (1, 'owner', 'vp', 'status?', 'posted', NULL, \
             '2026-10-24T00:00:00Z', NULL, NULL);",
        )
        .unwrap();
    }

    fn cell_value(cell: &Value) -> f64 {
        cell["value"]
            .as_f64()
            .unwrap_or_else(|| panic!("no numeric value in {cell}"))
    }

    #[test]
    fn owner_views_app_db_path_follows_python_default() {
        let explicit = Env::from_pairs(&[
            ("DUNKINGSHEEP_APP_DB", "/data/app.sqlite3"),
            ("HOME", "/home/owner"),
        ]);
        assert_eq!(
            app_db_path_from(&explicit).unwrap(),
            PathBuf::from("/data/app.sqlite3")
        );
        let default = Env::from_pairs(&[("HOME", "/home/owner")]);
        assert_eq!(
            app_db_path_from(&default).unwrap(),
            PathBuf::from("/home/owner/workspace/vp-of-agentry/data/messages.sqlite3")
        );
    }

    #[test]
    fn view_env_refuses_a_non_utf8_value_without_panicking() {
        use std::os::unix::ffi::OsStrExt;
        let _env_lock = lock_env();
        // The panic that `Env::from_process` raised on this input is now a named refusal.
        let bad = std::path::Path::new(std::ffi::OsStr::from_bytes(b"/tmp/\xff.sqlite3"));
        let _app_db = AppDbEnv::set(bad);
        let error = view_env().err().expect("a non-UTF-8 value must be refused");
        assert!(
            matches!(&error, DunkError::Refused(text)
                if text.contains("DUNKINGSHEEP_APP_DB") && text.contains("not valid UTF-8")),
            "{error}"
        );
    }

    #[test]
    fn view_env_keeps_the_python_checkpoint_override() {
        let _env_lock = lock_env();
        let checkpoint = "/tmp/poolrad-checkpoint-under-test.json";
        let previous = std::env::var_os("POOLRAD_CHECKPOINT");
        std::env::set_var("POOLRAD_CHECKPOINT", checkpoint);
        let captured = view_env();
        match previous {
            Some(value) => std::env::set_var("POOLRAD_CHECKPOINT", value),
            None => std::env::remove_var("POOLRAD_CHECKPOINT"),
        }
        assert_eq!(
            captured.unwrap().get("POOLRAD_CHECKPOINT"),
            Some(checkpoint)
        );
    }

    #[test]
    fn owner_views_message_state_reads_app_log_and_inbox_rows() {
        let (dir, services, inbox) = fixture();
        let app_db = dir.path().join("messages.sqlite3");
        write_app_db(&app_db);
        let _env_lock = lock_env();
        let _app_db = AppDbEnv::set(&app_db);
        let id = inbox
            .send(&Outgoing {
                mailbox: "w1:p1",
                body: Some("owner note"),
                sender: Some("pm"),
                ..Default::default()
            })
            .unwrap();
        inbox
            .mark_read(std::slice::from_ref(&id), Some(crate::clock::now_s()))
            .unwrap();
        let result = services
            .production_dispatcher()
            .dispatch(
                "message_state",
                &json!({"message_ids": format!("1,{id}")}),
                &desk(),
            )
            .unwrap();
        let messages = result["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 2, "{result}");
        assert_eq!(messages[0]["found"], true, "{result}");
        assert_eq!(messages[0]["source"], "application", "{result}");
        assert_eq!(messages[1]["found"], true, "{result}");
        assert_eq!(messages[1]["source"], "inbox", "{result}");
        assert_eq!(messages[1]["sent"]["by"], "pm", "{result}");
        assert!(messages[1]["seen"]["at"].is_string(), "{result}");
    }

    #[test]
    fn owner_views_reply_stats_aggregates_real_seen_latencies() {
        let (_dir, services, inbox) = fixture();
        let now = crate::clock::now_s();
        let mut ids = Vec::new();
        for _ in 0..2 {
            ids.push(
                inbox
                    .send(&Outgoing {
                        mailbox: "w1:p1",
                        body: Some("reply probe"),
                        sender: Some("pm"),
                        ..Default::default()
                    })
                    .unwrap(),
            );
        }
        // Read the two messages 4 s and 8 s after each was created.
        let mut mailbox = String::new();
        for (id, delay) in ids.iter().zip([4.0, 8.0]) {
            let status = crate::inbox::status(inbox.read_pool(), id, now).unwrap();
            mailbox = status.mailbox.clone();
            inbox
                .mark_read(std::slice::from_ref(id), Some(status.created_at + delay))
                .unwrap();
        }
        let result = services
            .production_dispatcher()
            .dispatch("reply_stats", &json!({"mailboxes": mailbox}), &desk())
            .unwrap();
        let rows = result["mailboxes"].as_array().unwrap();
        assert_eq!(rows.len(), 1, "{result}");
        let row = &rows[0];
        assert_eq!(row["samples"], 2, "{result}");
        // Latencies are ascending, so the last is the largest and the median averages 4 and 8.
        assert!(
            (cell_value(&row["seen_latency_last_s"]) - 8.0).abs() < 1e-3,
            "{result}"
        );
        assert!(
            (cell_value(&row["seen_latency_s"]) - 6.0).abs() < 1e-3,
            "{result}"
        );
    }

    #[test]
    fn owner_views_refuse_without_an_attached_inbox_store() {
        let (_dir, services, _inbox) = fixture();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let terminal: Mutex<Box<dyn Terminal>> = Mutex::new(Box::new(FakeTerminal::default()));
        let request = desk();
        let mut adapter = RequestRuntimeAdapter::new(
            services.flock.as_ref(),
            &request,
            runtime.handle(),
            &terminal,
        );
        adapter.inbox = None;
        let errors = [
            adapter.inbox_stats(0.0).unwrap_err(),
            adapter.inbox_status("i1", 0.0).unwrap_err(),
            adapter.seen_latencies("w1:p1", 200).unwrap_err(),
        ];
        for error in errors {
            assert!(
                matches!(&error, DunkError::Refused(text) if text.contains("inbox store is not attached")),
                "{error}"
            );
        }
    }

    #[test]
    fn owner_views_app_db_failure_matches_python() {
        let (dir, services, _inbox) = fixture();
        let _env_lock = lock_env();
        let dispatcher = services.production_dispatcher();
        let expected_missing = json!({
            "id": "7",
            "found": false,
            "source": "application",
            "reason": "no application row with this id"
        });
        // A missing file: Python's read-only open fails and `_app_row` returns None.
        let missing = dir.path().join("absent.sqlite3");
        {
            let _app_db = AppDbEnv::set(&missing);
            let result = dispatcher
                .dispatch(
                    "message_state",
                    &json!({"message_ids": "7,i-absent"}),
                    &desk(),
                )
                .unwrap();
            assert_eq!(result["messages"][0], expected_missing, "{result}");
            assert_eq!(
                result["messages"][1],
                json!({"id": "i-absent", "found": false}),
                "{result}"
            );
        }
        // A file that is not SQLite: Python's query raises sqlite3.DatabaseError, which `_app_row`
        // also catches. The owner view must answer, not fail the request.
        let garbage = dir.path().join("garbage.sqlite3");
        std::fs::write(&garbage, b"not a sqlite database").unwrap();
        let _app_db = AppDbEnv::set(&garbage);
        let result = dispatcher
            .dispatch("message_state", &json!({"message_ids": "7"}), &desk())
            .unwrap();
        assert_eq!(result["messages"][0], expected_missing, "{result}");
    }
}
