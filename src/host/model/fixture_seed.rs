//! Loads a fixture plan into an empty workspace-model store through the
//! registry's own edits: `OpenProject`, `CreateChannel`, `SeedView` and
//! `ChangeView`, exactly as windows and the CLI send them.
//!
//! Development builds only, while no daemon holds this instance's lease. See
//! docs/fixture-workspace.md.

use std::{path::Path, time::Instant};

use crate::{
    fixture_workspace::{
        Plan,
        destination::{Directories, Instance, Part},
    },
    model::{CardId, ChannelTitleSource, ProjectKind},
    workspace_model::{
        ModelEdit,
        views::{Change, Pane, Scope},
    },
};

use super::{Registry, SCHEMA_VERSION, views};

/// What an ID [`derived`] from the plan stands for.
const OPERATION: u8 = 0;
const IDLE_CARD: u8 = 1;

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

/// Seeds this instance's store. Refuses while a daemon runs, and refuses an
/// instance that holds anything an earlier seed of this plan did not write:
/// a seed never merges into real state.
pub(crate) fn seed(plan: &Path) -> Result<Seeded, String> {
    let plan = Plan::load(plan)?;
    let instance = Instance::admit(&plan, Directories::current()?)?;
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
    let destination = instance.claim(&plan, Part::Daemon)?;
    let path = destination.store();
    let mut seeded = destination.write(|staging| seed_at(&plan, staging))?;
    seeded.path = path;
    Ok(seeded)
}

/// Seeds the store at `path`, which must not exist yet, and closes it.
pub(crate) fn seed_at(plan: &Plan, path: &Path) -> Result<Seeded, String> {
    let started = Instant::now();
    let mut registry = Registry::at(path)?;
    // The seed holds the store alone, under the instance's lease, so its
    // schema cannot change between edits: check it once here rather than in
    // each of tens of thousands of transactions.
    super::schema::require_current(&registry.db)?;
    // Tens of thousands of single-edit transactions would otherwise each wait
    // for an fsync. Nothing reads the store until the seed exits, and the
    // checkpoint below makes the result durable, so the bytes are the same.
    registry
        .db
        .pragma_update(None, "synchronous", "OFF")
        .map_err(|error| error.to_string())?;
    for project in &plan.projects {
        seed_commit(
            &mut registry,
            &ModelEdit::OpenProject {
                repository: project.repository.clone(),
                name: project.name.clone(),
                kind: ProjectKind::Git,
            }
            .canonicalized()?,
        )?;
    }
    for channel in &plan.channels {
        seed_commit(
            &mut registry,
            &ModelEdit::CreateChannel {
                id: channel.id,
                repository: plan.project(channel.project)?.repository.clone(),
                title: channel.title.clone(),
                title_source: channel.title_source,
            },
        )?;
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
        seed_commit(
            &mut registry,
            &ModelEdit::SeedView {
                scope: scope.clone(),
                panes: seeded,
                closed: view.retired.clone(),
            },
        )?;
        edits += record_edits(&mut registry, plan, index, &scope, view)?;
    }
    let planned: u64 = plan.views.iter().map(|view| view.edits).sum();
    if edits != planned {
        return Err(format!(
            "The registry recorded {edits} of the plan's {planned} view edits"
        ));
    }
    // A channel's automatic name follows its primary conversation, so seeding
    // and rotating its panes cleared it. Name each one last, as the window
    // does once it has read the conversation, or as the person did.
    for channel in &plan.channels {
        if channel.title.is_empty() {
            continue;
        }
        seed_commit(
            &mut registry,
            &match channel.title_source {
                ChannelTitleSource::Manual => ModelEdit::RetitleChannel {
                    id: channel.id,
                    title: channel.title.clone(),
                },
                source => ModelEdit::SuggestChannelTitle {
                    id: channel.id,
                    title: channel.title.clone(),
                    source,
                },
            },
        )?;
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

fn seed_commit(registry: &mut Registry, edit: &ModelEdit) -> Result<(u64, bool), String> {
    registry.commit_checked(edit, false)
}

/// Records `view.edits` operations on the view, leaving its panes as seeded,
/// and returns how many the registry recorded.
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
        .map(Ok)
        .unwrap_or_else(|| derived(plan, IDLE_CARD, index, 0).map(CardId::from_uuid))?;
    let before = recorded(registry)?;
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
        let expected_revision = views::read(&registry.db, scope)?.revision;
        seed_commit(
            registry,
            &ModelEdit::ChangeView {
                scope: scope.clone(),
                expected_revision,
                operation: derived(plan, OPERATION, index, number)?,
                change,
            },
        )?;
    }
    Ok(recorded(registry)? - before)
}

fn recorded(registry: &Registry) -> Result<u64, String> {
    registry
        .db
        .query_row("SELECT COUNT(*) FROM shared_view_edits", [], |row| {
            row.get(0)
        })
        .map_err(|error| error.to_string())
}

/// A stable ID for the `number`th thing of `kind` in view `view`, so the same
/// plan records the same operations. The registry ignores an operation it has
/// seen, so no two may share one.
///
/// Bytes 0..6 carry `number` and bytes 9..16 `view`, each XORed with the
/// plan's namespace, and byte 7 carries `kind`. Neither touches the version
/// (the top of byte 6) or variant (the top of byte 8) bits the builder sets,
/// so distinct inputs give distinct IDs.
fn derived(plan: &Plan, kind: u8, view: usize, number: u64) -> Result<uuid::Uuid, String> {
    if number >= 1 << 48 || view as u64 >= 1 << 56 {
        return Err(format!(
            "View {view}'s edit {number} is past what a seeded operation ID can number"
        ));
    }
    let mut bytes = plan.operation_namespace.into_bytes();
    for (byte, value) in bytes[..6].iter_mut().zip(number.to_le_bytes()) {
        *byte ^= value;
    }
    bytes[7] ^= kind;
    for (byte, value) in bytes[9..].iter_mut().zip((view as u64).to_le_bytes()) {
        *byte ^= value;
    }
    Ok(uuid::Builder::from_random_bytes(bytes).into_uuid())
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
        // Through the destination a seed claims, which renames the closed
        // store into place.
        let destination = Instance::admit(
            &plan,
            Directories {
                data: plan.root.join("data"),
                config: plan.root.join("config"),
                cache: plan.root.join("cache"),
                runtime: Some(plan.root.join("runtime")),
                instance: None,
            },
        )
        .and_then(|instance| instance.claim(&plan, Part::Daemon))
        .unwrap();
        let path = destination.store();
        let seeded = destination
            .write(|staging| seed_at(&plan, staging))
            .unwrap();
        assert!(!destination_staging(&path).exists());

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

    fn destination_staging(store: &Path) -> std::path::PathBuf {
        store.with_file_name("workspace-model.sqlite3.seeding")
    }

    /// The registry ignores an operation it has seen, so a repeated ID would
    /// silently drop an edit. Views past 64 and 128 once collided.
    #[test]
    fn derived_ids_are_stable_and_distinct() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let id = |kind, view, number| derived(&plan, kind, view, number).unwrap();
        assert_eq!(id(OPERATION, 3, 7), id(OPERATION, 3, 7));
        assert_ne!(id(OPERATION, 0, 5), id(OPERATION, 64, 5));
        assert_ne!(id(OPERATION, 1, 5), id(OPERATION, 129, 5));
        let mut seen = std::collections::HashSet::new();
        for view in (0..300).chain([1 << 20, (1 << 56) - 1]) {
            for number in (0..200).chain([1 << 40, (1 << 48) - 1]) {
                assert!(seen.insert(id(OPERATION, view, number)), "{view} {number}");
            }
            assert!(seen.insert(id(IDLE_CARD, view, 0)), "idle {view}");
        }
        assert!(derived(&plan, OPERATION, 0, 1 << 48).is_err());
    }
}
