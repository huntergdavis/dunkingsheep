//! Private synthetic Claude ancestry; no live hook or daemon.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn subagent_does_not_end_and_concurrent_stops_consume_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let cli = root.join("fake-cli");
    fs::write(&cli, "#!/bin/sh\ncase \"$3\" in start) echo '{\"turn_id\":\"token-1\",\"pane_id\":\"%scratch\"}' ;; end) echo \"$5\" >> \"$CALLS\"; echo '{\"ended\":true}' ;; esac\n").unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    for (name, event) in [
        ("start", "UserPromptSubmit"),
        ("sub", "SubagentStop"),
        ("stop", "Stop"),
    ] {
        fs::write(root.join(name), format!("{{\"session_id\":\"session-1\",\"hook_event_name\":\"{event}\",\"stop_hook_active\":false,\"cwd\":\"{}\"}}", root.display())).unwrap();
    }
    let script = "exec -a claude /bin/bash -c '\"$HOOK\" start < \"$ROOT/start\"; \"$HOOK\" end < \"$ROOT/sub\"; test ! -e \"$CALLS\" || exit 1; \"$HOOK\" end < \"$ROOT/stop\" & \"$HOOK\" end < \"$ROOT/stop\" & wait'";
    let status = Command::new("/bin/bash")
        .args(["-c", script])
        .env("HOOK", env!("CARGO_BIN_EXE_dunk-claude-turn"))
        .env("ROOT", root)
        .env("DUNKINGSHEEP_BIN", &cli)
        .env("DUNKINGSHEEP_CLAUDE_STATE_DIR", root.join("state"))
        .env("DUNKINGSHEEP_CLAUDE_END_WAIT_S", "0")
        .env("CALLS", root.join("calls"))
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(fs::read_to_string(root.join("calls")).unwrap(), "token-1\n");
}
