//! The read-only scratch report (t3912): what this project's own tests and tools have left in
//! `/tmp`. Nothing here deletes or writes.
//!
//! For each known prefix it gives the number of entries, their size, the oldest and newest
//! modification, and how many are held open by a running process of this user (its working
//! directory or an open file is inside the entry). Everything else in the directory is only
//! counted and sized, never named: it belongs to other projects.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::worktree_disk::{usage, Budget};

/// Name prefixes of what this project's tests and tools create in the temp directory, with
/// where each comes from.
pub const OWN_PREFIXES: [(&str, &str); 9] = [
    (
        "dunkingsheep-tests-",
        "Python test suite (tests/_bootstrap.py)",
    ),
    ("outbox-", "Python outbox tests"),
    ("sweep-", "older Rust task tests (pid-named, before t1490)"),
    (
        "dunk-guard-",
        "Python CLI guard tests (tests/test_cli_guard.py)",
    ),
    ("handoff-", "next-job handoff tests"),
    ("ctx-", "tools/snapshot_context.py"),
    ("ds-golden-state-", "tools/diff_harness.py"),
    ("opencode-turn-", "opencode turn tests"),
    ("ds-", "Rust test scratch (testsupport::dir)"),
];

/// The prefix an entry name belongs to; the longest match wins (`ds-golden-state-` over `ds-`).
pub fn own_prefix(name: &str) -> Option<&'static str> {
    OWN_PREFIXES
        .iter()
        .map(|(prefix, _)| *prefix)
        .filter(|prefix| name.starts_with(prefix))
        .max_by_key(|prefix| prefix.len())
}

/// Paths running processes of this user hold: working directories and open files.
pub fn held_paths(proc_root: &Path, uid: u32) -> Vec<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let mut held = Vec::new();
    let Ok(entries) = std::fs::read_dir(proc_root) else {
        return held;
    };
    for entry in entries.flatten() {
        let numbered = entry
            .file_name()
            .to_str()
            .is_some_and(|name| !name.is_empty() && name.chars().all(|c| c.is_ascii_digit()));
        if !numbered || std::fs::metadata(entry.path()).map_or(true, |meta| meta.uid() != uid) {
            continue;
        }
        if let Ok(cwd) = std::fs::read_link(entry.path().join("cwd")) {
            held.push(cwd);
        }
        if let Ok(fds) = std::fs::read_dir(entry.path().join("fd")) {
            held.extend(
                fds.flatten()
                    .filter_map(|fd| std::fs::read_link(fd.path()).ok()),
            );
        }
    }
    held
}

#[derive(Default)]
struct Group {
    entries: u64,
    bytes: u64,
    oldest: Option<f64>,
    newest: Option<f64>,
    held_open: u64,
}

/// The report for `root` (normally `/tmp`). `held` is `held_paths`. Stops at the budget: what
/// was not reached is counted in `not_inspected` and the report is `partial`.
pub fn report(root: &Path, held: &[PathBuf], now: f64, budget: &Budget) -> Result<Value, String> {
    let mut names: Vec<(String, PathBuf)> = std::fs::read_dir(root)
        .map_err(|error| format!("{}: {error}", root.display()))?
        .flatten()
        .filter_map(|entry| Some((entry.file_name().to_str()?.to_string(), entry.path())))
        .collect();
    names.sort();
    let mut groups: BTreeMap<&str, Group> = BTreeMap::new();
    let mut other = Group::default();
    let mut not_inspected = 0u64;
    for (name, path) in names {
        if budget.spent() {
            not_inspected += 1;
            continue;
        }
        let used = usage(&path, &[], budget);
        let group = match own_prefix(&name) {
            Some(prefix) => groups.entry(prefix).or_default(),
            None => &mut other,
        };
        group.entries += 1;
        group.bytes += used.bytes;
        if let Some(at) = used.newest {
            group.oldest = Some(group.oldest.map_or(at, |old| old.min(at)));
            group.newest = Some(group.newest.map_or(at, |new| new.max(at)));
        }
        if held.iter().any(|path_held| path_held.starts_with(&path)) {
            group.held_open += 1;
        }
    }
    let age = |at: Option<f64>| at.map(|at| ((now - at).max(0.0) / 3600.0).floor() as i64);
    let row = |group: &Group| {
        json!({
            "entries": group.entries,
            "bytes": group.bytes,
            "oldest_age_h": age(group.oldest),
            "newest_age_h": age(group.newest),
            "held_open": group.held_open,
        })
    };
    let own: serde_json::Map<String, Value> = OWN_PREFIXES
        .iter()
        .filter_map(|(prefix, source)| {
            let group = groups.get(prefix)?;
            let mut value = row(group);
            value["source"] = json!(source);
            Some((prefix.to_string(), value))
        })
        .collect();
    let own_entries: u64 = groups.values().map(|group| group.entries).sum();
    let own_bytes: u64 = groups.values().map(|group| group.bytes).sum();
    Ok(json!({
        "root": root,
        "generated_at": now,
        "read_only": true,
        "partial": not_inspected > 0,
        "not_inspected": not_inspected,
        "own": own,
        "own_totals": {"entries": own_entries, "bytes": own_bytes},
        "other": {"entries": other.entries, "bytes": other.bytes, "held_open": other.held_open, "note": "other projects' and tools' entries: counted, not named"},
        "note": "nothing was deleted; removal needs a reviewed dry-run list and the coordinator's GO",
    }))
}

/// The operator's view of `report`.
pub fn render_text(report: &Value) -> String {
    let mib = |bytes: &Value| bytes.as_u64().unwrap_or(0) as f64 / (1024.0 * 1024.0);
    let mut out = format!(
        "{}: this project's leftovers {} entries, {:.1} MiB; other {} entries, {:.1} MiB\n",
        report["root"].as_str().unwrap_or("?"),
        report["own_totals"]["entries"],
        mib(&report["own_totals"]["bytes"]),
        report["other"]["entries"],
        mib(&report["other"]["bytes"]),
    );
    if report["partial"] == true {
        out.push_str(&format!(
            "PARTIAL: the time limit was reached; {} entries not inspected\n",
            report["not_inspected"]
        ));
    }
    for (prefix, row) in report["own"].as_object().into_iter().flatten() {
        out.push_str(&format!(
            "  {prefix:<20} {:>6} entries {:>8.1} MiB  oldest {} h  held open {}  ({})\n",
            row["entries"],
            mib(&row["bytes"]),
            row["oldest_age_h"],
            row["held_open"],
            row["source"].as_str().unwrap_or(""),
        ));
    }
    out.push_str("read-only: nothing was deleted.\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_longest_known_prefix_wins_and_others_are_not_ours() {
        assert_eq!(own_prefix("ds-golden-state-x1"), Some("ds-golden-state-"));
        assert_eq!(own_prefix("ds-gate-abc"), Some("ds-"));
        assert_eq!(
            own_prefix("dunkingsheep-tests-9"),
            Some("dunkingsheep-tests-")
        );
        assert_eq!(own_prefix("tmp.AbCdEf"), None);
        assert_eq!(own_prefix("claude-1000"), None);
    }

    #[test]
    fn the_report_groups_own_entries_counts_others_and_sees_held_ones() {
        let root = tempfile::tempdir().unwrap();
        for name in ["sweep-1", "sweep-2", "ds-golden-state-a", "tmp.Poolrad1"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
            std::fs::write(root.path().join(name).join("f"), vec![0u8; 4096]).unwrap();
        }
        std::fs::write(root.path().join("outbox-x"), "").unwrap();
        let held = vec![root.path().join("sweep-2/f")];
        let report = report(root.path(), &held, 2.0e9, &Budget::unlimited()).unwrap();
        assert_eq!(report["own"]["sweep-"]["entries"], 2);
        assert_eq!(report["own"]["sweep-"]["held_open"], 1);
        assert_eq!(report["own"]["ds-golden-state-"]["entries"], 1);
        assert_eq!(report["own"]["outbox-"]["entries"], 1);
        assert_eq!(report["own_totals"]["entries"], 4);
        assert_eq!(report["other"]["entries"], 1);
        assert!(
            !report.to_string().contains("Poolrad"),
            "other entries are not named"
        );
        assert_eq!(report["partial"], false);
        assert!(render_text(&report).contains("read-only"));
        // Nothing was removed.
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 5);
    }

    #[test]
    fn a_spent_budget_inspects_nothing_and_says_so() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("sweep-1")).unwrap();
        let report = report(root.path(), &[], 2.0e9, &Budget::new(0.0, usize::MAX)).unwrap();
        assert_eq!(
            (report["partial"].clone(), report["not_inspected"].clone()),
            (json!(true), json!(1))
        );
    }

    #[test]
    fn held_paths_reads_cwd_and_open_files_of_this_user() {
        let me = std::fs::metadata("/proc/self").map(|meta| {
            use std::os::unix::fs::MetadataExt;
            meta.uid()
        });
        let held = held_paths(Path::new("/proc"), me.unwrap());
        let cwd = std::env::current_dir().unwrap();
        assert!(held.contains(&cwd), "this test's own cwd is held");
    }
}
