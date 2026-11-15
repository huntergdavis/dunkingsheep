//! Model-context offers for the existing durable, coalesced inbox notice.
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{
    sends::{outbox_lock, Message},
    Flock,
};
use crate::error::DunkError;

pub(crate) const IDLE_FALLBACK_S: f64 = 120.0;
pub(crate) const OFFER_LIMIT: u8 = 2;

/// Persisted on the existing outbox row, never a separate delivery queue or mail receipt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct NoticeOffer {
    batch: String,
    offers: u8,
    last_edge: String,
    observed_at: f64,
}

pub(crate) fn hook_source(source: Option<&str>) -> bool {
    matches!(source, Some("codex-hook-mail-v1" | "claude-hook-mail-v1"))
}

impl Flock {
    /// Called only after the normal turn authentication. A sender's notify=false /
    /// recipient suppression has no queued notice and cannot be bypassed by this path.
    pub(crate) fn offer_mail_notice(
        &self,
        pane: &str,
        mailbox: &str,
        turn: &str,
        edge: &str,
        now: f64,
    ) -> Result<Option<Value>, DunkError> {
        let candidate = outbox_lock(&self.outbox)
            .get(pane)
            .and_then(|queue| queue.iter().find(|m| m.is_notice && m.status == "queued"))
            .cloned();
        let Some(candidate) = candidate else {
            return Ok(None);
        };
        let Some(inbox) = self.stores().inbox else {
            return Ok(None);
        };
        let mut ids = Vec::new();
        let mut all_consumed = !candidate.notify_for.is_empty();
        for id in &candidate.notify_for {
            // Metadata only; a stale/corrupted notice cannot expose another mailbox.
            let status = crate::inbox::status(inbox.read_pool(), id, now)?;
            let own = crate::inbox::normalise_mailbox(&status.mailbox)
                == crate::inbox::normalise_mailbox(mailbox);
            all_consumed &= own && status.state != "queued";
            if status.state == "queued"
                && crate::inbox::normalise_mailbox(&status.mailbox)
                    == crate::inbox::normalise_mailbox(mailbox)
            {
                ids.push(id.clone());
            }
        }
        ids.sort();
        ids.dedup();
        if ids.is_empty() {
            // Real inbox consumption obsoletes this offered wake, not the mail itself.
            // Recheck the exact batch under the queue lock so a folded arrival is retained.
            if all_consumed && candidate.notice_offer.is_some() {
                let removed = {
                    let mut outbox = outbox_lock(&self.outbox);
                    outbox.get_mut(pane).and_then(|queue| {
                        let index = queue.iter().position(|m| {
                            m.same_lifetime(&candidate)
                                && m.status == "queued"
                                && m.notify_for == candidate.notify_for
                        })?;
                        let mut row = queue.remove(index);
                        row.cancel_at(now);
                        row.detail = Some(
                            "mail consumed after hook offer; typed wake no longer needed".into(),
                        );
                        Some(row)
                    })
                };
                if let Some(row) = removed {
                    self.remember(&row);
                    row.done.settle();
                    self.save()?;
                }
            }
            return Ok(None);
        }
        let count = ids.len();
        let batch = format!("{:x}", Sha256::digest(ids.join("\n").as_bytes()));
        let edge = format!("{turn}:{edge}");
        let result = {
            let mut outbox = outbox_lock(&self.outbox);
            let Some(row) = outbox.get_mut(pane).and_then(|q| {
                q.iter_mut()
                    .find(|m| m.same_lifetime(&candidate) && m.status == "queued")
            }) else {
                return Ok(None);
            };
            let state = row.notice_offer.get_or_insert_with(|| NoticeOffer {
                batch: batch.clone(),
                offers: 0,
                last_edge: String::new(),
                observed_at: now,
            });
            if state.batch != batch {
                *state = NoticeOffer {
                    batch: batch.clone(),
                    offers: 0,
                    last_edge: String::new(),
                    observed_at: now,
                };
            }
            if state.last_edge == edge {
                return Ok(None);
            }
            state.last_edge = edge;
            state.observed_at = now;
            if state.offers >= OFFER_LIMIT {
                json!({"action":"fallback", "batch":batch, "unread":count, "reason":"unread_after_two_offers"})
            } else {
                state.offers += 1;
                json!({"action":"offer", "batch":batch, "unread":count,
                    "text":format!("[inbox] You have {count} unread message(s) in this batch. Read them with: dunkingsheep inbox read"),
                    "offers":state.offers})
            }
        };
        // Persist the offer before returning stdout. It proves only an offer, never consumption.
        self.save()?;
        if result["action"] == "fallback" {
            eprintln!("mail-first pane={pane} batch={batch} action=fallback reason=unread_after_two_offers idle_bound_s={IDLE_FALLBACK_S}");
        } else {
            eprintln!("mail-first pane={pane} batch={batch} action=offer unread={count}");
        }
        Ok(Some(result))
    }

    /// Before any terminal read or recovery, leave hook-owned notices to model context.
    /// Missing/expired turn evidence cannot suppress the existing guarded fallback forever.
    pub(crate) fn notice_waits_for_hook(&self, message: &Message, now: f64) -> bool {
        if !message.is_notice {
            return false;
        }
        let turns = self.turn_states.lock().unwrap_or_else(|p| p.into_inner());
        let turn = turns.get(&message.pane_id);
        if !turn.is_some_and(|t| hook_source(t.source.as_deref())) && message.notice_offer.is_none()
        {
            return false;
        }
        if turn.is_some_and(|t| t.ended_at.is_none() && t.expires_at > now) {
            return true;
        }
        let idle_since = turn
            .map(|t| t.ended_at.unwrap_or(t.expires_at))
            .filter(|at| *at > 0.0)
            .or_else(|| message.notice_offer.as_ref().map(|s| s.observed_at))
            .unwrap_or(message.created_at);
        now - idle_since < IDLE_FALLBACK_S
    }
}
