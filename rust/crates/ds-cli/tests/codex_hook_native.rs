//! Isolated Codex ancestry and concurrent-stop smoke; no daemon or live hooks.
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn concurrent_stops_consume_one_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let cli = path.join("fake-cli");
    fs::write(&cli, "#!/bin/sh\ncase \"$3\" in start) echo '{\"turn_id\":\"token-1\",\"pane_id\":\"%scratch\"}' ;; end) echo \"$5\" >> \"$CALLS\"; echo '{\"ended\":true}' ;; esac\n").unwrap();
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
    assert_eq!(fs::read_to_string(path.join("calls")).unwrap(), "token-1\n");
}

#[test]
fn python_flock_blocks_native_producer() {
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let state = path.join("state");
    fs::create_dir(&state).unwrap();
    let cli = path.join("fake-cli");
    fs::write(&cli, "#!/bin/sh\necho invoked > \"$CALLS\"\necho '{\"turn_id\":\"token-1\",\"pane_id\":\"%scratch\"}'\n").unwrap();
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
    let python = "import fcntl,os,time\nf=open(os.environ['LOCK'],'a+'); fcntl.flock(f,fcntl.LOCK_EX)\nopen(os.environ['READY'],'w').close()\ndeadline=time.monotonic()+10\nwhile not os.path.exists(os.environ['RELEASE']) and time.monotonic()<deadline: time.sleep(.01)\n";
    let mut holder = Command::new("python3")
        .args(["-c", python])
        .env("LOCK", state.join("session-1.lock"))
        .env("READY", path.join("ready"))
        .env("RELEASE", path.join("release"))
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !path.join("ready").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    if !path.join("ready").exists() {
        let _ = holder.kill();
        let _ = holder.wait();
        panic!("Python lock holder did not start");
    }
    let mut hook = Command::new("/bin/bash")
        .args([
            "-c",
            "exec -a codex /bin/bash -c '\"$HOOK\" start < \"$EVENT\"; :'",
        ])
        .env("HOOK", env!("CARGO_BIN_EXE_dunk-codex-turn"))
        .env("EVENT", &event)
        .env("DUNKINGSHEEP_BIN", &cli)
        .env("DUNKINGSHEEP_CODEX_STATE_DIR", &state)
        .env("CALLS", path.join("calls"))
        .spawn()
        .unwrap();
    // /proc/locks reports a queued flock with an arrow and the blocked hook PID.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut blocked = false;
    while Instant::now() < deadline {
        let children = fs::read_to_string(format!("/proc/{0}/task/{0}/children", hook.id()))
            .unwrap_or_default();
        let locks = fs::read_to_string("/proc/locks").unwrap_or_default();
        blocked = children.split_whitespace().any(|pid| {
            locks.lines().any(|line| {
                line.contains("->") && line.split_whitespace().any(|field| field == pid)
            })
        });
        if blocked || path.join("calls").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    fs::write(path.join("release"), "").unwrap();
    assert!(holder.wait().unwrap().success());
    assert!(hook.wait().unwrap().success());
    assert!(blocked, "native hook ignored Python flock");
    assert!(path.join("calls").exists());
}
