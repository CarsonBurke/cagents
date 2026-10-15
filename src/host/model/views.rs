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

/// Pane ownership and agent adoption must agree after both commit and restart.
/// Keep the original launch spec unchanged: it also identifies an idempotent
/// submission, whereas placement is the agent's current durable location.
fn transfer_agent(
    db: &Connection,
    pane: &crate::workspace_model::views::Pane,
    destination: &Scope,
    repository: &Path,
) -> Result<(), String> {
    let session = match &pane.kind {
        crate::model::CardKind::Terminal(session) => session.id,
        crate::model::CardKind::Chat(session) => session.id,
        _ => return Ok(()),
    };
    // The console and native presentation share their hosted session ID.
    // Use the unique session index instead of decoding every open agent.
    let record = db
        .query_row(
            "SELECT record FROM agents WHERE closed=0 AND session_id=?1",
            [session.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some(record) = record {
        let mut agent: crate::agent_execution::Agent =
            serde_json::from_str(&record).map_err(|error| error.to_string())?;
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
                    let pane = view
                        .panes
                        .iter()
                        .find(|pane| pane.id == *id)
                        .cloned()
                        .ok_or_else(|| format!("Pane {id} is not in the source view"))?;
                    let repository = project(transaction, scope)?;
                    if project(transaction, destination)? != repository {
                        return Err("Panes can only move between views of the same project".into());
                    }
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
    fn transfer_commits_agent_placement_with_pane_ownership_and_preserves_the_live_session() {
        use crate::{
            agent::Provider,
            agent_execution::{Destination, HostedSession, Origin, Spec, Submit, Surface, Target},
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
            assert_eq!(read(&r.db, &Scope::Channel(b)).unwrap().panes, vec![p]);
            assert!(read(&r.db, &Scope::Channel(a)).unwrap().panes.is_empty());
            assert!(!r.apply(&transfer).unwrap().changed);
            let replay = r.submit_agent(&Origin::User, &request, 2).unwrap();
            assert!(!replay.created);
            assert_eq!(replay.run.id, accepted.run.id);
            assert_eq!(replay.agent.placement, Scope::Channel(b));
        }
    }
}
