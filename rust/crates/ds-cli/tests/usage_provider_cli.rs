use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn native_provider_output_and_usage_runner_contract() {
    let dir = tempfile::tempdir().unwrap();
    let bin = env!("CARGO_BIN_EXE_dunkingsheep");
    for provider in ["claude", "codex", "pi"] {
        let out = Command::new(bin)
            .args(["usage-provider", provider])
            .env("HOME", dir.path())
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(1));
        let value: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(value["provider"], provider);
        assert_eq!(value["status"], "error");
    }
    let sessions = dir.path().join(".codex/sessions");
    fs::create_dir_all(&sessions).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    fs::write(sessions.join("fixture.jsonl"), format!(r#"{{"timestamp":{},"payload":{{"rate_limits":{{"primary":{{"used_percent":40.0,"window_minutes":10080}}}}}}}}"#, now)).unwrap();
    let out = Command::new(bin)
        .args(["usage-provider", "codex", "--max-age-s", "3600"])
        .env("HOME", dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["status"], "ok");
    assert_eq!(value["windows"][0]["percent_remaining"], 60.0);
    let opencode = dir.path().join("opencode");
    fs::write(
        &opencode,
        "#!/bin/sh\nprintf 'Sessions 3\\nMessages 7\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&opencode, fs::Permissions::from_mode(0o700)).unwrap();
    let out = Command::new(bin)
        .args(["usage-provider", "opencode"])
        .env("HOME", dir.path())
        .env("PATH", dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["provider"], "opencode-go");
    assert_eq!(value["status"], "unknown");
    assert!(value["reason"]
        .as_str()
        .unwrap()
        .contains("not account quota"));
    let config = dir.path().join("usage.json");
    fs::write(&config, format!(r#"{{"schema":1,"providers":{{"codex":{{"command":"{} usage-provider codex","timeout_s":5,"parse":{{"windows":"windows","meaning":"meaning","measured_at":"measured_at"}}}},"opencode-go":{{"command":"{} usage-provider opencode","timeout_s":5,"parse":{{"meaning":"meaning","measured_at":"measured_at"}}}}}}}}"#, bin, bin)).unwrap();
    let out = Command::new(bin)
        .args(["--json", "usage", "--config"])
        .arg(&config)
        .env("HOME", dir.path())
        .env("PATH", dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0));
    let readings: Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = readings.as_array().unwrap();
    let codex = rows.iter().find(|v| v["provider"] == "codex").unwrap();
    let opencode = rows
        .iter()
        .find(|v| v["provider"] == "opencode-go")
        .unwrap();
    assert_eq!(codex["status"], "ok");
    assert_eq!(codex["windows"][0]["percent_remaining"], 60.0);
    assert_eq!(opencode["status"], "unknown");
    assert_eq!(opencode["exit_code"], 1);
    let stale = Command::new(bin)
        .args(["usage-provider", "codex"])
        .env("HOME", dir.path())
        .env("DUNKINGSHEEP_USAGE_MAX_AGE_S", "-1")
        .output()
        .unwrap();
    assert_eq!(stale.status.code(), Some(1));
    let fresh = Command::new(bin)
        .args(["usage-provider", "codex", "--max-age-s", "3600"])
        .env("HOME", dir.path())
        .env("DUNKINGSHEEP_USAGE_MAX_AGE_S", "-1")
        .output()
        .unwrap();
    assert_eq!(fresh.status.code(), Some(0));
    fs::remove_dir_all(sessions).unwrap();
    let config = dir.path().join("usage.json");
    fs::write(&config, format!(r#"{{"schema":1,"providers":{{"codex":{{"command":"{} usage-provider codex","timeout_s":5}}}}}}"#, bin)).unwrap();
    let out = Command::new(bin)
        .args(["--json", "usage", "codex", "--config"])
        .arg(config)
        .env("HOME", dir.path())
        .output()
        .unwrap();
    let value: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(value[0]["provider"], "codex");
    assert_eq!(value[0]["status"], "error");
}
