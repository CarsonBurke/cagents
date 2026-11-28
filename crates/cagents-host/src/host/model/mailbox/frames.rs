//! Durable recognition evidence for courier prompts. This store is independent
//! of the workspace schema so installation rollback leaves its evidence intact.

use crate::{
    agent_execution::mailbox::{DeliveryLookup, DeliveryReplay, FrameEvidence, Recognizer, Seal},
    model::TerminalId,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

pub(crate) struct DeliveredFrames(Mutex<Connection>);

impl DeliveredFrames {
    pub(in crate::host::model) fn open(path: &Path) -> rusqlite::Result<Self> {
        let db = Connection::open_with_flags(
            crate::secure_file::sqlite_path(path),
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        db.busy_timeout(super::super::BUSY_TIMEOUT)?;
        db.execute_batch(
            "PRAGMA synchronous=FULL;
             CREATE TABLE IF NOT EXISTS delivered_frames(
                 recipient BLOB NOT NULL CHECK(length(recipient) = 16),
                 digest BLOB NOT NULL CHECK(length(digest) = 32),
                 lengths BLOB NOT NULL CHECK(length(lengths) BETWEEN 4 AND 256
                                            AND length(lengths) % 4 = 0),
                 PRIMARY KEY (recipient, digest)
             ) STRICT, WITHOUT ROWID;",
        )?;
        Ok(Self(Mutex::new(db)))
    }

    /// Persist before offering the prompt to the provider, so a live echo or
    /// a crash before acknowledgment cannot outrun its evidence.
    pub(crate) fn record(
        &self,
        recipient: TerminalId,
        evidence: &FrameEvidence,
    ) -> Result<bool, String> {
        let lengths: Vec<u8> = evidence
            .lengths
            .iter()
            .flat_map(|len| len.to_le_bytes())
            .collect();
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .execute(
                "INSERT INTO delivered_frames(recipient, digest, lengths) VALUES (?1, ?2, ?3)
                 ON CONFLICT(recipient, digest) DO NOTHING",
                params![recipient.as_uuid().as_bytes(), evidence.digest, lengths],
            )
            .map(|inserted| inserted == 1)
            .map_err(|e| e.to_string())
    }

    /// Delete only a record added by this offer, which was proved unsent.
    pub(crate) fn forget(&self, recipient: TerminalId, digest: [u8; 32]) -> Result<(), String> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .execute(
                "DELETE FROM delivered_frames WHERE recipient = ?1 AND digest = ?2",
                params![recipient.as_uuid().as_bytes(), digest],
            )
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    pub(crate) fn recognizer(
        self: &Arc<Self>,
        seal: Arc<Seal>,
        recipients: Vec<TerminalId>,
    ) -> Recognizer {
        seal.recognizer().with_delivery_lookup(RecipientFrames {
            frames: self.clone(),
            seal,
            recipients,
        })
    }
}

struct RecipientFrames {
    frames: Arc<DeliveredFrames>,
    seal: Arc<Seal>,
    recipients: Vec<TerminalId>,
}

impl RecipientFrames {
    fn lookup(&self, digest: &[u8; 32]) -> rusqlite::Result<Option<Vec<u32>>> {
        let db = self.frames.0.lock().unwrap_or_else(|p| p.into_inner());
        let mut query = db.prepare_cached(
            "SELECT lengths FROM delivered_frames WHERE recipient = ?1 AND digest = ?2",
        )?;
        for &recipient in &self.recipients {
            let key = self.seal.delivery_key(recipient, digest);
            let lengths = query
                .query_row(params![recipient.as_uuid().as_bytes(), key], lengths)
                .optional()?;
            if lengths.is_some() {
                return Ok(lengths);
            }
        }
        Ok(None)
    }
}

impl DeliveryLookup for RecipientFrames {
    fn lengths(&self, digest: &[u8; 32]) -> Option<Vec<u32>> {
        match self.lookup(digest) {
            Ok(lengths) => lengths,
            Err(error) => {
                tracing::warn!(%error, "cannot verify a delivered mailbox prompt");
                None
            }
        }
    }

    fn replay(&self) -> Result<DeliveryReplay, String> {
        let replay = || -> rusqlite::Result<DeliveryReplay> {
            let db = self.frames.0.lock().unwrap_or_else(|p| p.into_inner());
            let mut query = db.prepare_cached(
                "SELECT lengths, digest FROM delivered_frames WHERE recipient = ?1",
            )?;
            let mut records = Vec::new();
            for recipient in &self.recipients {
                let entries = query.query_map([recipient.as_uuid().as_bytes()], |row| {
                    Ok(FrameEvidence {
                        digest: row.get(1)?,
                        lengths: lengths(row)?,
                    })
                })?;
                for entry in entries {
                    records.push(entry?);
                }
            }
            Ok(DeliveryReplay {
                recipients: self.recipients.clone(),
                records,
            })
        };
        replay().map_err(|e| e.to_string())
    }
}

fn lengths(row: &rusqlite::Row<'_>) -> rusqlite::Result<Vec<u32>> {
    let encoded: Vec<u8> = row.get(0)?;
    Ok(encoded
        .as_chunks::<4>()
        .0
        .iter()
        .copied()
        .map(u32::from_le_bytes)
        .collect())
}
