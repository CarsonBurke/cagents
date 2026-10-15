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

impl Registry {
    pub(crate) fn automation_proposal(
        &self,
        id: Uuid,
        owner: Option<&Origin>,
    ) -> Result<Option<Proposal>, String> {
        read(&self.db, id, owner)
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
                checkout: None, primary_checkout: None,
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
                checkout: None, primary_checkout: None,
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
}
