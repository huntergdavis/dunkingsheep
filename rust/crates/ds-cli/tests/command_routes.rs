//! Generic `--json` daemon output through the actual Rust CLI process and an isolated fake socket.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::process::{Command, Output};

use serde_json::{json, Value};

fn ordered_result() -> Value {
    let mut nested = serde_json::Map::new();
    nested.insert("tiny".into(), json!(1e-7));
    nested.insert("label".into(), json!("café😀"));
    let mut result = serde_json::Map::new();
    result.insert("zeta".into(), Value::Object(nested));
    result.insert("alpha".into(), json!("second"));
    Value::Object(result)
}

fn run_cli(socket: &str, command: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_dunkingsheep"))
        .args(["--socket", socket, "--self-pane", "%99"])
        .args(command)
        .env_remove("DUNKINGSHEEP_SOCKET")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
        .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
        .output()
        .expect("run Rust CLI")
}

#[test]
fn cli_generic_json_preserves_daemon_insertion_order() {
    let temp = tempfile::tempdir().unwrap();
    let socket = temp.path().join("daemon.sock");
    let socket_text = socket.display().to_string();
    let listener = UnixListener::bind(&socket).unwrap();
    let daemon = std::thread::spawn(move || {
        // Each CLI command probes with status, then makes its requested call.
        for _ in 0..4 {
            let (stream, _) = listener.accept().unwrap();
            let mut line = String::new();
            BufReader::new(&stream).read_line(&mut line).unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            let result = if request["cmd"] == "status" {
                json!({})
            } else {
                ordered_result()
            };
            let response = json!({"ok": true, "result": result});
            let mut writer = &stream;
            writer
                .write_all(format!("{response}\n").as_bytes())
                .unwrap();
        }
    });

    let panes = run_cli(&socket_text, &["panes", "--json"]);
    assert!(
        panes.status.success(),
        "{}",
        String::from_utf8_lossy(&panes.stderr)
    );
    assert_eq!(
        "{\n  \"zeta\": {\n    \"tiny\": 1e-07,\n    \"label\": \"caf\\u00e9\\ud83d\\ude00\"\n  },\n  \"alpha\": \"second\"\n}\n",
        String::from_utf8(panes.stdout).unwrap()
    );

    let token = run_cli(&socket_text, &["gh-token", "--json"]);
    assert!(
        token.status.success(),
        "{}",
        String::from_utf8_lossy(&token.stderr)
    );
    assert_eq!(
        "{\n  \"zeta\": {\n    \"tiny\": 1e-07,\n    \"label\": \"caf\\u00e9\\ud83d\\ude00\"\n  },\n  \"alpha\": \"second\"\n}\n",
        String::from_utf8(token.stdout).unwrap()
    );
    daemon.join().unwrap();
}
