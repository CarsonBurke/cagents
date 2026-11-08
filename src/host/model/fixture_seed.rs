//! Loads a fixture plan into an empty workspace-model store through the
//! registry's own edits: `OpenProject`, `CreateChannel`, `SeedView` and
//! `ChangeView`, exactly as windows and the CLI send them.
//!
//! Development builds only, while no daemon holds this instance's lease. See
//! docs/fixture-workspace.md.

use std::{path::Path, time::Instant};

use crate::{
    fixture_workspace::Plan,
    model::{CardId, ChannelTitleSource, ProjectKind},
    workspace_model::{
        ModelEdit,
        views::{Change, Pane, Scope},
    },
};

use super::{Registry, SCHEMA_VERSION, views};

/// What a seed wrote, as the single JSON line the generator reads.
#[derive(serde::Serialize)]
pub(crate) struct Seeded {
    pub(crate) path: std::path::PathBuf,
    pub(crate) identity: uuid::Uuid,
    pub(crate) schema: u32,
    pub(crate) projects: usize,
    pub(crate) channels: usize,
    pub(crate) views: usize,
    pub(crate) panes: usize,
    pub(crate) edits: u64,
    pub(crate) seconds: f64,
}

/// Seeds this instance's store. Refuses while a daemon runs, and refuses a
/// store that already exists: a seed never merges into real state.
pub(crate) fn seed(plan: &Path) -> Result<Seeded, String> {
    let plan = Plan::load(plan)?;
    let paths = crate::daemon::RuntimePaths::discover().map_err(|error| error.to_string())?;
    let _lease =
        crate::daemon::HostLease::acquire_for_seeding(&paths).map_err(|error| match error {
            crate::daemon::PlatformError::AlreadyRunning { path } => format!(
                "A cagents daemon is running for this instance ({}). Stop it before seeding: \
             `scripts/run-development --stop-instance NAME` for an instance, or stop the \
             native-evidence session.",
                path.display()
            ),
            error => error.to_string(),
        })?;
    let dirs = crate::instance::project_dirs().ok_or("the data directory is unavailable")?;
    let path = dirs.data_dir().join("workspace-model.sqlite3");
    refuse_existing(&path)?;
    seed_at(&plan, &path)
}

fn refuse_existing(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    let version = rusqlite::Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .and_then(|db| db.pragma_query_value(None, "user_version", |row| row.get::<_, u32>(0)))
    .map_or_else(
        |error| format!("unreadable: {error}"),
        |version| format!("schema {version}"),
    );
    Err(format!(
        "{} already exists ({version}; this executable writes schema {SCHEMA_VERSION}). A seed only \
         fills an empty instance. Seed a fresh fixture, or delete this instance's data directory \
         ({}) first.",
        path.display(),
        path.parent().unwrap_or(path).display()
    ))
}

/// Seeds the store at `path`, which must not exist yet.
pub(crate) fn seed_at(plan: &Plan, path: &Path) -> Result<Seeded, String> {
    let started = Instant::now();
    let mut registry = Registry::at(path)?;
    // Tens of thousands of single-edit transactions would otherwise each wait
    // for an fsync. Nothing reads the store until the seed exits, and the
    // checkpoint below makes the result durable, so the bytes are the same.
    registry
        .db
        .pragma_update(None, "synchronous", "OFF")
        .map_err(|error| error.to_string())?;
    for project in &plan.projects {
        registry.commit(
            &ModelEdit::OpenProject {
                repository: project.repository.clone(),
                name: project.name.clone(),
                kind: ProjectKind::Git,
            }
            .canonicalized()?,
        )?;
    }
    for channel in &plan.channels {
        registry.commit(&ModelEdit::CreateChannel {
            id: channel.id,
            repository: plan.project(channel.project)?.repository.clone(),
            title: channel.title.clone(),
            title_source: channel.title_source,
        })?;
    }
    let mut panes = 0;
    let mut edits = 0;
    for (index, view) in plan.views.iter().enumerate() {
        let scope = view.scope.scope().canonicalized()?;
        let mut seeded = Vec::new();
        for id in &view.panes {
            let card = plan.card(*id)?;
            let pane_scope = match &scope {
                Scope::Worktree { path, .. } => Some(path.clone()),
                Scope::Channel(_) => card.scope.clone(),
            };
            let pane = Pane::from_card(&plan.card_state(card)?, pane_scope)
                .ok_or_else(|| format!("Card {id} is closed or a utility and cannot be a pane"))?;
            seeded.push(pane);
        }
        panes += seeded.len();
        registry.commit(&ModelEdit::SeedView {
            scope: scope.clone(),
            panes: seeded,
            closed: view.retired.clone(),
        })?;
        edits += record_edits(&mut registry, plan, index, &scope, view)?;
    }
    // A channel's automatic name follows its primary conversation, so seeding
    // and rotating its panes cleared it. Name each one last, as the window
    // does once it has read the conversation, or as the person did.
    for channel in &plan.channels {
        if channel.title.is_empty() {
            continue;
        }
        registry.commit(&match channel.title_source {
            ChannelTitleSource::Manual => ModelEdit::RetitleChannel {
                id: channel.id,
                title: channel.title.clone(),
            },
            source => ModelEdit::SuggestChannelTitle {
                id: channel.id,
                title: channel.title.clone(),
                source,
            },
        })?;
    }
    registry
        .db
        .pragma_update(None, "synchronous", "FULL")
        .map_err(|error| error.to_string())?;
    registry
        .db
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()))
        .map_err(|error| error.to_string())?;
    let model = registry.model()?;
    Ok(Seeded {
        path: path.to_owned(),
        identity: model.identity,
        schema: SCHEMA_VERSION,
        projects: model.projects.len(),
        channels: model.channels.len(),
        views: model.views.len(),
        panes,
        edits,
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// Records `view.edits` operations on the view, leaving its panes as seeded.
///
/// A view of n panes takes rotations in whole cycles of n, each moving the
/// first pane to the end, so its revision grows as a busy view's does while
/// its order returns to the seeded one. The remainder, and every edit to an
/// empty view, are moves the registry records but that change nothing.
fn record_edits(
    registry: &mut Registry,
    plan: &Plan,
    index: usize,
    scope: &Scope,
    view: &crate::fixture_workspace::View,
) -> Result<u64, String> {
    let count = view.panes.len() as u64;
    let rotations = if count >= 2 {
        view.edits - view.edits % count
    } else {
        0
    };
    let order = views::read(&registry.db, scope)?
        .panes
        .iter()
        .map(|pane| pane.id)
        .collect::<Vec<_>>();
    let idle = order
        .first()
        .copied()
        .or_else(|| view.retired.first().copied())
        .unwrap_or_else(|| CardId::from_uuid(operation(plan, index, u64::MAX)));
    for number in 0..view.edits {
        let change = if number < rotations {
            Change::Move {
                id: order[(number % count) as usize],
                before: None,
            }
        } else {
            Change::Move {
                id: idle,
                before: Some(idle),
            }
        };
        registry.commit(&ModelEdit::ChangeView {
            scope: scope.clone(),
            expected_revision: views::read(&registry.db, scope)?.revision,
            operation: operation(plan, index, number),
            change,
        })?;
    }
    Ok(view.edits)
}

/// A stable operation ID per view and edit, so the same plan records the
/// same operations.
fn operation(plan: &Plan, view: usize, number: u64) -> uuid::Uuid {
    let mut bytes = plan.operation_namespace.into_bytes();
    for (byte, value) in bytes[..8].iter_mut().zip(number.to_le_bytes()) {
        *byte ^= value;
    }
    for (byte, value) in bytes[8..].iter_mut().zip((view as u64).to_le_bytes()) {
        *byte ^= value;
    }
    uuid::Builder::from_random_bytes(bytes).into_uuid()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture_workspace::test_support;

    /// The seeded store reopens through the registry with every project,
    /// channel and shared view the plan describes, and the history of edits
    /// it says each view recorded.
    #[test]
    fn small_round_trips_through_the_registry() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let path = directory.path().join("workspace-model.sqlite3");
        let seeded = seed_at(&plan, &path).unwrap();

        let registry = Registry::at(&path).unwrap();
        let model = registry.model().unwrap();
        assert_eq!(model.identity, seeded.identity, "the identity is durable");
        assert_eq!(model.projects.len(), plan.projects.len());
        assert_eq!(model.channels.len(), plan.channels.len());
        for (position, channel) in plan.channels.iter().enumerate() {
            let saved = model.channel(channel.id).unwrap();
            assert_eq!(saved.title, channel.title);
            assert_eq!(saved.title_source, channel.title_source);
            assert_eq!(saved.ordinal, position as u32 + 1);
        }
        assert_eq!(model.views.len(), plan.views.len());
        let mut panes = 0;
        for view in &plan.views {
            let scope = view.scope.scope().canonicalized().unwrap();
            let saved = model
                .views
                .iter()
                .find(|saved| saved.scope == scope)
                .unwrap_or_else(|| panic!("no view for {scope:?}"));
            assert_eq!(
                saved.panes.iter().map(|pane| pane.id).collect::<Vec<_>>(),
                view.panes,
                "whole rotations leave each view as seeded"
            );
            let mut retired = saved.retired.clone();
            let mut planned = view.retired.clone();
            retired.sort();
            planned.sort();
            assert_eq!(retired, planned);
            panes += saved.panes.len();
        }
        assert_eq!(panes, seeded.panes);
        let recorded: u64 = registry
            .db
            .query_row("SELECT COUNT(*) FROM shared_view_edits", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            recorded,
            plan.views.iter().map(|view| view.edits).sum::<u64>()
        );
        assert_eq!(recorded, seeded.edits);
    }

    #[test]
    fn a_seed_never_merges_into_an_existing_store() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("workspace-model.sqlite3");
        drop(Registry::at(&path).unwrap());
        let error = refuse_existing(&path).unwrap_err();
        assert!(
            error.contains(&format!("schema {SCHEMA_VERSION}")),
            "{error}"
        );
        assert!(
            error.contains("delete this instance's data directory"),
            "{error}"
        );
    }

    #[test]
    fn operations_are_stable_and_distinct() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        assert_eq!(operation(&plan, 3, 7), operation(&plan, 3, 7));
        let mut seen = std::collections::HashSet::new();
        for view in 0..20 {
            for number in 0..50 {
                assert!(seen.insert(operation(&plan, view, number)));
            }
        }
    }
}
