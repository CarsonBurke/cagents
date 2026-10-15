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
    // may authenticate its separately bounded private record against the actor/grant.
    pub(crate) started_receipt_present: bool,
}

// At most1024 active first-start obligations enter any shared model push.
// Settled/retired history remains independently addressable and is not pruned.
const MAX_REQUIRED_STARTS: usize = 1024;
const RECORD_COLUMNS: &str =
    "session_id,card_id,intent_operation,generation,attempt,kind,phase,started_receipt IS NOT NULL";

fn invalid_column(index: usize, message: &'static str) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        rusqlite::types::Type::Text,
        Box::new(io::Error::other(message)),
    )
}
fn bounded_text<'a>(
    row: &'a rusqlite::Row<'_>,
    index: usize,
    limit: usize,
) -> rusqlite::Result<&'a str> {
    let text = row
        .get_ref(index)?
        .as_str()
        .map_err(|_| invalid_column(index, "Invalid directory-start text"))?;
    if text.len() > limit {
        return Err(invalid_column(
            index,
            "Directory-start metadata is too large",
        ));
    }
    Ok(text)
}
fn identifier<T: std::str::FromStr<Err = uuid::Error>>(
    row: &rusqlite::Row<'_>,
    index: usize,
) -> rusqlite::Result<T> {
    let text = bounded_text(row, index, 36)?;
    if text.len() != 36 {
        return Err(invalid_column(index, "Invalid directory-start identifier"));
    }
    text.parse().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })
}
fn record(row: &rusqlite::Row<'_>) -> rusqlite::Result<CreatedStartRecord> {
    let generation = row.get::<_, i64>(3)?;
    let requirement = CreatedStartRequirement {
        session: identifier(row, 0)?,
        card: identifier(row, 1)?,
        operation: identifier(row, 2)?,
        generation: u64::try_from(generation)
            .map_err(|_| invalid_column(3, "Invalid directory-start generation"))?,
        attempt: identifier(row, 4)?,
        kind: match bounded_text(row, 5, 11)? {
            "terminal" => CreatedStartKind::Terminal,
            "native_chat" => CreatedStartKind::NativeChat,
            _ => return Err(invalid_column(5, "Invalid directory-start kind")),
        },
    };
    requirement
        .validate()
        .map_err(|_| invalid_column(0, "Invalid directory-start tuple"))?;
    // Inspect presence only. The actual host-issued receipt decoder owns its
    // separate bounded codec; model reads never clone an unbounded JSON body.
    let started_receipt_present = row.get::<_, bool>(7)?;
    let state = match (bounded_text(row, 6, 8)?, started_receipt_present) {
        ("required", false) => CreatedStartState::Required,
        ("started", true) => CreatedStartState::Started,
        ("retired", _) => CreatedStartState::Retired,
        _ => {
            return Err(invalid_column(
                6,
                "Invalid directory-start phase or receipt",
            ));
        }
    };
    Ok(CreatedStartRecord {
        requirement,
        state,
        started_receipt_present,
    })
}
fn read(db: &Connection, session: TerminalId) -> Result<Option<CreatedStartRecord>, String> {
    db.query_row(
        &format!("SELECT {RECORD_COLUMNS} FROM created_start_requirements WHERE session_id=?1"),
        [session.to_string()],
        record,
    )
    .optional()
    .map_err(|error| error.to_string())
}

fn required_for_card(db: &Connection, card: CardId) -> Result<Option<CreatedStartRecord>, String> {
    db.query_row(&format!("SELECT {RECORD_COLUMNS} FROM created_start_requirements WHERE card_id=?1 AND phase='required'"),
        [card.to_string()], record).optional().map_err(|error| error.to_string())
}

pub(super) fn active(db: &Connection) -> Result<Vec<CreatedStartRequirement>, String> {
    let mut statement = db.prepare(&format!(
        "SELECT {RECORD_COLUMNS} FROM created_start_requirements WHERE phase='required' ORDER BY session_id LIMIT {}",
        MAX_REQUIRED_STARTS + 1,
    )).map_err(|error| error.to_string())?;
    let records = statement
        .query_map([], record)
        .map_err(|error| error.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| error.to_string())?;
    if records.len() > MAX_REQUIRED_STARTS {
        return Err(
            "The registry exceeds its active directory-start limit; obligations were preserved"
                .into(),
        );
    }
    Ok(records
        .into_iter()
        .map(|record| record.requirement)
        .collect())
}

/// Ordinary descriptor edits cannot substitute a different actor for a
/// required target. Retired/started history is checked by the actual start
/// boundary, not by presentation: an ended or paired console may stay visible.
/// This is a negative
/// source fence; successful validation still grants no launch authority.
pub(super) fn validate_pane(
    db: &Connection,
    pane: &crate::workspace_model::views::Pane,
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
                return Err(
                    "The session belongs to a different required directory-start target".into(),
                );
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
    tx: &rusqlite::Transaction<'_>,
    requirement: &CreatedStartRequirement,
) -> Result<bool, String> {
    requirement.validate().map_err(str::to_owned)?;
    if let Some(existing) = read(tx, requirement.session)? {
        if existing.requirement == *requirement && existing.state == CreatedStartState::Required {
            return Ok(false);
        }
        return Err(
            "The created-start session already has a different or settled obligation".into(),
        );
    }
    if required_for_card(tx, requirement.card)?.is_some() {
        return Err("The pane already has a required original-created start".into());
    }
    let active_count: i64 = tx
        .query_row(
            "SELECT count(*) FROM created_start_requirements WHERE phase='required'",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if active_count >= MAX_REQUIRED_STARTS as i64 {
        return Err("Too many required directory starts; finish or explicitly cancel an original pending start before creating another".into());
    }
    let attempt_exists: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM created_start_requirements WHERE attempt=?1)",
            [requirement.attempt.to_string()],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if attempt_exists {
        return Err("The created-start attempt was already used".into());
    }
    tx.execute(
        "INSERT INTO created_start_requirements
         (session_id,card_id,intent_operation,generation,attempt,kind,phase,started_receipt)
         VALUES(?1,?2,?3,?4,?5,?6,'required',NULL)",
        params![
            requirement.session.to_string(),
            requirement.card.to_string(),
            requirement.operation.to_string(),
            requirement.generation as i64,
            requirement.attempt.to_string(),
            match requirement.kind {
                CreatedStartKind::Terminal => "terminal",
                CreatedStartKind::NativeChat => "native_chat",
            }
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(true)
}

/// Resolve the negative obligation from the SAME transaction as an already
/// source/consent-validated removal. The before pane supplies lookup identities,
/// never launch or close authority; no client requirement is accepted here.
/// A started native recipient may now be its proved console presentation. Keep
/// the original stored target kind/tuple, rather than rebinding it to that view.
pub(super) fn retirement_requirement(
    tx: &rusqlite::Transaction<'_>,
    before: &crate::workspace_model::views::Pane,
) -> Result<Option<CreatedStartRequirement>, String> {
    let target = match &before.kind {
        crate::model::CardKind::Terminal(session) => Some((session.id, CreatedStartKind::Terminal)),
        crate::model::CardKind::Chat(session) => Some((session.id, CreatedStartKind::NativeChat)),
        _ => None,
    };
    let by_card = required_for_card(tx, before.id)?;
    let by_session = target
        .map(|(session, _)| read(tx, session))
        .transpose()?
        .flatten();
    if by_card.as_ref().is_some_and(|record| {
        target != Some((record.requirement.session, record.requirement.kind))
    }) || by_session
        .as_ref()
        .is_some_and(|record| record.requirement.card != before.id)
    {
        return Err("The removed pane does not match its original directory-start obligation".into());
    }
    Ok(by_session.or(by_card).map(|record| record.requirement))
}

/// Only an owning host's already-validated close/replacement transaction may
/// call this. There is intentionally no public client-controlled retire edit.
/// The tombstone remains permanently bound to this session and attempt.
pub(super) fn retire(
    tx: &rusqlite::Transaction<'_>,
    requirement: &CreatedStartRequirement,
    scope: &crate::workspace_model::views::Scope,
    before: &crate::workspace_model::views::Pane,
) -> Result<bool, String> {
    requirement.validate().map_err(str::to_owned)?;
    let existing =
        read(tx, requirement.session)?.ok_or("The original-created start obligation is missing")?;
    if existing.requirement != *requirement {
        return Err("The original-created start obligation changed".into());
    }
    if existing.state == CreatedStartState::Retired {
        return Ok(false);
    }
    let views = super::views::all(tx)?;
    let owners = views
        .iter()
        .flat_map(|view| {
            view.panes
                .iter()
                .filter(|pane| pane.id == requirement.card)
                .map(move |pane| (&view.scope, pane))
        })
        .collect::<Vec<_>>();
    if owners.len() != 1
        || owners[0].0 != scope
        || before.id != requirement.card
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
    )
    .map_err(|error| error.to_string())?;
    Ok(true)
}

impl Registry {
    /// FD registration invokes this BEFORE acknowledging the retained grant
    /// and BEFORE any default/pending pane publication. Its returned model is
    /// watchable; registration success is never a first-effect startup receipt.
    pub(crate) fn require_created_start(
        &mut self,
        requirement: &CreatedStartRequirement,
    ) -> Result<Applied, String> {
        let tx = self
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|error| error.to_string())?;
        // A newly declared obligation cannot retroactively fence an already
        // published pane. Exact replay is allowed after normal publication.
        if read(&tx, requirement.session)?.is_none() {
            let published = super::views::all(&tx)?
                .iter()
                .flat_map(|view| &view.panes)
                .any(|pane| {
                    pane.id == requirement.card
                        || match &pane.kind {
                            crate::model::CardKind::Terminal(session) => {
                                session.id == requirement.session
                            }
                            crate::model::CardKind::Chat(session) => {
                                session.id == requirement.session
                            }
                            _ => false,
                        }
                });
            if published {
                return Err(
                    "Register original directory authority before publishing its target session"
                        .into(),
                );
            }
        }
        let changed = require(&tx, requirement)?;
        if changed {
            tx.execute("UPDATE metadata SET sequence=sequence+1 WHERE key=0", [])
                .map_err(|error| error.to_string())?;
        }
        tx.commit().map_err(|error| error.to_string())?;
        Ok(Applied {
            model: self.model()?,
            changed,
        })
    }

    /// Both lookups are necessary: replacing a descriptor's session ID must
    /// not evade an outstanding obligation on the same logical pane.
    pub(crate) fn created_start_requirement(
        &self,
        card: CardId,
        session: TerminalId,
    ) -> Result<Option<CreatedStartRecord>, String> {
        let by_session = read(&self.db, session)?;
        let by_card = required_for_card(&self.db, card)?;
        if by_card
            .as_ref()
            .is_some_and(|record| record.requirement.session != session)
            || by_session
                .as_ref()
                .is_some_and(|record| record.requirement.card != card)
        {
            return Err(
                "The pane or session belongs to a different original-created start obligation"
                    .into(),
            );
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
        registry
            .apply(&ModelEdit::OpenProject {
                repository: "/repo".into(),
                name: "repo".into(),
                kind: Default::default(),
            })
            .unwrap();
        let channels = [ChannelId::new(), ChannelId::new()];
        for id in channels {
            registry
                .apply(&ModelEdit::CreateChannel {
                    id,
                    repository: "/repo".into(),
                    title: "channel".into(),
                    title_source: Default::default(),
                })
                .unwrap();
        }
        (registry, directory, channels[0], channels[1])
    }
    fn requirement() -> CreatedStartRequirement {
        CreatedStartRequirement {
            card: CardId::new(),
            session: TerminalId::new(),
            operation: OperationId::new(),
            generation: 7,
            attempt: Uuid::new_v4(),
            kind: CreatedStartKind::Terminal,
        }
    }
    fn pane(requirement: &CreatedStartRequirement) -> Pane {
        let launch = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec![],
            working_directory: "/repo".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        };
        Pane {
            id: requirement.card,
            title: "required".into(),
            user_title: None,
            pending_terminal_start: true,
            width_fraction: 0.5,
            scope: Some("/repo".into()),
            kind: CardKind::Terminal(TerminalSession {
                id: requirement.session,
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                launch: launch.clone(),
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::RelaunchAvailable {
                    recipe: launch,
                    last_exit: None,
                },
            }),
        }
    }
    fn seed(registry: &mut Registry, channel: ChannelId, pane: Pane) {
        registry
            .apply(&ModelEdit::SeedView {
                scope: Scope::Channel(channel),
                panes: vec![pane],
                closed: vec![],
            })
            .unwrap();
    }
    #[test]
    fn directory_start_registration_replay_and_restart_keep_the_negative_obligation() {
        let (mut registry, directory, a, _) = setup();
        let required = requirement();
        let applied = registry.require_created_start(&required).unwrap();
        assert!(applied.changed);
        assert_eq!(
            applied.model.created_start_requirements,
            vec![required.clone()]
        );
        let sequence = applied.model.sequence;
        seed(&mut registry, a, pane(&required));
        let after_publication = registry.model().unwrap();
        assert!(after_publication.sequence > sequence);
        let replay = registry.require_created_start(&required).unwrap();
        assert!(!replay.changed);
        assert_eq!(replay.model, after_publication);
        drop(registry);
        let registry = Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
        let record = registry
            .created_start_requirement(required.card, required.session)
            .unwrap()
            .unwrap();
        assert_eq!(record.requirement, required);
        assert_eq!(record.state, CreatedStartState::Required);
        assert!(
            !record.started_receipt_present,
            "restart does not invent an FD or startup receipt"
        );
    }
    #[test]
    fn directory_start_forked_card_session_attempt_and_generation_do_not_replace_history() {
        let (mut registry, _directory, _, _) = setup();
        let original = requirement();
        registry.require_created_start(&original).unwrap();
        let before = registry.model().unwrap();
        let mut forks = vec![];
        let mut fork = original.clone();
        fork.card = CardId::new();
        forks.push(fork);
        let mut fork = original.clone();
        fork.session = TerminalId::new();
        forks.push(fork);
        let mut fork = original.clone();
        fork.generation += 1;
        forks.push(fork);
        let mut fork = requirement();
        fork.attempt = original.attempt;
        forks.push(fork);
        for fork in forks {
            assert!(registry.require_created_start(&fork).is_err());
            assert_eq!(registry.model().unwrap(), before);
        }
        assert!(
            registry
                .created_start_requirement(original.card, TerminalId::new())
                .is_err()
        );
        assert!(
            registry
                .created_start_requirement(CardId::new(), original.session)
                .is_err()
        );
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
        assert!(
            registry
                .require_created_start(&replacement_obligation)
                .is_err(),
            "new-session metadata cannot retroactively fence an existing published card"
        );
        assert_eq!(registry.model().unwrap(), before_replacement);
        let required = requirement();
        registry.require_created_start(&required).unwrap();
        let mut replacement = pane(&required);
        let CardKind::Terminal(session) = &mut replacement.kind else {
            panic!()
        };
        session.id = TerminalId::new();
        let before = registry.model().unwrap();
        assert!(
            registry
                .apply(&ModelEdit::SeedView {
                    scope: Scope::Channel(a),
                    panes: vec![replacement],
                    closed: vec![],
                })
                .is_err()
        );
        assert_eq!(registry.model().unwrap(), before);
        assert_eq!(
            registry
                .created_start_requirement(required.card, required.session)
                .unwrap()
                .unwrap()
                .state,
            CreatedStartState::Required
        );
    }
    #[test]
    fn directory_start_transfer_and_presentation_edits_do_not_consume_the_requirement() {
        let (mut registry, _directory, a, b) = setup();
        let required = requirement();
        registry.require_created_start(&required).unwrap();
        let original = pane(&required);
        seed(&mut registry, a, original.clone());
        let current = registry
            .model()
            .unwrap()
            .views
            .into_iter()
            .find(|view| view.scope == Scope::Channel(a))
            .unwrap();
        registry
            .apply(&ModelEdit::ChangeView {
                scope: Scope::Channel(a),
                expected_revision: current.revision,
                operation: Uuid::new_v4(),
                change: Change::Transfer {
                    id: required.card,
                    destination: Scope::Channel(b),
                    before: None,
                },
            })
            .unwrap();
        let current = registry
            .model()
            .unwrap()
            .views
            .into_iter()
            .find(|view| view.scope == Scope::Channel(b))
            .unwrap();
        let mut renamed = current.panes[0].clone();
        renamed.user_title = Some("renamed".into());
        renamed.width_fraction = 0.75;
        registry
            .apply(&ModelEdit::ChangeView {
                scope: Scope::Channel(b),
                expected_revision: current.revision,
                operation: Uuid::new_v4(),
                change: Change::Update {
                    before: current.panes[0].clone(),
                    pane: renamed,
                },
            })
            .unwrap();
        assert_eq!(
            registry.model().unwrap().created_start_requirements,
            vec![required.clone()]
        );
        assert_eq!(
            registry
                .created_start_requirement(required.card, required.session)
                .unwrap()
                .unwrap()
                .state,
            CreatedStartState::Required
        );
    }
    #[test]
    fn directory_start_retirement_lookup_uses_transactional_original_target_not_client_metadata() {
        let (mut registry, _directory, _, _) = setup();
        let original = requirement();
        registry.require_created_start(&original).unwrap();
        let native = CreatedStartRequirement {
            kind: CreatedStartKind::NativeChat,
            ..requirement()
        };
        registry.require_created_start(&native).unwrap();
        let tx = registry
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let before = pane(&original);
        assert_eq!(
            retirement_requirement(&tx, &before).unwrap(),
            Some(original.clone())
        );
        let mut replacement = before.clone();
        let CardKind::Terminal(session) = &mut replacement.kind else {
            panic!()
        };
        session.id = TerminalId::new();
        assert!(
            retirement_requirement(&tx, &replacement).is_err(),
            "required card cannot omit its original session"
        );
        let mut alias = before.clone();
        alias.id = CardId::new();
        assert!(
            retirement_requirement(&tx, &alias).is_err(),
            "same-session alias cannot retire the original card"
        );
        let mut ordinary = before.clone();
        ordinary.id = CardId::new();
        let CardKind::Terminal(session) = &mut ordinary.kind else {
            panic!()
        };
        session.id = TerminalId::new();
        assert!(retirement_requirement(&tx, &ordinary).unwrap().is_none());
        let console = pane(&native);
        assert!(
            retirement_requirement(&tx, &console).is_err(),
            "a required native target cannot yet be a console presentation"
        );
        tx.rollback().unwrap();

        // State/receipt presence here is only a metadata fixture, not a start
        // or close proof. The actual owning caller still proves its console.
        registry.db.execute(
            "UPDATE created_start_requirements SET phase='started',started_receipt='opaque-fixture' WHERE session_id=?1",
            [native.session.to_string()],
        ).unwrap();
        let tx = registry
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let stored = retirement_requirement(&tx, &console).unwrap().unwrap();
        assert_eq!(stored, native, "console view cannot rewrite the stored target");
        let mut console_alias = console;
        console_alias.id = CardId::new();
        assert!(retirement_requirement(&tx, &console_alias).is_err());
        tx.rollback().unwrap();
    }

    #[test]
    fn directory_start_retirement_checks_current_source_and_retains_a_refusal_tombstone() {
        let (mut registry, _directory, a, _) = setup();
        let required = requirement();
        registry.require_created_start(&required).unwrap();
        let original = pane(&required);
        seed(&mut registry, a, original.clone());
        let tx = registry
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let mut stale = original.clone();
        stale.pending_terminal_start = false;
        assert!(retire(&tx, &required, &Scope::Channel(a), &stale).is_err());
        assert_eq!(
            read(&tx, required.session).unwrap().unwrap().state,
            CreatedStartState::Required
        );
        assert!(retire(&tx, &required, &Scope::Channel(a), &original).unwrap());
        let view = super::super::views::all(&tx)
            .unwrap()
            .into_iter()
            .find(|view| view.scope == Scope::Channel(a))
            .unwrap();
        super::super::views::apply(
            &tx,
            &ModelEdit::ChangeView {
                scope: Scope::Channel(a),
                expected_revision: view.revision,
                operation: Uuid::new_v4(),
                change: Change::Remove { id: required.card },
            },
        )
        .unwrap();
        tx.commit().unwrap();
        let retired = registry
            .created_start_requirement(required.card, required.session)
            .unwrap()
            .unwrap();
        assert_eq!(retired.state, CreatedStartState::Retired);
        assert!(
            registry
                .model()
                .unwrap()
                .created_start_requirements
                .is_empty()
        );
        assert!(
            registry.require_created_start(&required).is_err(),
            "retirement never renews the old attempt"
        );
    }

    #[test]
    fn directory_start_active_quota_refuses_new_admission_without_hiding_existing_obligations() {
        let (mut registry, _directory, _, _) = setup();
        let first = requirement();
        let tx = registry
            .db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        require(&tx, &first).unwrap();
        for _ in 1..MAX_REQUIRED_STARTS {
            require(&tx, &requirement()).unwrap();
        }
        tx.commit().unwrap();
        let before = registry.model().unwrap();
        assert_eq!(before.created_start_requirements.len(), MAX_REQUIRED_STARTS);
        let encoded = postcard::to_stdvec(&before.created_start_requirements).unwrap();
        assert!(
            encoded.len() < 256 * 1024,
            "bounded requirement carrier alone; not a whole-model or RSS ceiling"
        );
        let error = registry.require_created_start(&requirement()).unwrap_err();
        assert!(error.contains("Too many required directory starts"));
        assert_eq!(registry.model().unwrap(), before);
        assert!(
            !registry.require_created_start(&first).unwrap().changed,
            "exact replay still works at capacity"
        );
        assert_eq!(
            registry
                .created_start_requirement(first.card, first.session)
                .unwrap()
                .unwrap()
                .requirement,
            first
        );
    }

    #[test]
    fn directory_start_metadata_reader_refuses_oversized_identity_without_copying_receipt_body() {
        let (mut registry, _directory, _, _) = setup();
        let required = requirement();
        registry.require_created_start(&required).unwrap();
        // Deliberately corrupt a private fixture record. SQL keeps a large
        // receipt outside the Rust model carrier; presence alone is not proof.
        registry.db.execute("UPDATE created_start_requirements SET phase='started',started_receipt=?1 WHERE session_id=?2",
            params!["x".repeat(1024*1024), required.session.to_string()]).unwrap();
        let record = registry
            .created_start_requirement(required.card, required.session)
            .unwrap()
            .unwrap();
        assert!(record.started_receipt_present);
        assert_eq!(record.state, CreatedStartState::Started);
        assert!(
            registry
                .model()
                .unwrap()
                .created_start_requirements
                .is_empty()
        );
        registry
            .db
            .execute(
                "UPDATE created_start_requirements SET card_id=?1 WHERE session_id=?2",
                params!["x".repeat(1024 * 1024), required.session.to_string()],
            )
            .unwrap();
        assert!(
            registry
                .created_start_requirement(required.card, required.session)
                .is_err()
        );
    }
}
