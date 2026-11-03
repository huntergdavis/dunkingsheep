//! Local helpers used by the Poolrad strict-candidate queue.
//!
//! This module reads local files, invokes the local candidate exporter, and
//! writes injectable credit-pending state; it never accesses GitHub, the task
//! queue, or Poolrad repositories.

use chrono::Utc;
use regex::Regex;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const STRICT_MANIFEST: &str = "docs/re/strict_full_c_addresses.csv";
const IDENTITIES: &str = "docs/re/reviewed_function_identities.csv";
const TITLE_PREFIX: &str = "Poolrad strict C:";
const METHOD_DOC: &str = "docs/re/STRICT_CANDIDATE_QUEUE.md";
const FILED_KEY_PREFIX: &str = "poolrad-strict:";
const EXPORTER_INLINE_IN_STRICT: &str = "inline_in_strict";
const EXPORTER_MAIN_MODEL: &str = "main_model";
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
        .filter_map(|record| record.get(id_column).cloned())
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
    let parent = out
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(parent).map_err(|error| error.to_string())?;
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
        let detail = String::from_utf8_lossy(&detail);
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
    mut child: Child,
    timeout: Duration,
) -> Result<(std::process::ExitStatus, Vec<u8>, Vec<u8>), String> {
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "child stdout was not piped".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "child stderr was not piped".to_owned())?;
    let stdout_reader = drain_pipe(stdout);
    let stderr_reader = drain_pipe(stderr);
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                return Ok((status, join_pipe(stdout_reader)?, join_pipe(stderr_reader)?));
            }
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = join_pipe(stdout_reader);
                let _ = join_pipe(stderr_reader);
                return Err(error.to_string());
            }
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            let _ = join_pipe(stdout_reader);
            let _ = join_pipe(stderr_reader);
            return Err("strict_candidate_queue.py timed out".to_owned());
        }
        thread::sleep(Duration::from_millis(10));
    }
}

fn drain_pipe<R>(mut reader: R) -> JoinHandle<io::Result<Vec<u8>>>
where
    R: Read + Send + 'static,
{
    thread::spawn(move || {
        let mut output = Vec::new();
        reader.read_to_end(&mut output).map(|_| output)
    })
}

fn join_pipe(reader: JoinHandle<io::Result<Vec<u8>>>) -> Result<Vec<u8>, String> {
    reader
        .join()
        .map_err(|_| "child output reader panicked".to_owned())?
        .map_err(|error| error.to_string())
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
        py_repr(value)
    } else {
        "none".to_owned()
    }
}

fn py_repr(value: &Value) -> String {
    match value {
        Value::Array(values) => format!(
            "[{}]",
            values.iter().map(py_repr).collect::<Vec<_>>().join(", ")
        ),
        Value::Object(values) => format!(
            "{{{}}}",
            values
                .iter()
                .map(|(key, value)| format!("{}: {}", py_quote(key), py_repr(value)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Value::String(value) => py_quote(value),
        _ => py_str(value),
    }
}

fn py_quote(value: &str) -> String {
    let quote = if value.contains('\'') && !value.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut output = String::with_capacity(value.len() + 2);
    output.push(quote);
    for ch in value.chars() {
        match ch {
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            ch if ch == quote => {
                output.push('\\');
                output.push(ch);
            }
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                write!(&mut output, "\\x{:02x}", ch as u32).expect("writing to String");
            }
            ch => output.push(ch),
        }
    }
    output.push(quote);
    output
}

fn counts_summary(counts: &Value) -> (String, String) {
    let Some(counts) = counts.as_object() else {
        return ("?".to_owned(), String::new());
    };
    if counts.is_empty() {
        return ("?".to_owned(), String::new());
    }
    let mut keys: Vec<&String> = counts.keys().collect();
    keys.sort();
    let total: i64 = counts.values().filter_map(Value::as_i64).sum();
    let items = keys
        .into_iter()
        .map(|key| format!("{key}={}", py_str(&counts[key])))
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

/// Return row IDs with a matching disposition document on the repository tree.
///
/// The filename before the first `DISPOSITION` token is the row ID, with any
/// trailing underscores or hyphens removed. Sorting matches Python's
/// `sorted(Path.glob(...))`; if multiple files reduce to one ID, the last name
/// in that order supplies the value.
pub fn disposition_closures(repo: impl AsRef<Path>) -> HashMap<String, String> {
    let root = repo.as_ref().join("docs/re");
    let Ok(mut names) = sorted_names(&root) else {
        return HashMap::new();
    };
    names.retain(|name| name.contains("DISPOSITION") && name.ends_with(".md"));

    let mut output = HashMap::new();
    for name in names {
        let Some((row_id, _)) = name.split_once("DISPOSITION") else {
            continue;
        };
        let row_id = row_id.trim_end_matches(['_', '-']);
        if !row_id.is_empty() {
            output.insert(row_id.to_owned(), format!("docs/re/{name}"));
        }
    }
    output
}

/// Return row IDs listed after `CHECKER_ROWS` in Python checker files.
///
/// Checker paths are visited in filename order and each file contributes the
/// first path for a duplicate row ID, matching Python's `setdefault` behavior.
pub fn registered_checker_ids(repo: impl AsRef<Path>) -> HashMap<String, String> {
    let root = repo.as_ref().join("tools/overlay");
    let Ok(mut names) = sorted_names(&root) else {
        return HashMap::new();
    };
    names.retain(|name| name.ends_with(".py"));

    let mut output = HashMap::new();
    for name in names {
        let Ok(bytes) = fs::read(root.join(&name)) else {
            continue;
        };
        let text = String::from_utf8_lossy(&bytes);
        let Some((_, rows)) = text.split_once("CHECKER_ROWS") else {
            continue;
        };
        let rows: String = rows.chars().take(400).collect();
        for row_id in checker_row_ids(&rows) {
            output
                .entry(row_id)
                .or_insert_with(|| format!("tools/overlay/{name}"));
        }
    }
    output
}

/// Port of Python `_unit_tokens`: return the accepted filename spellings for a
/// row's unit and offset. The caller checks the unit and offset as separate
/// filename segments; adjacency is not required.
pub fn _unit_tokens(row_id: &str) -> HashSet<String> {
    let parts: Vec<&str> = row_id.split('_').collect();
    if parts.len() < 3 {
        return HashSet::new();
    }
    let unit = parts[1].to_ascii_lowercase();
    let offset = parts[2].to_ascii_lowercase();
    let mut shapes = HashSet::from([unit.clone()]);
    if let Some(trimmed) = unit.strip_prefix("00") {
        shapes.insert(trimmed.to_owned());
    }
    if let Some(trimmed) = unit.strip_prefix('0') {
        shapes.insert(trimmed.to_owned());
    }

    let mut tokens = HashSet::from([offset]);
    for shape in shapes {
        let unpadded = shape.trim_start_matches('0');
        tokens.insert(format!(
            "u{}",
            if unpadded.is_empty() { "0" } else { unpadded }
        ));
        tokens.insert(shape);
    }
    tokens
}

fn sorted_names(root: &Path) -> io::Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "non-UTF-8 filename"))?;
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn checker_row_ids(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut row_ids = Vec::new();
    let mut search_from = 0;
    while let Some(relative_start) = text[search_from..].find("OVL_") {
        let start = search_from + relative_start;
        let mut end = start + 4;
        while end < bytes.len() && (bytes[end].is_ascii_hexdigit() || bytes[end] == b'_') {
            end += 1;
        }
        if end > start + 4 {
            row_ids.push(text[start..end].to_owned());
            search_from = end;
        } else {
            search_from = start + 1;
        }
    }
    row_ids
}

/// Return the first merge-commit PR number above `commit`, if one exists.
pub fn merge_pr(repo: impl AsRef<Path>, commit: &str) -> io::Result<Option<u64>> {
    let log = git(
        repo,
        &[
            "log",
            "--merges",
            "--ancestry-path",
            &format!("{commit}..HEAD"),
            "--format=%s",
        ],
    )?;
    let pattern = Regex::new(r"#([0-9]+)").expect("pull request number pattern");
    for subject in log.lines().rev().filter(|line| !line.trim().is_empty()) {
        if let Some(captures) = pattern.captures(subject) {
            if let Some(number) = captures
                .get(1)
                .and_then(|digits| digits.as_str().parse().ok())
            {
                return Ok(Some(number));
            }
        }
    }
    Ok(None)
}

/// Match a recovered-model filename carrying both the row's unit and offset.
pub fn model_name_matches(name: &str, rid: &str) -> bool {
    let parts: Vec<&str> = rid.split('_').collect();
    if parts.len() < 3 {
        return false;
    }
    let unit = parts[1].to_lowercase();
    let offset = parts[2].to_lowercase();
    let unit = unit.trim_start_matches('0');
    let unit = if unit.is_empty() { "0" } else { unit };
    let offset = offset.trim_start_matches('0');
    let offset = if offset.is_empty() { "0" } else { offset };
    let name = name.to_lowercase();
    let unit_pattern = Regex::new(&format!(r"(^|[_/])u{}([_/.]|$)", regex::escape(unit)))
        .expect("escaped unit pattern");
    let offset_pattern = Regex::new(&format!(r"(^|_)0*{}([_/.]|$)", regex::escape(offset)))
        .expect("escaped offset pattern");
    unit_pattern.is_match(&name) && offset_pattern.is_match(&name)
}

/// Find the first matching recovered model and its local commit/merge evidence.
pub fn model_evidence(repo: impl AsRef<Path>, rid: &str) -> io::Result<Option<Value>> {
    let repo = repo.as_ref();
    let src = repo.join("src/recovered");
    let Ok(entries) = fs::read_dir(src) else {
        return Ok(None);
    };
    let mut names: Vec<String> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".c"))
        .collect();
    names.sort();
    for name in names {
        if !model_name_matches(&name, rid) {
            continue;
        }
        let relative = format!("src/recovered/{name}");
        let commit = git(repo, &["log", "-1", "--format=%H", "--", &relative])?;
        if commit.is_empty() {
            continue;
        }
        let pr = merge_pr(repo, &commit)?
            .map(Value::from)
            .unwrap_or(Value::Null);
        return Ok(Some(serde_json::json!({
            "model": relative,
            "commit": commit,
            "pr": pr
        })));
    }
    Ok(None)
}

/// Read the model-status ledger; missing or unreadable CSV data gives an empty map.
pub fn model_statuses(repo: impl AsRef<Path>) -> HashMap<String, String> {
    let Ok(contents) = fs::read_to_string(repo.as_ref().join("docs/re/partial_callee_models.csv"))
    else {
        return HashMap::new();
    };
    let mut rows = csv_records(&contents).into_iter();
    let Some(header) = rows.next() else {
        return HashMap::new();
    };
    let id_column = header.iter().position(|column| column == "id");
    let status_column = header.iter().position(|column| column == "status");
    let mut statuses = HashMap::new();
    for row in rows {
        let Some(id) = id_column.and_then(|column| row.get(column)) else {
            continue;
        };
        if id.is_empty() {
            continue;
        }
        let status = status_column
            .and_then(|column| row.get(column))
            .map_or_else(String::new, |status| status.trim().to_lowercase());
        statuses.insert(id.trim().to_owned(), status);
    }
    statuses
}

/// Whether the repository records enough evidence to call a model credit-ready.
pub fn credit_ready(
    repo: impl AsRef<Path>,
    rid: &str,
    checkers: Option<&HashMap<String, String>>,
    statuses: Option<&HashMap<String, String>>,
) -> (bool, Option<String>) {
    let checkers = checkers
        .cloned()
        .unwrap_or_else(|| registered_checker_ids(repo.as_ref()));
    let statuses = statuses.cloned().unwrap_or_else(|| model_statuses(repo));
    if !checkers.contains_key(rid) {
        return (false, Some("no registered checker".to_owned()));
    }
    if let Some(status) = statuses
        .get(rid)
        .filter(|status| !status.is_empty() && status.as_str() != "full")
    {
        return (
            false,
            Some(format!(
                "model status {status} in docs/re/partial_callee_models.csv"
            )),
        );
    }
    (true, None)
}

/// Find a registered checker and a recovered filename carrying this row's unit
/// and offset as separate underscore-delimited segments.
pub fn already_on_main(
    repo: impl AsRef<Path>,
    rid: &str,
    checkers: Option<&HashMap<String, String>>,
) -> Option<String> {
    let repo = repo.as_ref();
    let checkers = checkers
        .cloned()
        .unwrap_or_else(|| registered_checker_ids(repo));
    let checker = checkers.get(rid).filter(|checker| !checker.is_empty())?;
    let tokens = _unit_tokens(rid);
    let parts: Vec<&str> = rid.split('_').collect();
    let offset = parts.get(2).copied().unwrap_or_default().to_lowercase();
    let unit_shapes: HashSet<String> = tokens
        .into_iter()
        .filter(|token| token != &offset)
        .collect();
    let src = repo.join("src/recovered");
    let Ok(entries) = fs::read_dir(src) else {
        return None;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if !name.ends_with(".c") {
            continue;
        }
        let segments: HashSet<&str> = name.split('_').collect();
        if !offset.is_empty()
            && segments.contains(offset.as_str())
            && unit_shapes
                .iter()
                .any(|unit| segments.contains(unit.as_str()))
        {
            return Some(format!("already on main ({checker} audits {name})"));
        }
    }
    None
}

/// Resolve the credit-pending file, using the configured Dunking Sheep state dir.
pub fn credit_pending_path(path: Option<&Path>) -> PathBuf {
    if let Some(path) = path {
        return path.to_owned();
    }
    let config = std::env::var_os("DUNKINGSHEEP_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/dunkingsheep"))
        })
        .unwrap_or_else(|| PathBuf::from(".config/dunkingsheep"));
    config.join("poolrad/credit-pending.json")
}

/// Merge pending evidence into a disposable or owner-selected file atomically.
/// Existing IDs keep their first-seen evidence. A filesystem failure matches
/// Python's warning-and-None result; malformed evidence is returned as an error.
pub fn record_credit_pending(
    entries: &Map<String, Value>,
    path: Option<&Path>,
) -> Result<Option<Value>, String> {
    let path = credit_pending_path(path);
    let mut bodies = fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|data| data.get("bodies").and_then(Value::as_object).cloned())
        .unwrap_or_default();
    for (rid, evidence) in entries {
        if bodies.contains_key(rid) {
            continue;
        }
        let Some(mut evidence) = evidence.as_object().cloned() else {
            return Err(format!("credit evidence for {rid} must be an object"));
        };
        evidence.insert("first_seen".to_owned(), json_timestamp());
        bodies.insert(rid.clone(), Value::Object(evidence));
    }
    let mut payload = Map::new();
    payload.insert("bodies".to_owned(), Value::Object(bodies.clone()));
    payload.insert("count".to_owned(), Value::from(bodies.len()));
    payload.insert("updated_at".to_owned(), json_timestamp());
    let payload = Value::Object(payload);
    let serialized = serde_json::to_vec_pretty(&payload).map_err(|error| error.to_string())?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if let Err(error) = fs::create_dir_all(parent) {
        warn_credit_pending_write(&path, &error);
        return Ok(None);
    }
    let mut temporary = OsString::from(path.as_os_str());
    temporary.push(".tmp");
    let temporary = PathBuf::from(temporary);
    let opened = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&temporary);
    let mut file = match opened {
        Ok(file) => file,
        Err(error) => {
            warn_credit_pending_write(&path, &error);
            return Ok(None);
        }
    };
    if let Err(error) = file.write_all(&serialized).and_then(|()| file.flush()) {
        warn_credit_pending_write(&path, &error);
        return Ok(None);
    }
    drop(file);
    if let Err(error) = fs::rename(&temporary, &path) {
        warn_credit_pending_write(&path, &error);
        return Ok(None);
    }
    Ok(Some(payload))
}

fn warn_credit_pending_write(path: &Path, error: &io::Error) {
    tracing::warn!(
        path = %path.display(),
        error = %error,
        "credit-pending record not written"
    );
}

/// Return the saved credit-pending evidence, or an empty map for absent/bad data.
pub fn credit_pending(path: Option<&Path>) -> Map<String, Value> {
    fs::read_to_string(credit_pending_path(path))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|data| data.get("bodies").and_then(Value::as_object).cloned())
        .unwrap_or_default()
}

fn json_timestamp() -> Value {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    Value::from((seconds * 1000.0).round_ties_even() / 1000.0)
}

/// Minimal task-store seam used by the local Poolrad sync port. Production
/// Flock wiring is intentionally left to the separate runtime integration.
pub trait PoolradTaskStore {
    fn list(
        &self,
        scope: Option<&str>,
        include_done: bool,
        limit: usize,
        kind: &str,
    ) -> Result<Vec<Value>, String>;

    fn add(
        &self,
        scope: &str,
        title: &str,
        body: &str,
        priority: &str,
        kind: &str,
        created_by: &str,
    ) -> Result<Value, String>;
}

/// Injectable port of Python `Sync`. Its default exporter is the local,
/// read-only candidate exporter; stores and exporters can be faked in tests.
type PoolradExporter<'a> = dyn Fn(&Path) -> Result<Value, String> + 'a;

pub struct PoolradSync<'a> {
    store: &'a dyn PoolradTaskStore,
    repo: PathBuf,
    exporter: Option<&'a PoolradExporter<'a>>,
    credit_path: Option<PathBuf>,
}

impl<'a> PoolradSync<'a> {
    pub fn new(store: &'a dyn PoolradTaskStore) -> Self {
        let repo = std::env::var_os("DUNKINGSHEEP_POOLRAD_REPO")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/home/hunter/workspace/poolrad-decomp"));
        Self {
            store,
            repo,
            exporter: None,
            credit_path: None,
        }
    }

    pub fn with_repo(mut self, repo: impl AsRef<Path>) -> Self {
        self.repo = repo.as_ref().to_owned();
        self
    }

    pub fn with_exporter(mut self, exporter: &'a PoolradExporter<'a>) -> Self {
        self.exporter = Some(exporter);
        self
    }

    pub fn with_credit_path(mut self, path: impl AsRef<Path>) -> Self {
        self.credit_path = Some(path.as_ref().to_owned());
        self
    }

    /// Run with the Python defaults: limit six, real filing, medium priority.
    pub fn run(&self) -> Result<Value, String> {
        self.run_with(6, false, "medium")
    }

    pub fn run_with(&self, limit: usize, dry_run: bool, priority: &str) -> Result<Value, String> {
        let mut errors = Vec::new();
        let mut report = serde_json::json!({
            "repo": self.repo.to_string_lossy(),
            "base": Value::Null,
            "filed": [],
            "skipped": Value::Null,
            "errors": []
        });
        let queue = match self.export_queue() {
            Ok(queue) => queue,
            Err(error) => {
                errors.push(error);
                report["errors"] = Value::Array(errors.into_iter().map(Value::String).collect());
                return Ok(report);
            }
        };
        let base = git(&self.repo, &["rev-parse", "HEAD"]).map_err(|error| error.to_string())?;
        report["base"] = Value::String(base.clone());
        let credited = credited_ids(&self.repo);
        let everything = self.store.list(None, true, 2000, "work")?;
        let filed_keys = filed_keys_from_tasks(&everything);
        let filed: HashSet<String> = filed_keys.keys().cloned().collect();

        let mut closed = HashMap::new();
        for (rid, where_closed) in disposition_closures(&self.repo) {
            closed.insert(rid, format!("closed by {where_closed}"));
        }
        let checkers = registered_checker_ids(&self.repo);
        let rows = queue
            .get("rows")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut credit = Map::new();
        let mut not_ready = Map::new();
        let mut body_on_main = Map::new();
        for row in &rows {
            let Some(rid) = row.get("id").and_then(Value::as_str) else {
                continue;
            };
            if rid.is_empty() {
                continue;
            }
            if !credited.contains(rid) {
                if let Some(evidence) =
                    model_evidence(&self.repo, rid).map_err(|error| error.to_string())?
                {
                    let (ready, why_not) = credit_ready(&self.repo, rid, Some(&checkers), None);
                    if ready {
                        let model = evidence["model"].as_str().unwrap_or_default().to_owned();
                        credit.insert(rid.to_owned(), evidence);
                        closed.insert(
                            rid.to_owned(),
                            format!(
                                "model already on main ({model}) and credit-ready; ledger credit pending, nothing to author"
                            ),
                        );
                    } else {
                        let mut evidence = evidence.as_object().cloned().unwrap_or_default();
                        evidence.insert(
                            "why_not".to_owned(),
                            Value::String(why_not.unwrap_or_default()),
                        );
                        not_ready.insert(rid.to_owned(), Value::Object(evidence));
                    }
                }
            }
            if closed.contains_key(rid) {
                continue;
            }
            let prior_translation = row
                .get("prior_translation")
                .filter(|value| python_truthy(value));
            let prior_status = prior_translation
                .and_then(|value| value.get("status"))
                .and_then(Value::as_str);
            if prior_status == Some(EXPORTER_INLINE_IN_STRICT) {
                closed.insert(
                    rid.to_owned(),
                    format!(
                        "exporter says {EXPORTER_INLINE_IN_STRICT}: the body is inline in a model that already carries strict credit, so there is nothing to author"
                    ),
                );
                continue;
            }
            if prior_status == Some(EXPORTER_MAIN_MODEL) {
                let why_not = not_ready
                    .get(rid)
                    .and_then(|value| value.get("why_not"))
                    .and_then(Value::as_str)
                    .unwrap_or("exporter says the model is on main; no registered checker");
                let mut details = not_ready
                    .get(rid)
                    .and_then(Value::as_object)
                    .cloned()
                    .unwrap_or_else(|| {
                        let model = prior_translation
                            .and_then(|value| value.get("main_files"))
                            .filter(|value| python_truthy(value))
                            .cloned()
                            .unwrap_or_else(|| Value::Array(Vec::new()));
                        Map::from_iter([("model".to_owned(), model)])
                    });
                details.insert("why_not".to_owned(), Value::String(why_not.to_owned()));
                body_on_main.insert(rid.to_owned(), Value::Object(details));
                closed.insert(
                    rid.to_owned(),
                    format!(
                        "exporter says {EXPORTER_MAIN_MODEL}: the body is on main and the credit is not ready ({why_not}) - the missing piece is the checker/evidence, not the body"
                    ),
                );
                continue;
            }
            if not_ready.contains_key(rid) {
                continue;
            }
            if let Some(reason) = already_on_main(&self.repo, rid, Some(&checkers)) {
                closed.insert(rid.to_owned(), reason);
            }
        }

        report["credit_pending"] = Value::Object(credit.clone());
        report["models_not_credit_ready"] = Value::Object(not_ready.clone());
        report["body_on_main"] = Value::Object(body_on_main);
        if !credit.is_empty() && !dry_run {
            let credit_path = self.credit_path.as_deref();
            let written = record_credit_pending(&credit, credit_path)?;
            report["credit_pending_file"] = if written.is_some() {
                Value::String(
                    credit_pending_path(credit_path)
                        .to_string_lossy()
                        .into_owned(),
                )
            } else {
                Value::Null
            };
        }

        let picked = candidates(&queue, &credited, &filed, limit, Some(&closed));
        let mut already_filed_tasks = Map::new();
        let mut filed_rows: Vec<_> = filed_keys.iter().collect();
        filed_rows.sort_by_key(|(left, _)| *left);
        for (rid, (task_id, status)) in filed_rows {
            already_filed_tasks.insert(
                rid.clone(),
                serde_json::json!({"task": task_id, "status": status}),
            );
        }
        let mut closed_rows = Map::new();
        let mut closed_keys: Vec<_> = closed.keys().collect();
        closed_keys.sort();
        for rid in closed_keys {
            if let Some(reason) = closed.get(rid) {
                closed_rows.insert(rid.clone(), Value::String(reason.clone()));
            }
        }
        report["skipped"] = serde_json::json!({
            "credited": credited.len(),
            "already_filed": filed.len(),
            "already_filed_tasks": already_filed_tasks,
            "queue_rows": rows.len(),
            "closed": closed_rows
        });

        let mut filed_report = Vec::new();
        for row in picked {
            let Some(rid) = row.get("id").and_then(Value::as_str) else {
                continue;
            };
            if dry_run {
                filed_report.push(serde_json::json!({
                    "id": rid,
                    "rank": row.get("rank").cloned().unwrap_or(Value::Null),
                    "dry": true
                }));
                continue;
            }
            let created = (|| {
                let title = task_title(&row)?;
                let body = task_body(&row, &base, &self.repo)?;
                self.store
                    .add("global", &title, &body, priority, "work", "backlog-sync")
            })();
            match created {
                Ok(task) => {
                    let Some(task_id) = task.get("id") else {
                        errors.push(format!("{rid}: 'id'"));
                        continue;
                    };
                    filed_report.push(serde_json::json!({
                        "id": rid,
                        "task": task_id,
                        "rank": row.get("rank").cloned().unwrap_or(Value::Null)
                    }));
                }
                Err(error) => errors.push(format!("{rid}: {error}")),
            }
        }
        report["filed"] = Value::Array(filed_report);
        report["errors"] = Value::Array(errors.into_iter().map(Value::String).collect());
        Ok(report)
    }

    fn export_queue(&self) -> Result<Value, String> {
        if let Some(exporter) = self.exporter {
            return exporter(&self.repo);
        }
        let out = poolrad_queue_path();
        export_queue(&self.repo, out, Duration::from_secs(300))
    }
}

fn poolrad_queue_path() -> PathBuf {
    let credit = credit_pending_path(None);
    credit
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("strict-queue.json")
}

/// Lightweight Poolrad progress view matching `dunk_poolrad.progress`.
pub fn progress(
    repo: Option<&Path>,
    queue_path: Option<&Path>,
    credit_path: Option<&Path>,
) -> Result<Value, String> {
    let repo = repo
        .map(Path::to_owned)
        .or_else(|| std::env::var_os("DUNKINGSHEEP_POOLRAD_REPO").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("/home/hunter/workspace/poolrad-decomp"));
    let queue_path = queue_path
        .map(Path::to_owned)
        .unwrap_or_else(poolrad_queue_path);
    let credited = credited_ids(&repo);
    let mut summary = Value::Object(Map::new());
    if let Ok(contents) = fs::read_to_string(&queue_path) {
        if let Ok(value) = serde_json::from_str::<Value>(&contents) {
            summary = value
                .get("summary")
                .filter(|value| python_truthy(value))
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
        }
    }
    let exported_at = if queue_path.exists() {
        let modified = fs::metadata(&queue_path)
            .and_then(|metadata| metadata.modified())
            .map_err(|error| error.to_string())?;
        let seconds = modified
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_secs_f64())
            .unwrap_or_else(|error| -error.duration().as_secs_f64());
        Value::from(seconds)
    } else {
        Value::Null
    };
    let main = git(&repo, &["rev-parse", "--short", "HEAD"]).map_err(|error| error.to_string())?;
    Ok(serde_json::json!({
        "credited": credited.len(),
        "identities": identity_count(&repo),
        "ceiling": 815,
        "ceiling_note": "CEILING: 815 is an upper bound, not a verified denominator",
        "composable_bodies": summary.get("composable_bodies").cloned().unwrap_or(Value::Null),
        "queue_candidates": summary.get("candidates").cloned().unwrap_or(Value::Null),
        "replaced": Value::Null,
        "replaced_note": "replaced (bodies the game actually runs) is computed by the dashboard integration; not available here yet",
        "main": main,
        "queue_exported_at": exported_at,
        "credit_pending": credit_pending(credit_path).len()
    }))
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
    use std::os::unix::fs::PermissionsExt;

    fn tempdir() -> tempfile::TempDir {
        tempfile::tempdir().expect("temporary directory")
    }

    fn fixture_git(repo: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_AUTHOR_NAME", "Poolrad Fixture")
            .env("GIT_AUTHOR_EMAIL", "poolrad-fixture@example.invalid")
            .env("GIT_COMMITTER_NAME", "Poolrad Fixture")
            .env("GIT_COMMITTER_EMAIL", "poolrad-fixture@example.invalid")
            .output()
            .expect("git fixture command");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_owned()
    }

    fn model_git_fixture(repo: &Path, filename: &str, pr: Option<u64>) -> String {
        fixture_git(repo, &["init", "--quiet", "--initial-branch=main"]);
        fs::write(repo.join("README.md"), "fixture\n").unwrap();
        fixture_git(repo, &["add", "-A"]);
        fixture_git(repo, &["commit", "--quiet", "-m", "init"]);
        if pr.is_some() {
            fixture_git(repo, &["checkout", "--quiet", "-b", "side"]);
        }
        let relative = format!("src/recovered/{filename}");
        let model = repo.join(&relative);
        fs::create_dir_all(model.parent().unwrap()).unwrap();
        fs::write(&model, "/* fixture model */\n").unwrap();
        fixture_git(repo, &["add", "-A"]);
        fixture_git(repo, &["commit", "--quiet", "-m", "add model"]);
        let commit = fixture_git(repo, &["rev-parse", "HEAD"]);
        if let Some(pr) = pr {
            fixture_git(repo, &["checkout", "--quiet", "main"]);
            fixture_git(
                repo,
                &[
                    "merge",
                    "--quiet",
                    "--no-ff",
                    "-m",
                    &format!("Merge pull request #{pr} from fixture/model"),
                    "side",
                ],
            );
        }
        commit
    }

    #[derive(Default)]
    struct FakePoolradStore {
        tasks: std::sync::Mutex<Vec<Value>>,
        add_attempts: std::sync::atomic::AtomicUsize,
        fail_on_attempt: std::sync::Mutex<Option<usize>>,
    }

    impl PoolradTaskStore for FakePoolradStore {
        fn list(
            &self,
            scope: Option<&str>,
            include_done: bool,
            limit: usize,
            kind: &str,
        ) -> Result<Vec<Value>, String> {
            assert_eq!(scope, None);
            assert!(include_done);
            assert_eq!(limit, 2000);
            assert_eq!(kind, "work");
            Ok(self.tasks.lock().unwrap().clone())
        }

        fn add(
            &self,
            scope: &str,
            title: &str,
            body: &str,
            priority: &str,
            kind: &str,
            created_by: &str,
        ) -> Result<Value, String> {
            assert_eq!(scope, "global");
            assert_eq!(kind, "work");
            assert_eq!(created_by, "backlog-sync");
            let attempt = self
                .add_attempts
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;
            if *self.fail_on_attempt.lock().unwrap() == Some(attempt) {
                return Err("fixture task write failed".to_owned());
            }
            let mut tasks = self.tasks.lock().unwrap();
            let id = format!("t{}", tasks.len() + 1);
            let task = serde_json::json!({
                "id": id,
                "title": title,
                "body": body,
                "priority": priority,
                "kind": kind,
                "status": "open",
                "assignees": []
            });
            tasks.push(task.clone());
            Ok(task)
        }
    }

    fn poolrad_row(rid: &str, rank: i64, prior_translation: Value) -> Value {
        serde_json::json!({
            "id": rid,
            "rank": rank,
            "score": 50,
            "bytes": 100,
            "grade": "A",
            "unit": 1,
            "offset": 2,
            "ovr_offset": 3,
            "instructions": 40,
            "behaviour": "leaf",
            "classification": "game_evidenced",
            "confidence": 0.9,
            "aliases": ["0x1:0x2"],
            "prior_translation": prior_translation,
            "callee_readiness": {"composable": true, "ready": 2, "counts": {"strict": 2}}
        })
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
        fs::write(
            &exporter,
            "import json, sys\nout=sys.argv[sys.argv.index('--json')+1]\nsys.stdout.write('x' * 200000)\nsys.stdout.flush()\nsys.stderr.write('y' * 200000)\nsys.stderr.flush()\njson.dump({'rows':[{'id':'x'}]}, open(out, 'w'))\n",
        )
        .unwrap();
        let out = repo.path().join("fresh/nested/queue.json");
        assert_eq!(
            export_queue(repo.path(), &out, Duration::from_secs(5)).unwrap(),
            json!({"rows":[{"id":"x"}]})
        );
        assert!(out.parent().unwrap().is_dir());
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
        for needle in [
            "STRICT_CANDIDATE_QUEUE.md",
            "two reviewer-lane verdicts",
            "NOT verified",
        ] {
            assert!(
                body.contains(needle),
                "missing Python acceptance text: {needle}"
            );
        }
    }

    #[test]
    fn task_body_formats_prior_translation_objects_like_python_dicts() {
        let row = json!({
            "id": "OVL_0001_0002",
            "prior_translation": {"status": "source_mention"}
        });

        let body = task_body(&row, "abc123", "/ignored").unwrap();

        assert!(body.contains("prior translation: {'status': 'source_mention'}"));
    }

    #[test]
    fn filed_keys_include_all_statuses_fallback_titles_and_first_duplicate() {
        let tasks = vec![
            json!({"id":"t1","status":"done","body":"x\nfiled-key: poolrad-strict:OVL_1@abc\n"}),
            json!({"id":"t2","status":"cancelled","body":"","title":"Poolrad strict C: OVL_2 (1 B)"}),
            json!({"id":"t3","status":"open","body":"filed-key: poolrad-strict:OVL_1@new"}),
            json!({"id":"t4","status":"blocked","body":"", "title":"Poolrad strict C: OVL_4 (1 B)"}),
            json!({"id":"t5","status":"open","body":"POOLRAD STRICT FULL-C BODY OVL_5\nfiled-key: poolrad-strict:OVL_9@deadbeef", "title":"Poolrad strict C: OVL_5 (1 B)"}),
        ];
        let keys = filed_keys_from_tasks(&tasks);
        assert_eq!(keys.get("OVL_1"), Some(&(json!("t1"), "done".to_owned())));
        assert_eq!(
            keys.get("OVL_2"),
            Some(&(json!("t2"), "cancelled".to_owned()))
        );
        assert_eq!(
            filed_ids_from_tasks(&tasks),
            HashSet::from([
                "OVL_1".to_owned(),
                "OVL_2".to_owned(),
                "OVL_4".to_owned(),
                "OVL_9".to_owned()
            ])
        );
        assert_eq!(keys.get("OVL_9").unwrap().0, json!("t5"));
        assert!(
            !keys.contains_key("OVL_5"),
            "body filed-key takes precedence over title"
        );
    }

    #[test]
    fn disposition_closures_use_sorted_filename_keys() {
        let repo = tempdir();
        let dispositions = repo.path().join("docs/re");
        fs::create_dir_all(&dispositions).unwrap();
        fs::write(dispositions.join("abc--DISPOSITION-a.md"), "ignored").unwrap();
        fs::write(dispositions.join("abc_DISPOSITION-b.md"), "ignored").unwrap();
        fs::write(dispositions.join("OVL_0068_0294_DISPOSITION.md"), "ignored").unwrap();
        fs::write(dispositions.join("not-a-disposition.txt"), "ignored").unwrap();

        assert_eq!(
            disposition_closures(repo.path()),
            HashMap::from([
                ("abc".to_owned(), "docs/re/abc_DISPOSITION-b.md".to_owned()),
                (
                    "OVL_0068_0294".to_owned(),
                    "docs/re/OVL_0068_0294_DISPOSITION.md".to_owned(),
                ),
            ])
        );
        assert!(disposition_closures(tempdir().path()).is_empty());
    }

    #[test]
    fn registered_checker_ids_follow_sorted_files_and_python_scan_window() {
        let repo = tempdir();
        let overlay = repo.path().join("tools/overlay");
        fs::create_dir_all(&overlay).unwrap();
        fs::write(
            overlay.join("a.py"),
            "CHECKER_ROWS = ['OVL_0001_0002', 'OVL_0003_0004']\n",
        )
        .unwrap();
        let mut b_contents = "CHECKER_ROWS".to_owned();
        b_contents.push_str(&"x".repeat(400));
        b_contents.push_str(" OVL_0005_0006");
        fs::write(overlay.join("b.py"), b_contents).unwrap();
        fs::write(
            overlay.join("c.py"),
            "CHECKER_ROWS = ['OVL_0001_0002', 'OVL_0007_0008']\n",
        )
        .unwrap();
        fs::create_dir(overlay.join("unreadable.py")).unwrap();
        fs::write(overlay.join("z.txt"), "CHECKER_ROWS OVL_0009_0009").unwrap();

        assert_eq!(
            registered_checker_ids(repo.path()),
            HashMap::from([
                ("OVL_0001_0002".to_owned(), "tools/overlay/a.py".to_owned()),
                ("OVL_0003_0004".to_owned(), "tools/overlay/a.py".to_owned()),
                ("OVL_0007_0008".to_owned(), "tools/overlay/c.py".to_owned()),
            ])
        );
        assert!(registered_checker_ids(tempdir().path()).is_empty());
    }

    #[test]
    fn unit_tokens_include_zero_trimmed_and_offset_spellings() {
        assert_eq!(
            _unit_tokens("OVL_006A_0824"),
            HashSet::from([
                "006a".to_owned(),
                "06a".to_owned(),
                "6a".to_owned(),
                "u6a".to_owned(),
                "0824".to_owned(),
            ])
        );
        assert_eq!(
            _unit_tokens("OVL_03B0_06FB_extra"),
            HashSet::from([
                "03b0".to_owned(),
                "3b0".to_owned(),
                "u3b0".to_owned(),
                "06fb".to_owned(),
            ])
        );
        assert!(
            _unit_tokens("OVL_1").is_empty(),
            "IDs without unit and offset components have no tokens"
        );
    }

    #[test]
    fn model_evidence_reports_first_model_commit_and_its_merge_pr() {
        let repo = tempdir();
        let commit = model_git_fixture(repo.path(), "overlay_u20_list_walk_11ac_cpu.c", Some(1433));
        let evidence = model_evidence(repo.path(), "OVL_0020_11AC")
            .unwrap()
            .unwrap();
        assert_eq!(
            evidence,
            json!({
                "model":"src/recovered/overlay_u20_list_walk_11ac_cpu.c",
                "commit":commit,
                "pr":1433
            })
        );
        assert_eq!(merge_pr(repo.path(), &commit).unwrap(), Some(1433));
        assert!(model_evidence(repo.path(), "OVL_0001_0002")
            .unwrap()
            .is_none());
    }

    #[test]
    fn model_evidence_does_not_invent_a_pr_for_a_direct_commit() {
        let repo = tempdir();
        let commit = model_git_fixture(repo.path(), "overlay_u20_list_walk_11ac_cpu.c", None);
        assert_eq!(merge_pr(repo.path(), &commit).unwrap(), None);
        assert_eq!(
            model_evidence(repo.path(), "OVL_0020_11AC")
                .unwrap()
                .unwrap()["pr"],
            Value::Null
        );
    }

    #[test]
    fn credit_pending_writes_atomically_with_private_mode_and_keeps_first_evidence() {
        let state = tempdir();
        let path = state.path().join("nested/credit-pending.json");
        let entries = serde_json::from_value::<Map<String, Value>>(json!({
            "OVL_0020_11AC":{"model":"m.c","commit":"abc","pr":1433}
        }))
        .unwrap();
        let first = record_credit_pending(&entries, Some(&path))
            .unwrap()
            .unwrap();
        assert_eq!(first["count"], 1);
        let first_seen = first["bodies"]["OVL_0020_11AC"]["first_seen"].clone();
        assert!(first_seen.is_number());
        assert_eq!(
            credit_pending(Some(&path)),
            first["bodies"].as_object().unwrap().clone()
        );
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let later = serde_json::from_value::<Map<String, Value>>(json!({
            "OVL_0020_11AC":{"model":"replacement.c","commit":"def","pr":9},
            "OVL_0001_0002":{"model":"new.c","commit":"ghi","pr":10}
        }))
        .unwrap();
        let second = record_credit_pending(&later, Some(&path)).unwrap().unwrap();
        assert_eq!(second["count"], 2);
        assert_eq!(second["bodies"]["OVL_0020_11AC"]["model"], "m.c");
        assert_eq!(second["bodies"]["OVL_0020_11AC"]["first_seen"], first_seen);
        assert!(second["updated_at"].as_f64().unwrap() >= first["updated_at"].as_f64().unwrap());
        assert!(credit_pending_path(Some(&path)).ends_with("nested/credit-pending.json"));
    }

    #[test]
    fn credit_pending_reads_bad_data_as_empty_and_write_failure_returns_none() {
        let state = tempdir();
        let path = state.path().join("credit-pending.json");
        assert!(credit_pending(Some(&path)).is_empty());
        fs::write(&path, "{broken").unwrap();
        assert!(credit_pending(Some(&path)).is_empty());
        fs::remove_file(&path).unwrap();

        let directory_target = state.path().join("target-directory");
        fs::create_dir(&directory_target).unwrap();
        let entries = serde_json::from_value::<Map<String, Value>>(json!({
            "OVL_1_1":{"model":"m.c"}
        }))
        .unwrap();
        assert!(record_credit_pending(&entries, Some(&directory_target))
            .unwrap()
            .is_none());
    }

    #[test]
    fn model_statuses_trim_ids_normalize_statuses_and_keep_last_duplicate() {
        let repo = tempdir();
        let path = repo.path().join("docs/re/partial_callee_models.csv");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "id,status,evidence_doc\r\nOVL_A, PARTIAL ,a.md\r\nOVL_B,CALLBACK,b.md\r\n  OVL_C  , FULL ,c.md\r\nOVL_A,FULL,last.md\r\n,partial,empty.md\r\n",
        )
        .unwrap();
        assert_eq!(
            model_statuses(repo.path()),
            HashMap::from([
                ("OVL_A".to_owned(), "full".to_owned()),
                ("OVL_B".to_owned(), "callback".to_owned()),
                ("OVL_C".to_owned(), "full".to_owned()),
            ])
        );
        assert!(model_statuses(tempdir().path()).is_empty());
    }

    #[test]
    fn credit_ready_requires_checker_and_rejects_non_full_statuses() {
        let repo = tempdir();
        let checkers = HashMap::from([("full".to_owned(), "checker.py".to_owned())]);
        let statuses = HashMap::from([
            ("full".to_owned(), "full".to_owned()),
            ("partial".to_owned(), "partial".to_owned()),
            ("callback".to_owned(), "callback".to_owned()),
        ]);
        assert_eq!(
            credit_ready(repo.path(), "missing", Some(&checkers), Some(&statuses)),
            (false, Some("no registered checker".to_owned()))
        );
        assert_eq!(
            credit_ready(repo.path(), "partial", Some(&checkers), Some(&statuses)),
            (false, Some("no registered checker".to_owned()))
        );
        let checkers = HashMap::from([
            ("full".to_owned(), "checker.py".to_owned()),
            ("partial".to_owned(), "checker.py".to_owned()),
            ("callback".to_owned(), "checker.py".to_owned()),
            ("unknown".to_owned(), "checker.py".to_owned()),
        ]);
        assert_eq!(
            credit_ready(repo.path(), "full", Some(&checkers), Some(&statuses)),
            (true, None)
        );
        assert_eq!(
            credit_ready(repo.path(), "partial", Some(&checkers), Some(&statuses)),
            (
                false,
                Some("model status partial in docs/re/partial_callee_models.csv".to_owned())
            )
        );
        assert_eq!(
            credit_ready(repo.path(), "unknown", Some(&checkers), Some(&statuses)),
            (true, None)
        );
        assert_eq!(
            credit_ready(repo.path(), "callback", Some(&checkers), Some(&statuses)),
            (
                false,
                Some("model status callback in docs/re/partial_callee_models.csv".to_owned())
            )
        );
    }

    #[test]
    fn already_on_main_requires_checker_and_separate_unit_offset_segments() {
        let repo = tempdir();
        let recovered = repo.path().join("src/recovered");
        fs::create_dir_all(&recovered).unwrap();
        fs::write(
            recovered.join("overlay_u20_list_walk_11ac_cpu.c"),
            "/* fixture */\n",
        )
        .unwrap();
        let checkers = HashMap::from([("OVL_0020_11AC".to_owned(), "checker.py".to_owned())]);
        assert_eq!(
            already_on_main(repo.path(), "OVL_0020_11AC", Some(&checkers)),
            Some("already on main (checker.py audits overlay_u20_list_walk_11ac_cpu.c)".to_owned())
        );
        assert_eq!(
            already_on_main(repo.path(), "OVL_0020_0001", Some(&checkers)),
            None
        );
        assert_eq!(already_on_main(repo.path(), "OVL_0020_11AC", None), None);
        assert!(!model_name_matches("overlay_u200_11ac.c", "OVL_0020_11AC"));
        assert!(!model_name_matches("overlay_u20_11ac0.c", "OVL_0020_11AC"));
    }

    #[test]
    fn sync_files_ranked_rows_once_and_counts_completed_filings() {
        let repo = tempdir();
        let store = FakePoolradStore::default();
        let queue = serde_json::json!({"rows": [
            poolrad_row("OVL_0001_0001", 1, Value::Null),
            poolrad_row("OVL_0001_0002", 2, Value::Null),
            poolrad_row("OVL_0001_0003", 3, Value::Null),
            poolrad_row("OVL_0001_0004", 4, Value::Null),
            poolrad_row("OVL_0001_0005", 5, Value::Null)
        ]});
        let exporter = |_: &Path| Ok(queue.clone());
        let sync = PoolradSync::new(&store)
            .with_repo(repo.path())
            .with_exporter(&exporter);

        let first = sync.run_with(2, false, "medium").unwrap();
        assert_eq!(first["filed"].as_array().unwrap().len(), 2);
        assert!(store.tasks.lock().unwrap()[0]["title"]
            .as_str()
            .unwrap()
            .starts_with(TITLE_PREFIX));
        assert_eq!(
            store.tasks.lock().unwrap()[0]["assignees"],
            serde_json::json!([])
        );
        let second = sync.run_with(2, false, "medium").unwrap();
        assert_eq!(
            second["filed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["OVL_0001_0003", "OVL_0001_0004"]
        );
        let mut tasks = store.tasks.lock().unwrap();
        assert_eq!(tasks.len(), 4);
        for task in tasks.iter_mut() {
            task["status"] = Value::String("done".to_owned());
        }
        drop(tasks);
        let third = sync.run_with(2, false, "medium").unwrap();
        assert_eq!(third["filed"].as_array().unwrap().len(), 1);
        assert_eq!(third["skipped"]["already_filed"], 4);
        assert_eq!(
            third["skipped"]["already_filed_tasks"]
                .as_object()
                .unwrap()
                .len(),
            4
        );
    }

    #[test]
    fn sync_two_limited_passes_file_only_the_remainder() {
        let repo = tempdir();
        let store = FakePoolradStore::default();
        let queue = serde_json::json!({"rows": [
            poolrad_row("OVL_0001_0001", 1, Value::Null),
            poolrad_row("OVL_0001_0002", 2, Value::Null),
            poolrad_row("OVL_0001_0003", 3, Value::Null),
            poolrad_row("OVL_0001_0004", 4, Value::Null),
            poolrad_row("OVL_0001_0005", 5, Value::Null)
        ]});
        let exporter = |_: &Path| Ok(queue.clone());
        let sync = PoolradSync::new(&store)
            .with_repo(repo.path())
            .with_exporter(&exporter);
        let first = sync.run_with(2, false, "medium").unwrap();
        assert_eq!(
            first["filed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["OVL_0001_0001", "OVL_0001_0002"]
        );
        let second = sync.run_with(10, false, "medium").unwrap();
        assert_eq!(
            second["filed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["OVL_0001_0003", "OVL_0001_0004", "OVL_0001_0005"]
        );
        assert_eq!(store.tasks.lock().unwrap().len(), 5);
    }

    #[test]
    fn sync_exporter_failure_files_nothing_and_reports_the_error() {
        let repo = tempdir();
        let store = FakePoolradStore::default();
        let exporter = |_: &Path| Err("exporter exploded".to_owned());
        let report = PoolradSync::new(&store)
            .with_repo(repo.path())
            .with_exporter(&exporter)
            .run()
            .unwrap();
        assert!(report["filed"].as_array().unwrap().is_empty());
        assert_eq!(report["errors"][0], "exporter exploded");
        assert!(report["base"].is_null());
        assert!(store.tasks.lock().unwrap().is_empty());
    }

    #[test]
    fn sync_retry_skips_successes_after_a_partial_task_write_failure() {
        let repo = tempdir();
        let store = FakePoolradStore::default();
        *store.fail_on_attempt.lock().unwrap() = Some(2);
        let queue = serde_json::json!({"rows": [
            poolrad_row("OVL_0001_0001", 1, Value::Null),
            poolrad_row("OVL_0001_0002", 2, Value::Null),
            poolrad_row("OVL_0001_0003", 3, Value::Null)
        ]});
        let exporter = |_: &Path| Ok(queue.clone());
        let sync = PoolradSync::new(&store)
            .with_repo(repo.path())
            .with_exporter(&exporter);
        let partial = sync.run_with(3, false, "medium").unwrap();
        assert_eq!(
            partial["filed"]
                .as_array()
                .unwrap()
                .iter()
                .map(|entry| entry["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["OVL_0001_0001", "OVL_0001_0003"]
        );
        assert_eq!(
            partial["errors"][0],
            "OVL_0001_0002: fixture task write failed"
        );
        let retry = sync.run_with(3, false, "medium").unwrap();
        assert_eq!(retry["filed"][0]["id"], "OVL_0001_0002");
        assert_eq!(store.tasks.lock().unwrap().len(), 3);
    }

    #[test]
    fn sync_preserves_exporter_closures_control_rows_and_dry_run() {
        let repo = tempdir();
        let store = FakePoolradStore::default();
        let mut main_model = poolrad_row(
            "OVL_0001_0002",
            2,
            serde_json::json!({"status":"main_model","main_files":["src/recovered/x.c"]}),
        );
        main_model["rank"] = Value::from(2);
        let queue = serde_json::json!({"rows": [
            poolrad_row(
                "OVL_0001_0001",
                1,
                serde_json::json!({"status":"inline_in_strict"})
            ),
            main_model,
            poolrad_row(
                "OVL_0001_0003",
                3,
                serde_json::json!({"status":"source_mention"})
            )
        ]});
        let exporter = |_: &Path| Ok(queue.clone());
        let sync = PoolradSync::new(&store)
            .with_repo(repo.path())
            .with_exporter(&exporter);
        let filtered = sync.run_with(5, false, "medium").unwrap();
        assert_eq!(filtered["filed"][0]["id"], "OVL_0001_0003");
        assert!(filtered["skipped"]["closed"]["OVL_0001_0001"]
            .as_str()
            .unwrap()
            .contains("inline_in_strict"));
        assert!(filtered["body_on_main"]["OVL_0001_0002"]["why_not"]
            .as_str()
            .unwrap()
            .contains("checker"));

        let dry_store = FakePoolradStore::default();
        let dry = PoolradSync::new(&dry_store)
            .with_repo(repo.path())
            .with_exporter(&exporter)
            .run_with(5, true, "medium")
            .unwrap();
        assert_eq!(dry["filed"].as_array().unwrap().len(), 1);
        assert_eq!(dry["filed"][0]["dry"], true);
        assert!(dry_store.tasks.lock().unwrap().is_empty());
    }

    #[test]
    fn sync_records_credit_ready_models_and_keeps_partial_models_authorable() {
        let repo = tempdir();
        model_git_fixture(repo.path(), "overlay_u20_list_walk_11ac_cpu.c", Some(1433));
        let checker = repo.path().join("tools/overlay/check_model.py");
        fs::create_dir_all(checker.parent().unwrap()).unwrap();
        fs::write(&checker, "CHECKER_ROWS = ['OVL_0020_11AC']\n").unwrap();
        let statuses = repo.path().join("docs/re/partial_callee_models.csv");
        fs::create_dir_all(statuses.parent().unwrap()).unwrap();
        fs::write(&statuses, "id,status\nOVL_0020_11AC,full\n").unwrap();
        let state = tempdir();
        let credit_path = state.path().join("credit-pending.json");
        let queue = serde_json::json!({"rows":[poolrad_row(
            "OVL_0020_11AC", 1, Value::Null
        )]});
        let exporter = |_: &Path| Ok(queue.clone());
        let store = FakePoolradStore::default();
        let report = PoolradSync::new(&store)
            .with_repo(repo.path())
            .with_exporter(&exporter)
            .with_credit_path(&credit_path)
            .run()
            .unwrap();
        assert!(report["filed"].as_array().unwrap().is_empty());
        assert_eq!(report["credit_pending"]["OVL_0020_11AC"]["pr"], 1433);
        assert!(credit_path.exists());
        assert_eq!(credit_pending(Some(&credit_path)).len(), 1);
        assert!(store.tasks.lock().unwrap().is_empty());

        fs::write(&statuses, "id,status\nOVL_0020_11AC,partial\n").unwrap();
        let partial_store = FakePoolradStore::default();
        let partial = PoolradSync::new(&partial_store)
            .with_repo(repo.path())
            .with_exporter(&exporter)
            .with_credit_path(state.path().join("partial-credit-pending.json"))
            .run()
            .unwrap();
        assert_eq!(partial["filed"][0]["id"], "OVL_0020_11AC");
        assert_eq!(partial["credit_pending"], serde_json::json!({}));
        assert!(
            partial["models_not_credit_ready"]["OVL_0020_11AC"]["why_not"]
                .as_str()
                .unwrap()
                .contains("partial")
        );
    }

    #[test]
    fn sync_end_to_end_body_on_main_needs_checker_while_checker_alone_stays_authorable() {
        let repo = tempdir();
        model_git_fixture(repo.path(), "overlay_u20_list_walk_11ac_cpu.c", Some(1433));
        let queue = serde_json::json!({"rows":[poolrad_row(
            "OVL_0020_11AC", 1,
            serde_json::json!({"status":"main_model", "main_files":["src/recovered/overlay_u20_list_walk_11ac_cpu.c"]})
        )]});
        let exporter = |_: &Path| Ok(queue.clone());
        let state = tempdir();
        let credit_path = state.path().join("credit-pending.json");

        let with_body = FakePoolradStore::default();
        let report = PoolradSync::new(&with_body)
            .with_repo(repo.path())
            .with_exporter(&exporter)
            .with_credit_path(&credit_path)
            .run()
            .unwrap();
        assert!(report["filed"].as_array().unwrap().is_empty());
        assert!(report["body_on_main"]["OVL_0020_11AC"]["why_not"]
            .as_str()
            .unwrap()
            .contains("checker"));
        assert!(with_body.tasks.lock().unwrap().is_empty());

        fs::remove_file(
            repo.path()
                .join("src/recovered/overlay_u20_list_walk_11ac_cpu.c"),
        )
        .unwrap();
        let checker = repo.path().join("tools/overlay/check_model.py");
        fs::create_dir_all(checker.parent().unwrap()).unwrap();
        fs::write(&checker, "CHECKER_ROWS = ['OVL_0020_11AC']\n").unwrap();
        let checker_queue = serde_json::json!({"rows":[poolrad_row(
            "OVL_0020_11AC", 1, serde_json::json!({"status":"source_mention"})
        )]});
        let checker_exporter = |_: &Path| Ok(checker_queue.clone());
        let checker_only = FakePoolradStore::default();
        let report = PoolradSync::new(&checker_only)
            .with_repo(repo.path())
            .with_exporter(&checker_exporter)
            .with_credit_path(state.path().join("checker-only-credit.json"))
            .run()
            .unwrap();
        assert_eq!(report["filed"][0]["id"], "OVL_0020_11AC");
        assert!(report["body_on_main"].as_object().unwrap().is_empty());
        assert_eq!(checker_only.tasks.lock().unwrap().len(), 1);
    }

    #[test]
    fn sync_dry_run_reports_closed_and_body_on_main_credit_without_writes() {
        let repo = tempdir();
        model_git_fixture(repo.path(), "overlay_u20_list_walk_11ac_cpu.c", Some(1433));
        fixture_git(repo.path(), &["checkout", "--quiet", "-b", "side-credit"]);
        let credit_model = repo.path().join("src/recovered/overlay_u20_11ad_cpu.c");
        fs::write(&credit_model, "/* fixture model */\n").unwrap();
        fixture_git(repo.path(), &["add", "-A"]);
        fixture_git(
            repo.path(),
            &["commit", "--quiet", "-m", "add credit model"],
        );
        fixture_git(repo.path(), &["checkout", "--quiet", "main"]);
        fixture_git(
            repo.path(),
            &[
                "merge",
                "--quiet",
                "--no-ff",
                "-m",
                "Merge pull request #1434 from fixture/model",
                "side-credit",
            ],
        );
        let checker = repo.path().join("tools/overlay/check_model.py");
        fs::create_dir_all(checker.parent().unwrap()).unwrap();
        fs::write(&checker, "CHECKER_ROWS = ['OVL_0020_11AD']\n").unwrap();
        let dispositions = repo.path().join("docs/re");
        fs::create_dir_all(&dispositions).unwrap();
        fs::write(
            dispositions.join("OVL_0068_0294_DISPOSITION.md"),
            "closed\n",
        )
        .unwrap();
        let queue = serde_json::json!({"rows":[
            poolrad_row("OVL_0068_0294", 1, Value::Null),
            poolrad_row("OVL_0020_11AC", 2, serde_json::json!({"status":"main_model", "main_files":["src/recovered/overlay_u20_list_walk_11ac_cpu.c"]})),
            poolrad_row("OVL_0020_11AD", 3, Value::Null)
        ]});
        let exporter = |_: &Path| Ok(queue.clone());
        let state = tempdir();
        let credit_path = state.path().join("credit-pending.json");
        let store = FakePoolradStore::default();
        let report = PoolradSync::new(&store)
            .with_repo(repo.path())
            .with_exporter(&exporter)
            .with_credit_path(&credit_path)
            .run_with(5, true, "medium")
            .unwrap();
        assert!(report["filed"].as_array().unwrap().is_empty());
        assert!(report["skipped"]["closed"]["OVL_0068_0294"]
            .as_str()
            .unwrap()
            .contains("DISPOSITION"));
        assert!(report["skipped"]["closed"]["OVL_0020_11AC"]
            .as_str()
            .unwrap()
            .contains("checker"));
        assert_eq!(report["credit_pending"]["OVL_0020_11AD"]["pr"], 1434);
        assert!(!credit_path.exists());
        assert!(store.tasks.lock().unwrap().is_empty());
    }

    #[test]
    fn progress_reads_queue_summary_git_manifest_identity_and_credit_count() {
        let repo = tempdir();
        model_git_fixture(repo.path(), "overlay_u20_list_walk_11ac_cpu.c", None);
        let manifest = repo.path().join(STRICT_MANIFEST);
        fs::create_dir_all(manifest.parent().unwrap()).unwrap();
        fs::write(&manifest, "id,name\nOVL_0001_0001,fixture\n").unwrap();
        let identities = repo.path().join(IDENTITIES);
        fs::create_dir_all(identities.parent().unwrap()).unwrap();
        fs::write(
            &identities,
            "id,name\nOVL_0001_0001,fixture\nOVL_0001_0002,fixture\n",
        )
        .unwrap();
        let state = tempdir();
        let queue_path = state.path().join("strict-queue.json");
        fs::write(
            &queue_path,
            "{\"summary\":{\"composable_bodies\":7,\"candidates\":4}}",
        )
        .unwrap();
        let credit_path = state.path().join("credit-pending.json");
        let entries = serde_json::from_value::<Map<String, Value>>(serde_json::json!({
            "OVL_0001_0001":{"model":"m.c","commit":"abc","pr":1}
        }))
        .unwrap();
        record_credit_pending(&entries, Some(&credit_path))
            .unwrap()
            .unwrap();

        let summary = progress(Some(repo.path()), Some(&queue_path), Some(&credit_path)).unwrap();
        assert_eq!(summary["credited"], 1);
        assert_eq!(summary["identities"], 2);
        assert_eq!(summary["ceiling"], 815);
        assert_eq!(summary["composable_bodies"], 7);
        assert_eq!(summary["queue_candidates"], 4);
        assert!(summary["main"].as_str().unwrap().len() <= 7);
        assert!(summary["queue_exported_at"].is_number());
        assert_eq!(summary["credit_pending"], 1);

        let missing = progress(
            Some(repo.path()),
            Some(&state.path().join("missing.json")),
            Some(&state.path().join("missing-credit.json")),
        )
        .unwrap();
        assert!(missing["composable_bodies"].is_null());
        assert!(missing["queue_candidates"].is_null());
        assert!(missing["queue_exported_at"].is_null());
        assert_eq!(missing["credit_pending"], 0);
    }
}
