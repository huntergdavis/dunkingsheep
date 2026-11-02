//! `review` (T52): pure identity, attribution, row-selection, and accounting helpers
//! recovered from `dunk_review.py`.
//!
//! These functions operate on supplied task, PR, and ledger data. Service calls and
//! production writes remain outside this module. The accounting gates below take the
//! table text read by Python and preserve its refusal text byte-for-byte.

use std::collections::BTreeSet;

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
