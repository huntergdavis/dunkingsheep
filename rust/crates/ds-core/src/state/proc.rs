//! The /proc side of `dunk_state.py` (`_proc_children`, `_proc_snapshot`, `_proc_start_ticks`,
//! `_cmdline`, `_open_session_file`, `_env_of`, `_proc_start`, `_active_session_file`,
//! `_cwd_slug`, `_session_id_from_path`) behind a [`ProcReader`] trait, plus the identity-binding
//! helpers (`_still_the_same_process`, `_read_bound`, `_descendants`).
//!
//! The Python tests monkey-patch these module functions to stage a pid being recycled. Rust has
//! no monkey-patching, so the reads are a trait: [`LiveProc`] reads the real /proc, and a test
//! supplies a fake. The ORDER and COUNT of `proc_start_ticks` calls inside `session_of` is part of
//! the contract (the tests stage a recycle at the Nth call), so the helpers below call the reader
//! exactly where the Python does.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// pid -> child pids.
pub type Kids = BTreeMap<i64, Vec<i64>>;
/// pid -> `/proc/<pid>/stat` field 22 (start time in ticks), recorded in the same pass.
pub type Starts = BTreeMap<i64, i64>;

/// Every read `session_of` and `capture` make of the process table.
pub trait ProcReader {
    /// `_proc_children()`.
    fn proc_children(&self) -> Kids;
    /// `_proc_snapshot()`: ONE /proc pass.
    fn proc_snapshot(&self) -> (Kids, Starts);
    /// `_proc_start_ticks(pid)`: raw start-time ticks (field 22 of stat), or `None`. Deliberately
    /// not `proc_start`, which is a WALL-CLOCK start used to pick a session file.
    fn proc_start_ticks(&self, pid: i64) -> Option<i64>;
    /// `_cmdline(pid)`.
    fn cmdline(&self, pid: i64) -> Vec<String>;
    /// `_open_session_file(pid)`: identity from the PROCESS, not the cwd.
    fn open_session_file(&self, pid: i64) -> Option<String>;
    /// `_env_of(pid)`.
    fn env_of(&self, pid: i64) -> BTreeMap<String, String>;
    /// `_proc_start(pid)`: wall-clock start (stat starttime + boot time).
    fn proc_start(&self, pid: i64) -> Option<f64>;
    /// `_active_session_file(cwd, since)`: the newest session file written in this workspace at
    /// or after the pane's agent started (minus slack), and how many qualified.
    fn active_session_file(&self, cwd: Option<&str>, since: Option<f64>)
        -> (Option<String>, usize);
}

/// `_still_the_same_process(pid, starts)`: is `pid` still the process the snapshot saw? Unknown
/// is NOT the same: a missing snapshot entry, an unreadable stat and a changed start time all
/// answer false. This is the check that stops a recycled pid from donating a foreign session,
/// model or effort to a pane.
pub fn still_the_same_process(reader: &dyn ProcReader, pid: i64, starts: Option<&Starts>) -> bool {
    let Some(starts) = starts else {
        // The caller omitted snapshot evidence entirely (legacy callers). capture ALWAYS passes one.
        return true;
    };
    if starts.is_empty() {
        // t500 addendum [P2]: a SUPPLIED but EMPTY snapshot is not "no evidence" - it is evidence
        // that /proc was unreadable or racing. Treating it as a bypass is how an unverified root
        // argv became a session with no reason.
        return false;
    }
    let Some(expected) = starts.get(&pid) else {
        return false;
    };
    reader.proc_start_ticks(pid) == Some(*expected)
}

/// `_read_bound(pid, starts, reader)`: read process metadata only when the identity holds BEFORE
/// and AFTER. t500 addendum [P2]: a precheck alone leaves a check-to-read race - the pid can be
/// recycled inside the read. A changed or unreadable identity returns `(None, false)` and the
/// caller must not use the value.
pub fn read_bound<T>(
    reader: &dyn ProcReader,
    pid: i64,
    starts: Option<&Starts>,
    read: impl FnOnce(i64) -> T,
) -> (Option<T>, bool) {
    if !still_the_same_process(reader, pid, starts) {
        return (None, false);
    }
    let value = read(pid);
    if !still_the_same_process(reader, pid, starts) {
        return (None, false);
    }
    (Some(value), true)
}

/// `_descendants(pid, kids)`: every descendant, in the Python's discovery order (LIFO stack).
pub fn descendants(pid: i64, kids: &Kids) -> Vec<i64> {
    let mut out = Vec::new();
    let mut queue = vec![pid];
    while let Some(here) = queue.pop() {
        for child in kids.get(&here).into_iter().flatten() {
            out.push(*child);
            queue.push(*child);
        }
    }
    out
}

/// `_cwd_slug(cwd)`: the sessions-directory slug a harness uses for a working directory.
pub fn cwd_slug(cwd: Option<&str>) -> Option<String> {
    let cwd = cwd.filter(|c| !c.is_empty())?;
    Some(format!("--{}--", cwd.trim_matches('/').replace('/', "-")))
}

/// `_session_id_from_path(path)`: pi writes `2026-10-24T18-04-46-250Z_01a16f16-...`, claude
/// `<uuid>`.
pub fn session_id_from_path(path: &str) -> String {
    let base = path.rsplit('/').next().unwrap_or(path);
    let stem = base.rsplit_once('.').map_or(base, |(stem, _)| stem);
    if stem.contains('_') {
        stem.rsplit('_').next().unwrap_or(stem).to_string()
    } else {
        stem.to_string()
    }
}

/// The reads against the real /proc (`/proc/<pid>/...`) and the harness session directories.
#[derive(Debug, Clone)]
pub struct LiveProc {
    /// Where `~` expands to (the Python's `os.path.expanduser`).
    pub home: PathBuf,
}

impl LiveProc {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        Self { home: home.into() }
    }

    /// `_SESSION_ROOTS`.
    pub fn session_roots(&self) -> [PathBuf; 3] {
        [
            self.home.join(".pi/agent/sessions"),
            self.home.join(".claude/projects"),
            self.home.join(".codex/sessions"),
        ]
    }
}

fn stat_fields(pid: i64) -> Option<Vec<String>> {
    let raw = fs::read(format!("/proc/{pid}/stat")).ok()?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (_, tail) = text.rsplit_once(')')?;
    Some(tail.split_whitespace().map(str::to_string).collect())
}

fn numeric_entries() -> Option<Vec<i64>> {
    let entries = fs::read_dir("/proc").ok()?;
    Some(
        entries
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_str()
                    .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
                    .and_then(|n| n.parse().ok())
            })
            .collect(),
    )
}

impl ProcReader for LiveProc {
    fn proc_children(&self) -> Kids {
        let mut kids = Kids::new();
        for pid in numeric_entries().unwrap_or_default() {
            let Some(ppid) = stat_fields(pid).and_then(|f| f.get(1)?.parse::<i64>().ok()) else {
                continue;
            };
            kids.entry(ppid).or_default().push(pid);
        }
        kids
    }

    fn proc_snapshot(&self) -> (Kids, Starts) {
        let (mut kids, mut starts) = (Kids::new(), Starts::new());
        for pid in numeric_entries().unwrap_or_default() {
            let Some(fields) = stat_fields(pid) else {
                continue;
            };
            let (Some(ppid), Some(start)) = (
                fields.get(1).and_then(|v| v.parse::<i64>().ok()),
                fields.get(19).and_then(|v| v.parse::<i64>().ok()),
            ) else {
                continue;
            };
            kids.entry(ppid).or_default().push(pid);
            starts.insert(pid, start);
        }
        (kids, starts)
    }

    fn proc_start_ticks(&self, pid: i64) -> Option<i64> {
        stat_fields(pid)?.get(19)?.parse().ok()
    }

    fn cmdline(&self, pid: i64) -> Vec<String> {
        fs::read(format!("/proc/{pid}/cmdline"))
            .map(|raw| {
                String::from_utf8_lossy(&raw)
                    .split('\0')
                    .filter(|p| !p.is_empty())
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn open_session_file(&self, pid: i64) -> Option<String> {
        let roots = self.session_roots();
        for entry in fs::read_dir(format!("/proc/{pid}/fd")).ok()?.flatten() {
            let Ok(target) = fs::read_link(entry.path()) else {
                continue;
            };
            let target = target.to_string_lossy().into_owned();
            if !target.ends_with(".jsonl") {
                continue;
            }
            if roots
                .iter()
                .any(|root| target.starts_with(&root.to_string_lossy().into_owned()))
            {
                return Some(target);
            }
        }
        None
    }

    fn env_of(&self, pid: i64) -> BTreeMap<String, String> {
        let Ok(raw) = fs::read(format!("/proc/{pid}/environ")) else {
            return BTreeMap::new();
        };
        String::from_utf8_lossy(&raw)
            .split('\0')
            .filter_map(|item| {
                item.split_once('=')
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            })
            .collect()
    }

    fn proc_start(&self, pid: i64) -> Option<f64> {
        let ticks: f64 = stat_fields(pid)?.get(19)?.parse().ok()?;
        let stat = fs::read_to_string("/proc/stat").ok()?;
        let boot: f64 = stat.lines().find_map(|l| {
            l.strip_prefix("btime ")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })?;
        Some(boot + ticks / crate::load::ticks())
    }

    fn active_session_file(
        &self,
        cwd: Option<&str>,
        since: Option<f64>,
    ) -> (Option<String>, usize) {
        // The file this workspace is actually being written to. Evidence, not guessing: a candidate
        // must live in THIS workspace's session directory AND have been written at or after the
        // pane's agent started (minus a small slack for the file being created a moment before the
        // process is observable). The newest such file wins; the caller is told how many qualify.
        const SLACK_S: f64 = 90.0;
        let Some(slug) = cwd_slug(cwd) else {
            return (None, 0);
        };
        let cutoff = since.filter(|s| *s != 0.0).map(|s| s - SLACK_S);
        let (mut best, mut newer, mut count) = (None, 0.0_f64, 0usize);
        for root in self.session_roots() {
            let Ok(names) = fs::read_dir(Path::new(&root).join(&slug)) else {
                continue;
            };
            for entry in names.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !name.ends_with(".jsonl") {
                    continue;
                }
                let Some(mtime) = entry
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_secs_f64())
                else {
                    continue;
                };
                if cutoff.is_some_and(|c| mtime < c) {
                    continue;
                }
                count += 1;
                if mtime > newer {
                    best = Some(entry.path().to_string_lossy().into_owned());
                    newer = mtime;
                }
            }
        }
        (best, count)
    }
}
