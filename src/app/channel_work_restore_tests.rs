use super::*;

fn fixture() -> (Cagents, Home) {
    let (mut app, home) = main_fixture();
    two_channels(&mut app, home);
    // A new application has its pane projection and catalog but has not opened
    // the Work surface or connected to the model yet.
    app.channel_work = State::default();
    app.workspace_model = WorkspaceModel::default();
    (app, home)
}

#[test]
fn saved_work_waits_for_model_then_restores_the_exact_channel_surface() {
    let (mut app, home) = fixture();
    drop(app.restore_channel_work(Some(home)));
    assert!(!app.channel_work_is_main());
    assert_eq!(app.saved_channel_work(), Some(home));
    // Scope promotion/catalog loading is passive startup work and may change
    // the workspace generation before the registry's first snapshot arrives.
    app.workspace_view_generation += 1;
    drop(app.apply_workspace_model_update(
        super::super::super::workspace_model_sync::ModelUpdate::Read(model(home)),
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
        let (mut app, home) = fixture();
        drop(app.restore_channel_work(Some(home)));
        app.workspace_model = model(home);
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
        let (mut app, home) = fixture();
        drop(app.restore_channel_work(Some(home)));
        if explicit_navigation {
            drop(
                app.intercept_channel_work_navigation(&Message::Sidebar(
                    SidebarAction::OpenSettings,
                )),
            );
        } else {
            drop(app.intercept_channel_work_navigation(&Message::RunCommand(
                workspace_widget::GlobalShortcut::CycleWorkspace(
                    crate::workspace::FocusDirection::Right,
                ),
            )));
        }
        app.workspace_model = model(home);
        drop(app.restore_pending_channel_work());
        assert!(!app.channel_work_is_main());
        assert_eq!(app.saved_channel_work(), None);
    }
}

#[test]
fn old_windows_without_a_saved_work_surface_stay_in_conversations() {
    let (mut app, home) = fixture();
    app.workspace_model = model(home);
    drop(app.restore_channel_work(None));
    assert!(!app.channel_work_is_main());
    assert_eq!(app.saved_channel_work(), None);
}
