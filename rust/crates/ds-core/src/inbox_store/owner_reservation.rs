//! Durable storage primitives for an owner-question delegation.
//!
//! These primitives do not authorize callers or deliver notifications. The service layer must
//! bind the supervisor to a verified live registration before calling them.

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use crate::clock::now_s;
use crate::db::map_sqlite;
use crate::error::DunkError;
use crate::flock::mailbox::OWNER_MAILBOX;
use crate::inbox::normalise_mailbox;

use super::{refuse, Store};

pub(super) const SUPERVISOR: &str = "codex-rust-owner";

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Reservation {
    pub question_id: String,
    pub supervisor: String,
    pub incarnation: String,
    pub transport: String,
    pub generation: i64,
}

impl Store {
    /// Rebind an active reservation after an owner-authorized recovery decision.
    /// Authorization and new supervisor identity verification belong to the caller.
    #[allow(dead_code)] // Not exposed until the owner recovery gate is implemented.
    pub(crate) fn recover_owner_reservation(
        &self,
        question_id: &str,
        expected_generation: i64,
        new_incarnation: &str,
        actor: &str,
    ) -> Result<Reservation, DunkError> {
        if expected_generation < 1 || new_incarnation.trim().is_empty() || actor.trim().is_empty() {
            return Err(refuse(
                "recovery requires generation, verified incarnation and owner actor",
            ));
        }
        let (question_id, new_incarnation, actor) = (
            question_id.to_owned(),
            new_incarnation.to_owned(),
            actor.to_owned(),
        );
        self.writer.call(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(map_sqlite)?;
            let transport: Option<String> = tx.query_row(
                "SELECT transport FROM owner_delegation_reservations WHERE question_id = ? AND supervisor = ? AND generation = ? AND state = 'active'",
                params![question_id, SUPERVISOR, expected_generation], |row| row.get(0),
            ).optional().map_err(map_sqlite)?;
            let transport = transport.ok_or_else(|| refuse("reservation is no longer active at expected generation"))?;
            let generation = expected_generation.checked_add(1).ok_or_else(|| refuse("delegation generation exhausted"))?;
            let now = now_s();
            let old_notice = format!("owner-delegation-{question_id}-{expected_generation}");
            tx.execute("UPDATE messages SET state = 'cancelled', cancelled_at = ? WHERE id = ? AND sender = 'owner-delegation' AND state IN ('queued', 'read')", params![now, old_notice]).map_err(map_sqlite)?;
            tx.execute("UPDATE owner_delegation_reservations SET incarnation = ?, generation = ?, updated_at = ? WHERE question_id = ?", params![new_incarnation, generation, now, question_id]).map_err(map_sqlite)?;
            tx.execute("INSERT INTO owner_delegation_events (question_id, generation, event, actor, at) VALUES (?, ?, 'recovered', ?, ?)", params![question_id, generation, actor, now]).map_err(map_sqlite)?;
            tx.commit().map_err(map_sqlite)?;
            Ok(Reservation { question_id, supervisor: SUPERVISOR.to_owned(), incarnation: new_incarnation, transport, generation })
        })
    }

    /// Persist one supervisor notice only while the exact reservation is active.
    /// The caller must independently authenticate the fixed mailbox and pane incarnation.
    #[allow(dead_code)] // Used by the delegation worker once its identity gate is wired.
    pub(crate) fn ensure_owner_delegation_notice(
        &self,
        proof: &Reservation,
        pane: &str,
        task_id: &str,
    ) -> Result<String, DunkError> {
        if pane.trim().is_empty() || task_id.trim().is_empty() {
            return Err(refuse("a supervisor pane and task are required"));
        }
        let proof = proof.clone();
        let (pane, task_id) = (pane.to_owned(), task_id.to_owned());
        self.writer.call(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(map_sqlite)?;
            let valid: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM owner_delegation_reservations WHERE question_id = ? AND supervisor = ? AND incarnation = ? AND transport = ? AND generation = ? AND state = 'active')",
                params![proof.question_id, proof.supervisor, proof.incarnation, proof.transport, proof.generation],
                |row| row.get(0),
            ).map_err(map_sqlite)?;
            if !valid || proof.supervisor != SUPERVISOR { return Err(refuse("delegation reservation changed")); }
            let id = format!("owner-delegation-{}-{}", proof.question_id, proof.generation);
            let subject = format!("TASK {task_id}: delegated owner question");
            let body = format!("TASK {task_id}: answer owner question {} using the authenticated delegation reply operation.\n\nheartbeat: dunkingsheep task heartbeat {task_id}", proof.question_id);
            tx.execute("INSERT OR IGNORE INTO messages (id, mailbox, mailbox_label, pane_id, sender, subject, body, state, created_at) VALUES (?, ?, ?, ?, 'owner-delegation', ?, ?, 'queued', ?)", params![id, SUPERVISOR, SUPERVISOR, pane, subject, body, now_s()]).map_err(map_sqlite)?;
            let stored: (String, Option<String>, Option<String>, Option<String>, String) = tx.query_row(
                "SELECT mailbox, pane_id, subject, body, state FROM messages WHERE id = ?", [&id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            ).map_err(map_sqlite)?;
            if stored.0 != SUPERVISOR || stored.1.as_deref() != Some(&pane) || stored.2.as_deref() != Some(&subject) || stored.3.as_deref() != Some(&body) {
                return Err(refuse("delegation notice identity changed"));
            }
            // Never revive a terminal notice. An ack records inbox acknowledgement only;
            // cancelled or archived notices require explicit recovery instead of silent resend.
            if !matches!(stored.4.as_str(), "queued" | "read" | "acked") {
                return Err(refuse("terminal delegation notice requires recovery"));
            }
            tx.commit().map_err(map_sqlite)?;
            Ok(id)
        })
    }

    /// List durable work that still needs materialisation or an answer after restart.
    /// Callers must revalidate identity and reservation generation before acting on a row.
    #[allow(dead_code)] // Consumed by the recovery worker when the service is wired.
    pub(crate) fn active_owner_reservations(&self) -> Result<Vec<Reservation>, DunkError> {
        self.readers.with(|connection| {
            let mut statement = connection.prepare(
                "SELECT question_id, supervisor, incarnation, transport, generation FROM owner_delegation_reservations WHERE state = 'active' ORDER BY question_id",
            ).map_err(map_sqlite)?;
            let rows = statement.query_map([], |row| Ok(Reservation {
                question_id: row.get(0)?,
                supervisor: row.get(1)?,
                incarnation: row.get(2)?,
                transport: row.get(3)?,
                generation: row.get(4)?,
            })).map_err(map_sqlite)?;
            rows.collect::<Result<Vec<_>, _>>().map_err(map_sqlite)
        })
    }

    /// Reserve an unanswered transport question. Retries for the same incarnation are idempotent;
    /// an active reservation cannot silently be redirected to a different process.
    #[allow(dead_code)] // Wired by the delegation service in the next slice.
    pub(crate) fn reserve_owner_question(
        &self,
        question_id: &str,
        incarnation: &str,
        actor: &str,
    ) -> Result<Reservation, DunkError> {
        if incarnation.trim().is_empty() || actor.trim().is_empty() {
            return Err(refuse(
                "a verified supervisor incarnation and actor are required",
            ));
        }
        let (question_id, incarnation, actor) = (
            question_id.to_owned(),
            incarnation.to_owned(),
            actor.to_owned(),
        );
        self.writer.call(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(map_sqlite)?;
            let question: Option<(String, Option<String>, Option<String>, String, Option<String>)> = tx.query_row(
                "SELECT mailbox, reply_to, transport, state, sender FROM messages WHERE id = ?",
                [&question_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            ).optional().map_err(map_sqlite)?;
            let (mailbox, parent, transport, state, sender) = question.ok_or_else(|| refuse(format!("no message with id {question_id:?}")))?;
            if normalise_mailbox(&mailbox) != OWNER_MAILBOX || parent.as_deref().is_some_and(|id| !id.is_empty()) || sender.as_deref().is_some_and(|id| !id.is_empty()) {
                return Err(refuse("delegation requires an owner question"));
            }
            if matches!(state.as_str(), "cancelled" | "archived") {
                return Err(refuse("a cancelled or archived question cannot be delegated"));
            }
            let transport = transport.filter(|t| matches!(t.as_str(), "web" | "telegram"))
                .ok_or_else(|| refuse("delegation requires an originating web or telegram transport"))?;
            let answered: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM messages WHERE reply_to = ?)", [&question_id], |row| row.get(0)).map_err(map_sqlite)?;
            if answered { return Err(refuse("the owner question was already answered")); }
            let existing: Option<(String, String, String, i64, String)> = tx.query_row(
                "SELECT supervisor, incarnation, transport, generation, state FROM owner_delegation_reservations WHERE question_id = ?",
                [&question_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
            ).optional().map_err(map_sqlite)?;
            if let Some((supervisor, old_incarnation, old_transport, generation, state)) = existing {
                if state == "active" {
                    if supervisor != SUPERVISOR || old_incarnation != incarnation || old_transport != transport {
                        return Err(refuse("active delegation identity or transport changed"));
                    }
                    tx.commit().map_err(map_sqlite)?;
                    return Ok(Reservation { question_id, supervisor, incarnation, transport, generation });
                }
                return Err(refuse("terminal delegation requires an owner-authorized recovery operation"));
            }
            let now = now_s();
            tx.execute("INSERT INTO owner_delegation_reservations (question_id, supervisor, incarnation, transport, generation, state, updated_at) VALUES (?, ?, ?, ?, 1, 'active', ?)", params![question_id, SUPERVISOR, incarnation, transport, now]).map_err(map_sqlite)?;
            tx.execute("INSERT INTO owner_delegation_events (question_id, generation, event, actor, at) VALUES (?, 1, 'reserved', ?, ?)", params![question_id, actor, now]).map_err(map_sqlite)?;
            tx.commit().map_err(map_sqlite)?;
            Ok(Reservation { question_id, supervisor: SUPERVISOR.to_owned(), incarnation, transport, generation: 1 })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inbox_store::Outgoing;

    #[test]
    fn reservation_survives_restart_and_fences_other_reply_paths() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("inbox.db");
        let blobs = dir.path().join("blobs");
        let store = Store::open(&db, &blobs).unwrap();
        let question = store
            .send(&Outgoing {
                mailbox: OWNER_MAILBOX,
                body: Some("question"),
                transport: Some("telegram"),
                ..Outgoing::default()
            })
            .unwrap();
        let proof = store
            .reserve_owner_question(&question, "pid:1:start:2", "desk")
            .unwrap();
        assert_eq!(
            proof,
            store
                .reserve_owner_question(&question, "pid:1:start:2", "desk")
                .unwrap()
        );
        assert!(store
            .reserve_owner_question(&question, "pid:1:start:3", "desk")
            .is_err());
        drop(store);
        let store = Store::open(&db, &blobs).unwrap();
        assert_eq!(
            store.active_owner_reservations().unwrap(),
            vec![proof.clone()]
        );
        assert!(store
            .answer_owner_question(&question, "wrong", None)
            .is_err());
        assert!(store
            .send(&Outgoing {
                mailbox: OWNER_MAILBOX,
                body: Some("bypass"),
                reply_to: Some(&question),
                ..Outgoing::default()
            })
            .is_err());
        let mut stale = proof.clone();
        stale.generation += 1;
        assert!(store
            .answer_owner_question_guarded(&question, "stale", None, Some(&stale))
            .is_err());
        assert!(store
            .answer_owner_question_guarded(&question, "missing actor", None, Some(&proof))
            .is_err());
        let answer = store
            .answer_owner_question_guarded(&question, "answer", Some("%1"), Some(&proof))
            .unwrap();
        assert_eq!(answer.summary.transport.as_deref(), Some("telegram"));
        assert_eq!(answer.summary.reply_to.as_deref(), Some(question.as_str()));
        assert_eq!(store.replies_to(&question).unwrap().len(), 1);
        assert!(store.active_owner_reservations().unwrap().is_empty());
        assert!(store
            .send(&Outgoing {
                mailbox: OWNER_MAILBOX,
                body: Some("late bypass"),
                reply_to: Some(&question),
                ..Outgoing::default()
            })
            .is_err());
    }

    #[test]
    fn answer_and_reservation_are_serialized_across_store_connections() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("inbox.db");
        let first = Store::open(&db, dir.path().join("blobs1")).unwrap();
        let second = Store::open(&db, dir.path().join("blobs2")).unwrap();
        let question = first
            .send(&Outgoing {
                mailbox: OWNER_MAILBOX,
                body: Some("question"),
                transport: Some("web"),
                ..Outgoing::default()
            })
            .unwrap();
        let (reserved, answered) = std::thread::scope(|scope| {
            let a =
                scope.spawn(|| first.reserve_owner_question(&question, "pid:1:start:2", "desk"));
            let b = scope.spawn(|| second.answer_owner_question(&question, "answer", None));
            (a.join().unwrap(), b.join().unwrap())
        });
        assert_ne!(reserved.is_ok(), answered.is_ok());
        if let Ok(proof) = reserved {
            first
                .answer_owner_question_guarded(&question, "answer", Some("%1"), Some(&proof))
                .unwrap();
        }
        assert_eq!(first.replies_to(&question).unwrap().len(), 1);
    }

    #[test]
    fn generic_reply_cannot_race_a_reservation() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("inbox.db");
        let first = Store::open(&db, dir.path().join("blobs1")).unwrap();
        let second = Store::open(&db, dir.path().join("blobs2")).unwrap();
        let question = first
            .send(&Outgoing {
                mailbox: OWNER_MAILBOX,
                body: Some("question"),
                transport: Some("web"),
                ..Outgoing::default()
            })
            .unwrap();
        let (reserved, sent) = std::thread::scope(|scope| {
            let a = scope.spawn(|| first.reserve_owner_question(&question, "incarnation", "desk"));
            let b = scope.spawn(|| {
                second.send(&Outgoing {
                    mailbox: OWNER_MAILBOX,
                    body: Some("generic reply"),
                    reply_to: Some(&question),
                    ..Outgoing::default()
                })
            });
            (a.join().unwrap(), b.join().unwrap())
        });
        assert!(!(reserved.is_ok() && sent.is_ok()));
        if reserved.is_ok() {
            assert!(first.replies_to(&question).unwrap().is_empty());
        }
    }

    #[test]
    fn recovery_advances_generation_and_fences_old_supervisor() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("inbox.db"), dir.path().join("blobs")).unwrap();
        let question = store
            .send(&Outgoing {
                mailbox: OWNER_MAILBOX,
                body: Some("question"),
                transport: Some("web"),
                ..Outgoing::default()
            })
            .unwrap();
        let old = store
            .reserve_owner_question(&question, "old", "desk")
            .unwrap();
        store
            .ensure_owner_delegation_notice(&old, "%1", "t1")
            .unwrap();
        let new = store
            .recover_owner_reservation(&question, old.generation, "new", "owner")
            .unwrap();
        assert_eq!(new.generation, old.generation + 1);
        assert!(store
            .recover_owner_reservation(&question, old.generation, "again", "owner")
            .is_err());
        assert!(store
            .ensure_owner_delegation_notice(&old, "%1", "t1")
            .is_err());
        assert!(store
            .answer_owner_question_guarded(&question, "stale", Some("%1"), Some(&old))
            .is_err());
        store
            .answer_owner_question_guarded(&question, "answer", Some("%1"), Some(&new))
            .unwrap();
    }
}
