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
fn restart(mut app: Cagents, database: &Path, legacy_view: bool) -> Cagents {
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
            "saved_channel_work",
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
    restored.durable = Arc::new(state.unwrap());
    restored.workspace = Cagents::rebuild_workspace(&restored.durable);
    restored.workspace.resize_viewport(300.0);
    // These presentation values are applied by init before scope restoration.
    restored.context_tab = view.context_tab;
    restored.context_inspector_open = view.context_inspector_open;
    restored.chrome_ui = chrome_ui::State::default().with_user_titles(view.user_titles.clone());
    drop(restored.restore_window_view(Some(view)));
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
        Arc::make_mut(&mut app.durable).cards = vec![hidden.clone()];
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(hidden.id, root);
        app.context_tab = ContextTab::Changes;
        app.context_inspector_open = false;
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
        assert_eq!(restored.context_tab, ContextTab::Changes);
        assert!(!restored.context_inspector_open);
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
        durable.cards = vec![first.clone(), second.clone()];
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
        app.context_tab = ContextTab::Details;
        app.chrome_ui.user_titles.insert(second.id, "Review".into());
        let database = directory.path().join(format!("selected-{channel}.sqlite3"));
        let restored = restart(app, &database, false);
        assert_eq!(restored.active_workspace, Some(root));
        assert_eq!(restored.durable.active_channel, channel.then_some(id));
        assert_eq!(
            restored.workspace.order().collect::<Vec<_>>(),
            [first.id, second.id]
        );
        assert_eq!(restored.workspace.focused(), Some(second.id));
        assert_eq!(restored.context_tab, ContextTab::Details);
        assert_eq!(restored.chrome_ui.user_titles[&second.id], "Review");
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
        durable.cards = vec![hidden.clone(), visible.clone()];
        durable.focused = Some(visible.id);
        durable.card_scopes.insert(hidden.id, root);
        app.workspace = Cagents::rebuild_workspace(&WorkspaceState {
            cards: vec![visible.clone()],
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
    durable.cards = vec![retained.clone()];
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
    drop(app.restore_window_view(None));
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
    durable.cards = vec![card.clone(), other, last];
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
