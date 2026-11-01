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
fn cli_route_56_mcp_setup() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let bin = temp.path().join("bin");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin).unwrap();
    let capture = temp.path().join("claude-argv");
    let claude = bin.join("claude");
    std::fs::write(
        &claude,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$CAPTURE\"\nexit 7\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&claude, std::fs::Permissions::from_mode(0o755)).unwrap();
    let original_path = std::env::var("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin.clone()).chain(std::env::split_paths(&original_path)),
    )
    .unwrap();
    let executable = std::fs::canonicalize(env!("CARGO_BIN_EXE_dunkingsheep"))
        .unwrap()
        .display()
        .to_string();
    let run = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_dunkingsheep"))
            .args(args)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join("config"))
            .env("DUNKINGSHEEP_DIR", home.join("dunkingsheep"))
            .env("PATH", &path)
            .env("CAPTURE", &capture)
            .env_remove("DUNKINGSHEEP_SOCKET")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN")
            .env_remove("DUNKINGSHEEP_SERVICE_TOKEN_FILE")
            .output()
            .unwrap()
    };

    let json = run(&["mcp-setup", "json"]);
    assert_eq!(0, json.status.code().unwrap());
    assert_eq!(
        format!(
            "{{\n  \"mcpServers\": {{\n    \"dunkingsheep\": {{\n      \"command\": \"{executable}\",\n      \"args\": [\"mcp\"]\n    }}\n  }}\n}}\n\n"
        ),
        String::from_utf8(json.stdout).unwrap()
    );
    assert!(json.stderr.is_empty());

    let apply = run(&["mcp-setup", "claude", "--scope", "project", "--apply"]);
    assert_eq!(7, apply.status.code().unwrap());
    assert_eq!(
        format!(
            "# Claude Code\nclaude mcp add --scope project dunkingsheep -- {executable} mcp\nclaude mcp list   # expect: dunkingsheep: {executable} mcp - Connected\n\n$ claude mcp add --scope project dunkingsheep -- {executable} mcp\n"
        ),
        String::from_utf8(apply.stdout).unwrap()
    );
    assert!(apply.stderr.is_empty());
    assert_eq!(
        format!("mcp\nadd\n--scope\nproject\ndunkingsheep\n--\n{executable}\nmcp\n"),
        std::fs::read_to_string(capture).unwrap()
    );

    let cursor = run(&["mcp-setup", "cursor", "--apply"]);
    assert_eq!(2, cursor.status.code().unwrap());
    assert!(String::from_utf8(cursor.stdout)
        .unwrap()
        .contains("\"args\": [\"mcp\"]"));
    assert_eq!(
        "error: cursor has no CLI to apply; paste the block above\n",
        String::from_utf8(cursor.stderr).unwrap()
    );
    assert!(!home.join("config").exists());
    assert!(!home.join("dunkingsheep").exists());
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
