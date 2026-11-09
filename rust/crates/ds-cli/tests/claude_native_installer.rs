//! Fresh-home installer smoke; never touches user configuration.
use std::fs;
use std::process::Command;

#[test]
fn installs_into_private_home_and_refuses_existing_trust() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("claude user's home");
    fs::create_dir(&home).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(home.join("auth-sentinel"), b"preserve").unwrap();
    let installer = env!("CARGO_BIN_EXE_dunk-claude-install");
    let binary = dir.path().join("fake-producer");
    fs::write(
        &binary,
        b"#!/bin/sh\nprintf called > \"$INSTALL_TEST_CALLED\"\n",
    )
    .unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
    let run = || {
        Command::new(installer)
            .arg(&home)
            .arg(&binary)
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let before = fs::read(home.join("settings.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&before).unwrap();
    for (event, action) in [
        ("UserPromptSubmit", "start"),
        ("Stop", "end"),
        ("SubagentStop", "noop"),
    ] {
        let h = &doc["hooks"][event][0]["hooks"][0];
        assert_eq!(h["timeout"], 15);
        assert!(h["command"].as_str().unwrap().ends_with(action));
        assert!(h["command"].as_str().unwrap().contains("'\\''"));
    }
    let command = doc["hooks"]["UserPromptSubmit"][0]["hooks"][0]["command"]
        .as_str()
        .unwrap();
    let called = dir.path().join("called");
    let shell = Command::new("/bin/sh")
        .args(["-c", command])
        .env("INSTALL_TEST_CALLED", &called)
        .status()
        .unwrap();
    assert!(shell.success());
    assert_eq!(fs::read(&called).unwrap(), b"called");
    assert_eq!(fs::read(home.join("auth-sentinel")).unwrap(), b"preserve");
    assert!(!run().status.success());
    assert_eq!(fs::read(home.join("settings.json")).unwrap(), before);
}

#[test]
fn existing_hooks_prevent_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    fs::write(home.join("settings.json"), b"{\"existing\":true}\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_dunk-claude-install"))
        .arg(home)
        .arg(env!("CARGO_BIN_EXE_dunk-claude-turn"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        fs::read(home.join("settings.json")).unwrap(),
        b"{\"existing\":true}\n"
    );
    assert!(!home.join("dunk-claude-turn").exists());
    assert!(!home.join("dunkingsheep-turn.sh").exists());
}
