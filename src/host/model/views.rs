use super::*;
use crate::workspace_model::views::{Change, Scope, View};

pub(in crate::host::model) mod store;
pub(super) use store::{all, occupied, operation_seen, read};

pub(super) fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    let current: u32 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if current >= 8 {
        return Ok(());
    }
    let legacy: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='workspace_views')",
            [],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if legacy {
        schema::require_legacy_views(tx)?;
    }
    tx.execute_batch(store::V22_VIEW_TABLES)
        .map_err(|error| error.to_string())?;
    if legacy {
        tx.execute_batch("INSERT INTO shared_view_scopes(scope,channel) SELECT json_object('Channel',channel), channel FROM workspace_views;
            INSERT INTO shared_views SELECT json_object('Channel',channel),revision,panes,retired FROM workspace_views;
            INSERT INTO shared_view_panes SELECT id,json_object('Channel',channel) FROM workspace_view_panes;
            INSERT INTO shared_view_edits SELECT operation,json_object('Channel',channel) FROM workspace_view_edits;
            DROP TABLE workspace_view_edits; DROP TABLE workspace_view_panes; DROP TABLE workspace_views;") .map_err(|error| error.to_string())?;
    }
    Ok(())
}
/// Private durable provenance for a managed terminal pane whose ended
/// presentation intentionally no longer contains a live attachment proof.
/// Older ended panes are never backfilled from a terminal ID or launch recipe.
pub(super) fn migrate_terminal_sources(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    tx.execute_batch(store::V22_TERMINAL_SOURCES)
        .map_err(|error| error.to_string())?;
    Ok(())
}

fn ended_terminal(session: &crate::model::TerminalSession) -> bool {
    use crate::model::TerminalContinuity;
    match &session.continuity {
        TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: Some(_),
        } => recipe == &session.launch,
        TerminalContinuity::Exited { .. } => true,
        _ => false,
    }
}

/// Only presentation metadata may accompany the end or change afterward.
/// Clone-and-compare conservatively keeps any future identity fields fixed too.
fn same_terminal_identity(
    before: &crate::model::TerminalSession,
    after: &crate::model::TerminalSession,
) -> bool {
    let mut expected = before.clone();
    expected.continuity = after.continuity.clone();
    expected.resume_hint = after.resume_hint.clone();
    expected.transcript_path = after.transcript_path.clone();
    expected == *after
}

fn terminal_source(
    db: &Connection,
    id: crate::model::CardId,
) -> Result<Option<(crate::model::TerminalSession, crate::model::TerminalSession)>, String> {
    let raw: Option<(String, String)> = db
        .query_row(
            "SELECT source,ended FROM shared_view_terminal_sources WHERE pane=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    raw.map(|(source, ended)| {
        Ok((
            serde_json::from_str(&source).map_err(|error| error.to_string())?,
            serde_json::from_str(&ended).map_err(|error| error.to_string())?,
        ))
    })
    .transpose()
}
fn write_terminal_source(
    db: &Connection,
    id: crate::model::CardId,
    source: &crate::model::TerminalSession,
    ended: &crate::model::TerminalSession,
) -> Result<(), String> {
    db.execute(
        "INSERT INTO shared_view_terminal_sources(pane,source,ended) VALUES(?1,?2,?3)
        ON CONFLICT(pane) DO UPDATE SET source=excluded.source,ended=excluded.ended",
        params![
            id.to_string(),
            serde_json::to_string(source).map_err(|error| error.to_string())?,
            serde_json::to_string(ended).map_err(|error| error.to_string())?
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn update_terminal_source(
    db: &Connection,
    id: crate::model::CardId,
    claimed_before: &crate::model::CardKind,
    before: Option<&crate::model::CardKind>,
    after: Option<&crate::model::CardKind>,
) -> Result<(), String> {
    use crate::model::CardKind;
    if before == after {
        return Ok(());
    }
    if before == Some(claimed_before) {
        if let (Some(CardKind::Terminal(before)), Some(CardKind::Terminal(after))) = (before, after)
        {
            if same_terminal_identity(before, after) && ended_terminal(after) {
                if before
                    .continuity
                    .live_proof()
                    .is_some_and(|proof| proof.is_complete())
                {
                    return write_terminal_source(db, id, before, after);
                }
                if before.continuity == after.continuity {
                    if let Some((source, ended)) = terminal_source(db, id)? {
                        if ended == *before {
                            return write_terminal_source(db, id, &source, after);
                        }
                    }
                }
            }
        }
    }
    db.execute(
        "DELETE FROM shared_view_terminal_sources WHERE pane=?1",
        [id.to_string()],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn exists(db: &Connection, scope: &Scope) -> Result<bool, String> {
    match scope {
        Scope::Channel(id) => db.query_row(
            "SELECT EXISTS(SELECT 1 FROM channels WHERE id=?1)",
            [id.to_string()],
            |row| row.get(0),
        ),
        Scope::Worktree { repository, .. } => db.query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE repository=?1)",
            [repository_key(repository)?],
            |row| row.get(0),
        ),
    }
    .map_err(|error| error.to_string())
}
fn project(db: &Connection, scope: &Scope) -> Result<PathBuf, String> {
    match scope {
        Scope::Channel(id) => db
            .query_row(
                "SELECT repository FROM channels WHERE id=?1",
                [id.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| error.to_string())?
            .map(PathBuf::from)
            .ok_or_else(|| format!("Channel {id} is no longer open")),
        Scope::Worktree { repository, .. } if exists(db, scope)? => Ok(repository.clone()),
        Scope::Worktree { .. } => Err("The pane's project is no longer open".into()),
    }
}

/// Permanent transfer refusals are caller errors, distinct from storage
/// failures. This same predicate runs before publication and inside the write
/// transaction; a rejected edit never acknowledges an unchanged placement.
pub(super) fn refusal(
    db: &Connection,
    scope: &Scope,
    change: &Change,
) -> Result<Option<String>, String> {
    let Change::Transfer {
        id, destination, ..
    } = change
    else {
        return Ok(None);
    };
    if !exists(db, scope)? {
        return Ok(Some("The source view is no longer open".into()));
    }
    let view = read(db, scope)?;
    let Some(pane) = view.panes.iter().find(|pane| pane.id == *id) else {
        return Ok(Some(format!("Pane {id} is not in the source view")));
    };
    if let Some(agent) = pane_agent(db, pane)? {
        if !current_agent_process(&agent, pane) {
            return Ok(Some(
                "The agent pane changed process; refresh before moving it".into(),
            ));
        }
        if agent.spec.target.repository != project(db, scope)? {
            return Ok(Some("The agent pane belongs to another project".into()));
        }
    }
    if !exists(db, destination)? {
        return Ok(Some("The destination view is no longer open".into()));
    }
    Ok((project(db, scope)? != project(db, destination)?)
        .then(|| "Panes can only move between views of the same project".into()))
}

/// The session index finds the owner, but does not authenticate a process:
/// same-ID successor adoption can precede refreshing its shared pane.
fn pane_agent(
    db: &Connection,
    pane: &crate::workspace_model::views::Pane,
) -> Result<Option<crate::agent_execution::Agent>, String> {
    let session = match &pane.kind {
        crate::model::CardKind::Terminal(session) => session.id,
        crate::model::CardKind::Chat(session) => session.id,
        _ => return Ok(None),
    };
    db.query_row(
        "SELECT record FROM agents WHERE closed=0 AND session_id=?1",
        [session.to_string()],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|error| error.to_string())?
    .map(|record| serde_json::from_str(&record).map_err(|error| error.to_string()))
    .transpose()
}

fn current_agent_process(
    agent: &crate::agent_execution::Agent,
    pane: &crate::workspace_model::views::Pane,
) -> bool {
    use crate::agent_execution::HostedSession;
    let source = match &pane.kind {
        crate::model::CardKind::Terminal(session) => HostedSession::Terminal(session.clone()),
        crate::model::CardKind::Chat(session) => HostedSession::Chat(session.clone()),
        _ => return false,
    };
    // Equal absent or incomplete proofs do not authenticate a legacy record.
    // Keep same_process comparison semantics separate from live admission.
    if !source.has_complete_proof() {
        return false;
    }
    agent
        .session
        .as_ref()
        .is_some_and(|session| session.same_process(&source))
        || agent
            .console
            .as_ref()
            .is_some_and(|console| HostedSession::Terminal(console.clone()).same_process(&source))
}

/// Pane ownership and agent adoption must agree after both commit and restart.
/// Keep the original launch spec unchanged: it also identifies an idempotent
/// submission, whereas placement is the agent's current durable location.
fn transfer_agent(
    db: &Connection,
    pane: &crate::workspace_model::views::Pane,
    destination: &Scope,
    repository: &Path,
) -> Result<(), String> {
    if let Some(mut agent) = pane_agent(db, pane)? {
        if !current_agent_process(&agent, pane) {
            return Err("The agent pane changed process; refresh before moving it".into());
        }
        if agent.spec.target.repository != repository {
            return Err("The agent pane belongs to another project".into());
        }
        // A native agent can also own a separately saved console pane. Its
        // current display is the canonical agent card; moving another proved
        // surface must not relocate that card on the next window adoption.
        if pane.id != crate::model::CardId::from_uuid(agent.id.as_uuid()) {
            return Ok(());
        }
        agent.placement = destination.clone();
        // Keep a daemon stop's `process_end` marker, which lives only in the
        // stored record: moving a stopped card must not make it unclosable.
        db.execute(
            "UPDATE agents SET record=CASE WHEN json_extract(record,'$.process_end') IS NULL THEN ?2
                 ELSE json_set(?2,'$.process_end',json_extract(record,'$.process_end')) END
             WHERE id=?1",
            params![
                agent.id.to_string(),
                serde_json::to_string(&agent).map_err(|error| error.to_string())?
            ],
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}
/// A channel whose view was emptied by closing its only pane, as nearly every
/// view in a long-lived store is.
#[cfg(test)]
pub(super) struct EmptiedView(Scope);

#[cfg(test)]
impl EmptiedView {
    pub(super) fn plant(registry: &mut Registry) -> Self {
        let repository = std::path::PathBuf::from("/planted/emptied-view");
        registry
            .apply(&ModelEdit::OpenProject {
                repository: repository.clone(),
                name: "emptied".into(),
                kind: Default::default(),
            })
            .unwrap();
        let channel = ChannelId::new();
        registry
            .apply(&ModelEdit::CreateChannel {
                id: channel,
                repository,
                title: "emptied".into(),
                title_source: Default::default(),
            })
            .unwrap();
        let scope = Scope::Channel(channel);
        let pane = crate::workspace_model::views::Pane {
            id: crate::model::CardId::new(),
            title: "closed".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: None,
            kind: crate::model::CardKind::Browser(crate::model::BrowserSession {
                id: crate::model::BrowserId::new(),
                current_url: "https://example.com".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.,
                scroll_y: 0.,
                fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
            }),
        };
        let id = pane.id;
        for change in [Change::Insert { pane, before: None }, Change::Remove { id }] {
            registry
                .apply(&ModelEdit::ChangeView {
                    expected_revision: registry.view_revision(&scope).unwrap(),
                    scope: scope.clone(),
                    operation: Uuid::new_v4(),
                    change,
                })
                .unwrap();
        }
        assert!(read(&registry.db, &scope).unwrap().panes.is_empty());
        Self(scope)
    }

    /// Runs `search` while this view's stored tombstones cannot be parsed, so
    /// a search that reads emptied views fails, and one that skips them
    /// answers as it otherwise would. Whole-model reads, such as the model an
    /// applied edit returns, fail meanwhile, so `search` must not apply edits.
    pub(super) fn unreadable_during<T>(
        &self,
        registry: &mut Registry,
        search: impl FnOnce(&mut Registry) -> T,
    ) -> T {
        let scope = store::scope_id(&registry.db, &self.0).unwrap().unwrap();
        let readable: String = registry
            .db
            .query_row(
                "SELECT pane FROM retired_panes WHERE scope = ?1",
                [scope],
                |row| row.get(0),
            )
            .expect("the emptied view keeps exactly the tombstone of its closed pane");
        let set = |registry: &Registry, from: &str, to: &str| {
            assert_eq!(
                registry
                    .db
                    .execute(
                        "UPDATE retired_panes SET pane = ?3 WHERE scope = ?1 AND pane = ?2
                         AND NOT EXISTS(SELECT 1 FROM panes WHERE panes.scope = ?1)",
                        params![scope, from, to],
                    )
                    .unwrap(),
                1,
                "the emptied view holds no pane rows"
            );
        };
        set(registry, &readable, "unreadable");
        assert!(all(&registry.db).is_err(), "reading every view now fails");
        let found = search(registry);
        set(registry, "unreadable", &readable);
        found
    }
}

/// Read the pane still presenting this execution agent's resource without
/// changing it. Preflight and retirement share this exact authority predicate.
/// The current owner can differ from the agent's original placement after a
/// transfer. A replacement conversation in the same card belongs to its owner.
fn agent_retirement_view(
    db: &Connection,
    agent: &crate::agent_execution::Agent,
) -> Result<Option<View>, String> {
    use crate::{
        agent_execution::HostedSession,
        model::{CardId, CardKind},
    };
    let id = CardId::from_uuid(agent.id.as_uuid());
    let Some(scope) = owner(db, id)? else {
        return Ok(None);
    };
    let view = read(db, &scope)?;
    let Some(pane) = view.panes.iter().find(|pane| pane.id == id) else {
        return Ok(None);
    };
    let current = match &pane.kind {
        CardKind::Chat(session) => HostedSession::Chat(session.clone()),
        CardKind::Terminal(session) => HostedSession::Terminal(session.clone()),
        _ => return Ok(None),
    };
    let retained = agent
        .session
        .as_ref()
        .is_some_and(|source| source.same_process(&current))
        || agent
            .console
            .as_ref()
            .is_some_and(|source| HostedSession::Terminal(source.clone()).same_process(&current));
    let retained = if retained {
        true
    } else if let CardKind::Terminal(ended) = &pane.kind {
        let original = agent
            .session
            .iter()
            .filter_map(|source| match source {
                HostedSession::Terminal(session) => Some(session),
                _ => None,
            })
            .chain(agent.console.iter())
            .collect::<Vec<_>>();
        if !ended_terminal(ended)
            || !original
                .iter()
                .any(|source| source.id == ended.id && source.launch == ended.launch)
        {
            false
        } else if let Some((source, recorded_ended)) = terminal_source(db, id)? {
            if &recorded_ended != ended {
                return Err("Cannot safely retire the ended managed terminal pane: its current descriptor has no exact retained generation proof".into());
            }
            original.iter().any(|retained| {
                HostedSession::Terminal((**retained).clone())
                    .same_process(&HostedSession::Terminal(source.clone()))
            })
        } else {
            return Err("Cannot safely retire the ended managed terminal pane: historical generation proof is unavailable; the pane was retained and no retirement was guessed".into());
        }
    } else {
        false
    };
    if !retained {
        return Ok(None);
    }
    Ok(Some(view))
}

pub(super) fn preflight_retire_agent(
    db: &Connection,
    agent: &crate::agent_execution::Agent,
) -> Result<(), String> {
    agent_retirement_view(db, agent).map(|_| ())
}

pub(super) fn retire_agent(
    db: &Connection,
    agent: &crate::agent_execution::Agent,
) -> Result<bool, String> {
    let Some(mut view) = agent_retirement_view(db, agent)? else {
        return Ok(false);
    };
    let id = crate::model::CardId::from_uuid(agent.id.as_uuid());
    view.apply(&Change::Remove { id })?;
    view.revision = store::next_revision(db)?;
    write(db, &view)?;
    Ok(true)
}

pub(super) fn apply(
    transaction: &rusqlite::Transaction<'_>,
    edit: &ModelEdit,
) -> Result<bool, String> {
    let scope = match edit {
        ModelEdit::SeedView { scope, .. } | ModelEdit::ChangeView { scope, .. } => scope,
        _ => return Err("expected a workspace view edit".into()),
    };
    if !exists(transaction, scope)? {
        if matches!(
            edit,
            ModelEdit::ChangeView {
                change: Change::Transfer { .. },
                ..
            }
        ) {
            return Err("The source view is no longer open".into());
        }
        return Ok(false);
    }
    // Even an empty seed creates an authoritative view, so another client sees
    // emptiness rather than an uninitialized migration.
    let existed = store::scope_id(transaction, scope)?.is_some();
    let scope_row = store::register(transaction, scope)?;
    let mut view = read(transaction, scope)?;
    let stored = view.clone();
    let mut changed = false;
    let mut deferred_target = None;
    match edit {
        ModelEdit::SeedView { panes, closed, .. } => {
            // Migration is additive, including old independent windows. Closed
            // IDs are applied first and prevent stale caches from resurrecting.
            for id in closed {
                changed |= view.apply(&Change::Remove { id: *id })?;
            }
            for pane in panes {
                if view.panes.iter().any(|existing| {
                    existing.id != pane.id
                        && crate::workspace_model::views::same_resource(&existing.kind, &pane.kind)
                }) || resource_owner(transaction, pane)?.is_some()
                    || owner(transaction, pane.id)?.is_some_and(|owner| &owner != scope)
                {
                    continue;
                }
                changed |= view.apply(&Change::Insert {
                    pane: pane.clone(),
                    before: None,
                })?;
            }
        }
        ModelEdit::ChangeView {
            expected_revision,
            operation,
            change,
            ..
        } => {
            if operation_seen(transaction, *operation)? {
                return Ok(false);
            }
            if view.revision != *expected_revision {
                return Err(Registry::VIEW_REVISION_CONFLICT.into());
            }
            changed = match change {
                Change::Transfer {
                    id,
                    destination,
                    before,
                } => {
                    if let Some(reason) = refusal(transaction, scope, change)? {
                        return Err(reason);
                    }
                    let pane = view
                        .panes
                        .iter()
                        .find(|pane| pane.id == *id)
                        .cloned()
                        .ok_or_else(|| format!("Pane {id} is not in the source view"))?;
                    let repository = project(transaction, scope)?;
                    if destination == scope {
                        false
                    } else {
                        let destination_row = store::register(transaction, destination)?;
                        let mut target = read(transaction, destination)?;
                        let target_before = target.clone();
                        target.retired.retain(|retired| retired != id);
                        target.apply(&Change::Insert {
                            pane: pane.clone(),
                            before: *before,
                        })?;
                        target.revision = store::next_revision(transaction)?;
                        // Publish only after the source relinquishes its
                        // active placement, inside this same transaction.
                        deferred_target = Some((target_before, target));
                        transfer_agent(transaction, &pane, destination, &repository)?;
                        // The row moves, so what hangs off the pane moves with it.
                        store::move_pane(transaction, *id, destination_row)?;
                        view.apply(&Change::Remove { id: *id })?
                    }
                }
                Change::Insert { pane, .. } | Change::InsertIfEmpty { pane }
                    if resource_owner(transaction, pane)?.is_some() =>
                {
                    false
                }
                Change::Insert { pane, .. } | Change::InsertIfEmpty { pane } => {
                    if owner(transaction, pane.id)?.is_some_and(|owner| &owner != scope) {
                        return Err(
                            "pane already belongs to another view; use an atomic transfer".into(),
                        );
                    }
                    view.apply(change)?
                }
                Change::Update { before, pane } => {
                    // Reject the whole edit before recording its operation ID;
                    // the client can refuse source intent and retry presentation
                    // separately against the authoritative pane.
                    if view.source_update_conflicts(before, pane) {
                        return Err(Registry::VIEW_SOURCE_CONFLICT.into());
                    }
                    if resource_owner(transaction, pane)?.is_some() {
                        false
                    } else {
                        let previous = view
                            .panes
                            .iter()
                            .find(|current| current.id == pane.id)
                            .map(|current| current.kind.clone());
                        let changed = view.apply(change)?;
                        let current = view
                            .panes
                            .iter()
                            .find(|current| current.id == pane.id)
                            .map(|current| &current.kind);
                        update_terminal_source(
                            transaction,
                            pane.id,
                            &before.kind,
                            previous.as_ref(),
                            current,
                        )?;
                        changed
                    }
                }
                _ => view.apply(change)?,
            };
            store::record_operation(transaction, *operation, scope_row)?;
        }
        _ => unreachable!(),
    }
    if changed || !existed {
        view.revision = store::next_revision(transaction)?;
        write_from(transaction, &stored, &view)?;
    }
    if let Some((before, target)) = deferred_target {
        write_from(transaction, &before, &target)?;
    }
    Ok(changed || !existed)
}

impl Registry {
    /// Only saved membership, never proof that an exact worker was cleaned up.
    pub(crate) fn pinned_terminals(
        &self,
        candidates: &std::collections::BTreeSet<crate::model::TerminalId>,
    ) -> Result<std::collections::BTreeSet<crate::model::TerminalId>, String> {
        terminal_retention::pinned_terminals(&self.db, candidates)
    }

    pub(crate) fn title_conversation(
        &self,
        id: crate::model::ChannelId,
        card: crate::model::CardId,
        revision: u64,
    ) -> Result<crate::chat::Session, String> {
        let view = read(&self.db, &Scope::Channel(id))?;
        if view.revision != revision || view.primary_chat() != Some(card) {
            return Err("The channel's primary conversation changed".into());
        }
        view.panes
            .into_iter()
            .find_map(|pane| match pane.kind {
                crate::model::CardKind::Chat(session) if pane.id == card => Some(session),
                _ => None,
            })
            .ok_or_else(|| "The primary chat is unavailable".into())
    }

    pub(crate) const VIEW_REVISION_CONFLICT: &'static str = "workspace view revision conflict";
    pub(crate) const VIEW_SOURCE_CONFLICT: &'static str = "The pane source changed; refuse stale source intent and rebase presentation on the current pane";

    pub(crate) fn view_change_refusal(
        &self,
        scope: &Scope,
        change: &Change,
    ) -> Result<Option<String>, String> {
        refusal(&self.db, scope, change)
    }

    pub(crate) fn view_revision(&self, scope: &Scope) -> Result<u64, String> {
        Ok(read(&self.db, scope)?.revision)
    }
    pub(crate) fn view_operation_seen(&self, operation: Uuid) -> Result<bool, String> {
        operation_seen(&self.db, operation)
    }

    /// Forgets one batch of view operations applied more than
    /// [`store::OPERATION_RETENTION`] ago. Returns how many it forgot. One
    /// bounded batch per call keeps the writer lock short; the collector's
    /// interval drains a backlog well inside the retention period.
    pub(crate) fn collect_view_operations(&self) -> Result<usize, String> {
        self.collect_view_operations_before(
            store::now_unix_ms().saturating_sub(store::OPERATION_RETENTION.as_millis() as i64),
        )
    }

    fn collect_view_operations_before(&self, horizon_unix_ms: i64) -> Result<usize, String> {
        let tx = rusqlite::Transaction::new_unchecked(
            &self.db,
            rusqlite::TransactionBehavior::Immediate,
        )
        .map_err(|error| error.to_string())?;
        schema::require_current(&tx)?;
        let forgotten = store::prune_operations(&tx, horizon_unix_ms)?;
        tx.commit().map_err(|error| error.to_string())?;
        Ok(forgotten)
    }
}

fn resource_owner(
    db: &Connection,
    pane: &crate::workspace_model::views::Pane,
) -> Result<Option<crate::model::CardId>, String> {
    store::resource_holder(db, pane)
}

fn owner(db: &Connection, id: crate::model::CardId) -> Result<Option<Scope>, String> {
    store::owner(db, id)
}

/// Rotate an existing canonical pane with its authenticated holder, before the
/// new Agent descriptor becomes visible. A stale or replaced pane is evidence
/// to preserve, not permission to insert a pane or discover another alias.
pub(super) fn adopt_authenticated_agent_pane(
    db: &Connection,
    agent: crate::model::AgentId,
    retained: &[crate::agent_execution::HostedSession],
    successor: &crate::agent_execution::HostedSession,
) -> Result<bool, String> {
    use crate::{agent_execution::HostedSession, model::CardKind};
    let id = crate::model::CardId::from_uuid(agent.as_uuid());
    let Some(scope) = owner(db, id)? else {
        return Ok(false);
    };
    let mut view = read(db, &scope)?;
    if view.retired.contains(&id) || view.panes.iter().filter(|pane| pane.id == id).count() != 1 {
        return Ok(false);
    }
    let pane = view.panes.iter_mut().find(|pane| pane.id == id).unwrap();
    let replacement = match (&pane.kind, successor) {
        (CardKind::Chat(current), HostedSession::Chat(next))
            if retained.contains(&HostedSession::Chat(current.clone()))
                && (crate::chat::is_upgrade_recovery(current, next)
                    || crate::chat::is_upgrade_identity_learning(current, next)) =>
        {
            CardKind::Chat(next.clone())
        }
        (CardKind::Terminal(current), _) => {
            let terminal = |source: &HostedSession| match source {
                HostedSession::Terminal(session) => Some(session.clone()),
                HostedSession::Chat(session) => session.terminal.clone(),
            };
            let Some(next) = terminal(successor) else {
                return Ok(false);
            };
            if !retained.iter().any(|source| {
                terminal(source).as_ref() == Some(current)
                    && match (source, successor) {
                        (HostedSession::Terminal(_), HostedSession::Terminal(_)) => true,
                        (HostedSession::Chat(old), HostedSession::Chat(next)) => {
                            crate::chat::is_upgrade_recovery(old, next)
                                || crate::chat::is_upgrade_identity_learning(old, next)
                        }
                        _ => false,
                    }
            }) || !current
                .continuity
                .live_proof()
                .is_some_and(|proof| proof.is_complete())
                || !next
                    .continuity
                    .live_proof()
                    .is_some_and(|proof| proof.is_complete())
            {
                return Ok(false);
            }
            CardKind::Terminal(next)
        }
        _ => return Ok(false),
    };
    if pane.kind == replacement {
        return Ok(false);
    }
    // Another pane already presenting the successor keeps it, as an insert
    // of a second pane for one resource would leave it.
    let adopted = crate::workspace_model::views::Pane {
        kind: replacement,
        ..pane.clone()
    };
    if store::resource_holder(db, &adopted)?.is_some() {
        return Ok(false);
    }
    *pane = adopted;
    view.revision = store::next_revision(db)?;
    // Holder continuity does not start a new channel naming choice, including
    // learning a previously unobserved provider thread. Preserve all placement,
    // presentation, retirement and naming state outside the descriptor itself.
    terminal_retention::sync_view_pins(db, &view.panes)?;
    store::store(db, &view)?;
    Ok(true)
}

/// Point a stopped agent's pane at the process that resumed it. Position,
/// placement, presentation and naming are unchanged. A pane that is missing,
/// retired or already shows another process is left alone; the agent itself
/// still rebinds, so it is never stranded on an exited descriptor.
pub(super) fn rebind_agent_pane(
    db: &Connection,
    agent: crate::model::AgentId,
    predecessor: &crate::agent_execution::HostedSession,
    successor: &crate::agent_execution::HostedSession,
) -> Result<bool, String> {
    use crate::{agent_execution::HostedSession, model::CardKind};
    let HostedSession::Chat(next) = successor else {
        return Ok(false);
    };
    let id = crate::model::CardId::from_uuid(agent.as_uuid());
    let Some(scope) = owner(db, id)? else {
        return Ok(false);
    };
    let mut view = read(db, &scope)?;
    if view.retired.contains(&id) {
        return Ok(false);
    }
    let Some(pane) = view.panes.iter_mut().find(|pane| pane.id == id) else {
        return Ok(false);
    };
    match &pane.kind {
        CardKind::Chat(current)
            if HostedSession::Chat(current.clone()).same_process(predecessor) => {}
        _ => return Ok(false),
    }
    let rebound = crate::workspace_model::views::Pane {
        kind: CardKind::Chat(next.clone()),
        ..pane.clone()
    };
    if store::resource_holder(db, &rebound)?.is_some() {
        return Ok(false);
    }
    *pane = rebound;
    view.revision = store::next_revision(db)?;
    store::store(db, &view)?;
    Ok(true)
}

fn write(db: &Connection, view: &View) -> Result<(), String> {
    let before = read(db, &view.scope)?;
    write_from(db, &before, view)
}

/// [`write`] from a `before` read earlier in the transaction, for a transfer:
/// its pane row has already moved, but each view's derived state must still
/// observe the transition from what the view held.
fn write_from(db: &Connection, before: &View, view: &View) -> Result<(), String> {
    if let Scope::Channel(id) = &view.scope {
        let primary = |view: &View| {
            view.panes.iter().find_map(|pane| match &pane.kind {
                crate::model::CardKind::Chat(session) => Some((
                    pane.id,
                    session.id,
                    session.launch.provider,
                    session.launch.session_id.clone(),
                )),
                _ => None,
            })
        };
        if primary(before) != primary(view) {
            // A new primary conversation starts a new automatic naming choice.
            // Manual/legacy names describe the channel and survive all moves.
            db.execute(
                "UPDATE channels SET title='', title_source=0 WHERE id=?1 AND title_source<>2",
                [id.to_string()],
            )
            .map_err(|error| error.to_string())?;
        }
    }
    // Pins hang off pane rows, so the rows come first.
    store::store(db, view)?;
    terminal_retention::sync_view_pins(db, &view.panes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{BrowserId, BrowserSession, CardId, CardKind};
    use crate::workspace_model::views::Pane;
    fn setup() -> (Registry, tempfile::TempDir, ChannelId, ChannelId) {
        let dir = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&dir.path().join("model.sqlite3")).unwrap();
        registry
            .apply(&ModelEdit::OpenProject {
                repository: "/repo".into(),
                name: "repo".into(),
                kind: Default::default(),
            })
            .unwrap();
        let a = ChannelId::new();
        let b = ChannelId::new();
        for id in [a, b] {
            registry
                .apply(&ModelEdit::CreateChannel {
                    id,
                    repository: "/repo".into(),
                    title: "view".into(),
                    title_source: Default::default(),
                })
                .unwrap();
        }
        (registry, dir, a, b)
    }
    fn pane() -> Pane {
        Pane {
            id: CardId::new(),
            title: "browser".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: Some("/repo".into()),
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: "https://example.com".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.,
                scroll_y: 0.,
                fidelity: crate::model::BrowserRestoreFidelity::UrlOnly,
            }),
        }
    }
    fn seed(registry: &mut Registry, channel: ChannelId, panes: Vec<Pane>) {
        registry
            .apply(&ModelEdit::SeedView {
                scope: Scope::Channel(channel),
                panes,
                closed: vec![],
            })
            .unwrap();
    }
    fn edit(registry: &mut Registry, channel: ChannelId, change: Change) -> ModelEdit {
        ModelEdit::ChangeView {
            scope: Scope::Channel(channel),
            expected_revision: registry.view_revision(&Scope::Channel(channel)).unwrap(),
            operation: Uuid::new_v4(),
            change,
        }
    }
    #[test]
    fn view_listings_read_each_view_whole_and_occupied_skips_emptied_views() {
        let (mut r, _, a, b) = setup();
        let kept = pane();
        let emptied = pane();
        seed(&mut r, a, vec![kept.clone()]);
        seed(&mut r, b, vec![emptied.clone()]);
        let remove = edit(&mut r, b, Change::Remove { id: emptied.id });
        r.apply(&remove).unwrap();
        let every = all(&r.db).unwrap();
        assert_eq!(every.len(), 2);
        for view in &every {
            assert_eq!(view, &read(&r.db, &view.scope).unwrap());
        }
        assert_eq!(
            occupied(&r.db).unwrap(),
            vec![read(&r.db, &Scope::Channel(a)).unwrap()]
        );
    }
    #[test]
    fn hosted_resource_ownership_reads_only_occupied_views() {
        let (mut r, _, a, b) = setup();
        let emptied = EmptiedView::plant(&mut r);
        let p = pane();
        let mut alias = p.clone();
        alias.id = CardId::new();
        let mut renamed = p.clone();
        renamed.title = "renamed".into();
        // Seeding, inserting and updating each look for the resource's pane,
        // applied here without the whole model an applied edit returns.
        let owner = emptied.unreadable_during(&mut r, |r| {
            let tx = r.db.transaction().unwrap();
            let change = |tx: &rusqlite::Transaction<'_>, channel, change| {
                let scope = Scope::Channel(channel);
                let edit = ModelEdit::ChangeView {
                    expected_revision: read(tx, &scope).unwrap().revision,
                    scope,
                    operation: Uuid::new_v4(),
                    change,
                };
                apply(tx, &edit).unwrap();
            };
            apply(
                &tx,
                &ModelEdit::SeedView {
                    scope: Scope::Channel(a),
                    panes: vec![p.clone()],
                    closed: vec![],
                },
            )
            .unwrap();
            change(
                &tx,
                b,
                Change::Insert {
                    pane: alias,
                    before: None,
                },
            );
            change(
                &tx,
                a,
                Change::Update {
                    before: p.clone(),
                    pane: renamed.clone(),
                },
            );
            let owner = resource_owner(&tx, &pane_with_kind(&p)).unwrap();
            tx.commit().unwrap();
            owner
        });
        assert_eq!(owner, Some(p.id), "the resource's pane was found");
        assert!(
            read(&r.db, &Scope::Channel(b)).unwrap().panes.is_empty(),
            "the resource already has its pane in another view"
        );
        assert_eq!(
            read(&r.db, &Scope::Channel(a)).unwrap().panes,
            vec![renamed]
        );
    }
    fn pane_with_kind(of: &Pane) -> Pane {
        Pane {
            id: CardId::new(),
            ..of.clone()
        }
    }
    #[test]
    fn migration_and_updates_keep_one_pane_per_hosted_resource() {
        let (mut r, _, a, b) = setup();
        let p = pane();
        let mut alias = p.clone();
        alias.id = CardId::new();
        seed(&mut r, a, vec![p.clone(), alias.clone()]);
        assert_eq!(
            read(&r.db, &Scope::Channel(a)).unwrap().panes,
            vec![p.clone()]
        );
        seed(&mut r, b, vec![alias]);
        assert!(read(&r.db, &Scope::Channel(b)).unwrap().panes.is_empty());
        let other = pane();
        seed(&mut r, b, vec![other.clone()]);
        let mut conflicting = other.clone();
        conflicting.kind = p.kind;
        let update = edit(
            &mut r,
            b,
            Change::Update {
                before: other.clone(),
                pane: conflicting,
            },
        );
        r.apply(&update).unwrap();
        assert_eq!(read(&r.db, &Scope::Channel(b)).unwrap().panes, vec![other]);
    }
    #[test]
    fn worktrees_share_ownership_and_do_not_reopen_closed_projects() {
        let (mut r, _, a, _) = setup();
        let scope = Scope::Worktree {
            repository: "/repo".into(),
            path: "/repo/tree".into(),
        };
        let p = pane();
        seed(&mut r, a, vec![p.clone()]);
        let transfer = edit(
            &mut r,
            a,
            Change::Transfer {
                id: p.id,
                destination: scope.clone(),
                before: None,
            },
        );
        r.apply(&transfer).unwrap();
        assert!(read(&r.db, &Scope::Channel(a)).unwrap().panes.is_empty());
        assert_eq!(read(&r.db, &scope).unwrap().panes, vec![p]);
        r.apply(&ModelEdit::CloseProject {
            repository: "/repo".into(),
        })
        .unwrap();
        assert!(r.model().unwrap().views.is_empty());
        r.apply(&ModelEdit::SeedView {
            scope,
            panes: vec![pane()],
            closed: vec![],
        })
        .unwrap();
        assert!(r.model().unwrap().projects.is_empty());
        assert!(r.model().unwrap().views.is_empty());
    }
    #[test]
    fn concurrent_default_creation_accepts_only_one_pane() {
        let (mut r, _, a, _) = setup();
        seed(&mut r, a, vec![]);
        let first = pane();
        let second = pane();
        let winner = edit(
            &mut r,
            a,
            Change::InsertIfEmpty {
                pane: first.clone(),
            },
        );
        let mut loser = edit(&mut r, a, Change::InsertIfEmpty { pane: second });
        r.apply(&winner).unwrap();
        assert!(r.apply(&loser).is_err());
        if let ModelEdit::ChangeView {
            expected_revision, ..
        } = &mut loser
        {
            *expected_revision = r.view_revision(&Scope::Channel(a)).unwrap();
        }
        r.apply(&loser).unwrap();
        assert_eq!(read(&r.db, &Scope::Channel(a)).unwrap().panes, vec![first]);
    }
    #[test]
    fn conflicts_and_durable_replay_do_not_overwrite_concurrent_fields() {
        let (mut r, dir, a, _) = setup();
        let p = pane();
        seed(&mut r, a, vec![p.clone()]);
        let mut width = p.clone();
        width.width_fraction = 0.7;
        let stale = edit(
            &mut r,
            a,
            Change::Update {
                before: p.clone(),
                pane: width,
            },
        );
        let mut title = p.clone();
        title.title = "renamed".into();
        let first = edit(
            &mut r,
            a,
            Change::Update {
                before: p.clone(),
                pane: title,
            },
        );
        r.apply(&first).unwrap();
        assert!(r.apply(&stale).is_err());
        let mut rebased = stale;
        if let ModelEdit::ChangeView {
            expected_revision, ..
        } = &mut rebased
        {
            *expected_revision = r.view_revision(&Scope::Channel(a)).unwrap();
        }
        r.apply(&rebased).unwrap();
        let before = r.model().unwrap();
        let v = &before.views[0];
        assert_eq!(v.panes[0].title, "renamed");
        assert_eq!(v.panes[0].width_fraction, 0.7);
        drop(r);
        let mut r = Registry::at(&dir.path().join("model.sqlite3")).unwrap();
        assert!(!r.apply(&rebased).unwrap().changed);
        assert_eq!(r.model().unwrap(), before);
    }
    fn live_source_pane() -> Pane {
        let terminal = crate::model::TerminalSession {
            id: crate::model::TerminalId::new(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            launch: crate::model::LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: vec![],
                working_directory: "/repo".into(),
                environment: Default::default(),
                unset_environment: Default::default(),
            },
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::Live {
                host_session_id: crate::model::HostSessionId::new("live"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("host"),
            },
        };
        Pane {
            kind: CardKind::Terminal(terminal),
            ..pane()
        }
    }

    #[test]
    fn primary_chat_change_clears_only_an_inferred_channel_title() {
        for manual in [false, true] {
            let (mut registry, _directory, _, _) = setup();
            let channel = ChannelId::new();
            // Default titles are manual/legacy names. This case must own a
            // genuinely inferred channel before testing automatic clearing.
            registry
                .apply(&ModelEdit::CreateChannel {
                    id: channel,
                    repository: "/repo".into(),
                    title: "Initial inferred title".into(),
                    title_source: crate::model::ChannelTitleSource::Prompt,
                })
                .unwrap();
            let mut original = live_source_pane();
            let CardKind::Terminal(terminal) = original.kind else {
                unreachable!()
            };
            original.kind = CardKind::Chat(crate::chat::Session {
                id: terminal.id,
                launch: crate::chat::Launch {
                    provider: crate::agent::Provider::Codex,
                    recipe: terminal.launch.clone(),
                    session_id: Some("first-thread".into()),
                },
                proof: terminal.continuity.live_proof(),
                terminal: None,
                owned_codex_listener: None,
                native_continuity: Default::default(),
            });
            seed(&mut registry, channel, vec![original.clone()]);
            registry
                .apply(&if manual {
                    ModelEdit::RetitleChannel {
                        id: channel,
                        title: "User chosen".into(),
                    }
                } else {
                    ModelEdit::SuggestChannelTitle {
                        id: channel,
                        title: "Provider chosen".into(),
                        source: crate::model::ChannelTitleSource::Provider,
                    }
                })
                .unwrap();
            assert_eq!(
                registry.model().unwrap().channel(channel).unwrap().title,
                if manual {
                    "User chosen"
                } else {
                    "Provider chosen"
                },
                "establish the naming source before replacing the primary thread"
            );
            let mut successor = original.clone();
            let CardKind::Chat(chat) = &mut successor.kind else {
                unreachable!()
            };
            chat.launch.session_id = Some("replacement-thread".into());
            let update = edit(
                &mut registry,
                channel,
                Change::Update {
                    before: original,
                    pane: successor,
                },
            );
            registry.apply(&update).unwrap();
            let model = registry.model().unwrap();
            let canonical = model.channel(channel).unwrap();
            assert_eq!(canonical.title, if manual { "User chosen" } else { "" });
        }
    }

    #[test]
    fn source_update_retries_refuse_concurrent_sources_without_acknowledging_partial_intent() {
        for advance in 0..4 {
            let (mut registry, directory, channel, _) = setup();
            let original = live_source_pane();
            seed(&mut registry, channel, vec![original.clone()]);
            let mut requested = original.clone();
            if let CardKind::Terminal(terminal) = &mut requested.kind {
                terminal.resume_hint = Some("old source update".into());
            }
            requested.title = "independent title".into();
            requested.width_fraction = 0.7;
            let mut delayed = edit(
                &mut registry,
                channel,
                Change::Update {
                    before: original.clone(),
                    pane: requested,
                },
            );
            let operation = match &delayed {
                ModelEdit::ChangeView { operation, .. } => *operation,
                _ => unreachable!(),
            };
            // Simulate a writer committing after the client read/preflight.
            let mut writer = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
            let mut current = original.clone();
            match advance {
                0 => {
                    if let CardKind::Terminal(terminal) = &mut current.kind {
                        terminal.id = crate::model::TerminalId::new();
                    }
                }
                1 => {
                    if let CardKind::Terminal(terminal) = &mut current.kind {
                        terminal.transcript_path = Some("/repo/new-transcript".into());
                    }
                }
                2 => current.pending_terminal_start = true,
                3 => current.scope = Some("/repo/new-location".into()),
                _ => unreachable!(),
            }
            let advance = edit(
                &mut writer,
                channel,
                Change::Update {
                    before: original.clone(),
                    pane: current.clone(),
                },
            );
            writer.apply(&advance).unwrap();
            // An otherwise valid revision refresh cannot authorize stale source intent.
            if let ModelEdit::ChangeView {
                expected_revision, ..
            } = &mut delayed
            {
                *expected_revision = registry.view_revision(&Scope::Channel(channel)).unwrap();
            }
            let before = registry.model().unwrap();
            assert_eq!(
                registry.apply(&delayed).unwrap_err(),
                Registry::VIEW_SOURCE_CONFLICT
            );
            assert_eq!(registry.model().unwrap(), before);
            assert!(!registry.view_operation_seen(operation).unwrap());
            drop(registry);
            let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
            assert!(!registry.view_operation_seen(operation).unwrap());
            // Presentation is a distinct operation; refusal did not silently commit it.
            let mut presentation = original.clone();
            presentation.title = "independent title".into();
            presentation.width_fraction = 0.7;
            let presentation = edit(
                &mut registry,
                channel,
                Change::Update {
                    before: original,
                    pane: presentation,
                },
            );
            registry.apply(&presentation).unwrap();
            current.title = "independent title".into();
            current.width_fraction = 0.7;
            assert_eq!(
                read(&registry.db, &Scope::Channel(channel)).unwrap().panes,
                vec![current]
            );
        }
    }

    #[test]
    fn source_update_fence_preserves_new_confirmed_chat_thread() {
        let (mut registry, _, channel, _) = setup();
        let mut original = live_source_pane();
        let CardKind::Terminal(terminal) = original.kind else {
            unreachable!()
        };
        original.kind = CardKind::Chat(crate::chat::Session {
            id: terminal.id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: terminal.launch.clone(),
                session_id: Some("thread-one".into()),
            },
            proof: terminal.continuity.live_proof(),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        });
        seed(&mut registry, channel, vec![original.clone()]);
        let mut delayed = original.clone();
        if let CardKind::Chat(chat) = &mut delayed.kind {
            chat.terminal = Some(terminal);
        }
        let mut current = original.clone();
        if let CardKind::Chat(chat) = &mut current.kind {
            chat.launch.session_id = Some("thread-two".into());
        }
        let confirmed = edit(
            &mut registry,
            channel,
            Change::Update {
                before: original.clone(),
                pane: current.clone(),
            },
        );
        registry.apply(&confirmed).unwrap();
        let delayed = edit(
            &mut registry,
            channel,
            Change::Update {
                before: original,
                pane: delayed,
            },
        );
        let before = registry.model().unwrap();
        assert_eq!(
            registry.apply(&delayed).unwrap_err(),
            Registry::VIEW_SOURCE_CONFLICT
        );
        assert_eq!(registry.model().unwrap(), before);
        assert_eq!(
            read(&registry.db, &Scope::Channel(channel)).unwrap().panes,
            vec![current]
        );
    }

    #[test]
    fn stale_pending_or_location_only_intent_cannot_modify_a_successor() {
        for pending in [true, false] {
            let (mut registry, _, channel, _) = setup();
            let original = live_source_pane();
            seed(&mut registry, channel, vec![original.clone()]);
            let mut successor = original.clone();
            if let CardKind::Terminal(terminal) = &mut successor.kind {
                terminal.id = crate::model::TerminalId::new();
            }
            let replace = edit(
                &mut registry,
                channel,
                Change::Update {
                    before: original.clone(),
                    pane: successor,
                },
            );
            registry.apply(&replace).unwrap();
            let mut stale = original.clone();
            if pending {
                stale.pending_terminal_start = true;
            } else {
                stale.scope = Some("/repo/old-location".into());
            }
            let stale = edit(
                &mut registry,
                channel,
                Change::Update {
                    before: original,
                    pane: stale,
                },
            );
            let before = registry.model().unwrap();
            assert_eq!(
                registry.apply(&stale).unwrap_err(),
                Registry::VIEW_SOURCE_CONFLICT
            );
            assert_eq!(registry.model().unwrap(), before);
        }
    }

    #[test]
    fn a_source_update_whose_result_already_holds_applies_its_presentation() {
        let (mut registry, directory, channel, _) = setup();
        let original = live_source_pane();
        seed(&mut registry, channel, vec![original.clone()]);
        let mut attached = original.clone();
        if let CardKind::Terminal(terminal) = &mut attached.kind {
            terminal.continuity = crate::model::TerminalContinuity::Live {
                host_session_id: crate::model::HostSessionId::new("replacement"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("replacement"),
            };
        }
        // Each window read the old record, and each reports the same attachment.
        let mut peer = edit(
            &mut registry,
            channel,
            Change::Update {
                before: original.clone(),
                pane: attached.clone(),
            },
        );
        let mut writer = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        let first = edit(
            &mut writer,
            channel,
            Change::Update {
                before: original.clone(),
                pane: attached.clone(),
            },
        );
        writer.apply(&first).unwrap();
        let mut titled = attached.clone();
        titled.title = "independent title".into();
        if let ModelEdit::ChangeView {
            expected_revision,
            change,
            ..
        } = &mut peer
        {
            *expected_revision = registry.view_revision(&Scope::Channel(channel)).unwrap();
            *change = Change::Update {
                before: original,
                pane: titled.clone(),
            };
        }
        registry.apply(&peer).unwrap();
        assert_eq!(
            read(&registry.db, &Scope::Channel(channel)).unwrap().panes,
            vec![titled]
        );
    }

    /// A pane under `live_source_pane`'s terminal, attached under another proof.
    fn attached_under(original: &Pane, host: &str) -> Pane {
        let mut attached = original.clone();
        if let CardKind::Terminal(terminal) = &mut attached.kind {
            terminal.continuity = crate::model::TerminalContinuity::Live {
                host_session_id: crate::model::HostSessionId::new(host),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new(host),
            };
        }
        attached
    }

    /// Commits `advance` through another writer, then applies a stale client's
    /// update from `original` to `requested`, read before that commit.
    fn stale_update_after(
        original: Pane,
        requested: Pane,
        advance: impl FnOnce(&mut Registry, ChannelId, &Pane),
    ) -> (Result<(), String>, Vec<Pane>, Vec<crate::model::CardId>) {
        let (mut registry, directory, channel, _) = setup();
        seed(&mut registry, channel, vec![original.clone()]);
        let mut stale = edit(
            &mut registry,
            channel,
            Change::Update {
                before: original.clone(),
                pane: requested,
            },
        );
        let mut writer = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        advance(&mut writer, channel, &original);
        if let ModelEdit::ChangeView {
            expected_revision, ..
        } = &mut stale
        {
            *expected_revision = registry.view_revision(&Scope::Channel(channel)).unwrap();
        }
        let result = registry.apply(&stale).map(|_| ());
        let view = read(&registry.db, &Scope::Channel(channel)).unwrap();
        (result, view.panes, view.retired)
    }

    #[test]
    fn a_source_update_conflicts_unless_all_of_its_source_or_result_holds() {
        let before = live_source_pane();
        let result = attached_under(&before, "result");
        let view = |current: Pane| crate::workspace_model::views::View {
            panes: vec![current],
            ..crate::workspace_model::views::View::empty(Scope::Channel(ChannelId::new()))
        };
        let conflicts = |current: Pane| view(current).source_update_conflicts(&before, &result);
        assert!(!conflicts(before.clone()));
        assert!(!conflicts(result.clone()));
        assert!(conflicts(attached_under(&before, "another")));
        for held in [&before, &result] {
            let mut started = held.clone();
            started.pending_terminal_start = !started.pending_terminal_start;
            assert!(conflicts(started));
            let mut moved = held.clone();
            moved.scope = Some("/elsewhere".into());
            assert!(conflicts(moved));
        }
        // Presentation alone never conflicts, whatever the record says now.
        let mut titled = before.clone();
        titled.title = "independent title".into();
        assert!(
            !view(attached_under(&before, "another")).source_update_conflicts(&before, &titled)
        );
    }

    #[test]
    fn a_stale_client_cannot_overwrite_a_newer_attachment() {
        let original = live_source_pane();
        let requested = attached_under(&original, "older");
        let newer = attached_under(&original, "newer");
        let committed = newer.clone();
        let (result, panes, _) =
            stale_update_after(original, requested, |writer, channel, base| {
                let change = edit(
                    writer,
                    channel,
                    Change::Update {
                        before: base.clone(),
                        pane: committed,
                    },
                );
                writer.apply(&change).unwrap();
            });
        assert_eq!(result.unwrap_err(), Registry::VIEW_SOURCE_CONFLICT);
        assert_eq!(panes, vec![newer]);
    }

    #[test]
    fn a_stale_client_cannot_revive_an_ended_terminal() {
        let original = live_source_pane();
        let requested = attached_under(&original, "attached");
        // The terminal reached this proof and then ended, so the requested
        // record is one the registry has already moved past.
        let attached = requested.clone();
        let mut ended = requested.clone();
        if let CardKind::Terminal(terminal) = &mut ended.kind {
            terminal.continuity = crate::model::TerminalContinuity::Exited {
                last_exit: crate::model::TerminalExit {
                    code: Some(0),
                    signal: None,
                    occurred_at_unix_ms: 1,
                },
            };
        }
        let committed = ended.clone();
        let (result, panes, _) =
            stale_update_after(original, requested, |writer, channel, base| {
                for (before, pane) in [(base.clone(), attached.clone()), (attached, committed)] {
                    let change = edit(writer, channel, Change::Update { before, pane });
                    writer.apply(&change).unwrap();
                }
            });
        assert_eq!(result.unwrap_err(), Registry::VIEW_SOURCE_CONFLICT);
        assert_eq!(panes, vec![ended]);
    }

    #[test]
    fn a_stale_client_cannot_bring_back_a_retired_terminal() {
        let original = live_source_pane();
        let id = original.id;
        let requested = attached_under(&original, "attached");
        let (result, panes, retired) =
            stale_update_after(original, requested, |writer, channel, _| {
                let change = edit(writer, channel, Change::Remove { id });
                writer.apply(&change).unwrap();
            });
        assert!(result.is_err(), "a retired pane accepted an update");
        assert!(panes.is_empty(), "the retired pane came back: {panes:?}");
        assert!(retired.contains(&id));
    }

    #[test]
    fn exact_source_basis_allows_lifecycle_updates_and_durable_replay() {
        let (mut registry, directory, channel, _) = setup();
        let original = live_source_pane();
        seed(&mut registry, channel, vec![original.clone()]);
        let mut ended = original.clone();
        if let CardKind::Terminal(terminal) = &mut ended.kind {
            terminal.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
                recipe: terminal.launch.clone(),
                last_exit: None,
            };
        }
        let change = edit(
            &mut registry,
            channel,
            Change::Update {
                before: original,
                pane: ended.clone(),
            },
        );
        registry.apply(&change).unwrap();
        assert_eq!(
            read(&registry.db, &Scope::Channel(channel)).unwrap().panes,
            vec![ended]
        );
        let committed = registry.model().unwrap();
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert!(!registry.apply(&change).unwrap().changed);
        assert_eq!(registry.model().unwrap(), committed);
    }

    #[test]
    fn stale_seed_cannot_resurrect_a_removed_pane() {
        let (mut r, _, a, _) = setup();
        let p = pane();
        seed(&mut r, a, vec![p.clone()]);
        let removal = edit(&mut r, a, Change::Remove { id: p.id });
        r.apply(&removal).unwrap();
        seed(&mut r, a, vec![p]);
        assert!(read(&r.db, &Scope::Channel(a)).unwrap().panes.is_empty());
    }
    #[test]
    fn transfer_is_atomic_unique_and_reversible() {
        let (mut r, _, a, b) = setup();
        let p = pane();
        seed(&mut r, a, vec![p.clone()]);
        seed(&mut r, b, vec![p.clone()]);
        assert!(read(&r.db, &Scope::Channel(b)).unwrap().panes.is_empty());
        let duplicate = edit(
            &mut r,
            b,
            Change::Insert {
                pane: p.clone(),
                before: None,
            },
        );
        assert!(r.apply(&duplicate).is_err());
        for (source, destination) in [(a, b), (b, a)] {
            let transfer = edit(
                &mut r,
                source,
                Change::Transfer {
                    id: p.id,
                    destination: Scope::Channel(destination),
                    before: None,
                },
            );
            r.apply(&transfer).unwrap();
            assert!(
                read(&r.db, &Scope::Channel(source))
                    .unwrap()
                    .panes
                    .is_empty()
            );
            assert_eq!(
                read(&r.db, &Scope::Channel(destination)).unwrap().panes,
                vec![p.clone()]
            );
            assert_eq!(
                owner(&r.db, p.id).unwrap(),
                Some(Scope::Channel(destination))
            );
        }
    }

    #[test]
    fn transfer_rejects_missing_targets_and_foreign_projects_without_any_mutation() {
        let (mut r, _, a, b) = setup();
        let p = pane();
        seed(&mut r, a, vec![p.clone()]);
        let foreign = ChannelId::new();
        r.apply(&ModelEdit::OpenProject {
            repository: "/other".into(),
            name: "other".into(),
            kind: Default::default(),
        })
        .unwrap();
        r.apply(&ModelEdit::CreateChannel {
            id: foreign,
            repository: "/other".into(),
            title: "foreign".into(),
            title_source: Default::default(),
        })
        .unwrap();
        for (source, id, destination) in [
            (a, p.id, Scope::Channel(ChannelId::new())),
            (a, p.id, Scope::Channel(foreign)),
            (a, CardId::new(), Scope::Channel(b)),
            (a, CardId::new(), Scope::Channel(a)),
            (ChannelId::new(), p.id, Scope::Channel(b)),
            (
                a,
                p.id,
                Scope::Worktree {
                    repository: "/other".into(),
                    path: "/other/tree".into(),
                },
            ),
        ] {
            let before = r.model().unwrap();
            let transfer = edit(
                &mut r,
                source,
                Change::Transfer {
                    id,
                    destination,
                    before: None,
                },
            );
            assert!(
                r.apply(&transfer).is_err(),
                "invalid transfer must report failure"
            );
            assert_eq!(
                r.model().unwrap(),
                before,
                "failed transfer must roll back all views and metadata"
            );
        }
        let same = edit(
            &mut r,
            a,
            Change::Transfer {
                id: p.id,
                destination: Scope::Channel(a),
                before: None,
            },
        );
        let before = r.model().unwrap();
        assert!(!r.apply(&same).unwrap().changed);
        assert_eq!(r.model().unwrap(), before);
    }

    #[test]
    fn transfer_fences_stale_edits_and_replays_without_moving_a_pane_twice() {
        let (mut r, _, a, b) = setup();
        let p = pane();
        seed(&mut r, a, vec![p.clone()]);
        let mut transfer = edit(
            &mut r,
            a,
            Change::Transfer {
                id: p.id,
                destination: Scope::Channel(b),
                before: None,
            },
        );
        let update = edit(
            &mut r,
            a,
            Change::Insert {
                pane: pane(),
                before: None,
            },
        );
        r.apply(&update).unwrap();
        let before = r.model().unwrap();
        assert!(
            r.apply(&transfer)
                .unwrap_err()
                .contains("revision conflict")
        );
        assert_eq!(r.model().unwrap(), before);
        if let ModelEdit::ChangeView {
            expected_revision, ..
        } = &mut transfer
        {
            *expected_revision = r.view_revision(&Scope::Channel(a)).unwrap();
        }
        r.apply(&transfer).unwrap();
        let moved = r.model().unwrap();
        assert!(!r.apply(&transfer).unwrap().changed);
        assert_eq!(r.model().unwrap(), moved);
    }

    #[test]
    fn transfer_rejects_equal_unproved_managed_sessions_without_mutation() {
        use crate::{
            agent::Provider,
            agent_execution::{Destination, HostedSession, Origin, Spec, Submit, Surface, Target},
            model::{BootId, HostIncarnation, HostSessionId, TerminalContinuity, TerminalSession},
        };
        // Persisted legacy/malformed records can compare equal without carrying
        // complete proof. Both preflight and the transaction must refuse them.
        for (native_chat, proof_present) in [(true, false), (true, true), (false, true)] {
            let (mut r, _directory, a, b) = setup();
            let accepted = r
                .submit_agent(
                    &Origin::User,
                    &Submit {
                        name: None,
                        destination: Destination::New(Spec {
                            provider: Provider::Claude,
                            account: None,
                            target: Target {
                                repository: "/repo".into(),
                                directory: "/repo".into(),
                                placement: Some(Scope::Channel(a)),
                            },
                            surface: if native_chat {
                                Surface::Chat
                            } else {
                                Surface::Terminal
                            },
                            model: None,
                            permission_mode: None,
                            reasoning_effort: None,
                        }),
                        prompt: "Work".into(),
                        idempotency_key: "unproved-transfer".into(),
                    },
                    1,
                )
                .unwrap();
            let terminal = TerminalSession {
                id: crate::model::TerminalId::new(),
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                launch: crate::model::LaunchRecipe {
                    program: "/bin/sh".into(),
                    arguments: vec![],
                    working_directory: "/repo".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                transcript_path: None,
                resume_hint: None,
                continuity: TerminalContinuity::Live {
                    host_session_id: HostSessionId::new(""),
                    boot_id: BootId::new(""),
                    host_incarnation: HostIncarnation::new(""),
                },
            };
            let source = if !native_chat {
                HostedSession::Terminal(terminal)
            } else {
                HostedSession::Chat(crate::chat::Session {
                    id: terminal.id,
                    launch: crate::chat::Launch {
                        provider: Provider::Claude,
                        recipe: terminal.launch.clone(),
                        session_id: None,
                    },
                    proof: if proof_present {
                        terminal.continuity.live_proof()
                    } else {
                        None
                    },
                    terminal: None,
                    owned_codex_listener: None,
                    native_continuity: Default::default(),
                })
            };
            r.bind_agent(accepted.agent.id, None, source.clone())
                .unwrap();
            let p = Pane {
                kind: match source {
                    HostedSession::Chat(session) => CardKind::Chat(session),
                    HostedSession::Terminal(session) => CardKind::Terminal(session),
                },
                ..pane()
            };
            seed(&mut r, a, vec![p.clone()]);
            let change = Change::Transfer {
                id: p.id,
                destination: Scope::Channel(b),
                before: None,
            };
            let transfer = edit(&mut r, a, change.clone());
            let before = r.model().unwrap();
            assert!(
                r.view_change_refusal(&Scope::Channel(a), &change)
                    .unwrap()
                    .is_some()
            );
            assert!(r.apply(&transfer).unwrap_err().contains("changed process"));
            assert_eq!(
                r.model().unwrap(),
                before,
                "equal unproved descriptors preserve both views, Agent and model sequence"
            );
        }
    }

    #[test]
    fn authenticated_adoption_refreshes_canonical_panes_and_stale_transfer_still_refuses() {
        use crate::{
            agent::Provider,
            agent_execution::{Destination, HostedSession, Origin, Spec, Submit, Surface, Target},
        };
        for (native_chat, console, changed_launch, evidence) in [
            (false, false, false, "exact"),
            (false, false, true, "exact"),
            (true, false, false, "exact"),
            (true, false, true, "exact"),
            (true, true, false, "exact"),
            (true, false, false, "thread"),
            (true, false, false, "recipe"),
            (true, false, false, "proof"),
            (true, false, false, "unproved"),
            (true, false, false, "alias"),
            (true, false, false, "absent"),
            (true, false, false, "retired"),
            (true, false, false, "changed-thread"),
            (true, false, false, "learn-thread"),
        ] {
            let (mut r, _directory, a, b) = setup();
            let accepted = r
                .submit_agent(
                    &Origin::User,
                    &Submit {
                        name: None,
                        destination: Destination::New(Spec {
                            provider: Provider::Claude,
                            account: None,
                            target: Target {
                                repository: "/repo".into(),
                                directory: "/repo".into(),
                                placement: Some(Scope::Channel(a)),
                            },
                            surface: if native_chat {
                                Surface::Chat
                            } else {
                                Surface::Terminal
                            },
                            model: None,
                            permission_mode: None,
                            reasoning_effort: None,
                        }),
                        prompt: "Work".into(),
                        idempotency_key: "successor-transfer".into(),
                    },
                    1,
                )
                .unwrap();
            let terminal = crate::model::TerminalSession {
                id: crate::model::TerminalId::new(),
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                launch: crate::model::LaunchRecipe {
                    program: "/bin/sh".into(),
                    arguments: vec![],
                    working_directory: "/repo".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                transcript_path: None,
                resume_hint: None,
                continuity: crate::model::TerminalContinuity::Live {
                    host_session_id: crate::model::HostSessionId::new("live"),
                    boot_id: crate::model::BootId::new("boot"),
                    host_incarnation: crate::model::HostIncarnation::new("host"),
                },
            };
            let mut source = if native_chat {
                HostedSession::Chat(crate::chat::Session {
                    id: terminal.id,
                    launch: crate::chat::Launch {
                        provider: Provider::Claude,
                        recipe: terminal.launch.clone(),
                        session_id: Some(uuid::Uuid::new_v4().to_string()),
                    },
                    proof: terminal.continuity.live_proof(),
                    terminal: Some(terminal.clone()),
                    owned_codex_listener: None,
                    native_continuity: Default::default(),
                })
            } else {
                HostedSession::Terminal(terminal.clone())
            };
            if evidence == "learn-thread"
                && let HostedSession::Chat(session) = &mut source
            {
                session.launch.session_id = None;
            }
            let original = r
                .bind_agent(accepted.agent.id, None, source.clone())
                .unwrap();
            if console {
                r.bind_agent_console(original.id, &source, terminal.clone())
                    .unwrap();
            }
            let presentation = |session: &HostedSession| match session {
                HostedSession::Chat(session) if !console => CardKind::Chat(session.clone()),
                HostedSession::Chat(session) => {
                    CardKind::Terminal(session.terminal.clone().unwrap())
                }
                HostedSession::Terminal(session) => CardKind::Terminal(session.clone()),
            };
            let mut p = Pane {
                id: CardId::from_uuid(original.id.as_uuid()),
                kind: presentation(&source),
                user_title: Some("My retained actor".into()),
                width_fraction: 0.75,
                ..pane()
            };
            if let CardKind::Chat(session) = &mut p.kind {
                match evidence {
                    "thread" => session.launch.session_id = Some(uuid::Uuid::new_v4().to_string()),
                    "recipe" => session.launch.recipe.arguments.push("unrelated".into()),
                    "proof" => {
                        session.proof.as_mut().unwrap().host_incarnation =
                            crate::model::HostIncarnation::new("unrelated")
                    }
                    "unproved" => session.proof = None,
                    _ => {}
                }
            }
            if evidence == "alias" {
                p.id = CardId::new();
            }
            seed(
                &mut r,
                a,
                if evidence == "absent" {
                    vec![]
                } else {
                    vec![p.clone()]
                },
            );
            if evidence == "retired" {
                let removal = edit(&mut r, a, Change::Remove { id: p.id });
                r.apply(&removal).unwrap();
            }
            r.apply(&ModelEdit::SuggestChannelTitle {
                id: a,
                title: "Retained conversation".into(),
                source: crate::model::ChannelTitleSource::Provider,
            })
            .unwrap();
            let before_adoption = r.model().unwrap();
            let before_view = read(&r.db, &Scope::Channel(a)).unwrap();
            let mut successor = source.clone();
            match &mut successor {
                HostedSession::Chat(session) => {
                    if matches!(evidence, "changed-thread" | "learn-thread") {
                        session.launch.session_id = Some(uuid::Uuid::new_v4().to_string());
                    }
                    if changed_launch {
                        session.launch.recipe.arguments.push("successor".into());
                    } else {
                        session.proof.as_mut().unwrap().host_incarnation =
                            crate::model::HostIncarnation::new("successor");
                        session.terminal.as_mut().unwrap().continuity =
                            crate::model::TerminalContinuity::Live {
                                host_session_id: crate::model::HostSessionId::new("live"),
                                boot_id: crate::model::BootId::new("boot"),
                                host_incarnation: crate::model::HostIncarnation::new("successor"),
                            };
                    }
                }
                HostedSession::Terminal(session) => {
                    if changed_launch {
                        session.launch.arguments.push("successor".into());
                    } else {
                        session.continuity = crate::model::TerminalContinuity::Live {
                            host_session_id: crate::model::HostSessionId::new("live"),
                            boot_id: crate::model::BootId::new("boot"),
                            host_incarnation: crate::model::HostIncarnation::new("successor"),
                        };
                    }
                }
            }
            if !changed_launch && matches!(evidence, "exact" | "learn-thread") {
                // A failed pane write must roll back Agent ownership as well.
                r.db.execute_batch("CREATE TRIGGER reject_pane_adoption BEFORE UPDATE ON view_scopes BEGIN SELECT RAISE(ABORT, 'fixture failed pane commit'); END;").unwrap();
                assert!(
                    r.adopt_authenticated_agent_session(std::slice::from_ref(&source), &successor)
                        .is_err()
                );
                assert_eq!(r.model().unwrap(), before_adoption);
                r.db.execute_batch("DROP TRIGGER reject_pane_adoption;")
                    .unwrap();
                // Here the pane write succeeds first; a later ownership write
                // must roll it back together with the model revision/sequence.
                r.db.execute_batch("CREATE TRIGGER reject_agent_adoption BEFORE UPDATE ON agents BEGIN SELECT RAISE(ABORT, 'fixture failed agent commit'); END;").unwrap();
                assert!(
                    r.adopt_authenticated_agent_session(std::slice::from_ref(&source), &successor)
                        .is_err()
                );
                assert_eq!(r.model().unwrap(), before_adoption);
                r.db.execute_batch("DROP TRIGGER reject_agent_adoption;")
                    .unwrap();
            }
            assert!(
                r.adopt_authenticated_agent_session(std::slice::from_ref(&source), &successor)
                    .unwrap()
            );
            let refreshed = Pane {
                kind: presentation(&successor),
                ..p.clone()
            };
            let current = r.model().unwrap();
            let current_agent = r.agent(original.id).unwrap();
            let current_view = read(&r.db, &Scope::Channel(a)).unwrap();
            assert_eq!(current.sequence, before_adoption.sequence + 1);
            if !matches!(evidence, "exact" | "learn-thread") {
                assert_eq!(
                    current_view, before_view,
                    "{evidence} must not authorize canonical pane recovery"
                );
                continue;
            }
            if !changed_launch || !native_chat {
                assert_eq!(current_view.panes, vec![refreshed.clone()]);
                assert_eq!(current_view.revision, before_view.revision + 1);
                let display = crate::workspace_model::views::ManagedDisplayIndex::new(&current);
                assert!(display.permits_projection(&current_agent, &refreshed));
                assert!(display.resolve(&current_agent).is_some());
                let mut ambiguous = current.clone();
                ambiguous.views.push(current_view.clone());
                assert!(
                    crate::workspace_model::views::ManagedDisplayIndex::new(&ambiguous)
                        .resolve(&current_agent)
                        .is_none()
                );
                assert_eq!(current.channels, before_adoption.channels);
                // Recreate a genuinely stale source after the atomic adoption.
                // This simulates replacement/corrupt old evidence, rather than
                // requiring normal holder recovery to publish an inert pane.
                store::overwrite_panes(&r.db, &Scope::Channel(a), std::slice::from_ref(&p));
                let stale_sequence = r.model().unwrap().sequence;
                assert!(
                    r.adopt_authenticated_agent_session(std::slice::from_ref(&source), &successor)
                        .unwrap()
                );
                assert_eq!(
                    read(&r.db, &Scope::Channel(a)).unwrap().panes,
                    vec![refreshed.clone()]
                );
                assert_eq!(r.model().unwrap().sequence, stale_sequence + 1);
                assert!(
                    !r.adopt_authenticated_agent_session(std::slice::from_ref(&source), &successor)
                        .unwrap()
                );
                store::overwrite_panes(&r.db, &Scope::Channel(a), std::slice::from_ref(&p));
            } else {
                assert_eq!(
                    current_view, before_view,
                    "changed launch is not pane continuity"
                );
            }
            let change = Change::Transfer {
                id: p.id,
                destination: Scope::Channel(b),
                before: None,
            };
            let transfer = edit(&mut r, a, change.clone());
            let before = r.model().unwrap();
            assert!(
                r.view_change_refusal(&Scope::Channel(a), &change)
                    .unwrap()
                    .unwrap()
                    .contains("changed process")
            );
            assert!(r.apply(&transfer).unwrap_err().contains("changed process"));
            assert_eq!(
                r.model().unwrap(),
                before,
                "stale proof/launch must preserve both views, agent and sequence"
            );
            let refresh = edit(
                &mut r,
                a,
                Change::Update {
                    before: p,
                    pane: refreshed.clone(),
                },
            );
            assert!(r.apply(&refresh).unwrap().changed);
            let transfer = edit(&mut r, a, change);
            assert!(r.apply(&transfer).unwrap().changed);
            let moved = r.agent(original.id).unwrap();
            assert_eq!(moved.placement, Scope::Channel(b));
            assert_eq!(moved.session, Some(successor));
            assert_eq!(moved.spec, original.spec);
            assert_eq!(
                read(&r.db, &Scope::Channel(b)).unwrap().panes,
                vec![refreshed]
            );
        }
    }

    #[test]
    fn transfer_commits_agent_placement_with_pane_ownership_and_preserves_the_live_session() {
        use crate::{
            agent::Provider,
            agent_execution::{
                Destination, HostedSession, Origin, Spec, State, Submit, Surface, Target, Update,
            },
        };
        for native_chat in [false, true] {
            let (mut r, dir, a, b) = setup();
            let request = Submit {
                name: Some("worker".into()),
                destination: Destination::New(Spec {
                    provider: Provider::Claude,
                    account: None,
                    target: Target {
                        repository: "/repo".into(),
                        directory: "/repo".into(),
                        placement: Some(Scope::Channel(a)),
                    },
                    surface: if native_chat {
                        Surface::Chat
                    } else {
                        Surface::Terminal
                    },
                    model: None,
                    permission_mode: None,
                    reasoning_effort: None,
                }),
                prompt: "Keep working".into(),
                idempotency_key: "transfer-agent".into(),
            };
            let accepted = r.submit_agent(&Origin::User, &request, 1).unwrap();
            let terminal = crate::model::TerminalSession {
                id: crate::model::TerminalId::new(),
                owned_codex_listener: None,
                recovery_origin: None,
                predecessor: None,
                launch: crate::model::LaunchRecipe {
                    program: "/bin/sh".into(),
                    arguments: vec![],
                    working_directory: "/repo".into(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                transcript_path: None,
                resume_hint: None,
                continuity: crate::model::TerminalContinuity::Live {
                    host_session_id: crate::model::HostSessionId::new("live"),
                    boot_id: crate::model::BootId::new("boot"),
                    host_incarnation: crate::model::HostIncarnation::new("host"),
                },
            };
            let live = if native_chat {
                HostedSession::Chat(crate::chat::Session {
                    id: terminal.id,
                    launch: crate::chat::Launch {
                        provider: Provider::Claude,
                        recipe: terminal.launch.clone(),
                        session_id: Some(uuid::Uuid::new_v4().to_string()),
                    },
                    proof: terminal.continuity.live_proof(),
                    terminal: Some(terminal),
                    owned_codex_listener: None,
                    native_continuity: Default::default(),
                })
            } else {
                HostedSession::Terminal(terminal)
            };
            let original = r.bind_agent(accepted.agent.id, None, live.clone()).unwrap();
            let console = if let HostedSession::Chat(session) = &live {
                let terminal = session.terminal.clone().unwrap();
                r.bind_agent_console(original.id, &live, terminal.clone())
                    .unwrap();
                Some(Pane {
                    kind: CardKind::Terminal(terminal),
                    ..pane()
                })
            } else {
                None
            };
            let started = r
                .update_agent_run(
                    accepted.run.id,
                    accepted.run.revision,
                    Update::Started {
                        provider_key: "initial-turn".into(),
                    },
                    2,
                )
                .unwrap();
            r.update_agent_run(
                started.id,
                started.revision,
                Update::Finished {
                    state: State::Succeeded,
                    result: Default::default(),
                },
                3,
            )
            .unwrap();
            let followup = r
                .submit_agent(
                    &Origin::User,
                    &Submit {
                        name: None,
                        destination: Destination::Existing(original.id),
                        prompt: "Continue working".into(),
                        idempotency_key: "followup".into(),
                    },
                    4,
                )
                .unwrap();
            let kind = match &live {
                HostedSession::Chat(session) => CardKind::Chat(session.clone()),
                HostedSession::Terminal(session) => CardKind::Terminal(session.clone()),
            };
            let p = Pane {
                id: CardId::from_uuid(original.id.as_uuid()),
                kind,
                ..pane()
            };
            let mut panes = vec![p.clone()];
            panes.extend(console.clone());
            seed(&mut r, a, panes);
            if let Some(console) = &console {
                let before = r.agent(original.id).unwrap();
                let transfer = edit(
                    &mut r,
                    a,
                    Change::Transfer {
                        id: console.id,
                        destination: Scope::Channel(b),
                        before: None,
                    },
                );
                assert!(r.apply(&transfer).unwrap().changed);
                assert_eq!(
                    r.agent(original.id).unwrap(),
                    before,
                    "moving a separate proved console must preserve the canonical agent placement and execution"
                );
                assert_eq!(owner(&r.db, p.id).unwrap(), Some(Scope::Channel(a)));
                assert_eq!(owner(&r.db, console.id).unwrap(), Some(Scope::Channel(b)));
            }
            let transfer = edit(
                &mut r,
                a,
                Change::Transfer {
                    id: p.id,
                    destination: Scope::Channel(b),
                    before: None,
                },
            );
            let before = r.model().unwrap();
            r.db.execute_batch("CREATE TEMP TRIGGER fail_agent_move BEFORE UPDATE OF record ON agents BEGIN SELECT RAISE(ABORT, 'fixture agent placement failure'); END;").unwrap();
            assert!(
                r.apply(&transfer)
                    .unwrap_err()
                    .contains("fixture agent placement failure")
            );
            assert_eq!(
                r.model().unwrap(),
                before,
                "agent failure must roll back target, source, operation and sequence"
            );
            r.db.execute_batch("DROP TRIGGER fail_agent_move;").unwrap();
            r.apply(&transfer).unwrap();
            drop(r);
            let mut r = Registry::at(&dir.path().join("model.sqlite3")).unwrap();
            let model = r.model().unwrap();
            let agent = model
                .agents
                .iter()
                .find(|agent| agent.id == original.id)
                .unwrap();
            assert_eq!(agent.placement, Scope::Channel(b));
            assert_eq!(agent.session, Some(live));
            assert_eq!(
                agent.spec, original.spec,
                "move must not rewrite the accepted launch request"
            );
            assert_eq!(owner(&r.db, p.id).unwrap(), Some(Scope::Channel(b)));
            assert_eq!(
                read(&r.db, &Scope::Channel(b)).unwrap().panes,
                console
                    .into_iter()
                    .chain(std::iter::once(p.clone()))
                    .collect::<Vec<_>>()
            );
            assert!(read(&r.db, &Scope::Channel(a)).unwrap().panes.is_empty());
            assert!(!r.apply(&transfer).unwrap().changed);
            r.validate_agent_dispatch(&followup).unwrap();
            assert!(r.claim_agent_dispatch(&followup, 5).unwrap());
            assert!(!r.claim_agent_dispatch(&followup, 6).unwrap());
            let back = edit(
                &mut r,
                b,
                Change::Transfer {
                    id: p.id,
                    destination: Scope::Channel(a),
                    before: None,
                },
            );
            r.apply(&back).unwrap();
            r.validate_agent_dispatch(&followup).unwrap();
            let mut replaced = followup.clone();
            replaced.agent.session = None;
            assert!(
                r.validate_agent_dispatch(&replaced).is_err(),
                "only placement may differ from the accepted execution"
            );
            assert!(r.claim_agent_dispatch(&replaced, 7).is_err());
            let replay = r.submit_agent(&Origin::User, &request, 8).unwrap();
            assert!(!replay.created);
            assert_eq!(replay.run.id, accepted.run.id);
            assert_eq!(replay.agent.placement, Scope::Channel(a));
        }
    }

    #[test]
    fn an_empty_channel_retires_only_as_the_incarnation_and_view_revision_seen() {
        use crate::workspace_model::ChannelIncarnation;
        let (mut registry, _directory, a, b) = setup();
        let model = registry.model().unwrap();
        let home = |channel| ChannelIncarnation {
            registry: model.identity,
            channel,
            creation_ordinal: model.channel(channel).unwrap().ordinal,
        };
        let retire = |home, expected_view_revision| ModelEdit::RetireEmptyChannel {
            home,
            expected_view_revision,
        };
        let refusal = |registry: &mut Registry, edit: &ModelEdit| registry.apply(edit).err();
        let elsewhere = ChannelIncarnation {
            registry: Uuid::new_v4(),
            ..home(a)
        };
        assert_eq!(
            refusal(&mut registry, &retire(elsewhere, 0)).as_deref(),
            Some("The channel belongs to another registry")
        );
        let reborn = ChannelIncarnation {
            creation_ordinal: home(a).creation_ordinal + 100,
            ..home(a)
        };
        assert_eq!(
            refusal(&mut registry, &retire(reborn, 0)).as_deref(),
            Some("The channel incarnation changed")
        );
        let missing = ChannelIncarnation {
            channel: ChannelId::new(),
            ..home(a)
        };
        assert_eq!(
            refusal(&mut registry, &retire(missing, 0)).as_deref(),
            Some("The channel no longer exists")
        );
        // A pane, or a publication since the window looked, keeps the channel.
        seed(&mut registry, b, vec![pane()]);
        let held = registry.view_revision(&Scope::Channel(b)).unwrap();
        assert!(!registry.apply(&retire(home(b), held)).unwrap().changed);
        let seen = registry.view_revision(&Scope::Channel(a)).unwrap();
        assert!(!registry.apply(&retire(home(a), seen + 1)).unwrap().changed);
        assert!(registry.model().unwrap().channel(a).is_some());
        // The empty view at the revision the window saw retires it alone.
        assert!(registry.apply(&retire(home(a), seen)).unwrap().changed);
        let after = registry.model().unwrap();
        assert!(after.channel(a).is_none());
        assert!(after.channel(b).is_some());
    }

    #[test]
    fn authenticated_direct_adoption_publishes_only_on_a_committed_process_handoff() {
        use crate::agent_execution::HostedSession;
        let (mut registry, _directory, a, _) = setup();
        let original = live_source_pane();
        seed(&mut registry, a, vec![original.clone()]);
        let CardKind::Terminal(current) = &original.kind else {
            panic!()
        };
        let source = HostedSession::Terminal(current.clone());
        let mut next = current.clone();
        next.continuity = crate::model::TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("live"),
            boot_id: crate::model::BootId::new("boot"),
            host_incarnation: crate::model::HostIncarnation::new("successor"),
        };
        let successor = HostedSession::Terminal(next.clone());
        let agent = crate::model::AgentId::from_uuid(original.id.as_uuid());
        let before = read(&registry.db, &Scope::Channel(a)).unwrap();
        registry.db.execute_batch("CREATE TRIGGER reject_fixture_view BEFORE UPDATE ON panes BEGIN SELECT RAISE(ABORT,'fixture publication failure'); END;").unwrap();
        {
            let tx = registry
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            assert!(
                adopt_authenticated_agent_pane(
                    &tx,
                    agent,
                    std::slice::from_ref(&source),
                    &successor
                )
                .is_err()
            );
            // Drop rolls back the rejected descriptor write.
        }
        assert_eq!(read(&registry.db, &Scope::Channel(a)).unwrap(), before);
        registry
            .db
            .execute_batch("DROP TRIGGER reject_fixture_view;")
            .unwrap();
        {
            let tx = registry
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .unwrap();
            assert!(
                adopt_authenticated_agent_pane(
                    &tx,
                    agent,
                    std::slice::from_ref(&source),
                    &successor
                )
                .unwrap()
            );
            tx.commit().unwrap();
        }
        let after = read(&registry.db, &Scope::Channel(a)).unwrap();
        assert!(after.revision > before.revision);
        assert_eq!(
            after.panes[0],
            Pane {
                kind: CardKind::Terminal(next),
                ..original
            }
        );
    }

    #[test]
    fn channel_and_project_cascades_release_only_their_own_panes() {
        let (mut registry, _directory, a, b) = setup();
        let channel_actor = live_source_pane();
        let other_actor = live_source_pane();
        let worktree_actor = live_source_pane();
        seed(&mut registry, a, vec![channel_actor.clone()]);
        seed(&mut registry, b, vec![other_actor.clone()]);
        let worktree = Scope::Worktree {
            repository: "/repo".into(),
            path: "/repo/tree".into(),
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope: worktree.clone(),
                panes: vec![worktree_actor.clone()],
                closed: vec![],
            })
            .unwrap();
        registry.apply(&ModelEdit::DeleteChannel { id: a }).unwrap();
        assert!(owner(&registry.db, channel_actor.id).unwrap().is_none());
        assert_eq!(
            owner(&registry.db, other_actor.id).unwrap(),
            Some(Scope::Channel(b))
        );
        assert_eq!(
            owner(&registry.db, worktree_actor.id).unwrap(),
            Some(worktree)
        );
        registry
            .apply(&ModelEdit::CloseProject {
                repository: "/repo".into(),
            })
            .unwrap();
        assert!(all(&registry.db).unwrap().is_empty());
        for id in [channel_actor.id, other_actor.id, worktree_actor.id] {
            assert!(owner(&registry.db, id).unwrap().is_none());
        }
    }

    #[test]
    fn publication_failures_roll_back_transfer_agent_index_and_ledger() {
        use crate::agent_execution::{
            Destination, HostedSession, Origin, Spec, Submit, Surface, Target,
        };
        let (mut registry, _directory, a, b) = setup();
        let accepted = registry
            .submit_agent(
                &Origin::User,
                &Submit {
                    name: Some("fixture-worker".into()),
                    destination: Destination::New(Spec {
                        provider: crate::agent::Provider::Codex,
                        account: None,
                        target: Target {
                            repository: "/repo".into(),
                            directory: "/repo".into(),
                            placement: Some(Scope::Channel(a)),
                        },
                        surface: Surface::Terminal,
                        model: None,
                        permission_mode: None,
                        reasoning_effort: None,
                    }),
                    prompt: "Fixture only; never dispatched".into(),
                    idempotency_key: "placement-transfer-rollback".into(),
                },
                1,
            )
            .unwrap();
        let mut original = live_source_pane();
        original.id = CardId::from_uuid(accepted.agent.id.as_uuid());
        let CardKind::Terminal(session) = &original.kind else {
            panic!()
        };
        registry
            .bind_agent(
                accepted.agent.id,
                None,
                HostedSession::Terminal(session.clone()),
            )
            .unwrap();
        seed(&mut registry, a, vec![original.clone()]);
        seed(&mut registry, b, vec![]);
        let source = Scope::Channel(a);
        let target = Scope::Channel(b);
        let source_before = read(&registry.db, &source).unwrap();
        let target_before = read(&registry.db, &target).unwrap();
        let model_before = registry.model().unwrap();
        let agent_before = registry.agent(accepted.agent.id).unwrap();
        let transfer = edit(
            &mut registry,
            a,
            Change::Transfer {
                id: original.id,
                destination: target.clone(),
                before: None,
            },
        );
        let ModelEdit::ChangeView { operation, .. } = &transfer else {
            panic!()
        };
        assert!(!operation_seen(&registry.db, *operation).unwrap());
        // TEMP keeps the published main schema unchanged. Both failures occur
        // after agent/index/ledger work. The target's distinct error also proves
        // the real source view was empty before its prepared target was written.
        let source_id: i64 = registry
            .db
            .query_row(
                "SELECT id FROM view_scopes WHERE channel=?1",
                [a.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let target_id: i64 = registry
            .db
            .query_row(
                "SELECT id FROM view_scopes WHERE channel=?1",
                [b.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        for fail_source in [true, false] {
            let (scope_id, expected, failure) = if fail_source {
                (
                    source_id,
                    "fixture source publication failure",
                    "SELECT RAISE(ABORT,'fixture source publication failure');".to_owned(),
                )
            } else {
                (
                    target_id,
                    "fixture target publication failure",
                    format!(
                        "SELECT CASE WHEN EXISTS(SELECT 1 FROM main.panes WHERE scope={source_id})
                     THEN RAISE(ABORT,'fixture target preceded source')
                     ELSE RAISE(ABORT,'fixture target publication failure') END;"
                    ),
                )
            };
            registry
                .db
                .execute_batch(&format!(
                "CREATE TEMP TRIGGER reject_fixture_publication BEFORE UPDATE OF revision ON main.view_scopes
                 WHEN NEW.id={scope_id} BEGIN {failure} END;",
            ))
                .unwrap();
            assert!(registry.apply(&transfer).unwrap_err().contains(expected));
            assert_eq!(read(&registry.db, &source).unwrap(), source_before);
            assert_eq!(read(&registry.db, &target).unwrap(), target_before);
            assert_eq!(registry.model().unwrap(), model_before);
            assert_eq!(registry.agent(accepted.agent.id).unwrap(), agent_before);
            assert_eq!(
                owner(&registry.db, original.id).unwrap(),
                Some(source.clone())
            );
            assert!(!operation_seen(&registry.db, *operation).unwrap());
            registry
                .db
                .execute_batch("DROP TRIGGER temp.reject_fixture_publication;")
                .unwrap();
        }
        registry.apply(&transfer).unwrap();
        assert!(operation_seen(&registry.db, *operation).unwrap());
        assert!(read(&registry.db, &source).unwrap().panes.is_empty());
        assert_eq!(
            read(&registry.db, &target).unwrap().panes,
            [original.clone()]
        );
        assert_eq!(
            owner(&registry.db, original.id).unwrap(),
            Some(target.clone())
        );
        assert_eq!(registry.agent(accepted.agent.id).unwrap().placement, target);
        let back = edit(
            &mut registry,
            b,
            Change::Transfer {
                id: original.id,
                destination: source.clone(),
                before: None,
            },
        );
        registry.apply(&back).unwrap();
        assert_eq!(
            read(&registry.db, &source).unwrap().panes,
            [original.clone()]
        );
        assert_eq!(
            owner(&registry.db, original.id).unwrap(),
            Some(source.clone())
        );
        assert_eq!(registry.agent(accepted.agent.id).unwrap().placement, source);
    }

    #[test]
    fn terminal_retention_projection_tracks_view_changes_and_rolls_back_with_them() {
        use std::collections::BTreeSet;
        let (mut registry, directory, a, b) = setup();
        let original = live_source_pane();
        let terminal_id = |pane: &Pane| {
            let CardKind::Terminal(session) = &pane.kind else {
                panic!("terminal")
            };
            session.id
        };
        let old = terminal_id(&original);
        seed(&mut registry, a, vec![original.clone()]);
        registry.pin_window_terminals(&[old]).unwrap();
        let generation = registry.retention_pin_generation(old).unwrap().unwrap();
        registry.forget_retention_pin(old, &generation).unwrap();
        assert_eq!(
            registry.pinned_terminals(&BTreeSet::from([old])).unwrap(),
            BTreeSet::from([old]),
            "forgetting local membership must not erase an independently saved shared pane"
        );
        let mut replacement = original.clone();
        if let CardKind::Terminal(session) = &mut replacement.kind {
            session.id = crate::model::TerminalId::new();
        }
        let new = terminal_id(&replacement);
        let candidates = BTreeSet::from([old, new]);
        let update = edit(
            &mut registry,
            a,
            Change::Update {
                before: original.clone(),
                pane: replacement.clone(),
            },
        );
        let before = registry.model().unwrap();
        registry.db.execute_batch("CREATE TEMP TRIGGER reject_retention BEFORE UPDATE ON shared_terminal_retention_pins BEGIN SELECT RAISE(ABORT, 'retention projection failure'); END;").unwrap();
        assert!(
            registry
                .apply(&update)
                .unwrap_err()
                .contains("retention projection failure")
        );
        assert_eq!(registry.model().unwrap(), before);
        assert_eq!(
            registry.pinned_terminals(&candidates).unwrap(),
            BTreeSet::from([old])
        );
        registry
            .db
            .execute_batch("DROP TRIGGER reject_retention;")
            .unwrap();
        assert!(registry.apply(&update).unwrap().changed);
        assert_eq!(
            registry.pinned_terminals(&candidates).unwrap(),
            BTreeSet::from([new])
        );
        let browser = Pane {
            kind: pane().kind,
            ..replacement.clone()
        };
        let change = edit(
            &mut registry,
            a,
            Change::Update {
                before: replacement.clone(),
                pane: browser.clone(),
            },
        );
        registry.apply(&change).unwrap();
        assert!(registry.pinned_terminals(&candidates).unwrap().is_empty());
        let change = edit(
            &mut registry,
            a,
            Change::Update {
                before: browser,
                pane: replacement.clone(),
            },
        );
        registry.apply(&change).unwrap();
        let CardKind::Terminal(terminal) = replacement.kind.clone() else {
            unreachable!()
        };
        let mut chat = replacement.clone();
        chat.kind = CardKind::Chat(crate::chat::Session {
            id: terminal.id,
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: terminal.launch.clone(),
                session_id: None,
            },
            proof: terminal.continuity.live_proof(),
            terminal: Some(terminal),
            owned_codex_listener: None,
            native_continuity: Default::default(),
        });
        let change = edit(
            &mut registry,
            a,
            Change::Update {
                before: replacement,
                pane: chat.clone(),
            },
        );
        registry.apply(&change).unwrap();
        assert_eq!(
            registry.pinned_terminals(&candidates).unwrap(),
            BTreeSet::from([new])
        );
        let mut no_terminal = chat.clone();
        if let CardKind::Chat(session) = &mut no_terminal.kind {
            session.terminal = None;
        }
        let change = edit(
            &mut registry,
            a,
            Change::Update {
                before: chat.clone(),
                pane: no_terminal.clone(),
            },
        );
        registry.apply(&change).unwrap();
        assert!(registry.pinned_terminals(&candidates).unwrap().is_empty());
        let change = edit(
            &mut registry,
            a,
            Change::Update {
                before: no_terminal,
                pane: chat.clone(),
            },
        );
        registry.apply(&change).unwrap();
        let replacement = chat;
        let transfer = edit(
            &mut registry,
            a,
            Change::Transfer {
                id: replacement.id,
                destination: Scope::Channel(b),
                before: None,
            },
        );
        let before_transfer = registry.model().unwrap();
        registry.db.execute_batch("CREATE TEMP TRIGGER reject_retention_transfer BEFORE INSERT ON shared_terminal_retention_pins BEGIN SELECT RAISE(ABORT, 'retention transfer failure'); END;").unwrap();
        assert!(
            registry
                .apply(&transfer)
                .unwrap_err()
                .contains("retention transfer failure")
        );
        assert_eq!(registry.model().unwrap(), before_transfer);
        assert_eq!(
            registry.pinned_terminals(&candidates).unwrap(),
            BTreeSet::from([new])
        );
        registry
            .db
            .execute_batch("DROP TRIGGER reject_retention_transfer;")
            .unwrap();
        registry.apply(&transfer).unwrap();
        assert!(
            read(&registry.db, &Scope::Channel(a))
                .unwrap()
                .panes
                .is_empty()
        );
        assert_eq!(
            read(&registry.db, &Scope::Channel(b)).unwrap().panes,
            vec![replacement.clone()]
        );
        assert_eq!(
            registry.pinned_terminals(&candidates).unwrap(),
            BTreeSet::from([new])
        );
        drop(registry);
        let mut registry = Registry::at(&directory.path().join("model.sqlite3")).unwrap();
        assert_eq!(
            registry.pinned_terminals(&candidates).unwrap(),
            BTreeSet::from([new])
        );
        let removal = edit(&mut registry, b, Change::Remove { id: replacement.id });
        registry.apply(&removal).unwrap();
        assert!(registry.pinned_terminals(&candidates).unwrap().is_empty());
    }

    #[test]
    fn authenticated_terminal_adoption_updates_retention_in_the_same_transaction() {
        use crate::agent_execution::{
            Destination, HostedSession, Origin, Spec, Submit, Surface, Target,
        };
        use std::collections::BTreeSet;
        let (mut registry, directory, a, _) = setup();
        let accepted = registry
            .submit_agent(
                &Origin::User,
                &Submit {
                    name: None,
                    destination: Destination::New(Spec {
                        provider: crate::agent::Provider::Claude,
                        account: None,
                        target: Target {
                            repository: "/repo".into(),
                            directory: "/repo".into(),
                            placement: Some(Scope::Channel(a)),
                        },
                        surface: Surface::Terminal,
                        model: None,
                        permission_mode: None,
                        reasoning_effort: None,
                    }),
                    prompt: "Work".into(),
                    idempotency_key: "retention-index-adoption".into(),
                },
                1,
            )
            .unwrap();
        let mut pane = live_source_pane();
        pane.id = CardId::from_uuid(accepted.agent.id.as_uuid());
        let CardKind::Terminal(terminal) = &pane.kind else {
            panic!("terminal")
        };
        let id = terminal.id;
        let source = HostedSession::Terminal(terminal.clone());
        registry
            .bind_agent(accepted.agent.id, None, source.clone())
            .unwrap();
        seed(&mut registry, a, vec![pane]);
        let mut successor = source.clone();
        if let HostedSession::Terminal(terminal) = &mut successor {
            terminal.continuity = crate::model::TerminalContinuity::Live {
                host_session_id: crate::model::HostSessionId::new("live"),
                boot_id: crate::model::BootId::new("boot"),
                host_incarnation: crate::model::HostIncarnation::new("retention-successor"),
            };
        }
        let before = registry.model().unwrap();
        registry.db.execute_batch("CREATE TEMP TRIGGER reject_adoption_retention BEFORE INSERT ON shared_terminal_retention_pins BEGIN SELECT RAISE(ABORT, 'adoption retention failure'); END;").unwrap();
        assert!(
            registry
                .adopt_authenticated_agent_session(std::slice::from_ref(&source), &successor)
                .unwrap_err()
                .contains("adoption retention failure")
        );
        assert_eq!(
            registry.model().unwrap(),
            before,
            "Agent source, view, Work and sequence must all roll back"
        );
        assert_eq!(
            registry.pinned_terminals(&BTreeSet::from([id])).unwrap(),
            BTreeSet::from([id])
        );
        registry
            .db
            .execute_batch("DROP TRIGGER reject_adoption_retention;")
            .unwrap();
        assert!(
            registry
                .adopt_authenticated_agent_session(std::slice::from_ref(&source), &successor)
                .unwrap()
        );
        assert_eq!(
            registry.agent(accepted.agent.id).unwrap().session,
            Some(successor.clone())
        );
        assert!(
            matches!(&read(&registry.db, &Scope::Channel(a)).unwrap().panes[0].kind,
            CardKind::Terminal(terminal) if HostedSession::Terminal(terminal.clone()) == successor)
        );
        assert_eq!(
            registry.pinned_terminals(&BTreeSet::from([id])).unwrap(),
            BTreeSet::from([id])
        );
        drop(registry);
        assert_eq!(
            Registry::at(&directory.path().join("model.sqlite3"))
                .unwrap()
                .pinned_terminals(&BTreeSet::from([id]))
                .unwrap(),
            BTreeSet::from([id])
        );
    }
}
