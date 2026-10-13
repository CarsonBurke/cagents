use super::*;
use crate::interaction::tooltip;

#[derive(Debug, Default)]
pub(super) struct State {
    hovered_tab: Option<CardId>,
    pub(super) closing_group: Vec<CardId>,
    pub(super) user_titles: HashMap<CardId, String>,
    /// Tab visibility follows pane focus even when a creation path already
    /// updated durable focus before applying its viewport changes.
    pub(super) tabs_focus: Option<CardId>,
    rename: Option<(RenameTarget, String)>,
    pub(super) workspace_preview: Option<SidebarAction>,
    /// When the hold that opened the workspace switcher began, which also
    /// names the timer that reveals it.
    pub(super) workspace_preview_since: Option<Instant>,
    /// A tap between the two most recent workspaces ends before the switcher
    /// would be read, so it is drawn only once the hold outlasts one.
    pub(super) workspace_switcher_shown: bool,
    /// Where the switcher's list was last scrolled to, so following the
    /// selection moves it no further than the selection requires.
    pub(super) workspace_switcher_scroll: f32,
    pub(super) workspace_switcher_rows: Vec<WorkspaceSwitcherRow>,
    pub(super) projects_collapsed: bool,
    popup: Option<Popup>,
    focus_index: usize,
    /// The shell's own Tab traversal, set aside while a popup owns Tab: the
    /// two would each move focus on one key press, and whichever landed last
    /// won.
    restore_keyboard_nav: Option<bool>,
    /// Last reported horizontal offset of the tab strip.
    ///
    /// The native scrollable only accepts absolute offsets, so revealing a tab
    /// without knowing where the strip already sits means every reveal snaps
    /// the tab to the viewport edge - including a reveal triggered by merely
    /// hovering an already-visible tab under focus-follows-mouse.
    tabs_offset: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Retain the actual Settings pane while rename and palette modals change.
    /// Apply runtime operations to every real control in this same tree.
    fn rename_fixture_view(app: &Cagents) -> Element<'_, Message> {
        widget::column::with_children(vec![
            app.card_view(app.workspace.focused().unwrap(), true, false),
            app.palette_surface()
                .unwrap_or_else(|| widget::Space::new().width(1).height(1).into()),
            app.pane_rename_surface()
                .unwrap_or_else(|| widget::Space::new().width(1).height(1).into()),
        ])
        .into()
    }

    fn rename_fixture_step(
        app: &Cagents,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        task: Task<Message>,
        type_text: bool,
    ) -> Vec<Message> {
        use iced::advanced::{Layout, Shell, clipboard, layout, mouse};
        use iced::{Event, Rectangle, Size, futures::StreamExt};

        let mut element = rename_fixture_view(app);
        tree.diff(element.as_widget_mut());
        let viewport = Rectangle::with_size(Size::new(1000.0, 1600.0));
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut messages = Vec::new();
        executor.block_on(async {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                // Apply actual immediate runtime effects. Destination changes
                // also schedule maintenance timers; they are not the focus
                // contract and need not finish before typing into the widget.
                while let Ok(Some(action)) =
                    tokio::time::timeout(Duration::from_millis(250), stream.next()).await
                {
                    match action {
                        iced::runtime::Action::Widget(operation) => {
                            let mut next = Some(operation);
                            while let Some(mut operation) = next.take() {
                                element.as_widget_mut().operate(
                                    tree,
                                    Layout::new(&node),
                                    renderer,
                                    operation.as_mut(),
                                );
                                if let iced::advanced::widget::operation::Outcome::Chain(chained) =
                                    operation.finish()
                                {
                                    next = Some(chained);
                                }
                            }
                        }
                        iced::runtime::Action::Output(cosmic::Action::App(message)) => {
                            messages.push(message);
                        }
                        iced::runtime::Action::Window(_) => {}
                        _ => panic!("unexpected runtime action in focus fixture"),
                    }
                }
            }
        });
        executor.shutdown_background();
        if type_text {
            element.as_widget_mut().update(
                tree,
                &Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character("x".into()),
                    modified_key: keyboard::Key::Character("x".into()),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyX),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: Some("x".into()),
                    repeat: false,
                }),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        messages
    }

    fn rename_test_app() -> Cagents {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let card = app.workspace.focused().unwrap();
        app.cards.insert(card, CardRuntime::Settings);
        app.card_state_mut(card).unwrap().kind = CardKind::Settings;
        app
    }

    #[test]
    fn rename_dismissal_restores_retained_settings_input_and_preserves_title_policy() {
        use iced::advanced::widget::Tree;

        for dismissal in 0..3 {
            let mut app = rename_test_app();
            let card = app.workspace.focused().unwrap();
            let original = app.card_state(card).unwrap().title.clone();
            let renderer = super::super::tests::headless_renderer();
            let mut tree = Tree::new(rename_fixture_view(&app).as_widget());
            let focus = iced::widget::operation::focus(crate::settings_widgets::search_id())
                .map(cosmic::Action::App);
            let before = rename_fixture_step(&app, &mut tree, &renderer, focus, true);
            assert!(before.iter().any(|message| matches!(
                message, Message::SettingsSearchChanged(value) if value == "x"
            )));

            let open = app.update_chrome(Action::Rename(card));
            let during = rename_fixture_step(&app, &mut tree, &renderer, open, true);
            assert!(during.iter().any(|message| matches!(
                message, Message::ChromeUi(Action::RenameChanged(value)) if value == "x"
            )));
            assert!(
                !during
                    .iter()
                    .any(|message| matches!(message, Message::SettingsSearchChanged(_)))
            );
            let _ = app.update_chrome(Action::RenameChanged(" New name ".into()));
            let close = match dismissal {
                0 => app
                    .chrome_key_event(&keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(keyboard::key::Named::Escape),
                        modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::empty(),
                        text: None,
                        repeat: false,
                    })
                    .expect("rename handles Escape"),
                1 => app.update_chrome(Action::RenameCancel),
                _ => app.update_chrome(Action::RenameSubmit),
            };
            assert!(app.pane_rename_surface().is_none());
            assert_eq!(
                app.card_state(card).unwrap().title,
                if dismissal == 2 {
                    "New name"
                } else {
                    &original
                }
            );
            let after = rename_fixture_step(&app, &mut tree, &renderer, close, true);
            assert!(after.iter().any(|message| matches!(
                message, Message::SettingsSearchChanged(value) if value == "x"
            )));
        }
    }

    #[test]
    fn channel_rename_submit_restores_the_current_settings_input() {
        use crate::model::{ChannelId, ChannelTitleSource, ProjectChannel};
        use iced::advanced::widget::Tree;

        let mut app = rename_test_app();
        let card = app.workspace.focused().unwrap();
        let channel = ChannelId::new();
        Arc::make_mut(&mut app.durable)
            .channels
            .push(ProjectChannel {
                id: channel,
                project_id: crate::model::ProjectId::new(),
                title: "Old channel".into(),
                title_source: ChannelTitleSource::Prompt,
                ordinal: 0,
                shared: false,
                cards: vec![card],
                focused: Some(card),
            });
        let renderer = super::super::tests::headless_renderer();
        let mut tree = Tree::new(rename_fixture_view(&app).as_widget());
        let open = app.update_chrome(Action::RenameChannel(channel));
        let during = rename_fixture_step(&app, &mut tree, &renderer, open, true);
        assert!(during.iter().any(|message| matches!(
            message, Message::ChromeUi(Action::RenameChanged(value)) if value == "x"
        )));
        let _ = app.update_chrome(Action::RenameChanged(" New channel ".into()));
        let submit = app.update_chrome(Action::RenameSubmit);
        assert!(app.pane_rename_surface().is_none());
        assert_eq!(app.workspace.focused(), Some(card));
        let state = app
            .durable
            .channels
            .iter()
            .find(|state| state.id == channel)
            .unwrap();
        assert_eq!(state.title, "New channel");
        assert_eq!(state.title_source, ChannelTitleSource::Manual);
        let after = rename_fixture_step(&app, &mut tree, &renderer, submit, true);
        assert!(after.iter().any(|message| matches!(
            message, Message::SettingsSearchChanged(value) if value == "x"
        )));
    }

    #[test]
    fn blank_rename_submit_keeps_actual_input_focus() {
        use iced::advanced::widget::Tree;

        let mut app = rename_test_app();
        let card = app.workspace.focused().unwrap();
        let renderer = super::super::tests::headless_renderer();
        let mut tree = Tree::new(rename_fixture_view(&app).as_widget());
        let open = app.update_chrome(Action::Rename(card));
        let _ = rename_fixture_step(&app, &mut tree, &renderer, open, false);
        let _ = app.update_chrome(Action::RenameChanged(" ".into()));
        let submit = app.update_chrome(Action::RenameSubmit);
        assert!(app.pane_rename_surface().is_some());
        let after = rename_fixture_step(&app, &mut tree, &renderer, submit, true);
        assert!(after.iter().any(|message| matches!(
            message, Message::ChromeUi(Action::RenameChanged(value)) if value.ends_with('x')
        )));
        assert!(
            !after
                .iter()
                .any(|message| matches!(message, Message::SettingsSearchChanged(_)))
        );
    }

    #[test]
    fn stale_rename_cancel_does_not_steal_focus_from_a_new_palette() {
        use iced::advanced::widget::Tree;

        let mut app = rename_test_app();
        let renderer = super::super::tests::headless_renderer();
        let mut tree = Tree::new(rename_fixture_view(&app).as_widget());
        let open = app.update_palette(palette_ui::Action::Open);
        let _ = rename_fixture_step(&app, &mut tree, &renderer, open, false);
        let late_cancel = app.update_chrome(Action::RenameCancel);
        let after = rename_fixture_step(&app, &mut tree, &renderer, late_cancel, true);
        assert!(after.iter().any(|message| matches!(
            message, Message::Palette(palette_ui::Action::Query(value)) if value == "x"
        )));
        assert!(
            !after
                .iter()
                .any(|message| matches!(message, Message::SettingsSearchChanged(_)))
        );
    }

    #[test]
    fn blank_rename_keeps_the_draft_open_until_it_is_corrected() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let id = app.workspace.focused().unwrap();
        let original = app.card_state(id).unwrap().title.clone();
        for draft in ["", "   ", "\t\n"] {
            for target in [
                RenameTarget::Pane(id),
                RenameTarget::Channel(crate::model::ChannelId::new()),
            ] {
                app.chrome_ui.rename = Some((target, draft.into()));
                drop(app.update_chrome(Action::RenameSubmit));
                assert_eq!(app.chrome_ui.rename.as_ref().unwrap().1, draft);
                assert_eq!(app.card_state(id).unwrap().title, original);
            }
        }
        app.chrome_ui.rename = Some((RenameTarget::Pane(id), original));
        drop(app.update_chrome(Action::RenameChanged(" New name ".into())));
        drop(app.update_chrome(Action::RenameSubmit));
        assert!(app.chrome_ui.rename.is_none());
        assert_eq!(app.card_state(id).unwrap().title, "New name");
    }

    #[test]
    fn sidebar_toggle_keeps_the_users_choice_across_window_resizes() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        for width in [900.0, 1199.0, 1200.0, 1600.0] {
            let _ = app.update(Message::WindowResized(width, 900.0));
            assert_eq!(app.projects_sidebar_mode(), SidebarMode::Expanded);
            let _ = app.update_chrome(Action::ToggleProjects);
            assert_eq!(app.projects_sidebar_mode(), SidebarMode::Compact);
            let _ = app.update(Message::WindowResized(1600.0, 900.0));
            assert_eq!(app.projects_sidebar_mode(), SidebarMode::Compact);
            let _ = app.update(Message::WindowResized(width, 900.0));
            let _ = app.update_chrome(Action::ToggleProjects);
            assert_eq!(app.projects_sidebar_mode(), SidebarMode::Expanded);
        }
    }

    #[test]
    fn sidebar_toggle_synchronizes_the_inspector_overlay() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let _ = app.update(Message::WindowResized(1000.0, 900.0));
        assert!(app.context_inspector_overlay_active);
        let _ = app.update_chrome(Action::ToggleProjects);
        assert_eq!(app.context_inspector_mount(), Some(InspectorLayout::Docked));
        assert!(!app.context_inspector_overlay_active);
        assert!(!app.workspace_inputs_blocked());
        let _ = app.update_chrome(Action::ToggleProjects);
        assert_eq!(
            app.context_inspector_mount(),
            Some(InspectorLayout::Overlay)
        );
        assert!(app.context_inspector_overlay_active);
        assert!(app.workspace_inputs_blocked());
    }

    #[test]
    fn revealing_a_card_expands_the_sidebar_and_synchronizes_the_inspector() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let _ = app.update(Message::WindowResized(1000.0, 900.0));
        let _ = app.update_chrome(Action::ToggleProjects);
        assert!(!app.context_inspector_overlay_active);
        let _ = app.update_chrome(Action::Reveal(card));
        assert_eq!(app.projects_sidebar_mode(), SidebarMode::Expanded);
        assert!(app.context_inspector_overlay_active);
    }

    #[test]
    fn showing_a_switch_failure_keeps_its_explanation_open() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.notices
            .record(TransientNotice::error("Native attachment failed."));
        let _ = app.update_chrome(Action::ShowNoticesFor(card));
        assert_eq!(app.chrome_ui.popup, Some(Popup::Notices));
        let focus = app.chrome_ui.focus_index;
        let _ = app.update_chrome(Action::ShowNoticesFor(card));
        assert_eq!(app.chrome_ui.popup, Some(Popup::Notices));
        assert_eq!(app.chrome_ui.focus_index, focus);
        let _ = app.update_chrome(Action::Close);
        assert_eq!(app.chrome_ui.popup, None);
    }

    #[test]
    fn a_late_switch_failure_does_not_take_over_another_pane_or_dialog() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let source = app.workspace.focused().unwrap();
        app.notices
            .record(TransientNotice::error("Native attachment failed."));
        let next = super::super::tests::terminal_card_state();
        let destination = next.id;
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards.push(next);
        durable.focused = Some(destination);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        let _ = app.update_chrome(Action::ShowNoticesFor(source));
        assert_eq!(app.chrome_ui.popup, None);
        assert_eq!(app.workspace.focused(), Some(destination));

        app.settings_reset_confirm = true;
        let _ = app.update_chrome(Action::ShowNoticesFor(destination));
        assert_eq!(app.chrome_ui.popup, None);
        assert!(app.settings_reset_confirm);
        app.settings_reset_confirm = false;
        let _ = app.update_chrome(Action::ToggleNewTab);
        let _ = app.update_chrome(Action::ShowNoticesFor(destination));
        assert_eq!(app.chrome_ui.popup, Some(Popup::NewTab));
    }

    #[test]
    fn restored_codex_status_is_an_icon_not_a_tab_or_window_name() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let id = app.workspace.focused().unwrap();
        for (title, expected) in [
            ("[ . ] Action Required | Fix parser", "Fix parser"),
            ("[ ! ] Action Required | Fix parser", "Fix parser"),
            ("[ ! ] Action Required", "Codex"),
        ] {
            let card = app.card_state_mut(id).unwrap();
            let CardKind::Terminal(session) = &mut card.kind else {
                unreachable!();
            };
            session.launch.program = "codex".into();
            card.title = title.into();
            drop(app.sync_chrome());
            assert_eq!(app.card_chrome[&id].title, expected);
            assert_eq!(app.core.window.header_title, expected);
        }
        let custom = "[ ! ] Action Required documentation";
        app.chrome_ui.user_titles.insert(id, custom.into());
        drop(app.sync_chrome());
        assert_eq!(app.card_chrome[&id].title, custom);
        assert_eq!(app.core.window.header_title, custom);
    }

    #[test]
    fn tab_reaches_every_control_in_the_notices_popup() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.notices.record(TransientNotice::warning("first"));
        app.notices.record(TransientNotice::error("second"));
        let _ = app.update_chrome(Action::ToggleNotices);
        let ring = app.popup_focus_ring(Popup::Notices);
        let ids: Vec<String> = app
            .notices
            .recent()
            .map(|entry| notice_dismiss_id(entry.id))
            .collect();
        assert_eq!(
            ring,
            [
                NOTICES_DISMISS_ALL_ID,
                NOTICES_CLOSE_ID,
                ids[0].as_str(),
                ids[1].as_str()
            ]
        );
        // Opening lands on close, and Tab walks on to each notice's Dismiss.
        assert_eq!(ring[app.chrome_ui.focus_index], NOTICES_CLOSE_ID);
        let tab = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Tab),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Tab),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Tab),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let mut visited = Vec::new();
        for _ in 0..ring.len() {
            assert!(app.chrome_key_event(&tab).is_some());
            visited.push(ring[app.chrome_ui.focus_index].clone());
        }
        assert_eq!(
            visited,
            [
                ids[0].as_str(),
                ids[1].as_str(),
                NOTICES_DISMISS_ALL_ID,
                NOTICES_CLOSE_ID
            ]
        );

        // Dismissing the last notice from the keyboard keeps focus in the
        // popup, on what took its place.
        app.chrome_ui.focus_index = 3;
        let last = app.notices.recent().last().unwrap().id;
        let _ = app.update(Message::DismissNotice(last));
        assert_eq!(app.chrome_ui.popup, Some(Popup::Notices));
        assert_eq!(app.chrome_ui.focus_index, 2);
    }

    #[test]
    fn an_open_popup_owns_tab_until_it_closes() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.notices.record(TransientNotice::warning("first"));
        app.core.set_keyboard_nav(true);
        let _ = app.update(Message::ChromeUi(Action::ToggleNotices));
        // The shell's traversal would move focus a second time per Tab.
        assert!(!app.core.keyboard_nav());
        let _ = app.update(Message::ChromeUi(Action::Close));
        assert!(app.core.keyboard_nav());

        // A shell that had its traversal off keeps it off.
        app.core.set_keyboard_nav(false);
        let _ = app.update(Message::ChromeUi(Action::ToggleNewTab));
        let _ = app.update(Message::ChromeUi(Action::Close));
        assert!(!app.core.keyboard_nav());
    }

    #[test]
    fn terminal_activation_leaves_enter_for_the_input_surface() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Rectangle, Size};

        let app = super::super::tests::context_inspector_test_app(true);
        let renderer = super::super::tests::headless_renderer();
        let card = app.workspace.focused().unwrap();
        assert!(matches!(
            app.card_state(card).unwrap().kind,
            CardKind::Terminal(_)
        ));
        for (target, title_activated) in [
            (workspace_widget::card_focus_id(card), false),
            (card_control_id(card, "title"), true),
        ] {
            let mut header = app.column_header(card, true, 320.0);
            let mut tree = Tree::new(header.as_widget());
            let viewport = Rectangle::with_size(Size::new(320.0, 40.0));
            let node = header.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            header.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut iced::advanced::widget::operation::focusable::focus::<()>(target),
            );
            let event = Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            header.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            assert_eq!(shell.is_event_captured(), title_activated);
            drop(shell);
            assert_eq!(
                messages
                    .iter()
                    .any(|message| matches!(message, Message::FocusCard(id) if *id == card)),
                title_activated,
            );
        }
    }

    #[test]
    fn chat_column_title_does_not_adopt_the_terminal_focus_id() {
        use cosmic::iced::advanced::{Layout, Shell, clipboard, layout, mouse, widget::Tree};
        use cosmic::iced::{Event, Rectangle, Size};

        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let state = Arc::make_mut(&mut app.durable)
            .cards
            .iter_mut()
            .find(|state| state.id == card)
            .unwrap();
        let CardKind::Terminal(terminal) = &mut state.kind else {
            unreachable!();
        };
        terminal.launch.program = "codex".into();
        let session = chat_ui::conversion(terminal).expect("codex terminal converts");
        state.kind = CardKind::Chat(session);

        let renderer = super::super::tests::headless_renderer();
        for (target, title_activated) in [
            (workspace_widget::card_focus_id(card), false),
            (card_control_id(card, "title"), true),
        ] {
            let mut header = app.column_header(card, true, 320.0);
            let mut tree = Tree::new(header.as_widget());
            let viewport = Rectangle::with_size(Size::new(320.0, 40.0));
            let node = header.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, viewport.size()),
            );
            header.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut iced::advanced::widget::operation::focusable::focus::<()>(target.clone()),
            );
            let event = Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Enter),
                modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat: false,
            });
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            header.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &viewport,
            );
            assert_eq!(
                shell.is_event_captured(),
                title_activated,
                "focus target {target:?} should activate the title: {title_activated}"
            );
        }
    }
}

impl State {
    pub(super) fn with_user_titles(mut self, titles: HashMap<CardId, String>) -> Self {
        self.user_titles = titles;
        self
    }
    pub(super) fn with_projects_collapsed(mut self, collapsed: bool) -> Self {
        self.projects_collapsed = collapsed;
        self
    }
    pub(super) fn is_open(&self) -> bool {
        self.popup.is_some() || self.rename.is_some()
    }

    pub(super) fn close(&mut self) {
        self.popup = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Popup {
    NewTab,
    Notices,
}

#[derive(Debug, Clone, Copy)]
pub enum NewTab {
    Terminal,
    Thread,
    Resume,
    Browser,
}

// Threads choose their model in the composer; backend details are automatic.
const NEW_TAB_CHOICES: [(&str, &str, NewTab, &str); 4] = [
    (
        "New thread",
        "Choose a model and start a conversation",
        NewTab::Thread,
        "chrome.new-tab.thread",
    ),
    (
        "Resume conversation",
        "Search saved conversations across projects and accounts",
        NewTab::Resume,
        "chrome.new-tab.resume",
    ),
    (
        "Terminal",
        "Shell in the current worktree",
        NewTab::Terminal,
        "chrome.new-tab.terminal",
    ),
    (
        "Browser",
        "Open a browser tab",
        NewTab::Browser,
        "chrome.new-tab.browser",
    ),
];

#[derive(Debug, Clone)]
pub enum Action {
    HoverTab(Option<CardId>),
    RenameChannel(crate::model::ChannelId),
    Rename(CardId),
    RenameChanged(String),
    RenameSubmit,
    RenameCancel,
    CloseOthers(CardId),
    CloseToRight(CardId),
    Clone(CardId),
    CopyPath(CardId),
    Reveal(CardId),
    ToggleProjects,
    ToggleNewTab,
    ToggleNotices,
    ShowNoticesFor(CardId),
    Close,
    TabsScrolled(f32),
    Launch(NewTab),
}

#[derive(Debug, Clone, Copy)]
enum RenameTarget {
    Pane(CardId),
    Channel(crate::model::ChannelId),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PaneMenuCommand {
    Rename,
    Clone,
    CopyPath,
    Reveal,
    CloseOthers,
    CloseToRight,
    Close,
    Switch,
    Fork(chat_ui::Target),
    ReviewChanges(chat_ui::Target),
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct PaneMenu(CardId, PaneMenuCommand);

impl widget::menu::Action for PaneMenu {
    type Message = Message;
    fn message(&self) -> Message {
        let id = self.0;
        Message::ChromeUi(match self.1 {
            PaneMenuCommand::Rename => Action::Rename(id),
            PaneMenuCommand::Clone => Action::Clone(id),
            PaneMenuCommand::CopyPath => Action::CopyPath(id),
            PaneMenuCommand::Reveal => Action::Reveal(id),
            PaneMenuCommand::CloseOthers => Action::CloseOthers(id),
            PaneMenuCommand::CloseToRight => Action::CloseToRight(id),
            PaneMenuCommand::Close => return Message::CloseCard(id),
            PaneMenuCommand::Switch => return Message::ChatUi(chat_ui::Action::Switch(id)),
            PaneMenuCommand::Fork(target) => return Message::ChatUi(chat_ui::Action::BranchPicker(target, true)),
            PaneMenuCommand::ReviewChanges(target) => return Message::ChatUi(chat_ui::Action::ChangesPicker(target, true)),
        })
    }
}

// Tabs retain a neutral keyboard focus cue without an accent outline.
fn column_title_style() -> cosmic::theme::Button {
    fn appearance(focused: bool, theme: &cosmic::Theme) -> widget::button::Style {
        widget::button::Style {
            border_radius: theme.cosmic().corner_radii.radius_s.into(),
            // The tab owns its selection surface. Keep its mark and title
            // inline rather than painting another button pill inside it.
            border_width: if focused { 1.0 } else { 0.0 },
            border_color: iced::Color::from(theme.current_container().on).scale_alpha(0.18),
            icon_color: Some(design::muted_icon_color(theme)),
            ..widget::button::Style::new()
        }
    }
    cosmic::theme::Button::Custom {
        active: Box::new(appearance),
        disabled: Box::new(|_| widget::button::Style::new()),
        hovered: Box::new(appearance),
        pressed: Box::new(appearance),
    }
}

fn column_tab_style(
    theme: &cosmic::Theme,
    selected: bool,
    hovered: bool,
) -> iced::widget::container::Style {
    let component = &theme.current_container().component;
    iced::widget::container::Style {
        background: if selected {
            Some(iced::Background::Color(
                component.selected_state_color().into(),
            ))
        } else if hovered {
            Some(iced::Background::Color(component.hover.into()))
        } else {
            None
        },
        border: iced::Border {
            radius: theme.cosmic().corner_radii.radius_s.into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[cfg(test)]
mod tab_style_tests {
    #[test]
    fn whole_tab_hover_is_visible_and_preserves_selection() {
        let theme = cosmic::Theme::dark();
        assert!(
            super::column_tab_style(&theme, false, false)
                .background
                .is_none()
        );
        assert!(
            super::column_tab_style(&theme, false, true)
                .background
                .is_some()
        );
        assert_eq!(
            super::column_tab_style(&theme, true, true).background,
            super::column_tab_style(&theme, true, false).background,
        );
    }

    #[test]
    fn inline_tab_mark_and_title_do_not_paint_a_nested_hover_button() {
        let cosmic::theme::Button::Custom {
            active,
            hovered,
            pressed,
            ..
        } = super::column_title_style()
        else {
            unreachable!();
        };
        let theme = cosmic::Theme::dark();
        for appearance in [active, hovered, pressed] {
            assert!(appearance(false, &theme).background.is_none());
            let focused = appearance(true, &theme);
            assert!(focused.background.is_none());
            assert!(
                focused.border_width > 0.0,
                "keyboard focus retains a visible cue"
            );
        }
    }
}

const NOTICES_CLOSE_ID: &str = "chrome.notices.close";
const NOTICES_DISMISS_ALL_ID: &str = "chrome.notices.dismiss";

fn notice_dismiss_id(id: u64) -> String {
    format!("chrome.notices.dismiss.{id}")
}

fn command<'a>(
    icon: impl Into<widget::icon::Handle>,
    label: impl Into<Cow<'a, str>>,
    id: widget::Id,
    action: Option<Message>,
) -> Element<'a, Message> {
    crate::ui_controls::icon_button(icon.into(), label)
        .on_press_maybe(action)
        .id(id)
        .into()
}

impl Cagents {
    fn dismiss_rename(&mut self) -> Task<Message> {
        if self.chrome_ui.rename.take().is_none() {
            return Task::none();
        }
        self.workspace.focused().map_or_else(Task::none, |card| {
            iced::widget::operation::focus(self.card_keyboard_focus(card)).map(cosmic::Action::App)
        })
    }

    /// Keep Tab traversal and global shortcuts inside the open popup.
    pub(super) fn chrome_key_event(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        if self.chrome_ui.rename.is_some() {
            if is_escape_press(event) {
                return Some(self.update_chrome(Action::RenameCancel));
            }
            return Some(Task::none());
        }
        let popup = self.chrome_ui.popup?;
        if self.modal_is_open() {
            self.chrome_ui.close();
            return None;
        }
        if is_escape_press(event) {
            return Some(self.update_chrome(Action::Close));
        }
        let backwards = is_tab_press(event).or_else(|| match event {
            keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(key),
                modifiers,
                ..
            } if popup == Popup::NewTab && modifiers.is_empty() => match key {
                keyboard::key::Named::ArrowUp => Some(true),
                keyboard::key::Named::ArrowDown => Some(false),
                _ => None,
            },
            _ => None,
        });
        if let Some(backwards) = backwards {
            let ring = self.popup_focus_ring(popup);
            let count = ring.len();
            self.chrome_ui.focus_index = if backwards {
                (self.chrome_ui.focus_index.min(count - 1) + count - 1) % count
            } else {
                (self.chrome_ui.focus_index + 1) % count
            };
            return Some(self.focus_popup_control(ring));
        }
        Some(Task::none())
    }

    /// Every control the open popup draws, in the order it draws them, so
    /// Tab reaches each one - including a notice's own Dismiss and the
    /// update's Review - rather than a fixed subset. Never empty: the notices
    /// popup always has its close control.
    fn popup_focus_ring(&self, popup: Popup) -> Vec<Cow<'static, str>> {
        match popup {
            Popup::NewTab => NEW_TAB_CHOICES
                .iter()
                .map(|(_, _, _, id)| Cow::Borrowed(*id))
                .collect(),
            Popup::Notices => {
                let mut ring = Vec::new();
                if !self.notices.is_empty() {
                    ring.push(Cow::Borrowed(NOTICES_DISMISS_ALL_ID));
                }
                ring.push(Cow::Borrowed(NOTICES_CLOSE_ID));
                ring.extend(
                    self.notices
                        .recent()
                        .map(|entry| Cow::Owned(notice_dismiss_id(entry.id))),
                );
                // In the order the notices draw them: the lane's own notice,
                // then the refused writes. Only one of them offers a retry.
                let health = self.save_health.as_ref();
                if health.is_some_and(|health| {
                    matches!(health.health, SessionSaveHealth::Recovered { .. })
                }) {
                    ring.push(Cow::Borrowed("session-save.acknowledge-recovery"));
                }
                if health.is_some_and(SaveHealthState::offers_retry) {
                    ring.push(Cow::Borrowed("session-save.retry"));
                }
                if self.refused_terminal_recovery_target().is_some() {
                    ring.push(Cow::Borrowed("session-recovery.open"));
                }
                ring
            }
        }
    }

    /// Focuses the ring's control at `focus_index`, or its last one when the
    /// ring has shrunk under it.
    fn focus_popup_control(&mut self, ring: Vec<Cow<'static, str>>) -> Task<Message> {
        let Some(last) = ring.len().checked_sub(1) else {
            return Task::none();
        };
        self.chrome_ui.focus_index = self.chrome_ui.focus_index.min(last);
        let id = ring[self.chrome_ui.focus_index].clone();
        iced::widget::operation::focus(widget::Id::new(id)).map(cosmic::Action::App)
    }

    /// Gives an open popup sole ownership of Tab, and hands it back to the
    /// shell's traversal once the popup is gone or a dialog has taken over.
    pub(super) fn sync_popup_keyboard_nav(&mut self) {
        let owned = (self.chrome_ui.popup.is_some() || self.chat_picker_owns_keyboard())
            && !self.modal_is_open();
        match (owned, self.chrome_ui.restore_keyboard_nav) {
            (true, None) => {
                self.chrome_ui.restore_keyboard_nav = Some(self.core.keyboard_nav());
                self.core.set_keyboard_nav(false);
            }
            (false, Some(restore)) => {
                self.chrome_ui.restore_keyboard_nav = None;
                // A close dialog that opened over the popup recorded the
                // popup's suppressed traversal as its own restore point, and
                // it owns Tab now: hand it the reader's real choice instead.
                if !self.adopt_terminal_close_keyboard_nav(restore)
                    && !self.adopt_daemon_health_keyboard_nav(restore)
                    && !self.adopt_automation_approval_keyboard_nav(restore)
                    && !self.adopt_session_recovery_keyboard_nav(restore)
                {
                    self.core.set_keyboard_nav(restore);
                }
            }
            _ => {}
        }
    }

    /// Keeps keyboard focus inside the notices popup after a notice leaves
    /// it, on the control that took the dismissed one's place.
    pub(super) fn refocus_notices(&mut self) -> Task<Message> {
        if self.chrome_ui.popup != Some(Popup::Notices) || self.modal_is_open() {
            return Task::none();
        }
        let ring = self.popup_focus_ring(Popup::Notices);
        self.focus_popup_control(ring)
    }

    pub(super) fn update_chrome(&mut self, action: Action) -> Task<Message> {
        if self.modal_is_open() && !matches!(action, Action::Close) {
            return Task::none();
        }
        if let Action::ShowNoticesFor(card) = &action
            && (self.workspace.focused() != Some(*card)
                || self
                    .chrome_ui
                    .popup
                    .is_some_and(|popup| popup != Popup::Notices))
        {
            return Task::none();
        }
        match action {
            Action::HoverTab(card) => {
                self.chrome_ui.hovered_tab = card;
                Task::none()
            }
            Action::RenameChannel(id) => {
                let Some(channel) = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.id == id)
                else {
                    return Task::none();
                };
                self.chrome_ui.rename = Some((RenameTarget::Channel(id), channel.title.clone()));
                iced::widget::operation::focus(widget::Id::new("chrome.rename.input"))
                    .chain(iced::widget::operation::select_all(widget::Id::new(
                        "chrome.rename.input",
                    )))
                    .map(cosmic::Action::App)
            }
            Action::Rename(id) => {
                let Some(card) = self.card_state(id) else {
                    return Task::none();
                };
                self.chrome_ui.rename = Some((
                    RenameTarget::Pane(id),
                    self.card_display_title(card).to_owned(),
                ));
                iced::widget::operation::focus(widget::Id::new("chrome.rename.input"))
                    .chain(iced::widget::operation::select_all(widget::Id::new(
                        "chrome.rename.input",
                    )))
                    .map(cosmic::Action::App)
            }
            Action::RenameChanged(value) => {
                if let Some((_, draft)) = &mut self.chrome_ui.rename {
                    *draft = value;
                }
                Task::none()
            }
            Action::RenameCancel => self.dismiss_rename(),
            Action::RenameSubmit => {
                let Some((target, draft)) = self.chrome_ui.rename.as_ref() else {
                    return Task::none();
                };
                let Some(title) = normalize_card_name(&draft) else {
                    return Task::none();
                };
                let target = *target;
                let restore = self.dismiss_rename();
                let id = match target {
                    RenameTarget::Pane(id) => id,
                    RenameTarget::Channel(id) => {
                        if let Some(channel) = Arc::make_mut(&mut self.durable)
                            .channels
                            .iter_mut()
                            .find(|channel| channel.id == id)
                        {
                            channel.title = title.clone();
                            channel.title_source = crate::model::ChannelTitleSource::Manual;
                        }
                        self.publish_model_edit(
                            crate::workspace_model::ModelEdit::RetitleChannel { id, title },
                        );
                        self.refresh_sidebar_channels();
                        self.persist();
                        return restore;
                    }
                };
                self.chrome_ui.user_titles.insert(id, title.clone());
                if let Some(card) = self.card_state_mut(id) {
                    card.title = title.clone();
                }
                let presentation = self
                    .card_state(id)
                    .map(|card| self.card_display_title(card).to_owned())
                    .unwrap_or(title);
                let _ = self.workspace.rename_card(id, presentation);
                self.persist();
                restore.chain(self.save_settings_task(false))
            }
            Action::CloseOthers(id) | Action::CloseToRight(id) => {
                let to_right = matches!(action, Action::CloseToRight(_));
                let order: Vec<_> = self.workspace.order().collect();
                let index = order
                    .iter()
                    .position(|candidate| *candidate == id)
                    .unwrap_or(order.len());
                self.chrome_ui.closing_group = order
                    .into_iter()
                    .enumerate()
                    .filter(|(position, candidate)| {
                        *candidate != id && (!to_right || *position > index)
                    })
                    .map(|(_, id)| id)
                    .collect();
                let dirty = self.chrome_ui.closing_group.iter().copied().filter(|id| matches!(self.cards.get(id), Some(CardRuntime::Repository(runtime)) if runtime.blocks_navigation())).collect();
                self.begin_dirty_close(DirtyCloseIntent::PaneGroup, dirty)
            }
            Action::Clone(id) => {
                let focus = self.update(Message::FocusCard(id));
                if self.workspace.focused() == Some(id) {
                    Task::batch([focus, self.clone_focused_pane()])
                } else {
                    focus
                }
            }
            Action::CopyPath(id) => match self.cards.get(&id) {
                Some(CardRuntime::Repository(runtime)) => runtime
                    .selected_path()
                    .map(|path| runtime.descriptor.worktree.join(path))
                    .or_else(|| Some(runtime.descriptor.worktree.clone())),
                _ => self
                    .card_state(id)
                    .and_then(super::worktree_ui::card_directory)
                    .map(Path::to_path_buf),
            }
            .map_or_else(Task::none, |path| {
                iced::clipboard::write(path.display().to_string())
            }),
            Action::Reveal(id) => {
                let was_collapsed = self.chrome_ui.projects_collapsed;
                // FocusCard must see the new mounting decision so it does not
                // focus a workspace widget underneath an inspector overlay.
                self.chrome_ui.projects_collapsed = false;
                let focus = self.update(Message::FocusCard(id));
                if self.workspace.focused() != Some(id) {
                    self.chrome_ui.projects_collapsed = was_collapsed;
                    let overlay = self.sync_context_overlay_state();
                    return Task::batch([focus, overlay]);
                }
                let overlay = self.sync_context_overlay_state();
                self.synchronize_sidebar_selection();
                let destination = self.active_workspace_destination();
                let target = self.sidebar_model.projects.iter().find_map(|project| {
                    let row = match destination {
                        Some(WorkspaceDestination::Channel(id))
                            if project.channels.iter().any(|channel| channel.id == id) =>
                        {
                            sidebar::SelectionRow::Channel(id)
                        }
                        Some(WorkspaceDestination::Worktree(id))
                            if project.worktrees.iter().any(|worktree| worktree.id == id) =>
                        {
                            sidebar::SelectionRow::Worktree(project.id, id)
                        }
                        Some(WorkspaceDestination::Folder(id)) if project.id == id => {
                            sidebar::SelectionRow::Project(id)
                        }
                        _ => return None,
                    };
                    Some((project.id, row))
                });
                if let Some((project, row)) = target {
                    self.sidebar_state.reveal_project(project);
                    self.persist();
                    let reveal = if self.context_inspector_is_overlay() {
                        Task::none()
                    } else {
                        sidebar::focus_and_reveal(sidebar::selection_row_focus_id(row))
                            .map(cosmic::Action::App)
                    };
                    Task::batch([focus, reveal, overlay])
                } else {
                    Task::batch([focus, overlay])
                }
            }
            Action::ToggleProjects => {
                self.chrome_ui.projects_collapsed = !self.chrome_ui.projects_collapsed;
                self.persist();
                self.sync_context_overlay_state()
            }
            Action::ToggleNewTab | Action::ToggleNotices | Action::ShowNoticesFor(_) => {
                let popup = if matches!(action, Action::ToggleNewTab) {
                    Popup::NewTab
                } else {
                    Popup::Notices
                };
                if self.chrome_ui.popup == Some(popup) {
                    if matches!(action, Action::ShowNoticesFor(_)) {
                        return Task::none();
                    }
                    return self.update_chrome(Action::Close);
                }
                self.chrome_ui.popup = Some(popup);
                // The notices popup opens on its close control, which is not
                // the first stop when "Dismiss all" precedes it.
                let ring = self.popup_focus_ring(popup);
                self.chrome_ui.focus_index = match popup {
                    Popup::NewTab => 0,
                    Popup::Notices => ring
                        .iter()
                        .position(|id| id == NOTICES_CLOSE_ID)
                        .unwrap_or_default(),
                };
                self.focus_popup_control(ring)
            }
            Action::Close => {
                let popup = self.chrome_ui.popup.take();
                popup.map_or_else(Task::none, |popup| {
                    iced::widget::operation::focus(widget::Id::new(match popup {
                        Popup::NewTab => TOOLBAR_FOCUS_ID,
                        Popup::Notices => "chrome.notices",
                    }))
                    .map(cosmic::Action::App)
                })
            }
            Action::Launch(kind) => {
                self.chrome_ui.close();
                match kind {
                    NewTab::Terminal => cosmic::task::message(Message::NewTerminal),
                    NewTab::Thread => self.new_native_thread(),
                    NewTab::Resume => self.update_resume(resume_ui::Action::Open),
                    NewTab::Browser => cosmic::task::message(Message::NewBrowser),
                }
            }
            Action::TabsScrolled(offset) => {
                self.chrome_ui.tabs_offset = offset;
                Task::none()
            }
        }
    }

    fn new_tab_control(&self) -> Element<'_, Message> {
        let enabled = !self.modal_is_open() && !self.context_inspector_is_overlay();
        let control = command(
            Icon::Plus,
            format!(
                "New tab · Thread {} · Terminal {}",
                workspace_widget::shortcut_chords(
                    workspace_widget::GlobalShortcut::NewThread,
                    &self.settings.keyboard_shortcuts
                ),
                workspace_widget::shortcut_chords(
                    workspace_widget::GlobalShortcut::NewTerminal,
                    &self.settings.keyboard_shortcuts
                )
            ),
            widget::Id::new(TOOLBAR_FOCUS_ID),
            enabled.then_some(Message::ChromeUi(Action::ToggleNewTab)),
        );
        let content = (self.chrome_ui.popup == Some(Popup::NewTab) && enabled).then(|| {
            let items = NEW_TAB_CHOICES
                .into_iter()
                .map(|(label, description, kind, id)| {
                    let icon = match kind {
                        NewTab::Terminal => Icon::Terminal.handle(),
                        NewTab::Thread | NewTab::Resume => Icon::Chat.handle(),
                        NewTab::Browser => Icon::Browser.handle(),
                    };
                    let tooltip = match kind {
                        NewTab::Thread => workspace_widget::shortcut_tooltip(
                            workspace_widget::GlobalShortcut::NewThread,
                            &self.settings.keyboard_shortcuts,
                        ),
                        NewTab::Terminal => workspace_widget::shortcut_tooltip(
                            workspace_widget::GlobalShortcut::NewTerminal,
                            &self.settings.keyboard_shortcuts,
                        ),
                        NewTab::Browser | NewTab::Resume => description.to_owned(),
                    };
                    crate::interaction::tooltip(
                        crate::ui_controls::popover_item(
                            icon,
                            label,
                            description,
                            Message::ChromeUi(Action::Launch(kind)),
                        )
                        .id(widget::Id::new(id))
                        .name(format!("New {label} tab")),
                        tooltip,
                    )
                    .into()
                });
            widget::column::with_children(vec![
                widget::container(design::heading("New tab"))
                    .padding([6.0, space::S])
                    .into(),
                crate::design::horizontal_rule().into(),
                widget::column::with_children(items)
                    .spacing(space::XXS)
                    .into(),
            ])
            .spacing(6)
            .width(design::COMMAND_POPOVER_WIDTH)
            .into()
        });
        crate::ui_controls::popover(
            control,
            content,
            widget::popover::Position::Bottom,
            Message::ChromeUi(Action::Close),
        )
    }

    fn has_chrome_notices(&self) -> bool {
        !self.notices.is_empty()
            || self
                .save_health
                .as_ref()
                .is_some_and(SaveHealthState::is_visible)
    }

    pub(super) fn reconcile_notice_popup(&mut self) -> Task<Message> {
        if self.chrome_ui.popup == Some(Popup::Notices) && !self.has_chrome_notices() {
            self.chrome_ui.close();
            iced::widget::operation::focus(context_inspector_toggle_id()).map(cosmic::Action::App)
        } else {
            Task::none()
        }
    }

    fn notice_control(&self) -> Option<Element<'_, Message>> {
        if !self.has_chrome_notices() {
            return None;
        }
        // The anchor must carry the actual severity. Collapsing it to a binary
        // `error` flag rendered a completed pull request as an amber warning
        // triangle labelled "Application notices", and `NoticeSeverity::icon`
        // already owns the per-severity glyph.
        let health_severity = self.save_health.as_ref().and_then(|health| {
            (matches!(
                health.health,
                SessionSaveHealth::RetryExhausted { .. } | SessionSaveHealth::WriterUnavailable
            ) || !health.quarantined.is_empty())
            .then_some(NoticeSeverity::Error)
        });
        // From the log, not `self.status`: `update` drains that slot into the
        // log on every message, so reading it here would almost always see
        // `None` and fall back to a warning glyph for an error.
        let recorded_severity = self.notices.severity().max(health_severity);
        let severity = recorded_severity.unwrap_or(NoticeSeverity::Warning);
        let control = crate::ui_controls::icon_button(
            severity.icon(),
            match severity {
                NoticeSeverity::Success => "Application notices: success",
                NoticeSeverity::Warning => "Application notices",
                NoticeSeverity::Error => "Application errors",
            },
        )
        .selected(self.chrome_ui.popup == Some(Popup::Notices))
        .on_press_maybe((!self.modal_is_open()).then_some(Message::ChromeUi(Action::ToggleNotices)))
        .id(widget::Id::new("chrome.notices"))
        .tone(design::Tone::Themed(match severity {
            NoticeSeverity::Success => design::success_color,
            NoticeSeverity::Warning => design::warning_color,
            NoticeSeverity::Error => design::destructive_color,
        }));
        let content =
            (self.chrome_ui.popup == Some(Popup::Notices) && !self.modal_is_open()).then(|| {
                let mut notices = Vec::with_capacity(self.notices.len() + 1);
                for entry in self.notices.recent() {
                    let body: Element<'_, Message> = if entry.repeats > 1 {
                        widget::column::with_children(vec![
                            super::notice_body(&entry.notice.message),
                            super::notice_surface_detail(
                                entry.notice.severity,
                                format!("Repeated {} times", entry.repeats),
                            ),
                        ])
                        .spacing(space::XS)
                        .into()
                    } else {
                        super::notice_body(&entry.notice.message)
                    };
                    notices.push(super::notice_surface(
                        entry.notice.severity,
                        body,
                        Some(super::notice_surface_action(
                            entry.notice.severity,
                            "Dismiss",
                            "Dismiss application notice",
                            widget::Id::new(notice_dismiss_id(entry.id)),
                            Message::DismissNotice(entry.id),
                        )),
                        false,
                    ));
                }
                if let Some(health) = &self.save_health {
                    notices.extend(save_health_views(
                        health,
                        |card| {
                            self.card_state(card)
                                .map(|card| self.card_display_title(card))
                        },
                        self.session_recovery_action(),
                    ));
                }
                let mut heading_children = vec![
                    design::heading("Application notices")
                        .width(Length::Fill)
                        .into(),
                ];
                if !self.notices.is_empty() {
                    heading_children.push(
                        crate::ui_controls::button::quiet("Dismiss all")
                            .id(widget::Id::new(NOTICES_DISMISS_ALL_ID))
                            .name("Dismiss every application notice")
                            .on_press(Message::DismissAllNotices)
                            .into(),
                    );
                }
                heading_children.push(command(
                    Icon::Close,
                    "Close application notices",
                    widget::Id::new(NOTICES_CLOSE_ID),
                    Some(Message::ChromeUi(Action::Close)),
                ));
                let heading = widget::row::with_children(heading_children)
                    .align_y(Alignment::Center)
                    .spacing(space::S);
                widget::container(
                    widget::column::with_children(vec![
                        widget::container(heading)
                            .padding(iced::Padding::new(6.0).bottom(0.0))
                            .into(),
                        widget::scrollable(
                            widget::column::with_children(notices)
                                .spacing(space::M)
                                // Each notice ends in a Dismiss control on its
                                // right edge, which the floating thumb would
                                // otherwise cover.
                                .padding(design::with_vertical_rail(
                                    iced::Padding::new(6.0).top(0.0),
                                )),
                        )
                        .direction(iced::widget::scrollable::Direction::Vertical(
                            design::scrollbar(),
                        ))
                        .height(Length::Shrink)
                        .into(),
                    ])
                    .spacing(space::S),
                )
                .width(design::NOTICE_POPOVER_WIDTH)
                .max_height(408)
                .into()
            });
        Some(crate::ui_controls::popover(
            control,
            content,
            widget::popover::Position::Bottom,
            Message::ChromeUi(Action::Close),
        ))
    }

    fn native_header<'a>(
        &'a self,
        start: Element<'a, Message>,
        end: Option<Element<'a, Message>>,
        window_controls: bool,
    ) -> Element<'a, Message> {
        let focused = self
            .core
            .focus_chain()
            .iter()
            .any(|id| Some(*id) == self.core.main_window_id());
        let mut header = widget::header_bar()
            .density(cosmic::cosmic_theme::Density::Compact)
            .focused(focused)
            .maximized(self.core.window.is_maximized)
            .sharp_corners(self.core.window.sharp_corners)
            .start(start)
            .on_drag(Message::NativeWindowAction(cosmic::app::Action::Drag))
            .on_double_click(Message::NativeWindowAction(cosmic::app::Action::Maximize))
            .on_right_click(Message::NativeWindowAction(
                cosmic::app::Action::ShowWindowMenu,
            ));
        if let Some(end) = end {
            header = header.end(end);
        }
        if window_controls {
            if self.core.window.show_close
                && let Some(window) = self.core.main_window_id()
            {
                header = header.on_close(Message::WindowCloseRequested(window));
            }
            if self.core.window.show_maximize && cosmic::config::show_maximize() {
                header =
                    header.on_maximize(Message::NativeWindowAction(cosmic::app::Action::Maximize));
            }
            if self.core.window.show_minimize && cosmic::config::show_minimize() {
                header =
                    header.on_minimize(Message::NativeWindowAction(cosmic::app::Action::Minimize));
            }
        }
        widget::container(header)
            .center_y(design::HEADER_CONTENT_HEIGHT)
            .width(Length::Fill)
            .class(cosmic::theme::Container::Background)
            .into()
    }

    fn sidebar_toggle(&self) -> Element<'_, Message> {
        crate::ui_controls::icon_button(
            Icon::SplitPanel,
            workspace_widget::shortcut_tooltip(
                workspace_widget::GlobalShortcut::ToggleInspector,
                &self.settings.keyboard_shortcuts,
            ),
        )
        .on_press_maybe((!self.modal_is_open()).then_some(Message::ToggleContextInspector))
        .id(context_inspector_toggle_id())
        .into()
    }

    fn workspace_header_width(&self) -> f32 {
        let navigator = if should_show_card_navigator(self.workspace.cards().len()) {
            design::PANE_NAVIGATOR_RESERVED_WIDTH
        } else {
            0.0
        };
        let controls = if self.context_inspector_open {
            0.0
        } else {
            let count = usize::from(self.core.window.show_close)
                + usize::from(self.core.window.show_maximize && cosmic::config::show_maximize())
                + usize::from(self.core.window.show_minimize && cosmic::config::show_minimize());
            count as f32 * design::WINDOW_CONTROL_SIZE
                // libcosmic's header bar lays the controls out on the
                // desktop's density, so this gap has to follow it too.
                + count.saturating_sub(1) as f32
                    * f32::from(cosmic::theme::spacing().space_xxs)
                + design::PRIMARY_CONTROL_SIZE
                + 12.0
        };
        // The docked inspector owns both the alerts control and the update
        // control. Reserving the update here as well pushed it onto the
        // workspace header, on the far side of the bar from the alerts.
        let update = if !self.context_inspector_visible() && self.has_update_control() {
            design::PRIMARY_CONTROL_SIZE + 4.0
        } else {
            0.0
        };
        // Same predicate `workflow_header` mounts the notice control behind,
        // so the strip never reserves room for a control the panel is holding.
        let notice = if !self.context_inspector_visible() && self.has_chrome_notices() {
            design::PRIMARY_CONTROL_SIZE + 2.0
        } else {
            0.0
        };
        let width = navigator + controls + update + notice + 16.0;
        if self.workspace.viewport_width() > 0.0 {
            width.min(self.workspace.viewport_width())
        } else {
            width
        }
    }

    fn tab_strip_geometry(&self) -> (f32, f32) {
        let available = (self.workspace.viewport_width() - self.workspace_header_width()).max(0.0);
        // Reordering uses one shared width. Derive that width from the titles
        // instead of giving every agent the same eight-character title budget.
        let desired = self
            .workspace
            .order()
            .filter_map(|id| self.card_chrome.get(&id))
            .map(|chrome| 80.0 + chrome.title.chars().count().min(28) as f32 * 7.0)
            .fold(design::TAB_MIN_WIDTH, f32::max)
            .clamp(design::TAB_MIN_WIDTH, design::TAB_MAX_WIDTH);
        (available, desired)
    }

    /// Scrolls the tab strip by the least amount that brings the focused tab
    /// fully into view, and not at all when it is already visible.
    ///
    /// Reveal fires on every focus change, and under focus-follows-mouse that
    /// is every hover. An absolute right-aligning scroll therefore dragged the
    /// strip out from under the pointer.
    pub(super) fn reveal_active_tab(&self) -> Task<Message> {
        let Some(index) = self
            .workspace
            .focused()
            .and_then(|focused| self.workspace.order().position(|id| id == focused))
        else {
            return Task::none();
        };
        let (available, width) = self.tab_strip_geometry();
        if available <= 0.0 {
            return Task::none();
        }
        // The trailing "new tab" control shares the scrollable row, so a tab
        // revealed flush against the viewport edge would sit underneath it.
        let pitch = width + crate::tab_strip::SPACING;
        let content = self.workspace.order().count() as f32 * pitch
            + design::PRIMARY_CONTROL_SIZE
            + 2.0 * crate::tab_strip::EDGE_INSET;
        let max_offset = (content - available).max(0.0);
        if max_offset <= 0.0 {
            // Everything fits; any offset at all would be wrong.
            return if self.chrome_ui.tabs_offset > 0.0 {
                workspace_widget::scroll_to(widget::Id::new("workspace.tabs"), 0.0)
                    .map(cosmic::Action::App)
            } else {
                Task::none()
            };
        }
        let leading = crate::tab_strip::EDGE_INSET + index as f32 * pitch;
        let trailing = leading + width;
        let offset = self.chrome_ui.tabs_offset;
        let target = if leading < offset {
            leading
        } else if trailing > offset + available {
            trailing - available
        } else {
            return Task::none();
        };
        workspace_widget::scroll_to(
            widget::Id::new("workspace.tabs"),
            target.clamp(0.0, max_offset),
        )
        .map(cosmic::Action::App)
    }

    /// One compact navigation strip, independent of pane widths and camera offset.
    pub(super) fn workspace_chrome<'a>(
        &'a self,
        content: Element<'a, Message>,
    ) -> Element<'a, Message> {
        let (_, tab_width) = self.tab_strip_geometry();
        let tabs = self
            .workspace
            .order()
            .map(|id| self.column_header(id, self.workspace.focused() == Some(id), tab_width))
            .chain(std::iter::once(self.new_tab_control()))
            .collect();
        let generation = self.workspace_view_generation;
        let tabs = crate::tab_strip::reorderable(
            widget::Id::new("workspace.tabs"),
            tabs,
            design::PRIMARY_CONTROL_SIZE,
            design::HEADER_HEIGHT,
            Some(Box::new(|viewport| {
                Message::ChromeUi(Action::TabsScrolled(viewport.absolute_offset().x))
            })),
            crate::tab_strip::Reordering {
                order: self.workspace.order().collect(),
                tab_width,
                enabled: !self.workspace_inputs_blocked(),
                select: Box::new(Message::FocusCard),
                dropped: Box::new(move |card, before| Message::Viewport {
                    generation,
                    event: WorkspaceViewportEvent::Reordered { card, before },
                }),
            },
        );
        let actions = widget::row::with_children(self.workflow_header())
            .spacing(space::XXS)
            .align_y(Alignment::Center);
        let header = self.native_header(
            tabs.into(),
            Some(actions.into()),
            !self.context_inspector_open,
        );
        widget::column::with_children(vec![
            header,
            crate::design::horizontal_rule().into(),
            content,
        ])
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    pub(super) fn workflow_header(&self) -> Vec<Element<'_, Message>> {
        if self.context_inspector_is_overlay() {
            return Vec::new();
        }
        let mut actions = Vec::with_capacity(8);
        let total = self.workspace.cards().len();
        if should_show_card_navigator(total) {
            let current = self
                .workspace
                .focused()
                .and_then(|id| self.workspace.order().position(|candidate| candidate == id))
                .map_or(0, |index| index + 1);
            actions.push(command(
                Icon::ChevronLeft,
                workspace_widget::shortcut_tooltip(
                    workspace_widget::GlobalShortcut::FocusAdjacent(
                        crate::workspace::FocusDirection::Left,
                    ),
                    &self.settings.keyboard_shortcuts,
                ),
                widget::Id::new("command.previous-column"),
                (current > 1).then_some(Message::FocusAdjacent(
                    crate::workspace::FocusDirection::Left,
                )),
            ));
            actions.push(
                iced::widget::container(crate::design::caption(format!("{current} / {total}")))
                    .center_y(ICON_CONTROL_SIZE)
                    .align_x(Alignment::Center)
                    .width(
                        (format!("{current} / {total}").len() as f32 * 7.0 + 16.0)
                            .max(design::PANE_NAVIGATOR_MIN_WIDTH),
                    )
                    .into(),
            );
            actions.push(command(
                Icon::ChevronRight,
                workspace_widget::shortcut_tooltip(
                    workspace_widget::GlobalShortcut::FocusAdjacent(
                        crate::workspace::FocusDirection::Right,
                    ),
                    &self.settings.keyboard_shortcuts,
                ),
                widget::Id::new("command.next-column"),
                (current > 0 && current < total).then_some(Message::FocusAdjacent(
                    crate::workspace::FocusDirection::Right,
                )),
            ));
        }
        // The inspector panel's own header mounts the notice control, the update
        // control and the inspector toggle, and the two headers must never
        // mount the same one at once: a custom `widget::Id` may name only one
        // node in a frame. The pinned runtime lifts every named node's state
        // into one thread-local map keyed by that name before diffing, hands
        // it to the first widget that asks for the name, and leaves any later
        // claimant holding the `State::None` the lift left behind - which the
        // next draw downcasts, aborting the process.
        //
        // Ownership therefore changes hands when the panel mounts and
        // unmounts, not when it is asked to open or close. Those differ for
        // the whole of the collapse animation: `context_inspector_open` goes
        // false at the click, while the panel stays mounted and keeps drawing
        // its header until the transition ends.
        //
        // The update control sits immediately to the right of the alerts.
        // Leaving it on the workspace header while the panel held the alerts
        // put the two icons on opposite ends of the window.
        let panel_owns_shared_controls = self.context_inspector_visible();
        vec![
            widget::row::with_children(actions)
                .spacing(space::XXS)
                .align_y(Alignment::Center)
                .into(),
            widget::row::with_children(
                (!panel_owns_shared_controls)
                    .then(|| self.notice_control())
                    .flatten(),
            )
            .into(),
            widget::row::with_children(
                (!panel_owns_shared_controls)
                    .then(|| self.update_control())
                    .flatten(),
            )
            .into(),
            widget::row::with_children(
                (!panel_owns_shared_controls).then(|| self.sidebar_toggle()),
            )
            .into(),
        ]
    }

    pub(super) fn pane_rename_surface(&self) -> Option<Element<'_, Message>> {
        let (target, draft) = self.chrome_ui.rename.as_ref()?;
        Some(
            widget::container(
                widget::column::with_children(vec![
                    design::heading(match target {
                        RenameTarget::Pane(_) => "Rename pane",
                        RenameTarget::Channel(_) => "Rename channel",
                    })
                    .into(),
                    crate::ui_controls::field::text(
                        match target {
                            RenameTarget::Pane(_) => "Pane name",
                            RenameTarget::Channel(_) => "Channel name",
                        },
                        draft,
                    )
                    .id(widget::Id::new("chrome.rename.input"))
                    .on_input(|value| Message::ChromeUi(Action::RenameChanged(value)))
                    .on_submit(|_| Message::ChromeUi(Action::RenameSubmit))
                    .into(),
                    design::caption(if normalize_card_name(draft).is_none() {
                        "Name cannot be blank."
                    } else {
                        ""
                    })
                    .into(),
                    widget::row::with_children(vec![
                        crate::ui_controls::button::secondary("Cancel")
                            .on_press(Message::ChromeUi(Action::RenameCancel))
                            .into(),
                        crate::ui_controls::button::primary("Rename")
                            .on_press_maybe(
                                normalize_card_name(draft)
                                    .map(|_| Message::ChromeUi(Action::RenameSubmit)),
                            )
                            .into(),
                    ])
                    .spacing(space::XS)
                    .into(),
                ])
                .spacing(space::S),
            )
            .padding(space::M)
            .width(400)
            .class(cosmic::theme::Container::Card)
            .into(),
        )
    }

    /// The mark that names a card wherever the card itself is shown: its tab
    /// and the inspector heading. Agents wear their provider's mark, whether
    /// they run in a native chat or a terminal.
    pub(super) fn card_icon(&self, id: CardId) -> widget::icon::Handle {
        let kind = self.card_state(id).map(|card| &card.kind);
        let generic = || card_kind_labels(kind).icon.handle();
        match kind {
            Some(CardKind::Repository(_)) => crate::ui_controls::ceditor_icon(),
            Some(CardKind::Chat(session)) => {
                crate::ui_controls::provider_icon(session.launch.provider)
            }
            Some(CardKind::Terminal(session)) => self
                .sidebar_state
                .card_activity(id)
                .map(|activity| activity.provider)
                .or_else(|| crate::agent::Provider::recognize_launch(&session.launch))
                .map_or_else(generic, crate::ui_controls::provider_icon),
            _ => generic(),
        }
    }

    fn column_header(&self, id: CardId, focused: bool, width: f32) -> Element<'_, Message> {
        let card = self.card_state(id);
        let labels = card_kind_labels(card.map(|card| &card.kind));
        let chrome = self.card_chrome.get(&id);
        let title = chrome.map_or_else(
            || card.map_or("Column", |card| self.card_display_title(card)),
            |chrome| chrome.title.as_str(),
        );
        let focus_id = self.card_header_widget_id(id);
        let caption = chrome
            .map(|chrome| Cow::Borrowed(chrome.caption.as_str()))
            .or_else(|| card.map(|card| card_kind_caption(&card.kind)));
        let title_control = widget::button::custom(
            widget::row::with_children(vec![
                self.card_icon(id).icon().size(design::ROW_ICON_SIZE).into(),
                design::body(title)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(space::S)
            .align_y(Alignment::Center)
            .height(Length::Fill),
        )
        .id(focus_id)
        .name(chrome.map_or_else(
            || {
                Cow::Owned(card_accessible_name(
                    labels.type_label,
                    title,
                    caption.as_deref().unwrap_or(""),
                    focused,
                ))
            },
            |chrome| Cow::Borrowed(chrome.accessible_name(focused)),
        ))
        .class(column_title_style())
        .padding([0.0, space::XS])
        .width(Length::Fill)
        .height(Length::Fill)
        .on_press(Message::FocusCard(id));
        let tip = chrome.map_or_else(
            || {
                Cow::Owned(match caption {
                    Some(caption) => format!("{title}\n{caption}"),
                    None => title.to_owned(),
                })
            },
            |chrome| Cow::Borrowed(chrome.tooltip.as_str()),
        );
        let mut children = vec![tooltip(crate::reorder::handle(title_control, id), tip)];
        let status = match self.cards.get(&id) {
            _ if self.chat_start_pending(id) => Some(("Updating conversation", true)),
            None | Some(CardRuntime::Loading) => Some((labels.starting, true)),
            Some(CardRuntime::Failed(_)) => Some(("Unavailable", false)),
            #[cfg(feature = "integrated-browser")]
            Some(CardRuntime::Browser(browser))
                if browser.lifecycle == BrowserLifecycle::Restoring =>
            {
                Some(("Restoring", true))
            }
            #[cfg(feature = "integrated-browser")]
            Some(CardRuntime::Browser(browser)) if browser.load_state.is_some() => {
                Some(("Loading", true))
            }
            _ => None,
        };
        if let Some((label, busy)) = status {
            let indicator: Element<'_, Message> = if busy {
                crate::ui_controls::activity_spinner(
                    design::TAB_STATUS_ICON_SIZE as f32,
                    design::accent_color,
                )
            } else {
                Icon::Warning
                    .handle()
                    .icon()
                    .size(design::TAB_STATUS_ICON_SIZE)
                    .into()
            };
            children.push(tooltip(indicator, label));
        }
        if self.sidebar_state.has_card_update(id) {
            children.push(tooltip(
                Icon::Attention
                    .handle()
                    .icon()
                    .size(design::TAB_STATUS_ICON_SIZE),
                "Needs attention",
            ));
        } else if let Some(activity) = self.sidebar_state.card_activity(id) {
            match activity.state {
                crate::agent::ActivityState::Working => children.push(tooltip(
                    crate::ui_controls::activity_spinner(
                        design::TAB_STATUS_ICON_SIZE as f32,
                        design::warning_color,
                    ),
                    "Working",
                )),
                crate::agent::ActivityState::Waiting => children.push(sidebar::status_icon(
                    Icon::Warning,
                    "Waiting for input",
                    sidebar::BadgeTone::Warning,
                )),
                _ => {}
            }
        }
        if focused && self.chat_switch_available(id) {
            let blocker = self.chat_switch_blocker(id);
            let ready = blocker.is_none();
            children.push(command(
                if matches!(card.map(|state| &state.kind), Some(CardKind::Chat(_))) {
                    Icon::Terminal
                } else {
                    Icon::Chat
                },
                blocker.unwrap_or_else(|| Cow::Borrowed(self.chat_switch_label(id))),
                card_control_id(id, "switch"),
                ready.then_some(Message::ChatUi(chat_ui::Action::Switch(id))),
            ));
        }
        if focused || self.chrome_ui.hovered_tab == Some(id) {
            children.push(command(
                Icon::Close,
                {
                    let label = chrome.map_or_else(
                        || Cow::Owned(card_close_name(labels.type_label, title)),
                        |chrome| Cow::Borrowed(chrome.close_name.as_str()),
                    );
                    if focused {
                        Cow::Owned(workspace_widget::shortcut_tooltip_named(
                            workspace_widget::GlobalShortcut::CloseFocusedCard,
                            &self.settings.keyboard_shortcuts,
                            &label,
                        ))
                    } else {
                        label
                    }
                },
                chrome.map_or_else(
                    || card_control_id(id, "close"),
                    |chrome| chrome.close_id.clone(),
                ),
                Some(Message::CloseCard(id)),
            ));
        }
        let tab = crate::tab_strip::middle_click_close(
            widget::container(
                widget::row::with_children(children)
                    .align_y(Alignment::Center)
                    .height(Length::Fill)
                    .spacing(space::XXS)
                    .width(Length::Fill),
            )
            .padding([0.0, space::XS])
            .height(design::PRIMARY_CONTROL_SIZE)
            .width(width)
            .class(cosmic::theme::Container::custom(move |theme| {
                column_tab_style(theme, focused, self.chrome_ui.hovered_tab == Some(id))
            })),
            Message::CloseCard(id),
        );
        let tab = iced::widget::mouse_area(tab)
            .on_enter(Message::ChromeUi(Action::HoverTab(Some(id))))
            .on_exit(Message::ChromeUi(Action::HoverTab(None)));
        crate::interaction::context_menu_lazy(tab, move || {
            let mut entries: Vec<widget::menu::Item<PaneMenu, String>> = vec![
                widget::menu::Item::Button(
                    "Rename…".into(),
                    None,
                    PaneMenu(id, PaneMenuCommand::Rename),
                ),
                widget::menu::Item::Button(
                    "Clone pane".into(),
                    None,
                    PaneMenu(id, PaneMenuCommand::Clone),
                ),
                widget::menu::Item::Button(
                    "Copy path".into(),
                    None,
                    PaneMenu(id, PaneMenuCommand::CopyPath),
                ),
                widget::menu::Item::Button(
                    "Reveal in Projects".into(),
                    None,
                    PaneMenu(id, PaneMenuCommand::Reveal),
                ),
                widget::menu::Item::Button(
                    "Close other panes".into(),
                    None,
                    PaneMenu(id, PaneMenuCommand::CloseOthers),
                ),
                widget::menu::Item::Button(
                    "Close panes to the right".into(),
                    None,
                    PaneMenu(id, PaneMenuCommand::CloseToRight),
                ),
                widget::menu::Item::Button(
                    "Close pane".into(),
                    None,
                    PaneMenu(id, PaneMenuCommand::Close),
                ),
            ];
            if self.chat_switch_available(id) && self.chat_switch_ready(id) {
                entries.push(widget::menu::Item::Button(
                    self.chat_switch_label(id).into(),
                    None,
                    PaneMenu(id, PaneMenuCommand::Switch),
                ));
            }
            if let Some(target) = self.chat_fork_target(id) {
                entries.push(widget::menu::Item::Button(
                    "Fork conversation…".into(),
                    None,
                    PaneMenu(id, PaneMenuCommand::Fork(target)),
                ));
            }
            if let Some(target) = self.chat_changes_target(id) {
                entries.push(widget::menu::Item::Button(
                    "Review file changes".into(),
                    None,
                    PaneMenu(id, PaneMenuCommand::ReviewChanges(target)),
                ));
            }
            widget::menu::items(&HashMap::new(), entries)
        })
    }

    /// The navigation row the context rail speaks for: the active worktree's
    /// own row, so the rail reads one worktree rather than re-deriving it.
    pub(super) fn context_rail_indicators(&self) -> sidebar::WorktreeIndicators<'_> {
        let Some(key) = self.sidebar_state.active_worktree() else {
            return sidebar::WorktreeIndicators::default();
        };
        self.sidebar_model
            .projects
            .iter()
            .find(|project| project.id == key.project_id)
            .and_then(|project| {
                project
                    .worktrees
                    .iter()
                    .find(|worktree| worktree.id == key.worktree_id)
            })
            .map(sidebar::worktree_indicators)
            .unwrap_or_default()
    }

    pub(super) fn context_panel(
        &self,
        model: Option<inspector::InspectorModel>,
        width: f32,
    ) -> Element<'_, Message> {
        let indicators = self.context_rail_indicators();
        let tabs = widget::row::with_children(
            ContextTab::ALL
                .into_iter()
                .filter(|tab| {
                    !self.active_folder_scope()
                        || !matches!(tab, ContextTab::Changes | ContextTab::PullRequest)
                })
                .map(|tab| {
                    let selected = self.context_tab == tab;
                    let enabled = !self.modal_is_open()
                        && (tab != ContextTab::PullRequest || self.active_workspace.is_some());
                    let icon = match tab {
                        ContextTab::Files => Icon::Folder,
                        ContextTab::Changes => Icon::Changes,
                        ContextTab::PullRequest => Icon::PullRequest,
                        ContextTab::Details => Icon::Info,
                    };
                    let visual = (tab == ContextTab::PullRequest)
                        .then_some(indicators.pull_request)
                        .flatten()
                        .map(sidebar::pull_request_visual);
                    let dirty = (tab == ContextTab::Changes)
                        .then_some(indicators.dirty)
                        .flatten();
                    // The tab's one icon carries the worktree's state: the pull
                    // request's own glyph and tone, the changes tab tinted while the
                    // tree is dirty. What the tint means is said by the tab's name,
                    // which is the tooltip and the accessible name; a second icon
                    // beside the tab was the same fact drawn twice.
                    let mut name = format!("{} panel tab", tab.label());
                    match tab {
                        ContextTab::Changes => {
                            if let Some(label) = dirty {
                                name.push_str(": ");
                                name.push_str(label);
                            }
                        }
                        ContextTab::PullRequest if indicators.pending_checks => {
                            name.push_str(": checks pending");
                        }
                        _ => {}
                    }
                    let control = crate::ui_controls::icon_button(
                        visual
                            .as_ref()
                            .map_or_else(|| icon.handle(), |(icon, _)| icon.clone()),
                        name,
                    )
                    .selected(selected)
                    .on_press_maybe(enabled.then_some(Message::SelectContextTab(tab)));
                    match (&visual, dirty) {
                        (Some((_, color)), _) => control.tone(*color).into(),
                        (None, Some(_)) => control
                            .tone(design::Tone::Themed(design::warning_color))
                            .into(),
                        (None, None) => control.into(),
                    }
                }),
        )
        .spacing(space::XXS)
        .align_y(Alignment::Center);
        // Keep named controls in fixed slots as notices and updates come and go.
        // Alerts and updates remain beside the window controls; the toggle
        // belongs at the leading edge of the inspector it controls.
        // This header is the one mounted while the panel owns those controls,
        // including a docked inspector: the workspace header must not also
        // show the update or the icon stays on the far side of the window.
        let end = widget::row::with_children(vec![
            widget::row::with_children(self.notice_control()).into(),
            widget::row::with_children(self.update_control()).into(),
        ])
        .spacing(space::XXS)
        .align_y(Alignment::Center)
        .into();
        let heading = self.native_header(self.sidebar_toggle(), Some(end), true);
        let tabs = widget::container(tabs)
            .padding([0.0, space::S])
            .center_y(design::HEADER_CONTENT_HEIGHT)
            .width(Length::Fill);
        let body = match self.context_tab {
            ContextTab::Files | ContextTab::Changes if crate::device::is_remote() => {
                let changes = self.context_tab == ContextTab::Changes;
                let panel = if changes {
                    device_ui::Panel::Changes
                } else {
                    device_ui::Panel::Files
                };
                let path = self.active_workspace.as_ref();
                widget::container(
                    widget::column::with_children(vec![
                        crate::interaction::tooltip(
                            design::caption(
                                path.and_then(|path| path.file_name())
                                    .map(|name| name.to_string_lossy().into_owned())
                                    .unwrap_or_else(|| "Device".into()),
                            )
                            .width(Length::Fill),
                            path.map(|path| path.display().to_string())
                                .unwrap_or_else(|| {
                                    crate::device::selected()
                                        .map(|device| device.name.clone())
                                        .unwrap_or_default()
                                }),
                        ),
                        crate::ui_controls::button::secondary(if changes {
                            "Open changes"
                        } else {
                            "Open files"
                        })
                        .icon(if changes {
                            Icon::Diff
                        } else {
                            Icon::OpenFolder
                        })
                        .name(if changes {
                            "Open device changes"
                        } else {
                            "Open device files"
                        })
                        .on_press_maybe((!self.modal_is_open()).then_some(Message::Device(
                            device_ui::Action::OpenIn {
                                path: path.cloned(),
                                panel,
                            },
                        )))
                        .into(),
                    ])
                    .spacing(space::M),
                )
                .padding(space::L)
                .width(Length::Fill)
                .into()
            }
            ContextTab::Files => sidebar::file_panel(
                &self.sidebar_model,
                &self.sidebar_state,
                width,
                self.modal_is_open(),
            )
            .map(Message::Sidebar),
            ContextTab::Changes => sidebar::git_panel(
                &self.sidebar_model,
                &self.sidebar_state,
                width,
                self.modal_is_open(),
            )
            .map(Message::Sidebar),
            ContextTab::PullRequest => self.pull_request_panel(width),
            ContextTab::Details => model.map_or_else(
                || {
                    widget::container(
                        widget::column::with_children(vec![
                            design::caption("No pane selected")
                                .width(Length::Fill)
                                .into(),
                            crate::ui_controls::icon_button(Icon::OpenFolder, "Browse files")
                                .on_press(Message::SelectContextTab(ContextTab::Files))
                                .into(),
                        ])
                        .spacing(space::S),
                    )
                    .padding(space::M)
                    .width(Length::Fill)
                    .into()
                },
                |model| {
                    let diagnostics_open = self.inspector_diagnostics_card == Some(model.card_id);
                    let icon = self.card_icon(model.card_id);
                    inspector::view(model, icon, width, diagnostics_open)
                        .map(Message::ContextInspector)
                },
            ),
        };
        widget::container(
            widget::column::with_children(vec![
                heading.into(),
                crate::design::horizontal_rule().into(),
                tabs.into(),
                crate::design::horizontal_rule().into(),
                body,
            ])
            .width(Length::Fill)
            .height(Length::Fill),
        )
        .class(cosmic::theme::Container::Background)
        .width(width)
        .height(Length::Fill)
        .into()
    }
}
