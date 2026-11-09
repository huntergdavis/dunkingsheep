//! In-pane Codex hook. Refusals never block a user prompt.
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}
fn safe_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn root() -> Option<(u32, String)> {
    let mut pid = std::process::id();
    for _ in 0..64 {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let (head, tail) = stat.rsplit_once(')')?;
        let comm = head.split_once('(')?.1;
        let fields: Vec<_> = tail.split_whitespace().collect();
        let cmd = fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        let matches = comm.eq_ignore_ascii_case("codex")
            || cmd.split(|b| *b == 0).any(|p| {
                Path::new(std::ffi::OsStr::from_bytes(p))
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("codex"))
            });
        if matches {
            return Some((pid, fields.get(19)?.to_string()));
        }
        let parent: u32 = fields.get(1)?.parse().ok()?;
        if parent == 0 || parent == pid {
            return None;
        }
        pid = parent;
    }
    None
}
fn state_dir() -> Option<PathBuf> {
    if let Some(v) = std::env::var_os("DUNKINGSHEEP_CODEX_STATE_DIR") {
        return Some(v.into());
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|v| PathBuf::from(v).join(".local/state")))?;
    Some(base.join("dunkingsheep/codex-turns"))
}
fn cli(args: &[&str], cwd: &str) -> Option<Value> {
    let bin = std::env::var_os("DUNKINGSHEEP_BIN").unwrap_or_else(|| "dunkingsheep".into());
    let mut child = Command::new(bin)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let mut buf = [0u8; 8192];
        let mut overflow = false;
        loop {
            match stdout.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if out.len() + n <= 1024 * 1024 {
                        out.extend_from_slice(&buf[..n]);
                    } else {
                        overflow = true;
                    }
                }
                Err(_) => return None,
            }
        }
        if overflow {
            None
        } else {
            Some(out)
        }
    });
    let started = std::time::Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => {
                let out = reader.join().ok()??;
                return serde_json::from_slice(&out).ok();
            }
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if started.elapsed() >= Duration::from_secs(10) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
}
fn handoff(prompt: &str) -> Option<&str> {
    for (i, _) in prompt.match_indices("Handoff nj-") {
        if i > 0
            && (prompt.as_bytes()[i - 1].is_ascii_alphanumeric()
                || prompt.as_bytes()[i - 1] == b'_')
        {
            continue;
        }
        let tail = &prompt[i + 8..];
        let b = tail.as_bytes();
        if b.len() >= 36
            && &b[..3] == b"nj-"
            && b[3..35]
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            && b[35] == b'.'
        {
            return tail.get(..35);
        }
    }
    None
}
fn run() -> Option<()> {
    let action = std::env::args().nth(1)?;
    if action != "start" && action != "end" {
        return None;
    }
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).ok()?;
    let event: Value = serde_json::from_str(&input).ok()?;
    let session = event.get("session_id")?.as_str()?;
    let turn = event.get("turn_id")?.as_str()?;
    let cwd = event.get("cwd")?.as_str()?;
    if !safe_id(session) || !safe_id(turn) || cwd.is_empty() {
        return None;
    }
    let (pid, start) = root()?;
    let dir = state_dir()?;
    fs::create_dir_all(&dir).ok()?;
    fs::set_permissions(&dir, fs::Permissions::from_mode(0o700)).ok()?;
    let path = dir.join(format!("{session}.json"));
    let lock = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join(format!("{session}.lock")))
        .ok()?;
    lock.lock().ok()?;
    let read_state = || {
        fs::read(&path)
            .ok()
            .and_then(|v| serde_json::from_slice::<Value>(&v).ok())
    };
    let mut state = read_state();
    if action == "end" && state.is_none() {
        let wait = std::env::var("DUNKINGSHEEP_CODEX_END_WAIT_S")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|n| n.is_finite() && *n >= 0.0)
            .unwrap_or(2.0)
            .min(10.0);
        let deadline = std::time::Instant::now() + Duration::from_secs_f64(wait);
        while state.is_none() && std::time::Instant::now() < deadline {
            lock.unlock().ok()?;
            std::thread::sleep(Duration::from_millis(50));
            lock.lock().ok()?;
            state = read_state();
        }
    }
    let ttl: f64 = std::env::var("DUNKINGSHEEP_CODEX_TURN_TTL_S")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|n: &f64| n.is_finite() && *n > 0.0)
        .unwrap_or(1800.0);
    if action == "start" {
        if state.as_ref().is_some_and(|s| {
            s["codex_turn_id"] == turn
                && s["root_pid"] == pid
                && s["root_start"] == start
                && s["expires_at"].as_f64().unwrap_or(0.0) > now()
        }) {
            return Some(());
        }
        let ttl_s = (ttl as u64).to_string();
        let mut args = vec![
            "--json",
            "turn",
            "start",
            "--source",
            "codex-hook",
            "--ttl-s",
            &ttl_s,
        ];
        if let Some(id) = event
            .get("prompt")
            .and_then(Value::as_str)
            .and_then(handoff)
        {
            args.extend(["--handoff-id", id]);
        }
        let result = cli(&args, cwd)?;
        let token = result.get("turn_id")?.as_str().filter(|v| !v.is_empty())?;
        let pane = result.get("pane_id")?.as_str()?;
        let value = json!({"codex_turn_id":turn,"daemon_turn_id":token,"session_id":session,"cwd":cwd,"pane_id":pane,"root_pid":pid,"root_start":start,"started_at":now(),"expires_at":now()+ttl});
        let temp = dir.join(format!(".{session}.{}.tmp", std::process::id()));
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temp)
            .ok()?;
        f.write_all(serde_json::to_string(&value).ok()?.as_bytes())
            .ok()?;
        fs::rename(temp, path).ok()?;
    } else {
        let s = state?;
        if s["session_id"] != session
            || s["codex_turn_id"] != turn
            || s["root_pid"] != pid
            || s["root_start"] != start
        {
            return None;
        }
        if s["expires_at"].as_f64().unwrap_or(0.0) <= now() {
            fs::remove_file(path).ok()?;
            return None;
        }
        let token = s["daemon_turn_id"].as_str().filter(|s| !s.is_empty())?;
        let stored_cwd = s["cwd"].as_str()?;
        fs::remove_file(path).ok()?;
        drop(lock);
        let _ = cli(
            &[
                "--json",
                "turn",
                "end",
                "--turn-id",
                token,
                "--reason",
                "stop",
            ],
            stored_cwd,
        );
    }
    Some(())
}
fn main() {
    let _ = run();
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_session_and_handoff() {
        assert!(safe_id("session-1"));
        assert!(!safe_id("../session"));
        assert_eq!(
            handoff("Handoff nj-0123456789abcdef0123456789abcdef."),
            Some("nj-0123456789abcdef0123456789abcdef")
        );
        assert!(handoff("xHandoff nj-0123456789abcdef0123456789abcdef.").is_none());
    }
}
