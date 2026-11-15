//! Which mailboxes may be retired (t5014): the rules of the reviewed census, as a dry run.
//!
//! Every mailbox gets one class:
//!
//! * `active`: a live pane, or a name a live pane carries;
//! * `human`, `vp`: the owner's mailbox, and VP, chief-of-staff and desk mailboxes, by name;
//! * `quarantine`: the daemon's `unrouted:*` mailboxes;
//! * `ambiguous`: anything that may still be in use: a name in the role or lane registry, a
//!   role-like name (PM, coordinator, desk, lead), a comma-joined composite name, a name with
//!   spaces (a nickname), a bare agent-type word or number, unread mail, an open PR wait, or mail
//!   in the last 14 days;
//! * `retired`: none of the above.
//!
//! Only `retired` is a candidate. This module decides nothing on its own: it is a report.
//! Names are compared the way the inbox compares them (normalised, case-folded).

use std::collections::BTreeMap;

use serde_json::{json, Value};

use crate::inbox_store::mailbox_status::{CensusRow, QUIET_S};
use crate::unicode_casefold::full_casefold;

/// Owner mailboxes: never candidates.
const HUMAN_NAMES: [&str; 3] = ["owner", "human", "you"];
/// VP, chief-of-staff and desk mailboxes, matched as a whole name or a name prefix.
const VP_PREFIXES: [&str; 6] = [
    "vp",
    "codex-rust-owner",
    "cos",
    "chief",
    "front-desk",
    "frontdesk",
];
/// Bare agent-type words: a mailbox named only this is not an identifiable lane.
const AGENT_WORDS: [&str; 5] = ["claude", "codex", "pi", "opencode", "agent"];
/// Words that make a name role-like: a person may still write to it.
const ROLE_WORDS: [&str; 6] = ["pm", "coordinator", "planner", "desk", "lead", "supervisor"];

/// The names that keep a mailbox from retiring.
#[derive(Debug, Clone, Default)]
pub struct Identities {
    /// Live pane ids and every name a live pane carries.
    pub live: Vec<String>,
    /// Pane ids and names in the role registry and the lane registry.
    pub registered: Vec<String>,
}

fn key(name: &str) -> String {
    full_casefold(&crate::inbox::normalise_mailbox(name))
}

fn words(name: &str) -> Vec<String> {
    name.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect()
}

/// One mailbox's class and the reasons for it.
pub fn classify(
    row: &CensusRow,
    live: &[String],
    registered: &[String],
    now: f64,
) -> (&'static str, Vec<String>) {
    let name = key(&row.mailbox);
    if live.contains(&name) {
        return (
            "active",
            vec!["a live pane or a name a live pane carries".into()],
        );
    }
    if HUMAN_NAMES.contains(&name.as_str()) || name.starts_with("owner") {
        return ("human", vec!["the owner's mailbox".into()]);
    }
    // The owner's rule: VP-like names are never candidates, wherever "vp" stands in the name
    // ('poolrad-vp', '<project>-vp', 'VP for Poolrad-decomp', 'vp-claude').
    if words(&name).iter().any(|word| word == "vp")
        || VP_PREFIXES.iter().any(|prefix| {
            name == *prefix
                || words(&name).first().is_some_and(|w| w == prefix)
                || name.starts_with(&format!("{prefix}-"))
        })
    {
        return ("vp", vec!["a VP, chief-of-staff or desk mailbox".into()]);
    }
    // The daemon's quarantine mailboxes (unrouted:*) hold mail nobody could deliver: never
    // candidates.
    if name.starts_with("unrouted:") {
        return (
            "quarantine",
            vec!["a daemon quarantine mailbox (unrouted:*)".into()],
        );
    }
    let mut why = Vec::new();
    if registered.contains(&name) {
        why.push("in the role or lane registry".to_string());
    }
    // A comma-joined composite name ('wW:p1,w9:pH'): its parts may be live or registered ids.
    if name.contains(',') {
        let parts: Vec<&str> = name
            .split(',')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect();
        if parts
            .iter()
            .any(|part| registered.iter().any(|r| r == part) || live.iter().any(|l| l == part))
        {
            why.push("a composite name whose parts are live or registered ids".to_string());
        } else {
            why.push("a composite (comma-joined) name: its parts may be identities".to_string());
        }
    }
    // A free-form name with spaces ('dunky claude') may be a person's nickname.
    if name.chars().any(char::is_whitespace) {
        why.push("a free-form name with spaces: may be a person's nickname".to_string());
    }
    // A bare agent-type word or a bare number says nothing about who it is.
    if AGENT_WORDS.contains(&name.as_str()) || name.chars().all(|c| c.is_ascii_digit()) {
        why.push("a bare agent-type word or number: not an identifiable lane".to_string());
    }
    if words(&name)
        .iter()
        .any(|word| ROLE_WORDS.contains(&word.as_str()))
    {
        why.push("a role-like name: a person may still write to it".to_string());
    }
    if row.queued > 0 {
        why.push(format!("{} unread message(s)", row.queued));
    }
    if row.open_pr_waits > 0 {
        why.push("an open PR wait".to_string());
    }
    match row.newest_at {
        Some(newest) if now - newest < QUIET_S => why.push("mail in the last 14 days".to_string()),
        None => why.push("the time of its newest mail is unknown".to_string()),
        Some(_) => {}
    }
    if why.is_empty() {
        (
            "retired",
            vec![
                "no live pane or registry entry, nothing unread or waiting, quiet for 14 days"
                    .into(),
            ],
        )
    } else {
        ("ambiguous", why)
    }
}

/// The dry-run report: counts per class and one entry per mailbox. Nothing is changed.
pub fn report(rows: &[CensusRow], identities: &Identities, now: f64) -> Value {
    let live: Vec<String> = identities.live.iter().map(|name| key(name)).collect();
    let registered: Vec<String> = identities.registered.iter().map(|name| key(name)).collect();
    let mut mailboxes = Vec::new();
    let mut by_class: BTreeMap<&str, (i64, i64)> = BTreeMap::new();
    for row in rows {
        if row.status.as_deref() == Some("retired") {
            let entry = by_class.entry("already_retired").or_default();
            entry.0 += 1;
            entry.1 += row.messages;
            mailboxes.push(json!({"mailbox": row.mailbox, "class": "already_retired", "messages": row.messages, "why": []}));
            continue;
        }
        let (class, why) = classify(row, &live, &registered, now);
        let entry = by_class.entry(class).or_default();
        entry.0 += 1;
        entry.1 += row.messages;
        mailboxes.push(json!({
            "mailbox": row.mailbox,
            "class": class,
            "messages": row.messages,
            "attachments": row.attachments,
            "unread": row.queued,
            "waiting": row.open_pr_waits,
            "last_activity_at": row.newest_at,
            "newest_age_days": row.newest_at.map(|at| ((now - at) / 86_400.0 * 10.0).floor() / 10.0),
            "why": why,
        }));
    }
    json!({
        "dry_run": true,
        "note": "a report: nothing was retired. 'retired' lists the candidates; retiring them needs a reviewed list and the coordinator's GO",
        "mailboxes_total": rows.len(),
        "messages_total": rows.iter().map(|row| row.messages).sum::<i64>(),
        "by_class": by_class
            .into_iter()
            .map(|(class, (mailboxes, messages))| (class.to_string(), json!({"mailboxes": mailboxes, "messages": messages})))
            .collect::<serde_json::Map<_, _>>(),
        "mailboxes": mailboxes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: f64 = 2_000_000_000.0;
    const DAY: f64 = 86_400.0;

    fn row(mailbox: &str, days: f64) -> CensusRow {
        CensusRow {
            mailbox: mailbox.into(),
            messages: 3,
            attachments: 0,
            queued: 0,
            newest_at: Some(NOW - days * DAY),
            open_pr_waits: 0,
            status: None,
        }
    }

    fn class_of(row: CensusRow, identities: &Identities) -> (String, Vec<String>) {
        let report = report(&[row], identities, NOW);
        let entry = &report["mailboxes"][0];
        (
            entry["class"].as_str().unwrap().to_string(),
            entry["why"]
                .as_array()
                .unwrap()
                .iter()
                .map(|w| w.as_str().unwrap().to_string())
                .collect(),
        )
    }

    #[test]
    fn only_a_quiet_unregistered_mailbox_with_nothing_waiting_is_a_candidate() {
        let none = Identities::default();
        assert_eq!(class_of(row("old-worker-7", 15.0), &none).0, "retired");
        assert_eq!(
            class_of(row("old-worker-7", 13.9), &none),
            ("ambiguous".into(), vec!["mail in the last 14 days".into()])
        );
        let mut unread = row("old-worker-7", 30.0);
        unread.queued = 2;
        assert_eq!(class_of(unread, &none).1, ["2 unread message(s)"]);
        let mut waiting = row("old-worker-7", 30.0);
        waiting.open_pr_waits = 1;
        assert_eq!(class_of(waiting, &none).1, ["an open PR wait"]);
        let mut timeless = row("old-worker-7", 30.0);
        timeless.newest_at = None;
        assert_eq!(
            class_of(timeless, &none).1,
            ["the time of its newest mail is unknown"]
        );
    }

    #[test]
    fn live_registered_human_vp_and_role_names_are_never_candidates() {
        let identities = Identities {
            live: vec!["%12".into(), "Writer-A".into()],
            registered: vec!["Old-Reviewer".into()],
        };
        assert_eq!(
            class_of(row("writer-a", 90.0), &identities).0,
            "active",
            "names are case-folded"
        );
        assert_eq!(class_of(row("%12", 90.0), &identities).0, "active");
        assert_eq!(
            class_of(row("old-reviewer", 90.0), &identities).1,
            ["in the role or lane registry"]
        );
        for name in ["owner", "Owner", "owner-inbox"] {
            assert_eq!(class_of(row(name, 90.0), &identities).0, "human", "{name}");
        }
        for name in [
            "vp",
            "VP of Agentry",
            "vp-webui",
            "codex-rust-owner",
            "cos-ds",
            "chief-of-staff",
            "front-desk",
            // t5014, from the owner's dry run: 'vp' anywhere as a word.
            "poolrad-vp",
            "lookingglassnx-vp",
            "VP for Poolrad-decomp",
            "vp-claude",
        ] {
            assert_eq!(class_of(row(name, 90.0), &identities).0, "vp", "{name}");
        }
        // t5014 (%314's check of the candidates): none of these shapes is ever 'retired'.
        assert_eq!(
            class_of(row("unrouted:codex", 90.0), &identities).0,
            "quarantine"
        );
        let registered_parts = Identities {
            live: vec![],
            registered: vec!["ww:p1".into()],
        };
        let (class, why) = class_of(row("wW:p1,w9:pH", 90.0), &registered_parts);
        assert_eq!(class, "ambiguous");
        assert!(
            why.iter()
                .any(|w| w.contains("parts are live or registered")),
            "{why:?}"
        );
        let (class, why) = class_of(row("x,y", 90.0), &identities);
        assert_eq!(class, "ambiguous", "{why:?}");
        for name in ["dunky claude", "OpenCode", "1", "codex", "42"] {
            assert_eq!(
                class_of(row(name, 90.0), &identities).0,
                "ambiguous",
                "{name}"
            );
        }
        // 'vp' inside a longer word is not a VP name.
        for name in ["vpn-tunnel", "mvp-writer"] {
            assert_ne!(class_of(row(name, 90.0), &identities).0, "vp", "{name}");
        }
        for name in [
            "PM for Project",
            "project-coordinator",
            "team-lead",
            "planner-2",
        ] {
            let (class, why) = class_of(row(name, 90.0), &identities);
            assert_eq!(class, "ambiguous", "{name}");
            assert!(
                why.iter()
                    .any(|w| w == "a role-like name: a person may still write to it"),
                "{name}: {why:?}"
            );
            // t5014: a name with spaces also carries the nickname reason (added before the
            // role-like one).
            let spaced = name.contains(' ');
            assert_eq!(why.len(), 1 + usize::from(spaced), "{name}: {why:?}");
        }
        // A word that merely contains a role word is not role-like.
        assert_eq!(
            class_of(row("leader-board-bot", 90.0), &identities).0,
            "retired"
        );
        assert_eq!(class_of(row("vpn-monitor", 90.0), &identities).0, "retired");
    }

    #[test]
    fn the_report_counts_classes_and_lists_already_retired_mailboxes_apart() {
        let mut done = row("retired-before", 60.0);
        done.status = Some("retired".into());
        let report = report(
            &[row("a", 30.0), row("b", 1.0), done],
            &Identities::default(),
            NOW,
        );
        assert_eq!(report["dry_run"], true);
        assert_eq!(report["mailboxes_total"], 3);
        assert_eq!(report["messages_total"], 9);
        assert_eq!(
            report["by_class"]["retired"],
            json!({"mailboxes": 1, "messages": 3})
        );
        assert_eq!(
            report["by_class"]["ambiguous"],
            json!({"mailboxes": 1, "messages": 3})
        );
        assert_eq!(
            report["by_class"]["already_retired"],
            json!({"mailboxes": 1, "messages": 3})
        );
        assert_eq!(report["mailboxes"][0]["newest_age_days"], 30.0);
    }
}
