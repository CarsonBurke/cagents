//! Immutable consent requests. Activation and the exact approval receipt are
//! committed together; no pending record participates in scheduler claims.
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use uuid::Uuid;

use super::{Registry, automations::create_automation_in};
use crate::{
    agent_execution::Origin,
    automations::{
        Definition, DirectoryIdentity, Occurrence, Proposal, ProposalState, ProposalSummary,
    },
};

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let tx = rusqlite::Transaction::new_unchecked(db, TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let current: u32 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if current < 11 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS automation_proposals (
            id TEXT PRIMARY KEY, owner TEXT NOT NULL, repository TEXT NOT NULL,
            pending INTEGER NOT NULL CHECK(pending IN (0,1)), record TEXT NOT NULL, summary TEXT NOT NULL
        ) STRICT;
        CREATE INDEX IF NOT EXISTS pending_automation_proposals
            ON automation_proposals(repository,id) WHERE pending=1;
        CREATE INDEX IF NOT EXISTS owned_automation_proposals
            ON automation_proposals(owner,id);
        PRAGMA user_version=11;",
        )
        .map_err(|error| error.to_string())?;
    }
    tx.commit().map_err(|error| error.to_string())
}

fn encode(value: &impl serde::Serialize) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| error.to_string())
}

fn notification_key(id: Uuid) -> String {
    format!("proposal:{id}")
}

fn retire_notification(db: &Connection, id: Uuid) -> Result<(), String> {
    db.execute(
        "DELETE FROM automation_notifications WHERE key=?1",
        [notification_key(id)],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn read(db: &Connection, id: Uuid, owner: Option<&Origin>) -> Result<Option<Proposal>, String> {
    let owner = owner.map(encode).transpose()?;
    let record: Option<String> = db
        .query_row(
            "SELECT record FROM automation_proposals WHERE id=?1 AND (?2 IS NULL OR owner=?2)",
            params![id.to_string(), owner],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    record
        .map(|record| {
            serde_json::from_str(&record)
                .map_err(|error| format!("Stored automation proposal is invalid: {error}"))
        })
        .transpose()
}

const PENDING_PAGE_SQL: &str =
    "SELECT summary FROM automation_proposals WHERE pending=1 AND id>?1 ORDER BY id LIMIT ?2";

impl Registry {
    pub(crate) fn pending_automation_proposal_count(&self) -> Result<u64, String> {
        self.db
            .query_row(
                "SELECT count(*) FROM automation_proposals WHERE pending=1",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }

    pub(crate) fn automation_proposal_sequence(&self) -> Result<u64, String> {
        self.db
            .query_row(
                "SELECT automation_proposal_sequence FROM metadata WHERE key=0",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }

    pub(crate) fn pending_automation_proposals_page(
        &self,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<ProposalSummary>, String> {
        // Every stored ID is a nonempty canonical UUID. The empty lower bound
        // starts page one; all pages retain a genuine indexed range seek.
        let mut query = self
            .db
            .prepare(PENDING_PAGE_SQL)
            .map_err(|error| error.to_string())?;
        query
            .query_map(
                params![
                    after.map(|id| id.to_string()).unwrap_or_default(),
                    limit.clamp(1, crate::automations::PROPOSAL_PAGE_SIZE)
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

    pub(crate) fn automation_proposal(
        &self,
        id: Uuid,
        owner: Option<&Origin>,
    ) -> Result<Option<Proposal>, String> {
        read(&self.db, id, owner)
    }

    /// Approval attention follows the durable decision, including after a late
    /// event was loaded or journaled. Do not load the saved execution prompt.
    pub(crate) fn automation_proposal_needs_approval(&self, id: Uuid) -> Result<bool, String> {
        self.db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM automation_proposals WHERE id=?1 AND pending=1)",
                [id.to_string()],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())
    }

    /// Also discard an old outbox row restored from before decision retirement
    /// was supported. A pending request must retain its original notice.
    pub(crate) fn retire_automation_proposal_notification(&self, id: Uuid) -> Result<(), String> {
        self.db
            .execute(
                "DELETE FROM automation_notifications WHERE key=?1 AND NOT EXISTS(
                    SELECT 1 FROM automation_proposals WHERE id=?2 AND pending=1)",
                params![notification_key(id), id.to_string()],
            )
            .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(crate) fn pending_automation_proposals(
        &self,
        repository: &std::path::Path,
        after: Option<Uuid>,
        limit: usize,
    ) -> Result<Vec<ProposalSummary>, String> {
        let mut query = self
            .db
            .prepare(
                "SELECT summary FROM automation_proposals
            WHERE pending=1 AND repository=?1 AND (?2 IS NULL OR id>?2) ORDER BY id LIMIT ?3",
            )
            .map_err(|error| error.to_string())?;
        let rows = query
            .query_map(
                params![
                    super::repository_key(repository)?,
                    after.map(|id| id.to_string()),
                    limit.clamp(1, crate::automations::MAX_HISTORY_PAGE)
                ],
                |row| row.get::<_, String>(0),
            )
            .map_err(|error| error.to_string())?;
        rows.map(|record| {
            let record = record.map_err(|error| error.to_string())?;
            serde_json::from_str(&record).map_err(|error| error.to_string())
        })
        .collect()
    }

    pub(crate) fn propose_automation(
        &mut self,
        id: Uuid,
        owner: Origin,
        requested: Definition,
        definition: Definition,
        identity: DirectoryIdentity,
        channel_title: Option<String>,
        now_ms: u64,
    ) -> Result<Proposal, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        if let Some(previous) = read(&tx, id, Some(&owner))? {
            if previous.requested != requested {
                return Err(
                    "This request ID belongs to another saved definition; use a new request ID"
                        .into(),
                );
            }
            return Ok(previous);
        }
        let count: usize = tx
            .query_row(
                "SELECT count(*) FROM automation_proposals WHERE owner=?1 AND pending=1",
                [encode(&owner)?],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if count >= 32 {
            return Err(
                "Resolve this conversation's pending automation requests before proposing more"
                    .into(),
            );
        }
        super::automations::require_project(&tx, &definition)?;
        let proposal = Proposal {
            id,
            revision: 1,
            owner,
            requested,
            definition,
            target_identity: identity,
            channel_title,
            created_ms: now_ms,
            state: ProposalState::Pending,
        };
        tx.execute(
            "INSERT INTO automation_proposals(id,owner,repository,pending,record,summary)
            VALUES(?1,?2,?3,1,?4,?5)",
            params![
                id.to_string(),
                encode(&proposal.owner)?,
                super::repository_key(&proposal.definition.agent.target.repository)?,
                encode(&proposal)?,
                encode(&ProposalSummary::from(&proposal))?
            ],
        )
        .map_err(|_| "Could not save this request ID; use a new request ID".to_owned())?;
        let project: String = proposal
            .definition
            .agent
            .target
            .repository
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .chars()
            .filter(|c| !c.is_control())
            .take(128)
            .collect();
        // Attribute the request to its actual issuing conversation, rather
        // than the provider it proposes to schedule (which may differ).
        let issuer = if let Origin::Child { parent } = &proposal.owner {
            tx.query_row("SELECT record FROM agents WHERE session_id IN (SELECT session_id FROM session_lineages WHERE lineage=?1) ORDER BY rowid DESC LIMIT 1",
                [parent.to_string()], |row|row.get::<_,String>(0)).optional().map_err(|error|error.to_string())?
                .map(|record|serde_json::from_str::<crate::agent_execution::Agent>(&record).map_err(|error|error.to_string())).transpose()?
        } else {
            None
        };
        let mut requester = issuer
            .as_ref()
            .map(|agent| {
                agent.name.clone().unwrap_or_else(|| {
                    format!("{} agent {}", agent.spec.provider.label(), agent.id)
                })
            })
            .unwrap_or_else(|| match &proposal.owner {
                Origin::Child { parent } => format!("Hosted conversation {parent}"),
                _ => "Hosted conversation".into(),
            });
        crate::agent_execution::truncate(&mut requester, 256);
        super::automations::queue_automation_event(
            &tx,
            crate::notifications::Event {
                target: crate::notifications::Target::AutomationProposal {
                    id: proposal.id,
                    name: proposal.definition.name.clone(),
                    project,
                    requester,
                },
                source_session: None,
                provider: issuer.as_ref().map(|agent| agent.spec.provider),
                directory: Some(proposal.definition.agent.target.repository.clone()),
                destination: None,
                evidence: crate::notifications::Evidence {
                    key: notification_key(proposal.id),
                    kind: crate::notifications::Kind::AutomationProposed,
                },
            },
        )?;
        tx.execute("UPDATE metadata SET sequence=sequence+1,automation_proposal_sequence=automation_proposal_sequence+1 WHERE key=0", [])
            .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(proposal)
    }

    /// Callers validate filesystem/account outside SQLite, then this exact
    /// snapshot fence and project/channel checks close the registry race.
    pub(crate) fn resolve_automation_proposal(
        &mut self,
        expected: &Proposal,
        next: Option<Occurrence>,
        now_ms: u64,
    ) -> Result<Proposal, String> {
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        let mut current = read(&tx, expected.id, None)?
            .ok_or("The automation proposal is no longer available")?;
        if current.revision != expected.revision {
            return Err("The automation proposal changed; review its current definition".into());
        }
        if current.state != ProposalState::Pending {
            // Decisions are terminal. Concurrent windows and retries receive
            // the winning receipt, never reinterpret denial as approval.
            return Ok(current);
        }
        if &current != expected {
            return Err("The automation proposal changed; review it again".into());
        }
        current.state = if let Some(next) = next {
            let automation = create_automation_in(
                &tx,
                current.definition.clone(),
                now_ms,
                current.target_identity.clone(),
                Some(next),
            )?;
            ProposalState::Approved {
                automation: automation.id,
                execution_revision: automation.execution_revision,
            }
        } else {
            ProposalState::Declined
        };
        tx.execute(
            "UPDATE automation_proposals SET pending=0,record=?2 WHERE id=?1",
            params![current.id.to_string(), encode(&current)?],
        )
        .map_err(|error| error.to_string())?;
        retire_notification(&tx, current.id)?;
        tx.execute("UPDATE metadata SET sequence=sequence+1,automation_proposal_sequence=automation_proposal_sequence+1 WHERE key=0", [])
            .map_err(|error| error.to_string())?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent::Provider,
        agent_execution::{Lineage, Spec, Surface, Target},
        automations::{Schedule, SchedulePattern, SessionMode, Trigger},
        model::{ChannelId, ChannelTitleSource, ProjectKind, TerminalId},
        workspace_model::ModelEdit,
    };

    fn fixture() -> (tempfile::TempDir, Registry, Proposal) {
        let root = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&root.path().join("registry.sqlite3")).unwrap();
        let repository = root.path().to_path_buf();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: repository.clone(),
                name: "Consent fixture".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        let channel = ChannelId::new();
        registry
            .apply(&ModelEdit::CreateChannel {
                id: channel,
                repository: repository.clone(),
                title: "User standup channel".into(),
                title_source: ChannelTitleSource::Manual,
            })
            .unwrap();
        let definition = Definition {
            name: "Scheduled standup".into(),
            prompt: "Summarize the approved project".into(),
            agent: Spec {
                provider: Provider::Codex,
                account: Some("codex/system".parse().unwrap()),
                target: Target {
                    repository: repository.clone(),
                    directory: repository,
                    placement: Some(crate::agent_execution::Placement::Channel(channel)),
                },
                surface: Surface::Chat,
                model: Some("fixture".into()),
                reasoning_effort: Some("low".into()),
                permission_mode: Some("readOnly".into()),
            },
            schedule: Schedule {
                timezone: "America/Vancouver".into(),
                pattern: SchedulePattern::Cron {
                    expression: "30 9 * * 1,3,5".into(),
                },
            },
            session_mode: SessionMode::Fresh,
            visibility: crate::automations::Visibility::default(),
            catch_up: crate::automations::CatchUp::default(),
            precheck: None,
        };
        let proposal = registry
            .propose_automation(
                Uuid::new_v4(),
                Origin::Child {
                    parent: Lineage(TerminalId::new()),
                },
                definition.clone(),
                definition,
                DirectoryIdentity {
                    device: 1,
                    inode: 2,
                },
                Some("User standup channel".into()),
                100,
            )
            .unwrap();
        (root, registry, proposal)
    }

    fn next() -> Occurrence {
        Occurrence {
            due_ms: 1000,
            local: "2026-10-12T09:30".into(),
        }
    }

    #[test]
    fn proposal_creation_and_resolution_advance_watch_sequence_and_global_snapshot() {
        let (_root, mut registry, proposal) = fixture();
        let sequence = registry.execution_sequence().unwrap();
        assert!(sequence > 1);
        assert_eq!(
            registry.model().unwrap().pending_automation_proposals,
            vec![ProposalSummary::from(&proposal)]
        );
        let notices = registry.automation_notifications().unwrap();
        assert_eq!(notices.len(), 1);
        assert_eq!(
            notices[0].evidence.kind,
            crate::notifications::Kind::AutomationProposed
        );
        registry
            .resolve_automation_proposal(&proposal, None, 300)
            .unwrap();
        assert_eq!(registry.execution_sequence().unwrap(), sequence + 1);
        assert!(
            registry
                .model()
                .unwrap()
                .pending_automation_proposals
                .is_empty()
        );
    }

    #[test]
    fn proposal_is_durable_owner_scoped_and_does_not_create_a_schedule() {
        let (root, mut registry, proposal) = fixture();
        assert!(registry.automations(None, 100).unwrap().is_empty());
        assert!(
            registry
                .automation_proposal(
                    proposal.id,
                    Some(&Origin::Child {
                        parent: Lineage(TerminalId::new())
                    })
                )
                .unwrap()
                .is_none()
        );
        let mut changed = proposal.requested.clone();
        changed.prompt = "Other execution".into();
        assert!(
            registry
                .propose_automation(
                    proposal.id,
                    proposal.owner.clone(),
                    changed,
                    proposal.definition.clone(),
                    proposal.target_identity.clone(),
                    proposal.channel_title.clone(),
                    200
                )
                .is_err()
        );
        drop(registry);
        let registry = Registry::at(&root.path().join("registry.sqlite3")).unwrap();
        assert_eq!(
            registry
                .automation_proposal(proposal.id, Some(&proposal.owner))
                .unwrap(),
            Some(proposal.clone())
        );
        assert_eq!(
            registry
                .pending_automation_proposals(
                    &proposal.definition.agent.target.repository,
                    None,
                    100
                )
                .unwrap(),
            [ProposalSummary::from(&proposal)]
        );
        assert!(
            registry
                .pending_automation_proposals(&root.path().join("another-project"), None, 100)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn approval_commits_one_enabled_schedule_and_preserves_the_named_channel() {
        let (_root, mut registry, proposal) = fixture();
        let receipt = registry
            .resolve_automation_proposal(&proposal, Some(next()), 200)
            .unwrap();
        let ProposalState::Approved {
            automation,
            execution_revision,
        } = receipt.state
        else {
            panic!("approval receipt")
        };
        let saved = registry.automation(automation).unwrap();
        assert!(saved.enabled);
        assert_eq!(saved.execution_revision, execution_revision);
        assert_eq!(saved.definition, proposal.definition);
        assert_eq!(saved.channel, proposal.definition.agent.target.channel());
        let accepted = registry
            .claim_automation(
                automation,
                execution_revision,
                None,
                Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                300,
            )
            .unwrap()
            .accepted
            .unwrap();
        assert_eq!(
            accepted.agent.channel(),
            proposal.definition.agent.target.channel()
        );
        let model = registry.model().unwrap();
        let channel = model.channel(accepted.agent.channel().unwrap()).unwrap();
        assert_eq!(channel.title, "User standup channel");
        assert_eq!(
            channel.repository,
            proposal.definition.agent.target.repository
        );
        assert_eq!(
            registry
                .resolve_automation_proposal(&proposal, Some(next()), 400)
                .unwrap(),
            receipt
        );
        assert_eq!(
            registry
                .resolve_automation_proposal(&proposal, None, 400)
                .unwrap(),
            receipt
        );
        assert_eq!(registry.automations(None, 100).unwrap().len(), 1);
        assert!(
            registry
                .pending_automation_proposals(
                    &proposal.definition.agent.target.repository,
                    None,
                    100
                )
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn approval_receipt_failure_rolls_back_schedule_creation_then_can_retry() {
        let (_root, mut registry, proposal) = fixture();
        let original_notices = registry.automation_notifications().unwrap();
        registry
            .db
            .execute_batch(
                "CREATE TRIGGER reject_proposal_receipt BEFORE UPDATE ON automation_proposals
            BEGIN SELECT RAISE(ABORT,'fixture receipt failure'); END;",
            )
            .unwrap();
        assert!(
            registry
                .resolve_automation_proposal(&proposal, Some(next()), 200)
                .is_err()
        );
        assert!(registry.automations(None, 100).unwrap().is_empty());
        assert_eq!(
            registry.automation_proposal(proposal.id, None).unwrap(),
            Some(proposal.clone())
        );
        assert_eq!(
            registry.automation_notifications().unwrap(),
            original_notices
        );
        registry
            .db
            .execute_batch("DROP TRIGGER reject_proposal_receipt")
            .unwrap();
        assert!(matches!(
            registry
                .resolve_automation_proposal(&proposal, Some(next()), 300)
                .unwrap()
                .state,
            ProposalState::Approved { .. }
        ));
        assert!(registry.automation_notifications().unwrap().is_empty());
    }

    #[test]
    fn notification_retirement_failure_keeps_the_decision_and_schedule_uncommitted() {
        let (_root, mut registry, proposal) = fixture();
        let original_notices = registry.automation_notifications().unwrap();
        registry
            .db
            .execute_batch(
                "CREATE TRIGGER reject_proposal_notice_retirement BEFORE DELETE ON automation_notifications
                 BEGIN SELECT RAISE(ABORT,'fixture retirement failure'); END;",
            )
            .unwrap();
        assert!(
            registry
                .resolve_automation_proposal(&proposal, Some(next()), 200)
                .is_err()
        );
        assert!(registry.automations(None, 100).unwrap().is_empty());
        assert_eq!(
            registry.automation_proposal(proposal.id, None).unwrap(),
            Some(proposal.clone())
        );
        assert_eq!(
            registry.automation_notifications().unwrap(),
            original_notices
        );
        registry
            .db
            .execute_batch("DROP TRIGGER reject_proposal_notice_retirement")
            .unwrap();
        let receipt = registry
            .resolve_automation_proposal(&proposal, Some(next()), 300)
            .unwrap();
        assert_eq!(registry.automations(None, 100).unwrap().len(), 1);
        assert!(registry.automation_notifications().unwrap().is_empty());
        assert_eq!(
            registry
                .resolve_automation_proposal(&proposal, Some(next()), 400)
                .unwrap(),
            receipt
        );
    }

    #[test]
    fn stale_review_and_changed_channel_owner_cannot_activate_a_proposal() {
        let (_root, mut registry, proposal) = fixture();
        let mut stale = proposal.clone();
        stale.definition.prompt = "Changed display".into();
        assert!(
            registry
                .resolve_automation_proposal(&stale, Some(next()), 200)
                .is_err()
        );
        let other = proposal.definition.agent.target.repository.join("other");
        std::fs::create_dir(&other).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: other.clone(),
                name: "Other".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        registry
            .db
            .execute(
                "UPDATE channels SET repository=?2 WHERE id=?1",
                params![
                    proposal
                        .definition
                        .agent
                        .target
                        .channel()
                        .unwrap()
                        .to_string(),
                    super::super::repository_key(&other).unwrap()
                ],
            )
            .unwrap();
        assert!(
            registry
                .resolve_automation_proposal(&proposal, Some(next()), 200)
                .is_err()
        );
        assert!(registry.automations(None, 100).unwrap().is_empty());
        assert_eq!(
            registry
                .automation_proposal(proposal.id, None)
                .unwrap()
                .unwrap()
                .state,
            ProposalState::Pending
        );
    }

    #[test]
    fn declined_proposal_replay_never_activates_work() {
        let (_root, mut registry, proposal) = fixture();
        let receipt = registry
            .resolve_automation_proposal(&proposal, None, 200)
            .unwrap();
        assert_eq!(receipt.state, ProposalState::Declined);
        assert_eq!(
            registry
                .resolve_automation_proposal(&proposal, Some(next()), 300)
                .unwrap(),
            receipt
        );
        assert!(registry.automations(None, 100).unwrap().is_empty());
    }

    #[test]
    fn legacy_registry_migration_preserves_schedules_and_starts_without_proposals() {
        let (root, mut registry, proposal) = fixture();
        let automation = registry
            .create_automation(
                proposal.definition.clone(),
                200,
                proposal.target_identity.clone(),
            )
            .unwrap();
        registry
            .db
            .execute_batch("DROP TABLE automation_proposals; PRAGMA user_version=10;")
            .unwrap();
        drop(registry);
        let registry = Registry::at(&root.path().join("registry.sqlite3")).unwrap();
        assert_eq!(registry.automation(automation.id).unwrap(), automation);
        assert!(
            registry
                .pending_automation_proposals(
                    &proposal.definition.agent.target.repository,
                    None,
                    100
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            registry
                .db
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            super::super::SCHEMA_VERSION
        );
    }

    #[test]
    fn ten_thousand_pending_requests_keep_push_bounded_and_every_page_reachable() {
        let (_root, mut registry, original) = fixture();
        let tx = registry.db.transaction().unwrap();
        tx.execute("DELETE FROM automation_proposals", []).unwrap();
        {
            let mut insert = tx.prepare("INSERT INTO automation_proposals(id,owner,repository,pending,record,summary) VALUES(?1,?2,?3,1,?4,?5)").unwrap();
            for index in 1..=10_000u128 {
                let mut proposal = original.clone();
                proposal.id = Uuid::from_u128(index);
                proposal.owner = Origin::Child {
                    parent: Lineage(TerminalId::new()),
                };
                insert
                    .execute(params![
                        proposal.id.to_string(),
                        encode(&proposal.owner).unwrap(),
                        proposal
                            .definition
                            .agent
                            .target
                            .repository
                            .to_str()
                            .unwrap(),
                        encode(&proposal).unwrap(),
                        encode(&ProposalSummary::from(&proposal)).unwrap()
                    ])
                    .unwrap();
            }
        }
        tx.execute("UPDATE metadata SET automation_proposal_sequence=automation_proposal_sequence+1,sequence=sequence+1 WHERE key=0", []).unwrap();
        tx.commit().unwrap();
        let model = registry.model().unwrap();
        assert_eq!(model.pending_automation_proposal_count, 10_000);
        assert_eq!(
            model.pending_automation_proposals.len(),
            crate::automations::PROPOSAL_PAGE_SIZE
        );
        let bytes = serde_json::to_vec(&model).unwrap();
        assert!(
            bytes.len() < 128 * 1024,
            "summary payload is bounded: {} bytes",
            bytes.len()
        );
        assert!(
            !String::from_utf8(bytes)
                .unwrap()
                .contains(&original.definition.prompt)
        );
        let mut seen = std::collections::HashSet::new();
        let mut cursor = None;
        loop {
            let page = registry
                .pending_automation_proposals_page(cursor, usize::MAX)
                .unwrap();
            assert!(page.len() <= crate::automations::PROPOSAL_PAGE_SIZE);
            if page.is_empty() {
                break;
            }
            assert!(page.windows(2).all(|pair| pair[0].id < pair[1].id));
            assert!(cursor.is_none_or(|cursor| page[0].id > cursor));
            cursor = page.last().map(|proposal| proposal.id);
            for proposal in page {
                assert!(seen.insert(proposal.id));
            }
        }
        assert_eq!(seen.len(), 10_000);
        assert!(seen.contains(&Uuid::from_u128(10_000)));
        // Assert the actual production SQL seeks both the first and late page,
        // rather than scanning all earlier pending IDs on every request.
        for after in [String::new(), Uuid::from_u128(9_984).to_string()] {
            let mut explain = registry
                .db
                .prepare(&format!("EXPLAIN QUERY PLAN {PENDING_PAGE_SQL}"))
                .unwrap();
            let plan = explain
                .query_map(params![after, 64], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
                .join("\n");
            assert!(
                plan.contains("SEARCH") && plan.contains("id>?"),
                "page lookup must seek: {plan}"
            );
        }
    }

    #[test]
    fn proposal_clock_migrates_atomically_and_only_consent_edges_advance_it() {
        let (root, registry, proposal) = fixture();
        let definition = proposal.definition.clone();
        let original_clock = registry.automation_proposal_sequence().unwrap();
        assert_eq!(original_clock, 1);
        registry.db.execute_batch("ALTER TABLE metadata DROP COLUMN automation_proposal_sequence; PRAGMA user_version=12;").unwrap();
        drop(registry);
        let mut registry = Registry::at(&root.path().join("registry.sqlite3")).unwrap();
        assert_eq!(registry.automation_proposal_sequence().unwrap(), 0);
        assert_eq!(registry.pending_automation_proposal_count().unwrap(), 1);
        assert_eq!(
            registry.automation_proposal(proposal.id, None).unwrap(),
            Some(proposal.clone())
        );
        assert_eq!(
            registry
                .db
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            super::super::SCHEMA_VERSION
        );
        registry
            .apply(&ModelEdit::OpenProject {
                repository: definition.agent.target.repository,
                name: "Unrelated sidebar edit".into(),
                kind: ProjectKind::Folder,
            })
            .unwrap();
        assert_eq!(registry.automation_proposal_sequence().unwrap(), 0);
        registry
            .resolve_automation_proposal(&proposal, None, 300)
            .unwrap();
        assert_eq!(registry.automation_proposal_sequence().unwrap(), 1);
        assert_eq!(registry.pending_automation_proposal_count().unwrap(), 0);
        registry
            .resolve_automation_proposal(&proposal, None, 301)
            .unwrap();
        assert_eq!(
            registry.automation_proposal_sequence().unwrap(),
            1,
            "an immutable receipt retry is not a new proposal edge"
        );
        let future = super::super::SCHEMA_VERSION + 1;
        registry
            .db
            .pragma_update(None, "user_version", future)
            .unwrap();
        drop(registry);
        assert!(Registry::at(&root.path().join("registry.sqlite3")).is_err());
        let db = Connection::open(root.path().join("registry.sqlite3")).unwrap();
        assert_eq!(
            db.pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            future
        );
        assert_eq!(
            db.query_row(
                "SELECT pending FROM automation_proposals WHERE id=?1",
                [proposal.id.to_string()],
                |row| row.get::<_, u64>(0)
            )
            .unwrap(),
            0
        );
    }
    #[test]
    fn failed_automation_extension_upgrade_retains_clock_schema_and_durable_intent() {
        let (root, mut registry, proposal) = fixture();
        let automation = registry
            .create_automation(
                proposal.definition.clone(),
                200,
                proposal.target_identity.clone(),
            )
            .unwrap();
        let claim = registry
            .claim_automation(
                automation.id,
                automation.execution_revision,
                None,
                Trigger::Manual {
                    request: Uuid::new_v4(),
                },
                None,
                true,
                300,
            )
            .unwrap();
        let firing = registry
            .db
            .query_row(
                "SELECT record FROM automation_firings WHERE id=?1",
                [claim.firing.id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        let notice = registry
            .db
            .query_row(
                "SELECT event FROM automation_notifications WHERE key=?1",
                [format!("proposal:{}", proposal.id)],
                |row| row.get::<_, String>(0),
            )
            .unwrap();
        // A malformed legacy definition fails after clock/table creation. The
        // entire new checkpoint must roll back, including its schema number.
        registry.db.execute_batch("ALTER TABLE metadata DROP COLUMN automation_proposal_sequence; DROP TABLE automation_precheck_retirements; PRAGMA user_version=12;").unwrap();
        registry
            .db
            .execute(
                "UPDATE automation_revisions SET definition='{malformed' WHERE automation=?1",
                [automation.id.to_string()],
            )
            .unwrap();
        drop(registry);
        assert!(Registry::at(&root.path().join("registry.sqlite3")).is_err());
        let db = Connection::open(root.path().join("registry.sqlite3")).unwrap();
        assert_eq!(
            db.pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            12
        );
        assert!(!db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('metadata') WHERE name='automation_proposal_sequence')",[],|row|row.get::<_,bool>(0)).unwrap());
        assert!(!db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='automation_precheck_retirements')",[],|row|row.get::<_,bool>(0)).unwrap());
        assert_eq!(
            db.query_row(
                "SELECT record FROM automation_firings WHERE id=?1",
                [claim.firing.id.to_string()],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            firing
        );
        assert_eq!(
            db.query_row(
                "SELECT event FROM automation_notifications WHERE key=?1",
                [format!("proposal:{}", proposal.id)],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            notice
        );
        assert_eq!(
            db.query_row(
                "SELECT pending FROM automation_proposals WHERE id=?1",
                [proposal.id.to_string()],
                |row| row.get::<_, bool>(0)
            )
            .unwrap(),
            true
        );
        // Repairing the unrelated malformed input permits the same upgrade;
        // the accepted firing and pending request still have their identities.
        db.execute(
            "UPDATE automation_revisions SET definition=?2 WHERE automation=?1",
            params![
                automation.id.to_string(),
                encode(&proposal.definition).unwrap()
            ],
        )
        .unwrap();
        drop(db);
        let registry = Registry::at(&root.path().join("registry.sqlite3")).unwrap();
        assert_eq!(
            registry.automation(automation.id).unwrap().active_firing,
            Some(claim.firing.id)
        );
        assert_eq!(registry.pending_automation_proposal_count().unwrap(), 1);
        assert_eq!(registry.automation_proposal_sequence().unwrap(), 0);
    }
    #[test]
    fn automation_checkpoint_refuses_a_future_schema_committed_after_the_outer_read() {
        let (root, registry, proposal) = fixture();
        let database = root.path().join("registry.sqlite3");
        registry.db.execute_batch("ALTER TABLE metadata DROP COLUMN automation_proposal_sequence; DROP TABLE automation_precheck_retirements; PRAGMA user_version=12;").unwrap();
        // This is the opener's admitted version before its writer reservation.
        let observed: u32 = registry
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(observed, 12);
        let competing = Connection::open(&database).unwrap();
        let future = super::super::SCHEMA_VERSION + 1;
        competing.execute_batch(&format!("BEGIN IMMEDIATE; CREATE TABLE future_checkpoint_owned (value TEXT NOT NULL) STRICT; INSERT INTO future_checkpoint_owned VALUES ('preserve future intent'); PRAGMA user_version={future}; COMMIT;")).unwrap();
        let before: Vec<(String, Option<String>)> = competing
            .prepare("SELECT name,sql FROM sqlite_master ORDER BY type,name")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        // The stale outer branch still tries its old checkpoint. Its real
        // production helper must re-admit under BEGIN IMMEDIATE, not stamp13.
        let error = super::super::migrate_automation_extensions(&registry.db).unwrap_err();
        assert!(error.contains("newer than supported"), "{error}");
        assert_eq!(
            registry
                .db
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            future
        );
        let after: Vec<(String, Option<String>)> = registry
            .db
            .prepare("SELECT name,sql FROM sqlite_master ORDER BY type,name")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(after, before, "no extension DDL may run on a future schema");
        assert_eq!(
            registry
                .db
                .query_row("SELECT value FROM future_checkpoint_owned", [], |row| {
                    row.get::<_, String>(0)
                })
                .unwrap(),
            "preserve future intent"
        );
        assert_eq!(
            registry.automation_proposal(proposal.id, None).unwrap(),
            Some(proposal)
        );
        assert!(!registry.db.query_row("SELECT EXISTS(SELECT 1 FROM pragma_table_info('metadata') WHERE name='automation_proposal_sequence')", [], |row| row.get::<_,bool>(0)).unwrap());
    }

    #[test]
    fn automation_checkpoint_accepts_an_already_committed_compatible_upgrade() {
        let (root, registry, proposal) = fixture();
        registry.db.execute_batch("ALTER TABLE metadata DROP COLUMN automation_proposal_sequence; DROP TABLE automation_precheck_retirements; PRAGMA user_version=12;").unwrap();
        let observed: u32 = registry
            .db
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(observed, 12);
        let competing = Connection::open(root.path().join("registry.sqlite3")).unwrap();
        super::super::migrate_automation_extensions(&competing).unwrap();
        competing
            .execute(
                "UPDATE metadata SET automation_proposal_sequence=41 WHERE key=0",
                [],
            )
            .unwrap();
        // The first opener's old12 branch resumes after the other writer has
        // completed the atomic checkpoint and made a subsequent consent edit.
        super::super::migrate_automation_extensions(&registry.db).unwrap();
        assert_eq!(registry.automation_proposal_sequence().unwrap(), 41);
        assert_eq!(
            registry
                .db
                .pragma_query_value::<u32, _>(None, "user_version", |row| row.get(0))
                .unwrap(),
            13
        );
        assert_eq!(
            registry.automation_proposal(proposal.id, None).unwrap(),
            Some(proposal)
        );
    }
}
