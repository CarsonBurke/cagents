//! Future proposal creations only. Registered in the published schema24 opener
//! transaction alongside maintained delivery metadata. No historical backfill
//! or lifecycle-table change belongs to this leaf.
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Registry;
use crate::automations::{Proposal, ProposalState, ProposalSummary};

const MAX_RECORD_BYTES: usize = 512;
const MAX_SUMMARY_BYTES: usize = 8192;
const MAX_PAGE: usize = 32;
const MAX_ATTRIBUTION_BYTES: usize = 64;

fn attribution_label(value: &str) -> Option<String> {
    let mut label = crate::notifications::presentation::title(value);
    let mut end = label.len().min(MAX_ATTRIBUTION_BYTES);
    while !label.is_char_boundary(end) {
        end -= 1;
    }
    label.truncate(end);
    let label = label.trim();
    (!label.is_empty()).then(|| label.to_owned())
}

pub(super) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(
        "CREATE TABLE automation_proposal_notifications (
            proposal TEXT PRIMARY KEY, registry TEXT NOT NULL,
            revision INTEGER NOT NULL CHECK(revision>0),
            created_ms INTEGER NOT NULL CHECK(created_ms>=0), record TEXT NOT NULL
         ) STRICT;
         CREATE INDEX pending_automation_proposal_notifications
             ON automation_proposal_notifications(created_ms,proposal);",
    )
    .map_err(|error| error.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Created {
    registry: Uuid,
    proposal: Uuid,
    revision: u64,
    created_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    requester: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    project: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::host) struct Entry {
    created: Created,
    record: String,
}

impl Entry {
    pub(in crate::host) fn target(&self) -> crate::notifications::Target {
        crate::notifications::Target::AutomationProposal {
            registry: self.created.registry,
            id: self.created.proposal,
            revision: self.created.revision,
        }
    }

    pub(in crate::host) fn attribution(&self) -> Option<String> {
        match (&self.created.requester, &self.created.project) {
            (Some(requester), Some(project)) => {
                Some(format!("Requested by {requester} · Project {project}."))
            }
            (Some(requester), None) => Some(format!("Requested by {requester}.")),
            (None, Some(project)) => Some(format!("Project {project}.")),
            (None, None) => None,
        }
    }

    pub(in crate::host) fn evidence_key(&self) -> String {
        format!(
            "proposal:{}:{}:revision:{}:created",
            self.created.registry, self.created.proposal, self.created.revision
        )
    }
}

fn identity(db: &Connection) -> Result<Uuid, String> {
    let raw: String = db
        .query_row("SELECT identity FROM metadata WHERE key=0", [], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())?;
    raw.parse::<Uuid>()
        .ok()
        .filter(|id| !id.is_nil())
        .ok_or_else(|| "The proposal notice registry identity is unavailable".into())
}

fn decode(
    registry: String,
    proposal: String,
    revision: u64,
    created_ms: u64,
    record: Option<String>,
) -> Result<Entry, String> {
    let record = record.ok_or("Proposal notice metadata exceeds its boundary")?;
    let created: Created =
        serde_json::from_str(&record).map_err(|_| "Unsupported proposal notice metadata")?;
    if created.registry.is_nil()
        || created.proposal.is_nil()
        || created.revision == 0
        || i64::try_from(created.revision).is_err()
        || i64::try_from(created.created_ms).is_err()
        || created.registry.to_string() != registry
        || created.proposal.to_string() != proposal
        || created.revision != revision
        || created.created_ms != created_ms
        || created
            .requester
            .as_deref()
            .is_some_and(|label| attribution_label(label).as_deref() != Some(label))
        || created.project.as_deref().is_some_and(|label| {
            attribution_label(label).as_deref() != Some(label) || label.contains('/')
        })
    {
        return Err("Proposal notice metadata does not match its source identity".into());
    }
    Ok(Entry { created, record })
}

/// Called immediately after the real INSERT, before its clock update/commit.
/// Any failure rolls back the original proposal as well as this source record.
pub(super) fn queue_in(
    tx: &Transaction<'_>,
    proposal: &Proposal,
    requester: Option<&str>,
) -> Result<(), String> {
    if proposal.id.is_nil() || proposal.revision == 0 || proposal.state != ProposalState::Pending {
        return Err("Only the newly inserted pending proposal can create attention".into());
    }
    let created = Created {
        registry: identity(tx)?,
        proposal: proposal.id,
        revision: proposal.revision,
        created_ms: proposal.created_ms,
        requester: requester.and_then(attribution_label),
        project: proposal
            .definition
            .agent
            .target
            .repository
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(attribution_label),
    };
    i64::try_from(created.revision).map_err(|_| "Invalid proposal revision")?;
    i64::try_from(created.created_ms).map_err(|_| "Invalid proposal creation time")?;
    let pending: bool = tx
        .query_row(
            "SELECT pending FROM automation_proposals WHERE id=?1",
            [proposal.id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !pending {
        return Err("The proposal source INSERT is unavailable".into());
    }
    let record = serde_json::to_string(&created).map_err(|error| error.to_string())?;
    if record.len() > MAX_RECORD_BYTES {
        return Err("Proposal notice metadata exceeds its boundary".into());
    }
    // Never replace an older/unknown source. Store only bounded optional
    // human attribution, never the prompt, full definition, or parent path.
    tx.execute(
        "INSERT INTO automation_proposal_notifications(proposal,registry,revision,created_ms,record)
         VALUES(?1,?2,?3,?4,?5)",
        params![created.proposal.to_string(), created.registry.to_string(), created.revision, created.created_ms, record],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

/// Only the real pending->terminal resolution transaction calls this. Not now,
/// a lost ACK, an old revision, or an unknown replacement is never suppression.
pub(super) fn resolved_in(tx: &Transaction<'_>, proposal: &Proposal) -> Result<(), String> {
    if proposal.state == ProposalState::Pending {
        return Err("A pending proposal cannot suppress its creation notice".into());
    }
    // Naming is immutable source metadata, not reconstructible from the
    // mutable runtime or destination channel. Read one bounded known-canonical
    // source in this existing resolution transaction, then delete its EXACT
    // captured bytes only when the full creation tuple still matches.
    let row: Option<(String, String, u64, u64, Option<String>)> = tx.query_row(
        "SELECT registry,proposal,revision,created_ms,CASE WHEN length(CAST(record AS BLOB))<=?2 THEN record END FROM automation_proposal_notifications WHERE proposal=?1",
        params![proposal.id.to_string(), MAX_RECORD_BYTES],
        |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?)),
    ).optional().map_err(|error|error.to_string())?;
    let Some((registry, id, revision, created_ms, record)) = row else {
        return Ok(());
    };
    let Ok(entry) = decode(registry, id, revision, created_ms, record) else {
        return Ok(());
    };
    let created = &entry.created;
    if created.registry != identity(tx)?
        || created.proposal != proposal.id
        || created.revision != proposal.revision
        || created.created_ms != proposal.created_ms
        || serde_json::to_string(created).map_err(|error| error.to_string())? != entry.record
    {
        return Ok(());
    }
    tx.execute("DELETE FROM automation_proposal_notifications WHERE proposal=?1 AND registry=?2 AND revision=?3 AND created_ms=?4 AND record=?5",
        params![created.proposal.to_string(),created.registry.to_string(),created.revision,created.created_ms,entry.record])
        .map_err(|error|error.to_string())?;
    Ok(())
}

fn pending_in(db: &Connection, limit: usize) -> Result<Vec<Entry>, String> {
    if !(1..=MAX_PAGE).contains(&limit) {
        return Err("Invalid proposal notice page size".into());
    }
    let current = identity(db)?;
    let mut query=db.prepare("SELECT registry,proposal,revision,created_ms,CASE WHEN length(CAST(record AS BLOB))<=?2 THEN record END FROM automation_proposal_notifications ORDER BY created_ms,proposal LIMIT ?1").map_err(|error|error.to_string())?;
    let rows = query
        .query_map(params![limit, MAX_RECORD_BYTES], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    rows.map(|row| {
        let (registry, proposal, revision, created_ms, record) =
            row.map_err(|error| error.to_string())?;
        let entry = decode(registry, proposal, revision, created_ms, record)?;
        if entry.created.registry != current {
            return Err("The queued proposal notice belongs to another registry".into());
        }
        Ok(entry)
    })
    .collect()
}

fn title_in(db: &Connection, entry: &Entry) -> Result<Option<String>, String> {
    let raw:Option<Option<String>>=db.query_row("SELECT CASE WHEN length(CAST(summary AS BLOB))<=?2 THEN summary END FROM automation_proposals WHERE id=?1",params![entry.created.proposal.to_string(),MAX_SUMMARY_BYTES],|row|row.get(0)).optional().map_err(|error|error.to_string())?;
    Ok(raw
        .flatten()
        .and_then(|raw| serde_json::from_str::<ProposalSummary>(&raw).ok())
        .filter(|row| row.id == entry.created.proposal && row.revision == entry.created.revision)
        .map(|row| crate::notifications::presentation::title(&row.name))
        .filter(|name| !name.is_empty()))
}

impl Registry {
    fn proposal_notice_read<T>(
        &self,
        read: impl FnOnce(&Connection) -> Result<T, String>,
    ) -> Result<T, String> {
        if !self.db.is_autocommit() {
            return Err("Proposal notices require their own read snapshot".into());
        }
        let tx = Transaction::new_unchecked(&self.db, rusqlite::TransactionBehavior::Deferred)
            .map_err(|error| error.to_string())?;
        let result = super::schema::require_current(&tx).and_then(|()| read(&tx));
        let release = if result.is_ok() {
            tx.commit()
        } else {
            tx.rollback()
        };
        if !self.db.is_autocommit() {
            return Err("The proposal notice read snapshot did not release".into());
        }
        release.map_err(|error| {
            format!("The proposal notice read snapshot could not close: {error}")
        })?;
        result
    }

    pub(crate) fn pending_proposal_notifications(
        &self,
        limit: usize,
    ) -> Result<Vec<Entry>, String> {
        self.proposal_notice_read(|db| pending_in(db, limit))
    }

    /// Header, identity, exact source and bounded optional title share ONE
    /// admitted observation. No borrowed row/transaction escapes preparation.
    pub(crate) fn prepare_proposal_notification(
        &self,
    ) -> Result<Option<(Entry, Option<String>)>, String> {
        self.proposal_notice_read(|db| {
            let Some(entry) = pending_in(db, 1)?.into_iter().next() else {
                return Ok(None);
            };
            let title = title_in(db, &entry)?;
            Ok(Some((entry, title)))
        })
    }

    /// The exact source ACK is one admitted immediate transaction: another
    /// writer cannot replace identity between its check and the DELETE.
    pub(crate) fn acknowledge_proposal_notification(
        &mut self,
        entry: &Entry,
    ) -> Result<(), String> {
        if !self.db.is_autocommit() {
            return Err("Proposal notice ACK requires its own transaction".into());
        }
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let result = (|| {
            super::schema::require_current(&tx)?;
            if identity(&tx)? != entry.created.registry {
                return Err("The proposal notice registry changed before acknowledgement".into());
            }
            tx.execute("DELETE FROM automation_proposal_notifications WHERE proposal=?1 AND registry=?2 AND revision=?3 AND created_ms=?4 AND record=?5",params![entry.created.proposal.to_string(),entry.created.registry.to_string(),entry.created.revision,entry.created.created_ms,entry.record]).map_err(|error|error.to_string())?;
            Ok(())
        })();
        let release = if result.is_ok() {
            tx.commit()
        } else {
            tx.rollback()
        };
        if !self.db.is_autocommit() {
            return Err("The proposal notice ACK transaction did not release".into());
        }
        release.map_err(|error| {
            format!("The proposal notice ACK transaction could not close: {error}")
        })?;
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Origin, Spec, Surface, Target},
        automations::{Definition, DirectoryIdentity, Schedule, SchedulePattern, SessionMode},
        model::ProjectKind,
        workspace_model::ModelEdit,
    };

    // Requires Root's real forward migration/admission to include migrate().
    // Do not inject this table around published-layout validation in tests.
    fn registry() -> (tempfile::TempDir, Registry, Definition) {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().to_owned();
        let mut registry = Registry::at(&path.join("model.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: path.clone(),
                name: "Fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let definition = Definition {
            name: "Private fixture name".into(),
            prompt: "Never notification metadata".into(),
            agent: Spec {
                provider: Provider::Codex,
                account: None,
                target: Target {
                    repository: path.clone(),
                    directory: path,
                    placement: None,
                },
                surface: Surface::Chat,
                model: None,
                reasoning_effort: None,
                permission_mode: None,
            },
            schedule: Schedule {
                timezone: "UTC".into(),
                pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
            },
            session_mode: SessionMode::Fresh,
            precheck: None,
        };
        (root, registry, definition)
    }

    fn propose(
        registry: &mut Registry,
        id: Uuid,
        definition: &Definition,
    ) -> Result<Proposal, String> {
        registry.propose_automation(
            id,
            Origin::User,
            definition.clone(),
            definition.clone(),
            DirectoryIdentity {
                device: 1,
                inode: 2,
            },
            None,
            1,
        )
    }

    #[test]
    fn a_failed_source_insert_rolls_back_the_actual_proposal_and_exact_retry_queues_once() {
        let (_root, mut registry, definition) = registry();
        registry.db.execute_batch("CREATE TEMP TRIGGER fail_proposal_notice BEFORE INSERT ON automation_proposal_notifications BEGIN SELECT RAISE(ABORT,'source fault'); END;").unwrap();
        let id = Uuid::new_v4();
        assert!(propose(&mut registry, id, &definition).is_err());
        assert!(registry.automation_proposal(id, None).unwrap().is_none());
        assert!(
            registry
                .pending_proposal_notifications(1)
                .unwrap()
                .is_empty()
        );
        registry
            .db
            .execute_batch("DROP TRIGGER fail_proposal_notice;")
            .unwrap();
        let original = propose(&mut registry, id, &definition).unwrap();
        let entry = registry
            .pending_proposal_notifications(1)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(entry.created.proposal, id);
        assert!(!entry.record.contains(&definition.prompt));
        assert_eq!(propose(&mut registry, id, &definition).unwrap(), original);
        assert_eq!(
            registry.pending_proposal_notifications(32).unwrap(),
            vec![entry.clone()]
        );
        let mut changed = definition.clone();
        changed.prompt = "Changed original intent".into();
        assert!(propose(&mut registry, id, &changed).is_err());
        assert_eq!(
            registry.pending_proposal_notifications(32).unwrap(),
            vec![entry]
        );
    }

    #[test]
    fn real_resolution_suppresses_only_its_exact_source_and_old_ack_preserves_replacement() {
        let (_root, mut registry, definition) = registry();
        let first = propose(&mut registry, Uuid::new_v4(), &definition).unwrap();
        let second = propose(&mut registry, Uuid::new_v4(), &definition).unwrap();
        let first_entry = registry
            .pending_proposal_notifications(32)
            .unwrap()
            .into_iter()
            .find(|entry| entry.created.proposal == first.id)
            .unwrap();
        let receipt = registry
            .resolve_automation_proposal(&first, None, 2)
            .unwrap();
        assert_eq!(receipt.state, ProposalState::Declined);
        let remaining = registry.pending_proposal_notifications(32).unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].created.proposal, second.id);
        registry
            .acknowledge_proposal_notification(&first_entry)
            .unwrap();
        assert_eq!(
            registry.pending_proposal_notifications(32).unwrap(),
            remaining
        );
        // An unknown replacement of the original serialized metadata is not
        // consumed by the old exact ACK or a synthesized suppression record.
        let entry = remaining[0].clone();
        let replacement = entry.record.replace("\"revision\":1", "\"revision\":2");
        assert_ne!(replacement, entry.record);
        registry.db.execute("UPDATE automation_proposal_notifications SET revision=2,record=?2 WHERE proposal=?1",params![second.id.to_string(),replacement]).unwrap();
        registry.acknowledge_proposal_notification(&entry).unwrap();
        let replaced = registry
            .pending_proposal_notifications(1)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(replaced.created.revision, 2);
        registry
            .resolve_automation_proposal(&second, None, 3)
            .unwrap();
        assert_eq!(
            registry.pending_proposal_notifications(1).unwrap(),
            vec![replaced]
        );
    }

    #[test]
    fn retained_registry_refuses_future_header_and_changed_identity_without_deleting_source() {
        let (root, mut registry, definition) = registry();
        propose(&mut registry, Uuid::new_v4(), &definition).unwrap();
        let entry = registry
            .pending_proposal_notifications(1)
            .unwrap()
            .pop()
            .unwrap();
        let writer = Connection::open(root.path().join("model.sqlite3")).unwrap();
        let current = super::super::SCHEMA_VERSION;
        writer
            .pragma_update(None, "user_version", current + 1)
            .unwrap();
        assert!(registry.pending_proposal_notifications(1).is_err());
        assert!(registry.prepare_proposal_notification().is_err());
        assert!(registry.acknowledge_proposal_notification(&entry).is_err());
        assert!(registry.db.is_autocommit());
        let count: u64 = writer
            .query_row(
                "SELECT count(*) FROM automation_proposal_notifications",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
        writer.pragma_update(None, "user_version", current).unwrap();
        // Change identity AFTER the real preparation snapshot and BEFORE ACK.
        assert!(registry.prepare_proposal_notification().unwrap().is_some());
        writer
            .execute(
                "UPDATE metadata SET identity=?1 WHERE key=0",
                [Uuid::new_v4().to_string()],
            )
            .unwrap();
        assert!(registry.acknowledge_proposal_notification(&entry).is_err());
        assert!(registry.prepare_proposal_notification().is_err());
        assert!(registry.db.is_autocommit());
        let count: u64 = writer
            .query_row(
                "SELECT count(*) FROM automation_proposal_notifications",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn creation_attribution_is_bounded_immutable_optional_and_resolution_preserves_unknown_sources()
    {
        let (_root, mut registry, definition) = registry();
        let id = Uuid::new_v4();
        let requester = format!("  Human conversation\n{}", "\"".repeat(100));
        let proposal = registry
            .propose_automation_with_requester(
                id,
                Origin::User,
                definition.clone(),
                definition.clone(),
                DirectoryIdentity {
                    device: 1,
                    inode: 2,
                },
                Some("Destination channel is not the requester".into()),
                Some(&requester),
                1,
            )
            .unwrap();
        let entry = registry
            .pending_proposal_notifications(1)
            .unwrap()
            .pop()
            .unwrap();
        assert!(entry.record.len() <= MAX_RECORD_BYTES);
        let captured = entry.created.requester.as_ref().unwrap();
        assert!(captured.starts_with("Human conversation"));
        assert!(captured.len() <= MAX_ATTRIBUTION_BYTES && !captured.contains('\n'));
        assert_eq!(
            entry.created.project.as_deref(),
            definition
                .agent
                .target
                .repository
                .file_name()
                .and_then(|name| name.to_str())
        );
        assert!(!entry.record.contains("Destination channel"));
        assert!(!entry.record.contains(&definition.prompt));
        assert!(
            !entry.record.contains(
                definition
                    .agent
                    .target
                    .repository
                    .parent()
                    .unwrap()
                    .to_str()
                    .unwrap()
            )
        );
        assert_eq!(
            registry
                .propose_automation_with_requester(
                    id,
                    Origin::User,
                    definition.clone(),
                    definition.clone(),
                    proposal.target_identity.clone(),
                    None,
                    Some("A later mutable title"),
                    2,
                )
                .unwrap(),
            proposal
        );
        assert_eq!(
            registry.pending_proposal_notifications(1).unwrap(),
            vec![entry]
        );
        registry
            .resolve_automation_proposal(&proposal, None, 3)
            .unwrap();
        assert!(
            registry
                .pending_proposal_notifications(1)
                .unwrap()
                .is_empty()
        );

        // The exact pre-attribution canonical format remains suppressible.
        let old = propose(&mut registry, Uuid::new_v4(), &definition).unwrap();
        let mut entry = registry
            .pending_proposal_notifications(1)
            .unwrap()
            .pop()
            .unwrap();
        entry.created.requester = None;
        entry.created.project = None;
        let old_record = serde_json::to_string(&entry.created).unwrap();
        registry
            .db
            .execute(
                "UPDATE automation_proposal_notifications SET record=?2 WHERE proposal=?1",
                params![old.id.to_string(), old_record],
            )
            .unwrap();
        registry.resolve_automation_proposal(&old, None, 4).unwrap();
        assert!(
            registry
                .pending_proposal_notifications(1)
                .unwrap()
                .is_empty()
        );

        // Unknown extensions, noncanonical replacement bytes and a changed
        // creation time are not silently consumed as this proposal's source.
        for replacement in 0..3 {
            let proposal = propose(&mut registry, Uuid::new_v4(), &definition).unwrap();
            let entry = registry
                .pending_proposal_notifications(1)
                .unwrap()
                .pop()
                .unwrap();
            let mut created = entry.created.clone();
            let raw = match replacement {
                0 => format!(
                    "{},\"unknown\":true}}",
                    entry.record.strip_suffix('}').unwrap()
                ),
                1 => format!("{} ", entry.record),
                _ => {
                    created.created_ms += 1;
                    serde_json::to_string(&created).unwrap()
                }
            };
            registry.db.execute("UPDATE automation_proposal_notifications SET created_ms=?2,record=?3 WHERE proposal=?1", params![proposal.id.to_string(),created.created_ms,raw]).unwrap();
            registry.acknowledge_proposal_notification(&entry).unwrap();
            registry
                .resolve_automation_proposal(&proposal, None, 5)
                .unwrap();
            let retained: String = registry
                .db
                .query_row(
                    "SELECT record FROM automation_proposal_notifications WHERE proposal=?1",
                    [proposal.id.to_string()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(retained, raw);
            // Fixture teardown of this intentionally unsupported source keeps
            // the next independent case's bounded read unobstructed.
            registry
                .db
                .execute(
                    "DELETE FROM automation_proposal_notifications WHERE proposal=?1",
                    [proposal.id.to_string()],
                )
                .unwrap();
        }
    }
}
