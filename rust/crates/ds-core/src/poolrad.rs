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
}
