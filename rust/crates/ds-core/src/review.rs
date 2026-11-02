//! `review` (T52): pure identity, attribution, row-selection, and accounting helpers
//! recovered from `dunk_review.py`.
//!
//! These functions operate on supplied task, PR, and ledger data. Service calls and
//! production writes remain outside this module. The accounting gates below take the
//! table text read by Python and preserve its refusal text byte-for-byte.

use serde_json::Value;
use std::collections::BTreeSet;

pub type ReviewPrKey = (Option<Value>, Option<Value>);
pub type JudgedHeadKey = (Option<Value>, Option<Value>, Option<Value>);
pub type FinalVerdict = (bool, Vec<String>, Vec<String>, usize);

/// The exact task query used by the review timer, injectable for deterministic tests.
pub trait ReviewTaskStore {
    type Error;

    fn list(
        &self,
        scope: Option<&str>,
        include_done: bool,
        limit: usize,
        kind: &str,
    ) -> Result<Vec<Value>, Self::Error>;
}

/// The one GitHub policy query this pure review slice needs. Production callers
/// can adapt the API/config source; tests supply a fixture implementation.
pub trait ReviewPolicy {
    fn required_verdicts(&self, repo: &str) -> usize;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewPull {
    pub repo: String,
    pub number: i64,
    pub head: String,
    pub title: String,
    pub url: String,
    pub branch: String,
    pub base: String,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewDetail {
    pub additions: Option<i64>,
    pub deletions: Option<i64>,
    pub changed_files: Option<i64>,
    pub body: Option<String>,
}

/// Rust counterpart of `Sync`: dependencies are supplied at construction so
/// these helpers never create credentials, make requests, or touch a live store.
pub struct ReviewSync<S, T, G> {
    pub flock: S,
    pub tokens: T,
    pub github: G,
}

impl<S, G: Default> ReviewSync<S, (), G> {
    /// Missing tokens and GitHub adapters use their injected defaults, matching
    /// Python's `tokens or Tokens()` and `github=dunk_github` constructor values.
    pub fn new(flock: S, github: Option<G>) -> Self {
        Self {
            flock,
            tokens: (),
            github: github.unwrap_or_default(),
        }
    }
}

impl<S, T, G> ReviewSync<S, T, G> {
    /// Construct with an explicitly supplied token source.
    pub fn with_tokens(flock: S, tokens: T, github: G) -> Self {
        Self {
            flock,
            tokens,
            github,
        }
    }
}

impl<S: ReviewTaskStore, T, G: ReviewPolicy> ReviewSync<S, T, G> {
    /// Build the live review map, retaining Python's task query and last-value
    /// replacement behavior while keeping first-insertion order.
    pub fn _live_reviews(&self) -> Result<Vec<(ReviewPrKey, Value)>, S::Error> {
        let tasks = self.flock.list(None, true, 1000, "review")?;
        let mut by_pr: Vec<(ReviewPrKey, Value)> = Vec::new();
        for task in tasks {
            let status = task
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(status, "open" | "in_progress" | "blocked") {
                continue;
            }
            let key = (optional_value(&task, "repo"), optional_value(&task, "pr"));
            insert_replacing(&mut by_pr, key, task);
        }
        Ok(by_pr)
    }

    /// Build the already-judged map. A done review with a falsey verdict is not
    /// a judgment; duplicate keys keep the last task as Python's dict does.
    pub fn _judged_heads(&self) -> Result<Vec<(JudgedHeadKey, Value)>, S::Error> {
        let tasks = self.flock.list(None, true, 1000, "review")?;
        let mut out: Vec<(JudgedHeadKey, Value)> = Vec::new();
        for task in tasks {
            if task.get("status").and_then(Value::as_str) != Some("done")
                || !task.get("verdict").is_some_and(python_truthy)
            {
                continue;
            }
            let key = (
                optional_value(&task, "repo"),
                optional_value(&task, "pr"),
                optional_value(&task, "head"),
            );
            insert_replacing(&mut out, key, task);
        }
        Ok(out)
    }

    /// Distinct completed reviewer lanes at one repo/head, with an optional PR
    /// filter. Returned lists use Python's sorted-set order.
    pub fn head_verdicts(
        &self,
        repo: &str,
        head: &str,
        number: Option<i64>,
    ) -> Result<(Vec<String>, Vec<String>), S::Error> {
        let tasks = self.flock.list(None, true, 1000, "review")?;
        let mut accepts = BTreeSet::new();
        let mut needs_fix = BTreeSet::new();
        for task in tasks {
            if task.get("repo").and_then(Value::as_str) != Some(repo)
                || task.get("head").and_then(Value::as_str) != Some(head)
                || number.is_some_and(|n| task.get("pr").and_then(Value::as_i64) != Some(n))
                || task.get("status").and_then(Value::as_str) != Some("done")
            {
                continue;
            }
            let Some(lane) = task.get("completed_by").filter(|lane| python_truthy(lane)) else {
                continue;
            };
            let lane = python_string(lane);
            match task.get("verdict").and_then(Value::as_str) {
                Some("accept") => {
                    accepts.insert(lane);
                }
                Some("needs-fix") => {
                    needs_fix.insert(lane);
                }
                _ => {}
            }
        }
        Ok((
            accepts.into_iter().collect(),
            needs_fix.into_iter().collect(),
        ))
    }

    /// Whether a head is final and the distinct-lane evidence used to decide.
    pub fn _is_final(
        &self,
        repo: &str,
        head: &str,
        number: Option<i64>,
        lane: Option<&str>,
    ) -> Result<FinalVerdict, S::Error> {
        let (mut accepts, needs_fix) = self.head_verdicts(repo, head, number)?;
        if let Some(lane) = lane.filter(|lane| !lane.is_empty()) {
            if accepts
                .binary_search_by(|candidate| candidate.as_str().cmp(lane))
                .is_err()
            {
                accepts.push(lane.to_string());
                accepts.sort();
            }
        }
        let required = self.github.required_verdicts(repo).max(1);
        let final_verdict = needs_fix.is_empty() && accepts.len() >= required;
        Ok((final_verdict, accepts, needs_fix, required))
    }
}

fn optional_value(task: &Value, key: &str) -> Option<Value> {
    task.get(key).filter(|value| !value.is_null()).cloned()
}

fn insert_replacing<K: PartialEq, V>(entries: &mut Vec<(K, V)>, key: K, value: V) {
    if let Some(index) = entries.iter().position(|(existing, _)| existing == &key) {
        entries[index].1 = value;
    } else {
        entries.push((key, value));
    }
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_i64() != Some(0) && value.as_f64() != Some(0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

fn python_string(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::String(value) => value.clone(),
        _ => value.to_string(),
    }
}

/// Python counterpart of `review_body`; truncation counts Unicode characters,
/// matching Python string slicing rather than byte slicing.
pub fn review_body(pr: &ReviewPull, detail: &ReviewDetail, author: Option<&str>) -> String {
    let author = author
        .filter(|author| !author.is_empty())
        .unwrap_or("unknown (not attributable; say so in the verdict)");
    let mut lines = vec![
        format!("REVIEW {}#{} @ {}", pr.repo, pr.number, pr.head),
        format!("title: {}", pr.title),
        format!("url: {}", pr.url),
        format!("branch: {} -> {}", pr.branch, pr.base),
        format!("author lane: {author}"),
        format!(
            "opened: {}  updated: {}",
            pr.created_at.as_deref().unwrap_or("None"),
            pr.updated_at.as_deref().unwrap_or("None")
        ),
        format!(
            "diff: +{} -{} in {} files",
            detail
                .additions
                .map_or_else(|| "None".to_string(), |n| n.to_string()),
            detail
                .deletions
                .map_or_else(|| "None".to_string(), |n| n.to_string()),
            detail
                .changed_files
                .map_or_else(|| "None".to_string(), |n| n.to_string())
        ),
        String::new(),
        "Verdict: `dunkingsheep task complete <id> --verdict accept|needs-fix` from your"
            .to_string(),
        "reviewer lane, with the evidence in `task note` first: commands run, what was".to_string(),
        "reproduced from the media, what was NOT verified. A verdict is bound to this".to_string(),
        "exact head; a new push files a new review.".to_string(),
    ];
    if let Some(body) = detail
        .body
        .as_deref()
        .map(str::trim)
        .filter(|body| !body.is_empty())
    {
        let excerpt = body.chars().take(4000).collect::<String>();
        lines.extend([String::new(), "PR body:".to_string(), excerpt]);
    }
    lines.join("\n")
}

/// Pane id of the lane that opened a PR, or `None` when it cannot be
/// attributed without guessing. `lanes` and `mints` preserve Python mapping
/// iteration order as `(pane id, value)` pairs.
pub fn attribute_author(
    branch: Option<&str>,
    created_at: Option<&str>,
    lanes: &[(String, String)],
    mints: &[(String, String)],
) -> Option<String> {
    let branch = branch.unwrap_or_default().to_lowercase();
    for (pane_id, label) in lanes {
        let label = label.trim().to_lowercase();
        if !label.is_empty()
            && (branch.starts_with(&format!("{label}/"))
                || branch.starts_with(&format!("{label}-")))
        {
            return Some(pane_id.clone());
        }
    }

    let created = _iso(created_at)?;
    let recent: Vec<&String> = mints
        .iter()
        .filter_map(|(pane_id, minted_at)| {
            let minted = _iso(Some(minted_at))?;
            (minted <= created && created - minted < 2 * 60 * 60).then_some(pane_id)
        })
        .collect();
    (recent.len() == 1).then(|| recent[0].clone())
}

/// Attribute a PR to the sole lane found in its work tasks. Store errors and
/// zero or multiple authors remain unattributed, as in the Python helper.
pub fn author_from_tasks<E>(panes: Result<Vec<String>, E>) -> Option<String> {
    let mut panes = panes.ok()?.into_iter();
    let pane = panes.next()?;
    panes.next().is_none().then_some(pane)
}

/// Parse the strict UTC timestamp format accepted by `time.strptime` in the
/// Python review helper and return Unix seconds.
pub fn _iso(value: Option<&str>) -> Option<i64> {
    let value = value?;
    chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%SZ")
        .ok()
        .map(|time| time.and_utc().timestamp())
}

/// Every full row id named in `text`, in order of first appearance, deduped.
pub fn row_ids(text: Option<&str>) -> Vec<String> {
    let bytes = text.unwrap_or_default().as_bytes();
    let mut ids = Vec::new();
    let mut index = 0;
    while index + 13 <= bytes.len() {
        let start = index;
        let kind = &bytes[start..start + 3];
        let shape = (kind == b"OVL" || kind == b"RES")
            && bytes[start + 3] == b'_'
            && bytes[start + 8] == b'_'
            && bytes[start + 4..start + 8]
                .iter()
                .all(u8::is_ascii_hexdigit)
            && bytes[start + 9..start + 13]
                .iter()
                .all(u8::is_ascii_hexdigit)
            && bytes[start + 4..start + 13]
                .iter()
                .all(|byte| !byte.is_ascii_lowercase());
        let before_ok = start == 0 || !is_row_word(bytes[start - 1]);
        let after = start + 13;
        let after_ok = after == bytes.len() || !is_row_word(bytes[after]);
        if shape && before_ok && after_ok {
            let ident = String::from_utf8(bytes[start..after].to_vec()).unwrap();
            if !ids.contains(&ident) {
                ids.push(ident);
            }
            index = after;
        } else {
            index += 1;
        }
    }
    ids
}

fn is_row_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The final path component with its last extension removed.
pub fn stem_of(path: &str) -> String {
    let name = path.rsplit('/').next().unwrap_or_default();
    name.rsplit_once('.')
        .map_or(name, |(stem, _)| stem)
        .to_string()
}

/// Whether a row id is named by this evidence document's stem.
pub fn own_row(row: Option<&str>, stem: &str) -> bool {
    let Some(row) = row else { return false };
    let bytes = row.as_bytes();
    if bytes.len() != 13
        || !(bytes.starts_with(b"OVL_") || bytes.starts_with(b"RES_"))
        || bytes[8] != b'_'
        || !bytes[4..8].iter().all(u8::is_ascii_hexdigit)
        || !bytes[9..13].iter().all(u8::is_ascii_hexdigit)
        || bytes[4..13].iter().any(u8::is_ascii_lowercase)
    {
        return false;
    }
    let parts: Vec<String> = stem.to_lowercase().split('_').map(str::to_string).collect();
    let offset = row[9..13].to_lowercase();
    let unit = row[4..8].to_lowercase();
    let short_unit = format!("u{}", unit.trim_start_matches('0'));
    let short_unit = if short_unit == "u" {
        "u0".to_string()
    } else {
        short_unit
    };
    parts.contains(&offset) && (parts.contains(&short_unit) || parts.contains(&unit))
}

/// `(id, evidence_doc)` pairs from an `id,evidence_doc` ledger, header skipped.
pub fn ledger_rows(text: Option<&str>) -> Vec<(String, String)> {
    text.unwrap_or_default()
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .filter_map(|line| {
            let (ident, doc) = line.split_once(',')?;
            Some((ident.trim().to_string(), doc.trim().to_string()))
        })
        .collect()
}

/// Insert a ledger row in sorted position while preserving LF or CRLF endings.
pub fn with_row(text: Option<&str>, ident: &str, doc: &str) -> Result<String, &'static str> {
    let text = text.unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    if lines.is_empty() {
        return Err("the ledger is empty: no header line to keep");
    }
    let sep = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let header = lines.remove(0);
    let insertion = lines
        .partition_point(|line| line.split_once(',').map_or(line.as_str(), |(key, _)| key) < ident);
    lines.insert(insertion, format!("{ident},{doc}"));
    let mut result = std::iter::once(header)
        .chain(lines)
        .collect::<Vec<_>>()
        .join(sep);
    if text.ends_with('\n') || text.ends_with('\r') {
        result.push_str(sep);
    }
    Ok(result)
}

/// Select the single ledger row an accepted body earns, or return its refusal reason.
pub fn choose_row(
    sources: &[(&str, Option<&str>)],
    added_docs: &[&str],
    ledger_text: Option<&str>,
) -> (Option<(String, String)>, String) {
    let mut ident = None;
    for (label, text) in sources {
        let ids = row_ids(*text);
        if ids.len() == 1 {
            ident = Some(ids[0].clone());
            break;
        }
        if ids.len() > 1 {
            return (
                None,
                format!(
                    "the {label} names {} row ids ({}); one ledger row is one id",
                    ids.len(),
                    ids.join(", ")
                ),
            );
        }
    }
    let Some(ident) = ident else {
        return (
            None,
            "no row id (OVL_xxxx_yyyy / RES_xxxx_yyyy) is named in the verdict notes, the review task title or the PR body".to_string(),
        );
    };

    let mut existing = std::collections::BTreeMap::new();
    for (row, doc) in ledger_rows(ledger_text) {
        existing.insert(row, doc);
    }
    if let Some(doc) = existing.get(&ident) {
        return (None, format!("{ident} is already credited as {doc}"));
    }

    let matched: Vec<&str> = added_docs
        .iter()
        .copied()
        .filter(|doc| own_row(Some(&ident), &stem_of(doc)))
        .collect();
    match matched.as_slice() {
        [] => {
            (
                None,
                format!(
                "{ident}: no evidence document the PR adds carries its unit and offset (added: {})",
                if added_docs.is_empty() { "none".to_string() } else { added_docs.join(", ") }
            ),
            )
        }
        [doc] => (Some((ident, (*doc).to_string())), String::new()),
        _ => {
            let mut docs = matched;
            docs.sort_unstable();
            (
                None,
                format!(
                    "{ident}: {} added documents match it ({}); refusing to pick one",
                    docs.len(),
                    docs.join(", ")
                ),
            )
        }
    }
}

/// Ids whose STATUS column is `partial` in `partial_callee_models.csv`. The status is the
/// discriminator, not presence: on main 49 of the 50 `full` rows are already strict
/// credits, so only `partial` may be refused. The header line is skipped.
pub fn partial_ids(text: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for line in text.lines().skip(1) {
        let fields: Vec<&str> = line.split(',').collect();
        let name = fields[0].trim();
        let status = fields
            .get(1)
            .map(|s| s.trim().to_lowercase())
            .unwrap_or_default();
        if !name.is_empty() && status == "partial" {
            ids.insert(name.to_string());
        }
    }
    ids
}

/// Ids carrying an evidence-kind label in `credit_evidence_kind.csv`: the first column of
/// every non-blank line after the header.
pub fn labelled_ids(text: &str) -> BTreeSet<String> {
    text.lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .map(|line| line.split(',').next().unwrap_or("").trim().to_string())
        .collect()
}

/// GATE 2: a partial chain is not a strict full-C credit (poolrad#1637 credited
/// OVL_0038_0775, a partial model stopped at +0058, into the strict ledger). Returns the
/// refusal text when `ident` is partial.
pub fn partial_gate(ident: &str, partial_table: &str, partial_text: &str) -> Option<String> {
    partial_ids(partial_text).contains(ident).then(|| {
        format!(
            "{ident} is listed as a PARTIAL model in {partial_table}; a strict \
             full-C credit is not owed for it. Credit it in the partial ledger \
             when its chain closes, or close the gap first."
        )
    })
}

/// GATE 3: the strict table also requires an evidence-kind label, and that table is
/// GENERATED, so the automation refuses and names the tool rather than inventing a label.
///
/// `kind_text` is `None` when the table could not be read; the Python then records the
/// read error and does NOT refuse (it fails open here), and so does this.
pub fn kind_gate(ident: &str, kind_table: &str, kind_text: Option<&str>) -> Option<String> {
    let text = kind_text?;
    if labelled_ids(text).contains(ident) {
        return None;
    }
    Some(format!(
        "{ident} has no row in {kind_table}, which every strict credit \
         needs; that table is generated by \
         tools/overlay/credit_evidence_kind.py --write. The automation does \
         not guess the label - regenerate the table in the accounting branch \
         and re-run."
    ))
}

#[cfg(test)]
#[path = "review_tests.rs"]
mod review_tests;
