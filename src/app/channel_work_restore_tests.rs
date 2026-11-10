use super::*;

fn fixture() -> (Cagents, Home, WorkspaceModel, tempfile::TempDir) {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("project");
    assert!(
        std::process::Command::new("git")
            .args(["-c", "init.templateDir=", "init", "--quiet"])
            .arg(&root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .status()
            .unwrap()
            .success()
    );
    let repository = root.join(".git");
    let (mut app, home) = main_fixture();
    two_channels(&mut app, home);
    app.settings_path = Some(directory.path().join("settings.json"));
    let project = &mut app.sidebar_model.projects[0];
    project.repository = repository.clone();
    project.worktrees[0].path = root.clone();
    let durable = Arc::make_mut(&mut app.durable);
    for card in &mut durable.cards {
        if let CardKind::Terminal(session) = &mut card.kind {
            session.launch.working_directory = root.clone();
        }
    }
    for path in durable.card_scopes.values_mut() {
        *path = root.clone();
    }
    app.active_workspace = Some(root);
    let mut observed = model(home);
    observed.channels[0].repository = repository.clone();
    observed
        .projects
        .push(crate::workspace_model::ModelProject {
            repository,
            name: "Restore fixture".into(),
            ordinal: 1,
            kind: ProjectKind::Git,
        });
    // A new application has its pane projection and catalog but has not opened
    // the Work surface or connected to the model yet.
    app.channel_work = State::default();
    app.workspace_model = WorkspaceModel::default();
    (app, home, observed, directory)
}

fn saved_presentation_work(app: &mut Cagents) -> Option<Home> {
    // Inspect the actual persisted presentation payload without running any
    // settings writer or provider task from this application fixture.
    let blocked = app.settings_write_blocked;
    app.settings_write_blocked = false;
    let captured = app.capture_settings_save();
    app.settings_write_blocked = blocked;
    captured.unwrap().view.saved_channel_work
}

#[test]
fn saved_work_waits_for_model_then_restores_the_exact_channel_surface() {
    let (mut app, home, observed, _directory) = fixture();
    drop(app.restore_channel_work(Some(home)));
    assert!(!app.channel_work_is_main());
    assert_eq!(app.saved_channel_work(), Some(home));
    // Scope promotion/catalog loading is passive startup work and may change
    // the workspace generation before the registry's first snapshot arrives.
    app.workspace_view_generation += 1;
    drop(app.apply_workspace_model_update(
        super::super::super::workspace_model_sync::ModelUpdate::Read(observed.clone()),
    ));
    assert!(app.channel_work_is_main());
    assert_eq!(app.channel_work.view.home(), Some(home));
    assert_eq!(app.durable.active_channel, Some(home.channel));
    assert_eq!(app.saved_channel_work(), Some(home));
    assert!(app.channel_work.pending_restore.is_none());
    let owner = app.channel_work.view.owner();
    drop(app.restore_pending_channel_work());
    assert_eq!(app.channel_work.view.owner(), owner, "restore only once");
}

#[test]
fn saved_work_never_rebinds_to_a_replaced_registry_or_channel() {
    for replacement in ["registry", "ordinal", "removed"] {
        let (mut app, home, observed, _directory) = fixture();
        drop(app.restore_channel_work(Some(home)));
        app.workspace_model = observed.clone();
        match replacement {
            "registry" => app.workspace_model.identity = uuid::Uuid::new_v4(),
            "ordinal" => app.workspace_model.channels[0].ordinal += 1,
            "removed" => app.workspace_model.channels.clear(),
            _ => unreachable!(),
        }
        drop(app.restore_pending_channel_work());
        assert!(!app.channel_work_is_main(), "{replacement}");
        assert_eq!(app.saved_channel_work(), None, "{replacement}");
    }
}

#[test]
fn startup_work_restoration_yields_to_navigation_while_connecting() {
    for explicit_navigation in [false, true] {
        let (mut app, home, observed, _directory) = fixture();
        drop(app.restore_channel_work(Some(home)));
        if explicit_navigation {
            drop(app.update(Message::Sidebar(SidebarAction::OpenSettings)));
        } else {
            app.input_trace.record(&cosmic::iced::Event::Mouse(
                cosmic::iced::mouse::Event::WheelScrolled {
                    delta: cosmic::iced::mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
                },
            ));
        }
        app.workspace_model = observed.clone();
        drop(app.restore_pending_channel_work());
        assert!(!app.channel_work_is_main());
        assert_eq!(app.saved_channel_work(), None);
    }
}

#[test]
fn navigation_outside_the_old_message_list_retires_the_restore_lease() {
    let (mut app, home, observed, _directory) = fixture();
    drop(app.restore_channel_work(Some(home)));
    let navigation = Message::AccountUi(account_ui::Action::OpenPane);
    assert!(!workspace_navigation_blocked_during_save(&navigation));
    drop(app.update(navigation));
    let focused = app.workspace.focused().unwrap();
    assert!(matches!(
        app.card_state(focused).unwrap().kind,
        CardKind::Accounts
    ));
    assert!(app.channel_work.pending_restore.is_none());
    drop(app.apply_workspace_model_update(
        super::super::super::workspace_model_sync::ModelUpdate::Read(observed.clone()),
    ));
    assert!(!app.channel_work_is_main());
    assert_eq!(app.workspace.focused(), Some(focused));
    assert_eq!(saved_presentation_work(&mut app), None);
}

#[test]
fn startup_focus_without_user_input_does_not_cancel_restore() {
    let (mut app, home, observed, _directory) = fixture();
    drop(app.restore_channel_work(Some(home)));
    app.input_trace.record(&cosmic::iced::Event::InputMethod(
        cosmic::iced::advanced::input_method::Event::Opened,
    ));
    app.workspace_model = observed.clone();
    drop(app.restore_pending_channel_work());
    assert!(app.channel_work_is_main());
}

#[test]
fn a_project_that_never_opens_expires_and_saves_the_actual_view() {
    let (mut app, home, observed, _directory) = fixture();
    app.workspace_model = observed.clone();
    app.sidebar_model.projects.clear();
    drop(app.restore_channel_work(Some(home)));
    let pending = app.channel_work.pending_restore.as_mut().unwrap();
    let token = pending.token;
    pending.armed = Instant::now() - RESTORE_TIMEOUT;
    // A delayed timer cannot cause the expired request to be saved again.
    assert_eq!(app.saved_channel_work(), None);
    drop(app.update(Message::ChannelWorkRestoreExpired(token)));
    assert!(app.channel_work.pending_restore.is_none());
    assert!(!app.channel_work_is_main());
    assert_eq!(saved_presentation_work(&mut app), None);
    drop(app.restore_pending_channel_work());
    assert!(!app.channel_work_is_main());
}

#[test]
fn expiry_writes_the_actual_presentation_to_the_fixture_store() {
    let (mut app, home, _observed, directory) = fixture();
    let (session, _) = SessionRuntime::open(
        &directory.path().join("window.sqlite3"),
        crate::model::BootId::new("restore-expiry-fixture"),
    )
    .unwrap();
    let store = session.operation_store();
    app.session = Some(session);
    app.settings_write_blocked = false;
    drop(app.restore_channel_work(Some(home)));
    app.save_settings_now().unwrap();
    let before: serde_json::Value =
        serde_json::from_str(&store.load_window_view().unwrap().unwrap()).unwrap();
    assert!(!before["saved_channel_work"].is_null());
    let token = app.channel_work.pending_restore.as_ref().unwrap().token;
    // Poll only the expiry's settings write, never this fixture's provider,
    // registry, pane-start or restore-timer tasks.
    let task = app.expire_channel_work_restore(token);
    let mut stream = iced::runtime::task::into_stream(task).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut saved = false;
    while let Some(action) = runtime.block_on(stream.next()) {
        if let iced::runtime::Action::Output(cosmic::Action::App(Message::SettingsSaved {
            result,
            ..
        })) = action
        {
            result.unwrap();
            saved = true;
        }
    }
    assert!(saved);
    let after: serde_json::Value =
        serde_json::from_str(&store.load_window_view().unwrap().unwrap()).unwrap();
    assert!(after["saved_channel_work"].is_null());
    app.settings_write_blocked = true;
    app.session.take().unwrap().shutdown();
}

#[test]
fn unavailable_project_retires_the_request_without_waiting_for_timeout() {
    let (mut app, home, observed, _directory) = fixture();
    app.workspace_model = observed.clone();
    let repository = app.workspace_model.channels[0].repository.clone();
    app.sidebar_model.projects.clear();
    drop(app.restore_channel_work(Some(home)));
    app.bind_channel_work_restore_repository();
    app.channel_work_restore_project_unavailable(&repository);
    drop(app.cancel_stale_channel_work_restore());
    assert!(app.channel_work.pending_restore.is_none());
    assert_eq!(saved_presentation_work(&mut app), None);
}

#[test]
fn bootstrap_placeholder_is_part_of_the_initial_restore_destination() {
    for pane in [
        settings::DefaultPane::NativeChat,
        settings::DefaultPane::Terminal,
    ] {
        let (mut app, home, observed, _directory) = fixture();
        Arc::make_mut(&mut app.durable).cards.clear();
        for channel in &mut Arc::make_mut(&mut app.durable).channels {
            channel.cards.clear();
            channel.focused = None;
        }
        Arc::make_mut(&mut app.durable).focused = None;
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.cards.clear();
        app.settings.default_channel_pane = pane;
        drop(app.restore_channel_work(Some(home)));
        assert_eq!(app.workspace.focused(), None);
        // The production bootstrap builds default panes before publishing its
        // initial window, then begins fencing interaction with that finished view.
        drop(app.ensure_default_pane());
        app.finish_window_restore();
        drop(app.apply_workspace_model_update(
            super::super::super::workspace_model_sync::ModelUpdate::Read(observed.clone()),
        ));
        assert!(app.channel_work_is_main(), "{pane:?}");
        assert_eq!(app.channel_work.view.home(), Some(home));
    }
}

#[test]
fn projection_cannot_adopt_a_new_navigation_destination_as_its_restore_anchor() {
    let (mut app, home, observed, _directory) = fixture();
    drop(app.open_utility_card(CardKind::Accounts));
    drop(app.restore_channel_work(Some(home)));
    let first = app
        .durable
        .cards
        .iter()
        .find(|card| matches!(card.kind, CardKind::Terminal(_)))
        .unwrap()
        .id;
    // A future navigation handler need not be in any message list or call a
    // particular wrapper: its actual focus change still invalidates the lease.
    drop(app.workspace.focus(first).unwrap());
    drop(app.apply_workspace_model_update(
        super::super::super::workspace_model_sync::ModelUpdate::Read(observed),
    ));
    assert!(app.channel_work.pending_restore.is_none());
    assert!(!app.channel_work_is_main());
}

#[test]
fn unreadable_shared_project_expires_during_real_model_reconciliation() {
    let directory = tempfile::tempdir().unwrap();
    let repository = directory.path().join("unmounted/.git");
    let (mut app, home, observed, _directory) = fixture();
    app.sidebar_model.projects.clear();
    drop(app.restore_channel_work(Some(home)));
    let mut observed = observed.clone();
    observed.channels[0].repository = repository.clone();
    observed
        .projects
        .push(crate::workspace_model::ModelProject {
            repository,
            name: "Unavailable project".into(),
            ordinal: 1,
            kind: ProjectKind::Git,
        });
    drop(app.apply_workspace_model_update(
        super::super::super::workspace_model_sync::ModelUpdate::Read(observed),
    ));
    assert!(app.channel_work.pending_restore.is_none());
    assert_eq!(app.saved_channel_work(), None);
    assert_eq!(saved_presentation_work(&mut app), None);
}

#[test]
fn an_old_expiry_cannot_cancel_a_new_restore() {
    let (mut app, home, observed, _directory) = fixture();
    drop(app.restore_channel_work(Some(home)));
    let token = app.channel_work.pending_restore.as_ref().unwrap().token;
    drop(app.restore_channel_work(Some(home)));
    drop(app.update(Message::ChannelWorkRestoreExpired(token)));
    assert_eq!(app.saved_channel_work(), Some(home));
}

#[test]
fn explicit_activation_of_the_current_destination_cancels_restore() {
    for destination in ["pane", "channel", "worktree"] {
        let (mut app, home, observed, _directory) = fixture();
        drop(app.restore_channel_work(Some(home)));
        match destination {
            "pane" => drop(app.focus_workspace_card(app.workspace.focused().unwrap())),
            "channel" => drop(app.navigate_to_channel(home.channel)),
            "worktree" => drop(app.navigate_to_worktree(app.active_workspace.clone().unwrap())),
            _ => unreachable!(),
        }
        assert!(app.channel_work.pending_restore.is_none(), "{destination}");
        app.workspace_model = observed.clone();
        drop(app.restore_pending_channel_work());
        assert!(!app.channel_work_is_main(), "{destination}");
    }
}

#[test]
fn old_windows_without_a_saved_work_surface_stay_in_conversations() {
    let (mut app, home, observed, _directory) = fixture();
    app.workspace_model = observed.clone();
    drop(app.restore_channel_work(None));
    assert!(!app.channel_work_is_main());
    assert_eq!(app.saved_channel_work(), None);
}
