//! Durable delegation authority state. Callers must authenticate the owner and verify the
//! supervisor's peer-bound live identity before invoking these unexposed primitives.

use std::io::Read;

use rusqlite::{params, OptionalExtension, TransactionBehavior};

use super::{owner_reservation::SUPERVISOR, refuse, Store};
use crate::{clock::now_s, db::map_sqlite, error::DunkError};

pub(super) fn require_active_authority(
    tx: &rusqlite::Transaction<'_>,
    generation: &str,
) -> Result<(), DunkError> {
    let active: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM owner_delegation_authority WHERE supervisor = ? AND generation = ? AND state = 'active')",
        params![SUPERVISOR, generation], |row| row.get(0),
    ).map_err(map_sqlite)?;
    if !active {
        return Err(refuse(
            "delegation authority is missing, revoked or changed",
        ));
    }
    Ok(())
}

impl Store {
    /// Check the stored binding against a caller identity already verified against the
    /// socket peer, live process and declared mailbox. Mutations must also check the
    /// generation within their write transaction, because it can change after this read.
    #[allow(dead_code)] // The daemon identity gate is not wired yet.
    pub(crate) fn verify_delegation_binding(
        &self,
        generation: &str,
        pane: &str,
        pid: i64,
        start: &str,
    ) -> Result<(), DunkError> {
        if generation.trim().is_empty()
            || pane.trim().is_empty()
            || pid <= 1
            || start.trim().is_empty()
        {
            return Err(refuse("complete verified supervisor identity is required"));
        }
        self.readers.with(|connection| {
            let active: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM owner_delegation_authority WHERE supervisor = ? AND generation = ? AND pane_id = ? AND pane_pid = ? AND pane_start = ? AND state = 'active')",
                params![SUPERVISOR, generation, pane, pid, start], |row| row.get(0),
            ).map_err(map_sqlite)?;
            if !active { return Err(refuse("supervisor binding is missing, revoked or changed")); }
            Ok(())
        })
    }

    /// Persist a verified binding only when no authority exists or revocation completed.
    #[allow(dead_code)] // The owner-only migration gate is not yet wired.
    pub(crate) fn authorize_delegation_supervisor(
        &self,
        pane: &str,
        pid: i64,
        start: &str,
        actor: &str,
    ) -> Result<String, DunkError> {
        if pane.trim().is_empty() || pid <= 1 || start.trim().is_empty() || actor.trim().is_empty()
        {
            return Err(refuse("verified supervisor and owner actor are required"));
        }
        let mut entropy = [0u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut entropy)?;
        let generation: String = entropy.iter().map(|b| format!("{b:02x}")).collect();
        let (pane, start, actor) = (pane.to_owned(), start.to_owned(), actor.to_owned());
        self.writer.call(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(map_sqlite)?;
            let state: Option<String> = tx.query_row("SELECT state FROM owner_delegation_authority WHERE supervisor = ?", [SUPERVISOR], |row| row.get(0)).optional().map_err(map_sqlite)?;
            if state.as_deref().is_some_and(|s| s != "revoked") {
                return Err(refuse("delegation authority must be revoked before replacement"));
            }
            let now = now_s();
            tx.execute("INSERT INTO owner_delegation_authority (supervisor, pane_id, pane_pid, pane_start, generation, state, updated_at) VALUES (?, ?, ?, ?, ?, 'active', ?) ON CONFLICT(supervisor) DO UPDATE SET pane_id=excluded.pane_id, pane_pid=excluded.pane_pid, pane_start=excluded.pane_start, generation=excluded.generation, state='active', updated_at=excluded.updated_at", params![SUPERVISOR, pane, pid, start, generation, now]).map_err(map_sqlite)?;
            tx.execute("INSERT INTO owner_delegation_authority_events (supervisor, generation, event, actor, at) VALUES (?, ?, 'authorized', ?, ?)", params![SUPERVISOR, generation, actor, now]).map_err(map_sqlite)?;
            tx.commit().map_err(map_sqlite)?;
            Ok(generation)
        })
    }

    /// Record revocation intent before a material registry mutation. Retries are idempotent.
    #[allow(dead_code)] // Supported authority writers must be routed through this protocol.
    pub(crate) fn begin_delegation_revocation(
        &self,
        generation: &str,
        actor: &str,
    ) -> Result<(), DunkError> {
        if generation.trim().is_empty() || actor.trim().is_empty() {
            return Err(refuse("authority generation and owner actor are required"));
        }
        let (generation, actor) = (generation.to_owned(), actor.to_owned());
        self.writer.call(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(map_sqlite)?;
            let state: Option<String> = tx.query_row("SELECT state FROM owner_delegation_authority WHERE supervisor = ? AND generation = ?", params![SUPERVISOR, generation], |row| row.get(0)).optional().map_err(map_sqlite)?;
            match state.as_deref() {
                Some("revoking") => {},
                Some("active") => {
                    let now = now_s();
                    tx.execute("UPDATE owner_delegation_authority SET state='revoking', updated_at=? WHERE supervisor=? AND generation=?", params![now, SUPERVISOR, generation]).map_err(map_sqlite)?;
                    tx.execute("INSERT INTO owner_delegation_authority_events (supervisor, generation, event, actor, at) VALUES (?, ?, 'revocation_started', ?, ?)", params![SUPERVISOR, generation, actor, now]).map_err(map_sqlite)?;
                }
                _ => return Err(refuse("delegation authority generation changed")),
            }
            tx.commit().map_err(map_sqlite)?;
            Ok(())
        })
    }

    /// Finish only after the caller has reconciled the corresponding registry mutation.
    #[allow(dead_code)] // The mutation/recovery worker is not yet wired.
    pub(crate) fn finish_delegation_revocation(
        &self,
        generation: &str,
        actor: &str,
    ) -> Result<(), DunkError> {
        if generation.trim().is_empty() || actor.trim().is_empty() {
            return Err(refuse("authority generation and owner actor are required"));
        }
        let (generation, actor) = (generation.to_owned(), actor.to_owned());
        self.writer.call(move |connection| {
            let tx = connection.transaction_with_behavior(TransactionBehavior::Immediate).map_err(map_sqlite)?;
            let state: Option<String> = tx.query_row("SELECT state FROM owner_delegation_authority WHERE supervisor = ? AND generation = ?", params![SUPERVISOR, generation], |row| row.get(0)).optional().map_err(map_sqlite)?;
            match state.as_deref() {
                Some("revoked") => {},
                Some("revoking") => {
                    let now = now_s();
                    tx.execute("UPDATE owner_delegation_authority SET state='revoked', updated_at=? WHERE supervisor=? AND generation=?", params![now, SUPERVISOR, generation]).map_err(map_sqlite)?;
                    tx.execute("INSERT INTO owner_delegation_authority_events (supervisor, generation, event, actor, at) VALUES (?, ?, 'revocation_finished', ?, ?)", params![SUPERVISOR, generation, actor, now]).map_err(map_sqlite)?;
                }
                _ => return Err(refuse("delegation authority is not being revoked")),
            }
            tx.commit().map_err(map_sqlite)?;
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn revocation_intent_survives_restart_and_fences_replacement_generation() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("inbox.db");
        let blobs = dir.path().join("blobs");
        let store = Store::open(&db, &blobs).unwrap();
        let first = store
            .authorize_delegation_supervisor("%1", 123, "start", "owner")
            .unwrap();
        store
            .verify_delegation_binding(&first, "%1", 123, "start")
            .unwrap();
        assert!(store
            .verify_delegation_binding(&first, "%2", 123, "start")
            .is_err());
        assert!(store
            .verify_delegation_binding(&first, "%1", 124, "start")
            .is_err());
        assert!(store
            .authorize_delegation_supervisor("%1", 123, "start", "owner")
            .is_err());
        store.begin_delegation_revocation(&first, "owner").unwrap();
        assert!(store
            .verify_delegation_binding(&first, "%1", 123, "start")
            .is_err());
        drop(store);
        let store = Store::open(&db, &blobs).unwrap();
        store.begin_delegation_revocation(&first, "owner").unwrap();
        assert!(store
            .authorize_delegation_supervisor("%1", 123, "start", "owner")
            .is_err());
        store.finish_delegation_revocation(&first, "owner").unwrap();
        store.finish_delegation_revocation(&first, "owner").unwrap();
        let second = store
            .authorize_delegation_supervisor("%1", 123, "start", "owner")
            .unwrap();
        assert_ne!(first, second);
        assert!(store.begin_delegation_revocation(&first, "owner").is_err());
    }
}
