//! t4704: what a blocked task is waiting on, as a KIND read from its block reason.
//!
//! The store keeps one `block_reason` text. Two prefixes already carried meaning:
//! `needs-owner:` (the owner report, `task list --needs-owner`) and `blocked-by:<task>` (the
//! dependency block that completion reopens). This module adds the other waits lanes park
//! tasks on, in the same form, so a block says what it waits for without a schema change:
//!
//! | `--waiting-on`   | stored reason                         | kind       |
//! |------------------|---------------------------------------|------------|
//! | `owner`          | `needs-owner: <why>`                  | `owner`    |
//! | `task:<id>`      | `blocked-by:<id>` (plus a dependency) | `task`     |
//! | `pr:<repo>#<n>`  | `waiting-pr:<repo>#<n>: <why>`        | `pr`       |
//! | `evidence`       | `waiting-evidence: <why>`             | `evidence` |
//! | `release`        | `waiting-release: <why>`              | `release`  |
//! | `numbers`        | `waiting-numbers: <why>`              | `numbers`  |
//!
//! Any other reason, and no reason, is kind `other`.

use crate::error::DunkError;

/// Every kind, in the order reports list them.
pub const KINDS: [&str; 7] = [
    "owner", "task", "pr", "evidence", "release", "numbers", "other",
];

/// The kind of a block reason. Leading spaces are ignored, as the owner report ignores them.
pub fn kind_of(reason: Option<&str>) -> &'static str {
    let reason = reason.unwrap_or("").trim_start();
    for (prefix, kind) in [
        ("needs-owner:", "owner"),
        ("blocked-by:", "task"),
        ("waiting-pr:", "pr"),
        ("waiting-evidence:", "evidence"),
        ("waiting-release:", "release"),
        ("waiting-numbers:", "numbers"),
    ] {
        if reason.starts_with(prefix) {
            return kind;
        }
    }
    "other"
}

/// A parsed `--waiting-on` value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WaitingOn {
    Owner,
    Evidence,
    Release,
    Numbers,
    /// Another task: blocked with a dependency, reopened when that task completes.
    Task(String),
    /// A pull request, `<repo>#<number>`. Recorded; it does not reopen the task by itself yet.
    Pr(String),
}

/// What `--waiting-on` accepts, for the refusal text.
pub const WAITING_ON_CHOICES: &str =
    "owner, evidence, release, numbers, task:<id> or pr:<repo>#<number>";

/// Parse `owner | evidence | release | numbers | task:<id> | pr:<repo>#<number>`.
pub fn parse_waiting_on(text: &str) -> Result<WaitingOn, DunkError> {
    let refuse = || {
        DunkError::Refused(format!(
            "waiting_on must be {WAITING_ON_CHOICES}; got {text:?}"
        ))
    };
    let text = text.trim();
    match text {
        "owner" => return Ok(WaitingOn::Owner),
        "evidence" => return Ok(WaitingOn::Evidence),
        "release" => return Ok(WaitingOn::Release),
        "numbers" => return Ok(WaitingOn::Numbers),
        _ => {}
    }
    if let Some(task) = text.strip_prefix("task:") {
        let task = task.trim();
        if task.is_empty() || task.chars().any(char::is_whitespace) {
            return Err(refuse());
        }
        return Ok(WaitingOn::Task(task.to_string()));
    }
    if let Some(pr) = text.strip_prefix("pr:") {
        let pr = pr.trim();
        let well_formed = pr.rsplit_once('#').is_some_and(|(repo, number)| {
            !repo.is_empty()
                && !repo.chars().any(|c| c.is_whitespace() || c == ':')
                && !number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit())
        });
        if !well_formed {
            return Err(refuse());
        }
        return Ok(WaitingOn::Pr(pr.to_string()));
    }
    Err(refuse())
}

impl WaitingOn {
    /// The reason text stored for this wait. `Task` is stored by `Tasks::block_by`, which
    /// writes `blocked-by:<id>` itself; this is the same text, for callers that display it.
    pub fn reason(&self, why: Option<&str>) -> String {
        let why = why.map(str::trim).unwrap_or("");
        let with = |prefix: &str| {
            if why.is_empty() {
                prefix.to_string()
            } else {
                format!("{prefix} {why}")
            }
        };
        match self {
            WaitingOn::Owner => with("needs-owner:"),
            WaitingOn::Evidence => with("waiting-evidence:"),
            WaitingOn::Release => with("waiting-release:"),
            WaitingOn::Numbers => with("waiting-numbers:"),
            WaitingOn::Task(task) => format!("blocked-by:{task}"),
            WaitingOn::Pr(pr) => with(&format!("waiting-pr:{pr}:")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_waiting_on_value_round_trips_to_its_kind() {
        for (value, why, reason, kind) in [
            (
                "owner",
                Some("which port?"),
                "needs-owner: which port?",
                "owner",
            ),
            (
                "evidence",
                Some("CI on the exact head"),
                "waiting-evidence: CI on the exact head",
                "evidence",
            ),
            (
                "release",
                Some("ships after 4ea47f18"),
                "waiting-release: ships after 4ea47f18",
                "release",
            ),
            (
                "numbers",
                Some("24 h after deploy"),
                "waiting-numbers: 24 h after deploy",
                "numbers",
            ),
            ("task:t12", None, "blocked-by:t12", "task"),
            (
                "pr:owner/repo#42",
                Some("review"),
                "waiting-pr:owner/repo#42: review",
                "pr",
            ),
            ("pr:owner/repo#42", None, "waiting-pr:owner/repo#42:", "pr"),
            (" release ", None, "waiting-release:", "release"),
        ] {
            let parsed = parse_waiting_on(value).unwrap();
            assert_eq!(parsed.reason(why), reason, "{value}");
            assert_eq!(kind_of(Some(reason)), kind, "{value}");
        }
    }

    #[test]
    fn free_text_and_no_reason_are_kind_other_and_old_prefixes_keep_their_kind() {
        for reason in [
            None,
            Some(""),
            Some("waiting on CI"),
            Some("DEFERRED"),
            Some("owner said so"),
        ] {
            assert_eq!(kind_of(reason), "other", "{reason:?}");
        }
        assert_eq!(kind_of(Some("needs-owner: pick one")), "owner");
        assert_eq!(kind_of(Some("  needs-owner: pick one")), "owner");
        assert_eq!(kind_of(Some("blocked-by:t3")), "task");
        assert!(KINDS.contains(&kind_of(Some("waiting-numbers:"))));
    }

    #[test]
    fn a_malformed_waiting_on_is_refused_with_the_choices() {
        for bad in [
            "",
            "review",
            "task:",
            "task:t1 t2",
            "pr:",
            "pr:repo",
            "pr:repo#",
            "pr:#4",
            "pr:re po#4",
            "pr:repo#4x",
            "Owner",
        ] {
            let refused = parse_waiting_on(bad).unwrap_err().to_string();
            assert!(refused.contains(WAITING_ON_CHOICES), "{bad}: {refused}");
        }
    }
}
