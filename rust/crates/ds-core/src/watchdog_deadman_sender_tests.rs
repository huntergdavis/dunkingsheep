use super::*;
use crate::client::{ClientError, FlockClient, Transport};
use std::{cell::RefCell, time::Duration};

struct FakeDaemon {
    calls: RefCell<Vec<Value>>,
    response: Result<String, io::ErrorKind>,
}
impl Transport for &FakeDaemon {
    fn exchange(&self, line: &str, _: Duration) -> io::Result<Option<String>> {
        self.calls
            .borrow_mut()
            .push(serde_json::from_str(line).unwrap());
        self.response.clone().map(Some).map_err(io::Error::from)
    }
}
fn client(daemon: &FakeDaemon) -> FlockClient<&FakeDaemon> {
    FlockClient::new(
        daemon,
        Some("fake-pane".into()),
        "test".into(),
        Duration::from_secs(1),
        None,
    )
}
fn rows(path: &Path) -> i64 {
    let connection = rusqlite::Connection::open(path).unwrap();
    connection
        .query_row("SELECT count(*) FROM messages", [], |row| row.get(0))
        .unwrap()
}
#[test]
fn daemon_sender_submits_system_sender_and_propagates_failures_without_retry() {
    for response in [
        Ok(json!({"ok":true,"result":{"message_id":"i1"}}).to_string()),
        Ok(json!({"ok":false,"error":"unknown mailbox"}).to_string()),
        Err(io::ErrorKind::ConnectionRefused),
        Err(io::ErrorKind::TimedOut),
    ] {
        let success = response.as_ref().is_ok_and(|r| r.contains("message_id"));
        let daemon = FakeDaemon {
            calls: RefCell::new(vec![]),
            response,
        };
        let mut send = daemon_sender(client(&daemon));
        let result = send("fake-vp", "stale", "heartbeat missing");
        assert_eq!(result.is_ok(), success);
        if let Err(ClientError::Dunk(error)) = result {
            assert_eq!(error.to_string(), "unknown mailbox");
        }
        let calls = daemon.calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["cmd"], "inbox_send");
        assert_eq!(calls[0]["self_pane_id"], Value::Null);
        assert_eq!(
            calls[0]["args"],
            json!({"to":"fake-vp","subject":"stale","body":"heartbeat missing","system_sender":"watchdog"})
        );
    }
}
#[test]
fn direct_write_makes_one_verified_unread_row_in_temporary_inbox() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("inbox.db");
    let id = direct_write(&path, "fake-vp", "stale", "watchdog.deadman: missing 🐑").unwrap();
    let inbox = crate::inbox_store::Store::open(&path, dir.path().join("attachments")).unwrap();
    let back = inbox.get_message(&id).unwrap();
    assert_eq!(back.summary.sender.as_deref(), Some("watchdog"));
    assert_eq!(back.summary.mailbox, "fake-vp");
    assert_eq!(back.body, "watchdog.deadman: missing 🐑");
    assert_eq!(back.summary.subject.as_deref(), Some("stale"));
    assert_eq!(back.read_at, None);
    assert_eq!(rows(&path), 1);
}
#[test]
fn failed_direct_write_then_repaired_store_produces_one_row() {
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("late-dir");
    fs::write(&parent, "blocker").unwrap();
    let path = parent.join("inbox.db");
    assert!(direct_write(&path, "fake-vp", "stale", "body").is_err());
    assert!(!path.exists());
    fs::remove_file(&parent).unwrap();
    direct_write(&path, "fake-vp", "stale", "body").unwrap();
    assert_eq!(rows(&path), 1);
    // This is helper retry evidence only: repeated incident suppression belongs to check/t1671.
}
#[test]
fn direct_write_detects_actual_persisted_row_corruption() {
    for field in ["sender", "mailbox", "body"] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("inbox.db");
        drop(crate::inbox_store::Store::open(&path, dir.path().join("attachments")).unwrap());
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.execute_batch(&format!("CREATE TRIGGER corrupt AFTER INSERT ON messages BEGIN UPDATE messages SET {field}='corrupt' WHERE id=NEW.id; END;")).unwrap();
        drop(connection);
        let error = direct_write(&path, "fake-vp", "stale", "body").unwrap_err();
        assert!(error.to_string().starts_with("direct write of "));
        assert!(error.to_string().ends_with(" did not read back intact"));
        assert_eq!(
            rows(&path),
            1,
            "failed verification must not pretend the insert rolled back"
        );
    }
}
