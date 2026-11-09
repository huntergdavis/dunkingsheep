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

#[test]
fn python_flock_blocks_native_claude() {
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
            "{{\"session_id\":\"session-1\",\"hook_event_name\":\"UserPromptSubmit\",\"cwd\":\"{}\"}}",
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
            "exec -a claude /bin/bash -c '\"$HOOK\" start < \"$EVENT\"; :'",
        ])
        .env("HOOK", env!("CARGO_BIN_EXE_dunk-claude-turn"))
        .env("EVENT", &event)
        .env("DUNKINGSHEEP_BIN", &cli)
        .env("DUNKINGSHEEP_CLAUDE_STATE_DIR", &state)
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

#[test]
fn consumes_existing_python_format_state() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let cli = root.join("fake-cli");
    fs::write(
        &cli,
        "#!/bin/sh\necho \"$5\" >> \"$CALLS\"\necho '{\"ended\":true}'\n",
    )
    .unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    fs::create_dir(root.join("state")).unwrap();
    let event = root.join("stop");
    fs::write(&event, format!("{{\"session_id\":\"session-1\",\"hook_event_name\":\"Stop\",\"stop_hook_active\":false,\"cwd\":\"{}\"}}", root.display())).unwrap();
    let python = root.join("seed.py");
    fs::write(&python, "import json,os,time\npid=os.getppid()\ntail=open('/proc/%s/stat'%pid).read().rsplit(')',1)[1].split()\ns={'daemon_turn_id':'python-token','session_id':'session-1','cwd':os.environ['ROOT'],'pane_id':'%scratch','root_pid':pid,'root_start':tail[19],'started_at':time.time(),'expires_at':time.time()+120}\nwith open(os.path.join(os.environ['ROOT'],'state','session-1.json'),'w') as f: json.dump(s,f)\n").unwrap();
    let status = Command::new("/bin/bash").args(["-c", "exec -a claude /bin/bash -c 'python3 \"$ROOT/seed.py\"; \"$HOOK\" end < \"$ROOT/stop\"; :' "])
        .env("ROOT", root).env("HOOK", env!("CARGO_BIN_EXE_dunk-claude-turn"))
        .env("DUNKINGSHEEP_BIN", &cli).env("DUNKINGSHEEP_CLAUDE_STATE_DIR", root.join("state"))
        .env("CALLS", root.join("calls")).status().unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read_to_string(root.join("calls")).unwrap(),
        "python-token\n"
    );
    assert!(!root.join("state/session-1.json").exists());
}

#[test]
fn early_stop_waits_for_start_and_next_turn_still_completes() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let cli = root.join("fake-cli");
    fs::write(&cli, "#!/bin/sh\ncase \"$3\" in start) if [ -e \"$ROOT/count\" ]; then token=token-2; else token=token-1; : > \"$ROOT/count\"; fi; printf '{\"turn_id\":\"%s\",\"pane_id\":\"%%scratch\"}\\n' \"$token\" ;; end) echo \"$5\" >> \"$CALLS\"; echo '{\"ended\":true}' ;; esac\n").unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    for (name, event) in [("start", "UserPromptSubmit"), ("stop", "Stop")] {
        fs::write(root.join(name), format!("{{\"session_id\":\"session-1\",\"hook_event_name\":\"{event}\",\"stop_hook_active\":false,\"cwd\":\"{}\"}}", root.display())).unwrap();
    }
    let script = "exec -a claude /bin/bash -c '\"$HOOK\" end < \"$ROOT/stop\" & first=$!; sleep .1; \"$HOOK\" start < \"$ROOT/start\"; wait $first; \"$HOOK\" start < \"$ROOT/start\"; \"$HOOK\" end < \"$ROOT/stop\"; :'";
    let status = Command::new("/bin/bash")
        .args(["-c", script])
        .env("HOOK", env!("CARGO_BIN_EXE_dunk-claude-turn"))
        .env("ROOT", root)
        .env("DUNKINGSHEEP_BIN", &cli)
        .env("DUNKINGSHEEP_CLAUDE_STATE_DIR", root.join("state"))
        .env_remove("DUNKINGSHEEP_CLAUDE_END_WAIT_S")
        .env("CALLS", root.join("calls"))
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        fs::read_to_string(root.join("calls")).unwrap(),
        "token-1\ntoken-2\n"
    );
}
