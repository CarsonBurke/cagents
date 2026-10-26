//! New automation run edges retained with their source transaction. This is
//! pending delivery state, not a historical scan or permission to execute.

use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};

use super::Registry;
use crate::{
    agent_execution::{Agent, HostedSession, Origin, Run, State},
    automations::Firing,
    model::RunId,
    notifications::{Event, Evidence, Kind, Target},
};

const MAX_PAGE: usize = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(in crate::host) struct AutomationAttentionReceipt {
    pub run: RunId,
    pub revision: u64,
}

#[derive(Clone, Debug)]
pub(in crate::host) struct PendingAutomationAttention {
    pub receipt: AutomationAttentionReceipt,
    pub event: Event,
}

pub(super) fn migrate(tx: &Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS automation_run_attention (
            ordinal INTEGER PRIMARY KEY AUTOINCREMENT,
            run TEXT NOT NULL UNIQUE,
            revision TEXT NOT NULL,
            event TEXT NOT NULL
        ) STRICT;",
    )
    .map_err(|error| error.to_string())
}

/// Called only after writing this exact run in the enclosing source
/// transaction. Agent/firing identity comes from that same transaction;
/// neither a notification caller nor a delayed scheduler snapshot supplies it.
pub(super) fn stage(tx: &Transaction<'_>, run: &Run) -> Result<(), String> {
    // Ordinary user/child updates do not load or copy their agent spec or
    // potentially large result record just to decide notification ownership.
    let origin: String = tx
        .query_row(
            "SELECT origin FROM agent_runs WHERE id=?1",
            [run.id.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    let origin: Origin = decode(&origin)?;
    let Origin::Automation { id } = &origin else {
        return Ok(());
    };
    let (agent, stored): (String, String) = tx
        .query_row(
            "SELECT a.record, r.record FROM agent_runs r
             JOIN agents a ON a.id=r.agent WHERE r.id=?1",
            [run.id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|error| error.to_string())?;
    let agent: Agent = decode(&agent)?;
    if agent.id != run.agent || decode::<Run>(&stored)? != *run || origin != agent.origin {
        return Err("Automation attention and execution identity disagree".into());
    }
    let firing: Option<String> = tx
        .query_row(
            "SELECT record FROM automation_firings WHERE run_id=?1",
            [run.id.to_string()],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    // Generic execution can also use an Automation origin without creating a
    // scheduled firing. As in the scheduler's previous run_attention filter,
    // that alone does not create automation attention or a new authority gate.
    let Some(firing) = firing else { return Ok(()) };
    let firing: Firing = decode(&firing)?;
    if firing.run != Some(run.id) || firing.agent != Some(agent.id) || firing.automation != *id {
        return Err("Automation attention and firing ownership disagree".into());
    }

    let kind = match run.state {
        State::Waiting => Some(Kind::Waiting),
        State::Succeeded => Some(Kind::Succeeded),
        State::Failed => Some(Kind::Failed),
        State::Interrupted => Some(Kind::Interrupted),
        State::Cancelled => Some(Kind::Cancelled),
        State::Accepted | State::Running => None,
    };
    // Only latest pending attention survives for this run. Resume clears an
    // unsubmitted Waiting edge; an already-enqueued journal event is not
    // retracted. Deleting then inserting also gives replacements source order.
    tx.execute(
        "DELETE FROM automation_run_attention WHERE run=?1",
        [run.id.to_string()],
    )
    .map_err(|error| error.to_string())?;
    let Some(kind) = kind else { return Ok(()) };
    let event = Event {
        target: Target::AgentRun(run.id),
        source_session: agent.session.as_ref().map(|source| match source {
            HostedSession::Chat(session) => session.id,
            HostedSession::Terminal(session) => session.id,
        }),
        provider: Some(agent.spec.provider),
        directory: Some(agent.spec.target.directory),
        destination: None,
        evidence: Evidence {
            key: if kind == Kind::Waiting {
                format!("{}:waiting:{}", run.id, run.revision)
            } else {
                run.id.to_string()
            },
            kind,
        },
        content: None,
    };
    event.validate()?;
    tx.execute(
        "INSERT INTO automation_run_attention(run,revision,event) VALUES (?1,?2,?3)",
        params![
            run.id.to_string(),
            run.revision.to_string(),
            serde_json::to_string(&event).map_err(|error| error.to_string())?
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

impl Registry {
    /// Snapshot for inspection/tests. Transfer must use the short transaction
    /// below: another execution connection can commit behind this mutex.
    pub(crate) fn pending_automation_attention(
        &self,
        limit: usize,
    ) -> Result<Vec<PendingAutomationAttention>, String> {
        read_pending(&self.db, limit)
    }

    /// Serialize source read + bounded NONBLOCKING enqueue with every Registry
    /// writer, including independent WAL connections. The callback must never
    /// wait for a journal receipt or acquire HostState/control admission. It
    /// retains original receivers outside this transaction before returning.
    /// This transaction never removes source rows, including on failure.
    pub(crate) fn with_pending_automation_attention<T>(
        &mut self,
        limit: usize,
        enqueue: impl FnOnce(Vec<PendingAutomationAttention>) -> Result<T, String>,
    ) -> Result<T, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let result = enqueue(read_pending(&tx, limit)?)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(result)
    }

    /// A journal commit for A must not acknowledge a later replacement B.
    /// The broker calls this only for its original successful commit receipt.
    pub(crate) fn ack_automation_attention(
        &self,
        receipt: AutomationAttentionReceipt,
    ) -> Result<bool, String> {
        self.db
            .execute(
                "DELETE FROM automation_run_attention WHERE run=?1 AND revision=?2",
                params![receipt.run.to_string(), receipt.revision.to_string()],
            )
            .map(|changed| changed == 1)
            .map_err(|error| error.to_string())
    }

    /// A bounded batch of original successful journal receipts shares one
    /// source transaction. Callers retain every proof until this returns Ok;
    /// any delete/commit error leaves the entire batch unacknowledged. Stale
    /// or duplicate receipts are harmless exact-delete no-ops.
    pub(crate) fn ack_automation_attention_batch(
        &mut self,
        receipts: &[AutomationAttentionReceipt],
    ) -> Result<(), String> {
        if receipts.len() > MAX_PAGE {
            return Err("Automation attention acknowledgement batch exceeds its limit".into());
        }
        if receipts.is_empty() {
            return Ok(());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        {
            let mut delete = tx
                .prepare("DELETE FROM automation_run_attention WHERE run=?1 AND revision=?2")
                .map_err(|error| error.to_string())?;
            for receipt in receipts {
                delete
                    .execute(params![
                        receipt.run.to_string(),
                        receipt.revision.to_string()
                    ])
                    .map_err(|error| error.to_string())?;
            }
        }
        tx.commit().map_err(|error| error.to_string())
    }
}

fn read_pending(db: &Connection, limit: usize) -> Result<Vec<PendingAutomationAttention>, String> {
    let mut query = db
        .prepare(
            "SELECT run,revision,event FROM automation_run_attention ORDER BY ordinal LIMIT ?1",
        )
        .map_err(|error| error.to_string())?;
    let rows = query
        .query_map([limit.clamp(1, MAX_PAGE)], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(|error| error.to_string())?;
    rows.map(|row| {
        let (run, revision, event) = row.map_err(|error| error.to_string())?;
        let run = run
            .parse::<RunId>()
            .map_err(|_| "Stored automation attention identity is invalid")?;
        let revision = revision
            .parse::<u64>()
            .map_err(|_| "Stored automation attention revision is invalid")?;
        let event: Event = decode(&event)?;
        event.validate()?;
        let key = if event.evidence.kind == Kind::Waiting {
            format!("{run}:waiting:{revision}")
        } else {
            run.to_string()
        };
        if event.target != Target::AgentRun(run)
            || event.content.is_some()
            || event.destination.is_some()
            || event.evidence.key != key
            || !matches!(
                event.evidence.kind,
                Kind::Waiting
                    | Kind::Succeeded
                    | Kind::Failed
                    | Kind::Interrupted
                    | Kind::Cancelled
            )
        {
            return Err("Stored automation attention is not an exact run edge".into());
        }
        Ok(PendingAutomationAttention {
            receipt: AutomationAttentionReceipt { run, revision },
            event,
        })
    })
    .collect()
}

fn decode<T: serde::de::DeserializeOwned>(value: &str) -> Result<T, String> {
    serde_json::from_str(value).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent_execution::{Accepted, ResultSummary, Update},
        model::ProjectKind,
        workspace_model::ModelEdit,
    };
    use uuid::Uuid;

    fn accept(registry: &mut Registry) -> Accepted {
        let definition = serde_json::from_value(serde_json::json!({
            "name":"PRIVATE_NAME_NOT_NOTIFICATION_CONTENT",
            "prompt":"PRIVATE_PROMPT_NOT_NOTIFICATION_CONTENT",
            "agent":{"provider":"Claude","account":null,
                "target":{"repository":"/fixture","directory":"/fixture","channel":null},
                "surface":"Chat","model":null,"permission_mode":null},
            "schedule":{"pattern":{"daily":{"hour":9,"minute":0}},"timezone":"UTC"},
            "session_mode":"fresh","precheck":null
        }))
        .unwrap();
        let automation = registry
            .create_automation(
                definition,
                100,
                crate::automations::DirectoryIdentity {
                    device: 1,
                    inode: 1,
                },
            )
            .unwrap();
        registry
            .claim_automation(
                automation.id,
                1,
                None,
                crate::automations::Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                200,
            )
            .unwrap()
            .accepted
            .unwrap()
    }

    fn fixture() -> (Registry, tempfile::TempDir, Accepted) {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: "/fixture".into(),
                name: "Fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let accepted = accept(&mut registry);
        (registry, directory, accepted)
    }

    fn start(registry: &mut Registry, run: &Run) -> Run {
        registry
            .update_agent_run(
                run.id,
                run.revision,
                Update::Started {
                    provider_key: "PRIVATE_PROVIDER_KEY".into(),
                },
                300,
            )
            .unwrap()
    }

    #[test]
    fn committed_source_survives_reopen_without_copying_result_or_prompt_content() {
        let (mut registry, directory, accepted) = fixture();
        let running = start(&mut registry, &accepted.run);
        let waiting = registry
            .update_agent_run(running.id, running.revision, Update::Waiting, 400)
            .unwrap();
        let pending = registry.pending_automation_attention(100).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].receipt,
            AutomationAttentionReceipt {
                run: waiting.id,
                revision: waiting.revision
            }
        );
        assert_eq!(pending[0].event.evidence.kind, Kind::Waiting);
        assert_eq!(pending[0].event.source_session, None);
        assert_eq!(
            pending[0].event.directory.as_deref(),
            Some(std::path::Path::new("/fixture"))
        );
        drop(registry); // No scheduler, enqueue or receipt preceded this reopen.
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(
            registry.pending_automation_attention(100).unwrap()[0].event,
            pending[0].event
        );
        let finished = registry
            .update_agent_run(
                waiting.id,
                waiting.revision,
                Update::Finished {
                    state: State::Succeeded,
                    result: ResultSummary {
                        text: "PRIVATE_RESULT_NOT_NOTIFICATION_CONTENT".into(),
                        diagnostic: Some("PRIVATE_DIAGNOSTIC_NOT_NOTIFICATION_CONTENT".into()),
                        ..Default::default()
                    },
                },
                500,
            )
            .unwrap();
        let pending = registry.pending_automation_attention(100).unwrap();
        assert_eq!(pending[0].receipt.revision, finished.revision);
        assert_eq!(pending[0].event.target, Target::AgentRun(finished.id));
        assert_eq!(pending[0].event.evidence.kind, Kind::Succeeded);
        assert!(pending[0].event.content.is_none());
        let record: String = registry
            .db
            .query_row("SELECT event FROM automation_run_attention", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            !record.contains("PRIVATE_"),
            "prompt, names, provider key and result stay outside attention"
        );
        assert!(
            registry
                .ack_automation_attention(pending[0].receipt)
                .unwrap()
        );
        assert!(
            !registry
                .ack_automation_attention(pending[0].receipt)
                .unwrap()
        );
        drop(registry);
        let registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert!(
            registry
                .pending_automation_attention(100)
                .unwrap()
                .is_empty(),
            "reopen never backfills acknowledged history"
        );
    }

    #[test]
    fn latest_pending_edges_keep_source_order_and_stale_receipts_cannot_delete_replacements() {
        let (mut registry, _directory, first) = fixture();
        let second = accept(&mut registry);
        let first_running = start(&mut registry, &first.run);
        let second_running = start(&mut registry, &second.run);
        let first_waiting = registry
            .update_agent_run(first.run.id, first_running.revision, Update::Waiting, 400)
            .unwrap();
        let old = registry.pending_automation_attention(100).unwrap()[0].receipt;
        registry
            .update_agent_run(second.run.id, second_running.revision, Update::Waiting, 401)
            .unwrap();
        let resumed = registry
            .update_agent_run(first.run.id, first_waiting.revision, Update::Resumed, 402)
            .unwrap();
        assert!(!registry.ack_automation_attention(old).unwrap());
        let pending = registry.pending_automation_attention(100).unwrap();
        assert_eq!(
            pending.len(),
            1,
            "resume clears only the first run's unsubmitted Waiting"
        );
        assert_eq!(pending[0].receipt.run, second.run.id);
        let first_waiting = registry
            .update_agent_run(first.run.id, resumed.revision, Update::Waiting, 403)
            .unwrap();
        let pending = registry.pending_automation_attention(100).unwrap();
        assert_eq!(
            pending
                .iter()
                .map(|item| item.receipt.run)
                .collect::<Vec<_>>(),
            vec![second.run.id, first.run.id]
        );
        assert_eq!(registry.pending_automation_attention(1).unwrap().len(), 1);
        assert_ne!(old.revision, first_waiting.revision);
        assert!(!registry.ack_automation_attention(old).unwrap());
        assert_eq!(registry.pending_automation_attention(100).unwrap().len(), 2);
    }

    #[test]
    fn stage_failure_rolls_back_run_revision_and_model_sequence_then_same_update_can_retry() {
        let (mut registry, _directory, accepted) = fixture();
        let running = start(&mut registry, &accepted.run);
        let sequence = registry.model().unwrap().sequence;
        registry
            .db
            .execute_batch(
                "CREATE TEMP TRIGGER fail_attention BEFORE INSERT ON automation_run_attention
            BEGIN SELECT RAISE(ABORT, 'fixture journal staging failure'); END;",
            )
            .unwrap();
        assert!(
            registry
                .update_agent_run(running.id, running.revision, Update::Waiting, 400)
                .is_err()
        );
        assert_eq!(registry.agent_run(running.id).unwrap(), running);
        assert_eq!(registry.model().unwrap().sequence, sequence);
        assert!(
            registry
                .pending_automation_attention(100)
                .unwrap()
                .is_empty()
        );
        registry
            .db
            .execute_batch("DROP TRIGGER fail_attention")
            .unwrap();
        let waiting = registry
            .update_agent_run(running.id, running.revision, Update::Waiting, 400)
            .unwrap();
        assert_eq!(
            registry.pending_automation_attention(100).unwrap()[0]
                .receipt
                .revision,
            waiting.revision
        );
    }

    #[test]
    fn wrong_firing_ownership_refuses_the_source_update_without_creating_attention() {
        let (mut registry, _directory, accepted) = fixture();
        let running = start(&mut registry, &accepted.run);
        let mut firing = registry
            .automation_detail_for_run(running.id)
            .unwrap()
            .firing;
        firing.agent = Some(crate::model::AgentId::new());
        registry
            .db
            .execute(
                "UPDATE automation_firings SET record=?2 WHERE run_id=?1",
                params![
                    running.id.to_string(),
                    serde_json::to_string(&firing).unwrap()
                ],
            )
            .unwrap();
        assert!(
            registry
                .update_agent_run(running.id, running.revision, Update::Waiting, 400)
                .is_err()
        );
        assert_eq!(registry.agent_run(running.id).unwrap(), running);
        assert!(
            registry
                .pending_automation_attention(100)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn automation_origin_without_a_firing_preserves_generic_execution_without_attention() {
        let (mut registry, _directory, accepted) = fixture();
        let request = crate::agent_execution::Submit {
            name: None,
            destination: crate::agent_execution::Destination::New(accepted.agent.spec),
            prompt: "Generic execution fixture".into(),
            idempotency_key: "no-scheduled-firing".into(),
        };
        let generic = registry
            .submit_agent(&Origin::Automation { id: Uuid::new_v4() }, &request, 201)
            .unwrap();
        let running = start(&mut registry, &generic.run);
        let waiting = registry
            .update_agent_run(running.id, running.revision, Update::Waiting, 400)
            .unwrap();
        let finished = registry
            .update_agent_run(
                waiting.id,
                waiting.revision,
                Update::Finished {
                    state: State::Succeeded,
                    result: Default::default(),
                },
                500,
            )
            .unwrap();
        assert_eq!(finished.state, State::Succeeded);
        assert!(
            registry
                .pending_automation_attention(100)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn read_enqueue_transaction_excludes_independent_wal_writer_then_releases_it() {
        let (mut registry, _directory, accepted) = fixture();
        let running = start(&mut registry, &accepted.run);
        let waiting = registry
            .update_agent_run(running.id, running.revision, Update::Waiting, 400)
            .unwrap();
        let original = registry
            .pending_automation_attention(100)
            .unwrap()
            .remove(0);
        let mut independent = registry.execution_connection().unwrap();
        independent
            .db
            .busy_timeout(std::time::Duration::ZERO)
            .unwrap();
        let captured = registry
            .with_pending_automation_attention(100, |pending| {
                assert_eq!(pending.len(), 1);
                assert_eq!(pending[0].receipt, original.receipt);
                // No sleep/thread/global lock: this separate WAL connection must
                // be excluded by SQLite itself throughout the enqueue callback.
                assert!(
                    independent
                        .update_agent_run(waiting.id, waiting.revision, Update::Resumed, 500)
                        .is_err()
                );
                assert_eq!(independent.agent_run(waiting.id).unwrap(), waiting);
                Ok(pending[0].receipt)
            })
            .unwrap();
        assert_eq!(captured, original.receipt);
        assert_eq!(
            registry.pending_automation_attention(100).unwrap()[0].receipt,
            original.receipt
        );
        independent
            .update_agent_run(waiting.id, waiting.revision, Update::Resumed, 500)
            .unwrap();
        assert!(
            registry
                .pending_automation_attention(100)
                .unwrap()
                .is_empty()
        );
        assert!(!registry.ack_automation_attention(captured).unwrap());
    }

    #[test]
    fn refused_enqueue_leaves_exact_source_pending_for_retry() {
        let (mut registry, _directory, accepted) = fixture();
        let running = start(&mut registry, &accepted.run);
        registry
            .update_agent_run(running.id, running.revision, Update::Waiting, 400)
            .unwrap();
        let original = registry
            .pending_automation_attention(100)
            .unwrap()
            .remove(0);
        let refused = registry.with_pending_automation_attention(100, |pending| {
            assert_eq!(pending[0].receipt, original.receipt);
            Err::<(), String>("fixture bounded enqueue refused".into())
        });
        assert!(refused.is_err());
        let pending = registry.pending_automation_attention(100).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].receipt, original.receipt);
        assert_eq!(pending[0].event, original.event);
        registry
            .with_pending_automation_attention(100, |pending| {
                assert_eq!(pending[0].receipt, original.receipt);
                Ok(())
            })
            .unwrap();
        assert_eq!(
            registry.pending_automation_attention(100).unwrap()[0].receipt,
            original.receipt,
            "enqueue success alone is not a journal acknowledgement"
        );
    }

    #[test]
    fn published_sixteen_waiting_run_stays_history_until_a_new_edge_after_migration() {
        let (mut registry, _directory, accepted) = fixture();
        let running = start(&mut registry, &accepted.run);
        let waiting = registry
            .update_agent_run(running.id, running.revision, Update::Waiting, 400)
            .unwrap();
        let before_model = registry.model().unwrap();
        let before_detail = registry.automation_detail_for_run(waiting.id).unwrap();
        // Construct the exact published16 preimage, not a newer layout with
        // a downgraded header. None of the historical source rows are changed.
        let legacy = super::super::automation_proposals::published_fixture_from_rows(&registry, 16);
        drop(registry);
        let mut registry = Registry::at(&legacy).unwrap();
        assert_eq!(registry.model().unwrap(), before_model);
        assert_eq!(registry.agent_run(waiting.id).unwrap(), waiting);
        assert_eq!(
            registry.automation_detail_for_run(waiting.id).unwrap(),
            before_detail
        );
        assert!(
            registry
                .pending_automation_attention(100)
                .unwrap()
                .is_empty(),
            "migration must not backfill Waiting history"
        );
        let resumed = registry
            .update_agent_run(waiting.id, waiting.revision, Update::Resumed, 500)
            .unwrap();
        let new_waiting = registry
            .update_agent_run(waiting.id, resumed.revision, Update::Waiting, 600)
            .unwrap();
        let pending = registry.pending_automation_attention(100).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].receipt.revision, new_waiting.revision);
        assert_ne!(new_waiting.revision, waiting.revision);
        assert_eq!(
            pending[0].event.evidence.key,
            format!("{}:waiting:{}", waiting.id, new_waiting.revision)
        );
    }

    #[test]
    fn published_twenty_upgrade_preserves_all_rows_without_replaying_run_history() {
        fn rows(db: &Connection, table: &str) -> Vec<Vec<rusqlite::types::Value>> {
            let table = table.replace('"', "\"\"");
            let selected = if table == "automation_firings" {
                "id,automation,revision,occurrence_key,record,created_ms,state,run_id"
            } else {
                "*"
            };
            let mut query = db
                .prepare(&format!("SELECT {selected} FROM \"{table}\""))
                .unwrap();
            let columns = query.column_count();
            let mut rows = query
                .query_map([], |row| {
                    (0..columns)
                        .map(|column| row.get::<_, rusqlite::types::Value>(column))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            rows.sort_by_key(|row| format!("{row:?}"));
            rows
        }

        let (mut registry, _directory, accepted) = fixture();
        let running = start(&mut registry, &accepted.run);
        registry
            .update_agent_run(running.id, running.revision, Update::Waiting, 400)
            .unwrap();
        let original_id = registry.automations(None, 100).unwrap().remove(0).id;
        let original = registry.automation(original_id).unwrap();
        let no_run = registry
            .create_automation(
                original.definition.clone(),
                500,
                crate::automations::DirectoryIdentity {
                    device: 1,
                    inode: 1,
                },
            )
            .unwrap();
        let no_run = registry
            .set_automation_enabled(
                no_run.id,
                no_run.revision,
                Some(crate::automations::Occurrence {
                    due_ms: 10_000,
                    local: "fixture future".into(),
                }),
                true,
                600,
            )
            .unwrap();
        assert!(no_run.enabled);
        registry
            .pin_window_terminals(&[crate::model::TerminalId::new()])
            .unwrap();
        registry
            .apply(&ModelEdit::CloseProject {
                repository: "/fixture".into(),
            })
            .unwrap();
        assert_eq!(
            registry
                .pending_automation_notifications(100)
                .unwrap()
                .len(),
            1
        );
        assert!(
            !registry
                .pending_automation_attention(100)
                .unwrap()
                .is_empty()
        );
        let expected_model = registry.model().unwrap();
        let legacy = super::super::automation_proposals::published_fixture_from_rows(&registry, 20);
        drop(registry);
        let db = Connection::open(&legacy).unwrap();
        let tables: Vec<String> = db
            .prepare("SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(
            !tables
                .iter()
                .any(|table| table == "automation_run_attention")
        );
        let before = tables
            .iter()
            .map(|table| (table.clone(), rows(&db, table)))
            .collect::<Vec<_>>();
        assert_eq!(
            db.query_row("SELECT count(*) FROM terminal_retention_pins", [], |row| {
                row.get::<_, u64>(0)
            })
            .unwrap(),
            1
        );
        assert_eq!(
            db.pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            20
        );
        drop(db);
        for _ in 0..2 {
            let registry = Registry::at(&legacy).unwrap();
            assert_eq!(registry.model().unwrap(), expected_model);
            assert_eq!(
                registry
                    .db
                    .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                    .unwrap(),
                super::super::SCHEMA_VERSION
            );
            assert!(
                registry
                    .pending_automation_attention(100)
                    .unwrap()
                    .is_empty(),
                "migration/reopen must not backfill old Waiting or terminal history"
            );
            assert_eq!(
                registry
                    .pending_automation_notifications(100)
                    .unwrap()
                    .len(),
                1
            );
            for (table, expected) in &before {
                assert_eq!(
                    &rows(&registry.db, table),
                    expected,
                    "published20 payload changed in {table}"
                );
            }
        }
    }

    #[test]
    fn batch_ack_rolls_back_prior_deletes_and_retry_preserves_a_newer_replacement() {
        let (mut registry, _directory, first) = fixture();
        let second = accept(&mut registry);
        let third = accept(&mut registry);
        for accepted in [&first, &second, &third] {
            let running = start(&mut registry, &accepted.run);
            registry
                .update_agent_run(running.id, running.revision, Update::Waiting, 400)
                .unwrap();
        }
        let original = registry.pending_automation_attention(100).unwrap();
        let first_receipt = original[0].receipt;
        let second_receipt = original[1].receipt;
        let stale_third = original[2].receipt;
        let resumed = registry
            .update_agent_run(third.run.id, stale_third.revision, Update::Resumed, 500)
            .unwrap();
        registry
            .update_agent_run(third.run.id, resumed.revision, Update::Waiting, 600)
            .unwrap();
        let before = registry.pending_automation_attention(100).unwrap();
        let replacement = before[2].clone();
        assert_ne!(replacement.receipt, stale_third);
        registry
            .db
            .execute_batch(
                "CREATE TEMP TABLE refuse_attention_ack(run TEXT NOT NULL);
            CREATE TEMP TRIGGER fail_attention_ack BEFORE DELETE ON automation_run_attention
            WHEN OLD.run=(SELECT run FROM refuse_attention_ack)
            BEGIN SELECT RAISE(ABORT, 'fixture second source delete refused'); END;",
            )
            .unwrap();
        registry
            .db
            .execute(
                "INSERT INTO refuse_attention_ack VALUES (?1)",
                [second.run.id.to_string()],
            )
            .unwrap();
        assert!(
            registry
                .ack_automation_attention_batch(&[first_receipt, second_receipt, stale_third])
                .is_err()
        );
        let retained = registry.pending_automation_attention(100).unwrap();
        assert_eq!(retained.len(), before.len());
        for (actual, expected) in retained.iter().zip(&before) {
            assert_eq!(
                actual.receipt, expected.receipt,
                "the first delete must roll back when the second aborts"
            );
            assert_eq!(actual.event, expected.event);
        }
        registry
            .db
            .execute_batch("DROP TRIGGER fail_attention_ack")
            .unwrap();
        registry
            .ack_automation_attention_batch(&[
                first_receipt,
                first_receipt,
                second_receipt,
                stale_third,
            ])
            .unwrap();
        let retained = registry.pending_automation_attention(100).unwrap();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].receipt, replacement.receipt);
        assert_eq!(retained[0].event, replacement.event);
        assert!(
            registry
                .ack_automation_attention_batch(&[replacement.receipt; 101])
                .is_err()
        );
        registry.ack_automation_attention_batch(&[]).unwrap();
        assert_eq!(
            registry.pending_automation_attention(100).unwrap()[0].receipt,
            replacement.receipt
        );
        registry
            .ack_automation_attention_batch(&[replacement.receipt; 100])
            .unwrap();
        assert!(
            registry
                .pending_automation_attention(100)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn pending_revision_text_preserves_full_u64_and_exact_ack() {
        let (mut registry, _directory, accepted) = fixture();
        let running = start(&mut registry, &accepted.run);
        registry
            .update_agent_run(running.id, running.revision, Update::Waiting, 400)
            .unwrap();
        let mut event = registry
            .pending_automation_attention(100)
            .unwrap()
            .remove(0)
            .event;
        event.evidence.key = format!("{}:waiting:{}", running.id, u64::MAX);
        // Exercise the pending table codec without narrowing a u64 through a
        // SQLite INTEGER. This is not a claim that agent_runs supports MAX.
        registry
            .db
            .execute(
                "UPDATE automation_run_attention SET revision=?1,event=?2",
                params![u64::MAX.to_string(), serde_json::to_string(&event).unwrap()],
            )
            .unwrap();
        let pending = registry.pending_automation_attention(100).unwrap();
        assert_eq!(pending[0].receipt.revision, u64::MAX);
        assert!(
            !registry
                .ack_automation_attention(AutomationAttentionReceipt {
                    run: running.id,
                    revision: u64::MAX - 1
                })
                .unwrap()
        );
        assert!(
            registry
                .ack_automation_attention(pending[0].receipt)
                .unwrap()
        );
    }
}
