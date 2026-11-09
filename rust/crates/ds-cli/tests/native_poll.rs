//! Socket-level coverage of the release-native poll client; no real daemon or credentials.
use std::{io::{BufRead, BufReader, Write}, os::unix::net::UnixListener, process::Command, thread};
use serde_json::{json, Value};

#[test]
fn poll_sends_one_batched_read_with_explicit_options() {
    let dir=tempfile::tempdir().unwrap();
    let path=dir.path().join("socket");
    let listener=UnixListener::bind(&path).unwrap();
    let server=thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut line=String::new();
        BufReader::new(stream.try_clone().unwrap()).read_line(&mut line).unwrap();
        let request:Value=serde_json::from_str(&line).unwrap();
        writeln!(stream,"{}",json!({"ok":true,"result":{"status":{"version":"test"},"inbox_count":{"unread":2}}})).unwrap();
        request
    });
    let output=Command::new(env!("CARGO_BIN_EXE_dunk-poll"))
        .args(["--socket",path.to_str().unwrap(),"--self-pane","%77","--mailbox","desk","--limit","3","--tasks","--panes","--task-scope","self","--include-done","--check-herdr","--compact"])
        .env_remove("DUNKINGSHEEP_SOCKET").output().unwrap();
    assert!(output.status.success(),"{}",String::from_utf8_lossy(&output.stderr));
    let response:Value=serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(response["inbox_count"]["unread"],2);
    assert_eq!(server.join().unwrap(),json!({"id":1,"cmd":"poll","self_pane_id":"%77","args":{"mailbox":"desk","limit":3,"panes":true,"tasks":true,"task_scope":"self","include_done":true,"check_herdr":true}}));
}
