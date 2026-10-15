use super::*;
use crate::workspace_model::views::{Change, Scope, View};

pub(super) fn migrate(db: &Connection) -> Result<(), String> {
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let current: u32 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if current >= 8 {
        return tx.commit().map_err(|error| error.to_string());
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
    tx.execute_batch("PRAGMA user_version=8;")
        .map_err(|error| error.to_string())?;
    tx.commit().map_err(|error| error.to_string())
}
/// Private durable provenance for a managed terminal pane whose ended
/// presentation intentionally no longer contains a live attachment proof.
/// Older ended panes are never backfilled from a terminal ID or launch recipe.
pub(super) fn migrate_terminal_sources(db: &Connection) -> Result<(), String> {
    let tx = rusqlite::Transaction::new_unchecked(db, rusqlite::TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let current: u32 = tx
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(|error| error.to_string())?;
    if current < 13 {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS shared_view_terminal_sources(
            pane TEXT PRIMARY KEY REFERENCES shared_view_panes(id) ON DELETE CASCADE,
            source TEXT NOT NULL, ended TEXT NOT NULL
        ) STRICT; PRAGMA user_version=13;",
        )
        .map_err(|error| error.to_string())?;
    }
    tx.commit().map_err(|error| error.to_string())
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

/// Retire only the pane still presenting this execution agent's resource.
/// The current owner can differ from the agent's original placement after a
/// transfer. A replacement conversation in the same card belongs to its owner.
pub(super) fn retire_agent(
    db: &Connection,
    agent: &crate::agent_execution::Agent,
) -> Result<bool, String> {
    use crate::{
        agent_execution::HostedSession,
        model::{CardId, CardKind},
    };
    let id = CardId::from_uuid(agent.id.as_uuid());
    let Some(scope) = owner(db, id)? else {
        return Ok(false);
    };
    let mut view = read(db, &scope)?;
    let Some(pane) = view.panes.iter().find(|pane| pane.id == id) else {
        return Ok(false);
    };
    let current = match &pane.kind {
        CardKind::Chat(session) => HostedSession::Chat(session.clone()),
        CardKind::Terminal(session) => HostedSession::Terminal(session.clone()),
        _ => return Ok(false),
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
        return Ok(false);
    }
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
                    if destination == scope {
                        false
                    } else if !exists(transaction, destination)? {
                        false
                    } else if let Some(pane) =
                        view.panes.iter().find(|pane| pane.id == *id).cloned()
                    {
                        register(transaction, destination)?;
                        let mut target = read(transaction, destination)?;
                        target.retired.retain(|retired| retired != id);
                        target.apply(&Change::Insert {
                            pane,
                            before: *before,
                        })?;
                        target.revision = target
                            .revision
                            .checked_add(1)
                            .ok_or("view revision exhausted")?;
                        write(transaction, &target)?;
                        transaction
                            .execute(
                                "UPDATE shared_view_panes SET scope=?1 WHERE id=?2",
                                params![key(destination)?, id.to_string()],
                            )
                            .map_err(|error| error.to_string())?;
                        view.apply(&Change::Remove { id: *id })?
                    } else {
                        false
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
                Change::Update { pane, .. } if resource_owner(transaction, pane)?.is_some() => {
                    false
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
}
