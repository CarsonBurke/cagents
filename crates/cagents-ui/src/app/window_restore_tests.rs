use super::*;
use crate::model::{BootId, ChannelId, ProjectChannel, WorktreeId};

fn terminal(directory: &Path) -> CardState {
    let launch = LaunchRecipe {
        program: "/fixture-terminal-never-executed".into(),
        arguments: Vec::new(),
        working_directory: directory.into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    CardState {
        id: CardId::new(),
        generation: 0,
        title: "Fixture terminal".into(),
        width_fraction: 0.5,
        existence: Existence::Open,
        kind: CardKind::Terminal(TerminalSession {
            id: TerminalId::new(),
            launch: launch.clone(),
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::RelaunchAvailable {
                recipe: launch,
                last_exit: None,
            },
            owned_codex_listener: None,
            recovery_origin: None,
            predecessor: None,
        }),
    }
}

fn fixture(root: &Path) -> Cagents {
    let path = root.join("alpha");
    std::fs::create_dir_all(&path).unwrap();
    let mut app = context_inspector_test_app(false);
    app.sidebar_model.projects = vec![SidebarProject::new(
        ProjectId::new(),
        RepositoryId::new(),
        "alpha".into(),
        path.join(".git"),
        vec![SidebarWorktree::new(
            WorktreeId::new(),
            path,
            Some("main".into()),
            false,
            false,
            None,
            None,
            None,
        )],
        Vec::new(),
    )];
    app
}

/// Use the same snapshot and WindowView save boundaries as shutdown, then
/// reopen the actual SQLite store. No runtime tasks or providers are executed.
fn restart(app: Cagents, database: &Path, legacy_view: bool) -> Cagents {
    restart_with(app, database, legacy_view, None)
}

/// `scope_project` stands in for the project init reads from the saved
/// scope's worktree record, which outlives its sidebar row.
fn restart_with(
    mut app: Cagents,
    database: &Path,
    legacy_view: bool,
    scope_project: Option<ProjectId>,
) -> Cagents {
    let boot = BootId::new("window-view-fixture");
    let (session, _) = SessionRuntime::open(database, boot.clone()).unwrap();
    app.session = Some(session);
    app.settings_path = Some(database.with_extension("toml"));
    app.settings_write_blocked = false;
    app.persist();
    app.save_settings_now().unwrap();
    let mut session = app.session.take().unwrap();
    session.flush(Duration::from_secs(5)).unwrap();
    if legacy_view {
        // 991daa43e saved the original SidebarView fields. Its bytes must be
        // enough to recover home without requiring the old build to change.
        let store = session.operation_store();
        let mut view: serde_json::Value =
            serde_json::from_str(&store.load_window_view().unwrap().unwrap()).unwrap();
        for field in [
            "channel_offsets",
            "settings_category",
            "settings_search",
        ] {
            view.as_object_mut().unwrap().remove(field);
        }
        let sidebar = view["sidebar"].as_object_mut().unwrap();
        for field in [
            "filter",
            "toggled_sections",
            "panel_open",
            "files_query",
            "files_filter_open",
            "change_list_mode",
            "changes_tab",
        ] {
            sidebar.remove(field);
        }
        store.save_window_view(&view.to_string()).unwrap();
    }
    session.shutdown();
    drop(session);
    let (session, state) = SessionRuntime::open(database, boot).unwrap();
    let view: WindowView = serde_json::from_str(
        &session
            .operation_store()
            .load_window_view()
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    let mut restored = context_inspector_test_app(false);
    restored.sidebar_model = app.sidebar_model.clone();
    // Startup seeds these boundaries from all catalog records, including
    // retired worktrees whose panes must stay separate from the parent.
    restored.known_worktree_roots = app.known_worktree_roots.clone();
    restored.durable = Arc::new(state.unwrap());
    restored.workspace = Cagents::rebuild_workspace(&restored.durable);
    restored.workspace.resize_viewport(300.0);
    // These presentation values are applied by init before scope restoration.
    restored.chrome_ui = chrome_ui::State::default()
        .with_user_titles(view.user_titles.clone())
        .with_projects_collapsed(view.projects_sidebar_collapsed);
    drop(restored.restore_window_view(Some(view), scope_project));
    drop(session);
    restored
}

#[test]
fn independent_window_stores_restore_home_with_retained_shared_panes() {
    let directory = tempfile::tempdir().unwrap();
    for (slot, legacy) in [("primary", true), ("second", false)] {
        let mut app = fixture(directory.path());
        let root = app.sidebar_model.projects[0].worktrees[0].path.clone();
        let hidden = terminal(&root);
        Arc::make_mut(&mut app.durable).cards = vec![hidden.clone()].into();
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(hidden.id, root);
        app.chrome_ui.projects_collapsed = true;
        app.sidebar_state.apply(&SidebarAction::ToggleControls);
        app.sidebar_state
            .apply(&SidebarAction::SearchChanged(slot.into()));
        let project = app.sidebar_model.projects[0].id;
        app.sidebar_state
            .apply(&SidebarAction::ToggleProject(project));
        let database = directory.path().join(slot).join("sessions.sqlite3");
        std::fs::create_dir_all(database.parent().unwrap()).unwrap();
        let restored = restart(app, &database, legacy);
        assert_eq!(restored.active_workspace, None, "{slot}");
        assert_eq!(restored.durable.active_channel, None, "{slot}");
        assert_eq!(restored.workspace.focused(), None, "{slot}");
        assert!(restored.workspace.cards().is_empty(), "{slot}");
        assert!(
            restored.card_state(hidden.id).is_some(),
            "retain hidden work"
        );
        assert_eq!(restored.sidebar_state.query(), slot);
        assert!(!restored.sidebar_state.project_expanded(project));
        assert!(restored.chrome_ui.projects_collapsed);
    }
}

#[test]
fn selected_worktree_and_channel_restore_their_focused_pane_from_stores() {
    let directory = tempfile::tempdir().unwrap();
    for channel in [false, true] {
        let mut app = fixture(directory.path());
        let project = app.sidebar_model.projects[0].id;
        let root = app.sidebar_model.projects[0].worktrees[0].path.clone();
        let first = terminal(&root);
        let second = terminal(&root);
        let id = ChannelId::new();
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards = vec![first.clone(), second.clone()].into();
        durable.focused = Some(second.id);
        durable.active_worktree = Some(root.clone());
        durable.worktree_focus.insert(root.clone(), second.id);
        if channel {
            durable.active_channel = Some(id);
            durable.channels.push(ProjectChannel {
                id,
                project_id: project,
                title: "Release review".into(),
                title_source: Default::default(),
                ordinal: 0,
                shared: false,
                cards: vec![first.id, second.id],
                focused: Some(second.id),
            });
        }
        drop(app.restore_worktree_scope());
        app.chrome_ui.user_titles.insert(second.id, "Review".into());
        let mut sidebar = serde_json::to_value(app.sidebar_state.saved_view()).unwrap();
        sidebar["files_query"] = serde_json::json!("kept");
        app.sidebar_state
            .restore_view(serde_json::from_value(sidebar).unwrap());
        let database = directory.path().join(format!("selected-{channel}.sqlite3"));
        let restored = restart(app, &database, false);
        assert_eq!(restored.active_workspace, Some(root));
        assert_eq!(restored.durable.active_channel, channel.then_some(id));
        assert_eq!(
            restored.workspace.order().collect::<Vec<_>>(),
            [first.id, second.id]
        );
        assert_eq!(restored.workspace.focused(), Some(second.id));
        assert_eq!(restored.chrome_ui.user_titles[&second.id], "Review");
        // Back where it was saved, the panel keeps its worktree-local state.
        assert_eq!(restored.sidebar_state.files_query(), "kept");
    }
}

#[test]
fn home_preserves_utility_and_unowned_terminal_without_opening_retained_scope() {
    let directory = tempfile::tempdir().unwrap();
    for unowned_terminal in [false, true] {
        let mut app = fixture(directory.path());
        let root = app.sidebar_model.projects[0].worktrees[0].path.clone();
        let hidden = terminal(&root);
        let unowned = directory.path().join("outside-projects");
        std::fs::create_dir_all(&unowned).unwrap();
        let mut visible = terminal(&unowned);
        if !unowned_terminal {
            visible.kind = CardKind::Settings;
        }
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards = vec![hidden.clone(), visible.clone()].into();
        durable.focused = Some(visible.id);
        durable.card_scopes.insert(hidden.id, root);
        app.workspace = Cagents::rebuild_workspace(&WorkspaceState {
            cards: vec![visible.clone()].into(),
            focused: Some(visible.id),
            ..WorkspaceState::default()
        });
        app.settings_category = SettingsCategory::Terminal;
        app.settings_search = "cursor".into();
        let database = directory
            .path()
            .join(format!("home-{unowned_terminal}.sqlite3"));
        let restored = restart(app, &database, false);
        assert_eq!(restored.active_workspace, None);
        assert_eq!(restored.settings_category, SettingsCategory::Terminal);
        assert_eq!(restored.settings_search, "cursor");
        assert_eq!(restored.workspace.order().collect::<Vec<_>>(), [visible.id]);
        assert_eq!(restored.workspace.focused(), Some(visible.id));
        assert!(restored.card_state(hidden.id).is_some());
    }
}

#[test]
fn stale_channel_selection_does_not_enter_a_retained_panes_worktree() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("stale-channel.sqlite3");
    let mut app = fixture(directory.path());
    let project = app.sidebar_model.projects[0].id;
    let root = app.sidebar_model.projects[0].worktrees[0].path.clone();
    let retained = terminal(&root);
    let retained_channel = ChannelId::new();
    let stale_channel = ChannelId::new();
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards = vec![retained.clone()].into();
    durable.focused = Some(retained.id);
    durable.channels.push(ProjectChannel {
        id: retained_channel,
        project_id: project,
        title: "Retained channel".into(),
        title_source: Default::default(),
        ordinal: 0,
        shared: false,
        cards: vec![retained.id],
        focused: Some(retained.id),
    });
    let workspace = durable.id;
    let boot = BootId::new("stale-channel-fixture");
    let (mut session, _) = SessionRuntime::open(&database, boot.clone()).unwrap();
    session.snapshot(Arc::clone(&app.durable));
    session.flush(Duration::from_secs(5)).unwrap();
    session.shutdown();
    drop(session);

    // Simulate a selection left behind when its channel disappeared. Normal
    // saves reject this dangling reference, so inject only that metadata into
    // the fixture store; leave the other channel and its focused pane intact.
    let connection = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE metadata SET value = ?1 WHERE key = ?2",
                rusqlite::params![
                    serde_json::to_string(&Some(stale_channel)).unwrap(),
                    format!("workspace:{workspace}:navigation:active_thread"),
                ],
            )
            .unwrap(),
        1,
    );
    drop(connection);
    let (session, restored) = SessionRuntime::open(&database, boot).unwrap();
    app.durable = Arc::new(restored.unwrap());
    assert_eq!(app.durable.focused, Some(retained.id));
    assert_eq!(app.durable.active_worktree, None);
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    drop(app.restore_window_view(None, None));
    assert_eq!(app.active_workspace, None);
    assert_eq!(app.durable.active_channel, None);
    assert_eq!(app.workspace.focused(), None);
    assert!(app.workspace.cards().is_empty());
    assert!(app.card_state(retained.id).is_some());
    drop(session);
}

#[test]
fn sidebar_disclosures_restore_after_initial_source_control_binding() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = fixture(directory.path());
    let root = app.sidebar_model.projects[0].worktrees[0].path.clone();
    let card = terminal(&root);
    let other = terminal(&root);
    let last = terminal(&root);
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards = vec![card.clone(), other, last].into();
    durable.focused = Some(card.id);
    durable.active_worktree = Some(root.clone());
    durable.worktree_focus.insert(root.clone(), card.id);
    drop(app.restore_worktree_scope());
    let mut sidebar = serde_json::to_value(app.sidebar_state.saved_view()).unwrap();
    sidebar["expanded_directories"] = serde_json::json!(["src"]);
    sidebar["collapsed_change_directories"] = serde_json::json!([["Branch", ["src"]]]);
    sidebar["filter"] = serde_json::json!("Attention");
    sidebar["toggled_sections"] = serde_json::json!(["Untracked"]);
    sidebar["panel_open"] = serde_json::json!(true);
    sidebar["files_query"] = serde_json::json!("restore");
    sidebar["files_filter_open"] = serde_json::json!(true);
    sidebar["changes_tab"] = serde_json::json!("History");
    app.sidebar_state
        .restore_view(serde_json::from_value(sidebar.clone()).unwrap());
    // RepoPath accepts legacy strings but writes byte-preserving paths.
    let sidebar = serde_json::to_value(app.sidebar_state.saved_view()).unwrap();
    app.workspace.native_scroll(137.0, 300.0);
    assert_eq!(app.workspace.camera().offset(), 137.0);
    let restored = restart(app, &directory.path().join("sidebar.sqlite3"), false);
    assert!(restored.sidebar_state.active_worktree().is_some());
    assert_eq!(
        serde_json::to_value(restored.sidebar_state.saved_view()).unwrap(),
        sidebar
    );
    assert_eq!(restored.worktree_offsets[&root], 137.0);
    assert_eq!(restored.workspace.camera().offset(), 137.0);
    assert_eq!(restored.workspace.focused(), Some(card.id));
}

#[test]
fn removed_saved_worktree_restores_its_live_project_or_home_from_store() {
    let directory = tempfile::tempdir().unwrap();
    // External worktrees are found through the project their record names;
    // without a record, only nesting ties a directory to a project.
    for (name, nested, lingering, recorded) in [
        ("removed-nested", true, false, false),
        ("retired-nested-directory", true, true, false),
        ("removed-external", false, false, true),
        ("retired-external-directory", false, true, true),
        ("unrecorded-external", false, false, false),
    ] {
        let mut app = fixture(directory.path());
        let project = app.sidebar_model.projects[0].id;
        let primary = app.sidebar_model.projects[0].worktrees[0].path.clone();
        // The removed worktree's own panel state does not follow the window
        // to another checkout.
        let mut sidebar = serde_json::to_value(app.sidebar_state.saved_view()).unwrap();
        sidebar["files_query"] = serde_json::json!("removed only");
        sidebar["changes_tab"] = serde_json::json!("History");
        app.sidebar_state
            .restore_view(serde_json::from_value(sidebar).unwrap());
        let missing = if nested {
            primary.join("worktrees/removed")
        } else {
            directory.path().join("removed-external")
        };
        if lingering {
            std::fs::create_dir_all(&missing).unwrap();
        }
        // The retired record seeds ownership but isn't a navigable row. Git
        // may have removed only its worktree metadata, leaving the folder.
        app.known_worktree_roots.insert(missing.clone());
        let hidden = terminal(&missing);
        let main = terminal(&primary);
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards = vec![hidden.clone(), main.clone()].into();
        durable.focused = Some(hidden.id);
        durable.active_worktree = Some(missing.clone());
        durable.card_scopes.insert(hidden.id, missing.clone());
        durable.card_scopes.insert(main.id, primary.clone());
        durable.worktree_focus.insert(primary.clone(), main.id);
        let database = directory.path().join(format!("{name}.sqlite3"));
        let restored = restart_with(app, &database, false, recorded.then_some(project));
        let survives = nested || recorded;
        let expected = survives.then_some(primary);
        assert_eq!(restored.active_workspace, expected, "{name}");
        assert_eq!(restored.durable.active_worktree, expected, "{name}");
        assert_eq!(restored.durable.active_channel, None, "{name}");
        assert_eq!(
            restored.workspace.focused(),
            survives.then_some(main.id),
            "{name}"
        );
        assert_eq!(
            restored.workspace.order().collect::<Vec<_>>(),
            if survives { vec![main.id] } else { Vec::new() },
            "{name}"
        );
        assert_eq!(restored.sidebar_state.files_query(), "", "{name}");
        assert_eq!(
            restored.sidebar_state.changes_tab(),
            sidebar::ChangesTab::default(),
            "{name}"
        );
        assert!(
            restored.card_state(hidden.id).unwrap().existence.is_open(),
            "retain removed worktree's pane"
        );
        assert_eq!(restored.durable.card_scopes[&hidden.id], missing, "{name}");
        if lingering {
            std::fs::remove_dir_all(&missing).unwrap();
        }
    }
}

#[test]
fn saved_unregistered_directory_still_restores_when_it_exists() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = fixture(directory.path());
    let outside = directory.path().join("outside-catalog");
    std::fs::create_dir_all(&outside).unwrap();
    let card = terminal(&outside);
    let durable = Arc::make_mut(&mut app.durable);
    durable.cards = vec![card.clone()].into();
    durable.focused = Some(card.id);
    durable.active_worktree = Some(outside.clone());
    durable.worktree_focus.insert(outside.clone(), card.id);
    let restored = restart(app, &directory.path().join("unregistered.sqlite3"), false);
    assert_eq!(restored.active_workspace, Some(outside));
    assert_eq!(restored.workspace.focused(), Some(card.id));
}

#[test]
fn a_restored_file_filter_and_open_folders_are_read_once_restored() {
    let directory = tempfile::tempdir().unwrap();
    let reads = |files_query: &str, name: &str| {
        let mut app = fixture(directory.path());
        let root = app.sidebar_model.projects[0].worktrees[0].path.clone();
        let card = terminal(&root);
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards = vec![card.clone()].into();
        durable.focused = Some(card.id);
        durable.active_worktree = Some(root.clone());
        durable.worktree_focus.insert(root, card.id);
        drop(app.restore_worktree_scope());
        let mut sidebar = serde_json::to_value(app.sidebar_state.saved_view()).unwrap();
        sidebar["files_query"] = serde_json::json!(files_query);
        app.sidebar_state
            .restore_view(serde_json::from_value(sidebar).unwrap());
        let restored = restart(app, &directory.path().join(format!("{name}.sqlite3")), false);
        assert_eq!(restored.sidebar_state.files_query(), files_query);
        restored.refresh_ui.source_generation
    };
    // The first read was built before the filter came back, so one more
    // read lists what it matches.
    assert_eq!(reads("restore", "filtered"), reads("", "plain") + 1);
}

#[test]
fn a_saved_presentation_value_this_build_does_not_know_keeps_the_rest_of_the_view() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = fixture(directory.path());
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    let mut view = serde_json::to_value(app.capture_settings_save().unwrap().view).unwrap();
    view["settings_category"] = serde_json::json!("FromANewerBuild");
    view["settings_search"] = serde_json::json!("kept");
    let sidebar = &mut view["sidebar"];
    sidebar["filter"] = serde_json::json!("FromANewerBuild");
    sidebar["toggled_sections"] = serde_json::json!(["Untracked", "FromANewerBuild"]);
    sidebar["change_list_mode"] = serde_json::json!("FromANewerBuild");
    sidebar["changes_tab"] = serde_json::json!("FromANewerBuild");
    sidebar["files_query"] = serde_json::json!("kept");
    let view: WindowView = serde_json::from_value(view).unwrap();
    assert_eq!(view.settings_category, None);
    assert_eq!(view.settings_search, "kept");
    let sidebar = serde_json::to_value(&view.sidebar).unwrap();
    assert_eq!(sidebar["filter"], serde_json::json!(sidebar::SidebarFilter::default()));
    assert_eq!(sidebar["toggled_sections"], serde_json::json!([]));
    assert_eq!(sidebar["changes_tab"], serde_json::json!(sidebar::ChangesTab::default()));
    assert_eq!(sidebar["files_query"], serde_json::json!("kept"));
}
