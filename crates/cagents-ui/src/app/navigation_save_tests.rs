// Changes of where the window is are saved with the navigation burst rather
// than by a full workspace save on the UI thread; window-view state is saved
// where it lives, in settings.

use super::*;
use super::super::terminal_output_tests::persists;

/// Saves whatever the navigation burst holds, as its timer does.
fn finish_burst(app: &mut Cagents) {
    drop(app.update(Message::NavigationSaveDue(app.navigation_save_generation)));
    assert!(app.navigation_save.is_none());
}

/// The burst holds an unsaved navigation change and no save has run since
/// `saved`; its timer then saves the workspace exactly once.
fn assert_saved_with_the_burst(app: &mut Cagents, saved: usize, what: &str) {
    assert_eq!(persists(), saved, "{what} waits for the navigation burst");
    assert_eq!(app.navigation_save, Some(DeferredSave::Views), "{what}");
    finish_burst(app);
    assert_eq!(persists(), saved + 1, "{what} is saved once, by the burst");
}

#[test]
fn toggling_the_projects_sidebar_saves_the_window_view_not_the_workspace() {
    let directory = tempfile::tempdir().unwrap();
    let (session, _) = SessionRuntime::open(
        &directory.path().join("window.sqlite3"),
        crate::model::BootId::new("projects-sidebar-toggle"),
    )
    .unwrap();
    let store = session.operation_store();
    let window_view = || -> WindowView {
        serde_json::from_str(&store.load_window_view().unwrap().unwrap()).unwrap()
    };
    let mut app = context_inspector_test_app(true);
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    app.session = Some(session);
    let saved = persists();
    for collapsed in [true, false, true] {
        let task = app.update(Message::ChromeUi(chrome_ui::Action::ToggleProjects));
        assert_eq!(app.chrome_ui.projects_collapsed, collapsed);
        settle_settings_task(&mut app, task);
        assert_eq!(
            window_view().projects_sidebar_collapsed,
            collapsed,
            "the toggle reaches the window's saved view"
        );
    }
    assert_eq!(persists(), saved, "the workspace holds nothing it changed");
    assert!(app.navigation_save.is_none());
}

#[test]
fn revealing_a_pane_saves_the_unfolded_sidebar_with_the_window_view() {
    let directory = tempfile::tempdir().unwrap();
    let mut app = context_inspector_test_app(true);
    app.settings_path = Some(directory.path().join("settings.toml"));
    app.settings_write_blocked = false;
    let card = app.workspace.focused().unwrap();
    app.chrome_ui.projects_collapsed = true;
    let saved = persists();
    let generation = app.settings_save_generation;
    drop(app.update(Message::ChromeUi(chrome_ui::Action::Reveal(card))));
    assert!(!app.chrome_ui.projects_collapsed);
    assert!(app.settings_save_generation > generation, "the view is saved");
    assert!(!app.capture_settings_save().unwrap().view.projects_sidebar_collapsed);
    assert_eq!(persists(), saved, "revealing a row changes no workspace");
}

#[test]
fn opening_and_moving_settings_saves_with_the_navigation_burst() {
    let (mut app, channel) = alt_tab_test_app();
    drop(app.navigate_to_worktree("/repo".into(), false));
    finish_burst(&mut app);

    let saved = persists();
    drop(app.update(Message::Sidebar(SidebarAction::OpenSettings)));
    let settings = app.utility_card(&CardKind::Settings).expect("settings opened");
    assert_eq!(app.workspace.focused(), Some(settings));
    assert_saved_with_the_burst(&mut app, saved, "opening settings");

    drop(app.navigate_to_channel(channel));
    finish_burst(&mut app);
    assert!(app.workspace.order().all(|card| card != settings));
    let saved = persists();
    drop(app.update(Message::Sidebar(SidebarAction::OpenSettings)));
    assert_eq!(app.workspace.focused(), Some(settings), "the pane moved here");
    assert!(
        app.durable.channels[0].cards.contains(&settings),
        "the moved pane is part of the channel the next save writes"
    );
    assert_saved_with_the_burst(&mut app, saved, "moving settings");
}

/// The sidebar and the palette go to an open pane the same way, so each
/// leaves its save to the navigation burst.
#[test]
fn going_to_an_open_pane_saves_with_the_navigation_burst() {
    let entries: [(&str, fn(CardId) -> Message); 2] = [
        ("the sidebar", |card| {
            Message::Sidebar(SidebarAction::OpenCard { card })
        }),
        ("the palette", |card| {
            Message::Palette(palette_ui::Action::FocusPane(card))
        }),
    ];
    for (entry, open) in entries {
        let (mut app, channel) = alt_tab_test_app();
        drop(app.navigate_to_worktree("/repo".into(), false));
        finish_burst(&mut app);
        let feature = app.durable.cards[1].id;
        let channeled = app.durable.cards[2].id;

        let saved = persists();
        drop(app.update(open(feature)));
        assert_eq!(
            app.active_workspace.as_deref(),
            Some(Path::new("/repo/feature")),
            "{entry}"
        );
        assert_eq!(app.workspace.focused(), Some(feature), "{entry}");
        assert_saved_with_the_burst(
            &mut app,
            saved,
            &format!("a pane in another worktree, from {entry}"),
        );

        let saved = persists();
        drop(app.update(open(channeled)));
        assert_eq!(app.durable.active_channel, Some(channel), "{entry}");
        assert_eq!(app.workspace.focused(), Some(channeled), "{entry}");
        assert_saved_with_the_burst(
            &mut app,
            saved,
            &format!("a pane in a channel, from {entry}"),
        );
    }
}

/// An agent link whose session has not started opens the agent's channel,
/// which changes only where the window is.
#[test]
fn opening_an_agent_that_has_not_started_saves_its_channel_with_the_navigation_burst() {
    use crate::agent_execution::{Agent, Origin, Placement, Spec, Surface, Target};

    let (mut app, channel) = alt_tab_test_app();
    drop(app.navigate_to_worktree("/repo".into(), false));
    finish_burst(&mut app);
    let placement = Placement::Channel(channel);
    let agent = Agent {
        id: crate::model::AgentId::new(),
        name: None,
        origin: Origin::User,
        spec: Spec {
            provider: crate::agent::Provider::Claude,
            account: None,
            target: Target {
                repository: "/repo".into(),
                directory: "/repo".into(),
                placement: Some(placement.clone()),
            },
            surface: Surface::Chat,
            model: None,
            permission_mode: None,
            reasoning_effort: None,
        },
        placement,
        session: None,
        console: None,
        created_ms: 0,
        closed: false,
    };
    let prefix = agent.id.to_string();
    app.workspace_model.agents.push(agent);

    let saved = persists();
    drop(app.update(Message::ChatUi(chat_ui::Action::OpenAgent(prefix))));
    assert_eq!(app.durable.active_channel, Some(channel));
    assert_saved_with_the_burst(&mut app, saved, "an unstarted agent's channel");
}
