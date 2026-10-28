//! Minimal extraction of the donor's global pending discovery. Migration
//! registration is part of the published schema24 opener transaction.
use super::*;
use crate::automations::{PROPOSAL_PAGE_SIZE, ProposalDelivery, ProposalPage, ProposalReview};

const PENDING_PAGE_SQL: &str = "SELECT summary FROM automation_proposals \
    INDEXED BY global_pending_automation_proposals \
    WHERE pending=1 AND id>?1 ORDER BY id LIMIT ?2";

/// Published schema24 delivery metadata, registered once in the authoritative
/// opener transaction alongside the proposal-created source outbox.
/// Existing pending rows are counted once; no notification/history replay.
pub(in crate::host::model) fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(
        "ALTER TABLE metadata ADD COLUMN automation_proposal_sequence INTEGER NOT NULL DEFAULT 1 CHECK(automation_proposal_sequence>0);
         ALTER TABLE metadata ADD COLUMN pending_automation_proposal_count INTEGER NOT NULL DEFAULT 0 CHECK(pending_automation_proposal_count>=0);
         CREATE INDEX global_pending_automation_proposals ON automation_proposals(id) WHERE pending=1;
         UPDATE metadata SET pending_automation_proposal_count=(SELECT count(*) FROM automation_proposals WHERE pending=1) WHERE key=0;",
    ).map_err(|error| error.to_string())
}

/// Called only after a real INSERT or pending->terminal UPDATE, inside that
/// same transaction. Retries returning existing receipts never reach this.
pub(super) fn changed_in(tx: &rusqlite::Transaction<'_>, added: bool) -> Result<(), String> {
    let delta = if added { 1i64 } else { -1i64 };
    let changed = tx
        .execute(
            "UPDATE metadata SET sequence=sequence+1,
            automation_proposal_sequence=automation_proposal_sequence+1,
            pending_automation_proposal_count=pending_automation_proposal_count+?1
         WHERE key=0 AND sequence>=0 AND sequence<9223372036854775807
            AND automation_proposal_sequence>0 AND automation_proposal_sequence<9223372036854775807
            AND ((?1=1 AND pending_automation_proposal_count<9223372036854775807)
                 OR (?1=-1 AND pending_automation_proposal_count>0))",
            [delta],
        )
        .map_err(|error| error.to_string())?;
    if changed != 1 {
        return Err("The automation proposal delivery clock or count cannot advance".into());
    }
    Ok(())
}

fn page_in(
    db: &Connection,
    after: Option<Uuid>,
    limit: usize,
) -> Result<Vec<ProposalSummary>, String> {
    let mut query = db
        .prepare(PENDING_PAGE_SQL)
        .map_err(|error| error.to_string())?;
    query
        .query_map(
            params![
                after.map(|id| id.to_string()).unwrap_or_default(),
                limit.clamp(1, PROPOSAL_PAGE_SIZE)
            ],
            |row| row.get::<_, String>(0),
        )
        .map_err(|error| error.to_string())?
        .map(|row| {
            serde_json::from_str(&row.map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())
        })
        .collect()
}

fn header_in(db: &Connection) -> Result<(Uuid, u64, u64), String> {
    let (identity, sequence, count): (String, u64, u64) = db.query_row(
        "SELECT identity,automation_proposal_sequence,pending_automation_proposal_count FROM metadata WHERE key=0",
        [], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
    ).map_err(|error| error.to_string())?;
    let identity = identity
        .parse::<Uuid>()
        .map_err(|_| "The proposal registry identity is unreadable")?;
    if identity.is_nil() || sequence == 0 {
        return Err("The proposal delivery capability is unavailable".into());
    }
    Ok((identity, sequence, count))
}

fn snapshot_in(db: &Connection) -> Result<ProposalDelivery, String> {
    let (identity, sequence, count) = header_in(db)?;
    let proposals = page_in(db, None, PROPOSAL_PAGE_SIZE)?;
    if count < proposals.len() as u64 {
        return Err("The proposal delivery count disagrees with its pending page".into());
    }
    Ok(ProposalDelivery {
        identity,
        automation_proposal_sequence: sequence,
        pending_automation_proposal_count: count,
        pending_automation_proposals: proposals,
    })
}

pub(super) fn require_identity(db: &Connection, expected: Uuid) -> Result<(), String> {
    let identity: String = db
        .query_row("SELECT identity FROM metadata WHERE key=0", [], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())?;
    if identity.parse::<Uuid>().ok() != Some(expected) || expected.is_nil() {
        return Err("The proposal registry changed; refresh pending requests".into());
    }
    Ok(())
}

impl Registry {
    fn proposal_read<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        if !self.db.is_autocommit() {
            return Err("Proposal delivery requires its own read snapshot".into());
        }
        let tx = rusqlite::Transaction::new_unchecked(&self.db, TransactionBehavior::Deferred)
            .map_err(|error| error.to_string())?;
        let result = super::super::schema::require_current(&tx).and_then(|()| read(&tx));
        let release = if result.is_ok() {
            tx.commit()
        } else {
            tx.rollback()
        };
        if !self.db.is_autocommit() {
            return Err("The proposal read snapshot did not release its transaction".into());
        }
        release.map_err(|error| format!("The proposal read snapshot could not close: {error}"))?;
        result
    }

    pub(crate) fn pending_automation_proposals_page(
        &self,
        identity: Option<Uuid>,
        sequence: Option<u64>,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<ProposalPage, String> {
        self.proposal_read(|db| {
            let (actual_identity,actual_sequence,_) = header_in(db)?;
            match (identity, sequence, after) {
                (Some(identity), Some(sequence), _) if identity == actual_identity && sequence == actual_sequence => {},
                (None, None, None) => {}, // User CLI first-page bootstrap only.
                _ => return Err("The pending proposal page changed; refresh proposal delivery before continuing".into()),
            }
            Ok(ProposalPage { identity: actual_identity, automation_proposal_sequence: actual_sequence, after, proposals: page_in(db, after, limit)? })
        })
    }

    pub(crate) fn review_automation_proposal(
        &self,
        identity: Uuid,
        id: Uuid,
    ) -> Result<ProposalReview, String> {
        self.proposal_read(|db| {
            require_identity(db, identity)?;
            let proposal = read(db, id, None)?.ok_or("The automation proposal is unavailable")?;
            Ok(ProposalReview {
                identity,
                proposal: Box::new(proposal),
            })
        })
    }

    /// Count/clock/first page share one explicitly released read snapshot.
    pub(crate) fn automation_proposal_delivery(&self) -> Result<ProposalDelivery, String> {
        self.proposal_read(snapshot_in)
    }
}

#[cfg(test)]
mod tests;
