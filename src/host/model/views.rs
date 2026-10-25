use super::*;
use crate::workspace_model::views::{Change, Scope, View};

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
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS shared_view_scopes(
            scope TEXT PRIMARY KEY,
            channel TEXT REFERENCES channels(id) ON DELETE CASCADE,
            repository TEXT REFERENCES projects(repository) ON DELETE CASCADE,
            CHECK((channel IS NULL) != (repository IS NULL))
        ) STRICT;
        CREATE TABLE IF NOT EXISTS shared_views(
            scope TEXT PRIMARY KEY REFERENCES shared_view_scopes(scope) ON DELETE CASCADE,
            revision INTEGER NOT NULL, panes TEXT NOT NULL, retired TEXT NOT NULL
        ) STRICT;
        CREATE TABLE IF NOT EXISTS shared_view_panes(
            id TEXT PRIMARY KEY,
            scope TEXT NOT NULL REFERENCES shared_view_scopes(scope) ON DELETE CASCADE
        ) STRICT;
        CREATE TABLE IF NOT EXISTS shared_view_edits(
            operation TEXT PRIMARY KEY,
            scope TEXT NOT NULL REFERENCES shared_view_scopes(scope) ON DELETE CASCADE
        ) STRICT;",
    )
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
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS shared_view_terminal_sources(
            pane TEXT PRIMARY KEY REFERENCES shared_view_panes(id) ON DELETE CASCADE,
            source TEXT NOT NULL, ended TEXT NOT NULL
        ) STRICT;",
    )
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

fn key(scope: &Scope) -> Result<String, String> {
    serde_json::to_string(scope).map_err(|error| error.to_string())
}
fn parse_scope(key: &str) -> Result<Scope, String> {
    serde_json::from_str(key).map_err(|error| error.to_string())
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
        db.execute(
            "UPDATE agents SET record=?2 WHERE id=?1",
            params![
                agent.id.to_string(),
                serde_json::to_string(&agent).map_err(|error| error.to_string())?
            ],
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}
fn register(db: &Connection, scope: &Scope) -> Result<(), String> {
    let (channel, repository) = match scope {
        Scope::Channel(id) => (Some(id.to_string()), None),
        Scope::Worktree { repository, .. } => (None, Some(repository_key(repository)?.to_owned())),
    };
    db.execute("INSERT INTO shared_view_scopes(scope,channel,repository) VALUES (?1,?2,?3) ON CONFLICT DO NOTHING",params![key(scope)?,channel,repository]).map_err(|error|error.to_string())?;
    Ok(())
}

pub(super) fn read(db: &Connection, scope: &Scope) -> Result<View, String> {
    let row: Option<(u64, String, String)> = db
        .query_row(
            "SELECT revision, panes, retired FROM shared_views WHERE scope = ?1",
            [key(scope)?],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    match row {
        Some((revision, panes, retired)) => Ok(View {
            scope: scope.clone(),
            revision,
            panes: serde_json::from_str(&panes).map_err(|error| error.to_string())?,
            retired: serde_json::from_str(&retired).map_err(|error| error.to_string())?,
        }),
        None => Ok(View::empty(scope.clone())),
    }
}

pub(super) fn all(db: &Connection) -> Result<Vec<View>, String> {
    let scopes = db
        .prepare("SELECT scope FROM shared_views ORDER BY scope")
        .map_err(|error| error.to_string())?
        .query_map([], |row| row.get::<_, String>(0))
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map_err(|error| error.to_string())?;
    scopes
        .into_iter()
        .map(|id| read(db, &parse_scope(&id)?))
        .collect()
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
    view.revision = view
        .revision
        .checked_add(1)
        .ok_or("view revision exhausted")?;
    db.execute(
        "DELETE FROM shared_view_panes WHERE id=?1",
        [id.to_string()],
    )
    .map_err(|error| error.to_string())?;
    write(db, &view)?;
    Ok(true)
}

pub(super) fn operation_seen(db: &Connection, operation: Uuid) -> Result<bool, String> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM shared_view_edits WHERE operation = ?1)",
        [operation.to_string()],
        |row| row.get(0),
    )
    .map_err(|error| error.to_string())
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
    register(transaction, scope)?;
    let mut view = read(transaction, scope)?;
    let mut changed = false;
    let mut deferred_target = None;
    match edit {
        ModelEdit::SeedView { panes, closed, .. } => {
            // Migration is additive, including old independent windows. Closed
            // IDs are applied first and prevent stale caches from resurrecting.
            for id in closed {
                changed |= view.apply(&Change::Remove { id: *id })?;
                transaction
                    .execute(
                        "DELETE FROM shared_view_panes WHERE id=?1 AND scope=?2",
                        params![id.to_string(), key(scope)?],
                    )
                    .map_err(|error| error.to_string())?;
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
                return Err("workspace view revision conflict".into());
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
                        register(transaction, destination)?;
                        let mut target = read(transaction, destination)?;
                        target.retired.retain(|retired| retired != id);
                        target.apply(&Change::Insert {
                            pane: pane.clone(),
                            before: *before,
                        })?;
                        target.revision = target
                            .revision
                            .checked_add(1)
                            .ok_or("view revision exhausted")?;
                        // Publish only after the source relinquishes its
                        // active placement, inside this same transaction.
                        deferred_target = Some(target);
                        transfer_agent(transaction, &pane, destination, &repository)?;
                        transaction
                            .execute(
                                "UPDATE shared_view_panes SET scope=?1 WHERE id=?2",
                                params![key(destination)?, id.to_string()],
                            )
                            .map_err(|error| error.to_string())?;
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
                    // Revision-only retries must not replay an old source over
                    // a successor or undo host-owned start/location changes.
                    // Reject the whole edit before recording its operation ID;
                    // the client can refuse source intent and retry presentation
                    // separately against the authoritative pane.
                    let source_changed = before.kind != pane.kind
                        || before.pending_terminal_start != pane.pending_terminal_start
                        || before.scope != pane.scope;
                    if source_changed
                        && !view.panes.iter().any(|current| {
                            current.id == before.id
                                && current.kind == before.kind
                                && current.pending_terminal_start == before.pending_terminal_start
                                && current.scope == before.scope
                        })
                    {
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
                Change::Remove { id } => {
                    transaction
                        .execute(
                            "DELETE FROM shared_view_panes WHERE id=?1 AND scope=?2",
                            params![id.to_string(), key(scope)?],
                        )
                        .map_err(|error| error.to_string())?;
                    view.apply(change)?
                }
                _ => view.apply(change)?,
            };
            transaction
                .execute(
                    "INSERT INTO shared_view_edits(operation, scope) VALUES (?1, ?2)",
                    params![operation.to_string(), key(scope)?],
                )
                .map_err(|error| error.to_string())?;
        }
        _ => unreachable!(),
    }
    for pane in &view.panes {
        transaction.execute("INSERT INTO shared_view_panes(id, scope) VALUES (?1, ?2) ON CONFLICT(id) DO NOTHING", params![pane.id.to_string(), key(scope)?]).map_err(|error| error.to_string())?;
    }
    // Even an empty seed creates an authoritative view, so another client sees
    // emptiness rather than an uninitialized migration.
    let existed: bool = transaction
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM shared_views WHERE scope = ?1)",
            [key(scope)?],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if changed || !existed {
        view.revision = view
            .revision
            .checked_add(1)
            .ok_or("view revision exhausted")?;
        write(transaction, &view)?;
    }
    if let Some(target) = deferred_target {
        write(transaction, &target)?;
    }
    Ok(changed || !existed)
}

impl Registry {
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
}

fn resource_owner(
    db: &Connection,
    pane: &crate::workspace_model::views::Pane,
) -> Result<Option<crate::model::CardId>, String> {
    Ok(all(db)?
        .iter()
        .flat_map(|view| &view.panes)
        .find(|other| {
            other.id != pane.id
                && crate::workspace_model::views::same_resource(&other.kind, &pane.kind)
        })
        .map(|other| other.id))
}

fn owner(db: &Connection, id: crate::model::CardId) -> Result<Option<Scope>, String> {
    db.query_row(
        "SELECT scope FROM shared_view_panes WHERE id=?1",
        [id.to_string()],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(|error| error.to_string())?
    .map(|id| parse_scope(&id))
    .transpose()
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
    let before = view.clone();
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
    pane.kind = replacement;
    view.revision = view
        .revision
        .checked_add(1)
        .ok_or("view revision exhausted")?;
    channel_work::placement::observe_view_transition(db, &before, &view)?;
    // Holder continuity does not start a new channel naming choice, including
    // learning a previously unobserved provider thread. Preserve all placement,
    // presentation, retirement and naming state outside the descriptor itself.
    db.execute(
        "UPDATE shared_views SET revision=?2, panes=?3 WHERE scope=?1",
        params![
            key(&scope)?,
            view.revision,
            serde_json::to_string(&view.panes).map_err(|error| error.to_string())?
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(true)
}

/// Record removal before foreign-key cascades erase canonical membership.
/// Like normal view writes, this must participate in the deleting transaction.
pub(super) fn retire_channel_placements(db: &Connection, channel: ChannelId) -> Result<(), String> {
    retire_scope_placements(db, &Scope::Channel(channel))
}

pub(super) fn retire_project_placements(db: &Connection, repository: &Path) -> Result<(), String> {
    if db.is_autocommit() {
        return Err("Placement removal requires the canonical view transaction".into());
    }
    let repository = repository_key(repository)?;
    let scopes = {
        let mut statement = db
            .prepare(
                "SELECT s.scope FROM shared_view_scopes s
             LEFT JOIN channels c ON c.id=s.channel
             WHERE s.repository=?1 OR c.repository=?1",
            )
            .map_err(|error| error.to_string())?;
        statement
            .query_map([repository], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| error.to_string())?
    };
    for raw in scopes {
        let scope = parse_scope(&raw)?;
        retire_scope_placements(db, &scope)?;
    }
    Ok(())
}

fn retire_scope_placements(db: &Connection, scope: &Scope) -> Result<(), String> {
    let before = read(db, scope)?;
    let mut after = before.clone();
    after.panes.clear();
    channel_work::placement::observe_view_transition(db, &before, &after)
}

fn write(db: &Connection, view: &View) -> Result<(), String> {
    let before = read(db, &view.scope)?;
    channel_work::placement::observe_view_transition(db, &before, view)?;
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
        if primary(&before) != primary(view) {
            // A new primary conversation starts a new automatic naming choice.
            // Manual/legacy names describe the channel and survive all moves.
            db.execute(
                "UPDATE channels SET title='', title_source=0 WHERE id=?1 AND title_source<>2",
                [id.to_string()],
            )
            .map_err(|error| error.to_string())?;
            channel_work::renamed_in(db, *id)?;
        }
    }
    db.execute("INSERT INTO shared_views(scope, revision, panes, retired) VALUES (?1, ?2, ?3, ?4)
        ON CONFLICT(scope) DO UPDATE SET revision=excluded.revision, panes=excluded.panes, retired=excluded.retired",
        params![key(&view.scope)?, view.revision,
            serde_json::to_string(&view.panes).map_err(|error| error.to_string())?,
            serde_json::to_string(&view.retired).map_err(|error| error.to_string())?]).map_err(|error| error.to_string())?;
    Ok(())
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
    fn primary_chat_change_keeps_work_home_title_in_the_canonical_transaction() {
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
            let title: String = registry
                .db
                .query_row(
                    "SELECT title FROM channel_work_homes WHERE channel=?1 AND incarnation=?2",
                    params![channel.to_string(), canonical.ordinal],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                title, canonical.title,
                "retained title follows the exact current canonical incarnation"
            );
            assert!(
                channel_work::renamed_in(&registry.db, channel).is_err(),
                "title synchronization cannot escape the canonical transaction"
            );
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
                r.db.execute_batch("CREATE TRIGGER reject_pane_adoption BEFORE UPDATE ON shared_views BEGIN SELECT RAISE(ABORT, 'fixture failed pane commit'); END;").unwrap();
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
                r.db.execute(
                    "UPDATE shared_views SET panes=?2 WHERE scope=?1",
                    params![
                        key(&Scope::Channel(a)).unwrap(),
                        serde_json::to_string(&vec![p.clone()]).unwrap()
                    ],
                )
                .unwrap();
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
                r.db.execute(
                    "UPDATE shared_views SET panes=?2 WHERE scope=?1",
                    params![
                        key(&Scope::Channel(a)).unwrap(),
                        serde_json::to_string(&vec![p.clone()]).unwrap()
                    ],
                )
                .unwrap();
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

    include!("views/placement_tests.rs");
}
