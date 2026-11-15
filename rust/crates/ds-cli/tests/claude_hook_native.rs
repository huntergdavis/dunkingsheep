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

/// Opt-in output is private fixture evidence, not installed/model consumption.
#[test]
fn mail_hook_context_single_continuation_and_refusal_are_safe() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let cli = root.join("fake-cli");
    fs::write(&cli, r#"#!/bin/sh
echo "$*" >> "$ROOT/calls"
case "$3" in
start) echo '{"turn_id":"token-1","pane_id":"%scratch","mail_notice":{"action":"offer","text":"read private inbox"}}' ;;
end) if [ -e "$ROOT/refuse" ]; then echo '{"ended":false,"reason":"turn_changed","mail_notice":{"action":"offer","text":"must not print"}}'; elif [ "$7" = stop ]; then echo '{"ended":false,"reason":"mail_notice","mail_notice":{"action":"offer","text":"read private inbox"}}'; else echo '{"ended":true,"mail_notice":{"action":"offer","text":"must not chain"}}'; fi ;;
*) exit 1 ;;
esac
"#).unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    for (name, event, active, extra) in [
        ("start", "UserPromptSubmit", "false", ""),
        ("stop", "Stop", "false", ""),
        ("active", "Stop", "true", ""),
        ("sub", "SubagentStop", "false", ""),
        ("agent", "Stop", "false", ",\"agent_id\":\"child\""),
        ("bad", "Stop", "\"false\"", ""),
    ] {
        fs::write(root.join(name),format!("{{\"session_id\":\"session-1\",\"cwd\":\"{}\",\"hook_event_name\":\"{event}\",\"stop_hook_active\":{active}{extra}}}",root.display())).unwrap();
    }
    let script = r#"exec -a claude /bin/bash -c '
"$HOOK" start < "$ROOT/start" > "$ROOT/context"
"$HOOK" start < "$ROOT/start" > "$ROOT/duplicate-start"
for ev in sub agent bad; do "$HOOK" end < "$ROOT/$ev" >> "$ROOT/ignored"; done
: > "$ROOT/refuse"
"$HOOK" end < "$ROOT/stop" > "$ROOT/refusal"
rm "$ROOT/refuse"
"$HOOK" end < "$ROOT/stop" > "$ROOT/feedback"
"$HOOK" end < "$ROOT/stop" > "$ROOT/replay"
test -e "$ROOT/state/session-1.json" || exit 1
"$HOOK" end < "$ROOT/active" > "$ROOT/final"
"$HOOK" end < "$ROOT/active" > "$ROOT/duplicate-final"
'"#;
    let output = Command::new("/bin/bash")
        .args(["-c", script])
        .env("ROOT", root)
        .env("HOOK", env!("CARGO_BIN_EXE_dunk-claude-turn"))
        .env("DUNKINGSHEEP_BIN", cli)
        .env("DUNKINGSHEEP_CLAUDE_STATE_DIR", root.join("state"))
        .env("DUNKINGSHEEP_CLAUDE_MAIL_HOOK", "1")
        .env("DUNKINGSHEEP_CLAUDE_END_WAIT_S", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let context: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("context")).unwrap()).unwrap();
    assert_eq!(
        context["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    assert_eq!(
        context["hookSpecificOutput"]["additionalContext"],
        "read private inbox"
    );
    let feedback: serde_json::Value =
        serde_json::from_slice(&fs::read(root.join("feedback")).unwrap()).unwrap();
    assert_eq!(
        feedback,
        serde_json::json!({"decision":"block","reason":"read private inbox"})
    );
    for file in [
        "duplicate-start",
        "ignored",
        "refusal",
        "replay",
        "final",
        "duplicate-final",
    ] {
        assert!(
            fs::read(root.join(file)).unwrap().is_empty(),
            "unexpected output: {file}"
        );
    }
    assert!(!root.join("state/session-1.json").exists());
    let calls = fs::read_to_string(root.join("calls")).unwrap();
    assert_eq!(calls.lines().count(), 4);
    assert!(calls.contains("--source claude-hook-mail-v1"));
    assert_eq!(calls.matches("--reason stop-continuation").count(), 1);
    assert!(!calls.contains("send"));
}

#[test]
fn killed_after_marker_before_stdout_cannot_repeat_continuation() {
    use std::process::Stdio;
    use std::time::{Duration, Instant};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let cli = root.join("fake-cli");
    fs::write(&cli,r#"#!/bin/sh
echo "$*" >> "$ROOT/calls"
case "$3" in
start) echo '{"turn_id":"token-1","pane_id":"%scratch"}' ;;
end) if [ "$7" = stop ]; then echo '{"ended":false,"reason":"mail_notice","mail_notice":{"action":"offer","text":"read private inbox"}}'; else echo '{"ended":true}'; fi ;;
esac
"#).unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    for (name, event, active) in [
        ("start", "UserPromptSubmit", false),
        ("stop", "Stop", false),
        ("active", "Stop", true),
    ] {
        fs::write(root.join(name),format!("{{\"session_id\":\"session-1\",\"cwd\":\"{}\",\"hook_event_name\":\"{event}\",\"stop_hook_active\":{active}}}",root.display())).unwrap();
    }
    // Fill the private stdout pipe before exec. The real hook can persist its
    // marker, but its small JSON write cannot complete until the parent drains.
    fs::write(root.join("fill.py"),"import os,fcntl\nfcntl.fcntl(1,fcntl.F_SETPIPE_SZ,4096)\nos.write(1,b'x'*4096)\nos.execv(os.environ['HOOK'],[os.environ['HOOK'],'end'])\n").unwrap();
    let script = r#"exec -a claude /bin/bash -c '
"$HOOK" start < "$ROOT/start" > "$ROOT/context"
python3 "$ROOT/fill.py" < "$ROOT/stop" & child=$!
echo "$child" > "$ROOT/child"
wait "$child"
"$HOOK" end < "$ROOT/stop" > "$ROOT/replay"
"$HOOK" end < "$ROOT/active" > "$ROOT/final"
'"#;
    let mut process = Command::new("/bin/bash")
        .args(["-c", script])
        .env("ROOT", root)
        .env("HOOK", env!("CARGO_BIN_EXE_dunk-claude-turn"))
        .env("DUNKINGSHEEP_BIN", cli)
        .env("DUNKINGSHEEP_CLAUDE_STATE_DIR", root.join("state"))
        .env("DUNKINGSHEEP_CLAUDE_MAIL_HOOK", "1")
        .env("DUNKINGSHEEP_CLAUDE_END_WAIT_S", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut marked = false;
    while Instant::now() < deadline {
        let state = fs::read(root.join("state/session-1.json"))
            .ok()
            .and_then(|s| serde_json::from_slice::<serde_json::Value>(&s).ok());
        if state.is_some_and(|s| s["mail_continuing"] == true) {
            marked = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if let Ok(pid) = fs::read_to_string(root.join("child")) {
        assert!(Command::new("kill")
            .args(["-KILL", pid.trim()])
            .status()
            .unwrap()
            .success());
    } else {
        let _ = process.kill();
    }
    let output = process.wait_with_output().unwrap();
    assert!(marked, "hook never persisted continuation marker");
    assert!(output.status.success());
    assert_eq!(
        output.stdout,
        vec![b'x'; 4096],
        "hook JSON must not have reached stdout"
    );
    assert!(fs::read(root.join("replay")).unwrap().is_empty());
    assert!(fs::read(root.join("final")).unwrap().is_empty());
    assert!(!root.join("state/session-1.json").exists());
    let calls = fs::read_to_string(root.join("calls")).unwrap();
    assert_eq!(calls.lines().count(), 3);
    assert_eq!(calls.matches("--reason stop\n").count(), 1);
    assert_eq!(calls.matches("--reason stop-continuation").count(), 1);
}

#[test]
fn legacy_default_discards_offer_and_preserves_source() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let cli = root.join("fake-cli");
    fs::write(&cli,r#"#!/bin/sh
echo "$*" >> "$ROOT/calls"
case "$3" in
start) echo '{"turn_id":"token-1","pane_id":"%scratch","mail_notice":{"action":"offer","text":"ignore"}}' ;;
end) echo '{"ended":true,"mail_notice":{"action":"offer","text":"ignore"}}' ;;
esac
"#).unwrap();
    fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
    for (name, event) in [("start", "UserPromptSubmit"), ("stop", "Stop")] {
        fs::write(root.join(name),format!("{{\"session_id\":\"session-1\",\"cwd\":\"{}\",\"hook_event_name\":\"{event}\",\"stop_hook_active\":false}}",root.display())).unwrap();
    }
    let output = Command::new("/bin/bash")
        .args(["-c",r#"exec -a claude /bin/bash -c '"$HOOK" start < "$ROOT/start"; "$HOOK" end < "$ROOT/stop"; :'"#])
        .env("ROOT",root).env("HOOK",env!("CARGO_BIN_EXE_dunk-claude-turn"))
        .env("DUNKINGSHEEP_BIN",cli).env("DUNKINGSHEEP_CLAUDE_STATE_DIR",root.join("state"))
        .env_remove("DUNKINGSHEEP_CLAUDE_MAIL_HOOK").output().unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty());
    let calls = fs::read_to_string(root.join("calls")).unwrap();
    assert_eq!(calls.lines().count(), 2);
    assert!(calls.contains("--source claude-hook --ttl-s"));
    assert!(!calls.contains("mail-v1"));
}

#[test]
fn terminal_refusals_after_restart_allow_next_prompt_to_declare_busy() {
    for (reason, terminal) in [
        ("no_active_turn", true),
        ("already_ended", true),
        ("expired_turn", true),
        ("turn_mismatch", true),
        ("mail_continuation_active", false),
        ("turn_changed", false),
        ("foreign_turn", true),
        ("transport", false),
        ("malformed", true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cli = root.join("fake-cli");
        fs::write(
            &cli,
            r#"#!/bin/sh
echo "$*" >> "$ROOT/calls"
case "$3" in
start)
    n=0
    if [ -e "$ROOT/count" ]; then n=$(cat "$ROOT/count"); fi
    n=$((n+1))
    echo "$n" > "$ROOT/count"
    echo "token-$n" > "$ROOT/daemon-generation"
    printf '{"turn_id":"token-%s","pane_id":"%%scratch"}\n' "$n"
    ;;
end)
    if [ ! -e "$ROOT/daemon-generation" ]; then
        echo '{"ended":false,"reason":"no_active_turn"}'
        exit 0
    fi
    case "$REFUSAL" in
    transport) exit 9 ;;
    malformed) echo '{}' ;;
    *) printf '{"ended":false,"reason":"%s"}\n' "$REFUSAL" ;;
    esac
    ;;
esac
"#,
        )
        .unwrap();
        fs::set_permissions(&cli, fs::Permissions::from_mode(0o700)).unwrap();
        for (name, event) in [("start", "UserPromptSubmit"), ("stop", "Stop")] {
            fs::write(root.join(name),format!("{{\"session_id\":\"session-1\",\"cwd\":\"{}\",\"hook_event_name\":\"{event}\",\"stop_hook_active\":false}}",root.display())).unwrap();
        }
        let script = r#"exec -a claude /bin/bash -c '
"$HOOK" start < "$ROOT/start"
# Restart forgets daemon memory while preserving the native token on disk.
if [ "$REFUSAL" = no_active_turn ]; then rm "$ROOT/daemon-generation"; fi
"$HOOK" end < "$ROOT/stop"
if [ -e "$ROOT/state/session-1.json" ]; then echo yes; else echo no; fi > "$ROOT/retained"
# Another restart between Stop and the next prompt still needs a fresh generation.
if [ "$TERMINAL" = 1 ]; then rm -f "$ROOT/daemon-generation"; fi
"$HOOK" start < "$ROOT/start"
if [ "$REFUSAL" = no_active_turn ]; then rm "$ROOT/daemon-generation"; fi
"$HOOK" end < "$ROOT/stop"
"$HOOK" start < "$ROOT/start"
'"#;
        let output = Command::new("/bin/bash")
            .args(["-c", script])
            .env("ROOT", root)
            .env("REFUSAL", reason)
            .env("TERMINAL", if terminal { "1" } else { "0" })
            .env("HOOK", env!("CARGO_BIN_EXE_dunk-claude-turn"))
            .env("DUNKINGSHEEP_BIN", cli)
            .env("DUNKINGSHEEP_CLAUDE_STATE_DIR", root.join("state"))
            .env("DUNKINGSHEEP_CLAUDE_MAIL_HOOK", "1")
            .env("DUNKINGSHEEP_CLAUDE_END_WAIT_S", "0")
            .output()
            .unwrap();
        assert!(output.status.success(), "{reason}");
        assert!(output.stdout.is_empty(), "{reason} cannot emit feedback");
        let calls = fs::read_to_string(root.join("calls")).unwrap();
        let starts = calls.lines().filter(|s| s.contains("turn start ")).count();
        assert_eq!(starts, if terminal { 3 } else { 1 }, "{reason}: {calls}");
        assert_eq!(calls.lines().filter(|s| s.contains("turn end ")).count(), 2);
        assert!(calls.contains("--turn-id token-1 --reason stop"));
        if terminal {
            assert!(calls.contains("--turn-id token-2 --reason stop"));
        }
        assert_eq!(
            fs::read_to_string(root.join("retained")).unwrap(),
            if terminal { "no\n" } else { "yes\n" },
            "{reason}"
        );
        let state: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("state/session-1.json")).unwrap()).unwrap();
        assert_eq!(
            state["daemon_turn_id"],
            if terminal { "token-3" } else { "token-1" }
        );
    }
}
