//! Ported from tests/test_mcp_stdout_binary.py: the server must start through the real entry
//! point and answer `initialize` on stdout (no daemon is needed for it).

use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn test_initialize_roundtrip_through_cli() {
    let request = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2024-11-05", "capabilities": {},
                   "clientInfo": {"name": "t", "version": "0"}}})
    .to_string()
        + "\n";
    let mut child = Command::new(env!("CARGO_BIN_EXE_dunkingsheep-mcp"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dunkingsheep-mcp");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    let stdout = String::from_utf8(output.stdout).unwrap();
    let reply: serde_json::Value = serde_json::from_str(stdout.lines().next().unwrap()).unwrap();
    assert_eq!("dunkingsheep", reply["result"]["serverInfo"]["name"]);
}
