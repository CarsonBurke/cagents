//! Project relocation is one transaction; deleting/reopening a project would
//! cascade away its channels and shared pane identities.
use super::*;
use crate::project_recovery::Relocation;

pub(super) fn apply(
    tx: &rusqlite::Transaction<'_>,
    relocation: &Relocation,
    moves: &[super::agent_locations::AgentMove],
) -> Result<bool, String> {
    relocation.validate()?;
    let old = repository_key(&relocation.repository)?;
    let new = repository_key(&relocation.destination)?;
    let existing: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE repository=?1)",
            [old],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if !existing {
        return Ok(false);
    }
    if old == new {
        return relocate_agents(tx, relocation, moves);
    }
    let destination_exists: bool = tx
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM projects WHERE repository=?1)",
            [new],
            |row| row.get(0),
        )
        .map_err(|error| error.to_string())?;
    if destination_exists {
        return Err("The located folder is already open as another project".into());
    }
    tx.execute("INSERT INTO projects(repository,name,ordinal,kind) SELECT ?2,name,ordinal,kind FROM projects WHERE repository=?1", params![old,new]).map_err(|error| error.to_string())?;
    views::relocate(tx, relocation)?;
    tx.execute(
        "UPDATE channels SET repository=?2 WHERE repository=?1",
        params![old, new],
    )
    .map_err(|error| error.to_string())?;
    let navigation: Option<String> = tx
        .query_row(
            "SELECT items FROM navigation_orders WHERE repository=?1",
            [old],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some(navigation) = navigation {
        let mut items: Vec<crate::workspace_model::NavigationItem> =
            serde_json::from_str(&navigation).map_err(|error| error.to_string())?;
        for item in &mut items {
            if let crate::workspace_model::NavigationItem::Worktree(path) = item {
                relocation.path(path);
            }
        }
        tx.execute(
            "UPDATE navigation_orders SET repository=?2,items=?3 WHERE repository=?1",
            params![
                old,
                new,
                serde_json::to_string(&items).map_err(|error| error.to_string())?
            ],
        )
        .map_err(|error| error.to_string())?;
    }
    relocate_agents(tx, relocation, moves)?;
    // Automation definitions are immutable revisions with target leases. A
    // changed path needs a fresh user-approved revision, so pause their existing
    // definitions while retaining their history rather than executing old paths.
    automations::close_project(tx, old, crate::chat::now_ms().unwrap_or(0))?;
    tx.execute("DELETE FROM projects WHERE repository=?1", [old])
        .map_err(|error| error.to_string())?;
    tx.execute(
        "INSERT OR IGNORE INTO closed_projects(repository) VALUES (?1)",
        [old],
    )
    .map_err(|error| error.to_string())?;
    tx.execute("DELETE FROM closed_projects WHERE repository=?1", [new])
        .map_err(|error| error.to_string())?;
    Ok(true)
}

fn relocate_agents(
    tx: &rusqlite::Transaction<'_>,
    relocation: &Relocation,
    moves: &[super::agent_locations::AgentMove],
) -> Result<bool, String> {
    let agents = tx
        .prepare("SELECT id,record FROM agents")
        .map_err(|error| error.to_string())?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .and_then(Iterator::collect::<rusqlite::Result<Vec<_>>>)
        .map_err(|error| error.to_string())?;
    let mut changed = false;
    for (id, record) in agents {
        let mut agent: crate::agent_execution::Agent =
            serde_json::from_str(&record).map_err(|error| error.to_string())?;
        if !super::agent_locations::relocate(tx, &agent, relocation, moves)? {
            continue;
        }
        changed = true;
        relocation.agent(&mut agent);
        tx.execute(
            "UPDATE agents SET record=?2 WHERE id=?1",
            params![
                id,
                serde_json::to_string(&agent).map_err(|error| error.to_string())?
            ],
        )
        .map_err(|error| error.to_string())?;
        // agent_policies.directory stores the approved device/inode proof,
        // not a path. A same-filesystem move preserves that proof; locating a
        // replacement directory must never grant existing agents new authority.
    }
    Ok(changed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{CardId, CardKind, ProjectId, RepositoryView},
        workspace_model::views::{Pane, Scope},
    };

    #[test]
    fn locate_preserves_channels_and_shared_panes_atomically() {
        let directory = tempfile::tempdir().unwrap();
        let mut registry = Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
        let relocation = Relocation {
            repository: "/old/repo/.git".into(),
            destination: "/new/repo/.git".into(),
            old_root: "/old/repo".into(),
            new_root: "/new/repo".into(),
        };
        registry
            .apply(&ModelEdit::OpenProject {
                repository: relocation.repository.clone(),
                name: "Repo".into(),
                kind: ProjectKind::Git,
            })
            .unwrap();
        let channel = ChannelId::new();
        registry
            .apply(&ModelEdit::CreateChannel {
                id: channel,
                repository: relocation.repository.clone(),
                title: "Review".into(),
                title_source: Default::default(),
            })
            .unwrap();
        let card = CardId::new();
        let pane = Pane {
            id: card,
            title: "File".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            kind: CardKind::Repository(RepositoryView {
                project: ProjectId::new(),
                repository: relocation.repository.clone(),
                worktree: relocation.old_root.clone(),
                selected_file: Some("src/main.rs".into()),
            }),
            scope: Some(relocation.old_root.clone()),
        };
        let scope = Scope::Worktree {
            repository: relocation.repository.clone(),
            path: relocation.old_root.clone(),
        };
        registry
            .apply(&ModelEdit::SeedView {
                scope,
                panes: vec![pane],
                closed: vec![],
            })
            .unwrap();
        let before = registry.model().unwrap();
        let applied = registry
            .apply(&ModelEdit::RelocateProject {
                relocation: relocation.clone(),
            })
            .unwrap();
        assert!(applied.changed);
        let after = &applied.model;
        assert_eq!(after.projects.len(), 1);
        assert_eq!(after.projects[0].repository, relocation.destination);
        assert_eq!(after.projects[0].ordinal, before.projects[0].ordinal);
        assert_eq!(after.channels[0].id, channel);
        assert_eq!(after.channels[0].repository, relocation.destination);
        assert_eq!(after.views[0].panes[0].id, card);
        assert_eq!(
            after.views[0].panes[0].scope.as_ref(),
            Some(&relocation.new_root)
        );
        assert!(after.views[0].revision > before.views[0].revision);
        assert_eq!(
            after.views[0].scope,
            Scope::Worktree {
                repository: relocation.destination.clone(),
                path: relocation.new_root.clone()
            }
        );
        let CardKind::Repository(view) = &after.views[0].panes[0].kind else {
            panic!()
        };
        assert_eq!(view.worktree, relocation.new_root);
        assert_eq!(
            view.selected_file.as_deref(),
            Some(Path::new("src/main.rs"))
        );
        assert!(
            !registry
                .apply(&ModelEdit::RelocateProject { relocation })
                .unwrap()
                .changed
        );
        drop(registry);
        let reopened = Registry::at(&directory.path().join("registry.sqlite3"))
            .unwrap()
            .model()
            .unwrap();
        assert_eq!(reopened.projects, after.projects);
        assert_eq!(reopened.views, after.views);
    }
}
