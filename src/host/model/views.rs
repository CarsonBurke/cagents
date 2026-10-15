use super::*;
use crate::workspace_model::views::{Change, Scope, View};

mod presentation_repair;
mod presentation_close;
pub(super) use presentation_close::{remove_open_close, validate_open_close};

pub(super) fn migrate(tx: &rusqlite::Transaction<'_>) -> Result<(), String> {
    let current: u32 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if current >= 8 {
        return Ok(());
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
    let legacy: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='workspace_views')",
            [],
            |row| row.get(0),
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

/// The old JSON form is the exact original live source. A Locate receipt
/// additionally retains the strict historical end separately from its current
/// planned relaunch directory. None is explicitly presentation-only history:
/// locating an old descriptor never invents a worker-generation proof.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(untagged)]
enum TerminalSource {
    Issued(crate::model::TerminalSession),
    Located {
        historical_end: crate::model::TerminalSession,
        source: Option<crate::model::TerminalSession>,
    },
}

struct RecordedTerminalEnd {
    source: TerminalSource,
    ended: crate::model::TerminalSession,
}

impl RecordedTerminalEnd {
    fn original_source(&self) -> Option<&crate::model::TerminalSession> {
        match &self.source {
            TerminalSource::Issued(source) => Some(source),
            TerminalSource::Located { source, .. } => source.as_ref(),
        }
    }

    fn matches_end(&self, current: &crate::model::TerminalSession) -> bool {
        use crate::model::TerminalContinuity;
        let historical = match &self.source {
            TerminalSource::Issued(_) => &self.ended,
            TerminalSource::Located { historical_end, .. } => historical_end,
        };
        if current != &self.ended || !ended_terminal(historical)
            || !same_terminal_identity(historical, current)
            || self.original_source().is_some_and(|source| {
                !same_terminal_identity(source, historical)
                    || !source.continuity.live_proof().is_some_and(|proof| proof.is_complete())
            })
        {
            return false;
        }
        match (&historical.continuity, &current.continuity) {
            (TerminalContinuity::RelaunchAvailable { recipe: old, last_exit: old_exit },
                TerminalContinuity::RelaunchAvailable { recipe: current, last_exit }) => {
                let mut projected = old.clone();
                projected.working_directory = current.working_directory.clone();
                projected == *current && old_exit == last_exit
            }
            (TerminalContinuity::Exited { last_exit: old },
                TerminalContinuity::Exited { last_exit }) => old == last_exit,
            _ => false,
        }
    }
}

fn terminal_source(
    db: &Connection,
    id: crate::model::CardId,
) -> Result<Option<RecordedTerminalEnd>, String> {
    let raw: Option<(String, String)> = db
        .query_row(
            "SELECT source,ended FROM shared_view_terminal_sources WHERE pane=?1",
            [id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    raw.map(|(source, ended)| {
        Ok(RecordedTerminalEnd {
            source: serde_json::from_str(&source).map_err(|error| error.to_string())?,
            ended: serde_json::from_str(&ended).map_err(|error| error.to_string())?,
        })
    })
    .transpose()
}

fn write_terminal_record(
    db: &Connection,
    id: crate::model::CardId,
    record: &RecordedTerminalEnd,
) -> Result<(), String> {
    db.execute(
        "INSERT INTO shared_view_terminal_sources(pane,source,ended) VALUES(?1,?2,?3)
        ON CONFLICT(pane) DO UPDATE SET source=excluded.source,ended=excluded.ended",
        params![
            id.to_string(),
            serde_json::to_string(&record.source).map_err(|error| error.to_string())?,
            serde_json::to_string(&record.ended).map_err(|error| error.to_string())?
        ],
    )
    .map_err(|error| error.to_string())?;
    Ok(())
}

fn relocate_terminal_end(
    db: &Connection,
    before: &crate::workspace_model::views::Pane,
    after: &crate::workspace_model::views::Pane,
) -> Result<(), String> {
    use crate::model::CardKind;
    let (CardKind::Terminal(previous), CardKind::Terminal(current)) = (&before.kind, &after.kind)
        else { return Ok(()); };
    if previous == current { return Ok(()); }
    let record = terminal_source(db, before.id)?;
    let source = match record {
        Some(record) => {
            if !record.matches_end(previous) {
                return Err("Cannot locate the ended terminal: its current descriptor has no exact retained historical basis".into());
            }
            match record.source {
                TerminalSource::Issued(source) => TerminalSource::Located {
                    historical_end: previous.clone(), source: Some(source),
                },
                located @ TerminalSource::Located { .. } => located,
            }
        }
        None if ended_terminal(previous) => TerminalSource::Located {
            historical_end: previous.clone(), source: None,
        },
        // A restart candidate without an observed exit is not proof of an end.
        None => return Ok(()),
    };
    let record = RecordedTerminalEnd { source, ended: current.clone() };
    if !record.matches_end(current) {
        return Err("Locate may change only the planned relaunch directory of this exact historical end".into());
    }
    write_terminal_record(db, before.id, &record)
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
            if same_terminal_identity(before, after) {
                if ended_terminal(after) && before.continuity.live_proof()
                    .is_some_and(|proof| proof.is_complete())
                {
                    return write_terminal_record(db, id, &RecordedTerminalEnd {
                        source: TerminalSource::Issued(before.clone()), ended: after.clone(),
                    });
                }
                if before.continuity == after.continuity {
                    if let Some(mut record) = terminal_source(db, id)? {
                        if record.matches_end(before) {
                            record.ended = after.clone();
                            return write_terminal_record(db, id, &record);
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

/// Move shared scopes without losing pane/operation IDs. Revision fencing
/// refuses queued edits built against the pre-relocation descriptors.
pub(super) fn relocate(
    db: &Connection,
    relocation: &crate::project_recovery::Relocation,
) -> Result<(), String> {
    for mut view in all(db)? {
        let belongs = match &view.scope {
            Scope::Worktree { repository, .. } => *repository == relocation.repository,
            Scope::Channel(channel) => db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM channels WHERE id=?1 AND repository=?2)",
                    params![channel.to_string(), repository_key(&relocation.repository)?],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?,
        };
        if !belongs {
            continue;
        }
        let previous = key(&view.scope)?;
        let before = view.panes.clone();
        relocation.view(&mut view);
        for (before, after) in before.iter().zip(&view.panes) {
            relocate_terminal_end(db, before, after)?;
        }
        view.revision = view
            .revision
            .checked_add(1)
            .ok_or("view revision exhausted")?;
        let next = key(&view.scope)?;
        register(db, &view.scope)?;
        write(db, &view)?;
        if previous != next {
            for table in ["shared_view_panes", "shared_view_edits"] {
                db.execute(
                    &format!("UPDATE {table} SET scope=?2 WHERE scope=?1"),
                    params![previous, next],
                )
                .map_err(|error| error.to_string())?;
            }
            db.execute("DELETE FROM shared_view_scopes WHERE scope=?1", [previous])
                .map_err(|error| error.to_string())?;
        }
    }
    Ok(())
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
        if !original.iter().any(|source| source.id == ended.id && source.launch == ended.launch) {
            false
        } else if let Some(record) = terminal_source(db, id)? {
            if !record.matches_end(ended) {
                return Err("Cannot safely retire the ended managed terminal pane: its current descriptor has no exact retained generation proof".into());
            }
            let source = record.original_source().ok_or("Cannot safely retire the ended managed terminal pane: historical generation proof is unavailable; Locate retained only its presentation")?;
            original.iter().any(|retained| {
                HostedSession::Terminal((**retained).clone())
                    .same_process(&HostedSession::Terminal(source.clone()))
            })
        } else if matches!(ended.continuity,
            crate::model::TerminalContinuity::RelaunchAvailable { last_exit: Some(_), .. }
            | crate::model::TerminalContinuity::Exited { .. })
        {
            return Err("Cannot safely retire the ended managed terminal pane: historical generation proof is unavailable; the pane was retained and no retirement was guessed".into());
        } else {
            false
        }
    } else {
        false
    };
    if !retained {
        return Ok(None);
    }
    Ok(Some(view))
}

pub(super) fn preflight_retire_agent(db: &Connection, agent: &crate::agent_execution::Agent) -> Result<(), String> {
    agent_retirement_view(db, agent).map(|_| ())
}

pub(super) fn retire_agent(db: &Connection, agent: &crate::agent_execution::Agent) -> Result<bool, String> {
    let Some(mut view) = agent_retirement_view(db, agent)? else { return Ok(false); };
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
    if let ModelEdit::ChangeView {
        change:
            Change::RemoveArchivedAutomationSource {
                registry,
                repository,
                ..
            }
            | Change::InsertArchiveReport {
                registry,
                repository,
                ..
            },
        ..
    } = edit
    {
        let identity: String = transaction
            .query_row("SELECT identity FROM metadata WHERE key=0", [], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string())?;
        let scope_repository: String = match scope {
            Scope::Channel(channel) => transaction
                .query_row(
                    "SELECT repository FROM channels WHERE id=?1",
                    [channel.to_string()],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?,
            Scope::Worktree { repository, .. } => repository_key(repository)?.to_owned(),
        };
        let open: bool = transaction
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM projects WHERE repository=?1)",
                [repository_key(repository)?],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if *registry == crate::workspace_model::NO_REGISTRY
            || identity != registry.to_string()
            || scope_repository != repository_key(repository)?
            || !open
        {
            return Err("workspace archived pane source conflict".into());
        }
    }
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
                // A restart cannot turn an uncommitted local report pointer
                // into shared membership through ordinary migration seeding.
                if matches!(&pane.kind,crate::model::CardKind::Chat(session) if session.archive_report.is_some())
                {
                    if owner(transaction, pane.id)?.as_ref() != Some(scope)
                        || !view.panes.iter().any(|current| current == pane)
                    {
                        return Err(
                            "The saved report presentation requires its original typed publication"
                                .into(),
                        );
                    }
                    continue;
                }
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
            if let Change::RemoveSource {
                registry,
                repository,
                ..
            } = change
            {
                let identity: String = transaction
                    .query_row("SELECT identity FROM metadata WHERE key=0", [], |row| {
                        row.get(0)
                    })
                    .map_err(|error| error.to_string())?;
                let current_repository: String = match scope {
                    Scope::Channel(channel) => transaction
                        .query_row(
                            "SELECT repository FROM channels WHERE id=?1",
                            [channel.to_string()],
                            |row| row.get(0),
                        )
                        .map_err(|error| error.to_string())?,
                    Scope::Worktree { repository, .. } => repository_key(repository)?.to_owned(),
                };
                if *registry == crate::workspace_model::NO_REGISTRY
                    || identity != registry.to_string()
                    || current_repository != repository_key(repository)?
                {
                    return Err("workspace view removal source conflict".into());
                }
            }
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
                        write(transaction, &target)?;
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
                Change::InsertArchiveReport {
                    repository,
                    reference,
                    pane,
                    ..
                } => {
                    let crate::model::CardKind::Chat(session) = &pane.kind else {
                        return Err(
                            "A saved report must have a native read-only presentation".into()
                        );
                    };
                    let report =
                        super::automation_reports::report_in(transaction, reference, session)?;
                    if &report.scope != scope
                        || &report.repository != repository
                        || session.archive_report.as_ref() != Some(reference)
                        || owner(transaction, pane.id)?.is_some()
                        || pane.scope
                            != super::automation_reports::presentation_scope_in(
                                transaction,
                                reference,
                            )?
                    {
                        return Err("The saved report presentation source or scope changed".into());
                    }
                    view.apply(change)?
                }
                Change::Insert { pane, .. } | Change::InsertIfEmpty { pane } if matches!(&pane.kind,crate::model::CardKind::Chat(session) if session.archive_report.is_some()) =>
                {
                    return Err(
                        "Publish saved reports with their typed presentation receipt".into(),
                    );
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
                Change::Update { before, pane }
                    if {
                        let report = |kind: &crate::model::CardKind| matches!(kind,crate::model::CardKind::Chat(session) if session.archive_report.is_some());
                        let current = view.panes.iter().find(|current| current.id == before.id);
                        (report(&before.kind)
                            || report(&pane.kind)
                            || current.is_some_and(|current| report(&current.kind)))
                            && !current.is_some_and(|current| {
                                current.kind == before.kind && current.kind == pane.kind
                            })
                    } =>
                {
                    return Err("The saved report presentation identity cannot change".into());
                }
                Change::Update { pane, .. } if resource_owner(transaction, pane)?.is_some() => {
                    false
                }
                Change::RemoveSource { before, handoff, .. } => {
                    if owner(transaction, before.id)?.as_ref() != Some(scope)
                        || view
                            .panes
                            .iter()
                            .filter(|pane| pane.id == before.id)
                            .count()
                            != 1
                        || !view.panes.iter().any(|pane| {
                            crate::workspace_model::views::same_removal_pane_source(before, pane, handoff.as_ref())
                        })
                    {
                        return Err("workspace view removal source conflict".into());
                    }
                    let changed = view.apply(change)?;
                    transaction
                        .execute(
                            "DELETE FROM shared_view_panes WHERE id=?1 AND scope=?2",
                            params![before.id.to_string(), key(scope)?],
                        )
                        .map_err(|error| error.to_string())?;
                    changed
                }
                Change::RemoveArchivedAutomationSource { before, .. } => {
                    if owner(transaction, before.id)?.as_ref() != Some(scope)
                        || view
                            .panes
                            .iter()
                            .filter(|pane| pane.id == before.id)
                            .count()
                            != 1
                        || super::automation_membership::archived_owner_in(transaction, before)?
                            .is_none()
                    {
                        return Err("workspace archived pane source conflict".into());
                    }
                    super::automation_reports::capture_removal(
                        transaction,
                        *operation,
                        scope,
                        *expected_revision,
                        before,
                    )?;
                    let changed = view.apply(change)?;
                    transaction
                        .execute(
                            "DELETE FROM shared_view_panes WHERE id=?1 AND scope=?2",
                            params![before.id.to_string(), key(scope)?],
                        )
                        .map_err(|error| error.to_string())?;
                    changed
                }
                Change::Update { before, pane } => {
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
        transaction.execute("INSERT INTO shared_views(scope, revision, panes, retired) VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(scope) DO UPDATE SET revision=excluded.revision, panes=excluded.panes, retired=excluded.retired",
            params![key(scope)?, view.revision,
                serde_json::to_string(&view.panes).map_err(|error| error.to_string())?,
                serde_json::to_string(&view.retired).map_err(|error| error.to_string())?])
            .map_err(|error| error.to_string())?;
    }
    Ok(changed || !existed)
}

impl Registry {
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
fn write(db: &Connection, view: &View) -> Result<(), String> {
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
                checkout: None,
                primary_checkout: None,
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
            checkout: None, primary_checkout: None,
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
    fn transfer_requires_the_current_process_after_same_id_successor_adoption() {
        use crate::{
            agent::Provider,
            agent_execution::{Destination, HostedSession, Origin, Spec, Submit, Surface, Target},
        };
        for (native_chat, console, changed_launch) in [
            (false, false, false),
            (false, false, true),
            (true, false, false),
            (true, false, true),
            (true, true, false),
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
            let source = if native_chat {
                HostedSession::Chat(crate::chat::Session { archive_report: None,
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
            let p = Pane {
                kind: presentation(&source),
                ..pane()
            };
            seed(&mut r, a, vec![p.clone()]);
            let mut successor = source.clone();
            match &mut successor {
                HostedSession::Chat(session) => {
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
            assert!(
                r.adopt_authenticated_agent_session(std::slice::from_ref(&source), &successor)
                    .unwrap()
            );
            assert_eq!(
                read(&r.db, &Scope::Channel(a)).unwrap().panes,
                vec![p.clone()],
                "adoption need not refresh shared panes atomically"
            );
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
            let refreshed = Pane {
                kind: presentation(&successor),
                ..p.clone()
            };
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
                HostedSession::Chat(crate::chat::Session { archive_report: None,
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
            let p = Pane { kind, ..pane() };
            seed(&mut r, a, vec![p.clone()]);
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
                vec![p.clone()]
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
    fn source_checked_remove_refuses_replacement_registry_scope_and_startup_atomically() {
        for change in 0..5 {
            let (mut r, _, a, b) = setup();
            let original = pane();
            seed(&mut r, a, vec![original.clone()]);
            let identity = r.model().unwrap().identity;
            let mut conditional = edit(
                &mut r,
                a,
                Change::RemoveSource {
                    registry: identity,
                    repository: "/repo".into(),
                    before: original.clone(),
                    execution_instance: None,
                 handoff: None, },
            );
            match change {
                0 => {
                    if let ModelEdit::ChangeView {
                        change: Change::RemoveSource { registry, .. },
                        ..
                    } = &mut conditional
                    {
                        *registry = Uuid::new_v4();
                    }
                }
                1 => {
                    if let ModelEdit::ChangeView {
                        change: Change::RemoveSource { repository, .. },
                        ..
                    } = &mut conditional
                    {
                        *repository = "/foreign".into();
                    }
                }
                2 | 3 => {
                    let mut replacement = original.clone();
                    if change == 2 {
                        replacement.kind = pane().kind;
                    } else {
                        replacement.pending_terminal_start = true;
                    }
                    let update = edit(
                        &mut r,
                        a,
                        Change::Update {
                            before: original.clone(),
                            pane: replacement,
                        },
                    );
                    r.apply(&update).unwrap();
                }
                _ => {
                    let transfer = edit(
                        &mut r,
                        a,
                        Change::Transfer {
                            id: original.id,
                            destination: Scope::Channel(b),
                            before: None,
                        },
                    );
                    r.apply(&transfer).unwrap();
                }
            }
            // Rebase only the revision, reproducing the old unsafe worker.
            if let ModelEdit::ChangeView {
                expected_revision, ..
            } = &mut conditional
            {
                *expected_revision = r.view_revision(&Scope::Channel(a)).unwrap();
            }
            let before = r.model().unwrap();
            let operation = match &conditional {
                ModelEdit::ChangeView { operation, .. } => *operation,
                _ => unreachable!(),
            };
            assert!(r.apply(&conditional).is_err());
            assert_eq!(r.model().unwrap(), before);
            assert!(
                !r.view_operation_seen(operation).unwrap(),
                "refused source cannot consume operation receipt"
            );
            assert!(
                r.model()
                    .unwrap()
                    .views
                    .iter()
                    .any(|view| view.panes.iter().any(|pane| pane.id == original.id))
            );
        }
    }
    #[test]
    fn source_checked_remove_uses_transaction_revision_and_preserves_cli_remove_semantics() {
        let (mut r, _, a, _) = setup();
        let original = pane();
        seed(&mut r, a, vec![original.clone()]);
        let registry = r.model().unwrap().identity;
        let mut conditional = edit(
            &mut r,
            a,
            Change::RemoveSource {
                registry,
                repository: "/repo".into(),
                before: original.clone(),
                execution_instance: None,
             handoff: None, },
        );
        let mut titled = original.clone();
        titled.title = "independent display edit".into();
        let update = edit(
            &mut r,
            a,
            Change::Update {
                before: original.clone(),
                pane: titled,
            },
        );
        r.apply(&update).unwrap();
        let snapshot = r.model().unwrap();
        assert!(r.apply(&conditional).is_err());
        assert_eq!(r.model().unwrap(), snapshot);
        if let ModelEdit::ChangeView {
            expected_revision, ..
        } = &mut conditional
        {
            *expected_revision = r.view_revision(&Scope::Channel(a)).unwrap();
        }
        r.apply(&conditional).unwrap();
        let done = r.model().unwrap();
        r.apply(&conditional).unwrap();
        assert_eq!(r.model().unwrap(), done, "same operation is idempotent");
        let second = pane();
        seed(&mut r, a, vec![second.clone()]);
        let mut replacement = second.clone();
        replacement.kind = pane().kind;
        let update = edit(
            &mut r,
            a,
            Change::Update {
                before: second.clone(),
                pane: replacement,
            },
        );
        r.apply(&update).unwrap();
        let ordinary = edit(&mut r, a, Change::Remove { id: second.id });
        r.apply(&ordinary).unwrap();
        assert!(
            read(&r.db, &Scope::Channel(a)).unwrap().panes.is_empty(),
            "explicit CLI Remove retains its existing current-membership semantics"
        );
    }
}
