//! Retired panes: the ones we deliberately stopped feeding (T16 slice).
//!
//! A pane can be superseded (a replacement was started elsewhere) or replaced by
//! machinery (the dispatcher took over a PM's job). Its mailbox is then mail nobody will
//! ever read, and paging about it every four hours is an alarm that cannot be satisfied -
//! the failure mode we keep removing.
//!
//! Retiring is BOOKKEEPING, not deletion: the mail stays as the record of what was sent,
//! the pane stops counting as idle or ready, and the fact that it is retired is reported
//! instead of silently vanishing.
//!
//! Ported from `dunk_panes.py`. The Python writes with `mkstemp`-style temp + fsync +
//! rename under an `fcntl` lock; here that is [`crate::atomic_json::write`] (temp in the
//! same directory, fsync, rename, mode 0600) plus [`crate::lock`] for the exclusive
//! guard, which is the same two properties.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::atomic_json;
use crate::clock;
use crate::error::DunkError;
use crate::lock::{Guard, Mode};

/// A named refusal, the Python's `RetireError`.
pub fn refusal(text: impl Into<String>) -> DunkError {
    DunkError::Refused(text.into())
}

/// One retired pane, exactly the fields the Python stores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Retired {
    pub pane_id: String,
    pub reason: String,
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    /// MAILBOXES, not only labels: mail addressed to a dead pane is filed under a NAME
    /// ('crackdown-codex'), which is neither a pane id nor always the tab label. A retire
    /// that recorded only labels left those mailboxes alerting and unarchived.
    #[serde(default)]
    pub mailboxes: Vec<String>,
    pub retired_at: f64,
}

/// The store: one JSON file, keyed by pane id.
#[derive(Debug, Clone)]
pub struct RetiredPanes {
    pub path: PathBuf,
}

impl RetiredPanes {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        RetiredPanes { path: path.into() }
    }

    /// `retired-panes.json` under the config directory.
    pub fn at_config(config_dir: &Path) -> Self {
        RetiredPanes::new(config_dir.join("retired-panes.json"))
    }

    fn load(&self) -> Result<BTreeMap<String, Retired>, DunkError> {
        // A missing or malformed file reads as empty: retiring is bookkeeping, and a
        // corrupt file must not stop the daemon from starting (the Python's `_load`).
        Ok(atomic_json::read_or_default(&self.path).unwrap_or_default())
    }

    fn save(&self, data: &BTreeMap<String, Retired>) -> Result<(), DunkError> {
        let _guard = self.lock()?;
        atomic_json::write(&self.path, data)
    }

    /// The exclusive guard the Python takes around every write.
    fn lock(&self) -> Result<Guard, DunkError> {
        let mut lock_path = self.path.clone();
        let name = lock_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "retired-panes.json".to_string());
        lock_path.set_file_name(format!("{name}.lock"));
        Guard::acquire(&lock_path, Mode::Exclusive)
    }

    /// Mark a pane retired. The reason is required: an unexplained silence is a bug.
    pub fn retire(
        &self,
        pane_id: &str,
        reason: &str,
        by: Option<&str>,
        labels: &[String],
        mailboxes: &[String],
        now: f64,
    ) -> Result<Retired, DunkError> {
        let pane_id = pane_id.trim();
        if pane_id.is_empty() {
            return Err(refusal("a pane id is required"));
        }
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(refusal("a reason is required (e.g. superseded-by:wW:p7)"));
        }
        let mut data = self.load()?;
        let mut names: Vec<String> = labels
            .iter()
            .map(|label| label.trim().to_string())
            .filter(|label| !label.is_empty())
            .collect();
        names.sort();
        names.dedup();
        let mut boxes: Vec<String> = mailboxes
            .iter()
            .map(|box_name| box_name.trim().to_string())
            .filter(|box_name| !box_name.is_empty())
            .collect();
        boxes.sort();
        boxes.dedup();
        let retired = Retired {
            pane_id: pane_id.to_string(),
            reason: reason.to_string(),
            by: by.map(str::to_string),
            labels: names,
            mailboxes: boxes,
            // (3) The Python does `"retired_at": now or time.time()`, so 0 means
            // "stamp now" - and origin/main's `valid_retirements` refuses a record whose
            // `at` is 0, because an undated retirement must not withdraw identity
            // protection. Writing the 0 through produced a record the consumer rejects,
            // which is worse than either honest option; this stamps the clock instead.
            retired_at: if now > 0.0 { now } else { clock::now_s() },
        };
        data.insert(pane_id.to_string(), retired.clone());
        self.save(&data)?;
        Ok(retired)
    }

    /// The Python's `unretire`: refuse a pane that was not retired, so a typo is not a
    /// silent no-op.
    pub fn unretire(&self, pane_id: &str) -> Result<Retired, DunkError> {
        let mut data = self.load()?;
        let removed = data
            .remove(pane_id)
            .ok_or_else(|| refusal(format!("{pane_id} is not retired")))?;
        self.save(&data)?;
        Ok(removed)
    }

    /// Every retired pane, sorted by id (the Python's `all_retired`).
    pub fn all(&self) -> Result<Vec<Retired>, DunkError> {
        Ok(self.load()?.into_values().collect())
    }

    /// The Python's `str(pane_id or "").strip()`: a pane id is compared the way it is
    /// stored, so `" %110 "` finds `%110` here exactly as it does there.
    ///
    /// Measured (review t985): the first port used `contains_key(pane_id)` with no
    /// trim, so `is_retired(" %110 ")` was `Ok(false)` where Python answers `True`.
    /// A caller that pastes an id out of a log line is the normal case, not an edge.
    fn key(pane_id: &str) -> &str {
        pane_id.trim()
    }

    pub fn is_retired(&self, pane_id: &str) -> Result<bool, DunkError> {
        Ok(self.load()?.contains_key(Self::key(pane_id)))
    }

    /// The reason a pane is retired, for a report that says WHY rather than just "gone".
    pub fn reason_of(&self, pane_id: &str) -> Result<Option<String>, DunkError> {
        Ok(self
            .load()?
            .get(Self::key(pane_id))
            .map(|retired| retired.reason.clone()))
    }
}
