//! Isolated Codex ancestry and concurrent-stop smoke; no daemon or live hooks.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn concurrent_stops_consume_one_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let cli = path.join("fake-cli");
    fs::write(&cli, "#!/bin/sh\ncase \"$3\" in start) echo '{\"turn_id\":\"token-1\",\"pane_id\":\"%scratch\"}' ;; end) echo end >> \"$CALLS\"; echo '{\"ended\":true}' ;; esac\n").unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    let event = path.join("event.json");
    fs::write(
        &event,
        format!(
            "{{\"session_id\":\"session-1\",\"turn_id\":\"turn-1\",\"cwd\":\"{}\"}}",
            path.display()
        ),
    )
    .unwrap();
    let script = "exec -a codex /bin/bash -c '\"$HOOK\" start < \"$EVENT\"; \"$HOOK\" end < \"$EVENT\" & \"$HOOK\" end < \"$EVENT\" & wait'";
    let status = Command::new("/bin/bash")
        .args(["-c", script])
        .env("HOOK", env!("CARGO_BIN_EXE_dunk-codex-turn"))
        .env("EVENT", &event)
        .env("DUNKINGSHEEP_BIN", &cli)
        .env("DUNKINGSHEEP_CODEX_STATE_DIR", path.join("state"))
        .env("DUNKINGSHEEP_CODEX_END_WAIT_S", "0")
        .env("CALLS", path.join("calls"))
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(fs::read_to_string(path.join("calls")).unwrap(), "end\n");
}
