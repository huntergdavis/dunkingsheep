//! Local helpers used by the Poolrad strict-candidate queue.
//!
//! This module only reads local files and invokes the local candidate exporter;
//! it does not access GitHub, the task queue, or Poolrad repositories.

use chrono::Utc;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const STRICT_MANIFEST: &str = "docs/re/strict_full_c_addresses.csv";
const IDENTITIES: &str = "docs/re/reviewed_function_identities.csv";
const TITLE_PREFIX: &str = "Poolrad strict C:";
const METHOD_DOC: &str = "docs/re/STRICT_CANDIDATE_QUEUE.md";
const FILED_KEY_PREFIX: &str = "poolrad-strict:";
const EXPORT_ARGS: &[&str] = &[
    "--inventory",
    "resources/function-accounting-current/inventory.json",
    "--callgraph",
    "resources/function-accounting-current/callgraph.json",
    "--near-calls",
    "resources/function-accounting-current/near.json",
    "--ovr",
    "resources/DOS/POOLRAD/GAME.OVR",
    "--repo",
    ".",
    "--no-git",
];

/// Run `git -C repo args`, returning stripped stdout on success and an empty
/// string for a nonzero exit, as `dunk_poolrad._git` does.
pub fn git(repo: impl AsRef<Path>, args: &[&str]) -> std::io::Result<String> {
    let child = Command::new("git")
        .arg("-C")
        .arg(repo.as_ref())
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let (status, stdout, _) =
        wait_with_output(child, Duration::from_secs(30)).map_err(std::io::Error::other)?;
    if !status.success() {
        return Ok(String::new());
    }
    Ok(String::from_utf8_lossy(&stdout).trim().to_owned())
}

/// Read CSV `id` values from the strict manifest; an unreadable/missing file
/// yields an empty set. Values are filtered before trimming, matching Python's
/// `if row.get("id")` behavior.
pub fn credited_ids(repo: impl AsRef<Path>) -> HashSet<String> {
    let path = repo.as_ref().join(STRICT_MANIFEST);
    let Ok(contents) = fs::read_to_string(path) else {
        return HashSet::new();
    };
    let mut records = csv_records(&contents).into_iter();
    let Some(header) = records.next() else {
        return HashSet::new();
    };
    let Some(id_column) = header.iter().position(|column| column == "id") else {
        return HashSet::new();
    };
    records
        .filter_map(|record| record.get(id_column))
        .filter(|id| !id.is_empty())
        .map(|id| id.trim().to_owned())
        .collect()
}

/// Count rows in the reviewed identities CSV. A missing/unreadable file maps to
/// `None`, while an empty or header-only file has a count of zero.
pub fn identity_count(repo: impl AsRef<Path>) -> Option<usize> {
    let contents = fs::read_to_string(repo.as_ref().join(IDENTITIES)).ok()?;
    Some(csv_records(&contents).len().saturating_sub(1))
}

/// Run the repository's local exporter, then load its JSON output. Relative
/// output paths are passed to the child unchanged and opened relative to this
/// process afterward, matching the Python helper's behavior.
pub fn export_queue(
    repo: impl AsRef<Path>,
    out: impl AsRef<Path>,
    timeout: Duration,
) -> Result<Value, String> {
    let out = out.as_ref();
    let mut command = Command::new("python3");
    command
        .arg("tools/overlay/strict_candidate_queue.py")
        .args(EXPORT_ARGS)
        .arg("--json")
        .arg(out)
        .current_dir(repo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let child = command.spawn().map_err(|error| error.to_string())?;
    let (status, stdout, stderr) = wait_with_output(child, timeout)?;
    if !status.success() {
        let detail = if stderr.is_empty() { stdout } else { stderr };
        return Err(format!(
            "strict_candidate_queue.py failed ({}): {}",
            status.code().unwrap_or(-1),
            tail_chars(detail.trim(), 400)
        ));
    }
    let contents = fs::read(out).map_err(|error| error.to_string())?;
    serde_json::from_slice(&contents).map_err(|error| error.to_string())
}

fn wait_with_output(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
            let output = child
                .wait_with_output()
                .map_err(|error| error.to_string())?;
            return Ok((status, output.stdout, output.stderr));
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err("strict_candidate_queue.py timed out".to_owned());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn tail_chars(text: &str, max_chars: usize) -> &str {
    let start = text
        .char_indices()
        .rev()
        .nth(max_chars)
        .map_or(0, |(index, ch)| index + ch.len_utf8());
    &text[start..]
}

/// Select ranked composable candidates that are not credited, filed, closed,
/// or helper candidates. Sorting is stable for equal ranks.
pub fn candidates(
    queue: &Value,
    credited: &HashSet<String>,
    filed_ids: &HashSet<String>,
    limit: usize,
    closed: Option<&HashMap<String, String>>,
) -> Vec<Value> {
    let Some(rows) = queue.get("rows").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut sorted: Vec<(usize, &Value)> = rows.iter().enumerate().collect();
    sorted.sort_by(|(left_index, left), (right_index, right)| {
        rank_key(left)
            .total_cmp(&rank_key(right))
            .then(left_index.cmp(right_index))
    });
    let mut picked = Vec::new();
    for (_, row) in sorted {
        let Some(id) = row.get("id").and_then(Value::as_str) else {
            continue;
        };
        if id.is_empty()
            || credited.contains(id)
            || filed_ids.contains(id)
            || closed.is_some_and(|closed| closed.contains_key(id))
        {
            continue;
        }
        let readiness = row.get("callee_readiness").unwrap_or(&Value::Null);
        if !python_truthy(readiness.get("composable").unwrap_or(&Value::Null)) {
            continue;
        }
        if row.get("classification").and_then(Value::as_str) == Some("helper_candidate") {
            continue;
        }
        picked.push(row.clone());
        // Keep Python's check-after-append semantics, including limit == 0.
        if picked.len() >= limit {
            break;
        }
    }
    picked
}

fn rank_key(row: &Value) -> f64 {
    let rank = row.get("rank").unwrap_or(&Value::Null);
    if !python_truthy(rank) {
        return 1_000_000_000.0;
    }
    rank.as_f64().unwrap_or(1_000_000_000.0)
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

/// Format the candidate title using the Python helper's field layout.
pub fn task_title(row: &Value) -> Result<String, String> {
    let id = required(row, "id")?;
    Ok(format!(
        "{TITLE_PREFIX} {id} ({} B, grade {}, rank {})",
        optional(row, "bytes"),
        optional(row, "grade"),
        optional(row, "rank")
    ))
}

/// Render the deterministic portion of the task body plus its UTC filing
/// timestamp, matching `dunk_poolrad.task_body`.
pub fn task_body(row: &Value, base: &str, _repo: impl AsRef<Path>) -> Result<String, String> {
    let id = required(row, "id")?;
    let readiness = row.get("callee_readiness").unwrap_or(&Value::Null);
    let counts = readiness.get("counts").unwrap_or(&Value::Null);
    let (count_total, count_items) = counts_summary(counts);
    let aliases = row
        .get("aliases")
        .and_then(Value::as_array)
        .map(|values| values.iter().map(py_str).collect::<Vec<_>>().join(", "))
        .unwrap_or_default();
    let lines = [
        format!("POOLRAD STRICT FULL-C BODY {id}"),
        format!(
            "base: {base} (poolrad-decomp main at filing; rebase onto current main before the PR)"
        ),
        format!("aliases: {aliases}"),
        format!(
            "unit {} offset {} ovr_offset {}",
            optional(row, "unit"),
            optional(row, "offset"),
            optional(row, "ovr_offset")
        ),
        format!(
            "bytes {}  instructions {}  grade {}  behaviour {}  classification {}",
            optional(row, "bytes"),
            optional(row, "instructions"),
            optional(row, "grade"),
            optional(row, "behaviour"),
            optional(row, "classification")
        ),
        format!(
            "exporter rank {} score {} confidence {}",
            optional(row, "rank"),
            optional(row, "score"),
            optional(row, "confidence")
        ),
        format!(
            "callees: ready {} of {} ({})",
            optional(readiness, "ready"),
            count_total,
            count_items
        ),
        format!("prior translation: {}", prior_translation(row)),
        String::new(),
        format!("METHOD: {METHOD_DOC} (the strict full-C rules), docs/LANES.md (worker brief)."),
        "Work in a private worktree of poolrad-decomp; never edit the shared checkout.".to_owned(),
        "ACCEPTANCE: a PR that adds the body's C and its evidence document, with the".to_owned(),
        "media/hashes a reviewer can reproduce; two reviewer-lane verdicts of `accept`;".to_owned(),
        "the accounting commit is written by the review timer from the verdict (the".to_owned(),
        "strict manifest row), never by the author. A claim one step wider than its".to_owned(),
        "evidence is the defect this fleet records most; say what was NOT verified.".to_owned(),
        format!("filed-key: {FILED_KEY_PREFIX}{id}@{base}"),
        format!(
            "filed by backlog-sync at {}",
            Utc::now().format("%Y-%m-%dT%H:%M:%SZ")
        ),
    ];
    Ok(lines.join("\n"))
}

fn prior_translation(row: &Value) -> String {
    let value = row.get("prior_translation").unwrap_or(&Value::Null);
    if python_truthy(value) {
        py_str(value)
    } else {
        "none".to_owned()
    }
}

fn counts_summary(counts: &Value) -> (String, String) {
    let Some(counts) = counts.as_object() else {
        return ("?".to_owned(), String::new());
    };
    if counts.is_empty() {
        return ("?".to_owned(), String::new());
    }
    let mut values: Vec<(&String, &Value)> = counts.iter().collect();
    values.sort_by(|(a, _), (b, _)| a.cmp(b));
    let total: i64 = counts.values().filter_map(Value::as_i64).sum();
    let items = values
        .into_iter()
        .map(|(key, value)| format!("{key}={}", py_str(value)))
        .collect::<Vec<_>>()
        .join(", ");
    (total.to_string(), items)
}

/// Return the first filed key for each row id, in input order. Titles remain a
/// fallback for old tasks without the machine-readable key.
pub fn filed_keys_from_tasks(tasks: &[Value]) -> HashMap<String, (Value, String)> {
    let mut output = HashMap::new();
    for task in tasks {
        let status = task
            .get("status")
            .filter(|value| python_truthy(value))
            .map(py_str)
            .unwrap_or_default();
        let body = task.get("body").and_then(Value::as_str).unwrap_or_default();
        let mut id = parse_filed_key(body);
        if id.is_none() {
            let title = task
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if let Some(rest) = title.strip_prefix(TITLE_PREFIX) {
                id = rest.split_whitespace().next().map(str::to_owned);
            }
        }
        if let Some(id) = id.filter(|id| !id.is_empty()) {
            output
                .entry(id)
                .or_insert_with(|| (task.get("id").cloned().unwrap_or(Value::Null), status));
        }
    }
    output
}

fn parse_filed_key(body: &str) -> Option<String> {
    for line in body.lines() {
        let Some(value) = line.strip_prefix("filed-key: ") else {
            continue;
        };
        let value = value.trim_end();
        let value = value.strip_prefix(FILED_KEY_PREFIX)?;
        let (id, base) = value.split_once('@')?;
        if !id.is_empty()
            && !base.is_empty()
            && !id.chars().any(char::is_whitespace)
            && !base.chars().any(char::is_whitespace)
        {
            return Some(id.to_owned());
        }
    }
    None
}

/// Return the IDs present in any task status.
pub fn filed_ids_from_tasks(tasks: &[Value]) -> HashSet<String> {
    filed_keys_from_tasks(tasks).into_keys().collect()
}

fn required(row: &Value, key: &str) -> Result<String, String> {
    row.get(key).map(py_str).ok_or_else(|| format!("'{key}'"))
}

fn optional(row: &Value, key: &str) -> String {
    row.get(key)
        .map(py_str)
        .unwrap_or_else(|| "None".to_owned())
}

fn py_str(value: &Value) -> String {
    match value {
        Value::Null => "None".to_owned(),
        Value::Bool(true) => "True".to_owned(),
        Value::Bool(false) => "False".to_owned(),
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

/// Parse the CSV subset used by `csv.DictReader`, including escaped quotes,
/// quoted commas/newlines, CRLF, and a final record without a newline.
fn csv_records(input: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut chars = input.chars().peekable();
    let mut quoted = false;
    while let Some(ch) = chars.next() {
        if quoted {
            match ch {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                _ => field.push(ch),
            }
            continue;
        }
        match ch {
            '"' if field.is_empty() => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\n' => {
                row.push(std::mem::take(&mut field));
                if row.len() > 1 || !row[0].is_empty() {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            '\r' if chars.peek() == Some(&'\n') => {
                chars.next();
                row.push(std::mem::take(&mut field));
                if row.len() > 1 || !row[0].is_empty() {
                    rows.push(std::mem::take(&mut row));
                } else {
                    row.clear();
                }
            }
            _ => field.push(ch),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fs;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temporary directory")
    }

    #[test]
    fn git_helper_returns_trimmed_success_and_empty_failure() {
        let dir = tempdir();
        Command::new("git")
            .args(["init", "--quiet"])
            .current_dir(dir.path())
            .status()
            .unwrap();
        assert_eq!(
            git(dir.path(), &["rev-parse", "--show-toplevel"]).unwrap(),
            dir.path().to_string_lossy()
        );
        assert_eq!(git(dir.path(), &["not-a-command"]).unwrap(), "");
    }

    #[test]
    fn csv_helpers_match_missing_empty_and_quoted_rows() {
        let dir = tempdir();
        assert!(credited_ids(dir.path()).is_empty());
        assert_eq!(identity_count(dir.path()), None);
        let manifest = dir.path().join(STRICT_MANIFEST);
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(
            &manifest,
            "name,id\r\na, one \r\nb,\"two,three\"\r\nc,\"four\nfive\"\r\nd,\r\n",
        )
        .unwrap();
        assert_eq!(
            credited_ids(dir.path()),
            HashSet::from([
                "one".to_owned(),
                "two,three".to_owned(),
                "four\nfive".to_owned()
            ])
        );
        let identities = dir.path().join(IDENTITIES);
        fs::write(&identities, "id,name\n1,a\n2,\"b\nc\"\n").unwrap();
        assert_eq!(identity_count(dir.path()), Some(2));
    }

    #[test]
    fn export_queue_uses_isolated_fake_exporter_and_loads_json() {
        let repo = tempdir();
        let exporter = repo.path().join("tools/overlay/strict_candidate_queue.py");
        fs::create_dir_all(exporter.parent().unwrap()).unwrap();
        fs::write(&exporter, "import json, sys\nout=sys.argv[sys.argv.index('--json')+1]\njson.dump({'rows':[{'id':'x'}]}, open(out, 'w'))\n").unwrap();
        let out = repo.path().join("queue.json");
        assert_eq!(
            export_queue(repo.path(), &out, Duration::from_secs(5)).unwrap(),
            json!({"rows":[{"id":"x"}]})
        );
    }

    #[test]
    fn export_queue_reports_fake_exporter_error_and_timeout() {
        let repo = tempdir();
        let exporter = repo.path().join("tools/overlay/strict_candidate_queue.py");
        fs::create_dir_all(exporter.parent().unwrap()).unwrap();
        fs::write(
            &exporter,
            "import sys\nsys.stderr.write('fake exporter failed')\nsys.exit(7)\n",
        )
        .unwrap();
        let err = export_queue(
            repo.path(),
            repo.path().join("out.json"),
            Duration::from_secs(5),
        )
        .unwrap_err();
        assert!(err.contains("failed (7): fake exporter failed"));
        fs::write(&exporter, "import time\ntime.sleep(1)\n").unwrap();
        assert!(export_queue(
            repo.path(),
            repo.path().join("out.json"),
            Duration::from_millis(20)
        )
        .unwrap_err()
        .contains("timed out"));
    }

    #[test]
    fn candidate_selection_filters_and_stably_ranks_rows() {
        let queue = json!({"rows":[
            {"id":"late","rank":4,"callee_readiness":{"composable":true}},
            {"id":"credited","rank":1,"callee_readiness":{"composable":true}},
            {"id":"filed","rank":2,"callee_readiness":{"composable":true}},
            {"id":"closed","rank":3,"callee_readiness":{"composable":true}},
            {"id":"helper","rank":0,"classification":"helper_candidate","callee_readiness":{"composable":true}},
            {"id":"no","rank":-1,"callee_readiness":{"composable":false}},
            {"id":"first","rank":4,"callee_readiness":{"composable":true}},
            {"id":"last","callee_readiness":{"composable":true}}
        ]});
        let credited = HashSet::from(["credited".to_owned()]);
        let filed = HashSet::from(["filed".to_owned()]);
        let closed = HashMap::from([("closed".to_owned(), "reason".to_owned())]);
        let picked = candidates(&queue, &credited, &filed, 2, Some(&closed));
        assert_eq!(
            picked
                .iter()
                .map(|row| row["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["late", "first"]
        );
        assert_eq!(
            candidates(&queue, &credited, &filed, 0, Some(&closed)).len(),
            1
        );
    }

    #[test]
    fn task_title_and_body_preserve_layout_and_filed_key() {
        let row = json!({
            "id":"OVL_0001_0001", "bytes":12, "grade":"A", "rank":3,
            "aliases":["a", "b"], "unit":"U1", "offset":"0x20", "ovr_offset":"0x30",
            "instructions":4, "behaviour":"loop", "classification":"body", "score":0.8,
            "confidence":"high", "callee_readiness":{"ready":2,"counts":{"z":1,"a":2}},
            "prior_translation":null
        });
        assert_eq!(
            task_title(&row).unwrap(),
            "Poolrad strict C: OVL_0001_0001 (12 B, grade A, rank 3)"
        );
        let body = task_body(&row, "abc123", "/ignored").unwrap();
        assert!(body.starts_with("POOLRAD STRICT FULL-C BODY OVL_0001_0001\nbase: abc123 "));
        assert!(body.contains("callees: ready 2 of 3 (a=2, z=1)"));
        assert!(body.contains("filed-key: poolrad-strict:OVL_0001_0001@abc123"));
        assert!(body.contains("filed by backlog-sync at "));
    }

    #[test]
    fn filed_keys_include_all_statuses_fallback_titles_and_first_duplicate() {
        let tasks = vec![
            json!({"id":"t1","status":"done","body":"x\nfiled-key: poolrad-strict:OVL_1@abc\n"}),
            json!({"id":"t2","status":"cancelled","body":"","title":"Poolrad strict C: OVL_2 (1 B)"}),
            json!({"id":"t3","status":"open","body":"filed-key: poolrad-strict:OVL_1@new"}),
        ];
        let keys = filed_keys_from_tasks(&tasks);
        assert_eq!(keys.get("OVL_1"), Some(&(json!("t1"), "done".to_owned())));
        assert_eq!(
            keys.get("OVL_2"),
            Some(&(json!("t2"), "cancelled".to_owned()))
        );
        assert_eq!(
            filed_ids_from_tasks(&tasks),
            HashSet::from(["OVL_1".to_owned(), "OVL_2".to_owned()])
        );
    }
}
