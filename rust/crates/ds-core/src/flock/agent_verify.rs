//! T37 (slice 2): `Flock._verify_agent_started` and `Flock._match_agent_process`
//! (dunk_core.py) - the "a pane is not a running agent" check that `start_agent` runs
//! before it claims "started" (t215, t599, t957, t958).
//!
//! The backend calls it makes are behind [`VerifyBackend`] (the Python's `self.herdr` plus
//! `dunk_callers.process_start`), so the whole decision is testable against scripted panes,
//! screens and process tables. Time is real (`Instant` / `sleep(0.1)`), as in the Python;
//! the window is the caller's `verify_s` (`AGENT_VERIFY_S`, default 2.0).
//!
//! THE ORDER MATTERS and is the acceptance of T37: process evidence is read BEFORE the
//! failure markers (t599: a pi lane's launcher noise must not refuse a live agent), the
//! first-run dialog is checked before the failure markers and is NOT excused by a live
//! process (t957), an explicit `[agent exited rc=` line is never excused (t215 REVISE P1),
//! and a pid seen on a frame is revalidated at the deadline.

use std::time::{Duration, Instant};

use serde_json::Value;

use crate::error::DunkError;

/// `AGENT_FAILURE_MARKERS`.
pub const AGENT_FAILURE_MARKERS: [&str; 5] = [
    "command not found",
    ": not found",
    "no such file or directory",
    "permission denied",
    "cannot execute",
];

/// `AGENT_DIALOG_MARKERS`: matched only in the LIVE TAIL of the screen (t958).
pub const AGENT_DIALOG_MARKERS: [&str; 10] = [
    "do you trust the files",
    "trust this folder",
    "trust the files in this folder",
    "select login method",
    "log in to claude",
    "sign in to claude",
    "choose a theme",
    "press enter to continue",
    "allow access to",
    "press enter to confirm or esc to cancel",
];

pub const AGENT_DIALOG_TAIL_LINES: usize = 8;

/// `DEAD_STATES` (dunk_callers): zombie and dead.
const DEAD_STATES: [&str; 3] = ["Z", "X", "x"];

/// `float(os.environ.get("DUNKINGSHEEP_AGENT_VERIFY_S", "2.0"))`.
pub fn agent_verify_s_from(value: Option<&str>) -> f64 {
    value
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(2.0)
}

/// What `_verify_agent_started` reads from `self.herdr` (+ `process_start`).
pub trait VerifyBackend {
    fn get_pane(&self, pane_id: &str) -> Option<Value>;
    /// `read_pane(pane_id, lines=20)`: `None` when unreadable.
    fn read_pane(&mut self, pane_id: &str, lines: usize) -> Option<String>;
    /// `agent_child_pids(pane_pid)`: the pane's process table, `None` when this platform
    /// has none (screen-only verification).
    fn agent_child_pids(&mut self, pane_pid: i64) -> Option<Vec<Value>>;
    /// `dunk_callers.process_start`: the pane process's start time, `None` if unreadable.
    fn process_start(&self, pid: i64) -> Option<i64>;
}

fn refused<T>(text: String) -> Result<T, DunkError> {
    Err(DunkError::Refused(text))
}

/// Python `repr(str)`.
fn repr_str(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::from(quote);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

/// Python `repr(name)` for messages outside this module.
pub fn repr_name(name: &str) -> String {
    repr_str(name)
}

/// `captured.strip()[:200]`.
fn head200(captured: &str) -> String {
    captured.trim().chars().take(200).collect()
}

fn basename_lower(text: &str) -> String {
    text.rsplit('/').next().unwrap_or(text).to_lowercase()
}

/// `_match_agent_process`: pids with kernel evidence of running `kind`. An argv basename is
/// not identity; the kernel `comm` must name the agent, the state must be neither dead nor
/// stopped/traced, and when both start times are known the process must be no older than
/// the pane (a leftover pid from an earlier generation is not evidence).
pub fn match_agent_process(entries: &[Value], kind: &str, since_start: Option<i64>) -> Vec<i64> {
    let want = kind.to_lowercase();
    if want.is_empty() {
        return Vec::new();
    }
    let mut found = Vec::new();
    for ent in entries {
        let comm = ent.get("comm").and_then(Value::as_str).unwrap_or("");
        if basename_lower(comm) != want {
            continue;
        }
        let state = ent.get("state").and_then(Value::as_str).unwrap_or("");
        if DEAD_STATES.contains(&state) || state == "T" || state == "t" {
            continue;
        }
        if let (Some(since), Some(start)) = (since_start, ent.get("start").filter(|v| !v.is_null()))
        {
            match start
                .as_i64()
                .or_else(|| start.as_str().and_then(|s| s.trim().parse().ok()))
            {
                Some(start) if start < since => continue,
                Some(_) => {}
                None => continue,
            }
        }
        if let Some(pid) = ent.get("pid").and_then(Value::as_i64) {
            found.push(pid);
        }
    }
    found
}

/// `_verify_agent_started`: refuse to report "started" for a pane whose command never ran.
/// Returns the confirmed agent pid, or `None` on the screen-only path (no `kind`, or no
/// process table).
pub fn verify_agent_started(
    backend: &mut dyn VerifyBackend,
    name: &str,
    pane_id: Option<&str>,
    kind: Option<&str>,
    verify_s: f64,
) -> Result<Option<i64>, DunkError> {
    let Some(pane_id) = pane_id.filter(|p| !p.is_empty()) else {
        return refused(format!(
            "agent {} was launched without a pane id",
            repr_str(name)
        ));
    };
    let name_r = repr_str(name);
    let mut watch_procs = kind.is_some();
    let deadline = Instant::now() + Duration::from_secs_f64(verify_s.max(0.0));
    let mut captured = String::new();
    let mut pane_pid: Option<i64> = None;
    let mut since_start: Option<i64> = None;
    let mut seen: Vec<i64> = Vec::new();
    while Instant::now() < deadline {
        let Some(pane) = backend.get_pane(pane_id) else {
            let tail = if captured.trim().is_empty() {
                String::new()
            } else {
                format!("; captured: {}", repr_str(&head200(&captured)))
            };
            return refused(format!(
                "agent {name_r} did not stay up: pane {pane_id} is already gone{tail}"
            ));
        };
        if pane_pid.is_none() {
            pane_pid = pane.get("pane_pid").and_then(Value::as_i64);
            if let Some(pid) = pane_pid {
                // Generation binding: unreadable (a fixture pid, a race) skips the clause.
                since_start = backend.process_start(pid);
            }
        }
        captured = backend.read_pane(pane_id, 20).unwrap_or_default();
        let lowered = captured.to_lowercase();
        // PROCESS EVIDENCE FIRST, TEXT SECOND (t599).
        if watch_procs {
            let entries = pane_pid.and_then(|pid| backend.agent_child_pids(pid));
            match entries {
                None => watch_procs = false, // no table / no pid: screen-only from here on
                Some(entries) => {
                    // Latched, never returned early: a sighting is observation.
                    let matched = match_agent_process(&entries, kind.unwrap_or(""), since_start);
                    if !matched.is_empty() {
                        seen = matched;
                    }
                }
            }
        }
        // t957/t958: a first-run dialog is a REFUSAL, scoped to the LIVE TAIL; a live
        // process does not excuse it.
        let tail_lines: Vec<&str> = captured.lines().filter(|r| !r.trim().is_empty()).collect();
        let start = tail_lines.len().saturating_sub(AGENT_DIALOG_TAIL_LINES);
        let tail_rows = &tail_lines[start..];
        let tail = tail_rows.join("\n").to_lowercase();
        for marker in AGENT_DIALOG_MARKERS {
            if tail.contains(marker) {
                let line = tail_rows
                    .iter()
                    .find(|row| row.to_lowercase().contains(marker))
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| head200(&captured));
                return refused(format!(
                    "agent {name_r} started in pane {pane_id} but is NOT interactive: a first-run \
                     dialog is waiting ({}). Answer it once in that pane (or pre-seed the CLI's \
                     config) and start the agent again; a dunk sent here would sit unsubmitted (t957).",
                    repr_str(line.trim())
                ));
            }
        }
        for marker in AGENT_FAILURE_MARKERS {
            if lowered.contains(marker) && seen.is_empty() {
                let line = captured
                    .lines()
                    .find(|row| row.to_lowercase().contains(marker))
                    .map(|r| r.to_string())
                    .unwrap_or_else(|| head200(&captured));
                return refused(format!(
                    "agent {name_r} did not start in pane {pane_id}: {}. The pane is up but the \
                     command never ran - check the PATH the pane inherits.",
                    repr_str(line.trim())
                ));
            }
        }
        // An explicit exit line is NOT gated on a sighting (t215 REVISE P1).
        if lowered.contains("[agent exited rc=") {
            let line = captured
                .lines()
                .find(|row| row.contains("agent exited rc="))
                .unwrap_or("");
            return refused(format!(
                "agent {name_r} exited immediately in pane {pane_id}: {}",
                repr_str(line.trim())
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !watch_procs {
        return Ok(None);
    }
    // P1: revalidate at the deadline.
    let entries = pane_pid.and_then(|pid| backend.agent_child_pids(pid));
    let kind = kind.unwrap_or("");
    let confirmed = entries
        .map(|e| match_agent_process(&e, kind, since_start))
        .unwrap_or_default();
    if let Some(pid) = confirmed.first() {
        return Ok(Some(*pid));
    }
    let captured_note = if captured.trim().is_empty() {
        String::new()
    } else {
        format!(" Captured: {}", repr_str(&head200(&captured)))
    };
    if let Some(pid) = seen.first() {
        return refused(format!(
            "agent {name_r} started to appear in pane {pane_id} (pid {pid}) but the process is \
             gone at the end of the observation window - the command did not stay up.{captured_note}"
        ));
    }
    refused(format!(
        "agent {name_r} never became a process in pane {pane_id}: no {} process under pid {}. \
         The pane is up but the command never ran - check the PATH the pane inherits.{captured_note}",
        repr_str(kind),
        pane_pid.map(|p| p.to_string()).unwrap_or_else(|| "None".into())
    ))
}

#[cfg(test)]
#[path = "agent_verify_tests.rs"]
mod tests;
