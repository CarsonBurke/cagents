#[test]
fn the_accepted_sidebar_width_is_exactly_the_width_the_pane_renders() {
    use crate::sidebar::{SidebarMode, effective_width};
    use crate::{
        layout_metrics::{
            NAVIGATION_MAX_WIDTH as EXPANDED_WIDTH_MAX, NAVIGATION_MIN_WIDTH as EXPANDED_WIDTH_MIN,
        },
        settings::{AppSettings, SettingsError},
    };

    let with_width = |sidebar_width| AppSettings {
        sidebar_width,
        ..AppSettings::default()
    };

    // A width outside the pane's bounds used to validate and then render
    // clamped, so the file said 200 while the sidebar showed 240.
    for width in [
        EXPANDED_WIDTH_MIN - 40.0,
        EXPANDED_WIDTH_MIN - 0.5,
        EXPANDED_WIDTH_MAX + 0.5,
        f32::NAN,
    ] {
        assert!(
            matches!(
                with_width(width).validate(),
                Err(SettingsError::SidebarWidth { .. })
            ),
            "a width of {width} cannot be persisted: the pane will not render it"
        );
    }

    for width in [
        EXPANDED_WIDTH_MIN,
        AppSettings::default().sidebar_width,
        EXPANDED_WIDTH_MAX,
    ] {
        let accepted = with_width(width)
            .validate()
            .expect("a width the pane renders must validate");
        assert_eq!(
            effective_width(SidebarMode::Expanded, accepted.sidebar_width),
            width
        );
    }

    let message = SettingsError::SidebarWidth {
        min: EXPANDED_WIDTH_MIN,
        max: EXPANDED_WIDTH_MAX,
    }
    .to_string();
    assert!(
        message.contains(&EXPANDED_WIDTH_MIN.to_string())
            && message.contains(&EXPANDED_WIDTH_MAX.to_string()),
        "the rejection must quote the bounds it enforces, not a copy of them: {message}"
    );
}

#[test]
fn project_and_worktree_menu_creation_labels_match_their_scope() {
    assert_eq!(super::project_channel_command_label(None), "New channel");
    assert_eq!(
        super::project_channel_command_label(Some(crate::model::WorktreeId::new())),
        "New thread"
    );
}

#[test]
fn inspector_header_exposes_an_explicit_copy_branch_action() {
    let renderer = headless_renderer();
    let row = worktree(11, "feature/auth-fix", false);
    let mut source = source_control();
    source.key.worktree_id = row.id;
    let state = SidebarState::default();
    let actions = frame_enter_presses(
        &renderer,
        source_control_header(&state, &source, Some(&row), false, PanelView::Files),
    );
    assert!(actions.iter().any(|action| matches!(action,
        SidebarAction::ProjectCommand { worktree_id: Some(id), command: ProjectCommand::CopyBranch, .. } if *id == row.id)));
    assert_eq!(row.label(), row.branch.as_deref().unwrap());
}

#[test]
fn prefix_worktree_search_ranks_above_a_loose_channel_in_visible_navigation() {
    let mut project = project();
    let matching = worktree(60, "feature login", false);
    let row = SelectionRow::Worktree(project.id, matching.id);
    project.worktrees.push(matching);
    project.channels.push(SidebarChannel::new(
        ChannelId::new(),
        "Investigate task in alpha-wt-feat".into(),
        false,
        None,
        false,
        1,
        true,
        1,
    ));
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![project]);
    let mut state = expanded_worktree_state();
    state.query = "feat".into();
    let rows = visible_selection_rows(&model, &state, false);
    assert_eq!(rows.get(1), Some(&row));
}

#[test]
fn literal_branch_labels_preserve_prefixes_and_punctuation() {
    for branch in [
        "feature/auth-fix",
        "bugfix/auth-fix",
        "feat-z",
        "snake_case",
    ] {
        assert_eq!(
            worktree_display_name(Path::new("/repo/worktree-dir"), Some(branch), false),
            branch
        );
    }
    assert_eq!(
        worktree_display_name(
            Path::new("/repo/detached-dir"),
            Some("ignored/branch"),
            true
        ),
        "detached-dir"
    );
    assert_eq!(
        worktree_display_name(Path::new("/repo/no-branch"), None, false),
        "no-branch"
    );
}

#[test]
fn compact_attention_keeps_failed_news_and_unread_count_beside_independent_live_activity() {
    use crate::agent::{Activity, ActivityState, Provider};
    let project = project();
    let worktree = project.worktrees[0].id;
    let mut state = SidebarState::default();
    let failed = CardId::new();
    let working = CardId::new();
    let waiting = CardId::new();
    for card in [failed, working, waiting] {
        state.observe_agent_update(card, project.id, worktree);
    }
    state.set_failed_updates(BTreeSet::from([failed]));
    state.set_agent_activity(
        working,
        project.id,
        worktree,
        Activity {
            provider: Provider::Codex,
            state: ActivityState::Working,
        },
    );
    assert_eq!(
        state.card_attention(failed).state(),
        Some(AttentionState::Failed)
    );
    assert_eq!(
        state.worktree_attention(project.id, worktree).state(),
        Some(AttentionState::Failed)
    );
    assert_eq!(state.project_attention(project.id).unread, 3);
    assert_eq!(
        state.project_attention(project.id).state(),
        Some(AttentionState::Failed)
    );
    let channel = SidebarChannel::new(
        ChannelId::new(),
        "Mixed".into(),
        false,
        Some(ActivityState::Working),
        true,
        3,
        true,
        0,
    )
    .with_attention(state.project_attention(project.id));
    assert_eq!(
        channel.attention.state(),
        state.worktree_attention(project.id, worktree).state()
    );
    let names = accessible_names(
        &headless_renderer(),
        compact_project_identity(
            &project,
            false,
            false,
            &state,
            Some(state.project_attention(project.id)),
        ),
    );
    assert!(
        names
            .iter()
            .any(|name| name.contains("3 unread conversations")
                && name.contains("unread failed updates")),
        "{names:?}"
    );
    let prefix = format!("{}:", project.worktrees[0].label());
    assert!(
        names.iter().any(|name| name
            .lines()
            .any(|line| line.starts_with(&prefix) && line.contains("unread failed updates"))),
        "{names:?}"
    );
    state.set_agent_activity(
        waiting,
        project.id,
        worktree,
        Activity {
            provider: Provider::Claude,
            state: ActivityState::Waiting,
        },
    );
    assert_eq!(
        state.project_attention(project.id).state(),
        Some(AttentionState::Waiting)
    );
    assert_eq!(
        state.worktree_attention(project.id, worktree).state(),
        Some(AttentionState::Waiting)
    );
    state.acknowledge_agent_update(failed, &AttentionPlacement::default());
    assert_eq!(state.project_attention(project.id).failed, 0);
    assert_eq!(state.project_attention(project.id).unread, 2);
    assert_eq!(
        state.card_activity(working).unwrap().state,
        ActivityState::Working
    );
    assert_eq!(
        state.card_activity(waiting).unwrap().state,
        ActivityState::Waiting
    );
    state.clear_agent_activity(waiting);
    assert_eq!(
        state.project_attention(project.id).state(),
        Some(AttentionState::Working)
    );
    // A usage limit stops the working agent: it no longer counts as
    // working, and a later activity report does not lift the limit.
    state.set_card_limited(working, true);
    let limited = state.worktree_attention(project.id, worktree);
    assert_eq!(limited.state(), Some(AttentionState::Limited));
    assert_eq!((limited.working, limited.limited), (0, 1));
    assert_eq!(state.project_attention(project.id).limited, 1);
    state.set_agent_activity(
        working,
        project.id,
        worktree,
        Activity {
            provider: Provider::Claude,
            state: ActivityState::Idle,
        },
    );
    assert_eq!(state.project_attention(project.id).limited, 1);
    state.set_card_limited(working, false);
    assert_eq!(state.project_attention(project.id).limited, 0);
    state.set_card_limited(working, true);
    state.clear_agent_activity(working);
    assert_eq!(state.project_attention(project.id).limited, 0);
    state.set_card_limited(working, true);
    assert_eq!(
        state.project_attention(project.id).limited,
        0,
        "a pane reporting no activity has no agent to stop"
    );
    state.set_agent_activity(
        working,
        project.id,
        worktree,
        Activity {
            provider: Provider::Claude,
            state: ActivityState::Working,
        },
    );
    state.restore_unread_updates(BTreeMap::from([(working, Some((project.id, worktree)))]));
    assert_eq!(state.worktree_attention(project.id, worktree).unread, 1);
    assert_eq!(state.project_attention(project.id).unread, 1);
}

fn compact_attention_terminal(id: CardId) -> crate::model::CardState {
    let launch = crate::model::LaunchRecipe::shell("/repo".into()).unwrap();
    crate::model::CardState {
        id,
        generation: 0,
        title: "Agent".into(),
        width_fraction: 0.5,
        existence: crate::model::Existence::Open,
        kind: crate::model::CardKind::Terminal(crate::model::TerminalSession {
            id: crate::model::TerminalId::new(),
            launch: launch.clone(),
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
            transcript_path: None,
            resume_hint: None,
            continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
        }),
    }
}

#[test]
fn compact_attention_projection_counts_open_logical_conversations_once_and_reuses_unchanged_metadata()
 {
    let project = project();
    let other = {
        let mut other = project.clone();
        other.id = project_id(99);
        other
    };
    let first = compact_attention_terminal(CardId::new());
    let mut alias = first.clone();
    alias.id = CardId::new();
    let mut closed = compact_attention_terminal(CardId::new());
    closed.existence = crate::model::Existence::Closed {
        tombstone: crate::model::Tombstone {
            operation_id: None,
            closed_at_unix_ms: 0,
            reason: Some("closed".into()),
        },
    };
    let channel = crate::model::ProjectChannel {
        title_source: Default::default(),
        id: ChannelId::new(),
        project_id: project.id,
        title: "Mixed".into(),
        ordinal: 0,
        shared: false,
        cards: vec![first.id, alias.id, closed.id],
        focused: None,
    };
    let mut durable = Arc::new(crate::model::WorkspaceState {
        cards: vec![first.clone(), alias.clone(), closed.clone()].into(),
        channels: vec![channel],
        ..Default::default()
    });
    let mut state = SidebarState::default();
    for card in [first.id, alias.id, closed.id] {
        state.observe_card_update(card);
    }
    state.set_failed_updates(BTreeSet::from([first.id]));
    let projects = vec![project.clone(), other.clone()];
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.project_attention(project.id).unread, 1);
    assert_eq!(state.project_attention(project.id).failed, 1);
    let rebuilt = state.attention_projection_rebuilds;
    for _ in 0..20 {
        state.refresh_attention_projection(&durable, &projects);
    }
    assert_eq!(state.attention_projection_rebuilds, rebuilt);
    state.acknowledge_agent_update(first.id, &AttentionPlacement::default());
    state.acknowledge_agent_update(alias.id, &AttentionPlacement::default());
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.project_attention(project.id).unread, 0);
    assert_eq!(state.attention_projection_rebuilds, rebuilt + 1);
    state.observe_card_update(first.id);
    Arc::make_mut(&mut durable).channels[0].project_id = other.id;
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.project_attention(project.id).unread, 0);
    assert_eq!(state.project_attention(other.id).unread, 1);
    // Conflicting logical conversation filing is unavailable, not first-project routing.
    let mut conflict = durable.channels[0].clone();
    conflict.id = ChannelId::new();
    conflict.project_id = project.id;
    Arc::make_mut(&mut durable).channels.push(conflict);
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.project_attention(other.id).unread, 0);
    assert_eq!(state.project_attention(project.id).unread, 0);
}

#[test]
fn attention_cache_refiles_unchanged_workspace_when_root_inventory_changes() {
    let mut projects = vec![project()];
    let project = projects[0].id;
    let original = projects[0].worktrees[1].clone();
    let card = compact_attention_terminal(CardId::new());
    let durable = Arc::new(crate::model::WorkspaceState {
        card_scopes: std::collections::HashMap::from([(card.id, original.path.clone())]),
        cards: vec![card.clone()].into(),
        ..Default::default()
    });
    let mut state = SidebarState::default();
    state.observe_card_update(card.id);
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.worktree_attention(project, original.id).unread, 1);
    let rebuilt = state.attention_projection_rebuilds;
    projects[0].name = "Display metadata changed".into();
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.attention_projection_rebuilds, rebuilt);

    projects[0].worktrees[1].path = "/different-root".into();
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.project_attention(project).unread, 0);
    projects[0].worktrees[1].path = original.path.clone();
    let replacement = WorktreeId::new();
    projects[0].worktrees[1].id = replacement;
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.worktree_attention(project, original.id).unread, 0);
    assert_eq!(state.worktree_attention(project, replacement).unread, 1);

    let rebuilt = state.attention_projection_rebuilds;
    projects[0].worktrees.swap(0, 1);
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.attention_projection_rebuilds, rebuilt + 1);
    assert_eq!(state.worktree_attention(project, replacement).unread, 1);
    let removed = projects[0].worktrees.remove(0);
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.project_attention(project).unread, 0);
    projects[0].worktrees.insert(0, removed);
    let renamed_project = project_id(98);
    projects[0].id = renamed_project;
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.project_attention(project).unread, 0);
    assert_eq!(
        state
            .worktree_attention(renamed_project, replacement)
            .unread,
        1
    );

    let mut unrelated = projects[0].clone();
    unrelated.id = project_id(99);
    unrelated.worktrees.clear();
    projects.push(unrelated);
    state.refresh_attention_projection(&durable, &projects);
    let rebuilt = state.attention_projection_rebuilds;
    projects.swap(0, 1);
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.attention_projection_rebuilds, rebuilt + 1);
    assert_eq!(state.project_attention(renamed_project).unread, 1);
    projects.pop();
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.project_attention(renamed_project).unread, 0);
}

#[test]
fn attention_cache_tracks_folder_binding_without_a_new_workspace() {
    let mut folder = project().with_kind(ProjectKind::Folder);
    folder.worktrees.clear();
    let project = folder.id;
    let root = folder.repository.clone();
    let card = compact_attention_terminal(CardId::new());
    let durable = Arc::new(crate::model::WorkspaceState {
        card_scopes: std::collections::HashMap::from([(card.id, root.clone())]),
        cards: vec![card.clone()].into(),
        ..Default::default()
    });
    let mut projects = vec![folder];
    let mut state = SidebarState::default();
    state.observe_card_update(card.id);
    state.refresh_attention_projection(&durable, &projects);
    let folder_worktree = WorktreeId::from_uuid(project.as_uuid());
    assert_eq!(state.worktree_attention(project, folder_worktree).unread, 1);

    projects[0].repository = "/different-folder".into();
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.project_attention(project).unread, 0);
    projects[0].repository = root;
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.worktree_attention(project, folder_worktree).unread, 1);
    projects[0].kind = ProjectKind::Git;
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.project_attention(project).unread, 0);
    projects[0].kind = ProjectKind::Folder;
    projects[0].id = project_id(99);
    state.refresh_attention_projection(&durable, &projects);
    assert_eq!(state.project_attention(project).unread, 0);
    assert_eq!(
        state
            .worktree_attention(
                project_id(99),
                WorktreeId::from_uuid(project_id(99).as_uuid())
            )
            .unread,
        1
    );
}

/// A channel's pane keeps the scope of the checkout its agent runs in,
/// usually the repository's main worktree, but only its channel row shows,
/// opens and reads it. Its news must not also put a bell on that
/// worktree's row, where nothing under the row could clear it.
#[test]
fn channel_pane_attention_stays_off_the_worktree_it_runs_in() {
    let project = project();
    let main = project.worktrees[0].clone();
    let scoped = compact_attention_terminal(CardId::new());
    let remembered = compact_attention_terminal(CardId::new());
    let unchanneled = compact_attention_terminal(CardId::new());
    let channel = crate::model::ProjectChannel {
        title_source: Default::default(),
        id: ChannelId::new(),
        project_id: project.id,
        title: "Channel".into(),
        ordinal: 0,
        shared: false,
        cards: vec![scoped.id, remembered.id],
        focused: None,
    };
    let channel_only = Arc::new(crate::model::WorkspaceState {
        cards: vec![scoped.clone(), remembered.clone()].into(),
        channels: vec![channel.clone()],
        card_scopes: std::collections::HashMap::from([(scoped.id, main.path.clone())]),
        ..Default::default()
    });
    let projects = vec![project.clone()];
    let mut state = SidebarState::default();
    state.observe_card_update(scoped.id);
    // An owner recorded before the pane joined the channel is stale too.
    state.observe_agent_update(remembered.id, project.id, main.id);
    state.refresh_attention_projection(&channel_only, &projects);
    assert_eq!(state.project_attention(project.id).unread, 2);
    assert_eq!(
        state.worktree_attention(project.id, main.id),
        AttentionSummary::default()
    );

    // A pane in no channel is listed under its worktree, so it counts.
    let with_unchanneled = Arc::new(crate::model::WorkspaceState {
        cards: vec![scoped, remembered, unchanneled.clone()].into(),
        card_scopes: std::collections::HashMap::from([(unchanneled.id, main.path.clone())]),
        channels: vec![channel],
        ..Default::default()
    });
    state.observe_card_update(unchanneled.id);
    state.refresh_attention_projection(&with_unchanneled, &projects);
    assert_eq!(state.project_attention(project.id).unread, 3);
    assert_eq!(state.worktree_attention(project.id, main.id).unread, 1);
}

fn relocated_attention_model(
    count: usize,
) -> (SidebarModel, Arc<crate::model::WorkspaceState>, Vec<CardId>) {
    let mut project = project();
    if count > 1 {
        project
            .worktrees
            .push(worktree(90, "second-current", false));
    }
    let cards: Vec<_> = (0..count)
        .map(|_| compact_attention_terminal(CardId::new()))
        .collect();
    let ids = cards.iter().map(|card| card.id).collect();
    let durable = Arc::new(crate::model::WorkspaceState {
        card_scopes: cards
            .iter()
            .enumerate()
            .map(|(index, card)| (card.id, project.worktrees[index + 1].path.clone()))
            .collect(),
        cards: cards.into(),
        ..Default::default()
    });
    (
        SidebarModel::ready(SidebarMode::Expanded, vec![project]),
        durable,
        ids,
    )
}

/// Projects attention as the app does: counts first, then the pane rows
/// those counts give, titled from the durable cards.
fn refresh_attention(
    state: &mut SidebarState,
    model: &mut SidebarModel,
    durable: &Arc<crate::model::WorkspaceState>,
) {
    state.refresh_attention_projection(durable, &model.projects);
    let title = |id| {
        durable
            .cards
            .iter()
            .find(|card| card.id == id)
            .map(|card| card.title.as_str())
    };
    let stale = model.stale_panes(state, title);
    model.set_panes(state, &stale, |id| title(id).map(str::to_owned));
    assert!(
        model.stale_panes(state, title).is_empty(),
        "rebuilt rows match the projection they were built from"
    );
}

#[test]
fn attention_reads_keep_current_projected_rows_across_a_batch() {
    use crate::agent::{Activity, ActivityState, Provider};
    let (mut model, durable, cards) = relocated_attention_model(2);
    let project = model.projects[0].id;
    let historical = model.projects[0].worktrees[0].id;
    let rows: Vec<_> = cards.iter().copied().map(SelectionRow::Card).collect();
    let expected = rows
        .iter()
        .map(|row| (*row, Listing::Read))
        .collect::<BTreeMap<_, _>>();
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    for &card in &cards {
        state.observe_agent_update(card, project, historical);
    }
    refresh_attention(&mut state, &mut model, &durable);
    let listed = visible_selection_rows(&model, &state, false);
    assert_eq!(
        listed,
        [vec![SelectionRow::Project(project)], rows.clone()].concat(),
        "each pane has a row of its own, and no checkout does"
    );
    let placement = AttentionPlacement {
        channels: Vec::new(),
        listed: &listed,
    };
    for &card in &cards {
        state.acknowledge_agent_update(card, &placement);
    }
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(state.retained_attention, expected.clone());
    assert_eq!(visible_selection_rows(&model, &state, false), listed);
    let first = &model.projects[0].panes[0];
    assert_eq!(state.card_listing(first), Listing::Read);

    // A later answered wait keeps the stronger evidence that it was read.
    let activity = |state| Activity {
        provider: Provider::Codex,
        state,
    };
    state.set_agent_activity(
        cards[0],
        project,
        historical,
        activity(ActivityState::Waiting),
    );
    refresh_attention(&mut state, &mut model, &durable);
    let listed = visible_selection_rows(&model, &state, false);
    state.settle_waiting_agent(
        cards[0],
        &AttentionPlacement {
            channels: Vec::new(),
            listed: &listed,
        },
    );
    state.set_agent_activity(
        cards[0],
        project,
        historical,
        activity(ActivityState::Working),
    );
    refresh_attention(&mut state, &mut model, &durable);
    let first = &model.projects[0].panes[0];
    assert_eq!(state.card_listing(first), Listing::Read);
    assert_eq!(state.retained_attention, expected.clone());
}

#[test]
fn attention_settles_current_projected_waits_only_when_drawn() {
    use crate::agent::{Activity, ActivityState, Provider};
    for hidden in [false, true] {
        let (mut model, durable, cards) = relocated_attention_model(1);
        let project = model.projects[0].id;
        let historical = model.projects[0].worktrees[0].id;
        let row = SelectionRow::Card(cards[0]);
        let activity = |state| Activity {
            provider: Provider::Codex,
            state,
        };
        let mut state = expanded_worktree_state();
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        state.set_agent_activity(
            cards[0],
            project,
            historical,
            activity(ActivityState::Waiting),
        );
        refresh_attention(&mut state, &mut model, &durable);
        if hidden {
            state.apply(&SidebarAction::ToggleProject(project));
        }
        let listed = visible_selection_rows(&model, &state, false);
        assert_eq!(listed.contains(&row), !hidden);
        let placement = AttentionPlacement {
            channels: Vec::new(),
            listed: &listed,
        };
        state.settle_waiting_agent(cards[0], &placement);
        state.set_agent_activity(
            cards[0],
            project,
            historical,
            activity(ActivityState::Working),
        );
        refresh_attention(&mut state, &mut model, &durable);
        assert_eq!(
            state.retained_attention.get(&row),
            (!hidden).then_some(&Listing::Handled)
        );
        assert!(
            state
                .retained_attention
                .keys()
                .all(|row| matches!(row, SelectionRow::Card(_)))
        );
    }
    // An unread row hidden under that same disclosure was not seen either.
    let (mut model, durable, cards) = relocated_attention_model(1);
    let mut state = expanded_worktree_state();
    let project = model.projects[0].id;
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.observe_agent_update(cards[0], project, model.projects[0].worktrees[0].id);
    refresh_attention(&mut state, &mut model, &durable);
    state.apply(&SidebarAction::ToggleProject(project));
    let listed = visible_selection_rows(&model, &state, false);
    state.acknowledge_agent_update(
        cards[0],
        &AttentionPlacement {
            channels: Vec::new(),
            listed: &listed,
        },
    );
    assert!(state.retained_attention.is_empty());
}

/// Two cards of one conversation are one pane row, whichever card the
/// read arrives through, and a conversation split across checkouts names
/// none of them rather than guessing.
#[test]
fn attention_lists_one_pane_row_per_conversation() {
    let (mut model, mut durable, cards) = relocated_attention_model(2);
    let project = model.projects[0].id;
    let historical = model.projects[0].worktrees[0].id;
    let mut alias = durable.cards[0].clone();
    alias.id = CardId::new();
    let workspace = Arc::make_mut(&mut durable);
    workspace
        .card_scopes
        .insert(alias.id, model.projects[0].worktrees[0].path.clone());
    workspace.cards.push(alias.clone());
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    for card in [cards[0], cards[1], alias.id] {
        state.observe_agent_update(card, project, historical);
    }
    refresh_attention(&mut state, &mut model, &durable);
    let listed = visible_selection_rows(&model, &state, false);
    assert_eq!(
        listed,
        vec![
            SelectionRow::Project(project),
            SelectionRow::Card(cards[1]),
            SelectionRow::Card(cards[0]),
        ],
        "the split conversation has no checkout, so it sorts last"
    );
    let split = model.projects[0]
        .panes
        .iter()
        .find(|card| card.id == cards[0])
        .unwrap();
    assert_eq!(split.worktree, None);
    assert_eq!(split.checkout(&model.projects[0]), None);
    state.acknowledge_agent_update(
        alias.id,
        &AttentionPlacement {
            channels: Vec::new(),
            listed: &listed,
        },
    );
    assert_eq!(
        state.retained_attention,
        BTreeMap::from([(SelectionRow::Card(cards[0]), Listing::Read)]),
        "reading through the alias keeps the conversation's row"
    );
}

/// The pane workspace focus names is drawn as the current row, through
/// any card of its conversation, in place of its project header; the
/// keyboard order holds that row, and once it leaves the list no row is
/// drawn as current.
#[test]
fn attention_draws_the_focused_panes_row_as_current() {
    let (mut model, mut durable, cards) = relocated_attention_model(2);
    let project = model.projects[0].id;
    let historical = model.projects[0].worktrees[0].id;
    let mut alias = durable.cards[1].clone();
    alias.id = CardId::new();
    let workspace = Arc::make_mut(&mut durable);
    workspace
        .card_scopes
        .insert(alias.id, model.projects[0].worktrees[2].path.clone());
    workspace.cards.push(alias.clone());
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    for card in [cards[0], cards[1]] {
        state.observe_agent_update(card, project, historical);
    }
    refresh_attention(&mut state, &mut model, &durable);
    let worktree = SidebarSelection::Worktree {
        project_id: project,
        worktree_id: historical,
    };
    let current = |model: &SidebarModel, state: &SidebarState| {
        model.projects[0]
            .panes
            .iter()
            .filter(|card| state.follows_card(card))
            .map(|card| card.id)
            .collect::<Vec<_>>()
    };
    assert!(current(&model, &state).is_empty());
    assert_eq!(
        header_selection(&model.projects[0], &state, Some(worktree)),
        Some(worktree)
    );

    let cache = NavigationCache::default();
    let unfollowed = cache.generation(&model, &state);
    state.follow_card(Some(alias.id));
    assert_eq!(
        current(&model, &state),
        vec![cards[1]],
        "the alias's conversation row is current"
    );
    assert_ne!(unfollowed, cache.generation(&model, &state), "it redraws");
    let followed = cache.generation(&model, &state);
    state.follow_card(Some(cards[1]));
    assert_eq!(
        followed,
        cache.generation(&model, &state),
        "another card of the same row redraws nothing"
    );
    assert_eq!(
        header_selection(&model.projects[0], &state, Some(worktree)),
        None,
        "the row stands in for its project header"
    );
    // The highlight and the keyboard agree on where the user is.
    // The keyboard order holds the current row itself, not its alias,
    // so arrows from it reach the rows drawn beside it.
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![
            SelectionRow::Project(project),
            SelectionRow::Card(cards[0]),
            SelectionRow::Card(cards[1]),
        ]
    );

    // Read and hidden, the row leaves the list and takes the highlight.
    read(&mut state, &model, cards[1], Vec::new());
    state.apply(&SidebarAction::HideReadAttention);
    refresh_attention(&mut state, &mut model, &durable);
    assert!(!visible_selection_rows(&model, &state, false).contains(&SelectionRow::Card(cards[1])));
    assert!(
        filtered_projects(&model, &state, false)
            .into_iter()
            .flat_map(|project| project.cards)
            .chain(
                in_progress_rows(&model, &state, false)
                    .into_iter()
                    .filter_map(|row| match row.item {
                        InProgressItem::Card(card) => Some(card),
                        InProgressItem::Channel(_) => None,
                    })
            )
            .all(|card| !state.follows_card(card)),
        "no drawn row is current"
    );
    // Only the rail draws a checkout's selection on its project, as the
    // place focus is; the expanded header shows only its own.
    assert_eq!(
        header_selection(&model.projects[0], &state, Some(worktree)),
        Some(worktree)
    );
}

#[test]
fn compact_project_identity_and_waiting_badge_are_deterministic() {
    assert_eq!(project_identity("Alpha"), project_identity("Alpha"));
    let first = project_identity("Alpha");
    let second = project_identity("Atlas");
    assert_ne!(first.0, second.0);
    assert_ne!(first.1, second.1);
    let project = project();
    let mut state = SidebarState::default();
    state.set_agent_activity(
        CardId::new(),
        project.id,
        project.worktrees[0].id,
        crate::agent::Activity {
            provider: crate::agent::Provider::Codex,
            state: crate::agent::ActivityState::Waiting,
        },
    );
    let attention = compact_project_attention(&project, &state);
    let (activity, unread) = (attention.activity, attention.summary.unread != 0);
    assert_eq!(activity, Some(crate::agent::ActivityState::Waiting));
    assert!(agent_indicator(activity, unread).is_some());
    let renderer = headless_renderer();
    let names = accessible_names(
        &renderer,
        compact_project_identity(
            &project,
            true,
            false,
            &state,
            Some(state.project_attention(project.id)),
        ),
    );
    assert!(
        names
            .iter()
            .any(|name| name.contains(&project.name)
                && name.contains("agent is waiting for input")),
        "{names:?}"
    );
}

#[test]
fn compact_project_initials_paint_bright_inside_the_colored_chip() {
    use cosmic::iced::{
        Color, Rectangle,
        advanced::{
            mouse,
            renderer::{Headless, Style},
        },
    };

    for theme in [cosmic::Theme::light(), cosmic::Theme::dark()] {
        for name in ["Alpha", "Atlas", "Beta", "Gamma"] {
            let mut project = project();
            project.name = name.into();
            project.avatar = None;
            let mut renderer = headless_renderer();
            // No selected outline or hover: bright ink inside the chip
            // must come from its actual initials, not a white border.
            let mut element = compact_project_identity(
                &project,
                false,
                false,
                &SidebarState::default(),
                Some(AttentionSummary::default()),
            );
            let mut tree = Tree::new(&element);
            let viewport = Rectangle::with_size(Size::new(COMPACT_WIDTH, WORKTREE_ROW_HEIGHT));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            element.as_widget().draw(
                &tree,
                &mut renderer,
                &theme,
                &Style::default(),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &viewport,
            );
            let width = COMPACT_WIDTH as usize;
            let pixels = renderer.screenshot(
                Size::new(width as u32, WORKTREE_ROW_HEIGHT as u32),
                1.0,
                Color::BLACK,
            );
            let background = project_identity(name).1.into_rgba8();
            let chip: Vec<_> = pixels
                .chunks_exact(4)
                .enumerate()
                .filter(|(_, pixel)| {
                    pixel
                        .iter()
                        .zip(background)
                        .all(|(a, b)| a.abs_diff(b) <= 1)
                })
                .map(|(index, _)| (index % width, index / width))
                .collect();
            assert!(
                !chip.is_empty(),
                "missing actual chip background for {name}"
            );
            let left = chip.iter().map(|(x, _)| *x).min().unwrap();
            let right = chip.iter().map(|(x, _)| *x).max().unwrap();
            let top = chip.iter().map(|(_, y)| *y).min().unwrap();
            let bottom = chip.iter().map(|(_, y)| *y).max().unwrap();
            let bright_ink = (top + 2..bottom.saturating_sub(1))
                .flat_map(|y| (left + 2..right.saturating_sub(1)).map(move |x| (x, y)))
                .filter(|(x, y)| {
                    let offset = (y * width + x) * 4;
                    pixels[offset..offset + 3].iter().all(|value| *value >= 210)
                })
                .count();
            assert!(
                bright_ink >= 3,
                "{name} initials lack bright foreground ink"
            );
        }
    }
}

#[test]
fn highlighted_labels_truncate_to_measured_slot_with_an_ellipsis() {
    let _renderer = headless_renderer();
    let font = cosmic::font::default();
    let emphasis = cosmic::iced::Font {
        weight: cosmic::iced::font::Weight::Bold,
        ..font
    };
    let label = "feature/authentication-long-λ-branch";
    for width in [55.0, 105.0, 170.0] {
        let truncated = ellipsize_search_label(label, width, |label| {
            search_spans_width(label, "feat", CHILD_ROW_TEXT_SIZE, font, emphasis)
        });
        assert!(truncated.ends_with('…'), "{truncated}");
        assert!(
            search_spans_width(&truncated, "feat", CHILD_ROW_TEXT_SIZE, font, emphasis) <= width
        );
        assert!(!search_highlight_ranges(&truncated, "feat").is_empty());
    }
    assert_eq!(
        ellipsize_search_label(label, 2000.0, |label| search_spans_width(
            label,
            "feat",
            CHILD_ROW_TEXT_SIZE,
            font,
            emphasis
        )),
        label
    );
    assert!(search_highlight_ranges("fast elegant audit trail", "feat").is_empty());
}

#[test]
fn shuffled_unknown_projects_use_a_stable_fallback_then_durable_order() {
    let projects: Vec<_> = ["gamma", "alpha", "beta"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| {
            let mut project = project();
            project.id = project_id(index as u128 + 20);
            project.repository = PathBuf::from(format!("/repo/{name}"));
            project
        })
        .collect();
    let mut first = SidebarModel::ready(SidebarMode::Expanded, projects.clone());
    let mut second =
        SidebarModel::ready(SidebarMode::Expanded, projects.into_iter().rev().collect());
    first.apply_project_order(&[]);
    second.apply_project_order(&[]);
    assert_eq!(first.projects, second.projects);
    let order: Vec<_> = first.projects.iter().map(|project| project.id).collect();
    second.projects.rotate_left(1);
    second.apply_project_order(&order);
    assert_eq!(first.projects, second.projects);
}

#[test]
fn required_review_policy_does_not_claim_the_viewer_was_requested() {
    let mut pull_request = crate::pull_request_review::tests::pull_request(51);
    for decision in ["REVIEW_REQUIRED", "CHANGES_REQUESTED"] {
        pull_request.review_decision = Some(crate::github::RemoteValue(decision.into()));
        let row = super::SidebarPullRequest::from_github(
            crate::model::ProjectId::new(),
            &pull_request,
            &[],
        );
        assert!(!row.needs_review);
        assert!(!row.review_requested_again);
    }
}

use super::*;

#[test]
fn a_project_rebuilt_from_a_fresh_load_has_a_new_pull_request_revision() {
    let id = ProjectId::new();
    let project = || {
        SidebarProject::new(
            id,
            crate::repository::RepositoryId(id.as_uuid()),
            "widgets".into(),
            "/w/widgets".into(),
            Vec::new(),
            Vec::new(),
        )
    };
    let (loaded, reloaded) = (project(), project());
    assert_ne!(
        loaded.pull_requests_revision(),
        reloaded.pull_requests_revision(),
        "an index built from the first must see the second as new"
    );
    assert_eq!(loaded, reloaded, "the revision is not part of the project");
}
use crate::{
    github::RemoteValue,
    repository::{
        ChangeArea, ChangeKind, ChangedPath, HeadState, LocalRefRecord, RemoteHead, RemoteRecord,
        RemoteRefRecord, RepoPath, WorkingFiles, WorktreeLock,
    },
};
use cosmic::iced::{
    Rectangle, Size,
    advanced::{Layout, layout, widget::Tree},
};
use uuid::Uuid;

fn project_id(value: u128) -> ProjectId {
    ProjectId::from_uuid(Uuid::from_u128(value))
}

#[test]
fn navigation_cache_invalidates_attention_query_and_disclosure_but_reuses_identical_views() {
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let mut state = SidebarState::default();
    let cache = NavigationCache::default();
    let initial = cache.generation(&model, &state);
    assert_eq!(initial, cache.generation(&model.clone(), &state.clone()));
    state.apply(&SidebarAction::ToggleProject(project_id(1)));
    let collapsed = cache.generation(&model, &state);
    assert_ne!(initial, collapsed);
    state.query = "fix".into();
    let searched = cache.generation(&model, &state);
    assert_ne!(collapsed, searched);
    state.observe_agent_update(
        CardId::new(),
        project_id(1),
        WorktreeId::from_uuid(Uuid::from_u128(2)),
    );
    let attention = cache.generation(&model, &state);
    assert_ne!(searched, attention);
    assert_eq!(attention, cache.generation(&model, &state));
}

#[test]
fn navigation_cache_compares_projects_only_after_they_may_have_changed() {
    let comparisons = || super::PROJECT_COMPARISONS.with(std::cell::Cell::get);
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let state = SidebarState::default();
    let cache = NavigationCache::default();
    let initial = cache.generation(&model, &state);
    let before = comparisons();
    for _ in 0..3 {
        assert_eq!(initial, cache.generation(&model, &state));
    }
    assert_eq!(comparisons(), before, "an untouched model is not compared");

    // Borrowed for writing, changed nothing: one comparison proves it,
    // and the frames after it are spared another.
    model.projects[0].reloading = false;
    assert_eq!(initial, cache.generation(&model, &state));
    assert_eq!(comparisons(), before + 1);
    assert_eq!(initial, cache.generation(&model, &state));
    assert_eq!(comparisons(), before + 1);

    // A model built anew with the same projects is still compared, never
    // mistaken for the one the cache saw.
    model.projects = vec![project()].into();
    assert_eq!(initial, cache.generation(&model, &state));
    assert_eq!(comparisons(), before + 2);

    model.projects[0].reloading = true;
    let reloading = cache.generation(&model, &state);
    assert_ne!(initial, reloading, "a change is drawn");
    assert_eq!(reloading, cache.generation(&model, &state));
    assert_eq!(comparisons(), before + 3);
}

/// A channel's mark and count come from its members, so a change in who
/// is at work redraws the list, and a refresh that finds the same members
/// does not.
#[test]
fn navigation_cache_redraws_when_a_channels_members_change() {
    use crate::agent::Provider;
    let mut project = project();
    project.channels.push(SidebarChannel::new(
        ChannelId::new(),
        "Release prep".into(),
        false,
        None,
        false,
        1,
        true,
        1,
    ));
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project]);
    let state = SidebarState::default();
    let cache = NavigationCache::default();
    let alone = cache.generation(&model, &state);

    let mut members = ChannelMembers::default();
    members.add_agent(Provider::Claude, ChannelMemberState::Working);
    let channel = &mut model.projects[0].channels[0];
    *channel = channel.clone().with_members(members);
    let working = cache.generation(&model, &state);
    assert_ne!(alone, working, "an agent joining is drawn");

    let channel = &mut model.projects[0].channels[0];
    *channel = channel.clone().with_members(members);
    assert_eq!(working, cache.generation(&model, &state));

    members.add_agent(Provider::Codex, ChannelMemberState::Idle);
    let channel = &mut model.projects[0].channels[0];
    *channel = channel.clone().with_members(members);
    assert_ne!(
        working,
        cache.generation(&model, &state),
        "a second agent is drawn"
    );
}

#[test]
fn navigation_cache_ignores_large_source_control_and_file_panel_interactions() {
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let mut state = SidebarState::default();
    let cache = NavigationCache::default();
    let initial = cache.generation(&model, &state);
    let paths: Vec<_> = (0..10_000)
        .map(|index| RepoPath::parse(format!("src/file_{index}.rs")).unwrap())
        .collect();
    let mut source = SourceControlModel::loading(main_key());
    source.files = file_rows(&paths);
    model.source_control = Some(source);
    assert_eq!(initial, cache.generation(&model, &state));

    state.files_query = "file".into();
    state
        .expanded_directories
        .insert(RepoPath::parse("src").unwrap());
    state.commit_draft.message = "message".into();
    state
        .commit_draft
        .composer
        .0
        .perform(cosmic::iced::widget::text_editor::Action::Edit(
            cosmic::iced::widget::text_editor::Edit::Insert('x'),
        ));
    state.changes_tab = ChangesTab::History;
    assert_eq!(initial, cache.generation(&model, &state));
    model.source_control.as_mut().unwrap().files.clear();
    assert_eq!(initial, cache.generation(&model, &state));
}

#[test]
fn navigation_cache_tracks_active_worktree_open_panes_editors_and_project_changes() {
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let mut state = SidebarState::default();
    let cache = NavigationCache::default();
    let mut previous = cache.generation(&model, &state);
    state.followed_worktree = Some(main_key());
    let next = cache.generation(&model, &state);
    assert_ne!(previous, next);
    previous = next;
    state
        .worktrees_with_panes
        .insert((main_key().project_id, main_key().worktree_id));
    let next = cache.generation(&model, &state);
    assert_ne!(previous, next);
    previous = next;
    model.external_editors.push(SidebarEditor {
        id: "test".into(),
        name: "Test editor".into(),
        icon: "editor".into(),
    });
    let next = cache.generation(&model, &state);
    assert_ne!(previous, next);
    previous = next;
    model.projects[0].reloading = true;
    assert_ne!(previous, cache.generation(&model, &state));
}

#[test]
fn search_ignores_common_ancestor_paths_and_accepts_name_abbreviations() {
    let entry = project();
    assert!(!entry.matches("repo"));
    assert!(!entry.worktrees[0].matches("repo"));
    assert!(entry.worktrees[0].matches("/repo/"));
    assert!(search_score("feature/authentication", "fauth").is_some());
    assert!(search_score("feature/authentication", "fhz").is_none());
}

#[test]
fn compact_search_mounts_matching_rows_without_open_panes_or_attention() {
    let model = SidebarModel::ready(SidebarMode::Compact, vec![project()]);
    let mut state = SidebarState::default();
    let id = selection_row_focus_id(SelectionRow::Worktree(
        project_id(1),
        WorktreeId::from_uuid(Uuid::from_u128(3)),
    ));
    let quiet = project_list(&model, &state, true, false);
    assert!(Tree::new(&quiet).find(&id).is_none());
    state.controls_expanded = true;
    state.query = "fix".into();
    let result = project_list(&model, &state, true, false);
    assert!(
        Tree::new(&result).find(&id).is_some(),
        "a matching inactive worktree must be mounted so Search Down can focus it"
    );
}

#[test]
fn ranked_search_prefers_prefixes_then_substrings_then_abbreviations_and_paths() {
    let mut entry = project();
    entry.worktrees = vec![
        worktree(20, "x-fix-y", false),
        worktree(21, "fancy-index", false),
        worktree(22, "fix", false),
    ];
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let state = SidebarState {
        query: "fix".into(),
        ..SidebarState::default()
    };
    let filtered = filtered_projects(&model, &state, false);
    assert_eq!(
        filtered[0]
            .worktrees
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>(),
        [22, 20, 21].map(|id| WorktreeId::from_uuid(Uuid::from_u128(id)))
    );
    assert!(search_score("fix", "fix") < search_score("prefix-fix", "fix"));
    assert!(search_score("prefix-fix", "fix") < search_score("fancy-index", "fix"));
    assert!(
        model.projects[0].worktrees[0]
            .search_rank("/repo/")
            .unwrap()
            >= 10_000
    );
}

#[test]
fn fuzzy_highlights_preserve_utf8_boundaries_and_casefold_expansions() {
    assert_eq!(
        search_highlight_ranges("Feature authentication", "fauth"),
        Vec::<std::ops::Range<usize>>::new()
    );
    assert_eq!(search_highlight_ranges("Straße", "ss"), vec![4..6]);
    assert_eq!(search_highlight_ranges("Éclair", "écl"), vec![0..4]);
    assert!(search_highlight_ranges("Feature", "missing").is_empty());
    assert!(search_highlight_ranges("Feature", "").is_empty());
}

#[test]
fn search_navigation_and_escape_require_enabled_input_focus() {
    use cosmic::iced::{
        Event, Rectangle,
        advanced::{Shell, clipboard, widget::operation},
        keyboard, mouse,
    };
    for key in [
        keyboard::key::Named::ArrowDown,
        keyboard::key::Named::Escape,
    ] {
        for (focused, modal) in [(false, false), (true, false), (true, true)] {
            let state = SidebarState::default();
            let renderer = headless_renderer();
            let mut element = search(&state, modal);
            let mut tree = Tree::new(&element);
            let viewport = Rectangle::with_size(Size::new(240.0, 48.0));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            if focused {
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut operation::focusable::focus::<()>(search_focus_id()),
                );
            }
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(key),
                    modified_key: keyboard::Key::Named(key),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::ArrowDown),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                }),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            assert_eq!(
                messages,
                if focused && !modal {
                    vec![if key == keyboard::key::Named::Escape {
                        SidebarAction::ToggleControls
                    } else {
                        SidebarAction::FocusFirstSearchResult
                    }]
                } else {
                    vec![]
                }
            );
        }
    }
}

#[test]
fn waiting_indicator_survives_read_acknowledgement_and_ci_does_not_recolor_pr_state() {
    assert!(agent_indicator(Some(crate::agent::ActivityState::Waiting), false).is_some());
    assert!(agent_indicator(None, true).is_some());
    assert!(agent_indicator(None, false).is_none());
    let mut pr = project().pull_requests[0].clone();
    pr.needs_review = false;
    pr.draft = false;
    pr.state = Some(WorktreePullRequestState::Open);
    for checks in [
        CheckStatus::Failing,
        CheckStatus::Pending,
        CheckStatus::Passing,
    ] {
        pr.checks = checks;
        assert_eq!(pull_request_tone(&pr), PullRequestTone::Neutral);
    }
    pr.state = Some(WorktreePullRequestState::Closed);
    assert_eq!(pull_request_tone(&pr), PullRequestTone::Danger);
}

fn worktree(id: u128, branch: &str, dirty: bool) -> SidebarWorktree {
    let mut row = SidebarWorktree::from_repository(
        &WorktreeRecord {
            id: WorktreeId::from_uuid(Uuid::from_u128(id)),
            path: PathBuf::from(format!("/repo/{branch}")),
            created_at_unix_ms: None,
            head_oid: None,
            branch: Some(format!("refs/heads/{branch}")),
            detached: false,
            bare: false,
            lock: WorktreeLock::default(),
            prunable_reason: None,
        },
        Some(&StatusSnapshot {
            untracked_omitted: false,
            head: crate::repository::HeadState::Unknown,
            tracking: TrackingState::NoUpstream,
            dirty: DirtyState {
                unstaged: if dirty { 1 } else { 0 },
                ..DirtyState::default()
            },
            // `dirty` counts what `entries` lists, so a fixture that
            // claimed a modified file without naming one would describe a
            // snapshot the parser cannot produce.
            entries: if dirty {
                vec![ChangedPath {
                    path: RepoPath::parse("src/lib.rs").unwrap(),
                    old_path: None,
                    area: ChangeArea::Unstaged,
                    kind: ChangeKind::Modified,
                    conflict: None,
                    submodule: false,
                    added_lines: None,
                    removed_lines: None,
                }]
            } else {
                Vec::new()
            },
        }),
    );
    row.head_repository =
        Some(crate::github::GitHubRepoKey::new("github.com", None, "example", "cagents").unwrap());
    row
}

fn project() -> SidebarProject {
    let mut project = SidebarProject::new(
        project_id(1),
        RepositoryId(Uuid::from_u128(10)),
        "Cagents".into(),
        "/repo/cagents".into(),
        vec![worktree(2, "main", false), worktree(3, "fix-sidebar", true)],
        vec![SidebarPullRequest::new(
            project_id(1),
            42,
            "Polish sidebar".into(),
            Url::parse("https://github.com/example/cagents/pull/42").unwrap(),
            "fix-sidebar".into(),
            Some("octocat".into()),
            false,
            CheckStatus::Failing,
        )],
    );
    project.pull_requests[0].head_repository = project.worktrees[0].head_repository.clone();
    let pr = project.pull_requests[0].clone();
    project.worktrees[1].set_pull_request(pr);
    project
}

/// Existing row interaction contracts exercise the Expanded presentation.
fn expanded_worktree_state() -> SidebarState {
    SidebarState {
        sidebar_worktrees: crate::settings::SidebarWorktrees::Expanded,
        ..SidebarState::default()
    }
}

fn display_project() -> SidebarProject {
    let mut entry = project();
    entry.worktrees[0].path = entry.repository.clone();
    entry.channels = vec![
        SidebarChannel::new(
            ChannelId::from_uuid(Uuid::from_u128(91)),
            "Planning".into(),
            false,
            None,
            false,
            0,
            false,
            1,
        ),
        SidebarChannel::new(
            ChannelId::from_uuid(Uuid::from_u128(92)),
            "Review".into(),
            false,
            None,
            false,
            0,
            false,
            2,
        ),
    ];
    entry
}

#[test]
fn worktree_display_resolves_sparse_overrides_and_default_reset() {
    use crate::settings::SidebarWorktrees::*;
    let mut state = SidebarState::default();
    let project = project_id(1);
    assert_eq!(state.worktree_display(project), Grouped);
    state.set_sidebar_worktrees(Hidden);
    assert_eq!(state.worktree_display(project), Hidden);
    state.apply(&SidebarAction::SetWorktreeDisplay {
        project_id: project,
        mode: Some(Expanded),
    });
    state.set_sidebar_worktrees(Grouped);
    assert_eq!(state.worktree_display(project), Expanded);
    state.apply(&SidebarAction::SetWorktreeDisplay {
        project_id: project,
        mode: None,
    });
    assert_eq!(state.worktree_display(project), Grouped);
    assert!(state.worktree_display_overrides.is_empty());
}

#[test]
fn show_checkouts_clears_a_projects_hidden_mode_or_shows_them_everywhere() {
    use crate::settings::SidebarWorktrees::*;
    use widget::menu::Action as _;
    let project = project_id(1);
    for global in [Grouped, Expanded, Hidden] {
        let mut state = SidebarState::default();
        state.set_sidebar_worktrees(global);
        state.apply(&SidebarAction::SetWorktreeDisplay {
            project_id: project,
            mode: Some(Hidden),
        });
        let message = ContextCommand::ShowCheckouts(project, global).message();
        state.apply(&message);
        if global == Hidden {
            // A hidden default hides them everywhere, so only showing
            // them everywhere shows this project's; its own mode stays.
            assert_eq!(message, SidebarAction::ToggleCheckouts);
            assert_eq!(
                state.worktree_display_overrides.get(&project),
                Some(&Hidden)
            );
        } else {
            assert_eq!(state.worktree_display(project), global);
            assert!(state.worktree_display_overrides.is_empty());
        }
    }
}

#[test]
fn worktree_display_persistence_round_trip_and_legacy_default() {
    use crate::settings::SidebarWorktrees::*;
    let mut state = SidebarState::default();
    state.apply(&SidebarAction::SetWorktreeDisplay {
        project_id: project_id(1),
        mode: Some(Hidden),
    });
    state.apply(&SidebarAction::ToggleWorktrees(project_id(2)));
    let bytes = serde_json::to_vec(&state.saved_view()).unwrap();
    let mut restored = SidebarState::default();
    restored.restore_view(serde_json::from_slice(&bytes).unwrap());
    let settings = crate::settings::AppSettings {
        sidebar_worktree_overrides: state.worktree_display_overrides.clone(),
        ..crate::settings::AppSettings::default()
    };
    let settings: crate::settings::AppSettings =
        toml::from_str(&toml::to_string(&settings).unwrap()).unwrap();
    restored.set_worktree_overrides(settings.sidebar_worktree_overrides);
    assert_eq!(restored.worktree_display(project_id(1)), Hidden);
    assert_eq!(restored.worktree_display(project_id(2)), Grouped);
    assert!(restored.expanded_worktree_groups.contains(&project_id(2)));
    let mut legacy = serde_json::to_value(state.saved_view()).unwrap();
    legacy
        .as_object_mut()
        .unwrap()
        .remove("worktree_display_overrides");
    legacy
        .as_object_mut()
        .unwrap()
        .remove("expanded_worktree_groups");
    restored.restore_view(serde_json::from_value(legacy).unwrap());
    assert_eq!(
        restored.worktree_display(project_id(1)),
        Hidden,
        "restoring a view cannot overwrite preferences"
    );
    assert!(restored.expanded_worktree_groups.is_empty());
}

#[test]
fn worktree_display_order_preserves_channels_primary_and_saved_expanded_order() {
    use crate::settings::SidebarWorktrees::*;
    use crate::workspace_model::NavigationItem;
    let mut entry = display_project();
    let id = entry.id;
    let primary = SelectionRow::Worktree(id, entry.worktrees[0].id);
    let extra = SelectionRow::Worktree(id, entry.worktrees[1].id);
    let first = SelectionRow::Channel(entry.channels[0].id);
    let second = SelectionRow::Channel(entry.channels[1].id);
    entry.navigation_order = vec![
        NavigationItem::Worktree(entry.worktrees[1].path.clone()),
        NavigationItem::Channel(entry.channels[0].id),
        NavigationItem::Worktree(entry.worktrees[0].path.clone()),
        NavigationItem::Channel(entry.channels[1].id),
    ];
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let mut state = SidebarState::default();
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![SelectionRow::Project(id), first, primary, second]
    );
    state.apply(&SidebarAction::ToggleWorktrees(id));
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![SelectionRow::Project(id), first, primary, second, extra]
    );
    state.set_sidebar_worktrees(Expanded);
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![SelectionRow::Project(id), extra, first, primary, second]
    );
    state.set_sidebar_worktrees(Hidden);
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![SelectionRow::Project(id), first, second],
        "hidden checkouts include the root"
    );
    assert_eq!(
        workspace_cycle(&model, &state).len(),
        4,
        "hidden worktrees remain keyboard destinations"
    );
}

#[test]
fn worktree_display_rolls_up_strongest_attention_without_opening_group() {
    use crate::agent::ActivityState;
    let renderer = headless_renderer();
    let entry = display_project();
    let id = entry.id;
    let worktree = entry.worktrees[1].id;
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let mut state = SidebarState::default();
    state.observe_agent_update(CardId::new(), id, worktree);
    assert_eq!(
        state.grouped_attention(&model.projects[0]).state(),
        Some(AttentionState::Unread)
    );
    state.set_agent_activity(
        CardId::new(),
        id,
        worktree,
        crate::agent::Activity {
            state: ActivityState::Working,
            provider: crate::agent::Provider::Claude,
        },
    );
    state.set_agent_activity(
        CardId::new(),
        id,
        worktree,
        crate::agent::Activity {
            state: ActivityState::Waiting,
            provider: crate::agent::Provider::Claude,
        },
    );
    assert_eq!(
        state.grouped_attention(&model.projects[0]).state(),
        Some(AttentionState::Waiting)
    );
    assert!(!state.worktree_group_expanded(&model.projects[0]));
    let names = accessible_names(&renderer, project_list(&model, &state, false, false));
    assert!(
        names
            .iter()
            .any(|name| name.starts_with("Expand Worktrees (1)")
                && name.contains("waiting for input"))
    );
    state.set_sidebar_worktrees(crate::settings::SidebarWorktrees::Hidden);
    let names = accessible_names(&renderer, project_list(&model, &state, false, false));
    assert!(
        names
            .iter()
            .any(|name| name.contains("Cagents") && name.contains("agent needs input"))
    );
}

#[test]
fn worktree_display_keeps_operations_and_a_selected_checkout_visible_unless_hidden() {
    use crate::settings::SidebarWorktrees::*;
    let renderer = headless_renderer();
    let mut entry = display_project();
    let id = entry.id;
    let worktree = entry.worktrees[1].id;
    let path = entry.worktrees[1].path.clone();
    entry.operations.push(WorktreeOperationRow {
        id: crate::model::OperationId::new(),
        label: "Creating topic".into(),
        detail: "Creating topic worktree".into(),
        phase: WorktreeOperationPhase::Working,
        cancellable: true,
        reopenable: false,
    });
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    for mode in [Grouped, Expanded, Hidden] {
        let mut state = SidebarState::default();
        state.set_sidebar_worktrees(mode);
        state.apply(&SidebarAction::ToggleProject(id));
        let names = accessible_names(&renderer, project_list(&model, &state, false, false));
        assert!(
            names.iter().any(|name| name.contains("Creating topic")),
            "{mode:?}"
        );
        state.apply(&SidebarAction::ToggleProject(id));
        state.reveal_worktree(&model.projects[0], worktree, true);
        state.apply(&SidebarAction::OpenWorktree {
            project_id: id,
            worktree_id: worktree,
            path: path.clone(),
        });
        assert_eq!(
            visible_selection_rows(&model, &state, false)
                .contains(&SelectionRow::Worktree(id, worktree)),
            mode != Hidden,
            "{mode:?}: a hidden checkout stays hidden when selected"
        );
        if mode == Grouped {
            assert!(state.worktree_group_expanded(&model.projects[0]));
        }
        model.projects[0].channels[0].selected = true;
        state.set_channel_selected(true);
        if mode == Hidden {
            assert!(
                !visible_selection_rows(&model, &state, false)
                    .contains(&SelectionRow::Worktree(id, worktree))
            );
        }
        model.projects[0].channels[0].selected = false;
        state.set_channel_selected(false);
    }
}

/// What a list publishes, in tree order: its focusable ids and its texts.
#[derive(Default)]
struct Published {
    focusable: Vec<Id>,
    texts: Vec<String>,
}

impl cosmic::iced::advanced::widget::Operation for Published {
    fn traverse(
        &mut self,
        visit: &mut dyn FnMut(&mut dyn cosmic::iced::advanced::widget::Operation),
    ) {
        visit(self);
    }
    fn focusable(
        &mut self,
        id: Option<&Id>,
        _: Rectangle,
        _: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
    ) {
        self.focusable.extend(id.cloned());
    }
    fn text(&mut self, _: Option<&Id>, _: Rectangle, text: &str) {
        self.texts.push(text.to_owned());
    }
}

fn published(element: &mut Element<'_, SidebarAction>) -> Published {
    let renderer = headless_renderer();
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(320.0, 4_000.0)),
    );
    let mut published = Published::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut published);
    published
}

/// The rows the list draws, in the order it draws them: every row the
/// model could give, found by the focusable control that opens it.
fn drawn_rows(model: &SidebarModel, state: &SidebarState, compact: bool) -> Vec<SelectionRow> {
    let rows: std::collections::HashMap<Id, SelectionRow> = model
        .projects
        .iter()
        .flat_map(|project| {
            std::iter::once(SelectionRow::Project(project.id))
                .chain(
                    project
                        .channels
                        .iter()
                        .map(|channel| SelectionRow::Channel(channel.id)),
                )
                .chain(
                    project
                        .worktrees
                        .iter()
                        .map(|worktree| SelectionRow::Worktree(project.id, worktree.id)),
                )
                .chain(project.panes.iter().map(|card| SelectionRow::Card(card.id)))
        })
        .map(|row| (selection_row_focus_id(row), row))
        .collect();
    published(&mut project_list(model, state, compact, false))
        .focusable
        .into_iter()
        .filter_map(|id| rows.get(&id).copied())
        .collect()
}

/// A project with checkouts hidden: a pane idle in its root checkout,
/// and in its linked worktree one with unread news and one working.
/// The root's pane comes second in workspace order.
fn hidden_checkouts_scene() -> (SidebarModel, SidebarState, [CardId; 3]) {
    use crate::agent::{Activity, ActivityState, Provider};
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![display_project()]);
    let project = model.projects[0].id;
    let root = &model.projects[0].worktrees[0];
    let linked = &model.projects[0].worktrees[1];
    assert!(is_primary_worktree(
        &model.projects[0].repository,
        &root.path
    ));
    let pane = |id: u128, title: &str| {
        let mut card = compact_attention_terminal(CardId::from_uuid(Uuid::from_u128(id)));
        card.title = title.into();
        card
    };
    let unread = pane(71, "Review agent");
    let idle = pane(72, "Shell");
    let working = pane(73, "Build");
    let ids = [idle.id, unread.id, working.id];
    let durable = Arc::new(crate::model::WorkspaceState {
        card_scopes: [
            (unread.id, linked.path.clone()),
            (idle.id, root.path.clone()),
            (working.id, linked.path.clone()),
        ]
        .into_iter()
        .collect(),
        cards: vec![unread, idle, working].into(),
        ..Default::default()
    });
    let mut state = SidebarState::default();
    state.set_sidebar_worktrees(crate::settings::SidebarWorktrees::Hidden);
    state.observe_agent_update(ids[1], project, linked.id);
    state.set_agent_activity(
        ids[2],
        project,
        linked.id,
        Activity {
            provider: Provider::Codex,
            state: ActivityState::Working,
        },
    );
    refresh_attention(&mut state, &mut model, &durable);
    (model, state, ids)
}

#[test]
fn hidden_checkouts_give_every_open_pane_a_row_in_their_place() {
    let (model, mut state, [idle, unread, working]) = hidden_checkouts_scene();
    let entry = &model.projects[0];
    let project = entry.id;
    let root = entry.worktrees[0].label().to_owned();
    let linked = entry.worktrees[1].label().to_owned();
    let channels = [
        SelectionRow::Channel(entry.channels[1].id),
        SelectionRow::Channel(entry.channels[0].id),
    ];
    let panes = [
        SelectionRow::Card(idle),
        SelectionRow::Card(unread),
        SelectionRow::Card(working),
    ];
    let rows = visible_selection_rows(&model, &state, false);
    assert_eq!(
        rows,
        [&[SelectionRow::Project(project)][..], &channels, &panes].concat(),
        "no checkout row, the root's included; panes follow the checkout order"
    );
    assert_eq!(
        drawn_rows(&model, &state, false),
        rows,
        "keyboard order is drawn order"
    );

    let list = published(&mut project_list(&model, &state, false, false));
    assert!(
        list.texts.contains(&format!(" · {root}")),
        "{:?}",
        list.texts
    );
    assert!(
        list.texts.contains(&format!(" · {linked}")),
        "{:?}",
        list.texts
    );
    let renderer = headless_renderer();
    let names = accessible_names(&renderer, project_list(&model, &state, false, false));
    assert!(
        names.contains(&format!("Open pane Shell in {root}")),
        "{names:?}"
    );
    for (title, news) in [("Review agent", "unread"), ("Build", "working")] {
        let prefix = format!("Open pane {title} in {linked}, ");
        assert!(
            names
                .iter()
                .any(|name| name.starts_with(&prefix) && name.to_lowercase().contains(news)),
            "{title} names its {news} state: {names:?}"
        );
    }

    // The rail draws every pane too, and keyboard order agrees with it.
    let rail = visible_selection_rows(&model, &state, true);
    assert_eq!(drawn_rows(&model, &state, true), rail);
    assert!(panes.iter().all(|pane| rail.contains(pane)), "{rail:?}");
    assert!(
        !rail
            .iter()
            .any(|row| matches!(row, SelectionRow::Worktree(..))),
        "{rail:?}"
    );

    // A project with nothing open keeps its row.
    let mut empty = model.clone();
    empty.projects[0].panes.clear();
    empty.projects[0].channels.clear();
    assert_eq!(
        visible_selection_rows(&empty, &state, false),
        vec![SelectionRow::Project(project)]
    );

    // The Attention list is unchanged: the idle pane is not in it, and the
    // working one waits under In progress.
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![
            SelectionRow::Project(project),
            SelectionRow::Card(unread),
            SelectionRow::Card(working),
        ]
    );
    assert_eq!(
        in_progress_rows(&model, &state, false)
            .into_iter()
            .map(InProgressRow::row)
            .collect::<Vec<_>>(),
        vec![SelectionRow::Card(working)]
    );
}

#[test]
fn hiding_checkouts_by_default_overrides_every_projects_own_mode() {
    use crate::settings::SidebarWorktrees::*;
    let (model, mut state, [idle, unread, working]) = hidden_checkouts_scene();
    let entry = &model.projects[0];
    let project = entry.id;
    let checkouts = [
        SelectionRow::Worktree(project, entry.worktrees[0].id),
        SelectionRow::Worktree(project, entry.worktrees[1].id),
    ];
    let panes = [idle, unread, working].map(SelectionRow::Card);
    state.apply(&SidebarAction::SetWorktreeDisplay {
        project_id: project,
        mode: Some(Expanded),
    });
    let cache = NavigationCache::default();
    let hidden = cache.generation(&model, &state);
    let rows = visible_selection_rows(&model, &state, false);
    assert_eq!(state.worktree_display(project), Hidden);
    assert!(checkouts.iter().all(|row| !rows.contains(row)), "{rows:?}");
    assert!(panes.iter().all(|row| rows.contains(row)), "{rows:?}");
    assert_eq!(
        state.worktree_display_overrides.get(&project),
        Some(&Expanded),
        "the project's own mode is kept"
    );

    // Shown again, the project's own mode applies as before.
    state.set_sidebar_worktrees(Grouped);
    assert_ne!(hidden, cache.generation(&model, &state), "the list redraws");
    let rows = visible_selection_rows(&model, &state, false);
    assert_eq!(state.worktree_display(project), Expanded);
    assert!(checkouts.iter().all(|row| rows.contains(row)), "{rows:?}");
    assert!(panes.iter().all(|row| !rows.contains(row)), "{rows:?}");

    // A project hiding its own checkouts lists its panes the same way.
    state.apply(&SidebarAction::SetWorktreeDisplay {
        project_id: project,
        mode: Some(Hidden),
    });
    let rows = visible_selection_rows(&model, &state, false);
    assert!(checkouts.iter().all(|row| !rows.contains(row)), "{rows:?}");
    assert!(panes.iter().all(|row| rows.contains(row)), "{rows:?}");
    assert_eq!(drawn_rows(&model, &state, false), rows);
}

#[test]
fn a_hidden_checkout_stays_hidden_when_current_and_its_pane_or_project_says_so() {
    let (model, mut state, [_, unread, _]) = hidden_checkouts_scene();
    let entry = &model.projects[0];
    let project = entry.id;
    let linked = &entry.worktrees[1];
    state.apply(&SidebarAction::OpenWorktree {
        project_id: project,
        worktree_id: linked.id,
        path: linked.path.clone(),
    });
    assert!(
        !visible_selection_rows(&model, &state, false)
            .contains(&SelectionRow::Worktree(project, linked.id))
    );
    // With no pane of it current, the project row marks the place.
    assert_eq!(
        header_selection(entry, &state, state.drawn_selection()),
        Some(SidebarSelection::Project(project))
    );
    state.follow_card(Some(unread));
    assert!(
        entry
            .panes
            .iter()
            .any(|card| card.id == unread && state.follows_card(card))
    );
    assert_eq!(
        header_selection(entry, &state, state.drawn_selection()),
        None,
        "the current pane's row marks it instead"
    );
}

#[test]
fn search_finds_pane_rows_by_title_or_checkout() {
    let (model, mut state, [idle, unread, working]) = hidden_checkouts_scene();
    let project = model.projects[0].id;
    let linked = model.projects[0].worktrees[1].label().to_owned();
    state.apply(&SidebarAction::SearchChanged(linked));
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![
            SelectionRow::Project(project),
            SelectionRow::Card(unread),
            SelectionRow::Card(working),
        ]
    );
    state.apply(&SidebarAction::SearchChanged("shell".into()));
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![SelectionRow::Project(project), SelectionRow::Card(idle)]
    );
}

#[test]
fn hidden_checkouts_leave_pull_requests_folders_and_unowned_news_alone() {
    let (mut model, mut state, [idle, unread, _]) = hidden_checkouts_scene();
    let project = model.projects[0].id;
    let linked = model.projects[0].worktrees[1].id;

    // The pull request filter lists pull requests by their checkouts.
    state.apply(&SidebarAction::SetFilter(SidebarFilter::PullRequests));
    let rows = visible_selection_rows(&model, &state, false);
    assert!(
        rows.contains(&SelectionRow::Worktree(project, linked)),
        "{rows:?}"
    );
    assert!(!rows.iter().any(|row| matches!(row, SelectionRow::Card(_))));
    assert_eq!(drawn_rows(&model, &state, false), rows);
    state.apply(&SidebarAction::SetFilter(SidebarFilter::All));

    // The unread news shows once: on its pane's row, or on the header
    // while a search leaves that row out. An unread badge carries its
    // count as text; the working badge beside Build has none.
    let renderer = headless_renderer();
    let unread_badges = |state: &SidebarState| {
        let names = accessible_names(&renderer, project_list(&model, state, false, false));
        let badges = names.iter().filter(|name| *name == "1").count();
        (badges, names)
    };
    let (badges, names) = unread_badges(&state);
    assert_eq!(badges, 1, "{names:?}");
    assert!(
        names
            .iter()
            .any(|name| name.starts_with("Open pane Review agent"))
    );
    state.apply(&SidebarAction::SearchChanged("build".into()));
    let (badges, names) = unread_badges(&state);
    assert_eq!(badges, 1, "{names:?}");
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("Open pane Review agent"))
    );
    state.apply(&SidebarAction::SearchChanged(String::new()));

    // A folder has no checkouts to hide, so it lists no pane rows.
    model.projects[0].kind = ProjectKind::Folder;
    assert!(!state.lists_panes(&model.projects[0]));
    let rows = visible_selection_rows(&model, &state, false);
    assert!(
        ![idle, unread]
            .iter()
            .any(|card| rows.contains(&SelectionRow::Card(*card))),
        "{rows:?}"
    );
}

/// The rail's project chip leaves news to the pane rows drawn under it,
/// and still carries what none of them does, as the expanded header
/// does: a search that leaves out the unread pane's row must not take
/// its news off the rail while checkouts are hidden.
#[test]
fn hidden_checkouts_keep_news_no_drawn_row_carries_on_the_rail_chip() {
    let (model, mut state, _) = hidden_checkouts_scene();
    let renderer = headless_renderer();
    // A rail badge draws an unread count, which its row's name carries
    // instead, so count the drawn text.
    let unread_badges = |state: &SidebarState| {
        let badges = drawn_texts(project_list(&model, state, true, false), COMPACT_WIDTH)
            .into_iter()
            .filter(|(text, _)| text == "1")
            .count();
        let names = accessible_names(&renderer, project_list(&model, state, true, false));
        (badges, names)
    };
    let (badges, names) = unread_badges(&state);
    assert_eq!(badges, 1, "on the pane's row alone: {names:?}");
    assert!(
        names
            .iter()
            .any(|name| name.starts_with("Open pane Review agent"))
    );

    state.apply(&SidebarAction::ToggleControls);
    state.apply(&SidebarAction::SearchChanged("build".into()));
    let (badges, names) = unread_badges(&state);
    assert!(
        !names
            .iter()
            .any(|name| name.starts_with("Open pane Review agent")),
        "the search leaves the unread pane out: {names:?}"
    );
    assert!(
        names.iter().any(|name| name.starts_with("Open pane Build")),
        "the chip has a drawn row under it: {names:?}"
    );
    assert_eq!(badges, 1, "the chip carries the news: {names:?}");
}

#[test]
fn a_quiet_alias_cannot_give_unowned_news_a_row() {
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![display_project()]);
    let linked = model.projects[0].worktrees[1].path.clone();
    let canonical = compact_attention_terminal(CardId::new());
    let mut alias = canonical.clone();
    alias.id = CardId::new();
    let durable = Arc::new(crate::model::WorkspaceState {
        card_scopes: [(alias.id, linked)].into_iter().collect(),
        cards: vec![canonical.clone(), alias].into(),
        ..Default::default()
    });
    let mut state = SidebarState::default();
    state.set_sidebar_worktrees(crate::settings::SidebarWorktrees::Hidden);
    state.observe_card_update(canonical.id);
    refresh_attention(&mut state, &mut model, &durable);
    assert!(model.projects[0].panes.is_empty());
}

#[test]
fn the_checkouts_toggle_says_what_it_does_in_every_project() {
    use iced_accessibility::accesskit::Toggled;
    let renderer = headless_renderer();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![display_project()]);
    let mut state = SidebarState::default();
    for compact in [false, true] {
        for (mode, pressed, tooltip) in [
            (
                crate::settings::SidebarWorktrees::Grouped,
                Toggled::False,
                "Hide checkouts",
            ),
            (
                crate::settings::SidebarWorktrees::Expanded,
                Toggled::False,
                "Hide checkouts",
            ),
            (
                crate::settings::SidebarWorktrees::Hidden,
                Toggled::True,
                "Show checkouts (hidden in every project)",
            ),
        ] {
            state.set_sidebar_worktrees(mode);
            assert_eq!(checkouts_toggle_tooltip(&state), tooltip);
            let mut element = header(&model, &state, compact, false, false, &shortcuts);
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(1280.0, 4_000.0)),
            );
            let nodes = element.as_widget().a11y_nodes(
                Layout::new(&node),
                &tree,
                cosmic::iced::advanced::mouse::Cursor::Unavailable,
            );
            let buttons: Vec<_> = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .map(|node| node.node())
                .filter(|node| node.label().is_some())
                .collect();
            let bell = buttons
                .iter()
                .position(|node| node.label() == Some("Show projects needing attention"))
                .expect("the bell");
            assert_eq!(buttons[bell].toggled(), Some(Toggled::False));
            let toggle = buttons.get(bell + 1).expect("beside the bell");
            assert_eq!(
                (toggle.label(), toggle.toggled()),
                (Some(CHECKOUTS_TOGGLE_NAME), Some(pressed)),
                "{mode:?}, compact {compact}: the name stays and the pressed state says it"
            );
            assert!(
                !buttons.iter().any(|node| node
                    .label()
                    .is_some_and(|name| name.to_lowercase().contains("main"))),
                "{mode:?}, compact {compact}"
            );
            let focusable = published(&mut header(
                &model, &state, compact, false, false, &shortcuts,
            ))
            .focusable;
            let toggle = focusable
                .iter()
                .position(|id| *id == checkouts_toggle_focus_id())
                .expect("the toggle takes keyboard focus");
            assert_eq!(
                focusable.get(toggle.wrapping_sub(1)),
                Some(&filter_id(SidebarFilter::Attention)),
                "it follows the bell"
            );
        }
    }
}

/// Only the All view lists checkouts. The setting persists, so the
/// toggle stays live in the other views and says which view it changes.
#[test]
fn the_checkouts_toggle_names_the_all_view_from_the_others() {
    let mut state = SidebarState::default();
    for filter in [SidebarFilter::Attention, SidebarFilter::PullRequests] {
        state.filter = filter;
        for (mode, tooltip) in [
            (
                crate::settings::SidebarWorktrees::Grouped,
                "Hide checkouts in the All view",
            ),
            (
                crate::settings::SidebarWorktrees::Hidden,
                "Show checkouts in the All view (hidden in every project)",
            ),
        ] {
            state.set_sidebar_worktrees(mode);
            assert_eq!(checkouts_toggle_tooltip(&state), tooltip, "{filter:?}");
        }
    }
}

#[test]
fn worktree_display_operations_survive_search_filters_and_unreachable_projects() {
    let renderer = headless_renderer();
    for phase in [
        WorktreeOperationPhase::Working,
        WorktreeOperationPhase::Failed,
    ] {
        for unreachable in [None, Some("Missing checkout".to_owned())] {
            let mut entry = display_project();
            entry.unreachable = unreachable;
            entry.operations.push(WorktreeOperationRow {
                id: crate::model::OperationId::new(),
                label: "Pending worktree".into(),
                detail: "Creating a worktree".into(),
                phase,
                cancellable: phase == WorktreeOperationPhase::Working,
                reopenable: false,
            });
            let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
            for mode in crate::settings::SidebarWorktrees::ALL {
                for filter in [
                    SidebarFilter::All,
                    SidebarFilter::Attention,
                    SidebarFilter::PullRequests,
                ] {
                    let mut state = SidebarState::default();
                    state.set_sidebar_worktrees(mode);
                    state.apply(&SidebarAction::SetFilter(filter));
                    state.apply(&SidebarAction::SearchChanged("no matching project".into()));
                    for compact in [false, true] {
                        let names = accessible_names(
                            &renderer,
                            project_list(&model, &state, compact, false),
                        );
                        assert!(
                            names.iter().any(|name| name.contains("Pending worktree")),
                            "{mode:?} {filter:?} {phase:?} compact={compact}"
                        );
                    }
                }
            }
        }
    }
}

#[test]
fn worktree_display_count_and_attention_fit_wide_and_narrow_rows() {
    use cosmic::iced::advanced::widget::Operation;
    struct Bounds {
        status_id: widget::Id,
        status: Option<Rectangle>,
        count: Option<Rectangle>,
    }
    impl Operation for Bounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
            if id == Some(&self.status_id) {
                self.status = Some(bounds);
            }
        }
        fn text(&mut self, _: Option<&widget::Id>, bounds: Rectangle, text: &str) {
            if text == "126" {
                self.count = Some(bounds);
            }
        }
    }
    let renderer = headless_renderer();
    let mut project = display_project();
    for index in 0..125 {
        project
            .worktrees
            .push(worktree(index + 100, "topic", false));
    }
    let mut state = SidebarState::default();
    state.set_agent_activity(
        CardId::new(),
        project.id,
        project.worktrees[1].id,
        crate::agent::Activity {
            provider: crate::agent::Provider::Claude,
            state: crate::agent::ActivityState::Waiting,
        },
    );
    for width in [180.0, 260.0] {
        let mut row = worktree_group_row(&project, &state, false, false);
        let mut tree = Tree::new(&row);
        let node = row.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 40.0)),
        );
        let mut bounds = Bounds {
            status_id: entity_id("worktrees.attention", project.id),
            status: None,
            count: None,
        };
        row.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
        let status = bounds.status.unwrap();
        let count = bounds.count.unwrap();
        assert!(status.width >= f32::from(ROW_ICON_SIZE));
        assert!(
            count.x + count.width <= status.x,
            "count overlaps attention at {width}px: {count:?}, {status:?}"
        );
        assert!(status.x + status.width <= width);
    }
}

#[test]
fn worktree_display_disclosure_accepts_enter_and_space_and_modal_disables_both() {
    use cosmic::iced::{
        Event,
        advanced::{Shell, clipboard, widget::operation},
        keyboard, mouse,
    };
    let renderer = headless_renderer();
    let project = display_project();
    for modal in [false, true] {
        for key in [
            keyboard::Key::Named(keyboard::key::Named::Enter),
            keyboard::Key::Character(" ".into()),
        ] {
            let mut element = worktree_group_row(&project, &SidebarState::default(), false, modal);
            let mut tree = Tree::new(&element);
            let viewport = Rectangle::with_size(Size::new(260.0, 100.0));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut operation::focusable::focus::<()>(worktree_group_focus_id(project.id)),
            );
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &Event::Keyboard(keyboard::Event::KeyPressed {
                    key: key.clone(),
                    modified_key: key,
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                }),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            assert_eq!(
                messages,
                if modal {
                    vec![]
                } else {
                    vec![SidebarAction::ToggleWorktrees(project.id)]
                }
            );
        }
    }
}

#[test]
fn selection_order_matches_visible_rows_including_channels_and_collapses() {
    let mut entry = project();
    let channel = ChannelId::new();
    entry.channels.push(SidebarChannel::new(
        channel,
        "Conversation".into(),
        false,
        None,
        false,
        0,
        false,
        1,
    ));
    let id = entry.id;
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let mut state = expanded_worktree_state();
    let rows = visible_selection_rows(&model, &state, false);
    assert_eq!(rows[0], SelectionRow::Project(id));
    assert_eq!(rows[1], SelectionRow::Channel(channel));
    assert!(matches!(rows[2], SelectionRow::Worktree(..)));
    state.apply(&SidebarAction::ToggleProject(id));
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![SelectionRow::Project(id)]
    );
    assert_eq!(
        visible_selection_rows(&model, &state, true),
        vec![SelectionRow::Project(id)],
        "compact rail respects disclosure"
    );
    state.apply(&SidebarAction::SearchChanged("not-present".into()));
    assert!(visible_selection_rows(&model, &state, false).is_empty());
}

/// The rail draws the worktree the source-control panel is bound to as
/// current, so keyboard order reaches it too.
#[test]
fn compact_rail_keyboard_order_reaches_the_bound_worktree() {
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let project = project_id(1);
    let worktree = &model.projects[0].worktrees[1];
    let row = SelectionRow::Worktree(project, worktree.id);
    let mut state = SidebarState::default();
    assert!(
        !visible_selection_rows(&model, &state, true).contains(&row),
        "an idle worktree is off the rail"
    );
    state.followed_worktree = Some(ActiveWorktreeKey {
        project_id: project,
        worktree_id: worktree.id,
        canonical_path: worktree.path.clone(),
    });
    assert!(compact_rail_shows_worktree(&state, project, worktree));
    assert!(visible_selection_rows(&model, &state, true).contains(&row));
}

/// Every rail row is one fixed cell whatever its status says, and nothing
/// it draws - glyph, count pill or spinner - leaves that cell. The rail's
/// old status slot let a count wrap under its icon and a spinner spill
/// over the next row.
#[test]
fn rail_cells_keep_one_size_and_their_badges_inside_it() {
    use cosmic::iced::Rectangle;
    fn assert_within(layout: Layout<'_>, cell: Rectangle, case: &str) {
        let bounds = layout.bounds();
        assert!(
            bounds.x >= cell.x - 0.01
                && bounds.y >= cell.y - 0.01
                && bounds.x + bounds.width <= cell.x + cell.width + 0.01
                && bounds.y + bounds.height <= cell.y + cell.height + 0.01,
            "{case}: {bounds:?} leaves the cell {cell:?}"
        );
        for child in layout.children() {
            assert_within(child, cell, case);
        }
    }
    let renderer = headless_renderer();
    let summaries = [
        AttentionSummary::default(),
        AttentionSummary {
            unread: 1,
            ..AttentionSummary::default()
        },
        AttentionSummary {
            unread: 120,
            working: 2,
            ..AttentionSummary::default()
        },
        AttentionSummary {
            waiting: 1,
            working: 1,
            ..AttentionSummary::default()
        },
        AttentionSummary {
            unread: 12,
            failed: 3,
            ..AttentionSummary::default()
        },
    ];
    for summary in summaries {
        let channel = SidebarChannel::new(
            ChannelId::new(),
            "Investigation".into(),
            summary.unread != 0,
            None,
            false,
            4,
            true,
            1,
        )
        .with_attention(summary);
        let case = format!("{summary:?}");
        let mut element = channel_view(
            &channel,
            Listing::Live,
            channel.attention,
            false,
            true,
            Some(1),
            "",
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(rail::CELL_WIDTH, 400.0)),
        );
        assert_eq!(
            node.bounds().size(),
            Size::new(rail::CELL_WIDTH, rail::CELL_HEIGHT),
            "{case}"
        );
        let layout = Layout::new(&node);
        assert_within(layout, layout.bounds(), &case);
    }
}

#[test]
fn channel_hover_delete_targets_its_row_and_the_rail_cell_opens_it() {
    use cosmic::iced::{
        Event, Point, Rectangle,
        advanced::{Shell, clipboard},
        mouse,
    };
    let channel = SidebarChannel::new(
        ChannelId::new(),
        "Investigation".into(),
        false,
        None,
        false,
        1,
        true,
        1,
    );
    for (compact, modal) in [(false, false), (true, false), (false, true), (true, true)] {
        let renderer = headless_renderer();
        let mut element = channel_view(
            &channel,
            Listing::Live,
            channel.attention,
            modal,
            compact,
            Some(1),
            "",
        );
        let mut tree = Tree::new(&element);
        let width = if compact { rail::CELL_WIDTH } else { 240.0 };
        let viewport = Rectangle::with_size(Size::new(width, WORKTREE_ROW_HEIGHT));
        let limits = layout::Limits::new(Size::ZERO, viewport.size());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let cursor = mouse::Cursor::Available(Point::new(width - 8.0, 16.0));
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::CursorMoved {
                position: cursor.position().unwrap(),
            }),
            Layout::new(&node),
            cursor,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        for event in [
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(event),
                Layout::new(&node),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        // The rail has no hover delete: the whole cell opens the channel.
        assert_eq!(
            messages,
            if modal {
                Vec::new()
            } else if compact {
                vec![SidebarAction::OpenChannel {
                    channel_id: channel.id,
                }]
            } else {
                vec![SidebarAction::DeleteChannel {
                    channel_id: channel.id,
                }]
            },
            "compact={compact}, modal={modal}"
        );
    }
}

/// Lays `element` out at `size` with the pointer at `hover`, then returns
/// the bounds of the control named `label` and of the unnamed glyphs drawn
/// inside it, as the accessibility tree publishes them.
fn icon_glyphs_in_control(
    mut element: Element<'_, SidebarAction>,
    size: Size,
    hover: cosmic::iced::Point,
    label: &str,
) -> (cosmic::iced::Rectangle, Vec<cosmic::iced::Rectangle>) {
    use cosmic::iced::{
        Event, Point, Rectangle,
        advanced::{Shell, clipboard},
        mouse,
    };
    let renderer = headless_renderer();
    let mut tree = Tree::new(&element);
    let viewport = Rectangle::with_size(size);
    let limits = layout::Limits::new(Size::ZERO, size);
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let cursor = mouse::Cursor::Available(hover);
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::CursorMoved { position: hover }),
        Layout::new(&node),
        cursor,
        &renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut Vec::new()),
        &viewport,
    );
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let nodes = element
        .as_widget()
        .a11y_nodes(Layout::new(&node), &tree, cursor);
    let published: Vec<_> = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter_map(|node| {
            let rect = node.node().bounds()?;
            let bounds = Rectangle::new(
                Point::new(rect.x0 as f32, rect.y0 as f32),
                Size::new((rect.x1 - rect.x0) as f32, (rect.y1 - rect.y0) as f32),
            );
            Some((node.node().label().map(ToOwned::to_owned), bounds))
        })
        .collect();
    let control = published
        .iter()
        .find(|(name, _)| name.as_deref() == Some(label))
        .unwrap_or_else(|| panic!("{label} is published"))
        .1;
    let glyphs = published
        .iter()
        .filter(|(name, glyph)| {
            name.is_none()
                && *glyph != control
                && control.contains(glyph.position())
                && control.contains(Point::new(glyph.x + glyph.width, glyph.y + glyph.height))
        })
        .map(|(_, glyph)| *glyph)
        .collect();
    (control, glyphs)
}

/// What a channel row lays out at `width`, with the pointer over the row
/// or away from it.
struct ChannelRowLayout {
    /// The title's text, as laid out.
    title: Rectangle,
    /// The facepile's faces, in their order in the pile.
    faces: Vec<Rectangle>,
    /// Every text the row lays out.
    texts: Vec<String>,
    /// The count after the faces, if any.
    count: Option<Rectangle>,
    /// The delete button laid over the row's end.
    delete: Rectangle,
    /// What a click where the delete button appears does.
    end_click: Vec<SidebarAction>,
}

/// The faces with the given ids and the texts an operation finds in a laid
/// out element.
struct FacesAndTexts {
    faces: Vec<Id>,
    found: Vec<(usize, Rectangle)>,
    texts: Vec<(String, Rectangle)>,
    /// The focusable controls, by id.
    controls: Vec<(Id, Rectangle)>,
}

impl FacesAndTexts {
    /// What `element`, laid out as `node`, draws: the faces named by
    /// `faces`, in that order, and every text.
    fn in_layout(
        element: &mut Element<'_, SidebarAction>,
        tree: &mut Tree,
        node: &layout::Node,
        faces: Vec<Id>,
    ) -> Self {
        let mut collect = Self {
            faces,
            found: Vec::new(),
            texts: Vec::new(),
            controls: Vec::new(),
        };
        element.as_widget_mut().operate(
            tree,
            Layout::new(node),
            &headless_renderer(),
            &mut collect,
        );
        collect.found.sort_by_key(|(index, _)| *index);
        collect
    }

    fn faces(&self) -> Vec<Rectangle> {
        self.found.iter().map(|(_, bounds)| *bounds).collect()
    }
}

impl cosmic::iced::advanced::widget::Operation for FacesAndTexts {
    fn traverse(
        &mut self,
        visit: &mut dyn FnMut(&mut dyn cosmic::iced::advanced::widget::Operation),
    ) {
        visit(self);
    }
    fn container(&mut self, id: Option<&Id>, bounds: Rectangle) {
        if let Some(index) = id.and_then(|id| self.faces.iter().position(|face| face == id)) {
            self.found.push((index, bounds));
        }
    }
    fn text(&mut self, _: Option<&Id>, bounds: Rectangle, text: &str) {
        self.texts.push((text.to_owned(), bounds));
    }
    fn focusable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        _: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
    ) {
        if let Some(id) = id {
            self.controls.push((id.clone(), bounds));
        }
    }
}

fn channel_row_layout(channel: &SidebarChannel, width: f32, hovered: bool) -> ChannelRowLayout {
    use cosmic::iced::{
        Event, Point,
        advanced::{Shell, clipboard},
        mouse,
    };
    let mut element = channel_view(
        channel,
        Listing::Live,
        channel.attention,
        false,
        false,
        Some(1),
        "",
    );
    let renderer = headless_renderer();
    let mut tree = Tree::new(&element);
    let size = Size::new(width, WORKTREE_ROW_HEIGHT);
    let limits = layout::Limits::new(Size::ZERO, size);
    let mut node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    // Over the row's end, where its delete button shows.
    let at = Point::new(width - 8.0, WORKTREE_ROW_HEIGHT / 2.0);
    if hovered {
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::CursorMoved { position: at }),
            Layout::new(&node),
            mouse::Cursor::Available(at),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut Vec::new()),
            &Rectangle::with_size(size),
        );
        node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
    }
    let faces = (0..members::MAX_FACES)
        .map(|index| channel_face_id(channel.id, index))
        .collect();
    let collect = FacesAndTexts::in_layout(&mut element, &mut tree, &node, faces);
    let mut end_click = Vec::new();
    if hovered {
        for event in [
            mouse::Event::ButtonPressed(mouse::Button::Left),
            mouse::Event::ButtonReleased(mouse::Button::Left),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(event),
                Layout::new(&node),
                mouse::Cursor::Available(at),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut end_click),
                &Rectangle::with_size(size),
            );
        }
    }
    let title = collect
        .texts
        .iter()
        .find(|(text, _)| *text == channel.title)
        .unwrap_or_else(|| panic!("{:?} is laid out", channel.title))
        .1;
    let delete = collect
        .controls
        .iter()
        .find(|(id, _)| *id == channel.delete_id)
        .expect("the row mounts its delete button")
        .1;
    ChannelRowLayout {
        title,
        faces: collect.faces(),
        count: collect
            .texts
            .iter()
            .find(|(text, _)| text.starts_with('+'))
            .map(|(_, bounds)| *bounds),
        texts: collect.texts.into_iter().map(|(text, _)| text).collect(),
        delete,
        end_click,
    }
}

/// A channel titled `title` whose members are `agents` and `terminals`.
fn channel_with(
    title: &str,
    agents: &[(crate::agent::Provider, ChannelMemberState)],
    terminals: usize,
) -> SidebarChannel {
    let mut members = ChannelMembers::default();
    for (provider, state) in agents {
        members.add_agent(*provider, *state);
    }
    for _ in 0..terminals {
        members.add_terminal();
    }
    SidebarChannel::new(
        ChannelId::new(),
        title.into(),
        false,
        None,
        false,
        1,
        true,
        1,
    )
    .with_members(members)
}

/// How wide a channel row's title is with nothing cut from it.
fn natural_title_width(title: &str) -> f32 {
    let (font, emphasis) = search_label_fonts(false);
    search_spans_width(title, "", CHILD_ROW_TEXT_SIZE, font, emphasis).ceil()
}

/// Claude Code at work with Codex, Oh My Pi and a terminal.
fn crowded(title: &str) -> SidebarChannel {
    use crate::agent::Provider;
    channel_with(
        title,
        &[
            (Provider::Claude, ChannelMemberState::Working),
            (Provider::Codex, ChannelMemberState::Idle),
            (Provider::Omp, ChannelMemberState::Idle),
        ],
        1,
    )
}

/// Hovering a channel row reveals its delete button over the row's end
/// rather than beside it, so at no sidebar width does the pointer entering
/// the row move its title or any face, or change how many faces there are.
/// The pointer rests where the button shows, and a click there deletes the
/// channel, so the hovered layout is the one with the button showing.
#[test]
fn hovering_a_channel_row_moves_nothing_in_it() {
    for title in [
        "Channel switch perf",
        "Make the sidebar facepile readable at a glance",
    ] {
        let channel = crowded(title);
        let mut counts = BTreeSet::new();
        for width in (120..=360).step_by(2) {
            let width = width as f32;
            let resting = channel_row_layout(&channel, width, false);
            let hovered = channel_row_layout(&channel, width, true);
            assert_eq!(
                (resting.title, &resting.faces),
                (hovered.title, &hovered.faces),
                "{title:?} at {width}px"
            );
            assert_eq!(
                hovered.end_click,
                vec![SidebarAction::DeleteChannel {
                    channel_id: channel.id
                }],
                "the pointer is over the row, and its delete button shows at {width}px"
            );
            counts.insert(resting.faces.len());
        }
        assert!(
            counts.len() > 1,
            "{title:?}: the widths cover more than one pile: {counts:?}"
        );
    }
}

/// A title that fits its row is never cut for the facepile: where the
/// pile does not fit beside it, the pile goes.
#[test]
fn a_channel_title_that_fits_stays_whole() {
    let title = "Diff follows editor font";
    let channel = crowded(title);
    let natural = natural_title_width(title);
    let mut whole_without_faces = false;
    let mut whole_with_faces = false;
    for width in [220.0, 260.0, 300.0, 360.0] {
        let row = channel_row_layout(&channel, width, false);
        assert_eq!(row.title.width, natural, "whole at {width}px");
        whole_without_faces |= row.faces.is_empty();
        whole_with_faces |= !row.faces.is_empty();
        for face in &row.faces {
            assert!(face.x >= row.title.x + natural, "after the title: {face:?}");
        }
    }
    assert!(
        whole_without_faces && whole_with_faces,
        "the widths cover a row with and without room for the pile"
    );
}

/// A title too long for its row is cut anyway, so it shares the row with
/// one face and the count of the rest, as long as enough of it shows.
#[test]
fn a_long_channel_title_keeps_one_face_and_the_count() {
    let title = "Make the sidebar facepile readable at a glance in every theme";
    let channel = crowded(title);
    let natural = natural_title_width(title);
    for width in [220.0, 260.0] {
        let row = channel_row_layout(&channel, width, false);
        assert!(
            row.title.width < natural,
            "cut at {width}px: {:?}",
            row.title
        );
        assert!(row.title.width >= members::MIN_TITLE, "{:?}", row.title);
        assert_eq!(row.faces.len(), 1, "one face at {width}px");
        assert!(
            row.texts.iter().any(|text| text == "+2"),
            "and the count at {width}px: {:?}",
            row.texts
        );
    }
    let cramped = channel_row_layout(&channel, 110.0, false);
    assert!(
        cramped.faces.is_empty() && !cramped.texts.iter().any(|text| text.starts_with('+')),
        "too little title to share: no pile, and never a count alone: {:?}",
        cramped.texts
    );
}

/// One other member is drawn as its face alone, never counted as "+1".
#[test]
fn one_other_channel_member_shows_as_its_face() {
    use crate::agent::Provider;
    let channel = channel_with(
        "Shell and agent",
        &[(Provider::Claude, ChannelMemberState::Idle)],
        1,
    );
    for width in [220.0, 260.0] {
        let row = channel_row_layout(&channel, width, false);
        assert_eq!(row.faces.len(), 1, "at {width}px");
        assert!(
            !row.texts.iter().any(|text| text.starts_with('+')),
            "{:?}",
            row.texts
        );
    }
}

/// A channel whose members are all of one kind still shows who else is
/// there: the pile stands for the other members whatever their kind.
#[test]
fn a_channel_of_one_kind_shows_its_other_members() {
    use crate::agent::Provider;
    let claude = |state| (Provider::Claude, state);
    for (channel, faces, count) in [
        (channel_with("Shell farm", &[], 10), 1, Some("+8")),
        (
            channel_with(
                "Release prep",
                &[
                    claude(ChannelMemberState::Working),
                    claude(ChannelMemberState::Idle),
                    claude(ChannelMemberState::Idle),
                ],
                0,
            ),
            2,
            None,
        ),
        (
            channel_with(
                "Pair",
                &[
                    claude(ChannelMemberState::Working),
                    claude(ChannelMemberState::Idle),
                ],
                0,
            ),
            1,
            None,
        ),
    ] {
        let row = channel_row_layout(&channel, 260.0, false);
        assert_eq!(row.faces.len(), faces, "{:?}", channel.title);
        assert_eq!(
            row.texts
                .iter()
                .find(|text| text.starts_with('+'))
                .map(String::as_str),
            count,
            "{:?}",
            channel.title
        );
        assert_eq!(row.title.width, natural_title_width(&channel.title));
    }
}

/// The delete button a hover lays over a channel row's end covers only the
/// slot the row keeps for it, never the facepile or the title, so no face
/// or count is half hidden while the pointer is on the row.
#[test]
fn a_channel_rows_delete_button_never_covers_its_facepile() {
    use crate::agent::Provider;
    let channels = [
        crowded("Channel switch perf"),
        crowded("Investigate why switching channels stalls with many agents open"),
        channel_with("Shell farm", &[], 10),
        channel_with(
            "Shell and agent",
            &[(Provider::Claude, ChannelMemberState::Idle)],
            1,
        ),
    ];
    for channel in &channels {
        let mut piled = false;
        for width in (120..=360).step_by(2) {
            let width = width as f32;
            let row = channel_row_layout(channel, width, true);
            let pile = row.faces.iter().chain(&row.count);
            for bounds in pile.chain([&row.title]) {
                assert!(
                    bounds.x + bounds.width <= row.delete.x,
                    "{:?} at {width}px: {bounds:?} runs under {:?}",
                    channel.title,
                    row.delete
                );
            }
            piled |= !row.faces.is_empty();
        }
        assert!(piled, "{:?} shows a pile at some width", channel.title);
    }
}

/// A title followed by a facepile sits on the same pixel row as a title on
/// its own, rather than between two, where it would be drawn blurred.
#[test]
fn a_channel_title_keeps_its_line_beside_a_facepile() {
    use crate::agent::Provider;
    let title = "Diff follows editor font";
    let plain = SidebarChannel::new(
        ChannelId::new(),
        title.into(),
        false,
        None,
        false,
        1,
        true,
        1,
    );
    let piled = channel_with(
        title,
        &[
            (Provider::Claude, ChannelMemberState::Working),
            (Provider::Codex, ChannelMemberState::Idle),
        ],
        0,
    );
    let alone = channel_row_layout(&plain, 260.0, false).title;
    let beside = channel_row_layout(&piled, 260.0, false);
    assert_eq!(beside.faces.len(), 1);
    assert_eq!(
        (alone.y, alone.height),
        (beside.title.y, beside.title.height),
        "{alone:?} {:?}",
        beside.title
    );
}

/// The workspace switcher follows a channel's name with the same facepile
/// as its sidebar row, sharing the name's line by the same rules, and shows
/// the name alone where the row would.
#[test]
fn a_channel_name_elsewhere_takes_the_rows_facepile() {
    let faces_beside = |channel: &SidebarChannel, width: f32| {
        let name = channel.title.clone();
        let mut element = channel_name_with_company(
            channel,
            &name,
            RowInk::Full,
            |width| {
                widget::text(name.clone())
                    .width(Length::Fixed(width))
                    .into()
            },
            |index| Id::new(format!("elsewhere.{index}")),
        )?;
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &headless_renderer(),
            &layout::Limits::new(Size::ZERO, Size::new(width, WORKTREE_ROW_HEIGHT)),
        );
        let ids = (0..members::MAX_FACES)
            .map(|index| Id::new(format!("elsewhere.{index}")))
            .collect();
        Some(
            FacesAndTexts::in_layout(&mut element, &mut tree, &node, ids)
                .faces()
                .len(),
        )
    };
    use crate::agent::Provider;
    let lone = channel_with(
        "Release prep",
        &[(Provider::Claude, ChannelMemberState::Working)],
        0,
    );
    assert_eq!(faces_beside(&lone, 260.0), None);
    assert_eq!(
        faces_beside(&channel_with("Shell farm", &[], 10), 260.0),
        Some(1)
    );
    assert_eq!(faces_beside(&crowded("Review sweep"), 260.0), Some(3));
    assert_eq!(
        faces_beside(
            &crowded("Make the sidebar facepile readable at a glance in every theme"),
            260.0
        ),
        Some(1)
    );
}

fn assert_glyph_centered(
    label: &str,
    control: cosmic::iced::Rectangle,
    glyphs: &[cosmic::iced::Rectangle],
) {
    let [glyph] = glyphs else {
        panic!("{label}: expected one glyph inside {control:?}, found {glyphs:?}");
    };
    let (glyph, control) = (glyph.center(), control.center());
    assert!(
        (glyph.x - control.x).abs() <= 0.5 && (glyph.y - control.y).abs() <= 0.5,
        "{label}: glyph centered at {glyph:?}, but its hit area is centered at {control:?}"
    );
}

#[test]
fn row_quick_delete_centers_its_glyph_in_the_whole_hit_area() {
    let channel = SidebarChannel::new(
        ChannelId::new(),
        "WhatsApp conversation export".into(),
        false,
        None,
        false,
        1,
        true,
        1,
    );
    let label = "Delete channel WhatsApp conversation export";
    let width = 240.0;
    let hover = cosmic::iced::Point::new(width - 8.0, WORKTREE_ROW_HEIGHT / 2.0);
    let (control, glyphs) = icon_glyphs_in_control(
        channel_view(
            &channel,
            Listing::Live,
            channel.attention,
            false,
            false,
            Some(1),
            "",
        ),
        Size::new(width, WORKTREE_ROW_HEIGHT),
        hover,
        label,
    );
    assert_eq!(
        control.width, ICON_CONTROL_SIZE,
        "the hit area keeps its reserved slot"
    );
    assert_glyph_centered(label, control, &glyphs);

    let mut worktree = worktree(3, "feature", false);
    let mut pr = project().pull_requests.remove(0);
    pr.state = Some(WorktreePullRequestState::Merged);
    worktree.set_pull_request(pr);
    let (control, glyphs) = icon_glyphs_in_control(
        worktree_view(
            project_id(1),
            &worktree,
            false,
            false,
            None,
            false,
            true,
            &[],
            WorktreeRowState::default(),
            false,
            Some(1),
            "",
            true,
        ),
        Size::new(240.0, WORKTREE_ROW_HEIGHT),
        cosmic::iced::Point::new(232.0, WORKTREE_ROW_HEIGHT / 2.0),
        "Delete merged worktree feature",
    );
    assert_glyph_centered("Delete merged worktree feature", control, &glyphs);
}

#[test]
fn worktree_changes_icons_can_be_hidden_without_hiding_activity() {
    let renderer = headless_renderer();
    let mut worktree = worktree(3, "feature", false);
    worktree.chrome.dirty_badge = Some("3 changed files".into());
    fn badge_count(node: &layout::Node) -> usize {
        usize::from(
            node.children().is_empty()
                && node.bounds().size()
                    == Size::new(f32::from(ROW_ICON_SIZE), f32::from(ROW_ICON_SIZE)),
        ) + node.children().iter().map(badge_count).sum::<usize>()
    }
    {
        let compact = false;
        let row = |show| {
            worktree_view(
                project_id(1),
                &worktree,
                false,
                false,
                None,
                false,
                false,
                &[],
                WorktreeRowState::default(),
                compact,
                None,
                "",
                show,
            )
        };
        let counts = [true, false].map(|show| {
            let mut element = row(show);
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(380.0, WORKTREE_ROW_HEIGHT)),
            );
            badge_count(&node)
        });
        assert_eq!(
            counts[0],
            counts[1] + 1,
            "only the changes badge is removed"
        );
        assert_eq!(
            accessible_names(&renderer, row(true)),
            accessible_names(&renderer, row(false)),
            "dirty status remains available to assistive technology"
        );
        assert!(agent_indicator(None, true).is_some());
    }
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let mut state = SidebarState::default();
    let cache = NavigationCache::default();
    let before = cache.generation(&model, &state);
    state.set_show_worktree_changes_icons(false);
    assert_ne!(before, cache.generation(&model, &state));
}

#[test]
fn only_merged_nonprimary_worktree_rows_expose_delete_quick_action() {
    let renderer = headless_renderer();
    let mut worktree = worktree(3, "feature", false);
    let mut pr = project().pull_requests.remove(0);
    for (state, primary, expected) in [
        (WorktreePullRequestState::Open, false, false),
        (WorktreePullRequestState::Closed, false, false),
        (WorktreePullRequestState::Merged, true, false),
        (WorktreePullRequestState::Merged, false, true),
    ] {
        pr.state = Some(state);
        worktree.set_pull_request(pr.clone());
        let mut element = worktree_view(
            project_id(1),
            &worktree,
            primary,
            false,
            None,
            false,
            true,
            &[],
            WorktreeRowState::default(),
            false,
            Some(1),
            "",
            true,
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(240.0, WORKTREE_ROW_HEIGHT)),
        );
        let nodes = element.as_widget().a11y_nodes(
            Layout::new(&node),
            &tree,
            cosmic::iced::mouse::Cursor::Unavailable,
        );
        let has_delete = nodes.root().iter().chain(nodes.children()).any(|node| {
            node.node()
                .label()
                .is_some_and(|name| name == "Delete merged worktree feature")
        });
        assert_eq!(has_delete, expected, "{state:?}, primary={primary}");
    }
}

#[test]
fn row_selection_is_explicit_and_delete_targets_the_focused_row_unless_selected() {
    use cosmic::iced::{
        Event, Point, Rectangle,
        advanced::{Shell, clipboard, widget::operation},
        keyboard, mouse,
    };
    let renderer = headless_renderer();
    let row = SelectionRow::Project(project_id(1));
    let state = SidebarState::default();
    let mut element = RowInteraction::selectable(
        button::custom(design::body("Project"))
            .id(selection_row_focus_id(row))
            .name("Project")
            .width(120)
            .height(32)
            .on_press(SidebarAction::OpenRepositoryPicker)
            .into(),
        row,
        &state,
        true,
    );
    let mut tree = Tree::new(&element);
    let viewport = Rectangle::with_size(Size::new(120.0, 32.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let key = || {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Delete),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Delete),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Delete),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        })
    };
    let dispatch = |element: &mut Element<'_, SidebarAction>, tree: &mut Tree, event: Event| {
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Available(Point::new(30.0, 16.0)),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        messages
    };
    assert!(
        dispatch(&mut element, &mut tree, key()).is_empty(),
        "editor focus must not delete rows"
    );
    dispatch(
        &mut element,
        &mut tree,
        Event::Keyboard(keyboard::Event::ModifiersChanged(keyboard::Modifiers::CTRL)),
    );
    assert_eq!(
        dispatch(
            &mut element,
            &mut tree,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        ),
        vec![SidebarAction::SelectRows {
            row,
            intent: SelectionIntent::Toggle
        }]
    );
    assert!(
        dispatch(
            &mut element,
            &mut tree,
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left))
        )
        .is_empty()
    );
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::focusable::focus::<()>(selection_row_focus_id(row)),
    );
    assert_eq!(
        dispatch(&mut element, &mut tree, key()),
        vec![SidebarAction::DeleteRow(row)],
        "a focused row outside the selection is its own target"
    );
    assert!(
        dispatch(
            &mut element,
            &mut tree,
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            })
        )
        .is_empty(),
        "Escape with nothing selected is left for the rest of the window"
    );
    dispatch(
        &mut element,
        &mut tree,
        Event::Keyboard(keyboard::Event::ModifiersChanged(
            keyboard::Modifiers::empty(),
        )),
    );
    assert_eq!(
        dispatch(
            &mut element,
            &mut tree,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        ),
        vec![SidebarAction::SelectRows {
            row,
            intent: SelectionIntent::Navigate
        }],
        "a plain click navigates and selects nothing"
    );

    // The same focused row, now a member of the selection, deletes the
    // whole selection instead.
    let mut selected = SidebarState::default();
    selected.select_rows(row, SelectionIntent::Toggle, &[row]);
    let mut element = RowInteraction::selectable(
        button::custom(design::body("Project"))
            .id(selection_row_focus_id(row))
            .width(120)
            .height(32)
            .on_press(SidebarAction::OpenRepositoryPicker)
            .into(),
        row,
        &selected,
        true,
    );
    let mut tree = Tree::new(&element);
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::focusable::focus::<()>(selection_row_focus_id(row)),
    );
    assert_eq!(
        dispatch(&mut element, &mut tree, key()),
        vec![SidebarAction::DeleteSelectedRows]
    );
}

/// Unvisited history candidates follow sidebar order, worktrees before
/// channels, and are not narrowed by a transient filter.
#[test]
fn the_workspace_cycle_follows_the_order_the_sidebar_lists() {
    let mut entry = project();
    let mut root = worktree(10, "main", false);
    root.path = entry.repository.clone();
    let mut older = worktree(11, "z-old", false);
    older.created_at_unix_ms = Some(100);
    let mut newer = worktree(12, "a-new", false);
    newer.created_at_unix_ms = Some(200);
    entry.worktrees = vec![older, root, newer];
    let first = ChannelId::new();
    let second = ChannelId::new();
    entry.channels = vec![
        SidebarChannel::new(first, "First".into(), false, None, false, 1, true, 1),
        SidebarChannel::new(second, "Second".into(), false, None, false, 1, true, 2),
    ];
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let state = SidebarState {
        filter: SidebarFilter::Attention,
        ..SidebarState::default()
    };

    let cycle = workspace_cycle(&model, &state);
    let id = |id| WorktreeId::from_uuid(Uuid::from_u128(id));
    let worktrees = cycle
        .iter()
        .filter_map(|action| match action {
            SidebarAction::OpenWorktree { worktree_id, .. } => Some(*worktree_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    // The primary worktree pins to the top, then newest first.
    assert_eq!(worktrees, vec![id(10), id(12), id(11)]);
    let channels = cycle
        .iter()
        .filter_map(|action| match action {
            SidebarAction::OpenChannel { channel_id } => Some(*channel_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(channels, vec![second, first]);
    // Channel rows precede worktrees in the sidebar.
    assert!(matches!(
        cycle.first(),
        Some(SidebarAction::OpenChannel { .. })
    ));
}

#[test]
fn worktree_sort_pins_root_and_uses_creation_evidence_before_stable_import_fallback() {
    let mut entry = project();
    let mut root = worktree(10, "main", false);
    root.path = entry.repository.clone();
    let mut older = worktree(11, "z-old", false);
    older.created_at_unix_ms = Some(100);
    let mut newer = worktree(12, "a-new", false);
    newer.created_at_unix_ms = Some(200);
    let imported = worktree(13, "imported", false);
    entry.worktrees = vec![imported, older, root, newer];
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let mut state = SidebarState::default();
    let order = |state: &SidebarState| {
        filtered_projects(&model, state, false)[0]
            .worktrees
            .iter()
            .map(|row| row.id)
            .collect::<Vec<_>>()
    };
    let id = |id| WorktreeId::from_uuid(Uuid::from_u128(id));
    assert_eq!(order(&state), vec![id(10), id(12), id(11), id(13)]);
    state.set_worktree_sort(crate::settings::WorktreeSort::Oldest);
    assert_eq!(order(&state), vec![id(10), id(11), id(12), id(13)]);
    state.set_worktree_sort(crate::settings::WorktreeSort::Alphabetical);
    assert_eq!(order(&state), vec![id(10), id(12), id(13), id(11)]);
}

#[test]
fn worktree_labels_keep_literal_branches_through_pr_lifecycle() {
    let branch = "Developer/browser-challenge-fix";
    let mut row = worktree(11, branch, false);
    assert_eq!(&*row.chrome.label, branch);
    assert!(row.matches("developer/"));
    assert!(row.matches("challenge"));
    assert!(!row.matches("handling"));
    let mut pr = project().pull_requests.remove(0);
    pr.title = "Fix browser challenge handling".into();
    row.set_pull_request(pr);
    assert_eq!(&*row.chrome.label, branch);
    assert!(row.matches("challenge"));
    assert!(row.matches("handling"));
    assert!(
        row.chrome
            .tooltip
            .contains("Fix browser challenge handling")
    );
    assert!(row.chrome.tooltip.contains(branch));
    row.clear_pull_request();
    assert_eq!(&*row.chrome.label, branch);
    assert!(row.matches("challenge"));
    assert!(!row.matches("handling"));
    assert_eq!(row.branch.as_deref(), Some(branch));
}

#[test]
fn project_filters_restore_native_scroll_without_viewport_messages() {
    use cosmic::iced::{
        Rectangle, Vector,
        advanced::widget::{Operation, operation},
    };

    #[derive(Default)]
    struct Offset(Option<f32>);
    impl Operation for Offset {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }

        fn scrollable(
            &mut self,
            id: Option<&Id>,
            _bounds: Rectangle,
            _content_bounds: Rectangle,
            translation: Vector,
            _state: &mut dyn operation::Scrollable,
        ) {
            if id == Some(&stable_id("projects.scroll")) {
                self.0 = Some(translation.y);
            }
        }
    }

    let renderer = headless_renderer();
    let make = |filter, height| {
        project_scroll(
            cosmic::iced::widget::Space::new()
                .width(200)
                .height(height)
                .into(),
            filter,
            row_interaction::StickyViewport::default(),
        )
    };
    let mut element = make(SidebarFilter::All, 1_000.0);
    let mut tree = Tree::new(&element);
    let limits = layout::Limits::new(Size::ZERO, Size::new(240.0, 200.0));
    let mut node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::scrollable::scroll_to::<()>(
            stable_id("projects.scroll"),
            operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(640.0),
            },
        ),
    );

    for (filter, height, expected, scroll_to) in [
        (SidebarFilter::Attention, 500.0, 0.0, Some(170.0)),
        (SidebarFilter::PullRequests, 1_000.0, 0.0, Some(330.0)),
        (SidebarFilter::All, 1_000.0, 640.0, None),
        (SidebarFilter::Attention, 500.0, 170.0, None),
        (SidebarFilter::All, 1_000.0, 640.0, None),
        (SidebarFilter::PullRequests, 1_000.0, 330.0, None),
        (SidebarFilter::Attention, 0.0, 0.0, None),
        (SidebarFilter::All, 1_000.0, 640.0, None),
    ] {
        element = make(filter, height);
        tree.diff(&mut element);
        node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut offset = Offset::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut offset);
        assert_eq!(offset.0, Some(expected), "{filter:?}");
        if let Some(y) = scroll_to {
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut operation::scrollable::scroll_to::<()>(
                    stable_id("projects.scroll"),
                    operation::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(y),
                    },
                ),
            );
        }
    }
}

#[test]
fn attention_forgetting_closed_surface_is_not_read_retention_and_keeps_other_updates() {
    let mut state = SidebarState::default();
    let closed = CardId::new();
    let other = CardId::new();
    let project = project_id(1);
    let worktree = WorktreeId::new();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.observe_agent_update(closed, project, worktree);
    state.observe_card_update(other);
    state.set_agent_activity(
        closed,
        project,
        worktree,
        crate::agent::Activity {
            provider: crate::agent::Provider::Codex,
            state: crate::agent::ActivityState::Waiting,
        },
    );
    state.forget_card_attention(closed);
    assert!(!state.has_card_update(closed));
    assert!(!state.has_agent_update(project, worktree));
    assert!(state.card_activity(closed).is_none());
    assert!(state.has_card_update(other));
}

#[test]
fn attention_alias_update_transfer_preserves_canonical_owner_without_old_worktree_attribution() {
    let mut state = SidebarState::default();
    let alias = CardId::new();
    let canonical = CardId::new();
    let project = project_id(1);
    let old = WorktreeId::new();
    let current = WorktreeId::new();
    state.observe_agent_update(alias, project, old);
    state.set_failed_updates(BTreeSet::from([alias]));
    state.transfer_card_update(alias, canonical);
    assert_eq!(state.card_attention(canonical).failed, 1);
    assert_eq!(state.worktree_attention(project, old).failed, 0);
    assert!(!state.has_card_update(alias));
    assert_eq!(state.agent_updates.get(&canonical), Some(&None));
    assert!(!state.has_agent_update(project, old));
    state.observe_agent_update(canonical, project, current);
    state.observe_agent_update(alias, project, old);
    state.transfer_card_update(alias, canonical);
    assert_eq!(
        state.agent_updates.get(&canonical),
        Some(&Some((project, current)))
    );
    state.transfer_card_update(canonical, canonical);
    assert!(state.has_card_update(canonical));
    assert_eq!(state.worktree_attention(project, current).failed, 1);
    state.forget_card_attention(canonical);
    assert_eq!(
        state.project_attention(project),
        AttentionSummary::default()
    );
}

#[test]
fn channel_attention_keeps_its_project_visible_without_unread_worktrees() {
    let mut entry = project();
    let channel_id = ChannelId::from_uuid(Uuid::from_u128(20));
    entry.channels.push(SidebarChannel::new(
        channel_id,
        "Investigate login".into(),
        true,
        Some(crate::agent::ActivityState::Waiting),
        true,
        2,
        true,
        1,
    ));
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let mut state = SidebarState::default();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.apply(&SidebarAction::SearchChanged("login".into()));
    let projects = filtered_projects(&model, &state, false);
    assert_eq!(projects.len(), 1);
    assert!(projects[0].worktrees.is_empty());
    assert_eq!(projects[0].channels[0].id, channel_id);
    model.projects[0].channels[0].unread = false;
    assert_eq!(filtered_projects(&model, &state, false).len(), 1);
    assert_eq!(
        model.projects[0].channels[0].activity,
        Some(crate::agent::ActivityState::Waiting),
    );
}

/// Every navigation row laid out in `layout`: its content row's bounds and
/// the bounds of the slots inside it, outermost row first.
///
/// A row is recognised by the grid's own leading icon slot - the only node
/// whose first child is laid out to exactly `ROW_ICON_WIDTH` - so these are
/// the positions the renderer draws at rather than the numbers the builders
/// were handed.
fn navigation_rows(
    layout: Layout<'_>,
    rows: &mut Vec<(cosmic::iced::Rectangle, Vec<cosmic::iced::Rectangle>)>,
) {
    let slots: Vec<cosmic::iced::Rectangle> =
        layout.children().map(|child| child.bounds()).collect();
    if slots
        .first()
        .is_some_and(|slot| (slot.width - ROW_ICON_WIDTH).abs() < 0.5)
    {
        rows.push((layout.bounds(), slots));
        return;
    }
    for child in layout.children() {
        navigation_rows(child, rows);
    }
}

fn grid_fixture() -> SidebarModel {
    let mut entry = project();
    // The first worktree is the repository itself, so `main` is the
    // primary row the reported screenshot compares against.
    entry.worktrees[0].path = entry.repository.clone();
    entry.channels.push(SidebarChannel::new(
        ChannelId::from_uuid(Uuid::from_u128(20)),
        "Investigate login".into(),
        false,
        None,
        false,
        1,
        true,
        1,
    ));
    SidebarModel::ready(SidebarMode::Expanded, vec![entry])
}

#[test]
fn long_worktree_labels_cannot_paint_over_status_or_badges() {
    use cosmic::iced::{
        Color, Rectangle,
        advanced::{
            mouse,
            renderer::{Headless, Style},
        },
    };

    let mut row = project().worktrees[1].clone();
    row.chrome.label =
        "feat(claws): add JMR Media and Kari with a very long worktree description".into();
    row.detached = true;
    row.locked = true;
    row.chrome.dirty_badge = Some("3 changed files".into());
    row.chrome.pull_request.as_mut().unwrap().checks = CheckStatus::Passing;

    let frame = |worktree: &SidebarWorktree, query: &str, width: f32| {
        let mut renderer = headless_renderer();
        let mut element = worktree_view(
            project_id(1),
            worktree,
            true,
            false,
            None,
            false,
            false,
            &[],
            WorktreeRowState {
                has_panes: true,
                ..WorktreeRowState::default()
            },
            false,
            None,
            query,
            true,
        );
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(width, WORKTREE_ROW_HEIGHT));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let mut rows = Vec::new();
        navigation_rows(Layout::new(&node), &mut rows);
        assert_eq!(rows.len(), 1);
        let (bounds, slots) = &rows[0];
        assert!(slots[1].width > 0.0);
        for pair in slots.windows(2) {
            assert!(pair[0].x + pair[0].width <= pair[1].x);
        }
        for slot in slots.iter().skip(2) {
            assert!(slot.x + slot.width <= bounds.x + bounds.width);
        }
        let status_start = slots[2].x.ceil() as usize;
        element.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::dark(),
            &Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        let size = Size::new(width as u32, WORKTREE_ROW_HEIGHT as u32);
        (renderer.screenshot(size, 1.0, Color::BLACK), status_start)
    };

    let mut empty = row.clone();
    empty.chrome.label = "".into();
    for width in [220.0, 320.0] {
        let (reference, status_start) = frame(&empty, "", width);
        for query in ["", "claws"] {
            let (painted, actual_start) = frame(&row, query, width);
            assert_eq!(actual_start, status_start);
            let stride = width as usize * 4;
            for y in 0..WORKTREE_ROW_HEIGHT as usize {
                let region = y * stride + status_start * 4..(y + 1) * stride;
                assert_eq!(
                    painted[region.clone()],
                    reference[region],
                    "label ink must stay clear of trailing indicators (width={width}, query={query:?}, y={y})",
                );
            }
        }
    }
}

#[test]
fn navigation_rows_share_one_content_grid() {
    let renderer = headless_renderer();
    let model = grid_fixture();
    let state = SidebarState::default();
    let laid_out = |element: &mut Element<'_, SidebarAction>, width: f32| {
        let mut tree = Tree::new(&*element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 512.0)),
        );
        let mut rows = Vec::new();
        navigation_rows(Layout::new(&node), &mut rows);
        rows
    };
    // `RowInteraction::group` stashes its header's layout for sticky
    // positioning and leaves a childless node in its place, so the header
    // is measured through the builder the group mounts, under the limits
    // the group hands it.
    let header = |compact: bool, width: f32| {
        laid_out(
            &mut project_identity_row(&model.projects[0], false, false, compact, false, "", None),
            width,
        )
    };
    let members = |compact: bool, width: f32| {
        let projects = filtered_projects(&model, &state, compact);
        laid_out(
            &mut project_view(
                &projects[0],
                true,
                compact,
                None,
                false,
                row_interaction::StickyViewport::default(),
            ),
            width,
        )
    };

    let width = design::NAVIGATION_DEFAULT_WIDTH;
    let expanded_header = header(false, width);
    let expanded = members(false, width);
    // The channel, then both worktrees.
    assert_eq!((expanded_header.len(), expanded.len()), (1, 3));
    let (head, channel, primary) = (&expanded_header[0].1, &expanded[0].1, &expanded[1].1);
    assert_eq!(
        (channel[0].x, channel[1].x),
        (head[0].x, head[1].x),
        "project, channel and worktree glyphs and labels share columns",
    );
    assert_eq!(expanded[0].0.x, expanded_header[0].0.x);
    assert_eq!(expanded[0].0.width, expanded_header[0].0.width);
    assert_eq!(
        (channel[0].x, channel[1].x),
        (primary[0].x, primary[1].x),
        "channel and worktree rows share one icon and one label column",
    );
    for (row, slots) in expanded_header.iter().chain(&expanded) {
        assert_eq!(slots[0].height, design::ROW_HEIGHT, "row icon slot height");
        assert_eq!(
            slots[0].center_y(),
            slots[1].center_y(),
            "icon and label sit on one baseline grid",
        );
        assert_eq!(row.height, design::ROW_HEIGHT);
    }

    let compact_header = header(true, rail::CELL_WIDTH);
    let compact = members(true, rail::CELL_WIDTH);
    // Quiet children and creation controls do not fill the compact rail.
    assert_eq!((compact_header.len(), compact.len()), (1, 0));
    for (row, slots) in compact_header.iter().chain(&compact) {
        assert_eq!(slots[0].x, compact_header[0].1[0].x, "one rail column");
        assert_eq!(row.height, design::ROW_HEIGHT);
    }
    for (row, _) in &compact {
        assert_eq!(
            row.x + row.width,
            rail::CELL_WIDTH,
            "every rail row reserves the same indicator slot",
        );
    }
}

#[test]
fn project_drag_label_stays_plain_and_inset_navigation_states_are_rounded() {
    let theme = cosmic::Theme::dark();
    let theme::Button::Custom {
        active,
        hovered,
        pressed,
        ..
    } = row_button_style::<false>(false)
    else {
        unreachable!();
    };
    assert!(active(false, &theme).background.is_none());
    assert!(hovered(false, &theme).background.is_none());
    assert!(pressed(false, &theme).background.is_none());
    let focused = hovered(true, &theme);
    assert!(
        focused.background.is_some(),
        "keyboard focus remains visible"
    );
    assert_eq!(
        focused.border_radius,
        theme.cosmic().corner_radii.radius_s.into()
    );
    let theme::Button::Custom {
        active,
        hovered,
        pressed,
        disabled,
    } = row_button_style::<true>(true)
    else {
        unreachable!();
    };
    for state in [
        active(false, &theme),
        active(true, &theme),
        hovered(false, &theme),
        hovered(true, &theme),
        pressed(false, &theme),
        pressed(true, &theme),
        disabled(&theme),
    ] {
        assert!(state.background.is_some());
        assert_eq!(
            state.border_radius,
            theme.cosmic().corner_radii.radius_s.into()
        );
    }
}

/// The whole point of drawing the header's commands over the row is that
/// they cost the title nothing until they appear. A reservation that is
/// always there is the same as no overlay at all.
#[test]
fn project_hover_commands_preserve_disclosure_position_and_reserve_only_visible_commands() {
    use cosmic::iced::{
        Event, Point, Rectangle,
        advanced::{Shell, clipboard},
        mouse,
    };
    let renderer = headless_renderer();
    let model = grid_fixture();
    let width = design::NAVIGATION_DEFAULT_WIDTH;
    let limits = layout::Limits::new(Size::ZERO, Size::new(width, PROJECT_HEADER_HEIGHT));
    let mut element = project_header_row(
        project_identity_row(&model.projects[0], false, false, false, false, "", None),
        vec![
            icon_action(
                Icon::ChannelAdd,
                "New channel",
                None,
                Id::new("test.channel"),
            ),
            icon_action(
                Icon::WorktreeAdd,
                "New worktree",
                None,
                Id::new("test.worktree"),
            ),
        ],
        project_icon_action(
            Icon::ChevronDown,
            "Collapse",
            None,
            Id::new("test.collapse"),
        ),
    );
    let mut tree = Tree::new(&element);
    let label_edge = |element: &mut Element<'_, SidebarAction>, tree: &mut Tree| {
        let node = element.as_widget_mut().layout(tree, &renderer, &limits);
        let mut rows = Vec::new();
        navigation_rows(Layout::new(&node), &mut rows);
        assert_eq!(rows.len(), 1, "one header row");
        let label = rows[0].1[1];
        let disclosure = Layout::new(&node).children().last().unwrap().bounds();
        (label.x + label.width, disclosure)
    };

    let resting = label_edge(&mut element, &mut tree);
    let content_edge = width - ICON_CONTROL_SIZE - RowGrid::project(false).gutter();
    assert_eq!(
        resting.0, content_edge,
        "a hidden control reserves nothing: the title runs to the row's own content edge",
    );

    let viewport = Rectangle::with_size(Size::new(width, PROJECT_HEADER_HEIGHT));
    let over = Point::new(width / 2.0, PROJECT_HEADER_HEIGHT / 2.0);
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::CursorMoved { position: over }),
        Layout::new(&node),
        mouse::Cursor::Available(over),
        &renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut Vec::new()),
        &viewport,
    );
    let hovered = label_edge(&mut element, &mut tree);
    assert_eq!(
        hovered.0,
        content_edge - 2.0 * ICON_CONTROL_SIZE,
        "once the commands are drawn the title stops exactly where they start, so nothing \
             visible runs under them",
    );
    assert_eq!(resting.1, hovered.1, "the disclosure target stays in place");
    assert_eq!(hovered.1.x + hovered.1.width, width);
}

/// Rows with open panes keep full strength and the rest recede. Channel
/// activity never colors the identity glyph; pull requests retain their
/// meaningful state colors.
#[test]
fn rows_without_panes_mute_and_channel_glyphs_stay_neutral() {
    use crate::agent::ActivityState;
    assert_eq!(RowInk::of(true, false), RowInk::Full);
    assert_eq!(RowInk::of(false, true), RowInk::Full);
    assert_eq!(RowInk::of(false, false), RowInk::Muted);

    let channel = |activity, unread, has_panes| {
        SidebarChannel::new(
            ChannelId::from_uuid(Uuid::from_u128(20)),
            "Investigate login".into(),
            false,
            activity,
            unread,
            0,
            has_panes,
            1,
        )
    };
    let tone = |channel: &SidebarChannel, listing| {
        WorkspaceGlyph::Channel.tone(channel_ink(channel, listing))
    };
    assert_eq!(
        tone(&channel(None, false, true), Listing::Live),
        GlyphTone::Ink(RowInk::Full)
    );
    assert_eq!(
        tone(&channel(None, false, false), Listing::Live),
        GlyphTone::Ink(RowInk::Muted)
    );
    assert_eq!(
        tone(
            &channel(Some(ActivityState::Waiting), false, true),
            Listing::Live
        ),
        GlyphTone::Ink(RowInk::Full),
        "waiting is shown by the indicator, not the glyph"
    );
    assert_eq!(
        tone(
            &channel(Some(ActivityState::Working), false, true),
            Listing::Live
        ),
        GlyphTone::Ink(RowInk::Full),
        "working is shown by the indicator, not the glyph"
    );
    assert_eq!(
        tone(&channel(None, true, false), Listing::Live),
        GlyphTone::Ink(RowInk::Full),
        "unread channels keep neutral identity glyphs"
    );
    assert!(
        channel(None, false, false)
            .accessible_name
            .contains("no open panes")
    );
    assert!(
        !channel(None, false, true)
            .accessible_name
            .contains("no open panes")
    );

    assert_eq!(
        tone(&channel(None, false, true), Listing::Read),
        GlyphTone::Ink(RowInk::Muted),
        "a channel read under Attention recedes though its panes are open"
    );

    assert_eq!(
        worktree_glyph_tone(None, RowInk::of(true, false)),
        GlyphTone::Ink(RowInk::Full)
    );
    assert_eq!(
        worktree_glyph_tone(None, RowInk::of(false, false)),
        GlyphTone::Ink(RowInk::Muted)
    );
    assert_eq!(
        worktree_glyph_tone(None, RowInk::of(false, true)),
        GlyphTone::Ink(RowInk::Full),
        "an unread update keeps a paneless worktree at full strength"
    );
    assert_eq!(
        worktree_glyph_tone(None, RowInk::of(true, false).listed(Listing::Read)),
        GlyphTone::Ink(RowInk::Muted),
        "a worktree read under Attention recedes though its panes are open"
    );
    assert_eq!(
        worktree_glyph_tone(Some(PullRequestTone::Neutral), RowInk::of(false, false)),
        GlyphTone::Ink(RowInk::Muted),
        "a neutral pull request carries no news"
    );
    assert_eq!(
        worktree_glyph_tone(Some(PullRequestTone::Draft), RowInk::of(true, false)),
        GlyphTone::Draft(RowInk::Full),
        "a draft stays a step quieter than its row, whichever the row is"
    );
    assert_eq!(
        worktree_glyph_tone(Some(PullRequestTone::Draft), RowInk::of(false, false)),
        GlyphTone::Draft(RowInk::Muted)
    );
    for tone in [PullRequestTone::Merged, PullRequestTone::Danger] {
        assert_eq!(
            worktree_glyph_tone(Some(tone), RowInk::of(false, false)),
            GlyphTone::PullRequest(tone)
        );
    }
}

#[test]
fn worktree_pane_occupancy_is_keyed_by_project_and_worktree() {
    let project = ProjectId::from_uuid(Uuid::from_u128(1));
    let other_project = ProjectId::from_uuid(Uuid::from_u128(2));
    let worktree = WorktreeId::from_uuid(Uuid::from_u128(3));
    let other = WorktreeId::from_uuid(Uuid::from_u128(4));
    let mut state = SidebarState::default();
    assert!(!state.worktree_has_panes(project, worktree));

    state.set_worktrees_with_panes(BTreeSet::from([(project, worktree)]));
    assert!(state.worktree_has_panes(project, worktree));
    assert!(!state.worktree_has_panes(project, other));
    assert!(!state.worktree_has_panes(other_project, worktree));

    state.set_worktrees_with_panes(BTreeSet::new());
    assert!(!state.worktree_has_panes(project, worktree));
}

#[test]
fn a_paneless_worktree_says_so_to_assistive_technology() {
    let row = worktree(1, "feature", false);
    assert!(
        worktree_accessible_name(&row, WorktreeRowState::default()).ends_with(", no open panes")
    );
    let with_panes = WorktreeRowState {
        has_panes: true,
        ..WorktreeRowState::default()
    };
    assert!(!worktree_accessible_name(&row, with_panes).contains("no open panes"));
}

#[test]
fn row_indicators_share_one_reserved_slot() {
    let renderer = headless_renderer();
    let channel = |activity, unread, chats| {
        SidebarChannel::new(
            ChannelId::from_uuid(Uuid::from_u128(20)),
            "Investigate login".into(),
            false,
            activity,
            unread,
            chats,
            chats > 0,
            1,
        )
    };
    let label_slot = |channel: &SidebarChannel| {
        let mut element = channel_view(
            channel,
            Listing::Live,
            channel.attention,
            false,
            false,
            Some(1),
            "",
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(
                Size::ZERO,
                Size::new(design::NAVIGATION_DEFAULT_WIDTH, design::ROW_HEIGHT),
            ),
        );
        let mut rows = Vec::new();
        navigation_rows(Layout::new(&node), &mut rows);
        assert_eq!(rows.len(), 1);
        rows[0].1[1]
    };
    let plain = label_slot(&channel(None, false, 1));
    for (state, row) in [
        (
            "a working spinner",
            channel(Some(crate::agent::ActivityState::Working), false, 1),
        ),
        ("an attention badge", channel(None, true, 1)),
        ("a chat count", channel(None, false, 4)),
    ] {
        let slot = label_slot(&row);
        assert_eq!(
            (slot.x, slot.width),
            (plain.x, plain.width),
            "{state} reflowed the label",
        );
    }
}

/// How far right anything inside the scroll viewport's padded content
/// reaches. The `filtered_scroll` decorator, the scrollable and its content
/// container all span the pane deliberately; the first node inside that
/// container's padding is at depth three.
fn content_right_edge(layout: Layout<'_>, depth: usize) -> f32 {
    let own = if depth >= 3 {
        layout.bounds().x + layout.bounds().width
    } else {
        f32::MIN
    };
    layout.children().fold(own, |edge, child| {
        edge.max(content_right_edge(child, depth + 1))
    })
}

#[test]
fn sidebar_content_clears_the_scrollbar_rail_inside_the_viewport() {
    let renderer = headless_renderer();
    let model = grid_fixture();
    let state = SidebarState::default();
    for (compact, width) in [
        (false, design::NAVIGATION_DEFAULT_WIDTH),
        (false, EXPANDED_WIDTH_MIN),
        // A fractional logical width is what a 1.25 display scale hands a
        // pane whose device width is odd.
        (false, 259.5),
        (true, COMPACT_WIDTH),
    ] {
        let mut element = project_list(&model, &state, compact, false);
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 240.0)),
        );
        assert_eq!(
            Layout::new(&node).bounds().width,
            width,
            "the scrollable still spans the pane: the rail is inset inside \
                 its viewport rather than taken off a container around it",
        );
        let edge = content_right_edge(Layout::new(&node), 0);
        assert!(
            edge <= width - design::SCROLLBAR_GUTTER,
            "content reaches {edge} of {width}, under the rail",
        );
        assert!(
            edge > width - 2.0 * design::SCROLLBAR_GUTTER,
            "content stops {} short of {width}: more than the rail, so the \
                 reservation is not what is holding it back",
            width - edge,
        );
    }
}

#[test]
fn hiding_file_filter_does_not_leave_an_invisible_query() {
    let mut state = SidebarState::default();
    let key = main_key();
    state.apply(&SidebarAction::FilesQueryChanged {
        key: key.clone(),
        query: "missing".into(),
    });
    state.apply(&SidebarAction::ToggleFilesFilter { key });
    assert!(!state.files_filter_open());
    assert_eq!(state.files_query(), "");
}

#[test]
fn unread_attention_is_owned_and_acknowledged_per_pane() {
    let mut state = SidebarState::default();
    let project = project_id(1);
    let worktree = WorktreeId::from_uuid(Uuid::from_u128(2));
    let first = CardId::from_uuid(Uuid::from_u128(4));
    let second = CardId::from_uuid(Uuid::from_u128(5));
    state.observe_agent_update(first, project, worktree);
    state.observe_agent_update(second, project, worktree);
    state.acknowledge_agent_update(first, &AttentionPlacement::default());
    assert!(!state.has_card_update(first));
    assert!(state.has_card_update(second));
    assert!(state.has_agent_update(project, worktree));
    state.clear_agent_activity(second);
    assert!(
        state.has_agent_update(project, worktree),
        "provider exit is not acknowledgement"
    );
    state.acknowledge_agent_update(second, &AttentionPlacement::default());
    assert!(!state.has_agent_update(project, worktree));
    state.observe_card_update(first);
    assert!(state.has_card_update(first));
    assert!(!state.has_agent_update(project, worktree));
    state.observe_agent_update(first, project, worktree);
    assert!(state.has_agent_update(project, worktree));
    let other = WorktreeId::from_uuid(Uuid::from_u128(3));
    state.observe_agent_update(first, project, other);
    assert!(
        !state.has_agent_update(project, worktree),
        "reattribution cannot leave stale worktree attention"
    );
    assert!(state.has_agent_update(project, other));
}

#[test]
fn working_agents_aggregate_without_clearing_other_agents_or_unread_updates() {
    use crate::agent::{Activity, ActivityState, Provider};
    let mut state = SidebarState::default();
    let project = project_id(1);
    let worktree = WorktreeId::from_uuid(Uuid::from_u128(2));
    let other = WorktreeId::from_uuid(Uuid::from_u128(3));
    let first = CardId::from_uuid(Uuid::from_u128(4));
    let second = CardId::from_uuid(Uuid::from_u128(5));
    let activity = |state| Activity {
        provider: Provider::Omp,
        state,
    };
    state.observe_agent_update(first, project, worktree);
    state.set_agent_activity(first, project, worktree, activity(ActivityState::Working));
    state.set_agent_activity(second, project, worktree, activity(ActivityState::Waiting));
    assert_eq!(
        state.agent_activity(project, worktree),
        Some(ActivityState::Waiting)
    );
    assert_eq!(
        state.card_activity(first).unwrap().state,
        ActivityState::Working
    );
    assert_eq!(state.agent_activity(project, other), None);
    state.set_agent_activity(first, project, worktree, activity(ActivityState::Idle));
    assert_eq!(
        state.agent_activity(project, worktree),
        Some(ActivityState::Waiting)
    );
    state.set_agent_activity(second, project, other, activity(ActivityState::Working));
    assert_eq!(state.agent_activity(project, worktree), None);
    assert_eq!(
        state.agent_activity(project, other),
        Some(ActivityState::Working)
    );
    state.clear_agent_activity(second);
    assert_eq!(state.agent_activity(project, other), None);
    assert!(state.has_agent_update(project, worktree));
}

#[test]
fn standalone_activity_and_reattribution_keep_attention_on_its_pane() {
    use crate::agent::{Activity, ActivityState, Provider};
    let mut state = SidebarState::default();
    let card = CardId::from_uuid(Uuid::from_u128(4));
    let project = project_id(1);
    let worktree = WorktreeId::from_uuid(Uuid::from_u128(2));
    let working = Activity {
        provider: Provider::Omp,
        state: ActivityState::Working,
    };
    state.set_card_activity(card, working);
    assert_eq!(state.card_activity(card), Some(working));
    assert_eq!(state.agent_activity(project, worktree), None);
    assert!(
        !state.has_card_update(card),
        "activity is not unread attention"
    );
    state.observe_card_update(card);
    state.set_agent_activity(card, project, worktree, working);
    assert!(state.has_agent_update(project, worktree));
    state.set_card_activity(card, working);
    assert!(state.has_card_update(card));
    assert!(!state.has_agent_update(project, worktree));
    assert_eq!(state.agent_activity(project, worktree), None);
    state.clear_agent_activity(card);
    assert_eq!(state.card_activity(card), None);
    assert!(state.has_card_update(card));
}

#[test]
fn group_mutations_cover_folded_files_and_preserve_rename_halves() {
    let mut snapshot = snapshot();
    let mut renamed = changed(
        repo_path("keep.rs"),
        ChangeArea::Unstaged,
        ChangeKind::Renamed,
        None,
    );
    renamed.old_path = Some(repo_path("old.rs"));
    snapshot.status.entries = vec![
        changed(
            repo_path("src/hidden.rs"),
            ChangeArea::Unstaged,
            ChangeKind::Modified,
            None,
        ),
        renamed.clone(),
        changed(
            repo_path("keep.rs"),
            ChangeArea::Staged,
            ChangeKind::Modified,
            None,
        ),
    ];
    let source = SourceControlModel::ready(main_key(), &snapshot);
    let group = source.group(ChangeArea::Unstaged).unwrap();
    let stage = group_commands(&source, group, false)
        .into_iter()
        .filter_map(|command| command.action)
        .find_map(|action| match resolve_group_command(&source, &action) {
            Some(SidebarAction::Stage { selection, .. }) => Some(selection),
            _ => None,
        })
        .unwrap();
    assert_eq!(stage.len(), 2);
    assert!(stage.contains(&PathSelection::from_changed_path(&renamed)));
    assert!(
        stage
            .iter()
            .any(|selection| selection.path == repo_path("src/hidden.rs"))
    );
}

#[test]
fn project_reorder_preserves_new_projects_and_handles_forward_moves() {
    let projects = (1..=4)
        .map(|id| {
            let mut entry = project();
            entry.id = project_id(id);
            entry
        })
        .collect();
    let mut model = SidebarModel::ready(SidebarMode::Expanded, projects);
    model.apply_project_order(&[project_id(3), project_id(1)]);
    let ids = |model: &SidebarModel| {
        model
            .projects
            .iter()
            .map(|entry| entry.id)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&model),
        vec![project_id(3), project_id(1), project_id(2), project_id(4)]
    );
    assert!(model.reorder_project(project_id(3), Some(project_id(4))));
    assert_eq!(
        ids(&model),
        vec![project_id(1), project_id(2), project_id(3), project_id(4)]
    );
    assert!(!model.reorder_project(project_id(3), Some(project_id(3))));
    assert!(!model.reorder_project(project_id(3), Some(project_id(99))));
    assert!(model.reorder_project(project_id(1), None));
    assert_eq!(
        ids(&model),
        vec![project_id(2), project_id(3), project_id(4), project_id(1)]
    );
}

/// The widget identities of the controls an element renders, in tree
/// order.
///
/// Collects named controls for collision detection and focus routing.
fn control_ids(element: &Element<'_, SidebarAction>) -> Vec<String> {
    fn collect(tree: &Tree, ids: &mut Vec<String>) {
        ids.extend(tree.id.as_ref().map(ToString::to_string));
        for child in &tree.children {
            collect(child, ids);
        }
    }

    let mut ids = Vec::new();
    collect(&Tree::new(element), &mut ids);
    ids
}

#[test]
fn file_menu_opens_for_every_row_unless_blocked() {
    use cosmic::iced::{
        Event, Point, Rectangle, Vector,
        advanced::{Shell, clipboard},
        mouse,
    };
    let renderer = headless_renderer();
    let key = main_key();
    let rows = file_rows(&[repo_path("src/main.rs")]);
    let configured_editors = [SidebarEditor {
        id: "editor".into(),
        name: "External editor".into(),
        icon: "text-editor-symbolic".into(),
    }];
    for file_management in [false, true] {
        for blocked in [false, true] {
            for editors in [&[][..], &configured_editors[..]] {
                for row in &rows {
                    let content = cosmic::iced::widget::Space::new()
                        .width(240)
                        .height(32)
                        .into();
                    let mut element = file_context_controls(
                        content,
                        &key,
                        row,
                        editors,
                        file_management,
                        blocked,
                    );
                    let mut tree = Tree::new(&element);
                    let viewport = Rectangle::with_size(Size::new(240.0, 32.0));
                    let node = element.as_widget_mut().layout(
                        &mut tree,
                        &renderer,
                        &layout::Limits::new(Size::ZERO, viewport.size()),
                    );
                    let mut messages = Vec::new();
                    for event in [
                        mouse::Event::ButtonPressed(mouse::Button::Right),
                        mouse::Event::ButtonReleased(mouse::Button::Right),
                    ] {
                        element.as_widget_mut().update(
                            &mut tree,
                            &Event::Mouse(event),
                            Layout::new(&node),
                            mouse::Cursor::Available(Point::new(40.0, 16.0)),
                            &renderer,
                            &mut clipboard::Null,
                            &mut Shell::new(&mut messages),
                            &viewport,
                        );
                    }
                    let opens = element
                        .as_widget_mut()
                        .overlay(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            &viewport,
                            Vector::ZERO,
                        )
                        .is_some();
                    assert_eq!(
                        opens,
                        !blocked,
                        "file_management={file_management}, blocked={blocked}, directory={}, editors={}",
                        row.directory,
                        editors.len(),
                    );
                }
            }
        }
    }
}

#[test]
fn copying_a_path_needs_neither_file_management_nor_a_file() {
    let commands = |directory, file_management| {
        file_commands(directory, file_management)
            .map(|(_, _, command)| command)
            .collect::<Vec<_>>()
    };
    for directory in [false, true] {
        assert_eq!(
            commands(directory, false),
            vec![
                FileCommand::CopyPath,
                FileCommand::CopyRelativePath,
                FileCommand::RevealInFileManager
            ],
            "a clipboard write is not a working-tree mutation (directory={directory})"
        );
    }
    assert_eq!(
        commands(true, true),
        vec![
            FileCommand::CopyPath,
            FileCommand::CopyRelativePath,
            FileCommand::RevealInFileManager
        ],
        "a directory has nothing to rename or duplicate"
    );
    assert_eq!(
        commands(false, true),
        vec![
            FileCommand::Rename,
            FileCommand::CopyFile,
            FileCommand::CopyPath,
            FileCommand::CopyRelativePath,
            FileCommand::RevealInFileManager,
        ],
    );
}

#[test]
fn project_menu_renders_copy_path_and_dispatches_its_original_project() {
    use cosmic::iced::{
        Event, Point, Rectangle, Vector,
        advanced::{Shell, clipboard, widget::Operation},
        mouse,
    };
    let renderer = headless_renderer();
    let viewport = Rectangle::with_size(Size::new(900.0, 900.0));
    let mut element = context_controls(
        widget::Space::new().width(900).height(900),
        project_id(17),
        None,
        false,
        true,
        true,
        false,
        &[],
        Some(1),
    );
    let mut tree = Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let mut messages = Vec::new();
    let anchor = mouse::Cursor::Available(Point::new(850.0, 850.0));
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
        Layout::new(&node),
        anchor,
        &renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &viewport,
    );
    let mut popup = element
        .as_widget_mut()
        .overlay(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &viewport,
            Vector::ZERO,
        )
        .unwrap();
    let popup_node = popup.as_overlay_mut().layout(&renderer, viewport.size());
    popup.as_overlay_mut().update(
        &Event::Window(cosmic::iced::window::Event::RedrawRequested(
            std::time::Instant::now(),
        )),
        Layout::new(&popup_node),
        anchor,
        &renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
    );
    let popup_node = popup.as_overlay_mut().layout(&renderer, viewport.size());
    #[derive(Default)]
    struct Texts(Vec<(String, Rectangle)>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&Id>, bounds: Rectangle, text: &str) {
            self.0.push((text.to_owned(), bounds));
        }
    }
    let mut texts = Texts::default();
    popup
        .as_overlay_mut()
        .operate(Layout::new(&popup_node), &renderer, &mut texts);
    let copy = texts
        .0
        .iter()
        .find(|(text, _)| text == "Copy path")
        .expect("project root offers Copy path")
        .1;
    assert!(viewport.contains(copy.center()));
    for event in [
        mouse::Event::ButtonPressed(mouse::Button::Left),
        mouse::Event::ButtonReleased(mouse::Button::Left),
    ] {
        popup.as_overlay_mut().update(
            &Event::Mouse(event),
            Layout::new(&popup_node),
            mouse::Cursor::Available(copy.center()),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
        );
    }
    assert_eq!(
        messages,
        [SidebarAction::ProjectCommand {
            project_id: project_id(17),
            worktree_id: None,
            command: ProjectCommand::CopyPath
        }]
    );
}

#[test]
fn context_menu_preserves_child_cursors_and_primary_clicks() {
    use cosmic::iced::{
        Event, Point, Rectangle,
        advanced::{Shell, clipboard},
        mouse,
    };
    let renderer = headless_renderer();
    let content = widget::row::with_children(vec![
        button::custom(design::body("Open"))
            .width(80)
            .height(32)
            .on_press(SidebarAction::OpenRepositoryPicker)
            .into(),
        button::custom(design::body("Disabled"))
            .width(80)
            .height(32)
            .into(),
        cosmic::iced::widget::Space::new()
            .width(80)
            .height(32)
            .into(),
    ]);
    let mut element = context_controls(
        content,
        project_id(1),
        None,
        false,
        true,
        true,
        false,
        &[],
        Some(1),
    );
    let mut tree = Tree::new(&element);
    let viewport = Rectangle::with_size(Size::new(240.0, 32.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    for (x, expected) in [
        (40.0, mouse::Interaction::Pointer),
        (120.0, mouse::Interaction::None),
        (200.0, mouse::Interaction::None),
    ] {
        assert_eq!(
            element.as_widget().mouse_interaction(
                &tree,
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(x, 16.0)),
                &viewport,
                &renderer,
            ),
            expected,
        );
    }
    let mut messages = Vec::new();
    for event in [
        mouse::Event::ButtonPressed(mouse::Button::Left),
        mouse::Event::ButtonReleased(mouse::Button::Left),
    ] {
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(event),
            Layout::new(&node),
            mouse::Cursor::Available(Point::new(40.0, 16.0)),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
    }
    assert_eq!(messages, vec![SidebarAction::OpenRepositoryPicker]);
}

#[test]
fn cached_sidebar_project_submenu_opens_over_expanded_rows_at_label_and_arrow() {
    use cosmic::iced::{
        Event, Point,
        advanced::{clipboard, mouse, renderer::Style, widget::Operation},
        runtime::{UserInterface, user_interface::Cache},
    };

    #[derive(Default)]
    struct Probe {
        texts: Vec<(String, Rectangle)>,
        controls: Vec<(Option<Id>, Rectangle)>,
    }
    impl Operation for Probe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&Id>, bounds: Rectangle, text: &str) {
            self.texts.push((text.to_owned(), bounds));
        }
        fn focusable(
            &mut self,
            id: Option<&Id>,
            bounds: Rectangle,
            _: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
        ) {
            self.controls.push((id.cloned(), bounds));
        }
    }
    fn frame(
        model: &SidebarModel,
        state: &SidebarState,
        navigation: &NavigationCache,
        cache: &mut Cache,
        renderer: &mut cosmic::Renderer,
        events: &[Event],
        cursor: Point,
    ) -> (Vec<SidebarAction>, Probe) {
        let shortcuts = crate::settings::KeyboardShortcuts::default();
        let sidebar = view_cached(
            model,
            state,
            SidebarMode::Expanded,
            260.0,
            false,
            navigation,
            &shortcuts,
            false,
        );
        let mut ui = UserInterface::build(
            widget::row::with_children(vec![
                sidebar,
                cosmic::iced::widget::Space::new()
                    .width(Length::Fill)
                    .into(),
            ]),
            Size::new(1400.0, 900.0),
            std::mem::take(cache),
            renderer,
        );
        let mut messages = Vec::new();
        let cursor = mouse::Cursor::Available(cursor);
        ui.update(
            events,
            cursor,
            renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        ui.draw(renderer, &cosmic::Theme::dark(), &Style::default(), cursor);
        let mut probe = Probe::default();
        ui.operate(renderer, &mut probe);
        *cache = ui.into_cache();
        (messages, probe)
    }

    let mut project = display_project();
    project.worktrees.extend([
        worktree(30, "feature-search", false),
        worktree(31, "docs-refresh", false),
    ]);
    project.channels.push(SidebarChannel::new(
        ChannelId::new(),
        "Project roadmap".into(),
        false,
        None,
        false,
        0,
        true,
        3,
    ));
    let project_id = project.id;
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![project]);
    let mut renderer = headless_renderer();
    for mode in [
        crate::settings::SidebarWorktrees::Grouped,
        crate::settings::SidebarWorktrees::Hidden,
    ] {
        for over_arrow in [false, true] {
            let case = format!("{mode:?}, arrow {over_arrow}");
            let mut state = SidebarState::default();
            state.set_sidebar_worktrees(mode);
            state.apply(&SidebarAction::ToggleWorktrees(project_id));
            state.set_channel_selected(true);
            let navigation = NavigationCache::default();
            let mut cache = Cache::new();
            let (_, probe) = frame(
                &model,
                &state,
                &navigation,
                &mut cache,
                &mut renderer,
                &[],
                Point::ORIGIN,
            );
            let header = probe
                .controls
                .iter()
                .find(|(id, _)| {
                    id.as_ref() == Some(&selection_row_focus_id(SelectionRow::Project(project_id)))
                })
                .expect("project header is mounted")
                .1;
            let anchor = Point::new(70.0, header.center_y());
            let (mut messages, _) = frame(
                &model,
                &state,
                &navigation,
                &mut cache,
                &mut renderer,
                &[
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Right)),
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
                ],
                anchor,
            );
            let (next, probe) = frame(
                &model,
                &state,
                &navigation,
                &mut cache,
                &mut renderer,
                &[Event::Window(cosmic::iced::window::Event::RedrawRequested(
                    std::time::Instant::now(),
                ))],
                anchor,
            );
            messages.extend(next);
            let label = probe
                .texts
                .iter()
                .find(|(text, _)| text == "Checkouts in sidebar")
                .unwrap_or_else(|| panic!("{case}: project menu missing: {:?}", probe.texts))
                .1;
            let folder = probe
                .controls
                .iter()
                .filter(|(_, bounds)| bounds.contains(label.center()))
                .max_by(|(_, left), (_, right)| left.x.total_cmp(&right.x))
                .expect("native folder button bounds")
                .1;
            let pointer = if over_arrow {
                Point::new(folder.x + folder.width - 24.0, folder.center_y())
            } else {
                label.center()
            };
            let (next, _) = frame(
                &model,
                &state,
                &navigation,
                &mut cache,
                &mut renderer,
                &[Event::Mouse(mouse::Event::CursorMoved {
                    position: pointer,
                })],
                pointer,
            );
            messages.extend(next);
            let (next, probe) = frame(
                &model,
                &state,
                &navigation,
                &mut cache,
                &mut renderer,
                &[Event::Window(cosmic::iced::window::Event::RedrawRequested(
                    std::time::Instant::now(),
                ))],
                pointer,
            );
            messages.extend(next);
            let hidden = probe
                .texts
                .iter()
                .find(|(text, _)| text == "Hidden")
                .unwrap_or_else(|| {
                    panic!(
                        "{case}: pointer {pointer:?} did not open submenu: {:?}",
                        probe.texts
                    )
                })
                .1
                .center();
            assert!(
                messages.is_empty(),
                "{case}: hover must not activate rows: {messages:?}"
            );
            let (next, _) = frame(
                &model,
                &state,
                &navigation,
                &mut cache,
                &mut renderer,
                &[
                    Event::Mouse(mouse::Event::CursorMoved { position: hidden }),
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                ],
                hidden,
            );
            messages.extend(next);
            assert_eq!(
                messages,
                [SidebarAction::SetWorktreeDisplay {
                    project_id,
                    mode: Some(crate::settings::SidebarWorktrees::Hidden),
                }],
                "{case}: native child click"
            );
        }
    }
}

/// One frame's worth of the real sidebar, laid out the way the runtime
/// lays it out.
fn interface<'a>(
    model: &'a SidebarModel,
    state: &'a SidebarState,
    navigation: &'a NavigationCache,
    bounds: Size,
    cache: cosmic::iced::runtime::user_interface::Cache,
    renderer: &mut cosmic::Renderer,
) -> cosmic::iced::runtime::UserInterface<'a, SidebarAction, cosmic::Theme, cosmic::Renderer> {
    cosmic::iced::runtime::UserInterface::build(
        view_cached(
            model,
            state,
            SidebarMode::Expanded,
            design::NAVIGATION_DEFAULT_WIDTH,
            false,
            navigation,
            &crate::settings::KeyboardShortcuts::default(),
            false,
        ),
        bounds,
        cache,
        renderer,
    )
}

/// Drives the real sidebar the way the runtime does, so that a gesture can
/// be judged by the messages it publishes and the frame it paints. A
/// window shorter than the project list is what makes a scroll offset, and
/// the menu geometry that depends on it, observable at all.
struct Sidebar {
    navigation: NavigationCache,
    model: SidebarModel,
    state: SidebarState,
    renderer: cosmic::Renderer,
    cache: cosmic::iced::runtime::user_interface::Cache,
    bounds: Size,
}

impl Sidebar {
    /// A window shorter than a row menu, so the menu has to choose its
    /// side and slice itself the way it would on screen.
    fn scrolling() -> Self {
        Self::sized(320.0)
    }

    /// A window with room below the anchor for the whole menu, so its
    /// placement is fixed by the anchor alone. Nothing then moves the menu
    /// if a font or a theme makes it a few pixels taller, which is what
    /// makes measuring it in pixels reliable.
    fn roomy() -> Self {
        Self::sized(640.0)
    }

    /// The same roomy rail, with a channel row above the project's
    /// worktrees.
    fn with_channel(channel: ChannelId) -> Self {
        let mut sidebar = Self::roomy();
        sidebar.model.projects[0].channels.push(SidebarChannel::new(
            channel,
            "Investigate login".into(),
            false,
            None,
            false,
            1,
            true,
            1,
        ));
        sidebar
    }

    fn sized(height: f32) -> Self {
        let mut entry = project();
        entry.worktrees = (2..30)
            .map(|id| worktree(id, &format!("branch-{id}"), false))
            .collect();
        let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
        // Enough editors that the menu is taller than the short window.
        model.external_editors = (0..6)
            .map(|index| SidebarEditor {
                id: format!("editor-{index}"),
                name: format!("Editor {index}"),
                icon: "text-editor-symbolic".to_owned(),
            })
            .collect();
        Self {
            navigation: NavigationCache::default(),
            model,
            state: expanded_worktree_state(),
            renderer: headless_renderer(),
            cache: Default::default(),
            bounds: Size::new(design::NAVIGATION_DEFAULT_WIDTH, height),
        }
    }

    fn attention_frame(&mut self) -> (Vec<(String, [f64; 4])>, Vec<u8>) {
        use cosmic::iced::{
            Color,
            advanced::{
                clipboard, mouse,
                renderer::{Headless, Style},
            },
        };
        let mut ui = interface(
            &self.model,
            &self.state,
            &self.navigation,
            self.bounds,
            std::mem::take(&mut self.cache),
            &mut self.renderer,
        );
        let mut messages = Vec::new();
        ui.update(
            &[],
            mouse::Cursor::Unavailable,
            &mut self.renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert!(messages.is_empty());
        ui.draw(
            &mut self.renderer,
            &cosmic::Theme::light(),
            &Style::default(),
            mouse::Cursor::Unavailable,
        );
        let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let buttons = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| {
                let label = node.node().label()?;
                let bounds = node.node().bounds()?;
                (!node.node().is_hidden() && !node.node().is_disabled()).then(|| {
                    (
                        label.to_owned(),
                        [bounds.x0, bounds.y0, bounds.x1, bounds.y1],
                    )
                })
            })
            .collect();
        self.cache = ui.into_cache();
        let image = self.renderer.screenshot(
            Size::new(self.bounds.width as u32, self.bounds.height as u32),
            1.0,
            Color::BLACK,
        );
        (buttons, image)
    }

    fn feed(
        &mut self,
        events: &[cosmic::iced::Event],
        cursor: cosmic::iced::Point,
    ) -> Vec<SidebarAction> {
        use cosmic::iced::advanced::{clipboard, mouse};
        let mut messages = Vec::new();
        let mut interface = interface(
            &self.model,
            &self.state,
            &self.navigation,
            self.bounds,
            std::mem::take(&mut self.cache),
            &mut self.renderer,
        );
        interface.update(
            events,
            mouse::Cursor::Available(cursor),
            &mut self.renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        self.cache = interface.into_cache();
        messages
    }

    fn scroll(&mut self, pixels: f32, at: cosmic::iced::Point) {
        use cosmic::iced::{Event, advanced::mouse};
        self.feed(
            &[Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: 0.0, y: pixels },
            })],
            at,
        );
    }

    fn click(
        &mut self,
        button: cosmic::iced::advanced::mouse::Button,
        at: cosmic::iced::Point,
    ) -> Vec<SidebarAction> {
        use cosmic::iced::{Event, advanced::mouse};
        self.feed(
            &[
                Event::Mouse(mouse::Event::ButtonPressed(button)),
                Event::Mouse(mouse::Event::ButtonReleased(button)),
            ],
            at,
        )
    }

    fn move_to(&mut self, at: cosmic::iced::Point) {
        use cosmic::iced::{Event, advanced::mouse};
        self.feed(
            &[Event::Mouse(mouse::Event::CursorMoved { position: at })],
            at,
        );
    }

    /// Measure the actual native popup, including its current clipped
    /// slice, instead of assuming an entry sits below the click anchor.
    fn menu_entry(&mut self, label: &str) -> (Rectangle, Rectangle) {
        use cosmic::iced::advanced::widget::Operation;
        struct Entry<'a> {
            label: &'a str,
            pending: Option<Rectangle>,
            containers: Vec<Rectangle>,
            matches: Vec<(Rectangle, Rectangle)>,
        }
        impl Operation for Entry<'_> {
            fn container(&mut self, _: Option<&Id>, bounds: Rectangle) {
                self.pending = Some(bounds);
            }
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                let container = self.pending.take();
                if let Some(bounds) = container {
                    self.containers.push(bounds);
                }
                visit(self);
                if container.is_some() {
                    self.containers.pop();
                }
            }
            fn text(&mut self, _: Option<&Id>, bounds: Rectangle, text: &str) {
                if text == self.label {
                    // The runtime operates overlays separately from the
                    // base tree. Menu::operate's outer container is the
                    // native popup, not the sidebar behind it.
                    let popup = *self.containers.first().expect("native popup container");
                    self.matches.push((bounds, popup));
                }
            }
        }
        let mut entry = Entry {
            label,
            pending: None,
            containers: Vec::new(),
            matches: Vec::new(),
        };
        let mut interface = interface(
            &self.model,
            &self.state,
            &self.navigation,
            self.bounds,
            std::mem::take(&mut self.cache),
            &mut self.renderer,
        );
        interface.operate(&self.renderer, &mut entry);
        self.cache = interface.into_cache();
        assert_eq!(
            entry.matches.len(),
            1,
            "one visible native menu entry {label}"
        );
        entry.matches[0]
    }

    /// The window rows an open row menu paints on, measured against the
    /// same frame with the menu closed and the pointer absent from both,
    /// so that neither the list's scroll position nor hover feedback can
    /// be mistaken for the menu.
    fn menu_rows(scroll: f32) -> Vec<(usize, usize)> {
        Self::menu_rows_for(Self::roomy, MENU_ROW, scroll)
    }

    fn menu_rows_for(
        make: impl Fn() -> Self,
        anchor: cosmic::iced::Point,
        scroll: f32,
    ) -> Vec<(usize, usize)> {
        use cosmic::iced::{
            Color,
            advanced::{
                clipboard, mouse,
                renderer::{Headless, Style},
            },
        };
        let frame = |open: bool| {
            let mut sidebar = make();
            sidebar.scroll(scroll, anchor);
            if open {
                sidebar.click(mouse::Button::Right, anchor);
            }
            sidebar.move_to(anchor);
            // The frame that draws has to be the frame that updated: the
            // runtime keeps an overlay's layout from its own update pass.
            let mut interface = interface(
                &sidebar.model,
                &sidebar.state,
                &sidebar.navigation,
                sidebar.bounds,
                std::mem::take(&mut sidebar.cache),
                &mut sidebar.renderer,
            );
            interface.update(
                &[],
                mouse::Cursor::Unavailable,
                &mut sidebar.renderer,
                &mut clipboard::Null,
                &mut Vec::new(),
            );
            interface.draw(
                &mut sidebar.renderer,
                &cosmic::Theme::dark(),
                &Style::default(),
                mouse::Cursor::Unavailable,
            );
            let size = Size::new(sidebar.bounds.width as u32, sidebar.bounds.height as u32);
            (sidebar.renderer.screenshot(size, 1.0, Color::BLACK), size)
        };
        let (closed, size) = frame(false);
        let (opened, _) = frame(true);
        let width = 4 * size.width as usize;
        let mut rows: Vec<(usize, usize)> = Vec::new();
        for y in 0..closed.len() / width {
            let row = y * width..(y + 1) * width;
            if closed[row.clone()] == opened[row] {
                continue;
            }
            match rows.last_mut() {
                Some((_, last)) if *last + 1 == y => *last = y,
                _ => rows.push((y, y)),
            }
        }
        rows
    }
}

/// A worktree row under the pointer whether the list is scrolled or not.
/// The short window slices its menu to the available vertical space.
const MENU_ROW: cosmic::iced::Point = cosmic::iced::Point::new(56.0, 136.0);
/// How far the wheel moves the list in the scrolled half of each case.
const MENU_SCROLL: f32 = -80.0;

#[test]
fn a_row_menu_in_a_scrolled_list_runs_the_entry_for_its_row() {
    use cosmic::iced::advanced::mouse;
    let mut outcomes = Vec::new();
    for scroll in [0.0, MENU_SCROLL] {
        let mut sidebar = Sidebar::scrolling();
        sidebar.scroll(scroll, MENU_ROW);
        let activated = sidebar.click(mouse::Button::Left, MENU_ROW);
        let Some(SidebarAction::OpenWorktree { worktree_id, .. }) = activated
            .iter()
            .find(|action| matches!(action, SidebarAction::OpenWorktree { .. }))
        else {
            panic!("a worktree row sits under the pointer (scroll={scroll}): {activated:?}");
        };
        let under_pointer = *worktree_id;
        sidebar.click(mouse::Button::Right, MENU_ROW);
        sidebar.feed(
            &[cosmic::iced::Event::Window(
                cosmic::iced::window::Event::RedrawRequested(std::time::Instant::now()),
            )],
            MENU_ROW,
        );
        let (entry, popup) = sidebar.menu_entry("New terminal");
        let viewport = Rectangle::with_size(sidebar.bounds);
        assert!(viewport.contains(entry.center()));
        assert_eq!(
            popup.position(),
            cosmic::iced::Point::ORIGIN,
            "this tall, full-width popup clamps to the short window's origin"
        );
        assert!(popup.height > viewport.height);
        assert!(popup.width <= viewport.width);
        let entry_cursor = entry.center();
        sidebar.move_to(entry_cursor);
        let chosen = sidebar.click(mouse::Button::Left, entry_cursor);
        assert_eq!(
            chosen,
            vec![SidebarAction::ProjectCommand {
                project_id: sidebar.model.projects[0].id,
                worktree_id: Some(under_pointer),
                command: ProjectCommand::NewTerminal,
            }],
            "the measured New terminal row must activate only its original worktree"
        );
        let (worktree, command) = match chosen.first() {
            Some(SidebarAction::ProjectCommand {
                worktree_id,
                command,
                ..
            }) => (*worktree_id, command.clone()),
            other => panic!(
                "choosing a menu entry must run a row command, not fall through to the row \
                     underneath (scroll={scroll}): {other:?}"
            ),
        };
        assert_eq!(
            worktree,
            Some(under_pointer),
            "the menu belongs to the row the pointer opened it over (scroll={scroll})"
        );
        outcomes.push((under_pointer, command));
    }
    let [
        (unscrolled_row, unscrolled_entry),
        (scrolled_row, scrolled_entry),
    ] = outcomes.as_slice()
    else {
        unreachable!("one outcome per scroll state");
    };
    assert_ne!(
        unscrolled_row, scrolled_row,
        "the wheel has to move the list, or the scrolled case tests nothing"
    );
    assert_eq!(
        unscrolled_entry, scrolled_entry,
        "the same reach below the anchor has to land on the same entry, so the menu keeps \
             measuring itself against the window rather than against the scrolled-away content"
    );
}

/// A channel is deletable from the row that shows it, like every other
/// navigation row, and the entry that says so has to reach the
/// application rather than fall through to the row underneath.
#[test]
fn a_channel_row_offers_deleting_the_channel_it_shows() {
    use cosmic::iced::{Point, advanced::mouse};
    let channel_id = ChannelId::from_uuid(Uuid::from_u128(20));
    let mut sidebar = Sidebar::with_channel(channel_id);
    // Find the channel row the way the user does, by clicking until the
    // channel opens: its y depends on the rail's own chrome.
    let row = (0..40)
        .map(|step| Point::new(56.0, 8.0 * step as f32))
        .find(|point| {
            sidebar
                .click(mouse::Button::Left, *point)
                .iter()
                .any(|action| matches!(action, SidebarAction::OpenChannel { .. }))
        })
        .expect("the rail draws the project's channel row");

    // The third entry's position follows the native menu's measured
    // height, including fonts and theme padding. Use its painted bounds
    // rather than a fixed distance from the right-click anchor.
    let painted = Sidebar::menu_rows_for(|| Sidebar::with_channel(channel_id), row, 0.0);
    let (top, bottom) = painted
        .iter()
        .find(|(from, to)| *from <= row.y as usize && *to > row.y as usize)
        .expect("the channel menu paints over its anchor");
    // Menus include a shadow and theme padding. Probe only the measured
    // band, remounting each time so another command cannot close the menu
    // before the Delete entry is reached. Assert the entire event result
    // so a menu command cannot hide click-through to the row underneath.
    let attempted: Vec<_> = (*top..=*bottom)
        .step_by(4)
        .map(|y| {
            let mut candidate = Sidebar::with_channel(channel_id);
            candidate.click(mouse::Button::Right, row);
            // The redraw requested by opening a native menu initializes
            // its geometry at the anchor before the pointer leaves it.
            // Feed that frame just as the application event loop does.
            candidate.feed(
                &[cosmic::iced::Event::Window(
                    cosmic::iced::window::Event::RedrawRequested(std::time::Instant::now()),
                )],
                row,
            );
            let entry = Point::new(row.x + 30.0, y as f32);
            candidate.move_to(entry);
            candidate.click(mouse::Button::Left, entry)
        })
        .collect();
    let selected = attempted
        .iter()
        .find(|actions| actions.contains(&SidebarAction::DeleteChannel { channel_id }));
    assert_eq!(
        selected,
        Some(&vec![SidebarAction::DeleteChannel { channel_id }]),
        "Delete must target the channel without opening a row underneath: {attempted:?}",
    );
}

#[test]
fn a_row_menu_paints_where_it_answers_the_pointer() {
    // The band a menu paints on starts at the pointer and runs down the
    // window. Opening it also repaints the row it was opened on, which
    // borders that band from above, so the two cases are compared by how
    // far the menu reaches rather than by where the repainting starts.
    let band = |painted: Vec<(usize, usize)>, list: &str| {
        painted
            .iter()
            .copied()
            .find(|(from, to)| *from <= MENU_ROW.y as usize && *to > MENU_ROW.y as usize)
            .unwrap_or_else(|| {
                panic!("{list} row's menu paints where the pointer opened it: {painted:?}")
            })
    };
    let (_, unscrolled) = band(Sidebar::menu_rows(0.0), "an unscrolled");
    let (_, scrolled) = band(Sidebar::menu_rows(MENU_SCROLL), "a scrolled");
    assert_eq!(
        scrolled, unscrolled,
        "a scrolled row's menu has to reach as far down the window as an unscrolled one, or \
             it is drawn somewhere other than where it takes clicks"
    );
}

#[test]
fn clicking_beside_an_open_row_menu_only_dismisses_it() {
    use cosmic::iced::{Point, advanced::mouse};
    // Leave genuine space outside the popup. In a window shorter than
    // this tall menu, clamping deliberately covers this header point.
    let beside = Point::new(24.0, 20.0);
    for scroll in [0.0, MENU_SCROLL] {
        let mut closed = Sidebar::roomy();
        closed.scroll(scroll, MENU_ROW);
        let header_action = closed.click(mouse::Button::Left, beside);
        assert!(!header_action.is_empty(), "the underlying header is active");
        let mut sidebar = Sidebar::roomy();
        sidebar.scroll(scroll, MENU_ROW);
        sidebar.click(mouse::Button::Right, MENU_ROW);
        sidebar.move_to(MENU_ROW);
        let (_, popup) = sidebar.menu_entry("New terminal");
        assert!(
            !popup.contains(beside),
            "the dismissal point must be outside the actual native popup: {popup:?}"
        );
        let dismissing = sidebar.click(mouse::Button::Left, beside);
        assert_eq!(
            dismissing,
            Vec::new(),
            "a click beside an open menu dismisses it rather than reaching the control it \
                 covers (scroll={scroll})"
        );
        let reachable = sidebar.click(mouse::Button::Left, beside);
        assert_eq!(
            reachable, header_action,
            "the dismissed menu leaves that control usable again (scroll={scroll})"
        );
    }
}

#[test]
fn full_sidebar_hover_reveals_project_commands_without_moving_disclosure() {
    use cosmic::iced::{
        Point, Rectangle,
        advanced::{
            mouse,
            widget::{Operation, operation},
        },
    };

    struct ControlBounds {
        target: Id,
        bounds: Option<Rectangle>,
    }
    impl Operation for ControlBounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }

        fn focusable(
            &mut self,
            id: Option<&Id>,
            bounds: Rectangle,
            _state: &mut dyn operation::Focusable,
        ) {
            if id == Some(&self.target) {
                self.bounds = Some(bounds);
            }
        }
    }

    let control_bounds = |sidebar: &mut Sidebar, target: Id| {
        let mut bounds = ControlBounds {
            target,
            bounds: None,
        };
        let mut interface = interface(
            &sidebar.model,
            &sidebar.state,
            &sidebar.navigation,
            sidebar.bounds,
            std::mem::take(&mut sidebar.cache),
            &mut sidebar.renderer,
        );
        interface.operate(&sidebar.renderer, &mut bounds);
        sidebar.cache = interface.into_cache();
        bounds.bounds.expect("the project control is mounted")
    };

    for expanded in [true, false] {
        let mut sidebar = Sidebar::roomy();
        let project = sidebar.model.projects[0].id;
        let chrome = sidebar.model.projects[0].chrome.clone();
        if !expanded {
            sidebar.state.apply(&SidebarAction::ToggleProject(project));
        }
        sidebar.move_to(Point::new(400.0, 400.0));
        let resting_identity = control_bounds(&mut sidebar, chrome.open_id.clone());
        let resting_disclosure = control_bounds(&mut sidebar, chrome.toggle_id.clone());

        sidebar.move_to(resting_identity.center());
        let hovered_identity = control_bounds(&mut sidebar, chrome.open_id.clone());
        let hovered_disclosure = control_bounds(&mut sidebar, chrome.toggle_id.clone());
        assert_eq!(
            hovered_identity.width,
            resting_identity.width - 2.0 * ICON_CONTROL_SIZE,
            "pointer movement alone reveals creation commands (expanded={expanded})",
        );
        assert_eq!(
            hovered_disclosure, resting_disclosure,
            "hover cannot move the disclosure target (expanded={expanded})",
        );

        let channel = control_bounds(&mut sidebar, chrome.new_channel_id.clone());
        let actions = sidebar.click(mouse::Button::Left, channel.center());
        assert!(
            actions.contains(&SidebarAction::NewChannel {
                project_id: project
            }),
            "the revealed command responds without opening the project: {actions:?}",
        );
        assert!(!actions.contains(&SidebarAction::ToggleProject(project)));
    }
}

#[test]
fn project_drag_does_not_activate_the_pressed_header() {
    use cosmic::iced::{
        Event, Point, Rectangle,
        advanced::{Shell, clipboard},
        mouse,
    };
    let renderer = headless_renderer();
    let content = widget::column::with_children((1..=2).map(|id| {
        button::custom(design::body("Project"))
            .height(PROJECT_HEADER_HEIGHT)
            .width(240)
            .on_press(SidebarAction::ToggleProject(project_id(id)))
            .into()
    }));
    let mut element = RowInteraction::projects(
        content.into(),
        vec![project_id(1), project_id(2)],
        true,
        row_interaction::StickyViewport::default(),
    );
    let mut tree = Tree::new(&element);
    let viewport = Rectangle::with_size(Size::new(240.0, 2.0 * PROJECT_HEADER_HEIGHT));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let origin = Point::new(100.0, PROJECT_HEADER_HEIGHT / 2.0);
    let end = Point::new(100.0, 1.9 * PROJECT_HEADER_HEIGHT);
    assert_eq!(
        element.as_widget().mouse_interaction(
            &tree,
            Layout::new(&node),
            mouse::Cursor::Available(origin),
            &viewport,
            &renderer,
        ),
        mouse::Interaction::Grab,
        "project titles expose their existing drag gesture",
    );
    let mut dispatch = |event: Event, point: Point| {
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Available(point),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        messages
    };
    // A scrollable translates the cursor but not CursorMoved's raw position.
    // Jitter below the threshold must remain a click even after scrolling.
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            origin
        )
        .is_empty()
    );
    let jitter = Point::new(origin.x + 3.0, origin.y);
    let raw_jitter = Point::new(jitter.x, jitter.y - 100.0);
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::CursorMoved {
                position: raw_jitter
            }),
            jitter
        )
        .is_empty()
    );
    assert_eq!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            jitter
        ),
        vec![SidebarAction::ToggleProject(project_id(1))],
    );
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            origin
        )
        .is_empty()
    );
    let raw_end = Point::new(end.x, end.y - 100.0);
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::CursorMoved { position: raw_end }),
            end
        )
        .is_empty()
    );
    assert_eq!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            end
        ),
        vec![SidebarAction::ReorderProject {
            project: project_id(1),
            before: None
        }],
    );
    // A release outside the pane cancels instead of using a stale target.
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            origin
        )
        .is_empty()
    );
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::CursorMoved { position: end }),
            end
        )
        .is_empty()
    );
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            Point::new(-10.0, end.y),
        )
        .is_empty()
    );
}

#[test]
fn new_channels_precede_saved_mixed_navigation_without_reordering_existing_destinations() {
    use crate::workspace_model::NavigationItem;
    let mut entry = project();
    entry.worktrees = vec![worktree(20, "main", false), worktree(21, "feature", false)];
    let old = ChannelId::new();
    let ids: Vec<_> = (0..6).map(|_| ChannelId::new()).collect();
    for (index, id) in std::iter::once(&old).chain(&ids).enumerate() {
        entry.channels.push(SidebarChannel::new(
            *id,
            format!("Conversation {index}"),
            false,
            None,
            false,
            0,
            false,
            index as u32,
        ));
    }
    entry.navigation_order = vec![
        NavigationItem::Worktree(entry.worktrees[1].path.clone()),
        NavigationItem::Channel(old),
        NavigationItem::Worktree(entry.worktrees[0].path.clone()),
    ];
    let owner = entry.id;
    let worktrees = [entry.worktrees[1].id, entry.worktrees[0].id];
    let mut other = project();
    other.id = project_id(99);
    let unrelated = other.clone();
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry, other]);
    let mut state = expanded_worktree_state();
    let mut expected = vec![SelectionRow::Project(owner)];
    expected.extend(ids.iter().rev().copied().map(SelectionRow::Channel));
    expected.extend([
        SelectionRow::Worktree(owner, worktrees[0]),
        SelectionRow::Channel(old),
        SelectionRow::Worktree(owner, worktrees[1]),
    ]);
    let visible = visible_selection_rows(&model, &state, false);
    assert_eq!(&visible[..expected.len()], expected.as_slice());
    let cycle = workspace_cycle(&model, &state);
    assert_eq!(
        cycle[..6]
            .iter()
            .map(|action| match action {
                SidebarAction::OpenChannel { channel_id } => *channel_id,
                _ => panic!("a new channel must precede every saved worktree"),
            })
            .collect::<Vec<_>>(),
        ids.iter().rev().copied().collect::<Vec<_>>()
    );
    assert_eq!(model.projects[1], unrelated);
    let mut active = model.clone();
    active.projects[0].channels[0].activity = Some(crate::agent::ActivityState::Working);
    active.projects[0].channels[0].unread = true;
    assert_eq!(
        visible_selection_rows(&active, &state, false),
        visible,
        "activity and unread state do not change creation or manual order"
    );
    // An intentional later drag can place the new destination anywhere.
    let mut arranged = model.clone();
    arranged.projects[0]
        .navigation_order
        .push(NavigationItem::Channel(ids[5]));
    let rows = visible_selection_rows(&arranged, &state, false);
    assert_eq!(rows[1], SelectionRow::Channel(ids[4]));
    assert_eq!(rows[expected.len() - 1], SelectionRow::Channel(ids[5]));
    state.query = "feature".into();
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![
            SelectionRow::Project(owner),
            SelectionRow::Worktree(owner, worktrees[0])
        ],
        "search relevance remains independent of manual and creation ordering"
    );
}

#[test]
fn revealing_a_new_channel_scrolls_the_real_sidebar_without_taking_composer_focus() {
    use cosmic::iced::runtime::{Action, UserInterface, user_interface::Cache};
    use cosmic::iced::{
        Rectangle, Vector,
        advanced::widget::{Operation, operation},
        futures::StreamExt,
    };
    #[derive(Default)]
    struct Probe {
        offset: Option<f32>,
        viewport: Option<Rectangle>,
        focused: Option<Id>,
    }
    impl Operation for Probe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&Id>,
            _: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if state.is_focused() {
                self.focused = id.cloned();
            }
        }
        fn scrollable(
            &mut self,
            id: Option<&Id>,
            bounds: Rectangle,
            _: Rectangle,
            translation: Vector,
            _: &mut dyn operation::Scrollable,
        ) {
            if id == Some(&stable_id("projects.scroll")) {
                self.offset = Some(translation.y);
                self.viewport = Some(bounds);
            }
        }
    }
    let mut first = project();
    first.worktrees = (20..60)
        .map(|id| worktree(id, &format!("branch-{id}"), false))
        .collect();
    let mut last = project();
    last.id = project_id(99);
    last.name = "Last project".into();
    last.worktrees.clear();
    last.channels.clear();
    let channel = ChannelId::new();
    last.channels.push(SidebarChannel::new(
        channel,
        "Newest conversation".into(),
        true,
        None,
        false,
        0,
        true,
        1,
    ));
    let label = last.channels[0].accessible_name.clone();
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![first, last]);
    let state = expanded_worktree_state();
    let foreign = Id::new("new-channel-test-composer");
    let content = widget::column::with_children(vec![
        widget::container(project_list(&model, &state, false, false))
            .height(180)
            .into(),
        widget::text_input("Draft", "Keep editing here")
            .id(foreign.clone())
            .on_input(SidebarAction::SearchChanged)
            .into(),
    ]);
    let mut renderer = headless_renderer();
    let mut ui = UserInterface::build(
        content,
        Size::new(280.0, 240.0),
        Cache::new(),
        &mut renderer,
    );
    ui.operate(
        &renderer,
        &mut operation::focusable::focus::<()>(foreign.clone()),
    );
    let mut before = Probe::default();
    ui.operate(&renderer, &mut before);
    assert_eq!(before.offset, Some(0.0));
    assert_eq!(before.focused, Some(foreign.clone()));
    let target = selection_row_focus_id(SelectionRow::Channel(channel));
    let mut stream =
        cosmic::iced::runtime::task::into_stream(reveal::<()>(target.clone())).unwrap();
    cosmic::iced::futures::executor::block_on(async {
        while let Some(action) = stream.next().await {
            let Action::Widget(mut operation) = action else {
                panic!("reveal is a native widget operation only")
            };
            loop {
                ui.operate(&renderer, operation.as_mut());
                match operation.finish() {
                    operation::Outcome::Chain(next) => operation = next,
                    operation::Outcome::None => break,
                    _ => panic!("reveal must not emit an app or provider action"),
                }
            }
        }
    });
    let mut after = Probe::default();
    ui.operate(&renderer, &mut after);
    assert!(
        after.offset.unwrap() > 0.0,
        "the real project scrollable moved"
    );
    assert_eq!(after.focused, Some(foreign));
    let nodes = ui.a11y_nodes(cosmic::iced::mouse::Cursor::Unavailable);
    let bounds = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|node| {
            node.node().role() == iced_accessibility::accesskit::Role::Button
                && node.node().label() == Some(label.as_ref())
        })
        .expect("the actual channel button remains accessible")
        .node()
        .bounds()
        .unwrap();
    let viewport = after.viewport.unwrap();
    assert!(
        bounds.y0 >= f64::from(viewport.y) - 0.01
            && bounds.y1 <= f64::from(viewport.y + viewport.height) + 0.01
    );
    assert!(bounds.x1 > bounds.x0 && bounds.y1 > bounds.y0);
    // A subsequent explicit keyboard-navigation command still takes focus.
    let mut stream =
        cosmic::iced::runtime::task::into_stream(focus_and_reveal::<()>(target.clone())).unwrap();
    cosmic::iced::futures::executor::block_on(async {
        while let Some(Action::Widget(mut operation)) = stream.next().await {
            loop {
                ui.operate(&renderer, operation.as_mut());
                match operation.finish() {
                    operation::Outcome::Chain(next) => operation = next,
                    _ => break,
                }
            }
        }
    });
    let mut focused = Probe::default();
    ui.operate(&renderer, &mut focused);
    assert_eq!(focused.focused, Some(target));
}

#[test]
fn a_revealed_row_clears_the_fades_at_either_edge() {
    let fade = crate::interaction::OVERFLOW_FADE;
    // A 32 px row in a 180 px viewport over 1000 px of list, at 400.
    // Already clear of both fades: it stays put.
    assert_eq!(reveal_scroll(60.0..92.0, 0.0, 180.0, 1000.0, 400.0), 400.0);
    // Under the bottom fade: it ends a fade's depth above the edge.
    assert_eq!(
        reveal_scroll(140.0..172.0, 0.0, 180.0, 1000.0, 400.0),
        400.0 + 172.0 + fade - 180.0
    );
    // Under the top edge, or the fade below a pinned 32 px header: it
    // starts a fade's depth below either.
    assert_eq!(
        reveal_scroll(10.0..42.0, 0.0, 180.0, 1000.0, 400.0),
        400.0 + 10.0 - fade
    );
    assert_eq!(
        reveal_scroll(40.0..72.0, 32.0, 180.0, 1000.0, 400.0),
        400.0 + 40.0 - fade - 32.0
    );
    // At the list's ends nothing lies beyond and nothing fades: the
    // first and last rows may sit flush.
    assert_eq!(reveal_scroll(-4.0..28.0, 0.0, 180.0, 1000.0, 4.0), 0.0);
    assert_eq!(
        reveal_scroll(160.0..192.0, 0.0, 180.0, 1000.0, 812.0),
        820.0
    );
    // A viewport too short for both margins shares what room it has,
    // and a second reveal settles where the first left the row.
    let first = reveal_scroll(100.0..132.0, 0.0, 40.0, 1000.0, 400.0);
    assert_eq!(first, 400.0 + 100.0 - 4.0);
    assert_eq!(reveal_scroll(4.0..36.0, 0.0, 40.0, 1000.0, first), first);
}

/// The list's top fade stops short of the nearest project header and
/// gives way to a pinned one, so it never switches at full depth: as the
/// gap between two projects crosses the edge it grows and recedes with
/// the scroll, a pixel at a time.
#[test]
fn the_top_fade_grows_with_the_distance_to_the_nearest_header() {
    use cosmic::iced::advanced::{clipboard, widget::operation};
    use cosmic::iced::mouse;
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
    let projects = (0..3)
        .map(|index| {
            let mut project = project();
            project.id = project_id(90 + index);
            project.name = format!("Project {index}");
            project.worktrees.clear();
            project.pull_requests.clear();
            project.channels = (0..4)
                .map(|channel| {
                    SidebarChannel::new(
                        ChannelId::new(),
                        format!("Conversation {index}.{channel}"),
                        true,
                        None,
                        false,
                        0,
                        true,
                        1,
                    )
                })
                .collect();
            project
        })
        .collect();
    let model = SidebarModel::ready(SidebarMode::Expanded, projects);
    let state = expanded_worktree_state();
    let mut renderer = headless_renderer();
    // The compact rail wraps each project in a container of its own.
    for compact in [false, true] {
        let mut depths = Vec::new();
        for offset in (0..=450).map(|step| step as f32) {
            let mut ui = UserInterface::build(
                widget::container(project_list(&model, &state, compact, false)).height(180),
                Size::new(280.0, 180.0),
                Cache::new(),
                &mut renderer,
            );
            ui.operate(
                &renderer,
                &mut operation::scrollable::scroll_to::<()>(
                    stable_id("projects.scroll"),
                    operation::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(offset),
                    },
                ),
            );
            // An event lets the fade read the scroll it now has.
            ui.update(
                &[cosmic::iced::Event::Mouse(mouse::Event::CursorLeft)],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut Vec::new(),
            );
            let drawn = crate::interaction::capture_top_fades(|| {
                ui.draw(
                    &mut renderer,
                    &cosmic::Theme::dark(),
                    &cosmic::iced::advanced::renderer::Style::default(),
                    mouse::Cursor::Unavailable,
                );
            });
            assert_eq!(drawn.len(), 1, "one list, one fade");
            depths.push((offset, drawn[0]));
        }
        for pair in depths.windows(2) {
            let ((_, before), (offset, after)) = (pair[0], pair[1]);
            assert!(
                (after - before).abs() <= 1.0 + 0.01,
                "the top fade jumped from {before} to {after} at {offset} (compact: {compact})"
            );
        }
        assert!(
            depths.iter().any(|(_, depth)| *depth == 0.0),
            "a pinned header holds the edge (compact: {compact})"
        );
        // Well past the start of the list, where the fade could otherwise
        // be at its full depth, it stops short as a gap between projects
        // crosses the edge.
        assert!(
            depths.iter().any(
                |(offset, depth)| *offset > crate::interaction::OVERFLOW_FADE
                    && *depth > 0.0
                    && *depth < crate::interaction::OVERFLOW_FADE
            ),
            "the fade stops short of a header between projects (compact: {compact}): {depths:?}"
        );
    }
}

#[test]
fn upward_reveal_keeps_the_channel_below_its_own_sticky_header_and_preserves_focus() {
    use cosmic::iced::runtime::{Action, UserInterface, user_interface::Cache};
    use cosmic::iced::{
        Rectangle, Vector,
        advanced::widget::{Operation, operation},
        futures::StreamExt,
    };
    #[derive(Default)]
    struct Probe {
        offset: f32,
        viewport: Rectangle,
        focused: Option<Id>,
        bounds: std::collections::HashMap<Id, Rectangle>,
        member_insets: Vec<f32>,
        row_insets: std::collections::HashMap<Id, f32>,
    }
    impl Operation for Probe {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&Id>,
            bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if let Some(id) = id {
                self.bounds.insert(id.clone(), bounds);
                self.row_insets.insert(
                    id.clone(),
                    self.member_insets.last().copied().unwrap_or_default(),
                );
            }
            if state.is_focused() {
                self.focused = id.cloned();
            }
        }
        fn custom(&mut self, _: Option<&Id>, _: Rectangle, state: &mut dyn std::any::Any) {
            match state.downcast_ref::<row_interaction::RevealInset>() {
                Some(row_interaction::RevealInset::Enter(height)) => {
                    self.member_insets.push(*height)
                }
                Some(row_interaction::RevealInset::Exit) => {
                    self.member_insets.pop();
                }
                None => {}
            }
        }
        fn scrollable(
            &mut self,
            id: Option<&Id>,
            bounds: Rectangle,
            _: Rectangle,
            translation: Vector,
            _: &mut dyn operation::Scrollable,
        ) {
            if id == Some(&stable_id("projects.scroll")) {
                self.offset = translation.y;
                self.viewport = bounds;
            }
        }
    }
    let mut project = project();
    let owner = project.id;
    let header_label = project.chrome.open_name.clone();
    project.worktrees = (20..60)
        .map(|id| worktree(id, &format!("branch-{id}"), false))
        .collect();
    project.pull_requests.clear();
    let channel = ChannelId::new();
    project.channels.push(SidebarChannel::new(
        channel,
        "Newest conversation".into(),
        true,
        None,
        false,
        0,
        true,
        1,
    ));
    let label = project.channels[0].accessible_name.clone();
    let preceding = SidebarProject::new(
        project_id(98),
        RepositoryId::new(),
        "Preceding project".into(),
        "/preceding/.git".into(),
        Vec::new(),
        Vec::new(),
    );
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![preceding, project]);
    let state = expanded_worktree_state();
    let composer = Id::new("upward-reveal-composer");
    let content = widget::column::with_children(vec![
        widget::Space::new().height(37).into(),
        widget::container(project_list(&model, &state, false, false))
            .height(180)
            .into(),
        widget::text_input("Draft", "Keep editing here")
            .id(composer.clone())
            .on_input(SidebarAction::SearchChanged)
            .into(),
    ]);
    let mut renderer = headless_renderer();
    let mut ui = UserInterface::build(
        content,
        Size::new(280.0, 280.0),
        Cache::new(),
        &mut renderer,
    );
    let target = selection_row_focus_id(SelectionRow::Channel(channel));
    for focus in [false, true] {
        ui.operate(
            &renderer,
            &mut operation::scrollable::scroll_to::<()>(
                stable_id("projects.scroll"),
                operation::scrollable::AbsoluteOffset {
                    x: None,
                    y: Some(640.0),
                },
            ),
        );
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(composer.clone()),
        );
        let mut before = Probe::default();
        ui.operate(&renderer, &mut before);
        assert!(before.offset > 0.0);
        assert!(
            before.viewport.y > 0.0,
            "the real scroll viewport is translated below other UI"
        );
        let task = if focus {
            focus_and_reveal::<()>(target.clone())
        } else {
            reveal::<()>(target.clone())
        };
        let mut stream = cosmic::iced::runtime::task::into_stream(task).unwrap();
        cosmic::iced::futures::executor::block_on(async {
            while let Some(action) = stream.next().await {
                let Action::Widget(mut operation) = action else {
                    panic!("widget operation only")
                };
                loop {
                    ui.operate(&renderer, operation.as_mut());
                    match operation.finish() {
                        operation::Outcome::Chain(next) => operation = next,
                        operation::Outcome::None => break,
                        _ => panic!("no app or provider effect"),
                    }
                }
            }
        });
        let mut after = Probe::default();
        ui.operate(&renderer, &mut after);
        assert!(after.offset < before.offset);
        assert_eq!(
            after.focused,
            Some(if focus {
                target.clone()
            } else {
                composer.clone()
            })
        );
        let nodes = ui.a11y_nodes(cosmic::iced::mouse::Cursor::Unavailable);
        let bounds = |label: &str| {
            nodes
                .root()
                .iter()
                .chain(nodes.children())
                .find(|node| {
                    node.node().role() == iced_accessibility::accesskit::Role::Button
                        && node.node().label() == Some(label)
                })
                .unwrap()
                .node()
                .bounds()
                .unwrap()
        };
        let header = bounds(header_label.as_ref());
        let row = bounds(label.as_ref());
        assert!(header.y1 > header.y0);
        assert!(
            row.y0 >= header.y1 - 0.01,
            "the sticky header must not cover the revealed channel: {header:?} {row:?}; raw row={:?}, raw header={:?}, inset={:?}, offset={}, viewport={:?}, focus={focus}",
            after.bounds.get(&target),
            after
                .bounds
                .get(&selection_row_focus_id(SelectionRow::Project(owner))),
            after.row_insets.get(&target),
            after.offset,
            after.viewport
        );
        assert!(row.y1 <= f64::from(after.viewport.y + after.viewport.height) + 0.01);
        let raw_row = after.bounds[&target];
        let raw_header = after.bounds[&selection_row_focus_id(SelectionRow::Project(owner))];
        assert!((row.y0 - f64::from(raw_row.y - after.offset)).abs() < 0.01);
        assert!((header.y0 - f64::from(raw_header.y - after.offset)).abs() < 0.01);
        // The channel leads its group, so revealing it clear of the top
        // edge's fade brings its header down off that edge with it.
        assert!(
            (header.y0 - f64::from(after.viewport.y + crate::interaction::OVERFLOW_FADE)).abs()
                < 0.01
        );
        assert_eq!(after.row_insets[&target], raw_header.height);
        // Accessibility and operation were checked before any paint. Now
        // observe the real Group header draw, independent of either report.
        let painted = row_interaction::capture_headers(row_interaction::HeaderPass::Draw, || {
            ui.draw(
                &mut renderer,
                &cosmic::Theme::default(),
                &cosmic::iced::advanced::renderer::Style::default(),
                cosmic::iced::mouse::Cursor::Unavailable,
            );
        });
        assert!(
            painted
                .iter()
                .any(|paint| (f64::from(paint.y) - header.y0).abs() < 0.01
                    && (f64::from(paint.height) - (header.y1 - header.y0)).abs() < 0.01),
            "actual painted header must match accessibility: {painted:?} {header:?}"
        );
        let dragged = row_interaction::capture_headers(row_interaction::HeaderPass::Drag, || {
            drop(ui.dnd_rectangles(0, &renderer));
        });
        assert!(
            dragged
                .iter()
                .any(|drag| (f64::from(drag.y) - header.y0).abs() < 0.01
                    && (f64::from(drag.height) - (header.y1 - header.y0)).abs() < 0.01),
            "actual drag-destination header must retain the viewport before AX refresh: {dragged:?} {header:?}"
        );
        let after_drag = ui.a11y_nodes(cosmic::iced::mouse::Cursor::Unavailable);
        let drag_header = after_drag
            .root()
            .iter()
            .chain(after_drag.children())
            .find(|node| node.node().label() == Some(header_label.as_ref()))
            .unwrap()
            .node()
            .bounds()
            .unwrap();
        assert_eq!(
            drag_header, header,
            "drag-destination traversal preserves the viewport origin"
        );
        if !focus {
            ui = ui.relayout(Size::new(240.0, 280.0), &mut renderer);
        }
    }
    // Project headers themselves need no member inset and keep their
    // displayed sticky position when explicitly focused.
    let header_target = selection_row_focus_id(SelectionRow::Project(owner));
    let mut header_before = Probe::default();
    ui.operate(&renderer, &mut header_before);
    let mut stream =
        cosmic::iced::runtime::task::into_stream(focus_and_reveal::<()>(header_target.clone()))
            .unwrap();
    cosmic::iced::futures::executor::block_on(async {
        while let Some(Action::Widget(mut operation)) = stream.next().await {
            loop {
                ui.operate(&renderer, operation.as_mut());
                match operation.finish() {
                    operation::Outcome::Chain(next) => operation = next,
                    _ => break,
                }
            }
        }
    });
    let mut focused = Probe::default();
    ui.operate(&renderer, &mut focused);
    assert_eq!(focused.focused, Some(header_target));
    assert_eq!(
        focused.offset, header_before.offset,
        "an already visible sticky header must not scroll by a member inset"
    );
}

#[test]
fn manual_navigation_interleaves_channels_and_worktrees_for_keyboard_navigation() {
    use crate::workspace_model::NavigationItem;
    let mut entry = project();
    entry.worktrees = vec![worktree(20, "main", false), worktree(21, "feature", false)];
    let channel = ChannelId::new();
    entry.channels.push(SidebarChannel::new(
        channel,
        "Discussion".into(),
        false,
        None,
        false,
        0,
        false,
        1,
    ));
    entry.navigation_order = vec![
        NavigationItem::Worktree(entry.worktrees[1].path.clone()),
        NavigationItem::Channel(channel),
        NavigationItem::Worktree(entry.worktrees[0].path.clone()),
    ];
    let owner = entry.id;
    let first = entry.worktrees[1].id;
    let last = entry.worktrees[0].id;
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let state = expanded_worktree_state();
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![
            SelectionRow::Project(owner),
            SelectionRow::Worktree(owner, first),
            SelectionRow::Channel(channel),
            SelectionRow::Worktree(owner, last)
        ]
    );
    let cycle = workspace_cycle(&model, &state);
    assert!(
        matches!(&cycle[0], SidebarAction::OpenWorktree { worktree_id, .. } if *worktree_id == first)
    );
    assert!(
        matches!(&cycle[1], SidebarAction::OpenChannel { channel_id } if *channel_id == channel)
    );
    assert!(
        matches!(&cycle[2], SidebarAction::OpenWorktree { worktree_id, .. } if *worktree_id == last)
    );
    // Search keeps its existing relevance order instead of a manual override.
    let mut search = state;
    search.query = "feature".into();
    assert_eq!(
        visible_selection_rows(&model, &search, false),
        vec![
            SelectionRow::Project(owner),
            SelectionRow::Worktree(owner, first)
        ]
    );
}

#[test]
fn nested_member_drag_translates_accessibility_bounds_once_when_scrolled() {
    use cosmic::iced::{Vector, mouse};
    let renderer = headless_renderer();
    let row = SelectionRow::Worktree(project_id(9), WorktreeId::from_uuid(Uuid::from_u128(1)));
    let button = button::custom(design::body("Worktree"))
        .name("Drag accessibility worktree")
        .width(240)
        .height(PROJECT_HEADER_HEIGHT)
        .on_press(SidebarAction::OpenRepositoryPicker);
    let members = RowInteraction::members(
        widget::column::with_children(vec![
            button.into(),
            crate::ui_controls::field::TextInput::new("Rename worktree", "nonce")
                .on_input(|_| SidebarAction::OpenRepositoryPicker)
                .into(),
        ])
        .into(),
        project_id(9),
        vec![row],
        Vec::new(),
        true,
    );
    let mut element = RowInteraction::projects(
        widget::column::with_children(vec![members]).into(),
        vec![project_id(9)],
        true,
        row_interaction::StickyViewport::default(),
    );
    let mut tree = Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(240.0, 512.0)),
    );
    let bounds = |offset, label| {
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node).with_virtual_offset(Vector::new(0.0, offset)),
            &tree,
            mouse::Cursor::Unavailable,
        );
        accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| node.node().label() == Some(label))
            .unwrap()
            .node()
            .bounds()
            .unwrap()
    };
    for label in ["Drag accessibility worktree", "Rename worktree"] {
        let normal = bounds(0.0, label);
        let scrolled = bounds(40.0, label);
        assert_eq!(scrolled.y0, normal.y0 - 40.0);
        assert_eq!(scrolled.y1, normal.y1 - 40.0);
        assert_eq!(scrolled.x0, normal.x0);
        assert_eq!(scrolled.x1, normal.x1);
    }
}

#[test]
fn deleting_member_cannot_initiate_a_reorder() {
    use cosmic::iced::{
        Event, Point, Rectangle,
        advanced::{Shell, clipboard},
        mouse,
    };
    let renderer = headless_renderer();
    let row = SelectionRow::Worktree(project_id(9), WorktreeId::from_uuid(Uuid::from_u128(1)));
    let content = widget::column::with_children(vec![
        button::custom(design::body("Deleting worktree"))
            .width(240)
            .height(PROJECT_HEADER_HEIGHT)
            .into(),
    ]);
    let mut element =
        RowInteraction::members(content.into(), project_id(9), vec![row], vec![row], true);
    let mut tree = Tree::new(&element);
    let viewport = Rectangle::with_size(Size::new(240.0, PROJECT_HEADER_HEIGHT));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let origin = Point::new(100.0, PROJECT_HEADER_HEIGHT / 2.0);
    let end = Point::new(100.0, PROJECT_HEADER_HEIGHT - 1.0);
    assert_ne!(
        element.as_widget().mouse_interaction(
            &tree,
            Layout::new(&node),
            mouse::Cursor::Available(origin),
            &viewport,
            &renderer
        ),
        mouse::Interaction::Grab
    );
    let mut messages = Vec::new();
    for (event, point) in [
        (
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            origin,
        ),
        (
            Event::Mouse(mouse::Event::CursorMoved { position: end }),
            end,
        ),
        (
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            end,
        ),
    ] {
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Available(point),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
    }
    assert!(messages.is_empty(), "disabled member emitted {messages:?}");
}

#[test]
fn member_drag_does_not_activate_the_pressed_row() {
    use cosmic::iced::{
        Event, Point, Rectangle,
        advanced::{Shell, clipboard},
        mouse,
    };
    let renderer = headless_renderer();
    let content = widget::column::with_children((1..=2).map(|id| {
        button::custom(design::body("Project"))
            .height(PROJECT_HEADER_HEIGHT)
            .width(240)
            .on_press(SidebarAction::ToggleProject(project_id(id)))
            .into()
    }));
    let mut element = RowInteraction::members(
        content.into(),
        project_id(9),
        vec![
            SelectionRow::Worktree(project_id(9), WorktreeId::from_uuid(Uuid::from_u128(1))),
            SelectionRow::Channel(ChannelId::new()),
        ],
        Vec::new(),
        true,
    );
    let mut tree = Tree::new(&element);
    let viewport = Rectangle::with_size(Size::new(240.0, 2.0 * PROJECT_HEADER_HEIGHT));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let origin = Point::new(100.0, PROJECT_HEADER_HEIGHT / 2.0);
    let end = Point::new(100.0, 1.9 * PROJECT_HEADER_HEIGHT);
    assert_eq!(
        element.as_widget().mouse_interaction(
            &tree,
            Layout::new(&node),
            mouse::Cursor::Available(origin),
            &viewport,
            &renderer,
        ),
        mouse::Interaction::Grab,
        "project titles expose their existing drag gesture",
    );
    let mut dispatch = |event: Event, point: Point| {
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Available(point),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        messages
    };
    // A scrollable translates the cursor but not CursorMoved's raw position.
    // Jitter below the threshold must remain a click even after scrolling.
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            origin
        )
        .is_empty()
    );
    let jitter = Point::new(origin.x + 3.0, origin.y);
    let raw_jitter = Point::new(jitter.x, jitter.y - 100.0);
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::CursorMoved {
                position: raw_jitter
            }),
            jitter
        )
        .is_empty()
    );
    assert_eq!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            jitter
        ),
        vec![SidebarAction::ToggleProject(project_id(1))],
    );
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            origin
        )
        .is_empty()
    );
    let raw_end = Point::new(end.x, end.y - 100.0);
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::CursorMoved { position: raw_end }),
            end
        )
        .is_empty()
    );
    assert_eq!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            end
        ),
        vec![SidebarAction::ReorderMember {
            project: project_id(9),
            source: SelectionRow::Worktree(
                project_id(9),
                WorktreeId::from_uuid(Uuid::from_u128(1))
            ),
            before: None
        }],
    );
    // A release outside the pane cancels instead of using a stale target.
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            origin
        )
        .is_empty()
    );
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::CursorMoved { position: end }),
            end
        )
        .is_empty()
    );
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            Point::new(-10.0, end.y),
        )
        .is_empty()
    );
}

#[test]
fn sticky_project_headers_share_displayed_hit_focus_and_push_off_bounds() {
    use cosmic::iced::{
        Event, Point, Rectangle, Vector,
        advanced::{Shell, clipboard, widget::operation},
        keyboard, mouse,
    };
    let renderer = headless_renderer();
    let sticky = row_interaction::StickyViewport::default();
    let header_id = Id::new("sticky-project-regression-header");
    let groups = (1..=2).map(|id| {
        let header = button::custom(design::body("Project"))
            .id(if id == 1 {
                header_id.clone()
            } else {
                Id::unique()
            })
            .name(format!("Sticky project {id}"))
            .width(240)
            .height(PROJECT_HEADER_HEIGHT)
            .on_press(SidebarAction::ToggleProject(project_id(id)));
        let member = button::custom(design::body("Worktree"))
            .width(240)
            .height(3.0 * PROJECT_HEADER_HEIGHT)
            .on_press(SidebarAction::OpenRepositoryPicker);
        RowInteraction::group(header.into(), member.into(), sticky.clone())
    });
    let mut element = RowInteraction::projects(
        widget::column::with_children(groups).into(),
        vec![project_id(1), project_id(2)],
        true,
        sticky,
    );
    let mut tree = Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(240.0, 512.0)),
    );
    // Use precisely the content-coordinate cursor/viewport and virtual scroll
    // offset supplied by iced's scrollable, without relying on a prior draw.
    let at = |offset| Layout::new(&node).with_virtual_offset(Vector::new(0.0, offset));
    let accessible_bounds = |element: &Element<'_, SidebarAction>, tree: &Tree, offset| {
        let accessible =
            element
                .as_widget()
                .a11y_nodes(at(offset), tree, mouse::Cursor::Unavailable);
        let bounds = |label| {
            accessible
                .root()
                .iter()
                .chain(accessible.children())
                .find(|node| node.node().label() == Some(label))
                .unwrap()
                .node()
                .bounds()
                .unwrap()
        };
        (bounds("Sticky project 1"), bounds("Sticky project 2"))
    };
    assert_eq!(accessible_bounds(&element, &tree, 64.0).0.y0, 0.0);
    let pushed = accessible_bounds(&element, &tree, 112.0);
    assert_eq!((pushed.0.y0, pushed.0.y1, pushed.1.y0), (-16.0, 16.0, 16.0));

    let mut dispatch = |event: Event, offset: f32, point: Point| {
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &event,
            at(offset),
            mouse::Cursor::Available(point),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::new(Point::new(0.0, offset), Size::new(240.0, 96.0)),
        );
        messages
    };
    for (offset, y, project) in [(64.0, 80.0, 1), (112.0, 120.0, 1), (112.0, 140.0, 2)] {
        let point = Point::new(100.0, y);
        assert!(
            dispatch(
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                offset,
                point
            )
            .is_empty()
        );
        assert_eq!(
            dispatch(
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                offset,
                point
            ),
            vec![SidebarAction::ToggleProject(project_id(project))],
            "the displayed header owns the click, not the worktree underneath",
        );
    }
    let origin = Point::new(100.0, 80.0);
    let end = Point::new(100.0, 150.0);
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            64.0,
            origin
        )
        .is_empty()
    );
    assert!(
        dispatch(
            Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(end.x, end.y - 64.0)
            }),
            64.0,
            end
        )
        .is_empty()
    );
    assert_eq!(
        dispatch(
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            64.0,
            end
        ),
        vec![SidebarAction::ReorderProject {
            project: project_id(1),
            before: None
        }],
    );
    element.as_widget_mut().operate(
        &mut tree,
        at(64.0),
        &renderer,
        &mut operation::focusable::focus::<()>(header_id),
    );
    let mut messages = Vec::new();
    element.as_widget_mut().update(
        &mut tree,
        &Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }),
        at(64.0),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &Rectangle::new(Point::new(0.0, 64.0), Size::new(240.0, 96.0)),
    );
    assert_eq!(messages, vec![SidebarAction::ToggleProject(project_id(1))]);
}

#[test]
fn context_rows_survive_opening_and_closing_a_modal() {
    let row = |modal: bool| {
        context_controls(
            widget::button::custom(design::body("Worktree"))
                .name("Worktree")
                .width(120)
                .height(32)
                .on_press_maybe((!modal).then_some(SidebarAction::OpenRepositoryPicker)),
            project_id(1),
            Some(WorktreeId::from_uuid(Uuid::from_u128(2))),
            modal,
            true,
            true,
            true,
            &[],
            Some(1),
        )
    };
    let renderer = headless_renderer();
    let viewport = cosmic::iced::Rectangle::with_size(Size::new(120.0, 32.0));
    let initial = row(false);
    let mut tree = Tree::new(&initial);
    // The upstream context menu's diff assumes Some(menu). Rebuilding a
    // mounted row with None panics when a dialog disables context actions.
    for modal in [true, true, false, false] {
        let mut next = row(modal);
        tree.diff(&mut next);
        let layout = next.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let accessible = next.as_widget().a11y_nodes(
            Layout::new(&layout),
            &tree,
            cosmic::iced::advanced::mouse::Cursor::Unavailable,
        );
        assert!(
            accessible
                .root()
                .iter()
                .chain(accessible.children())
                .any(|node| node.node().label() == Some("Worktree"))
        );
        assert_eq!(
            next.as_widget().mouse_interaction(
                &tree,
                Layout::new(&layout),
                cosmic::iced::mouse::Cursor::Available(cosmic::iced::Point::new(60.0, 16.0)),
                &viewport,
                &renderer,
            ),
            if modal {
                cosmic::iced::mouse::Interaction::None
            } else {
                cosmic::iced::mouse::Interaction::Pointer
            },
        );
    }
}

#[test]
fn no_two_controls_in_one_frame_share_an_identity() {
    // Two widgets carrying the same custom id are not a cosmetic clash.
    // The runtime keeps state for named widgets in a map keyed by that
    // name, and drains it before every diff: with a name repeated, one node
    // is handed nothing back, keeps the tag that says it is stateful, and
    // the next focus traversal or draw panics on the downcast. The header's
    // command for the selected project and that project's own row are the
    // pair that collided, so selection is part of the cross product.
    // More than one of everything: a constant id minted inside a
    // per-entity view renders once with a thin fixture and passes, so the
    // pane carries two projects, and the first has two worktrees and two
    // pull requests.
    let mut crowded = project();
    crowded.pull_requests.push(SidebarPullRequest::new(
        project_id(1),
        43,
        "Rename the pane".into(),
        Url::parse("https://github.com/example/cagents/pull/43").unwrap(),
        "main".into(),
        None,
        true,
        CheckStatus::Passing,
    ));
    let other = SidebarProject::new(
        project_id(4),
        RepositoryId(Uuid::from_u128(11)),
        "Orca".into(),
        "/repo/orca".into(),
        vec![worktree(5, "main", false)],
        vec![SidebarPullRequest::new(
            project_id(4),
            7,
            "Trim the deps".into(),
            Url::parse("https://github.com/example/orca/pull/7").unwrap(),
            "main".into(),
            Some("octocat".into()),
            false,
            CheckStatus::Passing,
        )],
    );
    let populated = vec![crowded, other];
    let selections = [
        None,
        Some(SidebarSelection::Project(project_id(1))),
        Some(SidebarSelection::Worktree {
            project_id: project_id(1),
            worktree_id: populated[0].worktrees[0].id,
        }),
        Some(SidebarSelection::PullRequest {
            project_id: project_id(1),
            number: 42,
        }),
    ];

    for mode in [SidebarMode::Compact, SidebarMode::Expanded] {
        // Every data state carries the projects it already had: a failed
        // refresh replaces the body and keeps the list, which is the state
        // the collision was found in.
        for data_state in [
            SidebarDataState::Ready,
            SidebarDataState::Loading,
            SidebarDataState::Error("git exited with status 128".to_owned()),
        ] {
            for projects in [Vec::new(), populated.clone()] {
                let model = SidebarModel {
                    mode,
                    data_state: data_state.clone(),
                    projects: projects.into(),
                    source_control: None,
                    external_editors: Vec::new(),
                    file_management: false,
                };
                for selection in &selections {
                    for controls_expanded in [false, true] {
                        let state = SidebarState {
                            controls_expanded,
                            selection: *selection,
                            ..SidebarState::default()
                        };

                        let element = view(&model, &state, mode, DEFAULT_EXPANDED_WIDTH, false);
                        let mut seen = BTreeSet::new();
                        for id in control_ids(&element) {
                            // Only named identities share the runtime's
                            // map; an unnamed widget gets a fresh unique
                            // id per instance, which `Display` renders as
                            // "Undefined" and which cannot collide.
                            if id == "Undefined" {
                                continue;
                            }
                            assert!(
                                seen.insert(id.clone()),
                                "{id} is rendered twice at {mode:?} \
                                     with {} project(s), {data_state:?}, \
                                     selection {selection:?}, controls \
                                     {controls_expanded}",
                                model.projects.len()
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn local_state_transitions_are_idempotent_and_scoped() {
    let id = project_id(1);
    let mut state = SidebarState::default();
    state.apply(&SidebarAction::ToggleProject(id));
    assert!(state.collapsed_projects.contains(&id));
    state.apply(&SidebarAction::ToggleProject(id));
    assert!(!state.collapsed_projects.contains(&id));

    state.apply(&SidebarAction::ToggleControls);
    state.apply(&SidebarAction::SearchChanged(" Sidebar ".into()));
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    assert!(state.controls_expanded());
    assert_eq!(state.query(), " Sidebar ");
    assert_eq!(state.filter(), SidebarFilter::Attention);
}

#[test]
fn hiding_the_search_controls_ends_the_search_they_held() {
    let mut state = SidebarState::default();
    state.apply(&SidebarAction::ToggleControls);
    state.apply(&SidebarAction::SearchChanged("sidebar".into()));
    state.apply(&SidebarAction::SetFilter(SidebarFilter::PullRequests));
    assert!(state.search_filters_active());
    assert_eq!(state.saved_view().query, "sidebar");

    state.apply(&SidebarAction::ToggleControls);
    assert!(!state.controls_expanded());
    assert_eq!(state.query(), "");
    assert_eq!(
        state.filter(),
        SidebarFilter::All,
        "a hidden pull request filter would silently change row clicks"
    );
    assert!(!state.search_filters_active());

    // Attention keeps its own always-visible header toggle.
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.apply(&SidebarAction::ToggleControls);
    state.apply(&SidebarAction::ToggleControls);
    assert_eq!(state.filter(), SidebarFilter::Attention);
    assert!(!state.search_filters_active());
}

#[test]
fn a_hidden_query_is_neither_saved_nor_restored() {
    let mut state = SidebarState::default();
    state.apply(&SidebarAction::ToggleControls);
    state.apply(&SidebarAction::SearchChanged("kept".into()));
    let mut restored = SidebarState::default();
    restored.restore_view(state.saved_view());
    assert_eq!(restored.query(), "kept", "a visible search survives");

    // A view saved before hiding cleared the query.
    let legacy: SidebarView = serde_json::from_value(serde_json::json!({
        "query": "stale",
        "controls_expanded": false,
        "collapsed_projects": [],
        "selection": null,
    }))
    .unwrap();
    let mut restored = SidebarState::default();
    restored.restore_view(legacy);
    assert_eq!(restored.query(), "");
    assert_eq!(restored.saved_view().query, "");
}

#[test]
fn the_search_toggle_is_lit_while_any_search_filter_applies() {
    for (controls_expanded, query, filter, lit) in [
        (false, "", SidebarFilter::All, false),
        (false, "", SidebarFilter::Attention, false),
        (false, "  ", SidebarFilter::All, false),
        (true, "", SidebarFilter::All, true),
        // Neither can arise through the pane any more, but a state that
        // holds one must still show it.
        (false, "q", SidebarFilter::All, true),
        (false, "", SidebarFilter::PullRequests, true),
    ] {
        let state = SidebarState {
            controls_expanded,
            query: query.into(),
            filter,
            ..SidebarState::default()
        };
        assert_eq!(
            search_toggle_lit(&state),
            lit,
            "controls={controls_expanded} query={query:?} filter={filter:?}"
        );
    }
}

/// Reads `card` as the application does: with the channels holding it and
/// the rows the list draws right now.
fn read(state: &mut SidebarState, model: &SidebarModel, card: CardId, channels: Vec<ChannelId>) {
    let listed = visible_selection_rows(model, state, false);
    state.acknowledge_agent_update(
        card,
        &AttentionPlacement {
            channels,
            listed: &listed,
        },
    );
}

/// Settles `card`'s waiting agent as the application does.
fn settle(state: &mut SidebarState, model: &SidebarModel, card: CardId) {
    let listed = visible_selection_rows(model, state, false);
    state.settle_waiting_agent(
        card,
        &AttentionPlacement {
            channels: Vec::new(),
            listed: &listed,
        },
    );
}

/// One open terminal pane per card, each scoped to its checkout, as the
/// workspace holds unchanneled panes.
/// One project's header and rows as the list draws them, without the
/// rest of the list, In progress included.
fn project_section(
    model: &SidebarModel,
    state: &SidebarState,
    project: ProjectId,
    compact: bool,
) -> Element<'static, SidebarAction> {
    let projects = filtered_projects(model, state, compact);
    let entry = projects
        .iter()
        .find(|entry| entry.project.id == project)
        .expect("the list holds the project");
    project_view(
        entry,
        !state.collapsed_projects.contains(&project),
        compact,
        state.drawn_selection(),
        false,
        row_interaction::StickyViewport::default(),
    )
}

/// Whether `element` draws the work spinner anywhere.
fn draws_work_spinner(element: &Element<'_, SidebarAction>) -> bool {
    fn contains(tree: &Tree, tag: cosmic::iced::advanced::widget::tree::Tag) -> bool {
        tree.tag == tag || tree.children.iter().any(|child| contains(child, tag))
    }
    let spinner = crate::ui_controls::activity_spinner::<SidebarAction>(
        f32::from(ROW_ICON_SIZE),
        design::warning_color,
    );
    contains(&Tree::new(element), spinner.as_widget().tag())
}

fn pane_workspace(panes: &[(CardId, &Path)]) -> Arc<crate::model::WorkspaceState> {
    Arc::new(crate::model::WorkspaceState {
        cards: panes
            .iter()
            .map(|(card, _)| compact_attention_terminal(*card))
            .collect::<Vec<_>>()
            .into(),
        card_scopes: panes
            .iter()
            .map(|(card, path)| (*card, path.to_path_buf()))
            .collect(),
        ..Default::default()
    })
}

#[test]
fn filtering_matches_descendants_and_attention_state() {
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SearchChanged("FIX-SIDEBAR".into()));
    let filtered = filtered_projects(&model, &state, false);
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].worktrees.len(), 1);

    state.apply(&SidebarAction::SearchChanged(String::new()));
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    assert!(
        filtered_projects(&model, &state, false).is_empty(),
        "dirty files and failing checks are not unread attention"
    );
    let first = CardId::from_uuid(Uuid::from_u128(4));
    let second = CardId::from_uuid(Uuid::from_u128(5));
    let checkout = model.projects[0].worktrees[1].clone();
    let durable = pane_workspace(&[(first, &checkout.path), (second, &checkout.path)]);
    state.observe_agent_update(first, project_id(1), checkout.id);
    state.observe_agent_update(second, project_id(1), checkout.id);
    refresh_attention(&mut state, &mut model, &durable);
    let filtered = filtered_projects(&model, &state, false);
    assert!(filtered[0].worktrees.is_empty(), "no checkout rows");
    assert_eq!(filtered[0].cards.len(), 2);
    assert!(
        filtered[0]
            .cards
            .iter()
            .all(|card| card.checkout(&model.projects[0]) == Some("fix-sidebar"))
    );
    read(&mut state, &model, first, Vec::new());
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(filtered_projects(&model, &state, false)[0].cards.len(), 2);
    read(&mut state, &model, second, Vec::new());
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(
        filtered_projects(&model, &state, false)[0].cards.len(),
        2,
        "a row read under Attention stays listed"
    );
    state.apply(&SidebarAction::HideReadAttention);
    refresh_attention(&mut state, &mut model, &durable);
    assert!(filtered_projects(&model, &state, false).is_empty());
}

/// Reading a row in the Attention list keeps it, and the project holding
/// it, where the pointer left them; it only recedes. The control closing
/// the list, or another filter, asks for a fresh list.
#[test]
fn attention_keeps_read_rows_listed_and_dimmed_until_hidden() {
    let renderer = headless_renderer();
    let channel_id = ChannelId::from_uuid(Uuid::from_u128(30));
    let channel = |unread| {
        SidebarChannel::new(
            channel_id,
            "Investigate".into(),
            false,
            None,
            unread,
            1,
            true,
            1,
        )
    };
    let mut entry = project();
    entry.channels.push(channel(true));
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let project = project_id(1);
    let worktree = model.projects[0].worktrees[1].id;
    let in_worktree = CardId::from_uuid(Uuid::from_u128(31));
    let in_channel = CardId::from_uuid(Uuid::from_u128(32));
    let card_row = SelectionRow::Card(in_worktree);
    let channel_row = SelectionRow::Channel(channel_id);
    let durable = pane_workspace(&[(in_worktree, &model.projects[0].worktrees[1].path)]);
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.observe_agent_update(in_worktree, project, worktree);
    state.observe_card_update(in_channel);
    refresh_attention(&mut state, &mut model, &durable);
    let listed = visible_selection_rows(&model, &state, false);
    assert_eq!(
        listed,
        vec![SelectionRow::Project(project), channel_row, card_row]
    );
    let read_names = |model: &SidebarModel, state: &SidebarState, compact: bool| {
        accessible_names(&renderer, project_list(model, state, compact, false))
            .into_iter()
            .filter(|name| name.ends_with(", read"))
            .count()
    };
    let hide = |model: &SidebarModel, state: &SidebarState, compact: bool, modal: bool| {
        frame_enter_presses(&renderer, project_list(model, state, compact, modal))
            .into_iter()
            .filter(|action| *action == SidebarAction::HideReadAttention)
            .count()
    };
    let pane_listing = |model: &SidebarModel, state: &SidebarState| {
        state.card_listing(&model.projects[0].panes[0])
    };
    for compact in [false, true] {
        assert_eq!(read_names(&model, &state, compact), 0);
        assert_eq!(hide(&model, &state, compact, false), 0, "nothing read yet");
    }

    read(&mut state, &model, in_worktree, Vec::new());
    read(&mut state, &model, in_channel, vec![channel_id]);
    refresh_attention(&mut state, &mut model, &durable);
    // The application derives channel unread from the cards it holds.
    model.projects[0].channels[0] = channel(false);
    assert_eq!(pane_listing(&model, &state), Listing::Read);
    assert_eq!(
        state.channel_listing(&model.projects[0].channels[0]),
        Listing::Read
    );
    assert_eq!(
        channel_ink(&model.projects[0].channels[0], Listing::Read),
        RowInk::Muted
    );
    for compact in [false, true] {
        assert_eq!(
            visible_selection_rows(&model, &state, compact),
            listed,
            "read rows hold their places, compact={compact}"
        );
        assert_eq!(
            read_names(&model, &state, compact),
            2,
            "both rows are drawn as read, compact={compact}"
        );
        assert_eq!(hide(&model, &state, compact, false), 1, "compact={compact}");
        assert_eq!(hide(&model, &state, compact, true), 0, "a dialog is up");
    }
    for compact in [false, true] {
        let names = accessible_names(&renderer, project_list(&model, &state, compact, false));
        assert!(
            names
                .iter()
                .any(|name| name == "Hide read and handled rows"),
            "compact={compact}: {names:?}"
        );
    }

    // News arriving again draws the row as news, not as read.
    state.observe_agent_update(in_worktree, project, worktree);
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(pane_listing(&model, &state), Listing::Live);
    assert_eq!(read_names(&model, &state, false), 1);
    read(&mut state, &model, in_worktree, Vec::new());
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(pane_listing(&model, &state), Listing::Read);

    // The control drops the read rows and leaves the filter on.
    let cache = NavigationCache::default();
    let retained = cache.generation(&model, &state);
    let press = frame_enter_presses(&renderer, project_list(&model, &state, false, false))
        .into_iter()
        .find(|action| *action == SidebarAction::HideReadAttention)
        .expect("the hide control");
    state.apply(&press);
    assert_ne!(
        retained,
        cache.generation(&model, &state),
        "dropping read rows redraws the list"
    );
    assert_eq!(state.filter, SidebarFilter::Attention);
    refresh_attention(&mut state, &mut model, &durable);
    assert!(filtered_projects(&model, &state, false).is_empty());
    assert_eq!(
        state.channel_listing(&model.projects[0].channels[0]),
        Listing::Live
    );

    // Leaving through All and coming back starts fresh too.
    state.observe_agent_update(in_worktree, project, worktree);
    refresh_attention(&mut state, &mut model, &durable);
    read(&mut state, &model, in_worktree, Vec::new());
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(filtered_projects(&model, &state, false).len(), 1);
    state.apply(&SidebarAction::SetFilter(SidebarFilter::All));
    assert_eq!(
        pane_listing(&model, &state),
        Listing::Live,
        "only the Attention list dims read rows"
    );
    for compact in [false, true] {
        assert_eq!(hide(&model, &state, compact, false), 0);
    }
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    refresh_attention(&mut state, &mut model, &durable);
    assert!(filtered_projects(&model, &state, false).is_empty());
}

/// Only a row the list draws can shift when it goes, so only a drawn row
/// is kept. One hidden by the search or by its collapsed project would
/// otherwise surface later, dimmed, though the user never saw it read.
#[test]
fn attention_keeps_only_rows_it_is_drawing() {
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let project = project_id(1);
    let worktree = model.projects[0].worktrees[1].id;
    let card = CardId::from_uuid(Uuid::from_u128(60));
    let durable = pane_workspace(&[(card, &model.projects[0].worktrees[1].path)]);
    for hide in [
        SidebarAction::SearchChanged("no such row".into()),
        SidebarAction::ToggleProject(project),
    ] {
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
        state.observe_agent_update(card, project, worktree);
        refresh_attention(&mut state, &mut model, &durable);
        assert!(visible_selection_rows(&model, &state, false).contains(&SelectionRow::Card(card)));
        state.apply(&hide);
        assert!(
            !visible_selection_rows(&model, &state, false).contains(&SelectionRow::Card(card)),
            "{hide:?}"
        );
        read(&mut state, &model, card, Vec::new());
        assert!(state.retained_attention.is_empty(), "{hide:?}");
    }
}

/// A kept row whose channel or pane is gone is dropped, so a pane listed
/// again does not return already read.
#[test]
fn attention_forgets_kept_rows_whose_surface_is_gone() {
    let channel_id = ChannelId::from_uuid(Uuid::from_u128(70));
    let mut entry = project();
    entry.channels.push(SidebarChannel::new(
        channel_id,
        "Investigate".into(),
        false,
        None,
        true,
        1,
        true,
        1,
    ));
    let other_project = SidebarProject::new(
        project_id(2),
        RepositoryId(Uuid::from_u128(20)),
        "Other".into(),
        "/repo/other".into(),
        Vec::new(),
        Vec::new(),
    );
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![entry, other_project]);
    let project = project_id(1);
    let worktree = model.projects[0].worktrees[1].id;
    let card = CardId::from_uuid(Uuid::from_u128(71));
    let card_row = SelectionRow::Card(card);
    let channel_row = SelectionRow::Channel(channel_id);
    let durable = pane_workspace(&[(card, &model.projects[0].worktrees[1].path)]);
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.observe_agent_update(card, project, worktree);
    refresh_attention(&mut state, &mut model, &durable);
    read(&mut state, &model, card, vec![channel_id]);
    assert_eq!(
        state.retained_attention,
        BTreeMap::from([(card_row, Listing::Read), (channel_row, Listing::Read)])
    );

    refresh_attention(&mut state, &mut model, &durable);
    state.prune_retained_attention(&model);
    assert_eq!(state.retained_attention.len(), 2, "both still exist");
    // Channel identity survives a move between projects.
    let channel = model.projects[0].channels.pop().unwrap();
    model.projects[1].channels.push(channel);
    state.prune_retained_attention(&model);
    assert_eq!(state.retained_attention.len(), 2, "the channel moved");

    // The pane closes.
    let closed = Arc::new(crate::model::WorkspaceState::default());
    refresh_attention(&mut state, &mut model, &closed);
    state.prune_retained_attention(&model);
    assert_eq!(
        state.retained_attention,
        BTreeMap::from([(channel_row, Listing::Read)])
    );
    model.projects[1].channels.clear();
    state.prune_retained_attention(&model);
    assert!(state.retained_attention.is_empty());
    // The same pane listed again is news, not something already read.
    state.observe_agent_update(card, project, worktree);
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(
        state.card_listing(&model.projects[0].panes[0]),
        Listing::Live
    );
}

#[test]
fn attention_keeps_a_waiting_pane_visible_beside_a_working_pane() {
    use crate::agent::{Activity, ActivityState, Provider};
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let project = project_id(1);
    let worktree = model.projects[0].worktrees[1].id;
    let working = CardId::new();
    let waiting = CardId::new();
    let row = SelectionRow::Card(waiting);
    let path = model.projects[0].worktrees[1].path.clone();
    let durable = pane_workspace(&[(working, &path), (waiting, &path)]);
    let activity = |state| Activity {
        provider: Provider::Codex,
        state,
    };
    let waiting_listing = |model: &SidebarModel, state: &SidebarState| {
        let card = model.projects[0]
            .panes
            .iter()
            .find(|card| card.id == waiting)
            .unwrap();
        state.card_listing(card)
    };
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.observe_agent_update(waiting, project, worktree);
    refresh_attention(&mut state, &mut model, &durable);
    read(&mut state, &model, waiting, Vec::new());
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(waiting_listing(&model, &state), Listing::Read);

    state.set_agent_activity(working, project, worktree, activity(ActivityState::Working));
    state.set_agent_activity(waiting, project, worktree, activity(ActivityState::Waiting));
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(
        state.agent_activity(project, worktree),
        Some(ActivityState::Waiting),
        "the shared priority exposes waiting without changing live working panes"
    );
    assert_eq!(waiting_listing(&model, &state), Listing::Live);
    // The waiting pane needs the user; the working one only goes on.
    assert_eq!(
        visible_selection_rows(&model, &state, false),
        vec![
            SelectionRow::Project(project),
            row,
            SelectionRow::Card(working),
        ]
    );
    state.apply(&SidebarAction::ToggleProject(project));
    let collapsed = project_list(&model, &state, false, false);
    let collapsed_tree = Tree::new(&collapsed);
    assert!(
        collapsed_tree.find(&selection_row_focus_id(row)).is_none(),
        "the wait must be exposed by the header while its child is unmounted"
    );
    let names = accessible_names(&headless_renderer(), collapsed);
    assert!(
        names.contains(&format!(
            "{}, agent needs input",
            model.projects[0].chrome.open_name
        )),
        "the collapsed project button describes its waiting indicator: {names:?}"
    );
    // The working pane's own row, under In progress, draws a spinner;
    // the project's header and rows must not.
    assert!(
        !draws_work_spinner(&project_section(&model, &state, project, false)),
        "the collapsed header must not mask the wait with a working spinner"
    );
    let mut all = state.clone();
    all.apply(&SidebarAction::SetFilter(SidebarFilter::All));
    assert!(
        !draws_work_spinner(&project_section(&model, &all, project, false)),
        "All and Attention share the waiting priority"
    );
    assert!(
        accessible_names(
            &headless_renderer(),
            project_section(&model, &all, project, false)
        )
        .contains(&model.projects[0].chrome.open_name.to_string())
    );
    state.apply(&SidebarAction::ToggleProject(project));
    for compact in [false, true] {
        assert!(visible_selection_rows(&model, &state, compact).contains(&row));
    }
    state.apply(&SidebarAction::HideReadAttention);
    for compact in [false, true] {
        assert!(
            visible_selection_rows(&model, &state, compact).contains(&row),
            "a live wait cannot be hidden as read, compact={compact}"
        );
    }

    settle(&mut state, &model, waiting);
    state.set_agent_activity(waiting, project, worktree, activity(ActivityState::Idle));
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(waiting_listing(&model, &state), Listing::Handled);
    assert!(visible_selection_rows(&model, &state, false).contains(&row));
    state.apply(&SidebarAction::HideReadAttention);
    refresh_attention(&mut state, &mut model, &durable);
    assert!(!visible_selection_rows(&model, &state, false).contains(&row));
}

#[test]
fn attention_keeps_a_pane_whose_waiting_agent_was_answered() {
    use crate::agent::{Activity, ActivityState, Provider};
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let project = project_id(1);
    let worktree = model.projects[0].worktrees[1].id;
    let card = CardId::from_uuid(Uuid::from_u128(50));
    let durable = pane_workspace(&[(card, &model.projects[0].worktrees[1].path)]);
    let activity = |state| Activity {
        provider: Provider::Claude,
        state,
    };
    let listing = |model: &SidebarModel, state: &SidebarState| {
        state.card_listing(&model.projects[0].panes[0])
    };
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.set_agent_activity(card, project, worktree, activity(ActivityState::Waiting));
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(filtered_projects(&model, &state, false)[0].cards.len(), 1);

    settle(&mut state, &model, card);
    state.set_agent_activity(card, project, worktree, activity(ActivityState::Working));
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(
        filtered_projects(&model, &state, false)[0].cards.len(),
        1,
        "the answered pane stays where it was rather than moving to In progress"
    );
    assert!(in_progress_rows(&model, &state, false).is_empty());
    assert_eq!(
        listing(&model, &state),
        Listing::Handled,
        "the wait may have been answered elsewhere, so the row is not called read"
    );
    assert!(
        accessible_names(
            &headless_renderer(),
            project_list(&model, &state, false, false)
        )
        .iter()
        .any(|name| name.ends_with(", handled"))
    );

    // Reading the row afterwards says the user has seen it.
    state.observe_agent_update(card, project, worktree);
    refresh_attention(&mut state, &mut model, &durable);
    read(&mut state, &model, card, Vec::new());
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(listing(&model, &state), Listing::Read);
    // A later wait that carries on does not take that back.
    state.set_agent_activity(card, project, worktree, activity(ActivityState::Waiting));
    refresh_attention(&mut state, &mut model, &durable);
    settle(&mut state, &model, card);
    state.set_agent_activity(card, project, worktree, activity(ActivityState::Working));
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(listing(&model, &state), Listing::Read);

    // Hidden, the working pane moves to In progress.
    state.apply(&SidebarAction::HideReadAttention);
    refresh_attention(&mut state, &mut model, &durable);
    assert!(filtered_projects(&model, &state, false).is_empty());
    assert_eq!(
        in_progress_rows(&model, &state, false)
            .iter()
            .map(|row| row.row())
            .collect::<Vec<_>>(),
        vec![SelectionRow::Card(card)]
    );
    settle(&mut state, &model, card);
    assert!(
        state.retained_attention.is_empty(),
        "an agent that was not waiting has no wait to settle"
    );
}

/// Rows whose only news is that an agent is working sit last, under
/// "In progress", apart from everything that needs the user. The section
/// folds, stays folded across sessions, and keyboard order follows what
/// it draws.
#[test]
fn attention_lists_working_rows_last_under_in_progress() {
    use crate::agent::{Activity, ActivityState, Provider};
    let renderer = headless_renderer();
    let channel_id = ChannelId::from_uuid(Uuid::from_u128(80));
    let mut entry = project();
    entry.channels.push(
        SidebarChannel::new(
            channel_id,
            "Investigate".into(),
            false,
            Some(ActivityState::Working),
            false,
            1,
            true,
            1,
        )
        .with_attention(AttentionSummary::card(
            Some(ActivityState::Working),
            false,
            false,
        )),
    );
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let project = project_id(1);
    let main = model.projects[0].worktrees[0].clone();
    let feature = model.projects[0].worktrees[1].clone();
    let working = CardId::from_uuid(Uuid::from_u128(81));
    let waiting = CardId::from_uuid(Uuid::from_u128(82));
    let durable = pane_workspace(&[(working, &feature.path), (waiting, &main.path)]);
    let activity = |state| Activity {
        provider: Provider::Codex,
        state,
    };
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.set_agent_activity(
        working,
        project,
        feature.id,
        activity(ActivityState::Working),
    );
    state.set_agent_activity(waiting, project, main.id, activity(ActivityState::Waiting));
    refresh_attention(&mut state, &mut model, &durable);
    let needs_you = vec![SelectionRow::Project(project), SelectionRow::Card(waiting)];
    let in_progress = vec![
        SelectionRow::Channel(channel_id),
        SelectionRow::Card(working),
    ];
    for compact in [false, true] {
        assert_eq!(
            visible_selection_rows(&model, &state, compact),
            [needs_you.clone(), in_progress.clone()].concat(),
            "compact={compact}"
        );
    }
    let names = accessible_names(&renderer, project_list(&model, &state, false, false));
    for name in [
        "Open pane Agent in main, 1 waiting for input",
        "Collapse In progress, 2 agents working",
        "Open channel Investigate in Cagents, 1 working",
        "Open pane Agent in Cagents › fix-sidebar, 1 working",
    ] {
        assert!(names.iter().any(|known| known == name), "{name}: {names:?}");
    }
    let compact = accessible_names(&renderer, project_list(&model, &state, true, false));
    assert!(
        compact
            .iter()
            .any(|name| name == "Collapse In progress, 2 agents working"),
        "{compact:?}"
    );

    // Folding the section hides its rows from the list and the keyboard.
    let press = frame_enter_presses(&renderer, project_list(&model, &state, false, false))
        .into_iter()
        .find(|action| *action == SidebarAction::ToggleInProgress)
        .expect("the In progress header");
    let cache = NavigationCache::default();
    let expanded = cache.generation(&model, &state);
    state.apply(&press);
    assert_ne!(
        expanded,
        cache.generation(&model, &state),
        "folding redraws"
    );
    for compact in [false, true] {
        assert_eq!(visible_selection_rows(&model, &state, compact), needs_you);
        let names = accessible_names(&renderer, project_list(&model, &state, compact, false));
        assert!(
            names
                .iter()
                .any(|name| name == "Expand In progress, 2 agents working"),
            "compact={compact}: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name.ends_with(", 1 working")),
            "compact={compact}: {names:?}"
        );
    }
    let mut restored = SidebarState::default();
    restored.restore_view(state.saved_view());
    assert!(restored.in_progress_collapsed, "the fold is remembered");
    let legacy: SidebarView = serde_json::from_value(serde_json::json!({
        "query": "",
        "controls_expanded": false,
        "collapsed_projects": [],
        "selection": null,
    }))
    .unwrap();
    restored.restore_view(legacy);
    assert!(!restored.in_progress_collapsed, "the section starts open");
    state.apply(&SidebarAction::ToggleInProgress);

    // A working pane that comes to need the user moves up.
    state.set_agent_activity(
        working,
        project,
        feature.id,
        activity(ActivityState::Waiting),
    );
    refresh_attention(&mut state, &mut model, &durable);
    let visible = visible_selection_rows(&model, &state, false);
    assert_eq!(
        in_progress_rows(&model, &state, false)
            .iter()
            .map(|row| row.row())
            .collect::<Vec<_>>(),
        vec![SelectionRow::Channel(channel_id)]
    );
    assert_eq!(visible.last(), Some(&SelectionRow::Channel(channel_id)));
    assert!(visible[..visible.len() - 1].contains(&SelectionRow::Card(working)));

    // Pane rows are opened, never selected.
    state.select_rows(
        SelectionRow::Channel(channel_id),
        SelectionIntent::Toggle,
        &visible,
    );
    state.select_rows(
        SelectionRow::Card(working),
        SelectionIntent::Toggle,
        &visible,
    );
    state.select_rows(
        SelectionRow::Card(waiting),
        SelectionIntent::Range { extend: true },
        &visible,
    );
    assert_eq!(
        state.selected_rows(&visible),
        vec![SelectionRow::Channel(channel_id)]
    );
}

/// Every text the element draws, with where it draws it.
fn drawn_texts(mut element: Element<'_, SidebarAction>, width: f32) -> Vec<(String, Rectangle)> {
    use cosmic::iced::advanced::widget::Operation;
    #[derive(Default)]
    struct Texts(Vec<(String, Rectangle)>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&Id>, bounds: Rectangle, text: &str) {
            self.0.push((text.to_owned(), bounds));
        }
    }
    let renderer = headless_renderer();
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(width, 2_000.0)),
    );
    let mut texts = Texts::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
    texts.0
}

/// The Attention list's main section lists a row for what needs the
/// user, and its indicator says which. A row with unread news whose agent
/// is also working shows the news: the spinner means In progress there.
/// All keeps its one ranking, in which work outranks unread news.
#[test]
fn attention_rows_show_why_they_are_listed_never_the_work_spinner() {
    use crate::agent::{Activity, ActivityState, Provider};
    let channel_id = ChannelId::from_uuid(Uuid::from_u128(85));
    let mut entry = project();
    entry.channels.push(
        SidebarChannel::new(
            channel_id,
            "TLS renewals".into(),
            false,
            Some(ActivityState::Working),
            true,
            1,
            true,
            1,
        )
        .with_attention(AttentionSummary::card(
            Some(ActivityState::Working),
            true,
            false,
        )),
    );
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let project = project_id(1);
    let feature = model.projects[0].worktrees[1].clone();
    let busy = CardId::from_uuid(Uuid::from_u128(86));
    let durable = pane_workspace(&[(busy, &feature.path)]);
    let mut state = expanded_worktree_state();
    state.observe_agent_update(busy, project, feature.id);
    state.set_agent_activity(
        busy,
        project,
        feature.id,
        Activity {
            provider: Provider::Codex,
            state: ActivityState::Working,
        },
    );
    refresh_attention(&mut state, &mut model, &durable);
    assert!(draws_work_spinner(&project_section(
        &model, &state, project, false
    )));

    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    assert!(in_progress_rows(&model, &state, false).is_empty());
    for compact in [false, true] {
        assert_eq!(
            visible_selection_rows(&model, &state, compact),
            vec![
                SelectionRow::Project(project),
                SelectionRow::Channel(channel_id),
                SelectionRow::Card(busy),
            ],
            "compact={compact}"
        );
        assert!(
            !draws_work_spinner(&project_section(&model, &state, project, compact)),
            "compact={compact}"
        );
    }
    let names = accessible_names(
        &headless_renderer(),
        project_section(&model, &state, project, false),
    );
    assert_eq!(
        names.iter().filter(|name| *name == "1").count(),
        2,
        "each row shows its unread count instead: {names:?}"
    );
    // Folded, the header gives the same reason.
    state.apply(&SidebarAction::ToggleProject(project));
    let header = project_section(&model, &state, project, false);
    assert!(!draws_work_spinner(&header));
    let names = accessible_names(&headless_renderer(), header);
    assert!(
        names.contains(&model.projects[0].chrome.open_name.to_string()),
        "{names:?}"
    );
    state.apply(&SidebarAction::ToggleProject(project));

    // Read, the pane is kept in place with no news to give, and In
    // progress never takes it: its work shows on its own row.
    read(&mut state, &model, busy, Vec::new());
    refresh_attention(&mut state, &mut model, &durable);
    assert!(in_progress_rows(&model, &state, false).is_empty());
    assert!(visible_selection_rows(&model, &state, false).contains(&SelectionRow::Card(busy)));
    assert!(draws_work_spinner(&card_view(
        &model.projects[0],
        &model.projects[0].panes[0],
        &state,
        false,
        false,
    )));
}

/// Two working panes in one project read apart: each In progress row
/// names the project and the checkout its pane runs in, and is drawn as
/// every other row is, its kind first and its work in the indicator slot,
/// where the header keeps its count too.
#[test]
fn in_progress_rows_say_where_they_are() {
    use crate::agent::{Activity, ActivityState, Provider};
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let project = project_id(1);
    let main = model.projects[0].worktrees[0].clone();
    let feature = model.projects[0].worktrees[1].clone();
    let root = CardId::from_uuid(Uuid::from_u128(87));
    let linked = CardId::from_uuid(Uuid::from_u128(88));
    let durable = pane_workspace(&[(root, &main.path), (linked, &feature.path)]);
    let activity = |state| Activity {
        provider: Provider::Codex,
        state,
    };
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    for (card, worktree) in [(root, main.id), (linked, feature.id)] {
        state.set_agent_activity(card, project, worktree, activity(ActivityState::Working));
    }
    refresh_attention(&mut state, &mut model, &durable);
    let renderer = headless_renderer();
    let names = accessible_names(&renderer, project_list(&model, &state, false, false));
    for name in [
        format!("Open pane Agent in Cagents › {}, 1 working", main.label()),
        format!(
            "Open pane Agent in Cagents › {}, 1 working",
            feature.label()
        ),
        "Collapse In progress, 2 agents working".to_owned(),
    ] {
        assert!(names.contains(&name), "{name}: {names:?}");
    }

    let rows = in_progress_rows(&model, &state, false);
    let section = || in_progress_section(&rows, &state, false, false);
    let mut element = section();
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(260.0, 2_000.0)),
    );
    let mut grid = Vec::new();
    navigation_rows(Layout::new(&node), &mut grid);
    assert_eq!(grid.len(), 3, "the header and two rows");
    let status = grid[0].1[2];
    let count = drawn_texts(section(), 260.0)
        .into_iter()
        .find(|(text, _)| text == "2")
        .expect("the header's count")
        .1;
    assert!(
        status.contains(count.center()),
        "the count sits in the indicator slot: {count:?} in {status:?}"
    );
    for (_, slots) in &grid[1..] {
        assert_eq!(slots[2].x, status.x, "one indicator column");
    }
    assert!(draws_work_spinner(&section()));

    state.set_agent_activity(root, project, main.id, activity(ActivityState::Idle));
    refresh_attention(&mut state, &mut model, &durable);
    let names = accessible_names(&renderer, project_list(&model, &state, false, false));
    assert!(
        names
            .iter()
            .any(|name| name == "Collapse In progress, 1 agent working"),
        "{names:?}"
    );
}

/// A pane row matches a search for the checkout it runs in, as that
/// checkout's own row did, in both sections of the Attention list.
#[test]
fn attention_search_finds_pane_rows_by_their_checkout() {
    use crate::agent::{Activity, ActivityState, Provider};
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let project = project_id(1);
    let main = model.projects[0].worktrees[0].clone();
    let feature = model.projects[0].worktrees[1].clone();
    let waiting = CardId::from_uuid(Uuid::from_u128(89));
    let working = CardId::from_uuid(Uuid::from_u128(90));
    let durable = pane_workspace(&[(waiting, &feature.path), (working, &feature.path)]);
    let activity = |state| Activity {
        provider: Provider::Codex,
        state,
    };
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.set_agent_activity(
        waiting,
        project,
        feature.id,
        activity(ActivityState::Waiting),
    );
    state.set_agent_activity(
        working,
        project,
        feature.id,
        activity(ActivityState::Working),
    );
    refresh_attention(&mut state, &mut model, &durable);
    let everything = vec![
        SelectionRow::Project(project),
        SelectionRow::Card(waiting),
        SelectionRow::Card(working),
    ];
    assert_eq!(visible_selection_rows(&model, &state, false), everything);
    state.apply(&SidebarAction::SearchChanged(feature.label().to_owned()));
    assert_eq!(visible_selection_rows(&model, &state, false), everything);
    state.apply(&SidebarAction::SearchChanged(main.label().to_owned()));
    assert!(visible_selection_rows(&model, &state, false).is_empty());
}

/// The title is what tells two rows apart, so the place beside it gives
/// way first, a search highlight in the title included.
#[test]
fn a_highlighted_title_keeps_its_width_and_its_place_gives_way() {
    let (font, emphasis) = search_label_fonts(false);
    let title = "Review agent";
    let natural = search_spans_width(title, "review", CHILD_ROW_TEXT_SIZE, font, emphasis).ceil();
    let place = "fix-sidebar-and-a-long-checkout-name";
    let width = natural + 40.0;
    for query in ["", "review"] {
        let texts = drawn_texts(titled_label(title, Some(place), query, RowInk::Full), width);
        let (_, bounds) = texts
            .iter()
            .find(|(text, _)| text.starts_with(" · "))
            .expect("the place");
        assert!(
            bounds.x >= natural - 1.0 || query.is_empty(),
            "query={query:?}: the title keeps {natural}, place at {bounds:?}"
        );
        assert!(
            bounds.x + bounds.width <= width + 0.5,
            "query={query:?}: the place ellipsizes within the row: {bounds:?}"
        );
        assert!(bounds.width < 45.0, "query={query:?}: {bounds:?}");
    }
}

/// A search whose only matches are working rows shows them under In
/// progress with no "No matches" above them, while an empty Attention
/// list with no search still says nothing needs the user.
#[test]
fn attention_search_matching_only_in_progress_rows_shows_no_empty_state() {
    use crate::agent::{Activity, ActivityState, Provider};
    use cosmic::iced::advanced::widget::Operation;
    #[derive(Default)]
    struct Texts(Vec<String>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn text(&mut self, _: Option<&Id>, _: Rectangle, text: &str) {
            self.0.push(text.to_owned());
        }
    }
    let texts = |model: &SidebarModel, state: &SidebarState| {
        let renderer = headless_renderer();
        let mut element = project_list(model, state, false, false);
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(320.0, 2_000.0)),
        );
        let mut texts = Texts::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
        texts.0
    };
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let project = project_id(1);
    let feature = model.projects[0].worktrees[1].clone();
    let working = CardId::from_uuid(Uuid::from_u128(91));
    // A title nothing else in the list contains, the project included.
    let mut pane = compact_attention_terminal(working);
    pane.title = "Flaky retry".into();
    let durable = Arc::new(crate::model::WorkspaceState {
        cards: vec![pane].into(),
        card_scopes: [(working, feature.path.clone())].into_iter().collect(),
        ..Default::default()
    });
    let mut state = expanded_worktree_state();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.set_agent_activity(
        working,
        project,
        feature.id,
        Activity {
            provider: Provider::Codex,
            state: ActivityState::Working,
        },
    );
    refresh_attention(&mut state, &mut model, &durable);
    let idle = texts(&model, &state);
    assert!(idle.iter().any(|text| text == "In progress"), "{idle:?}");
    assert!(!idle.iter().any(|text| text == "No matches"), "{idle:?}");

    let title = "flaky";
    state.apply(&SidebarAction::SearchChanged(title.into()));
    assert!(filtered_projects(&model, &state, false).is_empty());
    assert_eq!(
        in_progress_rows(&model, &state, false)
            .iter()
            .map(|row| row.row())
            .collect::<Vec<_>>(),
        vec![SelectionRow::Card(working)],
        "{title} matches the working pane"
    );
    let searched = texts(&model, &state);
    assert!(
        searched.iter().any(|text| text == "In progress"),
        "{searched:?}"
    );
    assert!(
        !searched
            .iter()
            .any(|text| text == "No matches" || text == "Clear search"),
        "the matches are not contradicted: {searched:?}"
    );

    state.apply(&SidebarAction::SearchChanged("no such row".into()));
    assert!(in_progress_rows(&model, &state, false).is_empty());
    let unmatched = texts(&model, &state);
    assert!(
        unmatched.iter().any(|text| text == "No matches"),
        "{unmatched:?}"
    );
}

#[test]
fn reading_outside_attention_retains_nothing() {
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let project = project_id(1);
    let worktree = model.projects[0].worktrees[1].id;
    let card = CardId::from_uuid(Uuid::from_u128(40));
    let channel = ChannelId::from_uuid(Uuid::from_u128(41));
    // Even rows the list draws are not kept outside Attention.
    let listed = [SelectionRow::Card(card), SelectionRow::Channel(channel)];
    let everywhere = AttentionPlacement {
        channels: vec![channel],
        listed: &listed,
    };
    for filter in [SidebarFilter::All, SidebarFilter::PullRequests] {
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::SetFilter(filter));
        state.observe_agent_update(card, project, worktree);
        state.acknowledge_agent_update(card, &everywhere);
        assert!(!state.has_card_update(card), "{filter:?}");
        assert!(state.retained_attention.is_empty(), "{filter:?}");
    }
}

#[test]
fn sidebar_search_matches_unicode_case_in_each_row_type() {
    let worktree = SidebarWorktree::new(
        WorktreeId::new(),
        "/repo/equipe".into(),
        Some("Équipe".into()),
        false,
        false,
        None,
        None,
        None,
    );
    let mut project = SidebarProject::new(
        project_id(1),
        RepositoryId(Uuid::new_v4()),
        "Überblick".into(),
        "/repo/plain".into(),
        vec![worktree],
        Vec::new(),
    );
    project.channels.push(SidebarChannel::new(
        ChannelId::new(),
        "München".into(),
        false,
        None,
        false,
        0,
        false,
        1,
    ));
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![project]);
    let mut state = SidebarState::default();

    state.apply(&SidebarAction::SearchChanged("überblick".into()));
    assert_eq!(filtered_projects(&model, &state, false).len(), 1);
    state.apply(&SidebarAction::SearchChanged("équipe".into()));
    assert_eq!(
        filtered_projects(&model, &state, false)[0].worktrees.len(),
        1
    );
    state.apply(&SidebarAction::SearchChanged("münchen".into()));
    assert_eq!(
        filtered_projects(&model, &state, false)[0].channels.len(),
        1
    );
    assert_eq!(fold("ΟΣ"), fold("ος"), "final sigma must case-fold");
    assert_eq!(fold("Straße"), fold("STRASSE"), "full folds can expand");
}

#[test]
fn project_header_actions_are_independent_and_attention_can_be_cleared() {
    let renderer = headless_renderer();
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let mut state = SidebarState::default();
    state.apply(&SidebarAction::SearchChanged("main".into()));
    for compact in [false, true] {
        let actions = frame_enter_presses(
            &renderer,
            header(
                &model,
                &state,
                compact,
                false,
                false,
                &crate::settings::KeyboardShortcuts::default(),
            ),
        );
        assert_eq!(
            actions,
            vec![
                SidebarAction::SetFilter(SidebarFilter::Attention),
                SidebarAction::ToggleCheckouts,
                SidebarAction::ToggleControls,
                SidebarAction::OpenRepositoryPicker,
            ]
        );
        state.apply(&actions[0]);
        let actions = frame_enter_presses(
            &renderer,
            header(
                &model,
                &state,
                compact,
                false,
                false,
                &crate::settings::KeyboardShortcuts::default(),
            ),
        );
        assert_eq!(actions[0], SidebarAction::SetFilter(SidebarFilter::All));
        state.apply(&actions[0]);
        assert_eq!(state.query(), "main", "attention does not overwrite search");
        assert!(
            frame_enter_presses(
                &renderer,
                header(
                    &model,
                    &state,
                    compact,
                    false,
                    true,
                    &crate::settings::KeyboardShortcuts::default()
                )
            )
            .is_empty()
        );
    }
}

#[test]
fn projects_header_keeps_its_title_and_controls_within_the_minimum_width() {
    let renderer = headless_renderer();
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let state = SidebarState::default();
    let shortcuts = crate::settings::KeyboardShortcuts {
        toggle_projects: vec!["Alt+B".into()],
        ..crate::settings::KeyboardShortcuts::default()
    };
    for loading in [false, true] {
        let mut element = header(
            &model,
            &state,
            false,
            loading,
            false,
            &crate::settings::KeyboardShortcuts::default(),
        );
        let mut tree = Tree::new(&element);
        // The sidebar adds another four pixels of padding on each side.
        let width = EXPANDED_WIDTH_MIN - 8.0;
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, HEADER_HEIGHT)),
        );
        assert!(node.children()[0].bounds().height <= HEADER_HEIGHT / 2.0);
        for child in node.children() {
            assert!(child.bounds().x + child.bounds().width <= width);
        }
        assert!(
            accessible_names(&renderer, utilities(false, false, &shortcuts))
                .contains(&"Toggle Projects sidebar — Alt+B".to_owned())
        );
    }
}

/// The accessible names an element publishes, in tree order.
fn accessible_names(
    renderer: &cosmic::Renderer,
    mut element: Element<'_, SidebarAction>,
) -> Vec<String> {
    use cosmic::iced::advanced::mouse;

    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        renderer,
        &layout::Limits::new(Size::ZERO, Size::new(1280.0, 4_000.0)),
    );
    let nodes =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter_map(|node| node.node().label().map(ToOwned::to_owned))
        .collect()
}

/// Every bound in a layout, in tree order, to compare two layouts whole.
fn layout_bounds(node: &layout::Node, origin: cosmic::iced::Vector, out: &mut Vec<Rectangle>) {
    let bounds = node.bounds() + origin;
    out.push(bounds);
    for child in node.children() {
        layout_bounds(child, cosmic::iced::Vector::new(bounds.x, bounds.y), out);
    }
}

/// A kept list layout must be the layout the list would compute afresh.
/// Hovering or focusing a project header narrows its name for its commands,
/// so the list is driven through pointer moves over every row, and keyboard
/// focus onto the header, beside an uncached copy fed the same events. A
/// channel's or worktree's delete button covers its row's end instead, so
/// hovering those rows lays nothing out again.
#[test]
fn cached_project_list_layout_matches_a_fresh_layout_through_hover_and_focus() {
    use cosmic::iced::{
        Event, Point, Vector,
        advanced::{Shell, clipboard, mouse, widget::operation},
        time::Instant,
        window,
    };
    let renderer = headless_renderer();
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![display_project()]);
    let state = expanded_worktree_state();
    let mut cached = cached_projects::view((1, false, false), || {
        project_list(&model, &state, false, false)
    });
    let mut fresh = project_list(&model, &state, false, false);
    let mut cached_tree = Tree::new(&cached);
    let mut fresh_tree = Tree::new(&fresh);
    let viewport = Rectangle::with_size(Size::new(280.0, 640.0));
    let limits = layout::Limits::new(Size::ZERO, viewport.size());
    let bounds = |node: &layout::Node| {
        let mut out = Vec::new();
        layout_bounds(node, Vector::ZERO, &mut out);
        out
    };
    let mut cached_node = cached
        .as_widget_mut()
        .layout(&mut cached_tree, &renderer, &limits);
    let mut fresh_node = fresh
        .as_widget_mut()
        .layout(&mut fresh_tree, &renderer, &limits);
    assert_eq!(
        bounds(cached_projects::content_layout(&cached_tree)),
        bounds(&fresh_node)
    );
    let rows: Vec<Point> = {
        let nodes = cached.as_widget().a11y_nodes(
            Layout::new(&cached_node),
            &cached_tree,
            mouse::Cursor::Unavailable,
        );
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| node.node().bounds())
            .map(|rect| Point::new((rect.x1 - 4.0) as f32, ((rect.y0 + rect.y1) / 2.0) as f32))
            .collect()
    };
    // And the project header's name, where its commands show on hover.
    struct ControlBounds {
        target: Id,
        bounds: Option<Rectangle>,
    }
    impl operation::Operation for ControlBounds {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn operation::Operation)) {
            operate(self);
        }
        fn focusable(
            &mut self,
            id: Option<&Id>,
            bounds: Rectangle,
            _state: &mut dyn operation::Focusable,
        ) {
            if id == Some(&self.target) {
                self.bounds = Some(bounds);
            }
        }
    }
    // A list's headers are laid out apart from its rows, so the header's
    // name is found by its id rather than among the list's bounds.
    let header_id = model.projects[0].chrome.open_id.clone();
    fn control_bounds(
        element: &mut Element<'_, SidebarAction>,
        tree: &mut Tree,
        node: &layout::Node,
        renderer: &cosmic::Renderer,
        target: &Id,
    ) -> Rectangle {
        let mut control = ControlBounds {
            target: target.clone(),
            bounds: None,
        };
        element
            .as_widget_mut()
            .operate(tree, Layout::new(node), renderer, &mut control);
        control.bounds.expect("the project header is mounted")
    }
    let header =
        |element: &mut Element<'_, SidebarAction>, tree: &mut Tree, node: &layout::Node| {
            control_bounds(element, tree, node, &renderer, &header_id)
        };
    let resting = header(&mut fresh, &mut fresh_tree, &fresh_node);
    let rows: Vec<Point> = [resting.center()].into_iter().chain(rows).collect();
    assert!(rows.len() > 4, "the fixture lists several rows");
    let mut relaid = 0;
    let mut messages = Vec::new();
    for (step, at) in rows
        .into_iter()
        .chain([Point::new(-10.0, -10.0)])
        .enumerate()
    {
        let event = Event::Mouse(mouse::Event::CursorMoved { position: at });
        let before = (
            bounds(cached_projects::content_layout(&cached_tree)),
            header(&mut cached, &mut cached_tree, &cached_node),
        );
        let mut invalid = [false; 2];
        for (index, (element, tree, node)) in [
            (&mut cached, &mut cached_tree, &cached_node),
            (&mut fresh, &mut fresh_tree, &fresh_node),
        ]
        .into_iter()
        .enumerate()
        {
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(node),
                mouse::Cursor::Available(at),
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            invalid[index] = shell.is_layout_invalid();
        }
        assert_eq!(
            invalid[0], invalid[1],
            "step {step} reports the same relayout"
        );
        cached_node = cached
            .as_widget_mut()
            .layout(&mut cached_tree, &renderer, &limits);
        fresh_node = fresh
            .as_widget_mut()
            .layout(&mut fresh_tree, &renderer, &limits);
        let after = (
            bounds(cached_projects::content_layout(&cached_tree)),
            header(&mut cached, &mut cached_tree, &cached_node),
        );
        assert_eq!(
            after,
            (
                bounds(&fresh_node),
                header(&mut fresh, &mut fresh_tree, &fresh_node)
            ),
            "step {step}"
        );
        relaid += usize::from(after != before);
    }
    assert!(relaid > 0, "hovering a project header relays the list");

    // Keyboard focus onto a project header reveals its commands too. The
    // window moves focus with an operation, which has no shell, and then
    // redraws: that redraw must ask for the layout that reveals them.
    let focus = selection_row_focus_id(SelectionRow::Project(project_id(1)));
    for (element, tree, node) in [
        (&mut cached, &mut cached_tree, &cached_node),
        (&mut fresh, &mut fresh_tree, &fresh_node),
    ] {
        element.as_widget_mut().operate(
            tree,
            Layout::new(node),
            &renderer,
            &mut operation::focusable::focus::<()>(focus.clone()),
        );
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            tree,
            &Event::Window(window::Event::RedrawRequested(Instant::now())),
            Layout::new(node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut shell,
            &viewport,
        );
        assert!(shell.is_layout_invalid(), "the redraw after focus relays");
    }
    cached_node = cached
        .as_widget_mut()
        .layout(&mut cached_tree, &renderer, &limits);
    fresh_node = fresh
        .as_widget_mut()
        .layout(&mut fresh_tree, &renderer, &limits);
    assert_eq!(
        bounds(cached_projects::content_layout(&cached_tree)),
        bounds(&fresh_node)
    );
    let focused = header(&mut cached, &mut cached_tree, &cached_node);
    assert_eq!(focused, header(&mut fresh, &mut fresh_tree, &fresh_node));
    assert!(
        focused.width < resting.width,
        "focus reveals the header's commands: {resting:?} {focused:?}"
    );
}

/// An unchanged list is neither swept for named states nor laid out
/// again, while a widget before it may still ask the window to relayout.
#[test]
fn unchanged_cached_project_list_skips_the_named_sweep_and_layout() {
    use cosmic::iced::{
        Event, Point,
        advanced::{Shell, clipboard, mouse},
    };
    let renderer = headless_renderer();
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![display_project()]);
    let state = expanded_worktree_state();
    let make = |generation| {
        cached_projects::view((generation, false, false), || {
            project_list(&model, &state, false, false)
        })
    };
    let mut element = make(1);
    let mut tree = Tree::new(&element);
    let limits = layout::Limits::new(Size::ZERO, Size::new(280.0, 640.0));
    let layouts = || cached_projects::LAYOUTS.with(std::cell::Cell::get);
    let start = layouts();
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert!(
        tree.take_all_named().is_empty(),
        "the runtime's sweep does not enter the list"
    );
    for _ in 0..3 {
        element = make(1);
        tree.diff(element.as_widget_mut());
        drop(
            element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits),
        );
    }
    assert_eq!(layouts() - start, 1, "an unchanged list keeps its layout");

    // An event the rows ignore leaves a pending window relayout pending.
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    shell.invalidate_layout();
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::CursorMoved {
            position: Point::new(-10.0, -10.0),
        }),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut clipboard::Null,
        &mut shell,
        &Rectangle::with_size(Size::new(280.0, 640.0)),
    );
    assert!(shell.is_layout_invalid());
    drop(
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits),
    );
    assert_eq!(
        layouts() - start,
        1,
        "another widget's relayout keeps the list's"
    );

    drop(element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(320.0, 640.0)),
    ));
    assert_eq!(layouts() - start, 2, "new limits lay the list out again");
    element = make(2);
    tree.diff(element.as_widget_mut());
    drop(element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(320.0, 640.0)),
    ));
    assert_eq!(layouts() - start, 3, "a rebuilt list is laid out again");
}

/// A row content that notes each update, cursor query and overlay
/// request reaching it, reports focus while `focus` names it, and has a
/// menu open while `menu` does. Laid out 20 pixels tall.
mod row_probe {
    use super::*;
    use cosmic::iced::{
        Event, Length, Vector,
        advanced::{
            Clipboard, Shell, Widget, mouse, overlay, renderer,
            widget::{Operation, operation},
        },
    };
    use cosmic::{Renderer, Theme};
    use std::{
        cell::{Cell, RefCell},
        rc::Rc,
    };

    pub(super) type Log = Rc<RefCell<Vec<usize>>>;

    #[derive(Clone, Default)]
    pub(super) struct Logs {
        pub(super) updates: Log,
        pub(super) queries: Log,
        pub(super) overlays: Log,
        pub(super) menu: Rc<Cell<Option<usize>>>,
        pub(super) focus: Rc<Cell<Option<usize>>>,
    }

    impl Logs {
        /// `count` action rows of probes, in a column.
        pub(super) fn rows(&self, count: usize) -> Element<'static, SidebarAction> {
            widget::column::with_children(
                (0..count)
                    .map(|row| {
                        let probe = || {
                            Element::new(Probe {
                                row,
                                logs: self.clone(),
                            })
                        };
                        row_interaction::RowInteraction::actions(probe(), probe(), 0.0)
                    })
                    .collect::<Vec<_>>(),
            )
            .into()
        }

        /// `count` selectable rows of probes, in a column. A probe's focus
        /// is not the row's own target, as a row's delete button isn't.
        pub(super) fn selectable_rows(&self, count: usize) -> Element<'static, SidebarAction> {
            let state = SidebarState::default();
            widget::column::with_children(
                (0..count)
                    .map(|row| {
                        row_interaction::RowInteraction::selectable(
                            Element::new(Probe {
                                row,
                                logs: self.clone(),
                            }),
                            SelectionRow::Channel(ChannelId::new()),
                            &state,
                            true,
                        )
                    })
                    .collect::<Vec<_>>(),
            )
            .into()
        }

        /// The rows each log names since it was last taken, in order and
        /// without repeats.
        pub(super) fn take(log: &Log) -> Vec<usize> {
            let mut rows = std::mem::take(&mut *log.borrow_mut());
            rows.dedup();
            rows
        }
    }

    struct Probe {
        row: usize,
        logs: Logs,
    }

    struct Menu;

    impl overlay::Overlay<SidebarAction, Theme, Renderer> for Menu {
        fn layout(&mut self, _: &Renderer, bounds: Size) -> layout::Node {
            layout::Node::new(bounds)
        }
        fn draw(
            &self,
            _: &mut Renderer,
            _: &Theme,
            _: &renderer::Style,
            _: Layout<'_>,
            _: mouse::Cursor,
        ) {
        }
    }

    struct Focus(bool);

    impl operation::Focusable for Focus {
        fn is_focused(&self) -> bool {
            self.0
        }
        fn focus(&mut self) {}
        fn unfocus(&mut self) {}
    }

    impl Widget<SidebarAction, Theme, Renderer> for Probe {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fixed(20.0))
        }
        fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
            layout::Node::new(limits.resolve(Length::Fill, Length::Fixed(20.0), Size::ZERO))
        }
        fn draw(
            &self,
            _: &Tree,
            _: &mut Renderer,
            _: &Theme,
            _: &renderer::Style,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &Rectangle,
        ) {
        }
        fn operate(
            &mut self,
            _: &mut Tree,
            layout: Layout<'_>,
            _: &Renderer,
            operation: &mut dyn Operation,
        ) {
            operation.focusable(
                None,
                layout.bounds(),
                &mut Focus(self.logs.focus.get() == Some(self.row)),
            );
        }
        fn update(
            &mut self,
            _: &mut Tree,
            _: &Event,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &Renderer,
            _: &mut dyn Clipboard,
            _: &mut Shell<'_, SidebarAction>,
            _: &Rectangle,
        ) {
            self.logs.updates.borrow_mut().push(self.row);
        }
        fn mouse_interaction(
            &self,
            _: &Tree,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &Rectangle,
            _: &Renderer,
        ) -> mouse::Interaction {
            self.logs.queries.borrow_mut().push(self.row);
            mouse::Interaction::None
        }
        fn overlay<'a>(
            &'a mut self,
            _: &'a mut Tree,
            _: Layout<'a>,
            _: &Renderer,
            _: &Rectangle,
            _: Vector,
        ) -> Option<overlay::Element<'a, SidebarAction, Theme, Renderer>> {
            self.logs.overlays.borrow_mut().push(self.row);
            (self.logs.menu.get() == Some(self.row)).then(|| overlay::Element::new(Box::new(Menu)))
        }
    }
}

/// Visits every widget, as the runtime's focus operations do.
struct VisitAll;

impl cosmic::iced::advanced::widget::Operation for VisitAll {
    fn traverse(
        &mut self,
        operate: &mut dyn FnMut(&mut dyn cosmic::iced::advanced::widget::Operation),
    ) {
        operate(self);
    }
}

/// Every frame and every message sends each widget in the window a
/// redraw or resize request, so a long list must not walk the rows it has
/// scrolled away. Input that can concern any row, such as keys for a
/// focused one, still reaches them all; a row that was hovered hears the
/// redraw that tells it the pointer has gone; and a focused row keeps
/// every frame.
#[test]
fn rows_out_of_view_let_frame_broadcasts_pass() {
    use cosmic::iced::{
        Event, Point, Vector,
        advanced::{Shell, clipboard, mouse},
        event::{PlatformSpecific, wayland},
        keyboard,
        time::Instant,
        window,
    };
    use row_probe::Logs;

    let logs = Logs::default();
    let mut list = logs.rows(100);
    let renderer = headless_renderer();
    let mut tree = Tree::new(&list);
    let node = list.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(280.0, 2000.0)),
    );
    let list = &mut list;
    let tree = &mut tree;
    // Rows 0 to 9 fill the first 200 pixels; rows 50 to 59 the view
    // scrolled to 1000.
    let top = Rectangle::new(Point::ORIGIN, Size::new(280.0, 200.0));
    let scrolled = Rectangle::new(Point::new(0.0, 1000.0), Size::new(280.0, 200.0));
    let send = |list: &mut Element<'_, SidebarAction>,
                tree: &mut Tree,
                event: Event,
                cursor: mouse::Cursor,
                viewport: &Rectangle| {
        let mut messages = Vec::new();
        list.as_widget_mut().update(
            tree,
            &event,
            Layout::new(&node),
            cursor,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            viewport,
        );
        Logs::take(&logs.updates)
    };
    // Overlays take the window's viewport, and the scroll as translation.
    let screen = Rectangle::new(Point::ORIGIN, Size::new(280.0, 200.0));
    let collect = |list: &mut Element<'_, SidebarAction>, tree: &mut Tree, scroll: f32| {
        let open = list
            .as_widget_mut()
            .overlay(
                tree,
                Layout::new(&node),
                &renderer,
                &screen,
                Vector::new(0.0, -scroll),
            )
            .is_some();
        (Logs::take(&logs.overlays), open)
    };
    let focus_on = |list: &mut Element<'_, SidebarAction>, tree: &mut Tree, row| {
        logs.focus.set(row);
        list.as_widget_mut()
            .operate(tree, Layout::new(&node), &renderer, &mut VisitAll);
    };
    let redraw = || Event::Window(window::Event::RedrawRequested(Instant::now()));
    let away = mouse::Cursor::Unavailable;
    let rows = |rows: &[std::ops::Range<usize>], extra: &[usize]| {
        rows.iter()
            .cloned()
            .flatten()
            .chain(extra.iter().copied())
            .collect::<Vec<_>>()
    };

    assert_eq!(send(list, tree, redraw(), away, &top), rows(&[0..10], &[]));
    assert_eq!(
        send(
            list,
            tree,
            Event::PlatformSpecific(PlatformSpecific::Wayland(wayland::Event::RequestResize)),
            away,
            &scrolled,
        ),
        rows(&[50..60], &[])
    );
    assert_eq!(
        send(
            list,
            tree,
            Event::Keyboard(keyboard::Event::ModifiersChanged(
                keyboard::Modifiers::SHIFT
            )),
            away,
            &scrolled,
        ),
        rows(&[0..100], &[]),
        "keys reach rows out of view"
    );
    let over_first = mouse::Cursor::Available(Point::new(10.0, 10.0));
    assert_eq!(
        send(
            list,
            tree,
            Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(10.0, 10.0)
            }),
            over_first,
            &top,
        ),
        rows(&[0..10], &[])
    );
    // Row 0 was hovered when the list scrolled away from it.
    assert_eq!(
        send(list, tree, redraw(), away, &scrolled),
        rows(&[0..1, 50..60], &[]),
        "the hovered row hears that the pointer has gone"
    );
    assert_eq!(
        send(list, tree, redraw(), away, &scrolled),
        rows(&[50..60], &[])
    );

    // Focus moved by an operation, as keyboard navigation does, to a row
    // out of view.
    focus_on(list, tree, Some(80));
    assert_eq!(
        send(list, tree, redraw(), away, &scrolled),
        rows(&[50..60], &[80]),
        "a focused row out of view keeps its frames"
    );
    focus_on(list, tree, None);
    assert_eq!(
        send(list, tree, redraw(), away, &scrolled),
        rows(&[50..60], &[]),
        "once focus leaves, so do the frames"
    );

    let over_scrolled = mouse::Cursor::Available(Point::new(10.0, 1010.0));
    let _ = list.as_widget().mouse_interaction(
        tree,
        Layout::new(&node),
        over_scrolled,
        &scrolled,
        &renderer,
    );
    assert_eq!(
        Logs::take(&logs.queries),
        rows(&[50..60], &[]),
        "only rows in view are asked for the cursor"
    );

    assert_eq!(collect(list, tree, 1000.0), (rows(&[50..60], &[]), false));
    logs.menu.set(Some(55));
    assert_eq!(collect(list, tree, 1000.0), (rows(&[50..60], &[]), true));
    assert_eq!(
        collect(list, tree, 0.0),
        (rows(&[0..10], &[55]), true),
        "a menu still open as its row leaves the view stays open"
    );
    assert_eq!(
        send(list, tree, redraw(), away, &top),
        rows(&[0..10], &[55]),
        "a row whose overlay is on screen keeps its frames, so a tooltip \
             fading out as its row scrolls away can finish"
    );
    logs.menu.set(None);
    assert_eq!(collect(list, tree, 0.0), (rows(&[0..10], &[55]), false));
    assert_eq!(
        collect(list, tree, 0.0),
        (rows(&[0..10], &[]), false),
        "once closed, the row is left alone"
    );
    assert_eq!(
        send(list, tree, redraw(), away, &top),
        rows(&[0..10], &[]),
        "and its frames stop"
    );
}

/// A control focused inside a row, such as its delete button, keeps the
/// row's frames out of view even though the row's own target isn't
/// focused.
#[test]
fn rows_out_of_view_keep_frames_for_a_control_focused_within() {
    use cosmic::iced::{
        Event, Point,
        advanced::{Shell, clipboard, mouse},
        time::Instant,
        window,
    };
    use row_probe::Logs;

    let logs = Logs::default();
    let mut list = logs.selectable_rows(30);
    let renderer = headless_renderer();
    let mut tree = Tree::new(&list);
    let node = list.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(280.0, 600.0)),
    );
    let top = Rectangle::new(Point::ORIGIN, Size::new(280.0, 200.0));
    let redraw = |list: &mut Element<'_, SidebarAction>, tree: &mut Tree| {
        let mut messages = Vec::new();
        list.as_widget_mut().update(
            tree,
            &Event::Window(window::Event::RedrawRequested(Instant::now())),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &top,
        );
        Logs::take(&logs.updates)
    };
    assert_eq!(redraw(&mut list, &mut tree), (0..10).collect::<Vec<_>>());
    logs.focus.set(Some(20));
    list.as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut VisitAll);
    assert_eq!(
        redraw(&mut list, &mut tree),
        (0..10).chain([20]).collect::<Vec<_>>()
    );
    logs.focus.set(None);
    list.as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut VisitAll);
    assert_eq!(
        redraw(&mut list, &mut tree),
        (0..10).collect::<Vec<_>>(),
        "once focus leaves the row, so do its frames"
    );
}

/// The same culling through the sidebar's own scrollable, which hands its
/// content a viewport in content coordinates for events and the cursor,
/// and screen coordinates plus its translation for overlays. A test with
/// a bare column only stands in for those.
#[test]
fn rows_out_of_view_of_the_project_scroll_let_frame_broadcasts_pass() {
    use cosmic::iced::{
        Event, Point, Vector,
        advanced::{Shell, clipboard, mouse, widget::operation},
        time::Instant,
        window,
    };
    use row_probe::Logs;

    let logs = Logs::default();
    let mut list = project_scroll(
        logs.rows(100),
        SidebarFilter::All,
        row_interaction::StickyViewport::default(),
    );
    let renderer = headless_renderer();
    let mut tree = Tree::new(&list);
    let node = list.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(280.0, 200.0)),
    );
    let screen = Rectangle::new(Point::ORIGIN, Size::new(280.0, 200.0));
    let redraw = |list: &mut Element<'_, SidebarAction>, tree: &mut Tree| {
        let mut messages = Vec::new();
        list.as_widget_mut().update(
            tree,
            &Event::Window(window::Event::RedrawRequested(Instant::now())),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &screen,
        );
        Logs::take(&logs.updates)
    };
    assert_eq!(redraw(&mut list, &mut tree), (0..10).collect::<Vec<_>>());

    list.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::scrollable::scroll_to::<()>(
            stable_id("projects.scroll"),
            operation::scrollable::AbsoluteOffset {
                x: None,
                y: Some(1000.0),
            },
        ),
    );
    drop(Logs::take(&logs.updates));
    assert_eq!(
        redraw(&mut list, &mut tree),
        (50..60).collect::<Vec<_>>(),
        "scrolled, the rows in view are those the scroll shows"
    );

    let _ = list.as_widget().mouse_interaction(
        &tree,
        Layout::new(&node),
        mouse::Cursor::Available(Point::new(10.0, 10.0)),
        &screen,
        &renderer,
    );
    assert_eq!(
        Logs::take(&logs.queries),
        (50..60).collect::<Vec<_>>(),
        "only rows in view are asked for the cursor"
    );

    logs.menu.set(Some(52));
    let open = list
        .as_widget_mut()
        .overlay(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &screen,
            Vector::ZERO,
        )
        .is_some();
    assert_eq!(
        (Logs::take(&logs.overlays), open),
        ((50..60).collect::<Vec<_>>(), true),
        "overlays are collected from the rows the scroll shows"
    );
}

/// A row's overlay, such as its menu, handles events apart from the list's
/// own update. When it asks for a new layout, the list lays its rows out
/// again too, and still reports the request to the window.
#[test]
fn cached_project_list_relays_when_a_row_overlay_asks() {
    use cosmic::iced::{
        Event, Length, Point, Vector,
        advanced::{Clipboard, Shell, Widget, clipboard, mouse, overlay, renderer},
    };
    use cosmic::{Renderer, Theme};

    /// A row whose overlay asks for a new layout on every event.
    struct Row;
    struct Menu;
    impl Widget<SidebarAction, Theme, Renderer> for Row {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fixed(20.0))
        }
        fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
            layout::Node::new(limits.resolve(Length::Fill, Length::Fixed(20.0), Size::ZERO))
        }
        fn draw(
            &self,
            _: &Tree,
            _: &mut Renderer,
            _: &Theme,
            _: &renderer::Style,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &Rectangle,
        ) {
        }
        fn overlay<'a>(
            &'a mut self,
            _: &'a mut Tree,
            _: Layout<'a>,
            _: &Renderer,
            _: &Rectangle,
            _: Vector,
        ) -> Option<overlay::Element<'a, SidebarAction, Theme, Renderer>> {
            Some(overlay::Element::new(Box::new(Menu)))
        }
    }
    impl overlay::Overlay<SidebarAction, Theme, Renderer> for Menu {
        fn layout(&mut self, _: &Renderer, bounds: Size) -> layout::Node {
            layout::Node::new(bounds)
        }
        fn draw(
            &self,
            _: &mut Renderer,
            _: &Theme,
            _: &renderer::Style,
            _: Layout<'_>,
            _: mouse::Cursor,
        ) {
        }
        fn update(
            &mut self,
            _: &Event,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &Renderer,
            _: &mut dyn Clipboard,
            shell: &mut Shell<'_, SidebarAction>,
        ) {
            shell.invalidate_layout();
        }
    }

    let renderer = headless_renderer();
    let viewport = Rectangle::with_size(Size::new(280.0, 640.0));
    let limits = layout::Limits::new(Size::ZERO, viewport.size());
    let mut element = cached_projects::view((1, false, false), || Element::new(Row));
    let mut tree = Tree::new(&element);
    let layouts = || cached_projects::LAYOUTS.with(std::cell::Cell::get);
    let start = layouts();
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    drop(
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits),
    );
    assert_eq!(layouts() - start, 1);

    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    {
        let mut menu = element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &viewport,
                Vector::ZERO,
            )
            .expect("the row's overlay is forwarded");
        let menu_node = menu.as_overlay_mut().layout(&renderer, viewport.size());
        menu.as_overlay_mut().update(
            &Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(4.0, 4.0),
            }),
            Layout::new(&menu_node),
            mouse::Cursor::Available(Point::new(4.0, 4.0)),
            &renderer,
            &mut clipboard::Null,
            &mut shell,
        );
    }
    assert!(shell.is_layout_invalid(), "the window lays out again");
    drop(
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits),
    );
    assert_eq!(layouts() - start, 2, "and so does the list");
}

/// A pane drag that is cancelled while it highlights a channel, as when a
/// modal opens, clears the highlight from the cached list's rows too.
#[test]
fn cancelled_channel_drag_clears_highlights_in_the_cached_list() {
    use crate::reorder::channel_gesture_tests::{Highlighted, cancelling, highlight};
    let renderer = headless_renderer();
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![display_project()]);
    let state = expanded_worktree_state();
    let make = || {
        cached_projects::view((1, false, false), || {
            project_list(&model, &state, false, false)
        })
    };
    let mut element = make();
    let mut tree = Tree::new(&element);
    let limits = layout::Limits::new(Size::ZERO, Size::new(280.0, 640.0));
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let highlighted = |element: &mut Element<'_, SidebarAction>, tree: &mut Tree| {
        let mut count = Highlighted::default();
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), &renderer, &mut count);
        count.0
    };
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut highlight(ChannelId::from_uuid(Uuid::from_u128(91))),
    );
    assert_eq!(highlighted(&mut element, &mut tree), 1);
    element = make();
    tree.diff(element.as_widget_mut());
    assert_eq!(
        highlighted(&mut element, &mut tree),
        1,
        "an ordinary frame keeps the highlight"
    );
    element = make();
    cancelling(|| tree.diff(element.as_widget_mut()));
    assert_eq!(highlighted(&mut element, &mut tree), 0);
}

/// The list reconciles its own named states when it is rebuilt, so a
/// focused row stays focused when it moves, as it does when a project
/// before it is added.
#[test]
fn rebuilt_cached_project_list_keeps_a_moved_row_focused() {
    use cosmic::iced::advanced::widget::operation;
    #[derive(Default)]
    struct Focused(Option<Id>);
    impl operation::Operation for Focused {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn operation::Operation)) {
            operate(self);
        }
        fn focusable(
            &mut self,
            id: Option<&Id>,
            _bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if state.is_focused() {
                self.0 = id.cloned();
            }
        }
    }
    let renderer = headless_renderer();
    let state = expanded_worktree_state();
    let mut other = project();
    other.id = project_id(7);
    other.name = "Another".into();
    other.repository = "/repo/another".into();
    for worktree in &mut other.worktrees {
        worktree.id = WorktreeId::new();
        worktree.path = PathBuf::from("/repo/another").join(worktree.path.file_name().unwrap());
    }
    other.channels.clear();
    other.pull_requests.clear();
    let before = SidebarModel::ready(SidebarMode::Expanded, vec![display_project()]);
    let after = SidebarModel::ready(SidebarMode::Expanded, vec![other, display_project()]);
    let limits = layout::Limits::new(Size::ZERO, Size::new(280.0, 900.0));
    let mut element = cached_projects::view((1, false, false), || {
        project_list(&before, &state, false, false)
    });
    let mut tree = Tree::new(&element);
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let focus = selection_row_focus_id(SelectionRow::Channel(ChannelId::from_uuid(
        Uuid::from_u128(91),
    )));
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::focusable::focus::<()>(focus.clone()),
    );
    let mut element = cached_projects::view((2, false, false), || {
        project_list(&after, &state, false, false)
    });
    tree.diff(element.as_widget_mut());
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut focused = Focused::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut focused);
    assert_eq!(focused.0, Some(focus));
}

#[test]
fn cached_project_list_preserves_actual_accessibility_press_focus_and_overlay_state() {
    use cosmic::iced::{
        Event, Point, Rectangle, Vector,
        advanced::{Shell, clipboard, mouse, widget::operation},
    };
    use std::cell::Cell;
    #[derive(Default)]
    struct Focus(Option<Id>);
    impl operation::Operation for Focus {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn operation::Operation)) {
            operate(self);
        }
        fn focusable(
            &mut self,
            id: Option<&Id>,
            _bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if state.is_focused() {
                self.0 = id.cloned();
            }
        }
    }
    /// Whether an operation, which is how native focus, scrolling and
    /// accessibility find a widget, reaches the given focusable ID.
    struct Reaches(Id, bool);
    impl operation::Operation for Reaches {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn operation::Operation)) {
            operate(self);
        }
        fn focusable(
            &mut self,
            id: Option<&Id>,
            _bounds: Rectangle,
            _state: &mut dyn operation::Focusable,
        ) {
            self.1 |= id == Some(&self.0);
        }
    }
    let renderer = headless_renderer();
    let mut folder = project().with_kind(ProjectKind::Folder);
    folder.worktrees.clear();
    let expected = SidebarAction::OpenRepository {
        project_id: folder.id,
        repository: folder.repository.clone(),
    };
    let id = folder.chrome.open_id.clone();
    let project_id = folder.id;
    let name = format!("Open project {}", folder.name);
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![folder]);
    let state = SidebarState::default();
    let builds = Cell::new(0);
    let make = |generation, modal| {
        let (model, state, builds) = (&model, &state, &builds);
        cached_projects::view((generation, false, modal), move || {
            builds.set(builds.get() + 1);
            project_list(model, state, false, modal)
        })
    };
    let mut element = make(1, false);
    assert_eq!(
        element.as_widget().size(),
        Size::new(Length::Fill, Length::Fill)
    );
    assert_eq!(element.as_widget().size_hint(), element.as_widget().size());
    let mut tree = Tree::new(&element);
    let viewport = Rectangle::with_size(Size::new(280.0, 640.0));
    let limits = layout::Limits::new(Size::ZERO, viewport.size());
    let mut node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert_eq!(builds.get(), 1);
    let mut reaches = Reaches(id.clone(), false);
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut reaches);
    assert!(
        reaches.1,
        "native ID traversal reaches the actual cached child"
    );
    let nodes =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    let target = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|n| n.node().label() == Some(name.as_str()))
        .unwrap();
    let ax_id = target.id().clone();
    let rect = target.node().bounds().unwrap();
    assert!(rect.x1 > rect.x0 && rect.y1 > rect.y0);
    let at = Point::new(
        ((rect.x0 + rect.x1) / 2.0) as f32,
        ((rect.y0 + rect.y1) / 2.0) as f32,
    );
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::focusable::focus::<()>(id.clone()),
    );
    let mut focus = Focus::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut focus);
    assert_eq!(focus.0, Some(id.clone()));
    // Check keyboard focus across a cache hit before sending pointer input.
    // The native button intentionally clears keyboard focus on mouse press.
    element = make(1, false);
    tree.diff(element.as_widget_mut());
    node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut focus = Focus::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut focus);
    assert_eq!(
        focus.0,
        Some(id.clone()),
        "keyboard focus survives a cache hit before pointer input"
    );
    assert_eq!(builds.get(), 1);
    let mut messages = Vec::new();
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        Layout::new(&node),
        mouse::Cursor::Available(at),
        &renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &viewport,
    );
    // Row selection is published on press; navigation remains the
    // original native button's release action after the cache hit.
    assert_eq!(
        messages,
        vec![SidebarAction::SelectRows {
            row: SelectionRow::Project(project_id),
            intent: SelectionIntent::Navigate,
        }]
    );
    messages.clear();
    let mut after_press_focus = Focus::default();
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut after_press_focus,
    );
    assert_eq!(
        after_press_focus.0, None,
        "the pinned native button clears keyboard focus on pointer press"
    );
    element = make(1, false);
    tree.diff(element.as_widget_mut());
    node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let nodes =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    let retained = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|n| n.node().label() == Some(name.as_str()))
        .unwrap();
    assert_eq!(retained.id(), &ax_id);
    let mut reaches = Reaches(id.clone(), false);
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut reaches);
    assert!(reaches.1);
    let mut focus = Focus::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut focus);
    assert_eq!(
        focus.0, after_press_focus.0,
        "the post-press native focus survives the cache hit"
    );
    assert_eq!(
        builds.get(),
        1,
        "unchanged views, layout and AX must not rebuild rows"
    );
    assert_eq!(
        element.as_widget().mouse_interaction(
            &tree,
            Layout::new(&node),
            mouse::Cursor::Available(at),
            &viewport,
            &renderer
        ),
        mouse::Interaction::Grab,
        "the cached project header keeps its existing reorder gesture"
    );
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        Layout::new(&node),
        mouse::Cursor::Available(at),
        &renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &viewport,
    );
    assert_eq!(messages, vec![expected]);

    // The menu belongs to the same real child Tree, not an AX-only twin.
    messages.clear();
    for event in [
        mouse::Event::ButtonPressed(mouse::Button::Right),
        mouse::Event::ButtonReleased(mouse::Button::Right),
    ] {
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(event),
            Layout::new(&node),
            mouse::Cursor::Available(at),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
    }
    assert!(
        element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &viewport,
                Vector::ZERO
            )
            .is_some()
    );
    element = make(1, false);
    tree.diff(element.as_widget_mut());
    node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert!(
        element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &viewport,
                Vector::ZERO
            )
            .is_some()
    );
    assert_eq!(
        builds.get(),
        1,
        "the cached open menu survives an unrelated view rebuild"
    );
    element = make(2, true);
    tree.diff(element.as_widget_mut());
    node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert_eq!(
        builds.get(),
        2,
        "a changed dependency rebuilds exactly once"
    );
    assert!(
        element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &viewport,
                Vector::ZERO
            )
            .is_none()
    );
    let nodes =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    assert!(
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .any(|n| n.node().label() == Some(name.as_str()) && n.node().is_disabled())
    );
}

/// A Folder project's header is its root, so the Attention list has no
/// checkout row to put the root's panes under: each gets a row of its own,
/// named without a checkout, beside the channels. The header no longer
/// repeats them while expanded, and only aggregates them when collapsed.
#[test]
fn folder_root_panes_list_as_pane_rows_in_attention() {
    use crate::agent::{Activity, ActivityState, Provider};
    use cosmic::iced::{Point, advanced::mouse};
    let mut sidebar = Sidebar::roomy();
    let mut folder = project().with_kind(ProjectKind::Folder);
    folder.worktrees.clear();
    let root = compact_attention_terminal(CardId::new());
    let child = compact_attention_terminal(CardId::new());
    let channel = crate::model::ProjectChannel {
        title_source: Default::default(),
        id: ChannelId::new(),
        project_id: folder.id,
        title: "Waiting child".into(),
        ordinal: 1,
        shared: true,
        cards: vec![child.id],
        focused: None,
    };
    let channel_row = |state, unread| {
        SidebarChannel::new(
            channel.id,
            channel.title.clone(),
            false,
            Some(state),
            unread,
            1,
            true,
            1,
        )
        .with_attention(AttentionSummary::card(Some(state), unread, false))
    };
    folder
        .channels
        .push(channel_row(ActivityState::Waiting, true));
    let durable = Arc::new(crate::model::WorkspaceState {
        cards: vec![root.clone(), child.clone()].into(),
        channels: vec![channel.clone()],
        card_scopes: std::collections::HashMap::from([(root.id, folder.repository.clone())]),
        ..Default::default()
    });
    *sidebar.model.projects = vec![folder.clone()];
    sidebar
        .state
        .apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    for (card, state) in [
        (root.id, ActivityState::Working),
        (child.id, ActivityState::Waiting),
    ] {
        sidebar.state.observe_card_update(card);
        sidebar.state.set_card_activity(
            card,
            Activity {
                provider: Provider::Omp,
                state,
            },
        );
    }
    refresh_attention(&mut sidebar.state, &mut sidebar.model, &durable);
    let listed = visible_selection_rows(&sidebar.model, &sidebar.state, false);
    assert_eq!(
        listed,
        vec![
            SelectionRow::Project(folder.id),
            SelectionRow::Channel(channel.id),
            SelectionRow::Card(root.id),
        ]
    );
    let plain = format!("Open project {}", folder.name);
    let pane = "Open pane Agent, 1 working, 1 unread conversation";
    let (rows, _) = sidebar.attention_frame();
    assert!(
        rows.iter().any(|(name, _)| name == &plain),
        "the expanded header leaves the root's news to its pane row: {rows:?}"
    );
    let bounds = rows.iter().find(|(name, _)| name == pane).unwrap().1;
    assert!(
        sidebar
            .click(
                mouse::Button::Left,
                Point::new(
                    ((bounds[0] + bounds[2]) / 2.0) as f32,
                    ((bounds[1] + bounds[3]) / 2.0) as f32,
                )
            )
            .contains(&SidebarAction::OpenCard { card: root.id })
    );

    // Reading the root pane dims its own row only.
    read(&mut sidebar.state, &sidebar.model, root.id, Vec::new());
    refresh_attention(&mut sidebar.state, &mut sidebar.model, &durable);
    assert_eq!(
        sidebar.state.retained_attention,
        BTreeMap::from([(SelectionRow::Card(root.id), Listing::Read)])
    );
    assert_eq!(
        visible_selection_rows(&sidebar.model, &sidebar.state, false),
        listed
    );
    let (rows, _) = sidebar.attention_frame();
    assert!(
        rows.iter()
            .any(|(name, _)| name == "Open pane Agent, 1 working, read")
    );
    assert!(
        rows.iter()
            .any(|(name, _)| name == "Hide read and handled rows")
    );
    sidebar
        .state
        .apply(&SidebarAction::ToggleProject(folder.id));
    let (rows, _) = sidebar.attention_frame();
    assert!(
        rows.iter()
            .any(|(name, _)| name == &format!("{plain}, agent needs input")),
        "the collapsed header aggregates the child's outstanding wait"
    );
    assert!(
        !rows
            .iter()
            .any(|(name, _)| name.starts_with("Open project ") && name.ends_with(", read"))
    );
    let compact = accessible_names(
        &sidebar.renderer,
        body(&sidebar.model, &sidebar.state, true, false),
    );
    assert!(compact.iter().any(|name| name.starts_with(&folder.name)
        && name.contains("An agent is waiting for input")
        && !name.ends_with(", read")));
    sidebar
        .state
        .apply(&SidebarAction::ToggleProject(folder.id));

    // Hidden, the working root pane moves to In progress.
    sidebar.state.apply(&SidebarAction::HideReadAttention);
    refresh_attention(&mut sidebar.state, &mut sidebar.model, &durable);
    assert_eq!(
        visible_selection_rows(&sidebar.model, &sidebar.state, false),
        vec![
            SelectionRow::Project(folder.id),
            SelectionRow::Channel(channel.id),
            SelectionRow::Card(root.id),
        ],
        "the same pane, now the last row under In progress"
    );
    assert_eq!(
        in_progress_rows(&sidebar.model, &sidebar.state, false)
            .iter()
            .map(|row| row.row())
            .collect::<Vec<_>>(),
        vec![SelectionRow::Card(root.id)]
    );
}

#[test]
fn folder_projection_follows_folder_identity_and_its_kind() {
    let mut folder = project().with_kind(ProjectKind::Folder);
    folder.worktrees.clear();
    let key = folder.folder_key().unwrap();
    let mut model = SidebarModel::ready(SidebarMode::Expanded, vec![folder.clone()]);
    let terminal = compact_attention_terminal(CardId::new());
    let card = terminal.id;
    let durable = Arc::new(crate::model::WorkspaceState {
        cards: vec![terminal].into(),
        ..Default::default()
    });
    let mut state = SidebarState::default();
    state.apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    state.observe_agent_update(card, folder.id, key.worktree_id);
    refresh_attention(&mut state, &mut model, &durable);
    let rebuilt = state.attention_projection_rebuilds;
    state.refresh_attention_projection(&durable, &model.projects);
    assert_eq!(state.attention_projection_rebuilds, rebuilt);
    // A pane row not drawn now is not kept.
    state.acknowledge_agent_update(card, &AttentionPlacement::default());
    assert!(state.retained_attention.is_empty());
    state.observe_agent_update(card, folder.id, key.worktree_id);
    refresh_attention(&mut state, &mut model, &durable);
    read(&mut state, &model, card, Vec::new());
    assert_eq!(
        state.retained_attention,
        BTreeMap::from([(SelectionRow::Card(card), Listing::Read)]),
        "a Folder root keeps its pane's row, never its header"
    );
    assert_eq!(
        model.projects[0].panes[0].checkout(&model.projects[0]),
        None
    );
    refresh_attention(&mut state, &mut model, &durable);
    let folder_inventory = state
        .attention_projection
        .as_ref()
        .unwrap()
        .inventory
        .clone();
    let clean_rebuilt = state.attention_projection_rebuilds;

    // Identical ordered root inventory, but now a real Git worktree whose UUID
    // collides with the Folder synthetic identity: kind must invalidate the cache.
    let mut git = folder.clone().with_kind(ProjectKind::Git);
    let mut root = worktree(999, "main", false);
    root.id = key.worktree_id;
    root.path = folder.repository.clone();
    git.worktrees = vec![root];
    *model.projects = vec![git];
    refresh_attention(&mut state, &mut model, &durable);
    assert_eq!(state.attention_projection_rebuilds, clean_rebuilt + 1);
    assert_eq!(
        state.attention_projection.as_ref().unwrap().inventory,
        folder_inventory
    );
    assert!(
        !state
            .attention_projection
            .as_ref()
            .unwrap()
            .folders
            .contains(&folder.id)
    );
    // The pane is the same pane, so its kept row stays; it now names the
    // checkout it runs in.
    state.prune_retained_attention(&model);
    assert_eq!(state.retained_attention.len(), 1);
    assert_eq!(
        model.projects[0].panes[0].checkout(&model.projects[0]),
        Some("main")
    );
    let rebuilt = state.attention_projection_rebuilds;
    *model.projects = vec![folder.clone()];
    refresh_attention(&mut state, &mut model, &durable);
    assert!(state.attention_projection_rebuilds > rebuilt);
    let rebuilt = state.attention_projection_rebuilds;
    model.projects[0].repository = PathBuf::from("/different-folder-root");
    state.refresh_attention_projection(&durable, &model.projects);
    assert_eq!(
        state.attention_projection_rebuilds,
        rebuilt + 1,
        "root inventory changes also invalidate typed projection"
    );
    model.projects.clear();
    state.prune_retained_attention(&model);
    assert!(
        state.retained_attention.is_empty(),
        "removing the Folder removes its kept row"
    );
}

#[test]
fn folder_root_attention_is_rendered_and_filtered_without_a_child_worktree() {
    use crate::agent::{Activity, ActivityState, Provider};
    use cosmic::iced::{Point, advanced::mouse};
    let mut sidebar = Sidebar::roomy();
    let mut folder = project().with_kind(ProjectKind::Folder);
    folder.worktrees.clear();
    let key = folder.folder_key().unwrap();
    let plain = format!("Open project {}", folder.name);
    let open = SidebarAction::OpenRepository {
        project_id: folder.id,
        repository: folder.repository.clone(),
    };
    let terminal = compact_attention_terminal(CardId::new());
    let card = terminal.id;
    let durable = Arc::new(crate::model::WorkspaceState {
        card_scopes: std::collections::HashMap::from([(card, folder.repository.clone())]),
        cards: vec![terminal].into(),
        ..Default::default()
    });
    *sidebar.model.projects = vec![folder];
    let (quiet_rows, quiet_image) = sidebar.attention_frame();
    assert_eq!(
        quiet_rows.iter().filter(|(name, _)| name == &plain).count(),
        1
    );
    assert!(
        !quiet_rows
            .iter()
            .any(|(name, _)| name.starts_with("Collapse project "))
    );
    sidebar
        .state
        .observe_agent_update(card, key.project_id, key.worktree_id);
    let (unread_rows, unread_image) = sidebar.attention_frame();
    assert!(unread_rows.iter().any(|(name, _)| name == &plain));
    assert_eq!(
        unread_rows.iter().filter(|(name, _)| name == "1").count(),
        1,
        "the rendered Folder root has exactly one unread count"
    );
    assert_ne!(
        quiet_image, unread_image,
        "the actual expanded Folder frame must paint its unread indicator"
    );
    sidebar.state.set_agent_activity(
        card,
        key.project_id,
        key.worktree_id,
        Activity {
            provider: Provider::Omp,
            state: ActivityState::Working,
        },
    );
    let working = format!("{plain}, agent working");
    let (rows, _) = sidebar.attention_frame();
    assert_eq!(rows.iter().filter(|(name, _)| name == &working).count(), 1);
    sidebar
        .state
        .apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    refresh_attention(&mut sidebar.state, &mut sidebar.model, &durable);
    let (rows, _) = sidebar.attention_frame();
    assert!(
        rows.iter().any(|(name, _)| name == &plain)
            && rows
                .iter()
                .any(|(name, _)| name == "Open pane Agent, 1 working, 1 unread conversation"),
        "root unread lists the Folder with its pane under it: {rows:?}"
    );
    sidebar
        .state
        .acknowledge_agent_update(card, &AttentionPlacement::default());
    refresh_attention(&mut sidebar.state, &mut sidebar.model, &durable);
    let (rows, _) = sidebar.attention_frame();
    assert!(
        !rows.iter().any(|(name, _)| name.starts_with(&plain)),
        "Working alone is not a reason to list the Folder"
    );
    assert!(
        rows.iter()
            .any(|(name, _)| name == "Open pane Agent in Cagents, 1 working"),
        "it is in progress instead: {rows:?}"
    );
    sidebar.state.set_agent_activity(
        card,
        key.project_id,
        key.worktree_id,
        Activity {
            provider: Provider::Omp,
            state: ActivityState::Waiting,
        },
    );
    refresh_attention(&mut sidebar.state, &mut sidebar.model, &durable);
    let (rows, _) = sidebar.attention_frame();
    assert!(
        rows.iter()
            .any(|(name, _)| name == "Open pane Agent, 1 waiting for input")
    );
    let compact_names = accessible_names(
        &sidebar.renderer,
        body(&sidebar.model, &sidebar.state, true, false),
    );
    assert!(
        compact_names
            .iter()
            .any(|name| name.starts_with(&sidebar.model.projects[0].name))
            && compact_names
                .iter()
                .any(|name| name == "Open pane Agent, 1 waiting for input"),
        "the compact rail lists the Folder and its waiting pane: {compact_names:?}"
    );
    sidebar.state.apply(&SidebarAction::ToggleControls);
    sidebar
        .state
        .apply(&SidebarAction::SearchChanged("no-matching-folder".into()));
    let (rows, _) = sidebar.attention_frame();
    assert!(
        !rows.iter().any(|(name, _)| name.starts_with(&plain)),
        "root attention does not bypass search"
    );
    let compact_names = accessible_names(
        &sidebar.renderer,
        body(&sidebar.model, &sidebar.state, true, false),
    );
    assert!(
        !compact_names
            .iter()
            .any(|name| name.starts_with(&sidebar.model.projects[0].name)),
        "an available compact search still filters the Folder root"
    );
    sidebar.state.apply(&SidebarAction::ToggleControls);
    sidebar.state.forget_card_attention(card);
    refresh_attention(&mut sidebar.state, &mut sidebar.model, &durable);
    let (rows, _) = sidebar.attention_frame();
    assert!(!rows.iter().any(|(name, _)| name.starts_with(&plain)));
    let compact_names = accessible_names(
        &sidebar.renderer,
        body(&sidebar.model, &sidebar.state, true, false),
    );
    assert!(
        !compact_names
            .iter()
            .any(|name| name.starts_with(&sidebar.model.projects[0].name)),
        "quiet Folder roots stay out of compact Attention too"
    );
    sidebar
        .state
        .apply(&SidebarAction::SetFilter(SidebarFilter::All));
    let (rows, _) = sidebar.attention_frame();
    let (_, bounds) = rows.iter().find(|(name, _)| name == &plain).unwrap();
    assert!(
        bounds[0] >= 0.0
            && bounds[1] >= 0.0
            && bounds[2] > bounds[0]
            && bounds[3] > bounds[1]
            && bounds[2] <= f64::from(sidebar.bounds.width)
            && bounds[3] <= f64::from(sidebar.bounds.height)
    );
    let actions = sidebar.click(
        mouse::Button::Left,
        Point::new(
            ((bounds[0] + bounds[2]) / 2.0) as f32,
            ((bounds[1] + bounds[3]) / 2.0) as f32,
        ),
    );
    assert!(
        actions.contains(&open),
        "the unchanged actual Folder control still opens its exact destination"
    );
}

#[test]
fn folder_root_attention_does_not_duplicate_children_or_change_git_headers() {
    use crate::agent::{Activity, ActivityState, Provider};
    let mut sidebar = Sidebar::roomy();
    let mut folder = project().with_kind(ProjectKind::Folder);
    folder.worktrees.clear();
    let key = folder.folder_key().unwrap();
    let root = compact_attention_terminal(CardId::new());
    let child = compact_attention_terminal(CardId::new());
    let channel = crate::model::ProjectChannel {
        title_source: Default::default(),
        id: ChannelId::new(),
        project_id: folder.id,
        title: "Child work".into(),
        ordinal: 1,
        shared: true,
        cards: vec![child.id],
        focused: None,
    };
    folder.channels.push(
        SidebarChannel::new(
            channel.id,
            channel.title.clone(),
            false,
            Some(ActivityState::Waiting),
            true,
            1,
            true,
            1,
        )
        .with_attention(AttentionSummary::card(
            Some(ActivityState::Waiting),
            true,
            false,
        )),
    );
    let durable = Arc::new(crate::model::WorkspaceState {
        cards: vec![root.clone(), child.clone()].into(),
        channels: vec![channel],
        card_scopes: std::collections::HashMap::from([(root.id, folder.repository.clone())]),
        ..Default::default()
    });
    *sidebar.model.projects = vec![folder.clone()];
    sidebar.state.observe_card_update(root.id);
    sidebar.state.observe_card_update(child.id);
    sidebar.state.set_card_activity(
        root.id,
        Activity {
            provider: Provider::Omp,
            state: ActivityState::Working,
        },
    );
    sidebar.state.set_card_activity(
        child.id,
        Activity {
            provider: Provider::Omp,
            state: ActivityState::Waiting,
        },
    );
    sidebar
        .state
        .refresh_attention_projection(&durable, &sidebar.model.projects);
    assert_eq!(
        sidebar.state.worktree_attention(folder.id, key.worktree_id),
        AttentionSummary::card(Some(ActivityState::Working), true, false)
    );
    assert_eq!(
        sidebar.state.project_attention(folder.id),
        AttentionSummary {
            unread: 2,
            working: 1,
            waiting: 1,
            failed: 0,
            limited: 0,
        }
    );
    let plain = format!("Open project {}", folder.name);
    let (rows, _) = sidebar.attention_frame();
    assert!(
        rows.iter()
            .any(|(name, _)| name == &format!("{plain}, agent working"))
    );
    assert!(
        !rows
            .iter()
            .any(|(name, _)| name == &format!("{plain}, agent needs input")),
        "expanded root does not repeat the child's aggregate wait"
    );
    assert!(
        rows.iter()
            .any(|(name, _)| name.starts_with("Open channel Child work"))
    );
    assert_eq!(
        rows.iter().filter(|(name, _)| name == "1").count(),
        2,
        "expanded Folder root and channel each render only their own unread count"
    );
    assert!(
        !rows.iter().any(|(name, _)| name == "2"),
        "the project aggregate is not repeated above its expanded children"
    );
    sidebar
        .state
        .apply(&SidebarAction::SetFilter(SidebarFilter::Attention));
    sidebar
        .state
        .apply(&SidebarAction::ToggleProject(folder.id));
    let (rows, _) = sidebar.attention_frame();
    assert!(
        rows.iter()
            .any(|(name, _)| name == &format!("{plain}, agent needs input")),
        "collapsed header retains the existing full-project aggregate"
    );
    // The same header policy for Git is unchanged: expanded worktree rows
    // own the news, while a collapsed Attention header aggregates it.
    sidebar.model.projects[0].kind = ProjectKind::Git;
    let (rows, _) = sidebar.attention_frame();
    assert!(
        rows.iter()
            .any(|(name, _)| name == &format!("{plain}, agent needs input"))
    );
    sidebar
        .state
        .apply(&SidebarAction::ToggleProject(folder.id));
    let (rows, _) = sidebar.attention_frame();
    assert!(rows.iter().any(|(name, _)| name == &plain));
    assert!(
        !rows
            .iter()
            .any(|(name, _)| name == &format!("{plain}, agent working"))
    );
}

/// A project whose checkout could not be read is still a row, and that row
/// offers exactly one thing: reading it again. Its worktrees, channels and
/// pull requests came from the read that failed, so nothing built from
/// them may be pressable.
#[test]
fn folder_projects_remain_selectable_without_git_actions_in_both_sidebar_modes() {
    let renderer = headless_renderer();
    let mut entry = project().with_kind(ProjectKind::Folder);
    entry.worktrees.clear();
    entry.revoke_github();
    let open = SidebarAction::OpenRepository {
        project_id: entry.id,
        repository: entry.repository.clone(),
    };
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let state = SidebarState::default();
    for compact in [false, true] {
        let actions = frame_enter_presses(&renderer, body(&model, &state, compact, false));
        assert!(actions.contains(&open));
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, SidebarAction::NewWorktree { .. }))
        );
        let names = accessible_names(&renderer, body(&model, &state, compact, false));
        assert!(!names.iter().any(|name| name.starts_with("New worktree")));
    }
}

#[test]
fn an_unreadable_project_keeps_its_row_and_offers_only_a_reload() {
    let renderer = headless_renderer();
    let mut entry = project();
    entry.channels.push(SidebarChannel::new(
        ChannelId::from_uuid(Uuid::from_u128(20)),
        "Investigate login".into(),
        false,
        None,
        true,
        1,
        true,
        1,
    ));
    entry.unreachable = Some("fatal: not a git repository".into());
    let reload = SidebarAction::RefreshProject {
        project_id: entry.id,
        repository: entry.repository.clone(),
    };
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let state = SidebarState::default();
    for compact in [false, true] {
        let actions = frame_enter_presses(&renderer, body(&model, &state, compact, false));
        assert!(
            actions.contains(&reload),
            "compact {compact}: the row cannot be read again: {actions:?}"
        );
        assert!(
            actions.iter().all(|action| *action == reload),
            "compact {compact}: an unreadable project offered more than a reload: {actions:?}"
        );
        assert!(
            frame_enter_presses(&renderer, body(&model, &state, compact, true)).is_empty(),
            "compact {compact}: the reload stays withheld while a modal is up"
        );
        let names = accessible_names(&renderer, body(&model, &state, compact, false));
        assert!(
            names.iter().any(|name| name
                == "Couldn’t read project Cagents: fatal: not a git repository. Try again"),
            "compact {compact}: no accessible name reports the failure: {names:?}"
        );
    }
}

#[test]
fn a_read_already_in_flight_withholds_the_retry_and_says_so() {
    let renderer = headless_renderer();
    let mut entry = project();
    entry.unreachable = Some("fatal: not a git repository".into());
    entry.reloading = true;
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let state = SidebarState::default();
    for compact in [false, true] {
        assert!(
            frame_enter_presses(&renderer, body(&model, &state, compact, false)).is_empty(),
            "compact {compact}: a retry was offered while a read was already running"
        );
        let names = accessible_names(&renderer, body(&model, &state, compact, false));
        assert!(
            names.iter().any(|name| name
                == "Couldn’t read project Cagents: fatal: not a git repository. Trying again"),
            "compact {compact}: nothing reports that the read is running: {names:?}"
        );
    }
}

#[test]
fn a_filter_cannot_hide_a_project_that_cannot_be_read() {
    let mut entry = project();
    entry.unreachable = Some("fatal: not a git repository".into());
    let id = entry.id;
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    for filter in [
        SidebarFilter::All,
        SidebarFilter::Attention,
        SidebarFilter::PullRequests,
    ] {
        let state = SidebarState {
            filter,
            ..SidebarState::default()
        };
        assert_eq!(
            filtered_projects(&model, &state, false)
                .iter()
                .map(|project| project.project.id)
                .collect::<Vec<_>>(),
            vec![id],
            "{filter:?} hid the project that cannot be read"
        );
    }
}

#[test]
fn a_running_worktree_operation_stays_cancellable_on_a_project_that_cannot_be_read() {
    let renderer = headless_renderer();
    let mut entry = project();
    let operation = crate::model::OperationId::new();
    entry.unreachable = Some("fatal: not a git repository".into());
    entry.operations.push(WorktreeOperationRow {
        id: operation,
        label: "review".into(),
        detail: "Running git".into(),
        phase: WorktreeOperationPhase::Working,
        cancellable: true,
        reopenable: false,
    });
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let state = SidebarState::default();
    for compact in [false, true] {
        let actions = frame_enter_presses(&renderer, body(&model, &state, compact, false));
        assert!(
            actions.contains(&SidebarAction::CancelWorktreeOperation(operation)),
            "compact {compact}: a running operation lost its cancel: {actions:?}"
        );
    }
}

/// A deletion runs behind its row (#264): the row being deleted says so
/// and offers nothing, and one whose deletion failed says why, stays
/// navigable, and offers to run it again.
#[test]
fn a_row_being_deleted_is_inert_and_a_failed_one_offers_a_retry() {
    let renderer = headless_renderer();
    let mut entry = project();
    let project_id = entry.id;
    let (kept, doomed) = (entry.worktrees[0].id, entry.worktrees[1].id);
    let channel = ChannelId::new();
    entry.channels = vec![SidebarChannel::new(
        channel,
        "Investigation".into(),
        false,
        None,
        false,
        1,
        true,
        1,
    )];
    let opens = |actions: &[SidebarAction], id: WorktreeId| {
        actions.iter().any(|action| {
            matches!(action, SidebarAction::OpenWorktree { worktree_id, .. } if *worktree_id == id)
        })
    };

    entry.worktrees[1].deletion = Some(RowDeletion::Deleting);
    entry.channels[0].deletion = Some(RowDeletion::Deleting);
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry.clone()]);
    let mut state = expanded_worktree_state();
    state.set_worktrees_with_panes(BTreeSet::from([(project_id, kept)]));
    for compact in [false, true] {
        let actions = frame_enter_presses(&renderer, body(&model, &state, compact, false));
        assert!(opens(&actions, kept), "compact {compact}: {actions:?}");
        assert!(
            !opens(&actions, doomed),
            "compact {compact}: a worktree being deleted can still be opened"
        );
        assert!(
            !actions.contains(&SidebarAction::OpenChannel {
                channel_id: channel
            }),
            "compact {compact}: a channel being deleted can still be opened"
        );
        let names = accessible_names(&renderer, body(&model, &state, compact, false));
        for expected in [
            // Worktree identity retains its literal branch through PR lifecycle.
            "Deleting worktree fix-sidebar",
            "Deleting channel Investigation",
        ] {
            assert!(
                names.iter().any(|name| name == expected),
                "compact {compact}: {expected:?} missing from {names:?}"
            );
        }
    }

    entry.worktrees[1].deletion = Some(RowDeletion::Failed("The worktree has changes".into()));
    entry.channels[0].deletion = Some(RowDeletion::Failed("Daemon unreachable".into()));
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let actions = frame_enter_presses(&renderer, body(&model, &state, false, false));
    assert!(
        opens(&actions, doomed),
        "a failed deletion leaves the row usable"
    );
    assert!(
        actions.contains(&SidebarAction::RetryWorktreeDeletion {
            project_id,
            worktree_id: doomed,
        }),
        "{actions:?}"
    );
    assert!(
        actions.contains(&SidebarAction::RetryChannelDeletion {
            channel_id: channel
        }),
        "{actions:?}"
    );
    let names = accessible_names(&renderer, body(&model, &state, false, false));
    assert!(
        names
            .iter()
            .any(|name| name.ends_with("could not be deleted: The worktree has changes")),
        "{names:?}"
    );
}

#[test]
fn a_failed_worktree_operation_offers_dismiss_and_nothing_else() {
    let renderer = headless_renderer();
    let mut entry = project();
    let operation = crate::model::OperationId::new();
    entry.operations.push(WorktreeOperationRow {
        id: operation,
        label: "review".into(),
        detail: "Cancelled before git ran".into(),
        phase: WorktreeOperationPhase::Failed,
        cancellable: false,
        reopenable: false,
    });
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let state = SidebarState::default();
    for compact in [false, true] {
        let actions = frame_enter_presses(&renderer, body(&model, &state, compact, false));
        assert!(
            actions.contains(&SidebarAction::DismissWorktreeOperation(operation)),
            "compact {compact}: a failed operation cannot be dismissed: {actions:?}"
        );
        assert!(
            !actions.contains(&SidebarAction::CancelWorktreeOperation(operation)),
            "compact {compact}: a finished operation still offers cancel: {actions:?}"
        );
    }
}

/// A failure this session started keeps its input: the row reopens it in
/// the dialog to edit, or runs it again, beside dismissing it (#499).
#[test]
fn a_reopenable_failed_worktree_operation_offers_edit_and_retry() {
    let renderer = headless_renderer();
    let mut entry = project();
    let operation = crate::model::OperationId::new();
    entry.operations.push(WorktreeOperationRow {
        id: operation,
        label: "review".into(),
        detail: "destination already exists".into(),
        phase: WorktreeOperationPhase::Failed,
        cancellable: false,
        reopenable: true,
    });
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let state = SidebarState::default();
    let actions = frame_enter_presses(&renderer, body(&model, &state, false, false));
    for action in [
        SidebarAction::EditWorktreeOperation(operation),
        SidebarAction::RetryWorktreeOperation(operation),
        SidebarAction::DismissWorktreeOperation(operation),
    ] {
        assert!(actions.contains(&action), "{action:?} missing: {actions:?}");
    }
    let behind_a_dialog = frame_enter_presses(&renderer, body(&model, &state, false, true));
    assert!(
        !behind_a_dialog.iter().any(|action| matches!(
            action,
            SidebarAction::EditWorktreeOperation(_) | SidebarAction::RetryWorktreeOperation(_)
        )),
        "{behind_a_dialog:?}"
    );
}

#[test]
fn the_sidebar_header_keeps_its_titlebar_gestures_in_every_state() {
    use cosmic::iced::{
        Event, Point, Rectangle,
        advanced::{Shell, clipboard},
        mouse,
    };
    let renderer = headless_renderer();
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let state = SidebarState::default();
    // The window is undecorated, so the compact rail is as much of a
    // titlebar as the expanded header, and a dialog does not stop the user
    // moving the window.
    for compact in [false, true] {
        for modal_open in [false, true] {
            let mut element = header(
                &model,
                &state,
                compact,
                false,
                modal_open,
                &crate::settings::KeyboardShortcuts::default(),
            );
            let mut tree = Tree::new(&element);
            let viewport = Rectangle::with_size(Size::new(260.0, 44.0));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            let mut messages = Vec::new();
            for event in [
                mouse::Event::ButtonPressed(mouse::Button::Right),
                mouse::Event::ButtonReleased(mouse::Button::Right),
            ] {
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Mouse(event),
                    Layout::new(&node),
                    mouse::Cursor::Available(Point::new(2.0, 2.0)),
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
            }
            assert!(
                messages.contains(&SidebarAction::WindowMenu),
                "compact={compact}, modal_open={modal_open}"
            );
        }
    }
}

#[test]
fn pr_browser_keeps_authenticated_projects_without_matching_worktrees() {
    let mut entry = project();
    entry.github_ready = true;
    entry.pull_requests.clear();
    entry.worktrees.clear();
    entry.channels.clear();
    let model = SidebarModel::ready(SidebarMode::Expanded, vec![entry]);
    let state = SidebarState {
        filter: SidebarFilter::PullRequests,
        ..SidebarState::default()
    };
    for compact in [false, true] {
        assert_eq!(filtered_projects(&model, &state, compact).len(), 1);
    }
}

#[test]
fn compact_search_and_pr_filter_apply_while_the_popover_is_open() {
    let model = SidebarModel::ready(SidebarMode::Compact, vec![project()]);
    let mut state = SidebarState::default();
    state.apply(&SidebarAction::ToggleControls);
    state.apply(&SidebarAction::SearchChanged("fix-sidebar".into()));
    assert_eq!(
        filtered_projects(&model, &state, true)[0].worktrees.len(),
        1
    );
    state.apply(&SidebarAction::SearchChanged(String::new()));
    state.apply(&SidebarAction::SetFilter(SidebarFilter::PullRequests));
    let filtered = filtered_projects(&model, &state, true);
    assert_eq!(
        filtered[0].worktrees[0].branch.as_deref(),
        Some("fix-sidebar")
    );
    assert_eq!(filtered[0].worktrees.len(), 1);
    // Dismissing the popover ends both.
    state.apply(&SidebarAction::ToggleControls);
    assert_eq!(
        filtered_projects(&model, &state, true)[0].worktrees.len(),
        2
    );
}

#[test]
fn the_search_input_carries_the_focus_target_the_application_reveals() {
    let mut state = SidebarState::default();
    assert!(
        Tree::new(search(&state, false))
            .find(&search_focus_id())
            .is_some()
    );

    // A query adds the clear action, which must not cost the input the id
    // the application focuses.
    state.apply(&SidebarAction::SearchChanged("cagents".into()));
    assert!(
        Tree::new(search(&state, false))
            .find(&search_focus_id())
            .is_some()
    );
}

#[test]
fn the_search_is_reachable_exactly_when_the_pane_renders_its_input() {
    // The application refuses to focus the input when this is false, so a
    // pane that renders one anyway would leave the shortcut dead, and one
    // that renders none while this is true would blur the focused widget
    // and focus nothing.
    let empty = SidebarModel::ready(SidebarMode::Expanded, Vec::new());
    let populated = SidebarModel::ready(SidebarMode::Expanded, vec![project()]);
    let mut state = SidebarState::default();
    state.apply(&SidebarAction::ToggleControls);
    assert!(state.controls_expanded());

    for (model, mode) in [
        (&empty, SidebarMode::Compact),
        (&empty, SidebarMode::Expanded),
        (&populated, SidebarMode::Compact),
        (&populated, SidebarMode::Expanded),
    ] {
        let rendered = control_ids(&view(model, &state, mode, DEFAULT_EXPANDED_WIDTH, false))
            .contains(&search_focus_id().to_string());
        assert_eq!(
            search_is_reachable(model, mode),
            rendered,
            "reachability disagrees with the pane at {mode:?} over {} projects",
            model.projects.len()
        );
    }
}

#[test]
fn the_controls_toggle_is_offered_only_where_it_opens_something() {
    let mut state = SidebarState::default();
    state.apply(&SidebarAction::ToggleControls);
    let toggle = stable_id("toggle-controls").to_string();
    let offered = |model: &SidebarModel| {
        control_ids(&view(
            model,
            &state,
            SidebarMode::Expanded,
            DEFAULT_EXPANDED_WIDTH,
            false,
        ))
        .contains(&toggle)
    };

    assert!(offered(&SidebarModel::ready(
        SidebarMode::Expanded,
        vec![project()]
    )));
    // The pane renders the controls only for a non-empty project list, and
    // `SidebarModel::error` empties that list, so in either empty state the
    // toggle would offer, and paint itself selected over, a panel that
    // never opens.
    assert!(!offered(&SidebarModel::ready(
        SidebarMode::Expanded,
        Vec::new()
    )));
    assert!(!offered(&SidebarModel::error(
        SidebarMode::Expanded,
        "disk on fire"
    )));
}

#[test]
fn check_status_prefers_failure_then_pending_then_success() {
    let check = |bucket: &str, state: &str| PullRequestCheck {
        bucket: RemoteValue(bucket.into()),
        completed_at: None,
        description: String::new(),
        event: String::new(),
        link: Url::parse("https://github.com/example/check").unwrap(),
        name: "check".into(),
        started_at: None,
        state: RemoteValue(state.into()),
        workflow: String::new(),
    };
    assert_eq!(
        CheckStatus::from_checks(&[check("pass", "success")]),
        CheckStatus::Passing
    );
    assert_eq!(
        CheckStatus::from_checks(&[check("pending", "queued")]),
        CheckStatus::Pending
    );
    assert_eq!(
        CheckStatus::from_checks(&[check("pending", "queued"), check("fail", "failure")]),
        CheckStatus::Failing
    );
    assert_eq!(CheckStatus::from_checks(&[]), CheckStatus::Unknown);
}

#[test]
fn terminal_pull_request_rows_hide_retained_review_and_check_badges() {
    use cosmic::iced::{
        Color, Rectangle,
        advanced::{
            mouse,
            renderer::{Headless, Style},
        },
    };

    // Draw the actual expanded row and compact rail. Terminal rows with
    // stale remote flags must look exactly like those with no live flags.
    let frame = |row: &SidebarWorktree, compact: bool| {
        let mut renderer = headless_renderer();
        let mut element = worktree_view(
            project_id(1),
            row,
            false,
            false,
            None,
            false,
            true,
            &[],
            WorktreeRowState {
                has_panes: true,
                ..WorktreeRowState::default()
            },
            compact,
            None,
            "",
            true,
        );
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(460.0, WORKTREE_ROW_HEIGHT));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        element.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::dark(),
            &Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        renderer.screenshot(
            Size::new(460, WORKTREE_ROW_HEIGHT as u32),
            1.0,
            Color::BLACK,
        )
    };

    for state in [
        WorktreePullRequestState::Merged,
        WorktreePullRequestState::Closed,
    ] {
        let mut row = project().worktrees.remove(1);
        let mut pr = project().pull_requests.remove(0);
        pr.state = Some(state);
        pr.needs_review = true;
        pr.review_requested_again = true;
        for checks in [
            CheckStatus::Pending,
            CheckStatus::Passing,
            CheckStatus::Failing,
        ] {
            pr.checks = checks;
            row.set_pull_request(pr.clone());
            let terminal = row.pull_request().unwrap();
            assert!(terminal.needs_review);
            assert!(terminal.review_requested_again);
            assert_eq!(terminal.checks, checks, "remote metadata is retained");
            assert!(!worktree_indicators(&row).pending_checks);
            for description in [&row.chrome.accessible_name, &row.chrome.tooltip] {
                assert!(description.contains(match state {
                    WorktreePullRequestState::Merged => "merged",
                    _ => "closed",
                }));
                assert!(!description.contains(", checks "), "{description}");
                assert!(!description.contains("needs your review"), "{description}");
                assert!(
                    !description.contains("review re-requested"),
                    "{description}"
                );
            }
            assert_eq!(
                pull_request_tone(terminal),
                match state {
                    WorktreePullRequestState::Merged => PullRequestTone::Merged,
                    _ => PullRequestTone::Danger,
                }
            );

            let mut cleared = row.clone();
            let mut no_flags = pr.clone();
            no_flags.needs_review = false;
            no_flags.review_requested_again = false;
            no_flags.checks = CheckStatus::Unknown;
            cleared.set_pull_request(no_flags);
            for compact in [false, true] {
                assert_eq!(
                    frame(&row, compact),
                    frame(&cleared, compact),
                    "terminal badges leaked: state={state:?}, checks={checks:?}, compact={compact}"
                );
            }
        }
    }
}

#[test]
fn live_and_reopened_pull_request_rows_keep_review_and_check_status() {
    let mut row = project().worktrees.remove(1);
    let mut pr = project().pull_requests.remove(0);
    pr.needs_review = true;
    pr.review_requested_again = true;
    pr.checks = CheckStatus::Pending;
    for state in [
        None,
        Some(WorktreePullRequestState::Open),
        Some(WorktreePullRequestState::Draft),
    ] {
        pr.state = state;
        row.set_pull_request(pr.clone());
        assert!(worktree_indicators(&row).pending_checks);
        assert!(row.pull_request().unwrap().review_requested());
        assert_eq!(
            row.pull_request().unwrap().active_checks(),
            Some(CheckStatus::Pending)
        );
        assert!(row.chrome.accessible_name.contains(", checks pending"));
        assert!(row.chrome.tooltip.contains(", checks pending"));
        assert!(
            row.chrome
                .accessible_name
                .contains("review re-requested from you")
        );
        assert!(row.chrome.tooltip.contains("review re-requested from you"));
    }

    pr.state = Some(WorktreePullRequestState::Closed);
    row.set_pull_request(pr);
    let mut reopened = row.pull_request().unwrap().clone();
    reopened.state = Some(WorktreePullRequestState::Open);
    reopened.review_requested_again = false;
    row.set_pull_request(reopened);
    assert!(worktree_indicators(&row).pending_checks);
    assert!(row.chrome.accessible_name.contains("needs your review"));
    assert!(row.chrome.tooltip.contains("needs your review"));
    assert!(!row.chrome.accessible_name.contains("review re-requested"));
}

#[test]
fn pull_request_filter_rows_open_reviews_and_other_rows_open_worktrees() {
    let project = project();
    let mut worktree = project.worktrees[0].clone();
    let pr = project.pull_requests[0].clone();
    worktree.set_pull_request(pr.clone());
    assert!(
        matches!(worktree_primary_action(project.id, &worktree, true),
        SidebarAction::OpenPullRequest { project_id, number, url }
        if project_id == project.id && number == pr.number && url == pr.url)
    );
    assert!(
        matches!(worktree_primary_action(project.id, &worktree, false),
        SidebarAction::OpenWorktree { worktree_id, .. } if worktree_id == worktree.id)
    );
    worktree.clear_pull_request();
    assert!(
        matches!(worktree_primary_action(project.id, &worktree, true),
        SidebarAction::OpenWorktree { worktree_id, .. } if worktree_id == worktree.id)
    );
}

#[test]
fn worktree_pull_requests_require_an_exact_attached_branch_in_the_same_project() {
    let project = project();
    let pull_request = &project.pull_requests[0];
    let attached = worktree(7, &pull_request.head_ref, false);
    assert_eq!(
        matching_pull_request(&attached, &project.pull_requests)
            .map(|pull_request| pull_request.number),
        Some(pull_request.number),
    );
    let other_branch = worktree(8, &format!("{}-other", pull_request.head_ref), false);
    assert!(matching_pull_request(&other_branch, &project.pull_requests).is_none());
    let case_changed = worktree(9, &pull_request.head_ref.to_ascii_uppercase(), false);
    assert!(matching_pull_request(&case_changed, &project.pull_requests).is_none());
    let mut detached = attached.clone();
    detached.detached = true;
    assert!(matching_pull_request(&detached, &project.pull_requests).is_none());
    assert!(matching_pull_request(&attached, &[]).is_none());
}

#[test]
fn fork_branch_collisions_and_missing_identity_never_supply_a_title() {
    let mut project = project();
    let mut row = worktree(7, "fix-sidebar", false);
    let mut foreign = project.pull_requests[0].clone();
    foreign.head_repository.as_mut().unwrap().owner = "another-fork".into();
    assert!(matching_pull_request(&row, &[foreign.clone()]).is_none());
    assert!(matching_pull_request(&row, &project.pull_requests).is_some());
    project.pull_requests[0].head_repository = None;
    assert!(matching_pull_request(&row, &project.pull_requests).is_none());
    // Rebuilding from refreshed data must drop an old incorrect title.
    row.set_pull_request(foreign.clone());
    let rebuilt = SidebarProject::new(
        project.id,
        project.repository_id,
        project.name,
        project.repository,
        vec![row],
        vec![foreign],
    );
    assert!(rebuilt.worktrees[0].pull_request().is_none());
}

#[test]
fn a_unique_open_pr_survives_same_repository_closed_branch_history() {
    let project = project();
    let row = worktree(7, "fix-sidebar", false);
    let mut open = project.pull_requests[0].clone();
    open.state = Some(WorktreePullRequestState::Open);
    let mut merged = open.clone();
    merged.number += 1;
    merged.state = Some(WorktreePullRequestState::Merged);
    assert_eq!(
        matching_pull_request(&row, &[merged, open.clone()])
            .unwrap()
            .number,
        open.number
    );
    let mut other = open.clone();
    other.number += 2;
    assert!(matching_pull_request(&row, &[other, open]).is_none());
}

#[test]
fn sidebar_width_uses_compact_rail_and_clamped_persisted_expansion() {
    assert_eq!(effective_width(SidebarMode::Compact, 320.0), COMPACT_WIDTH);
    assert_eq!(
        effective_width(SidebarMode::Expanded, DEFAULT_EXPANDED_WIDTH),
        DEFAULT_EXPANDED_WIDTH
    );
    assert_eq!(
        effective_width(SidebarMode::Expanded, EXPANDED_WIDTH_MIN - 120.0),
        EXPANDED_WIDTH_MIN
    );
    assert_eq!(
        effective_width(SidebarMode::Expanded, EXPANDED_WIDTH_MAX + 120.0),
        EXPANDED_WIDTH_MAX
    );
    assert_eq!(
        effective_width(SidebarMode::Expanded, f32::NAN),
        DEFAULT_EXPANDED_WIDTH
    );
}

#[test]
fn primary_worktree_matches_repository_root_or_git_directory() {
    let root = Path::new("/repo/cagents");
    assert!(is_primary_worktree(root, root));
    assert!(is_primary_worktree(&root.join(".git"), root));
    assert!(!is_primary_worktree(
        &root.join(".git"),
        Path::new("/repo/cagents-worktree")
    ));
}

#[test]
fn the_resize_handle_is_a_named_focusable_splitter() {
    use iced_accessibility::accesskit::{Action, Role};

    let handle: Element<'_, SidebarAction> = resize_handle(DEFAULT_EXPANDED_WIDTH, false).into();
    assert_eq!(
        handle.as_widget().id().map(|id| id.to_string()),
        Some(resize_handle_id().to_string()),
        "a mouse-only resize is an accessibility regression: the handle must be a focus target"
    );

    let tree = Tree::new(&handle);
    let bounds = Size::new(crate::splitter::WIDTH, 480.0);
    let node = layout::Node::with_children(bounds, vec![layout::Node::new(bounds)]);
    let accessible = handle.as_widget().a11y_nodes(
        Layout::new(&node),
        &tree,
        cosmic::iced::advanced::mouse::Cursor::Unavailable,
    );

    let root = accessible.root();
    assert_eq!(root.len(), 1);
    let node = root[0].node();
    assert_eq!(node.role(), Role::Splitter);
    assert_eq!(node.label(), Some(RESIZE_HANDLE_NAME));
    assert_eq!(
        node.numeric_value(),
        Some(f64::from(DEFAULT_EXPANDED_WIDTH))
    );
    assert_eq!(
        node.min_numeric_value(),
        Some(f64::from(EXPANDED_WIDTH_MIN))
    );
    assert_eq!(
        node.max_numeric_value(),
        Some(f64::from(EXPANDED_WIDTH_MAX))
    );
    assert_eq!(
        node.numeric_value_step(),
        Some(f64::from(crate::splitter::KEYBOARD_STEP))
    );
    for action in [Action::Focus, Action::Increment, Action::Decrement] {
        assert!(
            node.supports_action(action),
            "the handle must expose {action:?} to assistive technologies"
        );
    }
}

/// libcosmic labels a button from `name` alone; its visible text is not a
/// fallback, so an unnamed button reaches a screen reader as "button".
#[test]
fn every_pane_state_offers_its_action_under_an_accessible_name() {
    /// The accessible names of the button nodes only: the surrounding
    /// text contributes labels of its own, which are no substitute.
    fn button_labels(mut element: Element<'_, SidebarAction>) -> Vec<String> {
        let renderer = headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        let root = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(240.0, 320.0)),
        );
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&root),
            &tree,
            cosmic::iced::advanced::mouse::Cursor::Unavailable,
        );
        accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
            .filter_map(|node| node.node().label().map(str::to_owned))
            .collect()
    }

    assert!(
        button_labels(error("disk on fire", false, false))
            .iter()
            .any(|label| label == "Try again"),
        "the error state's retry button must carry its own accessible name"
    );

    for (query, filter, expected) in [
        (String::new(), SidebarFilter::All, "Open folder"),
        (String::new(), SidebarFilter::Attention, "Show all"),
        ("nothing".to_owned(), SidebarFilter::All, "Clear search"),
    ] {
        let mut state = SidebarState::default();
        state.apply(&SidebarAction::SearchChanged(query));
        state.apply(&SidebarAction::SetFilter(filter));
        let empty = empty(&state, false, false);
        assert!(
            button_labels(empty)
                .iter()
                .any(|label| label == expected || label.starts_with(&format!("{expected} · "))),
            "the empty state's action must be named {expected:?}"
        );
    }
}

#[test]
fn opening_entities_updates_only_the_selected_navigation_identity() {
    let project = project_id(1);
    let worktree = worktree(2, "main", false);
    let mut state = SidebarState::default();
    state.apply(&SidebarAction::OpenRepository {
        project_id: project,
        repository: "/repo/cagents".into(),
    });
    assert_eq!(state.selection, Some(SidebarSelection::Project(project)));

    state.apply(&SidebarAction::OpenWorktree {
        project_id: project,
        worktree_id: worktree.id,
        path: worktree.path.clone(),
    });
    assert_eq!(
        state.selection,
        Some(SidebarSelection::Worktree {
            project_id: project,
            worktree_id: worktree.id,
        })
    );

    state.apply(&SidebarAction::OpenPullRequest {
        project_id: project,
        number: 42,
        url: Url::parse("https://github.com/example/cagents/pull/42").unwrap(),
    });
    assert_eq!(
        state.selection,
        Some(SidebarSelection::PullRequest {
            project_id: project,
            number: 42,
        })
    );
}

fn repo_path(value: &str) -> RepoPath {
    RepoPath::parse(value).expect("valid repository path")
}

fn ref_name(value: &str) -> ValidatedRefName {
    ValidatedRefName::parse(value).expect("valid ref name")
}

fn oid_of(seed: u8) -> Oid {
    Oid::parse(format!("{seed:02x}").repeat(20)).expect("valid object id")
}

fn main_key() -> ActiveWorktreeKey {
    ActiveWorktreeKey {
        project_id: project_id(1),
        worktree_id: WorktreeId::from_uuid(Uuid::from_u128(2)),
        canonical_path: PathBuf::from("/repo/main"),
    }
}

fn fix_key() -> ActiveWorktreeKey {
    ActiveWorktreeKey {
        project_id: project_id(1),
        worktree_id: WorktreeId::from_uuid(Uuid::from_u128(3)),
        canonical_path: PathBuf::from("/repo/fix-sidebar"),
    }
}

fn changed(
    path: RepoPath,
    area: ChangeArea,
    kind: ChangeKind,
    counts: Option<(u64, u64)>,
) -> ChangedPath {
    ChangedPath {
        path,
        old_path: None,
        area,
        kind,
        conflict: None,
        submodule: false,
        added_lines: counts.map(|(added, _)| added),
        removed_lines: counts.map(|(_, removed)| removed),
    }
}

fn panel_remote(name: &str, url: &str) -> RemoteRecord {
    RemoteRecord {
        name: name.into(),
        fetch_url: Some(url.into()),
        push_url: None,
        gh_resolved: None,
    }
}

fn panel_remote_ref(name: &str) -> RemoteRefRecord {
    let reference = ref_name(name);
    let rest = name.strip_prefix("refs/remotes/").unwrap_or(name);
    let (remote, branch) = rest.split_once('/').unwrap_or((rest, ""));
    RemoteRefRecord {
        name: reference,
        oid: oid_of(0xaa),
        remote: remote.into(),
        branch: branch.into(),
    }
}

/// A worktree on `main`, ahead of `origin/main`, with one entry in each of
/// the four change areas and one unmeasured (binary) count.
fn snapshot() -> RepositorySnapshot {
    RepositorySnapshot {
        status: StatusSnapshot {
            untracked_omitted: false,
            head: HeadState::Branch {
                name: "main".into(),
                oid: oid_of(0x11).to_string(),
            },
            tracking: TrackingState::Compared {
                upstream: "refs/remotes/origin/main".into(),
                ahead: 2,
                behind: 0,
            },
            dirty: DirtyState {
                staged: 1,
                unstaged: 1,
                untracked: 1,
                conflicted: 1,
                submodule: 0,
            },
            entries: vec![
                changed(
                    repo_path("src/app.rs"),
                    ChangeArea::Staged,
                    ChangeKind::Modified,
                    Some((12, 3)),
                ),
                changed(
                    repo_path("assets/logo.png"),
                    ChangeArea::Unstaged,
                    ChangeKind::Modified,
                    None,
                ),
                changed(
                    repo_path("notes.txt"),
                    ChangeArea::Untracked,
                    ChangeKind::Untracked,
                    None,
                ),
                ChangedPath {
                    conflict: Some(ConflictKind::BothModified),
                    ..changed(
                        repo_path("src/sidebar.rs"),
                        ChangeArea::Conflict,
                        ChangeKind::Unmerged,
                        None,
                    )
                },
            ],
        },
        files: WorkingFiles {
            paths: vec![
                repo_path("README.md"),
                repo_path("src/app.rs"),
                repo_path("src/sidebar.rs"),
            ],
            tracked_count: 3,
            ..WorkingFiles::default()
        },
        remotes: vec![panel_remote(
            "origin",
            "https://github.com/example/cagents.git",
        )],
        local_refs: vec![LocalRefRecord {
            name: ref_name("refs/heads/main"),
            oid: oid_of(0x11),
            upstream: Some(ref_name("refs/remotes/origin/main")),
            head: true,
        }],
        remote_refs: vec![panel_remote_ref("refs/remotes/origin/main")],
        remote_heads: vec![RemoteHead {
            remote: "origin".into(),
            target: ref_name("refs/remotes/origin/main"),
        }],
    }
}

fn source_control() -> SourceControlModel {
    SourceControlModel::ready(main_key(), &snapshot())
}

#[test]
fn ignored_files_and_folders_are_muted_without_affecting_change_review() {
    let mut snapshot = snapshot();
    snapshot.files.ignored = vec![repo_path("build/nested/output.log"), repo_path("debug.log")];
    snapshot.files.ignored_directories = vec![repo_path("build"), repo_path("empty")];
    snapshot.files.paths.push(repo_path("build/tracked.rs"));
    snapshot.files.paths.sort();
    snapshot
        .files
        .ignored_directory_errors
        .insert(repo_path("empty"), "Folder is not readable".into());
    let original_changes = snapshot.status.entries.clone();
    let model = SourceControlModel::ready(source_control().key, &snapshot);
    assert_eq!(
        model.ignored_directory_errors,
        snapshot.files.ignored_directory_errors
    );
    for name in [
        "build",
        "build/nested",
        "build/nested/output.log",
        "debug.log",
        "empty",
    ] {
        assert!(
            model
                .files
                .iter()
                .find(|row| row.path == repo_path(name))
                .unwrap()
                .ignored,
            "{name}"
        );
    }
    assert!(
        !model
            .files
            .iter()
            .find(|row| row.path == repo_path("build/tracked.rs"))
            .unwrap()
            .ignored
    );
    assert!(
        !model
            .files
            .iter()
            .find(|row| row.path == repo_path("README.md"))
            .unwrap()
            .ignored
    );
    assert_eq!(
        model
            .groups
            .iter()
            .map(|group| group.rows.len())
            .sum::<usize>(),
        original_changes.len()
    );
}

#[test]
fn change_pages_reach_every_file_and_bulk_commands_cover_the_entire_group() {
    let paths: Vec<_> = (0..501)
        .map(|index| repo_path(&format!("file-{index:04}.rs")))
        .collect();
    let tree = file_rows(&paths);
    let mut visited = Vec::new();
    for page in 0..3 {
        let (rows, total) = change_row_page(&tree, None, true, page);
        assert_eq!(total, 501);
        assert!(rows.len() <= MAX_VISIBLE_CHANGE_ROWS);
        visited.extend(rows.into_iter().map(|row| row.path.clone()));
    }
    assert_eq!(visited, paths);
    let mut source = source_control();
    let group = ChangeGroup::new(
        source.key.worktree_id,
        ChangeArea::Unstaged,
        paths
            .iter()
            .map(|path| {
                ChangeRow::new(
                    source.key.worktree_id,
                    &changed(
                        path.clone(),
                        ChangeArea::Unstaged,
                        ChangeKind::Modified,
                        None,
                    ),
                )
            })
            .collect(),
    );
    source.groups = vec![group];
    let command = group_commands(&source, &source.groups[0], false)
        .into_iter()
        .find_map(|command| command.action)
        .unwrap();
    let command = resolve_group_command(&source, &command).unwrap();
    let SidebarAction::Stage { selection, .. } = command else {
        panic!("expected stage-all");
    };
    assert_eq!(selection.len(), 501);
    assert_eq!(selection.last().unwrap().path, paths[500]);
}

#[test]
fn partial_status_does_not_claim_untracked_files_are_clean() {
    let mut worktree =
        SidebarWorktree::from_durable(WorktreeId::new(), "/repo".into(), Some("main".into()));
    let mut status = StatusSnapshot {
        untracked_omitted: true,
        ..StatusSnapshot::default()
    };
    worktree.apply_status(&status);
    assert!(worktree.chrome.tooltip.contains("no tracked changes"));
    assert!(!worktree.chrome.tooltip.contains("clean"));
    status.dirty.unstaged = 1;
    worktree.apply_status(&status);
    assert_eq!(
        worktree.chrome.dirty_badge.as_deref(),
        Some("1 tracked change")
    );
    status.untracked_omitted = false;
    worktree.apply_status(&status);
    assert_eq!(worktree.chrome.dirty_badge.as_deref(), Some("1 change"));
}

#[test]
fn worktree_change_badge_clears_after_clean_status_read() {
    let mut worktree =
        SidebarWorktree::from_durable(WorktreeId::new(), "/repo".into(), Some("main".into()));
    let mut status = StatusSnapshot::default();
    status.dirty.unstaged = 1;
    worktree.apply_status(&status);
    assert!(worktree.chrome.dirty_badge.is_some());
    worktree.apply_status(&StatusSnapshot::default());
    assert!(worktree.chrome.dirty_badge.is_none());
    status.dirty.unstaged = 0;
    status.dirty.untracked = 1;
    worktree.apply_status(&status);
    assert!(worktree.chrome.dirty_badge.is_some());
    worktree.apply_status(&StatusSnapshot::default());
    assert!(worktree.chrome.dirty_badge.is_none());
}

#[test]
fn branch_only_changes_keep_historical_paths_and_accent() {
    let mut source = SourceControlModel::ready(
        main_key(),
        &RepositorySnapshot {
            status: StatusSnapshot::default(),
            files: WorkingFiles::default(),
            remotes: Vec::new(),
            local_refs: Vec::new(),
            remote_refs: Vec::new(),
            remote_heads: Vec::new(),
        },
    );
    assert!(!source.has_changes());
    let comparison = BranchComparison {
        base_ref: ref_name("refs/heads/trunk"),
        base_oid: oid_of(0x22),
        head: oid_of(0x11),
        merge_base: oid_of(0x22),
        entries: vec![
            crate::repository::DiffEntry {
                path: repo_path("deleted.txt"),
                old_path: None,
                kind: ChangeKind::Deleted,
                added_lines: Some(0),
                removed_lines: Some(4),
            },
            crate::repository::DiffEntry {
                path: repo_path("new name.txt"),
                old_path: Some(repo_path("old name.txt")),
                kind: ChangeKind::Renamed,
                added_lines: Some(0),
                removed_lines: Some(0),
            },
        ],
    };
    let branch = Arc::new(BranchChangesModel::new(comparison));
    assert_eq!(
        branch.rows[1].entry.old_path,
        Some(repo_path("old name.txt"))
    );
    assert!(
        branch
            .tree
            .iter()
            .any(|file| file.path == repo_path("deleted.txt"))
    );
    source.branch_changes = BranchChanges::Ready(branch);
    assert!(source.has_changes());
}

#[test]
fn change_folders_fold_independently_and_survive_view_restore() {
    let source = source_control();
    let mut state = SidebarState::default();
    let scope = ChangeTreeScope::Area(ChangeArea::Unstaged);
    let paths = |state: &SidebarState, scope, flat| {
        visible_change_rows(
            &source.files,
            state.collapsed_change_directories.get(&scope),
            flat,
        )
        .0
        .iter()
        .map(|row| row.path.display().into_owned())
        .collect::<Vec<_>>()
    };
    assert_eq!(
        paths(&state, scope, false),
        ["src", "src/app.rs", "src/sidebar.rs", "README.md"]
    );
    state.apply(&SidebarAction::ToggleChangeDirectory {
        key: main_key(),
        scope,
        path: repo_path("src"),
    });
    assert_eq!(paths(&state, scope, false), ["src", "README.md"]);
    assert_eq!(
        paths(&state, ChangeTreeScope::Branch, false),
        ["src", "src/app.rs", "src/sidebar.rs", "README.md"]
    );
    assert_eq!(
        paths(&state, scope, true),
        ["src/app.rs", "src/sidebar.rs", "README.md"]
    );
    let saved = state.saved_view();
    let serialized = serde_json::to_string(&saved).unwrap();
    let mut restored = SidebarState::default();
    restored.restore_view(serde_json::from_str(&serialized).unwrap());
    assert_eq!(paths(&restored, scope, false), ["src", "README.md"]);
}

#[test]
fn file_pages_reach_later_siblings_without_mounting_the_entire_tree() {
    let paths = (0..450)
        .map(|i| repo_path(&format!("early/file-{i:03}.txt")))
        .chain(std::iter::once(repo_path("later/file.txt")))
        .collect::<Vec<_>>();
    let rows = file_rows(&paths);
    let expanded = BTreeSet::from([repo_path("early")]);
    let (first, total) = file_row_page(&rows, &expanded, "", false, false, 0);
    assert_eq!(first.len(), MAX_VISIBLE_FILE_ROWS);
    assert_eq!(total, 452);
    let (second, _) = file_row_page(&rows, &expanded, "", false, false, 1);
    assert_eq!(second.len(), 52);
    assert_eq!(second.last().unwrap().path, repo_path("later"));
    let (clamped, total) = file_row_page(&rows, &BTreeSet::new(), "", false, false, 1);
    assert_eq!(total, 2);
    assert_eq!(clamped.len(), 2);
    let top_level = file_rows(
        &(0..450)
            .map(|i| repo_path(&format!("file-{i:03}.txt")))
            .collect::<Vec<_>>(),
    );
    let (last, total) = file_row_page(&top_level, &BTreeSet::new(), "", false, false, 1);
    assert_eq!(total, 450);
    assert_eq!(last.len(), 50);
    assert_eq!(last.last().unwrap().path, repo_path("file-449.txt"));
}

#[test]
fn tree_starts_collapsed_and_flat_ignores_expansion() {
    let source = source_control();
    let mut state = SidebarState::default();
    let paths = |state: &SidebarState, flat, query: &str| {
        visible_file_rows(
            &source.files,
            &state.expanded_directories,
            query,
            flat,
            false,
        )
        .0
        .iter()
        .map(|row| row.path.display().into_owned())
        .collect::<Vec<_>>()
    };
    // A fresh tree shows only top-level rows; loading the whole project
    // tree stays cheap until the user opens folders.
    assert_eq!(paths(&state, false, ""), ["src", "README.md"]);
    state.apply(&SidebarAction::ToggleDirectory {
        key: main_key(),
        path: repo_path("src"),
    });
    assert_eq!(
        paths(&state, false, ""),
        ["src", "src/app.rs", "src/sidebar.rs", "README.md"]
    );
    // Flat mode and filters ignore expansion without losing tree state.
    assert_eq!(
        paths(&state, true, ""),
        ["src/app.rs", "src/sidebar.rs", "README.md"]
    );
    assert_eq!(paths(&state, true, "sidebar"), ["src/sidebar.rs"]);
    assert_eq!(
        paths(&state, false, ""),
        ["src", "src/app.rs", "src/sidebar.rs", "README.md"]
    );
    // Diff and change review always expand, independent of the file tree.
    let (expanded, _) = visible_file_rows(
        &source.files,
        &SidebarState::default().expanded_directories,
        "",
        false,
        true,
    );
    assert_eq!(
        expanded
            .iter()
            .map(|row| row.path.display().into_owned())
            .collect::<Vec<_>>(),
        ["src", "src/app.rs", "src/sidebar.rs", "README.md"]
    );
}

#[test]
fn expanded_folders_round_trip_through_display_paths() {
    let mut state = SidebarState::default();
    state.restore_expanded_display_paths(&[
        "src".into(),
        "src/nested".into(),
        "/absolute".into(),
        "..".into(),
        String::new(),
    ]);
    // Only relative, well-formed folders survive: the tree renders,
    // just collapsed, for anything the session could not name.
    assert_eq!(
        state.expanded_display_paths(),
        ["src".to_owned(), "src/nested".to_owned()]
    );
}

fn panel_worktree(key: &ActiveWorktreeKey, branch: &str) -> SidebarWorktree {
    SidebarWorktree::from_repository(
        &WorktreeRecord {
            id: key.worktree_id,
            path: key.canonical_path.clone(),
            created_at_unix_ms: None,
            head_oid: None,
            branch: Some(format!("refs/heads/{branch}")),
            detached: false,
            bare: false,
            lock: WorktreeLock::default(),
            prunable_reason: None,
        },
        None,
    )
}

/// A project holding both fixture worktrees, so the panel has somewhere to
/// bind and somewhere to rebind to.
pub(crate) fn panel_model() -> SidebarModel {
    SidebarModel::ready(
        SidebarMode::Expanded,
        vec![SidebarProject::new(
            project_id(1),
            RepositoryId(Uuid::from_u128(9)),
            "cagents".into(),
            "/repo/cagents".into(),
            vec![
                panel_worktree(&main_key(), "main"),
                panel_worktree(&fix_key(), "fix-sidebar"),
            ],
            Vec::new(),
        )],
    )
    .with_source_control(Some(source_control()))
}

fn focus_on(key: &ActiveWorktreeKey) -> SidebarFocus {
    SidebarFocus::Repository {
        worktree: key.canonical_path.clone(),
    }
}

/// A pane bound to `main` with every panel section open.
pub(crate) fn panel_state(model: &SidebarModel) -> SidebarState {
    let mut state = SidebarState::default();
    assert!(state.sync_active_worktree(model, &focus_on(&main_key())));
    for section in PanelSection::ALL {
        if !state.section_expanded(section) {
            state.apply(&SidebarAction::ToggleSection {
                key: main_key(),
                section,
            });
        }
    }
    state
}

/// A headless `cosmic::Renderer`. `Widget::update` and `Widget::operate`
/// take one by reference; with no graphics device it falls back to
/// `tiny-skia`, which needs neither a device nor a surface.
pub(super) fn headless_renderer() -> cosmic::Renderer {
    use cosmic::iced::advanced::renderer::Headless;
    use cosmic::iced::{Font, Pixels};

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("executor")
        .block_on(cosmic::Renderer::new(
            Font::default(),
            Pixels(14.0),
            Some("tiny-skia"),
        ))
        .expect("a headless renderer")
}

/// Everything a frame publishes when each of its focusable controls is
/// focused in turn and answered with a bare `Enter`.
///
/// This is the path a dialog leaves open. The overlay captures pointer and
/// touch, but the pane keeps whatever focus it held when the dialog opened
/// and libcosmic's modal popover declines to deliver a focus operation to
/// the content beneath it, so a focused control keeps answering the
/// keyboard. Focus by id exercises the same commands as keyboard traversal.
fn frame_enter_presses(
    renderer: &cosmic::Renderer,
    element: Element<'_, SidebarAction>,
) -> Vec<SidebarAction> {
    frame_enter_presses_at_width(renderer, element, 1280.0)
}

fn frame_enter_presses_at_width(
    renderer: &cosmic::Renderer,
    mut element: Element<'_, SidebarAction>,
    width: f32,
) -> Vec<SidebarAction> {
    use cosmic::iced::advanced::{Shell, clipboard, mouse, widget::operation};
    use cosmic::iced::{Event, Rectangle, keyboard};

    fn identities(tree: &Tree, into: &mut Vec<Id>) {
        into.extend(tree.id.clone());
        for child in &tree.children {
            identities(child, into);
        }
    }

    let mut tree = Tree::new(element.as_widget());
    let viewport = Rectangle::with_size(Size::new(width, 4_000.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let enter = Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(keyboard::key::Named::Enter),
        modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: None,
        repeat: false,
    });
    let mut identifiers = Vec::new();
    identities(&tree, &mut identifiers);
    let mut fired = Vec::new();
    for id in identifiers {
        let mut focus = operation::focusable::focus::<()>(id);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), renderer, &mut focus);
        let mut published = Vec::new();
        let mut shell = Shell::new(&mut published);
        element.as_widget_mut().update(
            &mut tree,
            &enter,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            renderer,
            &mut clipboard::Null,
            &mut shell,
            &viewport,
        );
        drop(shell);
        fired.extend(published);
    }
    fired
}

#[test]
fn the_panel_mounts_for_the_active_worktree_and_rebinds_rather_than_leaking_the_previous_one() {
    let model = panel_model();
    let mut state = SidebarState::default();

    assert!(
        bound_source_control(&model, &state).is_none(),
        "with nothing focused the panel must not mount: there is no worktree it could be \
             reading"
    );

    assert!(
        state.sync_active_worktree(&model, &focus_on(&main_key())),
        "focusing a known worktree must bind the panel to it"
    );
    assert_eq!(state.active_worktree(), Some(&main_key()));
    assert!(
        bound_source_control(&model, &state).is_some(),
        "the panel mounts once the bound worktree is the one the model holds"
    );

    // Focus moves to the sibling worktree. The model still holds `main`'s
    // rows, and those rows may not be shown under `fix-sidebar`'s name: a
    // press on one would stage a path in the wrong tree.
    assert!(state.sync_active_worktree(&model, &focus_on(&fix_key())));
    assert_eq!(state.active_worktree(), Some(&fix_key()));
    assert!(
        bound_source_control(&model, &state).is_none(),
        "a rebind must withhold the panel until the new worktree's own snapshot lands"
    );

    // A card that names no worktree - a browser, a pull-request list -
    // leaves the binding where it is rather than tearing the panel down.
    assert!(
        !state.sync_active_worktree(&model, &SidebarFocus::None),
        "focus that names no directory is not a rebind"
    );
    assert_eq!(state.active_worktree(), Some(&fix_key()));

    // A worktree the project list no longer holds cannot stay bound.
    let emptied = SidebarModel::ready(SidebarMode::Expanded, Vec::new());
    assert!(emptied.source_control.is_none());
    assert!(state.sync_active_worktree(&emptied, &SidebarFocus::None));
    assert_eq!(state.active_worktree(), None);
    assert!(bound_source_control(&emptied, &state).is_none());
}

#[test]
fn a_pinned_panel_worktree_binds_the_panel_without_moving_the_list() {
    let model = panel_model();
    let mut state = SidebarState::default();
    assert!(state.sync_active_worktree(&model, &focus_on(&main_key())));
    let selected = state.selection;
    let drawn = state.drawn_selection();

    state.pin_panel_worktree(Some(fix_key().canonical_path));
    assert!(state.sync_active_worktree(&model, &focus_on(&main_key())));
    assert_eq!(state.active_worktree(), Some(&fix_key()));
    // The list still draws, and acts on, what focus names.
    assert_eq!(state.selection, selected);
    assert_eq!(state.drawn_selection(), drawn);
    assert_eq!(
        selected_project(&model, &state).map(|project| project.id),
        Some(main_key().project_id)
    );

    // Focus moving keeps the pinned panel; the list follows focus.
    assert!(!state.sync_active_worktree(&model, &focus_on(&fix_key())));
    assert_eq!(
        state.drawn_selection(),
        Some(SidebarSelection::Worktree {
            project_id: fix_key().project_id,
            worktree_id: fix_key().worktree_id,
        })
    );

    // Unpinned, the panel follows focus again.
    state.pin_panel_worktree(None);
    assert!(state.sync_active_worktree(&model, &focus_on(&main_key())));
    assert_eq!(state.active_worktree(), Some(&main_key()));

    // A pin to a worktree no project lists falls back to focus, not to
    // the worktree bound before.
    state.pin_panel_worktree(Some(PathBuf::from("/nowhere")));
    assert!(!state.sync_active_worktree(&model, &focus_on(&main_key())));
    assert_eq!(state.active_worktree(), Some(&main_key()));
}

#[test]
fn separate_commit_fields_preserve_description_and_clear_together() {
    let mut state = SidebarState::default();
    let key = main_key();
    state.apply(&SidebarAction::CommitMessageChanged {
        key: key.clone(),
        message: "Original subject\n\nDescription paragraph\nAnother line".into(),
    });
    assert_eq!(state.commit_draft.subject, "Original subject");
    let description = state.commit_draft.composer.0.text();
    assert!(description.starts_with("Description paragraph\nAnother line"));
    state.apply(&SidebarAction::CommitSubjectEditorAction {
        key: key.clone(),
        action: cosmic::iced::widget::text_editor::Action::SelectAll,
    });
    state.apply(&SidebarAction::CommitSubjectEditorAction {
        key: key.clone(),
        action: cosmic::iced::widget::text_editor::Action::Edit(
            cosmic::iced::widget::text_editor::Edit::Paste(std::sync::Arc::new(
                "Revised subject".into(),
            )),
        ),
    });
    assert_eq!(state.commit_draft.composer.0.text(), description);
    assert_eq!(
        state.commit_message(),
        compose_commit_message("Revised subject", &description)
    );
    state.apply(&SidebarAction::CommitEditorAction {
        key,
        action: cosmic::iced::widget::text_editor::Action::Edit(
            cosmic::iced::widget::text_editor::Edit::Paste(std::sync::Arc::new(
                "Added detail ".into(),
            )),
        ),
    });
    assert!(state.commit_message().starts_with("Revised subject\n\n"));
    assert!(state.commit_message().contains("Added detail "));
    state.clear_commit_message();
    assert!(state.commit_draft.subject.is_empty());
    assert!(state.commit_message().is_empty());
    assert!(state.commit_draft.composer.0.text().trim().is_empty());
    assert_eq!(compose_commit_message("Subject only", "\n"), "Subject only");
    state.apply(&SidebarAction::CommitSubjectEditorAction {
        key: main_key(),
        action: cosmic::iced::widget::text_editor::Action::Edit(
            cosmic::iced::widget::text_editor::Edit::Paste(std::sync::Arc::new(
                "First line\nSecond line\nThird line".into(),
            )),
        ),
    });
    assert_eq!(
        state.commit_draft.subject,
        "First line Second line Third line"
    );
    assert!(
        !state
            .commit_draft
            .subject_composer
            .0
            .text()
            .trim_end_matches('\n')
            .contains('\n')
    );
    assert_eq!(state.commit_message(), "First line Second line Third line");
}

#[test]
fn rebinding_scopes_panel_drafts_and_restores_the_worktrees_commit_draft() {
    let model = panel_model();
    let mut state = SidebarState::default();
    assert!(state.sync_active_worktree(&model, &focus_on(&main_key())));

    let key = main_key();
    state.apply(&SidebarAction::FilesQueryChanged {
        key: key.clone(),
        query: "sidebar".into(),
    });
    state.apply(&SidebarAction::CommitMessageChanged {
        key: key.clone(),
        message: "wip".into(),
    });
    state.apply(&SidebarAction::ToggleDirectory {
        key: key.clone(),
        path: repo_path("src"),
    });
    state.apply(&SidebarAction::RequestDiscard {
        key: key.clone(),
        request: DiscardRequest {
            worktree: key.canonical_path.clone(),
            tracked: vec![PathSelection::from_changed_path(&changed(
                repo_path("src/app.rs"),
                ChangeArea::Unstaged,
                ChangeKind::Modified,
                None,
            ))],
            untracked: Vec::new(),
            confirmed: false,
        },
    });
    assert_eq!(state.files_query(), "sidebar");
    assert_eq!(state.commit_message(), "wip");
    assert!(state.pending_discard().is_some());

    assert!(state.sync_active_worktree(&model, &focus_on(&fix_key())));
    assert_eq!(
        state.files_query(),
        "",
        "a filter typed against one worktree's file tree cannot survive onto another's"
    );
    assert_eq!(
        state.commit_message(),
        "",
        "a commit message is written for the tree it will be committed to"
    );
    assert!(state.expanded_directories.is_empty());
    assert!(
        state.pending_discard().is_none(),
        "a confirmation counted against the worktree that was left must not be answerable"
    );
    state.apply(&SidebarAction::CommitMessageChanged {
        key: fix_key(),
        message: "Sibling subject\n\nSibling body".into(),
    });
    assert!(state.sync_active_worktree(&model, &focus_on(&main_key())));
    assert_eq!(
        state.commit_message(),
        "wip",
        "returning to the owner restores its input"
    );
    assert!(state.sync_active_worktree(&model, &focus_on(&fix_key())));
    assert_eq!(state.commit_message(), "Sibling subject\n\nSibling body");
}

#[test]
fn submitted_commit_clears_only_its_revision_even_while_parked() {
    let model = panel_model();
    let mut state = panel_state(&model);
    let key = main_key();
    state.apply(&SidebarAction::CommitMessageChanged {
        key: key.clone(),
        message: "First subject\n\nFirst body".into(),
    });
    state.apply(&SidebarAction::AmendChanged {
        key: key.clone(),
        amend: true,
    });
    let submitted = state
        .submitted_commit_revision(&key, state.commit_message(), true)
        .unwrap();
    assert_eq!(
        state.submitted_commit_revision(&key, "Stale frame", true),
        None
    );
    state.sync_active_worktree(&model, &focus_on(&fix_key()));
    state.apply(&SidebarAction::CommitMessageChanged {
        key: fix_key(),
        message: "Other draft".into(),
    });
    state.clear_submitted_commit(&key, submitted);
    assert_eq!(state.commit_message(), "Other draft");
    state.sync_active_worktree(&model, &focus_on(&key));
    assert!(state.commit_message().is_empty());
    assert!(!state.commit_draft.amend);
    state.apply(&SidebarAction::CommitMessageChanged {
        key: key.clone(),
        message: "New subject\n\nNew body".into(),
    });
    let next = state
        .submitted_commit_revision(&key, state.commit_message(), false)
        .unwrap();
    state.apply(&SidebarAction::CommitEditorAction {
        key: key.clone(),
        action: cosmic::iced::widget::text_editor::Action::Edit(
            cosmic::iced::widget::text_editor::Edit::Paste(Arc::new("Later edit ".into())),
        ),
    });
    let edited = state.commit_message().to_owned();
    state.sync_active_worktree(&model, &focus_on(&fix_key()));
    state.clear_submitted_commit(&key, next);
    state.sync_active_worktree(&model, &focus_on(&key));
    assert_eq!(
        state.commit_message(),
        edited,
        "later edits survive the older commit's completion"
    );
    // Even replacing text with the same text is a newer editing intent.
    state.apply(&SidebarAction::CommitMessageChanged {
        key: key.clone(),
        message: "New subject\n\nNew body".into(),
    });
    state.clear_submitted_commit(&key, next);
    assert_eq!(state.commit_message(), "New subject\n\nNew body");
}

#[test]
fn every_panel_section_expands_and_collapses() {
    let key = main_key();
    let mut state = SidebarState::default();

    for section in PanelSection::ALL {
        let before = state.section_expanded(section);
        state.apply(&SidebarAction::ToggleSection {
            key: key.clone(),
            section,
        });
        assert_eq!(
            state.section_expanded(section),
            !before,
            "{section:?} must answer its own toggle"
        );
        for other in PanelSection::ALL
            .into_iter()
            .filter(|other| *other != section)
        {
            assert_eq!(
                state.section_expanded(other),
                other.expanded_by_default(),
                "toggling {section:?} moved {other:?}"
            );
        }
        state.apply(&SidebarAction::ToggleSection {
            key: key.clone(),
            section,
        });
        assert_eq!(
            state.section_expanded(section),
            before,
            "{section:?} must return to where it was"
        );
    }
}

/// A path Git holds that Rust cannot decode.
///
/// Every mutation this panel raises names its paths by these bytes, so a
/// lossy label is the only thing a decode may ever produce. A `String`
/// anywhere on that route would either refuse the row or stage a different
/// file.
fn non_utf8_path() -> RepoPath {
    RepoPath::from_bytes(b"src/caf\xff.rs").expect("a repository path Git can hold")
}

#[test]
fn a_change_row_stages_unstages_and_discards_exactly_its_own_path() {
    let mut snapshot = snapshot();
    snapshot.status.entries = vec![
        changed(
            repo_path("src/app.rs"),
            ChangeArea::Staged,
            ChangeKind::Modified,
            Some((12, 3)),
        ),
        changed(
            non_utf8_path(),
            ChangeArea::Unstaged,
            ChangeKind::Modified,
            None,
        ),
        changed(
            repo_path("notes.txt"),
            ChangeArea::Untracked,
            ChangeKind::Untracked,
            None,
        ),
    ];
    let source = SourceControlModel::ready(main_key(), &snapshot);
    let key = main_key();

    let staged = source
        .group(ChangeArea::Staged)
        .expect("a staged group exists");
    let row = &staged.rows()[0];
    assert_eq!(
        row_commands(&source, ChangeArea::Staged, row, false)
            .into_iter()
            .map(|command| command.action)
            .collect::<Vec<_>>(),
        vec![Some(SidebarAction::Unstage {
            key: key.clone(),
            selection: vec![row.selection()],
        })],
        "a staged row offers unstage, and never a discard: there is nothing in the worktree \
             to throw away"
    );

    // The unstaged row's path cannot be decoded, and every command it
    // raises must still name it byte for byte.
    let unstaged = source
        .group(ChangeArea::Unstaged)
        .expect("an unstaged group exists");
    let row = &unstaged.rows()[0];
    assert_eq!(row.entry.path, non_utf8_path());
    let actions = row_commands(&source, ChangeArea::Unstaged, row, false)
        .into_iter()
        .map(|command| command.action)
        .collect::<Vec<_>>();
    assert_eq!(
        actions,
        vec![
            Some(SidebarAction::Stage {
                key: key.clone(),
                selection: vec![row.selection()],
            }),
            Some(SidebarAction::RequestDiscard {
                key: key.clone(),
                request: DiscardRequest {
                    worktree: key.canonical_path.clone(),
                    tracked: vec![row.selection()],
                    untracked: Vec::new(),
                    confirmed: false,
                },
            }),
        ]
    );

    // An untracked path exists nowhere but the worktree, so discarding it
    // deletes a file rather than restoring one from the index.
    let untracked = source
        .group(ChangeArea::Untracked)
        .expect("an untracked group exists");
    let row = &untracked.rows()[0];
    assert_eq!(
        row_commands(&source, ChangeArea::Untracked, row, false)
            .into_iter()
            .filter_map(|command| command.action)
            .find(|action| matches!(action, SidebarAction::RequestDiscard { .. })),
        Some(SidebarAction::RequestDiscard {
            key: key.clone(),
            request: DiscardRequest {
                worktree: key.canonical_path.clone(),
                tracked: Vec::new(),
                untracked: vec![repo_path("notes.txt")],
                confirmed: false,
            },
        })
    );

    // The group command acts on exactly the rows the group drew.
    assert_eq!(
        group_commands(&source, unstaged, false)
            .into_iter()
            .next()
            .and_then(|command| command.action)
            .and_then(|action| resolve_group_command(&source, &action)),
        Some(SidebarAction::Stage {
            key,
            selection: unstaged.visible_selection(),
        })
    );
}

#[test]
fn a_running_mutation_withholds_every_further_command_against_the_same_index() {
    let mut source = source_control();
    source.set_mutation(Some(MutationKind::Stage));
    let group = source
        .group(ChangeArea::Unstaged)
        .expect("an unstaged group exists");
    assert!(
        group_commands(&source, group, false)
            .iter()
            .all(|command| command.action.is_none())
            && row_commands(&source, ChangeArea::Unstaged, &group.rows()[0], false)
                .iter()
                .all(|command| command.action.is_none()),
        "two `git add` runs against one index corrupt each other's lock, so the second is not \
             enabled"
    );
}

#[test]
fn a_discard_is_raised_unconfirmed_and_only_the_dialog_can_confirm_it() {
    let source = source_control();
    let key = main_key();
    let group = source
        .group(ChangeArea::Unstaged)
        .expect("an unstaged group exists");
    let request = group_commands(&source, group, false)
        .into_iter()
        .filter_map(|command| command.action)
        .find_map(|action| match resolve_group_command(&source, &action) {
            Some(SidebarAction::RequestDiscard { request, .. }) => Some(request),
            _ => None,
        })
        .expect("the unstaged group offers a discard");
    assert!(
        !request.confirmed,
        "the surface may never build a confirmed discard: the service refuses an unconfirmed \
             one, and that refusal is the guard"
    );

    let mut state = SidebarState::default();
    state.apply(&SidebarAction::RequestDiscard {
        key: key.clone(),
        request: request.clone(),
    });
    let pending = state.pending_discard().expect("the dialog is now mounted");
    let SidebarAction::ConfirmDiscard {
        key: confirmed_key,
        request: confirmed,
    } = pending.confirm_action()
    else {
        panic!("the dialog's answer is the confirmed discard");
    };
    assert_eq!(confirmed_key, key);
    assert!(confirmed.confirmed);
    assert_eq!(
        (&confirmed.tracked, &confirmed.untracked),
        (&request.tracked, &request.untracked),
        "the confirmed request must name exactly the paths the dialog counted"
    );

    state.apply(&SidebarAction::CancelDiscard);
    assert!(state.pending_discard().is_none());
}

#[test]
fn sidebar_worktree_and_tab_headers_share_the_same_separator_baseline() {
    use cosmic::iced::Point;
    let renderer = headless_renderer();
    let model = panel_model();
    let state = panel_state(&model);
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let width = 380.0;
    let tabs = widget::column::with_children(vec![
        crate::tab_strip::horizontal(
            widget::Id::new("header.geometry.tabs"),
            vec![],
            design::PRIMARY_CONTROL_SIZE,
            design::HEADER_HEIGHT,
        ),
        design::horizontal_rule(),
    ])
    .into();
    let surfaces = [
        view_inner(
            &model,
            &state,
            SidebarMode::Expanded,
            width,
            false,
            None,
            &shortcuts,
            false,
        ),
        file_panel(&model, &state, width, false),
        git_panel(&model, &state, width, false),
        tabs,
    ];
    fn first_rule(node: &layout::Node, parent: Point, width: f32) -> Option<f32> {
        let bounds = node.bounds();
        let origin = Point::new(parent.x + bounds.x, parent.y + bounds.y);
        let own = (node.children().is_empty()
            && (bounds.width - width).abs() < 0.1
            && (bounds.height - design::BORDER_WIDTH).abs() < 0.1)
            .then_some(origin.y);
        node.children()
            .iter()
            .filter_map(|child| first_rule(child, origin, width))
            .chain(own)
            .min_by(f32::total_cmp)
    }
    for mut surface in surfaces {
        let mut tree = Tree::new(&surface);
        let node = surface.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 300.0)),
        );
        assert_eq!(
            first_rule(&node, Point::ORIGIN, width),
            Some(design::HEADER_CONTENT_HEIGHT)
        );
    }
}

#[test]
fn the_git_tab_strip_offers_every_view_it_is_not_already_showing() {
    let renderer = headless_renderer();
    let model = panel_model();
    let mut state = panel_state(&model);
    for tab in ChangesTab::ALL {
        state.changes_tab = tab;
        let published = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false));
        let offered: Vec<ChangesTab> = published
            .iter()
            .filter_map(|action| match action {
                SidebarAction::SetChangesTab { tab, .. } => Some(*tab),
                _ => None,
            })
            .collect();
        let expected: Vec<ChangesTab> = ChangesTab::ALL
            .into_iter()
            .filter(|it| *it != tab)
            .collect();
        assert_eq!(
            offered, expected,
            "the strip must offer the other two views and refuse the one already showing; \
                 from {tab:?} it published {offered:?}"
        );
    }
}

/// The standalone editor's sidebar has one row of navigation: the three
/// panels. It never also offers the Git views, which that host does not
/// load, and a view's option appears only on the view it arranges.
#[test]
fn the_standalone_sidebar_offers_one_level_of_navigation() {
    let renderer = headless_renderer();
    let model = panel_model();
    let state = panel_state(&model);
    for panel in WorktreePanel::ALL {
        let published = frame_enter_presses(
            &renderer,
            worktree_sidebar(&model, &state, 380.0, false, panel, true),
        );
        let offered: Vec<WorktreePanel> = published
            .iter()
            .filter_map(|action| match action {
                SidebarAction::SetWorktreePanel { panel } => Some(*panel),
                _ => None,
            })
            .collect();
        let expected: Vec<WorktreePanel> = WorktreePanel::ALL
            .into_iter()
            .filter(|it| *it != panel)
            .collect();
        assert_eq!(
            offered, expected,
            "from {panel:?} it published {published:?}"
        );
        assert!(
            !published
                .iter()
                .any(|action| matches!(action, SidebarAction::SetChangesTab { .. })),
            "a second row of Git views is a second level of navigation; {panel:?} \
                 published {published:?}"
        );
        assert_eq!(
            published
                .iter()
                .any(|action| matches!(action, SidebarAction::ToggleFilesFilter { .. })),
            panel == WorktreePanel::Files,
            "only Files offers the file filter; {panel:?} published {published:?}"
        );
        assert_eq!(
            published
                .iter()
                .any(|action| matches!(action, SidebarAction::SetChangeListMode { .. })),
            panel == WorktreePanel::Changes,
            "only a list of changed files offers tree/list; {panel:?} published \
                 {published:?}"
        );
    }
}

/// A Git view the standalone editor never loads cannot be left selected
/// behind its back: its Changes panel is the working tree, whatever the
/// shared state last recorded.
#[test]
fn the_standalone_changes_panel_is_always_the_working_tree() {
    let renderer = headless_renderer();
    let model = panel_model();
    let mut state = panel_state(&model);
    for tab in ChangesTab::ALL {
        state.changes_tab = tab;
        let published = frame_enter_presses(
            &renderer,
            worktree_sidebar(&model, &state, 380.0, false, WorktreePanel::Changes, true),
        );
        assert!(
            published
                .iter()
                .any(|action| matches!(action, SidebarAction::Stage { .. })),
            "with {tab:?} recorded the panel must still draw the working tree; it \
                 published {published:?}"
        );
    }
}

/// A plain folder has only files, and one tab is not a choice. The host
/// says so up front, so the row never depends on whether the read has
/// landed yet, and a panel remembered from a Git tab falls back to Files.
#[test]
fn a_plain_folder_draws_no_panel_tabs() {
    let renderer = headless_renderer();
    let model = panel_model();
    let state = panel_state(&model);
    for panel in WorktreePanel::ALL {
        let published = frame_enter_presses(
            &renderer,
            worktree_sidebar(&model, &state, 380.0, false, panel, false),
        );
        assert!(
            !published
                .iter()
                .any(|action| matches!(action, SidebarAction::SetWorktreePanel { .. })),
            "{panel:?} published {published:?}"
        );
        assert!(
            published
                .iter()
                .any(|action| matches!(action, SidebarAction::ToggleFilesFilter { .. })),
            "{panel:?} must fall back to Files; it published {published:?}"
        );
    }
}

/// The tree/flat choice arranges changed files, so History, a list of
/// commits, does not offer it; and the one control names the arrangement
/// it switches to.
#[test]
fn the_list_mode_toggle_follows_the_view_it_arranges() {
    let renderer = headless_renderer();
    let model = panel_model();
    let mut state = panel_state(&model);
    for tab in ChangesTab::ALL {
        state.changes_tab = tab;
        let modes: Vec<FileListMode> =
            frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false))
                .into_iter()
                .filter_map(|action| match action {
                    SidebarAction::SetChangeListMode { mode, .. } => Some(mode),
                    _ => None,
                })
                .collect();
        let expected = if tab == ChangesTab::History {
            Vec::new()
        } else {
            vec![FileListMode::Flat]
        };
        assert_eq!(modes, expected, "{tab:?}");
    }
    state.changes_tab = ChangesTab::Changes;
    state.apply(&SidebarAction::SetChangeListMode {
        key: main_key(),
        mode: FileListMode::Flat,
    });
    let names = accessible_names(&renderer, git_panel(&model, &state, 380.0, false));
    assert!(names.iter().any(|name| name == "View as tree"), "{names:?}");
    assert!(
        !names.iter().any(|name| name == "View as list"),
        "{names:?}"
    );
}

/// Every tab is named in words; none is an unlabeled glyph.
#[test]
fn every_panel_tab_is_named() {
    let renderer = headless_renderer();
    let model = panel_model();
    let state = panel_state(&model);
    let names = accessible_names(&renderer, git_panel(&model, &state, 380.0, false));
    for tab in ChangesTab::ALL {
        assert!(
            names.iter().any(|name| name.starts_with(tab.description())),
            "{tab:?} is missing from {names:?}"
        );
    }
    // The count the tab draws is part of what it is called.
    assert!(
        names
            .iter()
            .any(|name| name == "Uncommitted changes in this worktree: 4"),
        "{names:?}"
    );
    let names = accessible_names(
        &renderer,
        worktree_sidebar(&model, &state, 380.0, false, WorktreePanel::Files, true),
    );
    for panel in WorktreePanel::ALL {
        assert!(
            names
                .iter()
                .any(|name| name.starts_with(panel.description())),
            "{panel:?} is missing from {names:?}"
        );
    }
}

#[test]
fn tab_counts_stop_growing_past_three_digits() {
    assert_eq!(&*tab_count_label(7), "7");
    assert_eq!(&*tab_count_label(999), "999");
    assert_eq!(&*tab_count_label(1_000), "999+");
}

/// The Changes tab counts changed files, not group rows: a file with
/// both staged and unstaged edits is one change.
#[test]
fn the_changes_count_is_distinct_paths() {
    assert_eq!(source_control().change_count_label.as_deref(), Some("4"));
    let mut snapshot = snapshot();
    snapshot.status.entries.push(changed(
        repo_path("src/app.rs"),
        ChangeArea::Unstaged,
        ChangeKind::Modified,
        Some((1, 0)),
    ));
    let source = SourceControlModel::ready(main_key(), &snapshot);
    assert_eq!(source.change_count_label.as_deref(), Some("4"));
    let mut clean = self::snapshot();
    clean.status.entries.clear();
    assert_eq!(
        SourceControlModel::ready(main_key(), &clean).change_count_label,
        None
    );
}

#[test]
fn uploads_do_not_require_a_commit_subject_and_keep_their_guards() {
    let renderer = headless_renderer();
    let state = SidebarState::default();
    for width in [240.0, 380.0] {
        let mut source = source_control();
        assert!(source.can_push());
        let actions =
            frame_enter_presses_at_width(&renderer, changes_footer(&state, &source, false), width);
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, SidebarAction::Push { .. }))
        );
        assert!(
            !actions
                .iter()
                .any(|action| matches!(action, SidebarAction::Commit { .. }))
        );
        assert!(
            !frame_enter_presses_at_width(&renderer, changes_footer(&state, &source, true), width)
                .iter()
                .any(|action| matches!(action, SidebarAction::Push { .. }))
        );
        source.set_mutation(Some(MutationKind::Stage));
        assert!(
            !frame_enter_presses_at_width(&renderer, changes_footer(&state, &source, false), width)
                .iter()
                .any(|action| matches!(action, SidebarAction::Push { .. }))
        );
        source.set_mutation(None);
        source.upstream = None;
        source.tracking = TrackingState::NoUpstream;
        assert!(source.can_publish());
        let actions =
            frame_enter_presses_at_width(&renderer, changes_footer(&state, &source, false), width);
        assert!(
            actions
                .iter()
                .any(|action| matches!(action, SidebarAction::OpenPublish { .. }))
        );
        assert!(!actions.iter().any(|action| matches!(
            action,
            SidebarAction::Push { .. } | SidebarAction::Commit { .. }
        )));
    }
}

/// Committing belongs to the working tree, and the working tree is one of
/// three views. Leaving the commit box under History or branch review
/// would offer a mutation of state that view is not showing.
#[test]
fn committing_is_offered_only_by_the_working_tree_view() {
    let renderer = headless_renderer();
    let model = panel_model();
    let mut state = panel_state(&model);
    state.apply(&SidebarAction::CommitMessageChanged {
        key: main_key(),
        message: "a message".into(),
    });
    for tab in ChangesTab::ALL {
        state.changes_tab = tab;
        let commits = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false))
            .iter()
            .any(|action| matches!(action, SidebarAction::Commit { .. }));
        assert_eq!(
            commits,
            tab == ChangesTab::Changes,
            "{tab:?} offered Commit: {commits}"
        );
    }
}

/// Every change group discloses the same way. `Unstaged` used to be the
/// one group with no way to fold a long list away.
#[test]
fn every_change_group_folds_away_its_rows() {
    let renderer = headless_renderer();
    let model = panel_model();
    let mut state = panel_state(&model);
    state.changes_tab = ChangesTab::Changes;
    for section in PanelSection::ALL {
        assert!(
            state.section_expanded(section),
            "the fixture opens every group; {section:?} was closed"
        );
    }
    let unstaged_row = |action: &SidebarAction| {
        matches!(
            action,
            SidebarAction::OpenChange {
                area: ChangeArea::Unstaged,
                ..
            }
        )
    };
    let open = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false));
    assert!(
        open.iter().any(unstaged_row),
        "the fixture must draw unstaged rows for this test to mean anything; it published \
             {open:?}"
    );
    state.apply(&SidebarAction::ToggleSection {
        key: main_key(),
        section: PanelSection::Unstaged,
    });
    assert!(!state.section_expanded(PanelSection::Unstaged));
    let folded = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false));
    assert!(
        !folded.iter().any(unstaged_row),
        "a folded group must draw none of its rows; it published {folded:?}"
    );
    assert!(
        folded.iter().any(|action| matches!(
            action,
            SidebarAction::ToggleSection {
                section: PanelSection::Unstaged,
                ..
            }
        )),
        "the folded group must keep the control that reopens it; it published {folded:?}"
    );
}

#[test]
fn no_panel_command_is_pressable_while_a_dialog_is_open() {
    let renderer = headless_renderer();
    let mut model = panel_model();
    for project in &mut model.projects {
        project.github_ready = true;
    }
    let mut state = panel_state(&model);
    state.changes_tab = ChangesTab::Changes;

    // The Changes view contains index mutations; History and Branch are
    // separate views with their own commands, and each must stand down.
    let live = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, false));
    assert!(
        live.iter()
            .any(|action| matches!(action, SidebarAction::Stage { .. })),
        "the fixture must actually draw the commands this test is about; it published {live:?}"
    );

    let gated = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, true));
    assert!(
        gated.is_empty(),
        "a dialog is an overlay: the pane keeps the focus it held when the dialog opened and \
             answers Enter behind it, and the application refuses every pane action while a modal \
             is up - so a control that still published one would report itself enabled, take the \
             press, and do nothing. It published {gated:?}"
    );
    for tab in ChangesTab::ALL {
        state.changes_tab = tab;
        let gated = frame_enter_presses(&renderer, git_panel(&model, &state, 380.0, true));
        assert!(
            gated.is_empty(),
            "{tab:?} published {gated:?} behind a modal"
        );
    }
}

#[test]
fn the_discard_confirmations_own_answers_survive_the_modal_it_is_itself() {
    let renderer = headless_renderer();
    let key = main_key();
    let pending = DiscardConfirmation {
        key: key.clone(),
        request: DiscardRequest {
            worktree: key.canonical_path.clone(),
            tracked: Vec::new(),
            untracked: vec![repo_path("notes.txt")],
            confirmed: false,
        },
    };
    let fired = frame_enter_presses(
        &renderer,
        discard_confirmation_dialog(&pending, Id::new("dialog.question.DiscardChanges")),
    );
    assert!(
        fired.contains(&SidebarAction::CancelDiscard),
        "refusing the dialog's own Cancel while the dialog is up would strand it with no way \
             out; it published {fired:?}"
    );
    assert!(
        fired.contains(&pending.confirm_action()),
        "the dialog's Discard is the only route to a confirmed request; it published {fired:?}"
    );
}

#[test]
fn background_activity_keeps_panel_content_in_place() {
    use cosmic::iced::Size;
    use cosmic::iced::advanced::{Layout, layout, mouse, widget::Tree};
    let renderer = headless_renderer();
    let bounds = |mut element: Element<'_, SidebarAction>, width| {
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 700.0)),
        );
        let nodes =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter_map(|node| {
                let label = node.node().label()?;
                let rect = node.node().bounds()?;
                Some((label.to_owned(), [rect.x0, rect.y0, rect.x1, rect.y1]))
            })
            .collect::<BTreeMap<_, _>>()
    };
    for width in [220.0, 380.0] {
        for panel in [WorktreePanel::Files, WorktreePanel::Changes] {
            let model = panel_model();
            let state = panel_state(&model);
            let before = bounds(
                worktree_sidebar(&model, &state, width, false, panel, true),
                width,
            );
            assert!(before.len() > 3, "fixture must contain visible controls");
            for mutation in [None, Some(MutationKind::Stage)] {
                let mut busy = model.clone();
                let source = busy.source_control.as_mut().unwrap();
                source.reloading = true;
                source.mutation = mutation;
                let after = bounds(
                    worktree_sidebar(&busy, &state, width, false, panel, true),
                    width,
                );
                for (label, rect) in &before {
                    assert_eq!(after.get(label), Some(rect), "{panel:?}: {label} moved");
                }
            }
        }
    }
}

#[test]
fn loading_more_history_keeps_commits_and_pager_in_place() {
    let mut source = source_control();
    let identity = CommitIdentity {
        name: "Author".into(),
        email: "author@example.com".into(),
        timestamp: 0,
        offset_minutes: 0,
    };
    source.history = vec![CommitRow::new(&CommitSummary {
        oid: oid_of(1),
        parents: vec![],
        author: identity.clone(),
        committer: identity,
        subject: "Existing commit".into(),
        body: String::new(),
    })];
    source.history_loaded = true;
    assert_eq!(history_notice(&source), None);
    let renderer = headless_renderer();
    let state = SidebarState::default();
    let before = accessible_names(&renderer, history_section(&state, &source, false));
    assert!(
        before
            .iter()
            .any(|name| name.contains(&format!("Load {HISTORY_PAGE_SIZE} more commits")))
    );
    source.mark_history_requested();
    assert_eq!(history_notice(&source), None);
    let after = accessible_names(&renderer, history_section(&state, &source, false));
    assert_eq!(before, after);
    assert!(
        frame_enter_presses(&renderer, history_section(&state, &source, false))
            .iter()
            .all(|action| !matches!(action, SidebarAction::LoadMoreHistory { .. }))
    );
}

#[test]
fn a_history_page_is_asked_for_once_and_a_short_page_ends_the_walk() {
    let mut source = source_control();
    assert!(
        source.wants_history(),
        "a worktree whose HEAD names a commit has a walk worth offering"
    );
    assert!(!source.history_requested());
    source.mark_history_requested();
    assert!(
        source.history_requested(),
        "expanding History twice must not queue a second walk"
    );

    assert_eq!(source.history_count(), None, "nobody has asked yet");
    source.append_history(&[]);
    assert_eq!(
        source.history_count(),
        Some(0),
        "an empty page is an answer; \"nobody asked\" is not"
    );
    assert!(!source.can_load_history(), "a short page ends the walk");
}
#[test]
fn untracked_starts_expanded_and_history_reloads_when_head_moves() {
    assert!(SidebarState::default().section_expanded(PanelSection::Untracked));
    let mut previous = SourceControlModel::ready(main_key(), &snapshot());
    previous.head_oid = Some(oid_of(1).as_str().into());
    previous.mark_history_requested();
    let mut same = SourceControlModel::ready(main_key(), &snapshot());
    same.head_oid = previous.head_oid.clone();
    previous.carry_mutation_into(&mut same);
    assert!(same.history_loading);
    let mut moved = SourceControlModel::ready(main_key(), &snapshot());
    moved.head_oid = Some(oid_of(2).as_str().into());
    previous.carry_mutation_into(&mut moved);
    assert!(!moved.history_requested());
    assert!(moved.wants_history());
    previous.history_failed("Failed".into());
    assert!(!previous.history_loading);
    assert!(
        previous.history_requested(),
        "failure requires explicit retry, not an automatic loop"
    );
    assert!(previous.can_load_history());
}
