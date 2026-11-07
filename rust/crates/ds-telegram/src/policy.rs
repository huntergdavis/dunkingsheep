//! `bridges/telegram_transport.py`: what Telegram may carry, and how a transport delivers it.
//!
//! The owner's rule (t594, 2026-10-29): reply on the transport the question arrived on. Telegram
//! carries answers to Telegram questions and nothing else, plus owner pages (security, outage,
//! money; at most three per hour). This module is the policy with no I/O, so it is tested without
//! a network, a token or a desk. It is a 1:1 port: the Python functions keep their names where a
//! Rust name is not already taken.

/// The watchdog's marker for a page; `is_page` reads it, nothing new is invented.
pub const PAGE_MARKER: &str = "PAGE OWNER";
/// The sender the daemon stamps on an owner page row in the owner mailbox (t665).
pub const OWNER_PAGE_SENDER: &str = "owner-page";
pub const PAGE_CLASSES: [&str; 3] = ["security", "outage", "money"];
pub const PAGE_MAX_PER_HOUR: usize = 3;
pub const PAGE_WINDOW_S: f64 = 3600.0;
/// `_chunks` default: a message is cut to fit Telegram's 4096 limit with some margin.
pub const CHUNK_LIMIT: usize = 3900;

/// Delivery kinds, as the Python strings. `reply` and `mailbox-page` carry a daemon receipt.
pub const KIND_REPLY: &str = "reply";
pub const KIND_PAGE: &str = "page";
pub const KIND_MAILBOX_PAGE: &str = "mailbox-page";

/// Is this owner-mailbox row one of the page classes? The marker is the test.
pub fn is_page(body: &str) -> bool {
    body.to_uppercase().contains(PAGE_MARKER)
}

/// The first class word found in the page text, in declaration order, or `None`.
pub fn page_class(body: &str) -> Option<&'static str> {
    let lower = body.to_lowercase();
    PAGE_CLASSES
        .iter()
        .copied()
        .find(|name| lower.contains(name))
}

/// How many pages may still go out: the budget, not a queue. Timestamps inside the window count.
pub fn pages_allowed(sent_at: &[f64], now: f64, limit: usize, window: f64) -> usize {
    let recent = sent_at.iter().filter(|at| now - **at < window).count();
    limit.saturating_sub(recent)
}

/// One candidate row from the daemon outbox or the legacy desk store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub message_id: String,
    pub body: String,
    /// `page`, `mailbox-page` or `reply`; the caller chooses it per source.
    pub kind: String,
    /// The desk-store row id, present for a legacy page only (its watermark key).
    pub id: Option<i64>,
}

/// One thing to send now, in order, with the reason it is going (or being held).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub message_id: String,
    pub body: String,
    pub kind: String,
    pub reason: String,
    pub id: Option<i64>,
    pub hold: bool,
    pub page_class: Option<String>,
}

/// A delivered item, as `deliver` returns it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sent {
    pub message_id: String,
    pub kind: String,
    pub reason: String,
}

/// What to send now: every reply, then pages while the budget lasts. A page over the budget is
/// HELD (kept, with `hold`), never dropped. Anything else - a brief, a digest, a web echo - is not
/// in either input, so nothing here can select it.
pub fn outbox_items(
    replies: &[Row],
    pages: &[Row],
    sent_page_times: &[f64],
    now: f64,
    page_limit: usize,
    window: f64,
) -> Vec<Item> {
    let mut items = Vec::with_capacity(replies.len() + pages.len());
    for row in replies {
        items.push(Item {
            message_id: row.message_id.clone(),
            body: row.body.clone(),
            kind: KIND_REPLY.to_string(),
            reason: "reply on its own transport".to_string(),
            id: None,
            hold: false,
            page_class: None,
        });
    }
    let mut budget = pages_allowed(sent_page_times, now, page_limit, window);
    for row in pages {
        if budget == 0 {
            items.push(Item {
                message_id: row.message_id.clone(),
                body: row.body.clone(),
                kind: row.kind.clone(),
                reason: format!("held: {page_limit} pages per hour already sent"),
                id: row.id,
                hold: true,
                page_class: None,
            });
            continue;
        }
        budget -= 1;
        let class = page_class(&row.body);
        items.push(Item {
            message_id: row.message_id.clone(),
            body: row.body.clone(),
            kind: row.kind.clone(),
            reason: format!("page ({})", class.unwrap_or("unclassified")),
            id: row.id,
            hold: false,
            page_class: class.map(str::to_string),
        });
    }
    items
}

/// Send each item and record its receipt; a failure stops the batch there, with no receipt for
/// the send that did not happen. `send(body)` is the transport's own send; `mark_delivered`
/// records the daemon-side receipt (and, for a page, the budget and watermark).
pub fn deliver<E>(
    items: &[Item],
    mut send: impl FnMut(&str) -> Result<(), E>,
    mut mark_delivered: impl FnMut(&str, &str) -> Result<(), E>,
) -> Result<Vec<Sent>, E> {
    let mut sent = Vec::new();
    for item in items {
        if item.hold {
            continue;
        }
        send(&item.body)?;
        mark_delivered(&item.message_id, &item.kind)?;
        sent.push(Sent {
            message_id: item.message_id.clone(),
            kind: item.kind.clone(),
            reason: item.reason.clone(),
        });
    }
    Ok(sent)
}

/// `_chunks`: cut at the last newline in the first `limit` characters when that newline is in the
/// second half of the window, else at `limit`. The remainder drops its leading newlines.
/// Lengths are counted in characters, as Python counts code points.
pub fn chunks(body: &str, limit: usize) -> Vec<String> {
    let mut rest: Vec<char> = body.chars().collect();
    let mut out = Vec::new();
    while !rest.is_empty() {
        if rest.len() <= limit {
            out.push(rest.iter().collect());
            break;
        }
        let cut = match rest[..limit].iter().rposition(|c| *c == '\n') {
            Some(index) if index >= limit / 2 => index,
            _ => limit,
        };
        out.push(rest[..cut].iter().collect());
        let mut next = rest[cut..].to_vec();
        let leading = next.iter().take_while(|c| **c == '\n').count();
        next.drain(..leading);
        rest = next;
    }
    out
}
