//! Durable first-start obligations, independent of a pane's JSON projection.
//!
//! These rows never grant permission to launch. The owning host must also
//! possess the original retained directory grant. In particular, neither a
//! missing descriptor marker nor a retired row permits a path-based retry.
//! The cumulative Registry19 controller owns this table and its migration.
use super::*;
use crate::model::{CardId, CreatedStartKind, CreatedStartRequirement, TerminalId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CreatedStartState {
    Required,
    Started,
    Retired,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CreatedStartRecord {
    pub(crate) requirement: CreatedStartRequirement,
    pub(crate) state: CreatedStartState,
    // Presence is not a startup proof. Only the owning host receipt decoder
    // may authenticate this private record against its actual worker/grant.
    pub(crate) started_receipt: Option<String>,
}

fn read(db: &Connection, session: TerminalId) -> Result<Option<CreatedStartRecord>, String> {
    let row = db.query_row(
        "SELECT card_id, intent_operation, generation, attempt, kind, phase, started_receipt
         FROM created_start_requirements WHERE session_id=?1",
        [session.to_string()],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?, row.get::<_, String>(3)?,
            row.get::<_, String>(4)?, row.get::<_, String>(5)?,
            row.get::<_, Option<String>>(6)?)),
    ).optional().map_err(|error| error.to_string())?;
    row.map(|(card, operation, generation, attempt, kind, phase, receipt)| {
        let requirement = CreatedStartRequirement {
            card: card.parse::<CardId>().map_err(|error| error.to_string())?,
            session,
            operation: operation.parse().map_err(|error: uuid::Error| error.to_string())?,
            generation: u64::try_from(generation).map_err(|_| "Invalid created-start generation")?,
            attempt: attempt.parse::<Uuid>().map_err(|error| error.to_string())?,
            kind: match kind.as_str() {
                "terminal" => CreatedStartKind::Terminal,
                "native_chat" => CreatedStartKind::NativeChat,
                _ => return Err("Invalid created-start kind".to_owned()),
            },
        };
        requirement.validate().map_err(str::to_owned)?;
        let state = match (phase.as_str(), receipt.as_ref()) {
            ("required", None) => CreatedStartState::Required,
            ("started", Some(_)) => CreatedStartState::Started,
            ("retired", _) => CreatedStartState::Retired,
            _ => return Err("Invalid created-start phase or receipt".to_owned()),
        };
        Ok(CreatedStartRecord { requirement, state, started_receipt: receipt })
    }).transpose()
}

fn required_for_card(db: &Connection, card: CardId) -> Result<Option<CreatedStartRecord>, String> {
    let session: Option<String> = db.query_row(
        "SELECT session_id FROM created_start_requirements WHERE card_id=?1 AND phase='required'",
        [card.to_string()], |row| row.get(0),
    ).optional().map_err(|error| error.to_string())?;
    session.map(|session| {
        let session = session.parse::<TerminalId>().map_err(|error| error.to_string())?;
        read(db, session)?.ok_or_else(|| "Created-start requirement disappeared".to_owned())
    }).transpose()
}

pub(super) fn active(db: &Connection) -> Result<Vec<CreatedStartRequirement>, String> {
    let mut statement = db.prepare(
        "SELECT session_id FROM created_start_requirements WHERE phase='required' ORDER BY session_id",
    ).map_err(|error| error.to_string())?;
    let sessions = statement.query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>().map_err(|error| error.to_string())?;
    sessions.into_iter().map(|session| {
        let session = session.parse::<TerminalId>().map_err(|error| error.to_string())?;
        read(db, session)?.map(|record| record.requirement)
            .ok_or_else(|| "Created-start requirement disappeared".to_owned())
    }).collect()
}

/// Ordinary descriptor edits cannot substitute a different actor for a
/// required target. Retired/started history is checked by the actual start
/// boundary, not by presentation: an ended or paired console may stay visible.
/// This is a negative
/// source fence; successful validation still grants no launch authority.
pub(super) fn validate_pane(
    db: &Connection, pane: &crate::workspace_model::views::Pane,
) -> Result<(), String> {
    let target = match &pane.kind {
        crate::model::CardKind::Terminal(session) => Some((session.id, CreatedStartKind::Terminal)),
        crate::model::CardKind::Chat(session) => Some((session.id, CreatedStartKind::NativeChat)),
        _ => None,
    };
    if let Some(required) = required_for_card(db, pane.id)? {
        if target != Some((required.requirement.session, required.requirement.kind)) {
            return Err("The pane still requires its exact original-directory start target".into());
        }
    }
    if let Some((session, kind)) = target {
        if let Some(record) = read(db, session)? {
            if record.state == CreatedStartState::Required
                && (record.requirement.card != pane.id || record.requirement.kind != kind)
            {
                return Err("The session belongs to a different required directory-start target".into());
            }
        }
    }
    Ok(())
}

/// Called within the SAME accepted publication/registration transaction,
/// before the pending/default pane can be observed or claimed by any peer.
/// Replay of an unchanged required tuple is idempotent; retired/started tuples
/// cannot be renewed and neither session nor attempt identities are recycled.
pub(super) fn require(
    tx: &rusqlite::Transaction<'_>, requirement: &CreatedStartRequirement,
) -> Result<bool, String> {
    requirement.validate().map_err(str::to_owned)?;
    if let Some(existing) = read(tx, requirement.session)? {
        if existing.requirement == *requirement && existing.state == CreatedStartState::Required {
            return Ok(false);
        }
        return Err("The created-start session already has a different or settled obligation".into());
    }
    if required_for_card(tx, requirement.card)?.is_some() {
        return Err("The pane already has a required original-created start".into());
    }
    let attempt_exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM created_start_requirements WHERE attempt=?1)",
        [requirement.attempt.to_string()], |row| row.get(0),
    ).map_err(|error| error.to_string())?;
    if attempt_exists {
        return Err("The created-start attempt was already used".into());
    }
    tx.execute(
        "INSERT INTO created_start_requirements
         (session_id,card_id,intent_operation,generation,attempt,kind,phase,started_receipt)
         VALUES(?1,?2,?3,?4,?5,?6,'required',NULL)",
        params![requirement.session.to_string(), requirement.card.to_string(),
            requirement.operation.to_string(), requirement.generation as i64,
            requirement.attempt.to_string(), match requirement.kind {
                CreatedStartKind::Terminal => "terminal", CreatedStartKind::NativeChat => "native_chat",
            }],
    ).map_err(|error| error.to_string())?;
    Ok(true)
}

/// Only an owning host's already-validated close/replacement transaction may
/// call this. There is intentionally no public client-controlled retire edit.
/// The tombstone remains permanently bound to this session and attempt.
pub(super) fn retire(
    tx: &rusqlite::Transaction<'_>, requirement: &CreatedStartRequirement,
    scope: &crate::workspace_model::views::Scope,
    before: &crate::workspace_model::views::Pane,
) -> Result<bool, String> {
    requirement.validate().map_err(str::to_owned)?;
    let existing = read(tx, requirement.session)?
        .ok_or("The original-created start obligation is missing")?;
    if existing.requirement != *requirement {
        return Err("The original-created start obligation changed".into());
    }
    if existing.state == CreatedStartState::Retired { return Ok(false); }
    let views = super::views::all(tx)?;
    let owners = views.iter().flat_map(|view| view.panes.iter()
        .filter(|pane| pane.id == requirement.card).map(move |pane| (&view.scope, pane)))
        .collect::<Vec<_>>();
    if owners.len() != 1 || owners[0].0 != scope || before.id != requirement.card
        || owners[0].1.kind != before.kind
        || owners[0].1.pending_terminal_start != before.pending_terminal_start
        || owners[0].1.scope != before.scope
    {
        return Err("The directory-start pane source changed before retirement".into());
    }
    validate_pane(tx, before)?;
    tx.execute(
        "UPDATE created_start_requirements SET phase='retired' WHERE session_id=?1",
        [requirement.session.to_string()],
    ).map_err(|error| error.to_string())?;
    Ok(true)
}

impl Registry {
    /// FD registration invokes this BEFORE acknowledging the retained grant
    /// and BEFORE any default/pending pane publication. Its returned model is
    /// watchable; registration success is never a first-effect startup receipt.
    pub(crate) fn require_created_start(
        &mut self, requirement: &CreatedStartRequirement,
    ) -> Result<Applied, String> {
        let tx = self.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        // A newly declared obligation cannot retroactively fence an already
        // published pane. Exact replay is allowed after normal publication.
        if read(&tx, requirement.session)?.is_none() {
            let published = super::views::all(&tx)?.iter().flat_map(|view| &view.panes)
                .any(|pane| pane.id == requirement.card || match &pane.kind {
                    crate::model::CardKind::Terminal(session) => session.id == requirement.session,
                    crate::model::CardKind::Chat(session) => session.id == requirement.session,
                    _ => false,
                });
            if published { return Err("Register original directory authority before publishing its target session".into()); }
        }
        let changed = require(&tx, requirement)?;
        if changed {
            tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
                .map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(Applied { model: self.model()?, changed })
    }

    /// Both lookups are necessary: replacing a descriptor's session ID must
    /// not evade an outstanding obligation on the same logical pane.
    pub(crate) fn created_start_requirement(
        &self, card: CardId, session: TerminalId,
    ) -> Result<Option<CreatedStartRecord>, String> {
        let by_session = read(&self.db, session)?;
        let by_card = required_for_card(&self.db, card)?;
        if by_card.as_ref().is_some_and(|record| record.requirement.session != session)
            || by_session.as_ref().is_some_and(|record| record.requirement.card != card)
        {
            return Err("The pane or session belongs to a different original-created start obligation".into());
        }
        Ok(by_session.or(by_card))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{CardKind, LaunchRecipe, OperationId, TerminalContinuity, TerminalSession};
    use crate::workspace_model::views::{Change, Pane, Scope};

    // These are owning Registry19 tests. The separate cumulative migration
    // controller must be composed; no test-only DDL creates a parallel schema.
    fn setup() -> (Registry, tempfile::TempDir, ChannelId, ChannelId) {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
        registry.apply(&ModelEdit::OpenProject {
            repository: "/repo".into(), name: "repo".into(), kind: Default::default(),
        }).unwrap();
        let channels = [ChannelId::new(), ChannelId::new()];
        for id in channels {
            registry.apply(&ModelEdit::CreateChannel {
                id, repository: "/repo".into(), title: "channel".into(), title_source: Default::default(),
            }).unwrap();
        }
        (registry, directory, channels[0], channels[1])
    }
    fn requirement() -> CreatedStartRequirement {
        CreatedStartRequirement {
            card: CardId::new(), session: TerminalId::new(), operation: OperationId::new(),
            generation: 7, attempt: Uuid::new_v4(), kind: CreatedStartKind::Terminal,
        }
    }
    fn pane(requirement: &CreatedStartRequirement) -> Pane {
        let launch = LaunchRecipe {
            program: "/bin/sh".into(), arguments: vec![], working_directory: "/repo".into(),
            environment: Default::default(), unset_environment: Default::default(),
        };
        Pane { id: requirement.card, title: "required".into(), user_title: None,
            pending_terminal_start: true, width_fraction: 0.5, scope: Some("/repo".into()),
            kind: CardKind::Terminal(TerminalSession {
                id: requirement.session, owned_codex_listener: None, recovery_origin: None,
                predecessor: None, launch: launch.clone(), transcript_path: None, resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable { recipe: launch, last_exit: None },
            }),
        }
    }
    fn seed(registry: &mut Registry, channel: ChannelId, pane: Pane) {
        registry.apply(&ModelEdit::SeedView { scope: Scope::Channel(channel), panes: vec![pane], closed: vec![] }).unwrap();
    }
    #[test]
    fn directory_start_registration_replay_and_restart_keep_the_negative_obligation() {
        let (mut registry, directory, a, _) = setup();
        let required = requirement();
        let applied = registry.require_created_start(&required).unwrap();
        assert!(applied.changed);
        assert_eq!(applied.model.created_start_requirements, vec![required.clone()]);
        let sequence = applied.model.sequence;
        seed(&mut registry, a, pane(&required));
        let after_publication = registry.model().unwrap();
        assert!(after_publication.sequence > sequence);
        let replay = registry.require_created_start(&required).unwrap();
        assert!(!replay.changed);
        assert_eq!(replay.model, after_publication);
        drop(registry);
        let registry = Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
        let record = registry.created_start_requirement(required.card, required.session).unwrap().unwrap();
        assert_eq!(record.requirement, required);
        assert_eq!(record.state, CreatedStartState::Required);
        assert!(record.started_receipt.is_none(), "restart does not invent an FD or startup receipt");
    }
    #[test]
    fn directory_start_forked_card_session_attempt_and_generation_do_not_replace_history() {
        let (mut registry, _directory, _, _) = setup();
        let original = requirement();
        registry.require_created_start(&original).unwrap();
        let before = registry.model().unwrap();
        let mut forks = vec![];
        let mut fork = original.clone(); fork.card = CardId::new(); forks.push(fork);
        let mut fork = original.clone(); fork.session = TerminalId::new(); forks.push(fork);
        let mut fork = original.clone(); fork.generation += 1; forks.push(fork);
        let mut fork = requirement(); fork.attempt = original.attempt; forks.push(fork);
        for fork in forks {
            assert!(registry.require_created_start(&fork).is_err());
            assert_eq!(registry.model().unwrap(), before);
        }
        assert!(registry.created_start_requirement(original.card, TerminalId::new()).is_err());
        assert!(registry.created_start_requirement(CardId::new(), original.session).is_err());
    }
    #[test]
    fn directory_start_target_cannot_be_published_before_registration_or_substituted_afterward() {
        let (mut registry, _directory, a, _) = setup();
        let late = requirement();
        seed(&mut registry, a, pane(&late));
        assert!(registry.require_created_start(&late).is_err());
        let before_replacement = registry.model().unwrap();
        let mut replacement_obligation = requirement();
        replacement_obligation.card = late.card;
        assert_ne!(replacement_obligation.session, late.session);
        assert!(registry.require_created_start(&replacement_obligation).is_err(),
            "new-session metadata cannot retroactively fence an existing published card");
        assert_eq!(registry.model().unwrap(), before_replacement);
        let required = requirement();
        registry.require_created_start(&required).unwrap();
        let mut replacement = pane(&required);
        let CardKind::Terminal(session) = &mut replacement.kind else { panic!() };
        session.id = TerminalId::new();
        let before = registry.model().unwrap();
        assert!(registry.apply(&ModelEdit::SeedView {
            scope: Scope::Channel(a), panes: vec![replacement], closed: vec![],
        }).is_err());
        assert_eq!(registry.model().unwrap(), before);
        assert_eq!(registry.created_start_requirement(required.card, required.session).unwrap().unwrap().state, CreatedStartState::Required);
    }
    #[test]
    fn directory_start_transfer_and_presentation_edits_do_not_consume_the_requirement() {
        let (mut registry, _directory, a, b) = setup();
        let required = requirement();
        registry.require_created_start(&required).unwrap();
        let original = pane(&required);
        seed(&mut registry, a, original.clone());
        let current = registry.model().unwrap().views.into_iter().find(|view| view.scope == Scope::Channel(a)).unwrap();
        registry.apply(&ModelEdit::ChangeView {
            scope: Scope::Channel(a), expected_revision: current.revision, operation: Uuid::new_v4(),
            change: Change::Transfer { id: required.card, destination: Scope::Channel(b), before: None },
        }).unwrap();
        let current = registry.model().unwrap().views.into_iter().find(|view| view.scope == Scope::Channel(b)).unwrap();
        let mut renamed = current.panes[0].clone(); renamed.user_title = Some("renamed".into()); renamed.width_fraction = 0.75;
        registry.apply(&ModelEdit::ChangeView {
            scope: Scope::Channel(b), expected_revision: current.revision, operation: Uuid::new_v4(),
            change: Change::Update { before: current.panes[0].clone(), pane: renamed },
        }).unwrap();
        assert_eq!(registry.model().unwrap().created_start_requirements, vec![required.clone()]);
        assert_eq!(registry.created_start_requirement(required.card, required.session).unwrap().unwrap().state, CreatedStartState::Required);
    }
    #[test]
    fn directory_start_retirement_checks_current_source_and_retains_a_refusal_tombstone() {
        let (mut registry, _directory, a, _) = setup();
        let required = requirement();
        registry.require_created_start(&required).unwrap();
        let original = pane(&required);
        seed(&mut registry, a, original.clone());
        let tx = registry.db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate).unwrap();
        let mut stale = original.clone(); stale.pending_terminal_start = false;
        assert!(retire(&tx, &required, &Scope::Channel(a), &stale).is_err());
        assert_eq!(read(&tx, required.session).unwrap().unwrap().state, CreatedStartState::Required);
        assert!(retire(&tx, &required, &Scope::Channel(a), &original).unwrap());
        let view = super::super::views::all(&tx).unwrap().into_iter().find(|view| view.scope == Scope::Channel(a)).unwrap();
        super::super::views::apply(&tx, &ModelEdit::ChangeView {
            scope: Scope::Channel(a), expected_revision: view.revision, operation: Uuid::new_v4(),
            change: Change::Remove { id: required.card },
        }).unwrap();
        tx.commit().unwrap();
        let retired = registry.created_start_requirement(required.card, required.session).unwrap().unwrap();
        assert_eq!(retired.state, CreatedStartState::Retired);
        assert!(registry.model().unwrap().created_start_requirements.is_empty());
        assert!(registry.require_created_start(&required).is_err(), "retirement never renews the old attempt");
    }
}
