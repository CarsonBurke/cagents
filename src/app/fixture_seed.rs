//! Loads a fixture plan into one window's empty session store through its
//! `StateStore`, as the window itself saves projects, worktrees, the
//! workspace snapshot and the window view.
//!
//! Development builds only, while no window holds the slot. See
//! docs/fixture-workspace.md.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    time::Instant,
};

use crate::{
    fixture_workspace::Plan,
    model::{
        Existence, ProjectChannel, ProjectKind, ProjectState, Tombstone, WorkspaceDestination,
        WorkspaceState, WorktreeState,
    },
    persistence::{CURRENT_SCHEMA_VERSION, SnapshotBase, StateStore},
};

use super::{ContextTab, WindowView};

#[derive(serde::Serialize)]
pub(crate) struct SeededWindow {
    pub(crate) path: std::path::PathBuf,
    pub(crate) slot: String,
    pub(crate) schema: u32,
    pub(crate) cards: usize,
    pub(crate) open: usize,
    pub(crate) channels: usize,
    pub(crate) projects: usize,
    pub(crate) seconds: f64,
}

/// Seeds the `slot` window of this instance. `identity` is the workspace
/// model the daemon was seeded with; with it the window's channels are known
/// to be shared, and without it they are this window's own.
pub(crate) fn seed(
    plan: &Path,
    slot: &str,
    identity: Option<uuid::Uuid>,
) -> Result<SeededWindow, String> {
    let plan = Plan::load(plan)?;
    let _lease = crate::instance::WindowLease::acquire(false, Some(slot)).map_err(|error| {
        format!(
            "Cannot take the {slot} window's lease ({error}). Close that cagents window, or stop the \
             native-evidence session, before seeding."
        )
    })?;
    let path = crate::instance::window_database_path(slot).map_err(|error| error.to_string())?;
    refuse_existing(&path)?;
    let boot = crate::session::current_boot_id().map_err(|error| error.to_string())?;
    let store = StateStore::open(&path, boot).map_err(|error| error.to_string())?;
    let mut seeded = seed_store(&plan, slot, identity, &store)?;
    seeded.path = path;
    copy_avatars(&plan)?;
    Ok(seeded)
}

fn refuse_existing(path: &Path) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    Err(format!(
        "{} already exists. A seed only fills an empty window (this executable writes session schema \
         {CURRENT_SCHEMA_VERSION}). Seed a fresh fixture, or delete this instance's data directory ({}) \
         first.",
        path.display(),
        path.parent().unwrap_or(path).display()
    ))
}

/// Writes the window `slot` describes into `store`.
pub(crate) fn seed_store(
    plan: &Plan,
    slot: &str,
    identity: Option<uuid::Uuid>,
    store: &StateStore,
) -> Result<SeededWindow, String> {
    let started = Instant::now();
    let window = plan
        .windows
        .iter()
        .find(|window| window.slot == slot)
        .ok_or_else(|| format!("The plan has no {slot} window"))?;
    let error = |error: crate::persistence::StoreError| error.to_string();
    for project in &plan.projects {
        store
            .save_project(&ProjectState {
                id: project.id,
                generation: 0,
                name: project.name.clone(),
                repository: project.repository.clone(),
                kind: ProjectKind::Git,
                existence: Existence::Open,
            })
            .map_err(error)?;
        for worktree in &project.worktrees {
            store
                .save_worktree(&WorktreeState {
                    id: worktree.id,
                    project_id: project.id,
                    generation: 0,
                    path: worktree.path.clone(),
                    branch: Some(worktree.branch.clone()),
                    existence: if worktree.removed {
                        Existence::Closed {
                            tombstone: Tombstone {
                                operation_id: None,
                                closed_at_unix_ms: plan.epoch * 1000,
                                reason: Some("Worktree removed".into()),
                            },
                        }
                    } else {
                        Existence::Open
                    },
                })
                .map_err(error)?;
        }
    }
    let cards = window
        .cards
        .iter()
        .map(|id| plan.card_state(plan.card(*id)?))
        .collect::<Result<Vec<_>, _>>()?;
    let held: HashSet<_> = window.cards.iter().copied().collect();
    let mut state = WorkspaceState {
        id: window.workspace_id,
        project_order: plan.projects.iter().map(|project| project.id).collect(),
        channels: plan
            .channels
            .iter()
            .enumerate()
            .map(|(position, channel)| ProjectChannel {
                id: channel.id,
                project_id: channel.project,
                title: channel.title.clone(),
                title_source: channel.title_source,
                // The registry ranks channels in the order they were created.
                ordinal: position as u32 + 1,
                shared: identity.is_some(),
                cards: channel
                    .cards
                    .iter()
                    .copied()
                    .filter(|id| held.contains(id))
                    .collect(),
                focused: channel.focused.filter(|id| held.contains(id)),
            })
            .collect(),
        active_channel: window.active_channel,
        active_worktree: window.active_worktree.clone(),
        focused: window.focused,
        worktree_focus: window
            .worktree_focus
            .iter()
            .filter(|(_, card)| held.contains(card))
            .map(|(path, card)| (path.clone(), *card))
            .collect(),
        card_scopes: window
            .card_scopes
            .iter()
            .map(|id| {
                let card = plan.card(*id)?;
                card.scope
                    .clone()
                    .map(|scope| (*id, scope))
                    .ok_or_else(|| format!("Card {id} has a scope entry but no scope"))
            })
            .collect::<Result<HashMap<_, _>, String>>()?,
        chat_drafts: window
            .drafts
            .iter()
            .map(|(card, text)| (*card, text.clone()))
            .collect(),
        agent_updates: window
            .unread
            .iter()
            .map(|unread| (unread.card, unread.owner))
            .collect::<BTreeMap<_, _>>(),
        workspace_history: window
            .active_channel
            .map(WorkspaceDestination::Channel)
            .into_iter()
            .chain(
                plan.projects
                    .iter()
                    .take(3)
                    .map(|project| WorkspaceDestination::Worktree(project.worktrees[0].id)),
            )
            .collect(),
        pending_chat_admissions: window
            .pending_chat_admissions
            .iter()
            .filter_map(|id| {
                cards
                    .iter()
                    .find(|card| card.id == *id)
                    .and_then(|card| match &card.kind {
                        crate::model::CardKind::Chat(session) => Some((*id, session.clone())),
                        _ => None,
                    })
            })
            .collect(),
        workspace_model_identity: identity,
        cards,
        ..WorkspaceState::default()
    };
    state.normalize_channels();
    store
        .save_workspace(
            &state,
            SnapshotBase {
                workspace: 0,
                cards: &BTreeMap::new(),
            },
        )
        .map_err(error)?;
    let view = WindowView {
        user_titles: window
            .user_titles
            .iter()
            .map(|(card, title)| (*card, title.clone()))
            .collect(),
        projects_sidebar_collapsed: false,
        sidebar_width: crate::layout_metrics::NAVIGATION_DEFAULT_WIDTH,
        inspector_width: crate::layout_metrics::INSPECTOR_DEFAULT_WIDTH,
        context_inspector_open: false,
        context_tab: ContextTab::default(),
        // The sidebar keeps its interaction state private; it reads back what
        // it saved, so the seed writes that shape.
        sidebar: serde_json::from_value(serde_json::json!({
            "query": "",
            "controls_expanded": false,
            "collapsed_projects": window.collapsed_projects,
            "selection": null,
        }))
        .map_err(|error| format!("The sidebar's saved view changed shape: {error}"))?,
        worktree_offsets: window
            .worktree_offsets
            .iter()
            .map(|(path, offset)| (path.clone(), *offset))
            .collect(),
    };
    store
        .save_window_view(&serde_json::to_string(&view).map_err(|error| error.to_string())?)
        .map_err(error)?;
    Ok(SeededWindow {
        path: Default::default(),
        slot: slot.to_owned(),
        schema: store.schema_version().map_err(error)?,
        open: state
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .count(),
        cards: state.cards.len(),
        channels: state.channels.len(),
        projects: plan.projects.len(),
        seconds: started.elapsed().as_secs_f64(),
    })
}

/// Puts each project's avatar where the app caches GitHub avatars, fresh, so
/// the sidebar draws it without a download.
fn copy_avatars(plan: &Plan) -> Result<(), String> {
    let Some(dirs) = crate::instance::project_dirs() else {
        return Ok(());
    };
    let cache = dirs.cache_dir().join("github-avatars");
    for project in &plan.projects {
        let Some(avatar) = &project.avatar else {
            continue;
        };
        std::fs::create_dir_all(&cache).map_err(|error| error.to_string())?;
        let source = plan
            .root
            .join("avatars")
            .join(format!("{}.png", avatar.owner));
        std::fs::copy(
            &source,
            cache.join(format!("github.com-{}.image", avatar.owner)),
        )
        .map_err(|error| format!("Cannot cache the avatar {}: {error}", source.display()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{fixture_workspace::test_support, model::BootId};

    /// Each seeded window reopens through the loaders a starting window uses:
    /// its projects and worktrees, its workspace with every card decoding,
    /// and a window view the window parses.
    #[test]
    fn small_round_trips_through_the_session_store() {
        let directory = tempfile::tempdir().unwrap();
        let plan = test_support::small(directory.path());
        let identity = uuid::Uuid::new_v4();
        for window in &plan.windows {
            let path = directory.path().join(format!("{}.sqlite3", window.slot));
            {
                let store = StateStore::open(&path, BootId::new("seed")).unwrap();
                let seeded = seed_store(&plan, &window.slot, Some(identity), &store).unwrap();
                assert_eq!(seeded.cards, window.cards.len());
                store.shutdown().unwrap();
            }
            let store = StateStore::open(&path, BootId::new("next-boot")).unwrap();
            assert_eq!(store.list_projects().unwrap().len(), plan.projects.len());
            let worktrees: usize = plan.projects.iter().map(|p| p.worktrees.len()).sum();
            assert_eq!(store.list_worktrees().unwrap().len(), worktrees);
            let summaries = store.list_workspaces().unwrap();
            assert_eq!(
                summaries.iter().map(|s| s.id).collect::<Vec<_>>(),
                [window.workspace_id]
            );
            let restored = store
                .load_workspace_restored(window.workspace_id, None)
                .unwrap()
                .unwrap();
            assert_eq!(restored.warnings, Vec::<String>::new(), "{}", window.slot);
            let state = restored.state;
            assert_eq!(state.cards.len(), window.cards.len(), "{}", window.slot);
            let open = window
                .cards
                .iter()
                .filter(|id| plan.card(**id).unwrap().closed_at_ms.is_none())
                .count();
            assert_eq!(
                state
                    .cards
                    .iter()
                    .filter(|card| card.existence.is_open())
                    .count(),
                open
            );
            assert_eq!(state.workspace_model_identity, Some(identity));
            assert_eq!(state.channels.len(), plan.channels.len());
            assert!(state.channels.iter().all(|channel| channel.shared));
            assert_eq!(state.agent_updates.len(), window.unread.len());
            assert_eq!(state.focused, window.focused);
            assert_eq!(state.chat_drafts.len(), window.drafts.len());
            let view: WindowView =
                serde_json::from_str(&store.load_window_view().unwrap().unwrap()).unwrap();
            assert_eq!(view.user_titles.len(), window.user_titles.len());
        }
    }

    #[test]
    fn a_seed_never_merges_into_an_existing_session() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("session.sqlite3");
        drop(StateStore::open(&path, BootId::new("seed")).unwrap());
        let error = refuse_existing(&path).unwrap_err();
        assert!(error.contains("already exists"), "{error}");
    }
}
