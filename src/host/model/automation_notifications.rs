//! Current actionable automation attention, separate from retained audit history.
//!
//! Unwired leaf: the owning migration must call `migrate`, and model hooks call
//! `queue` immediately after the same transaction's lifecycle audit write. No
//! historical audit scan, provider/run dependency, title, path or diagnostic is
//! an attention producer. There is one pending slot per automation identity;
//! only a newer causal lifecycle receipt supersedes its older actionable edge.
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use uuid::Uuid;

use crate::automations::{Automation, LifecycleActor, LifecycleCause, LifecycleEvent};

pub(super) const PAGE_SIZE: usize = 100;
const MAX_RECORD_BYTES: usize = 2048;

/// No fixed global row cap can prevent a safety disable/project close. Storage
/// is bounded by existing automation identities, not all historical transitions.
/// No cascade can erase an unacknowledged row when its definition is removed.
pub(super) const SCHEMA_SQL: &str = "CREATE TABLE automation_notifications (
    automation TEXT PRIMARY KEY REFERENCES automations(id),
    sequence INTEGER NOT NULL UNIQUE CHECK(sequence > 0),
    record TEXT NOT NULL
) STRICT;";

pub(super) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(SCHEMA_SQL).map_err(|e| e.to_string())
}

/// Immutable delivery identity and fixed metadata only. An Entry is a pending
/// observation, never proof that the notification journal or desktop committed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(in crate::host) struct Entry {
    event: LifecycleEvent,
    record: String,
}

impl Entry {
    pub(in crate::host) fn event(&self) -> &LifecycleEvent {
        &self.event
    }

    pub(in crate::host) fn evidence_key(&self) -> String {
        format!(
            "automation:{}:lifecycle:{}",
            self.event.automation, self.event.sequence
        )
    }
}

fn actionable(cause: LifecycleCause) -> bool {
    matches!(
        cause,
        LifecycleCause::ProjectClosed
            | LifecycleCause::ProjectUnavailable
            | LifecycleCause::StartPaused
            | LifecycleCause::PrecheckInterrupted
            | LifecycleCause::PrecheckPaused
    )
}

fn validate(event: &LifecycleEvent) -> Result<(), String> {
    if event.automation.is_nil()
        || event.firing.is_some_and(|id| id.is_nil())
        || event.sequence == 0
        || i64::try_from(event.sequence).is_err()
        || event.revision == 0
        || i64::try_from(event.revision).is_err()
        || i64::try_from(event.occurred_ms).is_err()
        || event.actor != LifecycleActor::System
        || !actionable(event.cause)
        || event.enabled
    {
        return Err("Invalid automation attention metadata".into());
    }
    Ok(())
}

fn decode(sequence: u64, automation: &str, record: String) -> Result<Entry, String> {
    if record.len() > MAX_RECORD_BYTES {
        return Err("Automation attention metadata is too large".into());
    }
    let event: LifecycleEvent = serde_json::from_str(&record)
        .map_err(|_| "Invalid automation attention metadata".to_owned())?;
    validate(&event)?;
    if event.sequence != sequence || event.automation.to_string() != automation {
        return Err("Automation attention identity does not match its record".into());
    }
    // Preserve unknown/new metadata rather than silently dropping it on ACK.
    let supplied: serde_json::Value = serde_json::from_str(&record)
        .map_err(|_| "Invalid automation attention metadata".to_owned())?;
    let expected = serde_json::to_value(&event).map_err(|e| e.to_string())?;
    if supplied != expected {
        return Err("Unsupported automation attention metadata".into());
    }
    Ok(Entry { event, record })
}

fn current(db: &Connection, automation: Uuid) -> Result<Option<Entry>, String> {
    let raw: Option<(u64, Option<String>)> = db
        .query_row(
            "SELECT sequence, CASE WHEN length(CAST(record AS BLOB)) <= ?2 THEN record END
             FROM automation_notifications WHERE automation=?1",
            params![automation.to_string(), MAX_RECORD_BYTES],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    raw.map(|(sequence, record)| {
        decode(
            sequence,
            &automation.to_string(),
            record.ok_or("Automation attention metadata is too large")?,
        )
    })
    .transpose()
}

fn latest_audit(db: &Connection, automation: Uuid) -> Result<LifecycleEvent, String> {
    let (sequence, record): (u64, Option<String>) = db
        .query_row(
            "SELECT sequence, CASE WHEN length(CAST(record AS BLOB)) <= ?2 THEN record END
             FROM automation_lifecycle WHERE automation=?1 ORDER BY sequence DESC LIMIT 1",
            params![automation.to_string(), MAX_RECORD_BYTES],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|_| "The causal automation lifecycle receipt is unavailable".to_owned())?;
    let mut event: LifecycleEvent = serde_json::from_str(
        &record.ok_or("The causal automation lifecycle receipt is too large")?,
    )
    .map_err(|_| "The causal automation lifecycle receipt is invalid".to_owned())?;
    if sequence == 0 || event.sequence != 0 || event.automation != automation {
        return Err("The causal automation lifecycle receipt is invalid".into());
    }
    event.sequence = sequence;
    Ok(event)
}

/// Queue only the exact latest fixed system receipt written by this state
/// transition. Supersession is explicit: the audit's bounded history remains
/// separate, while this slot represents the newest currently actionable cause.
/// Caller must roll back its transaction on any storage/corruption error.
pub(super) fn queue(
    tx: &Transaction<'_>,
    automation: &Automation,
    cause: LifecycleCause,
    firing: Option<Uuid>,
) -> Result<(), String> {
    let event = latest_audit(tx, automation.id)?;
    validate(&event)?;
    if event.revision != automation.revision
        || event.cause != cause
        || event.firing != firing
        || event.occurred_ms != automation.updated_ms
        || event.enabled != automation.enabled
        || event.blocked != automation.blocked.is_some()
    {
        return Err("Automation attention does not match its causal state transition".into());
    }
    let state: (u64, bool, u64) = tx
        .query_row(
            "SELECT revision,enabled,updated_ms FROM automations WHERE id=?1",
            [automation.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .map_err(|e| e.to_string())?;
    if state != (event.revision, false, event.occurred_ms) {
        return Err("Automation attention does not match the saved state".into());
    }
    // Accepted provider runs already have AgentRun attention. Only exact
    // no-run state transitions belong to this first slice; never duplicate
    // accepted-run delivery or change its lifecycle ownership.
    for id in [firing, automation.active_firing].into_iter().flatten() {
        let (owner, run): (String, Option<String>) = tx
            .query_row(
                "SELECT automation,run_id FROM automation_firings WHERE id=?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|_| "The exact automation firing is unavailable".to_owned())?;
        if owner != automation.id.to_string() {
            return Err("Automation attention firing belongs to another definition".into());
        }
        if run.is_some() {
            return Ok(());
        }
    }
    if let Some(previous) = current(tx, automation.id)? {
        if previous.event.sequence > event.sequence {
            return Err("Automation attention cannot replace a newer receipt".into());
        }
        if previous.event.sequence == event.sequence {
            return if previous.event == event {
                Ok(())
            } else {
                Err("Automation attention receipt identity was reused".into())
            };
        }
    }
    let record = serde_json::to_string(&event).map_err(|e| e.to_string())?;
    if record.len() > MAX_RECORD_BYTES {
        return Err("Automation attention metadata is too large".into());
    }
    tx.execute(
        "INSERT INTO automation_notifications(automation,sequence,record) VALUES(?1,?2,?3)
         ON CONFLICT(automation) DO UPDATE SET sequence=excluded.sequence,record=excluded.record
         WHERE excluded.sequence>automation_notifications.sequence",
        params![automation.id.to_string(), event.sequence, record],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// Read only a finite oldest-first page. Reading/enqueueing is not ACK. The
/// sole transfer owner permits one selected/in-flight snapshot. Independent
/// model connections may supersede or suppress it before enqueue; it remains
/// historical evidence, and its exact ACK cannot consume a replacement. Release
/// Registry/Host/control guards before observing any journal receipt.
pub(super) fn pending(db: &Connection, limit: usize) -> Result<Vec<Entry>, String> {
    if !(1..=PAGE_SIZE).contains(&limit) {
        return Err("Invalid automation attention page size".into());
    }
    let mut query = db
        .prepare(
            "SELECT sequence,automation,
             CASE WHEN length(CAST(record AS BLOB)) <= ?2 THEN record END
             FROM automation_notifications ORDER BY sequence LIMIT ?1",
        )
        .map_err(|e| e.to_string())?;
    let rows = query
        .query_map(params![limit, MAX_RECORD_BYTES], |row| {
            Ok((
                row.get::<_, u64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })
        .map_err(|e| e.to_string())?;
    rows.map(|row| {
        let (sequence, automation, record) = row.map_err(|e| e.to_string())?;
        decode(
            sequence,
            &automation,
            record.ok_or("Automation attention metadata is too large")?,
        )
    })
    .collect()
}

/// Delivery ACK only. The caller owns the real `record_committed` Result
/// receiver: enqueue success, channel EOF, timeout, full/stopped queue, or desktop
/// acceptance is not journal success and MUST NOT call this function.
/// A late ACK cannot consume a newer cause or a same-ID changed record.
pub(super) fn acknowledge(tx: &Transaction<'_>, committed: &[Entry]) -> Result<usize, String> {
    if committed.len() > PAGE_SIZE {
        return Err("Too many automation attention acknowledgments".into());
    }
    for entry in committed {
        validate(&entry.event)?;
    }
    let mut removed = 0;
    for entry in committed {
        removed += tx.execute(
            "DELETE FROM automation_notifications WHERE automation=?1 AND sequence=?2 AND record=?3",
            params![entry.event.automation.to_string(), entry.event.sequence, entry.record],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(removed)
}

/// Causal resolution, not delivery ACK. Suppress only this firing's older,
/// still-pending precheck interruption after its exact recovered audit receipt.
/// Already-selected/enqueued/committed snapshots cannot be retracted by this
/// leaf; activation must show the historical cause and current authorized status.
pub(super) fn suppress_precheck(
    tx: &Transaction<'_>,
    automation: Uuid,
    firing: Uuid,
) -> Result<(), String> {
    if automation.is_nil() || firing.is_nil() {
        return Err("Invalid precheck recovery identity".into());
    }
    let recovered = latest_audit(tx, automation)?;
    if recovered.cause != LifecycleCause::PrecheckRecovered
        || recovered.actor != LifecycleActor::System
        || recovered.firing != Some(firing)
        || recovered.blocked
        || recovered.enabled
    {
        return Err("The exact precheck recovery receipt is unavailable".into());
    }
    let Some(entry) = current(tx, automation)? else {
        return Ok(());
    };
    if entry.event.sequence < recovered.sequence
        && entry.event.firing == Some(firing)
        && matches!(
            entry.event.cause,
            LifecycleCause::PrecheckInterrupted | LifecycleCause::PrecheckPaused
        )
    {
        // Exact bytes prevent this semantic resolution from consuming any
        // replacement. No generic automation-wide notification retirement.
        tx.execute(
            "DELETE FROM automation_notifications WHERE automation=?1 AND sequence=?2 AND record=?3",
            params![automation.to_string(), entry.event.sequence, entry.record],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

impl super::Registry {
    pub(in crate::host) fn pending_automation_notifications(
        &self,
        limit: usize,
    ) -> Result<Vec<Entry>, String> {
        pending(&self.db, limit)
    }

    /// `false` means the exact old entry is absent or has been superseded, not
    /// that the current slot was acknowledged. Storage failure retains custody.
    pub(in crate::host) fn acknowledge_automation_notification(
        &mut self,
        committed: &Entry,
    ) -> Result<bool, String> {
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        let removed = acknowledge(&tx, std::slice::from_ref(committed))?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(removed == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Provider;
    use crate::agent_execution::{Spec, Surface, Target};
    use crate::automations::{
        Definition, DirectoryIdentity, Schedule, SchedulePattern, SessionMode,
    };
    use crate::host::model::{Registry, automation_audit};
    use crate::model::ProjectKind;
    use crate::workspace_model::ModelEdit;

    fn fixture() -> (Registry, tempfile::TempDir, Automation) {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        // The coordinated published-layout migration must install the table.
        // Never inject an unregistered table and then bypass schema validation.
        assert!(
            registry
                .pending_automation_notifications(PAGE_SIZE)
                .unwrap()
                .is_empty()
        );
        let repository = directory.path().to_owned();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: repository.clone(),
                name: "Fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let automation = registry
            .create_automation(
                Definition {
                    name: "PRIVATE_TITLE".into(),
                    prompt: "PRIVATE_PROMPT".into(),
                    agent: Spec {
                        provider: Provider::Codex,
                        account: None,
                        target: Target {
                            repository: repository.clone(),
                            directory: repository,
                            placement: None,
                        },
                        surface: Surface::Chat,
                        model: None,
                        permission_mode: None,
                        reasoning_effort: None,
                    },
                    schedule: Schedule {
                        pattern: SchedulePattern::Daily { hour: 9, minute: 0 },
                        timezone: "UTC".into(),
                    },
                    session_mode: SessionMode::Reuse,
                    precheck: None,
                },
                100,
                DirectoryIdentity {
                    device: 1,
                    inode: 1,
                },
            )
            .unwrap();
        (registry, directory, automation)
    }

    // Test-only state preparation uses the real Registry schema and real audit
    // writer in one transaction; no schedule, provider or broker is executed.
    fn stage(
        tx: &Transaction<'_>,
        automation: &mut Automation,
        cause: LifecycleCause,
        firing: Option<Uuid>,
        time: u64,
    ) {
        automation.updated_ms = time;
        automation.enabled = false;
        automation.next = None;
        tx.execute("UPDATE automations SET record=?2,summary=?3,enabled=0,next_due_ms=NULL,updated_ms=?4 WHERE id=?1",
            params![automation.id.to_string(),serde_json::to_string(automation).unwrap(),serde_json::to_string(&automation.summary(None)).unwrap(),time]).unwrap();
        if let Some(id) = firing {
            let firing_record = crate::automations::Firing {
                id,
                automation: automation.id,
                name: automation.definition.name.clone(),
                revision: automation.revision,
                trigger: crate::automations::Trigger::Manual { request: id },
                state: crate::automations::FiringState::Interrupted,
                release_pending: false,
                cancel_requested: false,
                created_ms: time,
                started_ms: None,
                finished_ms: Some(time),
                agent: None,
                run: None,
                skip: None,
                diagnostic: None,
            };
            tx.execute("INSERT OR IGNORE INTO automation_firings(id,automation,revision,occurrence_key,record,created_ms,state,run_id)
                VALUES(?1,?2,?3,?4,?5,?6,'interrupted',NULL)",
                params![id.to_string(),automation.id.to_string(),automation.revision,format!("manual:{id}"),serde_json::to_string(&firing_record).unwrap(),time]).unwrap();
        }
        automation_audit::record(tx, automation, cause, firing).unwrap();
    }

    fn emit(
        registry: &mut Registry,
        automation: &mut Automation,
        cause: LifecycleCause,
        firing: Option<Uuid>,
        time: u64,
    ) -> Entry {
        let tx = registry.db.transaction().unwrap();
        stage(&tx, automation, cause, firing, time);
        queue(&tx, automation, cause, firing).unwrap();
        tx.commit().unwrap();
        current(&registry.db, automation.id).unwrap().unwrap()
    }

    #[test]
    fn unacknowledged_attention_survives_read_and_reopen_until_exact_journal_ack() {
        let (mut registry, directory, mut automation) = fixture();
        let entry = emit(
            &mut registry,
            &mut automation,
            LifecycleCause::ProjectClosed,
            None,
            200,
        );
        assert_eq!(pending(&registry.db, 100).unwrap(), [entry.clone()]);
        assert_eq!(
            pending(&registry.db, 100).unwrap(),
            [entry.clone()],
            "full/stopped/error delivery has no ACK and cannot consume the slot"
        );
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(pending(&registry.db, 100).unwrap(), [entry.clone()]);
        let tx = registry.db.transaction().unwrap();
        acknowledge(&tx, &[entry.clone()]).unwrap();
        drop(tx);
        assert_eq!(
            pending(&registry.db, 100).unwrap(),
            [entry.clone()],
            "an uncommitted ACK is not durable"
        );
        let tx = registry.db.transaction().unwrap();
        acknowledge(&tx, &[entry]).unwrap();
        tx.commit().unwrap();
        assert!(pending(&registry.db, 100).unwrap().is_empty());
    }

    #[test]
    fn newer_actionable_receipt_supersedes_one_slot_and_old_ack_cannot_delete_it() {
        let (mut registry, _directory, mut automation) = fixture();
        let original = emit(
            &mut registry,
            &mut automation,
            LifecycleCause::ProjectClosed,
            None,
            200,
        );
        let firing = Uuid::new_v4();
        let mut latest = original.clone();
        for time in 201..601 {
            latest = emit(
                &mut registry,
                &mut automation,
                LifecycleCause::PrecheckPaused,
                Some(firing),
                time,
            );
            assert_eq!(pending(&registry.db, 100).unwrap(), [latest.clone()]);
        }
        assert!(latest.event().sequence > original.event().sequence);
        assert_ne!(latest.evidence_key(), original.evidence_key());
        let tx = registry.db.transaction().unwrap();
        acknowledge(&tx, &[original]).unwrap();
        tx.commit().unwrap();
        assert_eq!(pending(&registry.db, 100).unwrap(), [latest]);
        assert_eq!(
            registry
                .automation_audit(automation.id, None, 100)
                .unwrap()
                .events
                .len(),
            100,
            "audit remains a separate bounded paged history"
        );
    }

    #[test]
    fn duplicate_same_receipt_is_idempotent_but_late_or_forged_cause_cannot_replace() {
        let (mut registry, _directory, mut automation) = fixture();
        let original = emit(
            &mut registry,
            &mut automation,
            LifecycleCause::ProjectClosed,
            None,
            200,
        );
        let tx = registry.db.transaction().unwrap();
        queue(&tx, &automation, LifecycleCause::ProjectClosed, None).unwrap();
        assert!(queue(&tx, &automation, LifecycleCause::StartPaused, None).is_err());
        tx.commit().unwrap();
        assert_eq!(pending(&registry.db, 100).unwrap(), [original.clone()]);
        let old = automation.clone();
        let latest = emit(
            &mut registry,
            &mut automation,
            LifecycleCause::StartPaused,
            Some(Uuid::new_v4()),
            201,
        );
        let tx = registry.db.transaction().unwrap();
        assert!(queue(&tx, &old, LifecycleCause::ProjectClosed, None).is_err());
        tx.commit().unwrap();
        assert_eq!(pending(&registry.db, 100).unwrap(), [latest]);
    }

    #[test]
    fn insertion_failure_rolls_back_state_audit_and_previous_actionable_slot() {
        let (mut registry, _directory, mut automation) = fixture();
        let original = emit(
            &mut registry,
            &mut automation,
            LifecycleCause::ProjectClosed,
            None,
            200,
        );
        let state = registry.automation(automation.id).unwrap();
        let audit = registry.automation_audit(automation.id, None, 100).unwrap();
        registry.db.execute_batch("CREATE TEMP TRIGGER reject_attention BEFORE UPDATE ON automation_notifications BEGIN SELECT RAISE(ABORT,'attention fixture failure'); END;").unwrap();
        let tx = registry.db.transaction().unwrap();
        stage(&tx, &mut automation, LifecycleCause::StartPaused, None, 300);
        assert!(queue(&tx, &automation, LifecycleCause::StartPaused, None).is_err());
        drop(tx);
        assert_eq!(registry.automation(automation.id).unwrap(), state);
        assert_eq!(
            registry.automation_audit(automation.id, None, 100).unwrap(),
            audit
        );
        assert_eq!(pending(&registry.db, 100).unwrap(), [original]);
    }

    #[test]
    fn explicit_recovery_suppresses_only_exact_older_pending_precheck_firing() {
        let (mut registry, _directory, mut automation) = fixture();
        let firing = Uuid::new_v4();
        let other = Uuid::new_v4();
        let interrupted = emit(
            &mut registry,
            &mut automation,
            LifecycleCause::PrecheckInterrupted,
            Some(firing),
            200,
        );
        let tx = registry.db.transaction().unwrap();
        stage(
            &tx,
            &mut automation,
            LifecycleCause::PrecheckRecovered,
            Some(other),
            201,
        );
        suppress_precheck(&tx, automation.id, other).unwrap();
        tx.commit().unwrap();
        assert_eq!(pending(&registry.db, 100).unwrap(), [interrupted]);
        let tx = registry.db.transaction().unwrap();
        assert!(
            suppress_precheck(&tx, automation.id, firing).is_err(),
            "wrong recovered receipt cannot resolve a different firing"
        );
        stage(
            &tx,
            &mut automation,
            LifecycleCause::PrecheckRecovered,
            Some(firing),
            202,
        );
        suppress_precheck(&tx, automation.id, firing).unwrap();
        drop(tx);
        assert_eq!(
            pending(&registry.db, 100).unwrap().len(),
            1,
            "recovery rollback preserves pending evidence"
        );
        let tx = registry.db.transaction().unwrap();
        stage(
            &tx,
            &mut automation,
            LifecycleCause::PrecheckRecovered,
            Some(firing),
            203,
        );
        suppress_precheck(&tx, automation.id, firing).unwrap();
        tx.commit().unwrap();
        assert!(pending(&registry.db, 100).unwrap().is_empty());
        let newer = emit(
            &mut registry,
            &mut automation,
            LifecycleCause::PrecheckInterrupted,
            Some(other),
            204,
        );
        let tx = registry.db.transaction().unwrap();
        stage(
            &tx,
            &mut automation,
            LifecycleCause::PrecheckRecovered,
            Some(firing),
            205,
        );
        suppress_precheck(&tx, automation.id, firing).unwrap();
        tx.commit().unwrap();
        assert_eq!(
            pending(&registry.db, 100).unwrap(),
            [newer],
            "recovering an old helper cannot remove newer blocked attention"
        );
    }

    #[test]
    fn pending_metadata_has_no_name_prompt_path_diagnostic_or_implicit_user_pause() {
        let (mut registry, directory, mut automation) = fixture();
        automation.blocked = Some("PRIVATE_DIAGNOSTIC".into());
        let entry = emit(
            &mut registry,
            &mut automation,
            LifecycleCause::PrecheckInterrupted,
            Some(Uuid::new_v4()),
            200,
        );
        let raw = entry.record.clone();
        for private in [
            "PRIVATE_TITLE",
            "PRIVATE_PROMPT",
            "PRIVATE_DIAGNOSTIC",
            directory.path().to_str().unwrap(),
        ] {
            assert!(!raw.contains(private));
        }
        let tx = registry.db.transaction().unwrap();
        stage(&tx, &mut automation, LifecycleCause::Disabled, None, 201);
        assert!(queue(&tx, &automation, LifecycleCause::Disabled, None).is_err());
        tx.commit().unwrap();
        assert_eq!(pending(&registry.db, 100).unwrap(), [entry]);
        assert!(pending(&registry.db, 0).is_err());
        assert!(pending(&registry.db, PAGE_SIZE + 1).is_err());
    }

    #[test]
    fn corrupt_or_newer_metadata_is_retained_and_cannot_be_acknowledged_by_old_entry() {
        let (mut registry, _directory, mut automation) = fixture();
        let entry = emit(
            &mut registry,
            &mut automation,
            LifecycleCause::ProjectClosed,
            None,
            200,
        );
        let mut value = serde_json::to_value(entry.event()).unwrap();
        value["unexpected_future_field"] = true.into();
        let corrupt = value.to_string();
        registry
            .db
            .execute("UPDATE automation_notifications SET record=?1", [&corrupt])
            .unwrap();
        assert!(pending(&registry.db, 100).is_err());
        let tx = registry.db.transaction().unwrap();
        acknowledge(&tx, &[entry]).unwrap();
        tx.commit().unwrap();
        let actual: String = registry
            .db
            .query_row("SELECT record FROM automation_notifications", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(actual, corrupt);
    }
    #[test]
    fn fifo_pages_and_independent_writer_preserve_newer_slot_under_late_ack() {
        let (mut registry, _directory, mut first) = fixture();
        let mut second = registry
            .create_automation(
                first.definition.clone(),
                101,
                DirectoryIdentity {
                    device: 1,
                    inode: 1,
                },
            )
            .unwrap();
        let old = emit(
            &mut registry,
            &mut first,
            LifecycleCause::ProjectClosed,
            None,
            200,
        );
        let middle = emit(
            &mut registry,
            &mut second,
            LifecycleCause::ProjectUnavailable,
            None,
            201,
        );
        let mut writer = registry.execution_connection().unwrap();
        let latest = emit(
            &mut writer,
            &mut first,
            LifecycleCause::StartPaused,
            Some(Uuid::new_v4()),
            202,
        );
        assert_eq!(
            registry.pending_automation_notifications(1).unwrap(),
            [middle.clone()]
        );
        assert_eq!(
            registry
                .pending_automation_notifications(PAGE_SIZE)
                .unwrap(),
            [middle.clone(), latest.clone()]
        );
        assert!(!registry.acknowledge_automation_notification(&old).unwrap());
        assert_eq!(
            registry
                .pending_automation_notifications(PAGE_SIZE)
                .unwrap(),
            [middle.clone(), latest.clone()]
        );
        assert!(
            registry
                .acknowledge_automation_notification(&middle)
                .unwrap()
        );
        assert!(
            !registry
                .acknowledge_automation_notification(&middle)
                .unwrap()
        );
        assert_eq!(
            writer.pending_automation_notifications(1).unwrap(),
            [latest]
        );
    }

    #[test]
    fn migration_does_not_backfill_historical_audit_into_attention() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE automations(id TEXT PRIMARY KEY) STRICT;
            CREATE TABLE automation_lifecycle(sequence INTEGER PRIMARY KEY,record TEXT NOT NULL) STRICT;
            INSERT INTO automation_lifecycle VALUES(1,'old audit is not notification input');").unwrap();
        let tx = db.transaction().unwrap();
        migrate(&tx).unwrap();
        tx.commit().unwrap();
        assert!(pending(&db, PAGE_SIZE).unwrap().is_empty());
        assert_eq!(
            db.query_row("SELECT count(*) FROM automation_lifecycle", [], |row| row
                .get::<_, u64>(
                0
            ))
            .unwrap(),
            1
        );
    }
    #[test]
    fn accepted_run_is_not_duplicated_and_fixed_run_causes_are_out_of_scope() {
        let (mut registry, _directory, mut automation) = fixture();
        let trigger = crate::automations::Trigger::Manual {
            request: Uuid::new_v4(),
        };
        let claimed = registry
            .claim_automation(
                automation.id,
                automation.revision,
                None,
                trigger,
                None,
                true,
                crate::host::agent_execution::concurrency_limits(),
                200,
            )
            .unwrap();
        assert!(claimed.accepted.is_some());
        assert!(claimed.firing.run.is_some());
        automation = registry.automation(automation.id).unwrap();
        let tx = registry.db.transaction().unwrap();
        stage(
            &tx,
            &mut automation,
            LifecycleCause::ProjectClosed,
            None,
            201,
        );
        queue(&tx, &automation, LifecycleCause::ProjectClosed, None).unwrap();
        assert!(
            pending(&tx, PAGE_SIZE).unwrap().is_empty(),
            "existing AgentRun attention owns accepted work"
        );
        stage(
            &tx,
            &mut automation,
            LifecycleCause::RunInterrupted,
            Some(claimed.firing.id),
            202,
        );
        assert!(
            queue(
                &tx,
                &automation,
                LifecycleCause::RunInterrupted,
                Some(claimed.firing.id)
            )
            .is_err()
        );
        drop(tx);
        assert!(
            registry
                .automation(claimed.firing.automation)
                .unwrap()
                .active_firing
                .is_some()
        );
    }
}
