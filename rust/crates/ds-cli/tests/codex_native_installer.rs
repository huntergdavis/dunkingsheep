//! Fresh-home installer smoke; never touches user configuration.
use std::fs;
use std::process::Command;

#[test]
fn installs_into_private_home_and_refuses_existing_trust() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("codex user's home");
    fs::create_dir(&home).unwrap();
    fs::write(home.join("auth-sentinel"), b"preserve").unwrap();
    let installer = env!("CARGO_BIN_EXE_dunk-codex-install");
    let binary = env!("CARGO_BIN_EXE_dunk-codex-turn");
    let run = || {
        Command::new(installer)
            .arg(&home)
            .arg(binary)
            .output()
            .unwrap()
    };
    let first = run();
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let before = fs::read(home.join("hooks.json")).unwrap();
    let doc: serde_json::Value = serde_json::from_slice(&before).unwrap();
    for (event, action) in [("UserPromptSubmit", "start"), ("Stop", "end")] {
        let h = &doc["hooks"][event][0]["hooks"][0];
        assert_eq!(h["timeout"], 15);
        assert!(h["command"].as_str().unwrap().ends_with(action));
        assert!(h["command"].as_str().unwrap().contains("'\\''"));
    }
    assert_eq!(fs::read(home.join("auth-sentinel")).unwrap(), b"preserve");
    assert!(!run().status.success());
    assert_eq!(fs::read(home.join("hooks.json")).unwrap(), before);
}

#[test]
fn existing_hooks_prevent_any_write() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    fs::write(home.join("hooks.json"), b"{\"existing\":true}\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_dunk-codex-install"))
        .arg(home)
        .arg(env!("CARGO_BIN_EXE_dunk-codex-turn"))
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert_eq!(
        fs::read(home.join("hooks.json")).unwrap(),
        b"{\"existing\":true}\n"
    );
    assert!(!home.join("dunk-codex-turn").exists());
    assert!(!home.join("dunkingsheep-turn.sh").exists());
}
