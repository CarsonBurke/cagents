use super::*;
use crate::interaction::tooltip;

/// Shape once when cached chrome changes, with the same font and size as the tab.
pub(super) fn measure_tab_title(title: &str) -> f32 {
    use iced::{
        Pixels, Size,
        advanced::text::{self, Paragraph as _},
    };
    let paragraph = <cosmic::Renderer as text::Renderer>::Paragraph::with_text(text::Text {
        content: title,
        bounds: Size::new(f32::INFINITY, f32::INFINITY),
        size: Pixels(design::TEXT_BODY),
        line_height: text::LineHeight::Absolute(Pixels(design::LINE_BODY)),
        font: cosmic::font::default(),
        align_x: text::Alignment::Left,
        align_y: iced::alignment::Vertical::Top,
        shaping: text::Shaping::default(),
        wrapping: text::Wrapping::None,
        ellipsize: text::Ellipsize::None,
    });
    paragraph.min_bounds().width
}

#[derive(Debug, Default)]
pub(super) struct State {
    hovered_tab: Option<CardId>,
    pub(super) closing_group: Vec<CardId>,
    pub(super) user_titles: HashMap<CardId, String>,
    pub(super) terminal_ordinals: HashMap<CardId, u32>,
    pub(super) terminal_next_ordinals: HashMap<PathBuf, u32>,
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
    attention_page: usize,
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
    /// The notices control kept on screen, at this severity, for the frame
    /// after the last notice went: long enough to learn whether it held the
    /// keyboard, which would otherwise go nowhere with it.
    retiring_notice_control: Option<NoticeSeverity>,
}

#[cfg(test)]
mod tests {
    include!("chrome_ui/notice_actions_tests.rs");
    #[test]
    fn attention_popup_pages_unique_incidents_and_survives_an_empty_project_list() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.sidebar_model.projects.clear();
        app.workspace_model.identity = uuid::Uuid::new_v4();
        drop(app.update_chrome(Action::ToggleAttention));
        assert!(app.attention_is_open());
        assert_eq!(app.popup_focus_ring(Popup::Attention), ["attention.close"]);
        let target = crate::model::TerminalId::new();
        for index in 0..21 {
            app.notification_ui
                .snapshot
                .notices
                .push(crate::notifications::Notice {
                    id: uuid::Uuid::new_v4(),
                    event: crate::notifications::Event::session(
                        target,
                        crate::agent::Provider::Codex,
                        "/tmp/toy".into(),
                        crate::notifications::Evidence {
                            key: format!("incident-{index}"),
                            kind: crate::notifications::Kind::Succeeded,
                        },
                    ),
                    delivered: false,
                    acknowledged: false,
                    window: None,
                });
        }
        let proof = HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("attention-pages"),
            boot_id: crate::model::BootId::new("attention-pages"),
            host_incarnation: crate::model::HostIncarnation::new("attention-pages"),
        };
        drop(app.update_notifications(notification_ui::Action::HostBound(proof)));
        let snapshot = app.notification_ui.snapshot.clone();
        drop(app.update_notifications(notification_ui::Action::Snapshot(snapshot)));
        let duplicate = app.notification_ui.snapshot.notices[0].clone();
        app.notification_ui.snapshot.notices.push(duplicate);
        let first = app.popup_focus_ring(Popup::Attention);
        assert_eq!(first.len(), 22, "close, twenty incident actions, more");
        assert_eq!(
            first
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            first.len()
        );
        assert_eq!(first.last().unwrap(), "attention.next");
        drop(app.update_chrome(Action::AttentionPage(1)));
        let last = app.popup_focus_ring(Popup::Attention);
        assert_eq!(last.len(), 3, "close, last incident, previous");
        assert_eq!(last.last().unwrap(), "attention.previous");
        app.notification_ui.snapshot.notices.clear();
        assert_eq!(app.popup_focus_ring(Popup::Attention), ["attention.close"]);
        drop(app.update_chrome(Action::Close));
        assert!(!app.attention_is_open());
        app.settings_reset_confirm = true;
        drop(app.update_chrome(Action::ToggleAttention));
        assert!(
            !app.attention_is_open(),
            "a modal keeps its existing keyboard owner"
        );
    }

    #[test]
    fn narrow_header_mounts_four_complete_tabs() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree, operation::Focusable},
        };
        use iced::{Rectangle, Size};
        struct Bounds(HashMap<String, Rectangle>, Option<Rectangle>);
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
                if let Some(id) = id {
                    self.0.insert(id.to_string(), bounds);
                }
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: Rectangle,
                _: Rectangle,
                _: iced::Vector,
                _: &mut dyn iced::advanced::widget::operation::Scrollable,
            ) {
                if id == Some(&widget::Id::new("workspace.tabs")) {
                    self.1 = Some(bounds);
                }
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: Rectangle,
                _: &mut dyn Focusable,
            ) {
                if let Some(id) = id {
                    self.0.insert(id.to_string(), bounds);
                }
            }
        }
        let renderer = super::super::tests::headless_renderer();
        for width in [640.0, 900.0, 1600.0] {
            let app = tab_fixture(30, width);
            let mut element =
                app.workspace_chrome(widget::Space::new().height(Length::Fill).into());
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::new(width, 600.0), Size::new(width, 600.0)),
            );
            let mut bounds = Bounds(HashMap::new(), None);
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
            if width <= 900.0 {
                let viewport = bounds.1.expect("native tab viewport");
                let visible = app
                    .workspace
                    .order()
                    .filter(|id| {
                        let tab = bounds.0[&card_control_id(*id, "tab").to_string()];
                        tab.x >= viewport.x
                            && tab.x + tab.width <= viewport.x + viewport.width + 0.1
                    })
                    .count();
                assert!(
                    visible >= 4,
                    "only {visible} complete tabs fit in {viewport:?}"
                );
            }
        }
    }

    fn tab_fixture(count: usize, width: f32) -> Cagents {
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.context_inspector_open = false;
        let cards: Vec<_> = (0..count)
            .map(|_| super::super::tests::terminal_card_state())
            .collect();
        let durable = Arc::make_mut(&mut app.durable);
        durable.focused = cards.first().map(|card| card.id);
        durable.cards = cards;
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(width);
        drop(app.sync_chrome());
        app
    }

    #[test]
    fn narrow_tabs_fit_four_and_wide_tabs_fit_real_twenty_character_titles() {
        let mut app = tab_fixture(30, 900.0);
        let (available, width) = app.tab_strip_geometry();
        assert!(
            4.0 * (width + crate::tab_strip::SPACING) + 2.0 * crate::tab_strip::EDGE_INSET
                <= available + 0.1
        );
        let id = app.durable.cards[0].id;
        app.chrome_ui
            .user_titles
            .insert(id, "WWWWWWWWWWWWWWWWWWWW".into());
        app.workspace.resize_viewport(1600.0);
        drop(app.sync_chrome());
        let (_, width) = app.tab_strip_geometry();
        assert!(width >= measure_tab_title("WWWWWWWWWWWWWWWWWWWW") + 144.0);
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree},
        };
        struct TitleBounds(Option<iced::Rectangle>);
        impl Operation for TitleBounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
                if text == "WWWWWWWWWWWWWWWWWWWW" {
                    self.0 = Some(bounds);
                }
            }
        }
        let renderer = super::super::tests::headless_renderer();
        let mut header = app.column_header(id, true, width);
        let mut tree = Tree::new(header.as_widget());
        let size = iced::Size::new(width, design::PRIMARY_CONTROL_SIZE);
        let node =
            header
                .as_widget_mut()
                .layout(&mut tree, &renderer, &layout::Limits::new(size, size));
        let mut title = TitleBounds(None);
        header
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut title);
        assert!(
            title.0.expect("actual mounted title text").width
                >= measure_tab_title("WWWWWWWWWWWWWWWWWWWW"),
            "the real title allocation must fit, after its icon, padding and controls"
        );
    }

    #[test]
    fn new_terminal_after_worktree_move_keeps_existing_numbers_distinct() {
        let mut app = tab_fixture(3, 1600.0);
        let moved = PathBuf::from("/moved-worktree");
        for card in &app.durable.cards {
            app.worktree_owners.insert(card.id, moved.clone());
        }
        let card = super::super::tests::terminal_card_state();
        let added = card.id;
        app.worktree_owners.insert(added, moved);
        Arc::make_mut(&mut app.durable).cards.insert(0, card);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        drop(app.sync_chrome());
        assert!(app.card_chrome[&added].title.ends_with("(4)"));
    }

    #[test]
    fn reported_terminal_titles_keep_osc_precedence_over_launch_fallbacks() {
        let mut app = tab_fixture(1, 1600.0);
        let id = app.durable.cards[0].id;
        if let CardKind::Terminal(session) = &mut Arc::make_mut(&mut app.durable).cards[0].kind {
            session.launch.working_directory = PathBuf::from("/repo/alpha");
        }
        for reported in ["alpha", "Terminal", "/repo/alpha"] {
            app.apply_reported_title(id, reported.into());
            drop(app.sync_chrome());
            assert_eq!(app.card_chrome[&id].title, format!("{reported} (1)"));
            assert!(app.core.window.header_title.starts_with(reported));
        }
    }

    #[test]
    fn terminal_ordinals_survive_an_unrelated_tab_closing() {
        let mut app = tab_fixture(3, 1600.0);
        let survivor = app.durable.cards[2].id;
        let before = app.card_chrome[&survivor].title.clone();
        assert!(before.ends_with("(3)"), "{before}");
        assert!(
            app.card_chrome[&survivor]
                .compact_title
                .as_deref()
                .unwrap()
                .starts_with("3 ")
        );
        Arc::make_mut(&mut app.durable).cards.remove(0);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        drop(app.sync_chrome());
        assert_eq!(app.card_chrome[&survivor].title, before);
        assert_eq!(
            app.chrome_ui.terminal_ordinals.len(),
            2,
            "retired pane IDs must not accumulate in the ordinal cache"
        );
        let added = super::super::tests::terminal_card_state();
        let added_id = added.id;
        Arc::make_mut(&mut app.durable).cards.push(added);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        drop(app.sync_chrome());
        assert!(
            app.card_chrome[&added_id].title.ends_with("(4)"),
            "retiring an ID must preserve the worktree's next ordinal"
        );
        assert_eq!(app.card_chrome[&survivor].title, before);
    }
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
                message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(value) if value == "x")
            )));

            let open = app.update_chrome(Action::Rename(card));
            let during = rename_fixture_step(&app, &mut tree, &renderer, open, true);
            assert!(during.iter().any(|message| matches!(
                message, Message::ChromeUi(Action::RenameChanged(value)) if value == "x"
            )));
            assert!(
                !during
                    .iter()
                    .any(|message| matches!(message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(_))))
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
                message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(value) if value == "x")
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
            message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(value) if value == "x")
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
                .any(|message| matches!(message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(_))))
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
                .any(|message| matches!(message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(_))))
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

    fn focus_main_window(app: &mut Cagents, focused: bool) {
        app.core
            .set_main_window_id(Some(cosmic::iced::window::Id::RESERVED));
        let _ = app.update(Message::RefreshUi(
            super::super::refresh_ui::Action::Focused(cosmic::iced::window::Id::RESERVED, focused),
        ));
        assert_eq!(app.refresh_ui.focused, focused);
    }

    /// The expiry timer runs only while a confirmation is waiting to retire
    /// and someone could be reading it: the window in the foreground and the
    /// notices closed. Time spent otherwise is added back to the deadline. It
    /// fires as a message that retires what is due.
    #[test]
    fn confirmations_retire_only_while_someone_could_read_them() {
        // A measurable pause, so a resume that shifted nothing cannot pass.
        let pause = || std::thread::sleep(Duration::from_millis(5));
        let mut app = super::super::tests::context_inspector_test_app(true);
        focus_main_window(&mut app, true);
        app.notices.record(TransientNotice::error("Still failing."));
        assert_eq!(
            app.notices.next_expiry(),
            None,
            "an idle window arms no timer"
        );
        let recorded = Instant::now();
        app.notices
            .record_at(TransientNotice::success("Account added."), recorded);
        let due = recorded + NOTICE_SUCCESS_LIFETIME;
        assert_eq!(app.notices.next_expiry(), Some(due));

        let _ = app.update(Message::ChromeUi(Action::ToggleNotices));
        assert_eq!(app.chrome_ui.popup, Some(Popup::Notices));
        assert_eq!(
            app.notices.next_expiry(),
            None,
            "nothing retires while read"
        );
        let paused = app.notices.paused_at.expect("reading pauses the countdown");
        // A timer that fired just before the popup opened changes nothing.
        let _ = app.update(Message::ExpireNotices);
        assert_eq!(app.notices.len(), 2);
        assert_eq!(app.chrome_ui.popup, Some(Popup::Notices));
        pause();
        let before = Instant::now();
        let _ = app.update(Message::ChromeUi(Action::Close));
        let after = Instant::now();
        let resumed = app.notices.next_expiry().expect("the countdown resumes");
        // Moved on by exactly the pause, which ended inside the update.
        assert!(
            due + (before - paused) <= resumed && resumed <= due + (after - paused),
            "the time spent reading is not counted"
        );

        focus_main_window(&mut app, false);
        assert_eq!(app.notices.next_expiry(), None, "nothing retires unseen");
        let paused = app.notices.paused_at.expect("the background pauses it");
        let _ = app.update(Message::ExpireNotices);
        assert_eq!(app.notices.len(), 2);
        pause();
        let before = Instant::now();
        focus_main_window(&mut app, true);
        let after = Instant::now();
        let refocused = app.notices.next_expiry().expect("focus resumes it");
        assert!(
            resumed + (before - paused) <= refocused && refocused <= resumed + (after - paused),
            "time in the background is not counted"
        );

        // Backdated, so the repeat is already due when the timer fires.
        app.notices.record_at(
            TransientNotice::success("Account added."),
            recorded - NOTICE_SUCCESS_LIFETIME,
        );
        let _ = app.update(Message::ExpireNotices);
        assert_eq!(app.notices.len(), 1);
        assert_eq!(app.notices.severity(), Some(NoticeSeverity::Error));
        assert_eq!(app.notices.next_expiry(), None);
    }

    /// The header's notices control beside the inspector toggle, as the
    /// window header mounts them.
    fn notice_header(app: &Cagents) -> Element<'_, Message> {
        widget::row::with_children(vec![
            app.sidebar_toggle(),
            widget::row::with_children(app.notice_control()).into(),
        ])
        .into()
    }

    fn notice_focus_fixture(app: &Cagents) -> Element<'_, Message> {
        widget::column::with_children(vec![
            notice_header(app),
            rename_fixture_view(app),
            app.dialog()
                .unwrap_or_else(|| widget::Space::new().width(1).height(1).into()),
        ])
        .into()
    }

    /// Mounts the header as `app` now draws it, then applies `task`'s widget
    /// operations to it, as the runtime does once an update has been drawn.
    /// Returns the messages the task produced and the ids holding focus after.
    fn apply_to_header(
        app: &Cagents,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        task: Task<Message>,
    ) -> (Vec<Message>, Vec<String>) {
        use iced::advanced::widget::operation::{Focusable, Outcome};
        use iced::advanced::{Layout, layout, widget::Operation};
        use iced::{Rectangle, Size, futures::StreamExt};

        struct Focused(Vec<String>);
        impl Operation for Focused {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                _: Rectangle,
                state: &mut dyn Focusable,
            ) {
                if let Some(id) = id
                    && state.is_focused()
                {
                    self.0.push(id.to_string());
                }
            }
        }

        let mut element = notice_focus_fixture(app);
        tree.diff(element.as_widget_mut());
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(1000.0, 1600.0)),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut messages = Vec::new();
        runtime.block_on(async {
            let Some(mut stream) = iced::runtime::task::into_stream(task) else {
                return;
            };
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
                            if let Outcome::Chain(chained) = operation.finish() {
                                next = Some(chained);
                            }
                        }
                    }
                    iced::runtime::Action::Output(cosmic::Action::App(message)) => {
                        messages.push(message);
                    }
                    _ => {}
                }
            }
        });
        runtime.shutdown_background();
        let mut focused = Focused(Vec::new());
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), renderer, &mut focused);
        (messages, focused.0)
    }

    /// The notices control goes with the last notice, whatever removes it.
    /// When it held the keyboard, the keyboard moves to the inspector toggle
    /// beside it rather than to nowhere; otherwise it stays where it is.
    #[test]
    fn the_last_notice_going_moves_the_keyboard_off_its_control() {
        let toggle = context_inspector_toggle_id().to_string();
        let control = widget::Id::new(NOTICES_CONTROL_ID).to_string();
        // A confirmation retiring on its timer, and a failure withdrawn by
        // the reader giving up on what failed.
        let removals: [(&str, fn(&mut Cagents) -> Task<Message>); 2] = [
            ("timer", |app| {
                app.notices.record_at(
                    TransientNotice::success("Copied."),
                    Instant::now() - NOTICE_SUCCESS_LIFETIME,
                );
                app.update(Message::ExpireNotices)
            }),
            ("link cancel", |app| {
                let _ = app.update(Message::AccountUi(
                    super::super::account_ui::Action::LinkStart(crate::agent::Provider::Claude),
                ));
                let _ = app.update(Message::AccountUi(
                    super::super::account_ui::Action::LinkReady {
                        key: uuid::Uuid::new_v4(),
                        result: Err("no credentials there".into()),
                    },
                ));
                app.update(Message::AccountUi(
                    super::super::account_ui::Action::LinkCancel,
                ))
            }),
        ];
        for (path, remove) in removals {
            for holds_keyboard in [true, false] {
                let mut app = super::super::tests::context_inspector_test_app(true);
                focus_main_window(&mut app, true);
                app.notices.record(TransientNotice::warning("Placeholder."));
                let renderer = super::super::tests::headless_renderer();
                let mut tree = iced::advanced::widget::Tree::new(notice_header(&app).as_widget());
                let target = if holds_keyboard {
                    widget::Id::new(NOTICES_CONTROL_ID)
                } else {
                    widget::Id::new("elsewhere")
                };
                let (_, focused) = apply_to_header(
                    &app,
                    &mut tree,
                    &renderer,
                    iced::widget::operation::focus(target),
                );
                assert_eq!(focused == [control.clone()], holds_keyboard, "{path}");
                app.notices.clear();

                let query = remove(&mut app);
                assert!(!app.has_chrome_notices(), "{path}");
                let (mut messages, _) = apply_to_header(&app, &mut tree, &renderer, query);
                assert!(
                    matches!(messages.as_slice(), [Message::NoticeControlRetired]),
                    "{path}: {messages:?}"
                );
                let moved = app.update(messages.pop().unwrap());
                assert!(app.notice_control().is_none(), "{path}");
                let (_, focused) = apply_to_header(&app, &mut tree, &renderer, moved);
                if holds_keyboard {
                    assert_eq!(focused, [toggle.clone()], "{path}");
                } else {
                    assert!(focused.is_empty(), "{path}: {focused:?}");
                }
            }
        }

        // A tree without the control answers nothing; the control still goes.
        let mut app = super::super::tests::context_inspector_test_app(true);
        focus_main_window(&mut app, true);
        app.notices.record_at(
            TransientNotice::success("Copied."),
            Instant::now() - NOTICE_SUCCESS_LIFETIME,
        );
        let query = app.update(Message::ExpireNotices);
        assert!(app.notice_control().is_some(), "kept for the query");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut messages = runtime.block_on(async {
            use cosmic::iced::futures::StreamExt;
            let mut messages = Vec::new();
            let mut stream = cosmic::iced::runtime::task::into_stream(query).unwrap();
            while let Some(action) = stream.next().await {
                match action {
                    cosmic::iced::runtime::Action::Widget(operation) => drop(operation),
                    cosmic::iced::runtime::Action::Output(cosmic::Action::App(message)) => {
                        messages.push(message);
                    }
                    _ => {}
                }
            }
            messages
        });
        assert!(
            matches!(messages.as_slice(), [Message::NoticeControlRetired]),
            "{messages:?}"
        );
        let _ = app.update(messages.pop().unwrap());
        assert!(app.notice_control().is_none());
    }

    #[test]
    fn retiring_notices_never_take_focus_from_a_newer_keyboard_owner() {
        use iced::advanced::widget::Tree;

        // Interleave both before the widget operation and after its reply:
        // neither a stale focus query nor its cleanup may override new UI.
        for before_operation in [false, true] {
            for owner in ["palette", "rename", "dialog", "editor"] {
                let mut app = rename_test_app();
                focus_main_window(&mut app, true);
                app.notices.record_at(
                    TransientNotice::success("Copied."),
                    Instant::now() - NOTICE_SUCCESS_LIFETIME,
                );
                let renderer = super::super::tests::headless_renderer();
                let mut tree = Tree::new(notice_focus_fixture(&app).as_widget());
                let (_, focused) = apply_to_header(
                    &app,
                    &mut tree,
                    &renderer,
                    iced::widget::operation::focus(widget::Id::new(NOTICES_CONTROL_ID)),
                );
                assert_eq!(focused, [NOTICES_CONTROL_ID.to_owned()]);
                let mut retirement = Some(app.update(Message::ExpireNotices));
                let mut replies = if before_operation {
                    Vec::new()
                } else {
                    apply_to_header(&app, &mut tree, &renderer, retirement.take().unwrap()).0
                };
                let (open, destination) = match owner {
                    "palette" => (
                        app.update_palette(palette_ui::Action::Open),
                        widget::Id::new("palette.query"),
                    ),
                    "rename" => (
                        app.update_chrome(Action::Rename(app.workspace.focused().unwrap())),
                        widget::Id::new("chrome.rename.input"),
                    ),
                    "dialog" => {
                        // Mount the real confirmation without requesting any
                        // daemon operation; Cancel is an actual dialog widget.
                        app.daemon_health_ui.confirmation = true;
                        let destination = widget::Id::new("daemon-health.cancel");
                        (
                            iced::widget::operation::focus(destination.clone()),
                            destination,
                        )
                    }
                    _ => {
                        let destination = crate::settings_widgets::search_id();
                        (
                            iced::widget::operation::focus(destination.clone()),
                            destination,
                        )
                    }
                };
                let (_, focused) = apply_to_header(&app, &mut tree, &renderer, open);
                assert_eq!(focused, [destination.to_string()], "{owner}");
                if let Some(retirement) = retirement {
                    replies = apply_to_header(&app, &mut tree, &renderer, retirement).0;
                }
                assert!(matches!(
                    replies.as_slice(),
                    [Message::NoticeControlRetired]
                ));
                let cleanup = app.update(replies.pop().unwrap());
                let (_, focused) = apply_to_header(&app, &mut tree, &renderer, cleanup);
                assert_eq!(
                    focused,
                    [destination.to_string()],
                    "{owner}, before operation: {before_operation}"
                );
            }
        }
    }

    #[test]
    fn a_new_notice_cancels_the_pending_focus_handoff() {
        use iced::advanced::widget::Tree;

        let mut app = rename_test_app();
        focus_main_window(&mut app, true);
        app.notices.record_at(
            TransientNotice::success("Copied."),
            Instant::now() - NOTICE_SUCCESS_LIFETIME,
        );
        let renderer = super::super::tests::headless_renderer();
        let mut tree = Tree::new(notice_focus_fixture(&app).as_widget());
        let _ = apply_to_header(
            &app,
            &mut tree,
            &renderer,
            iced::widget::operation::focus(widget::Id::new(NOTICES_CONTROL_ID)),
        );
        let retirement = app.update(Message::ExpireNotices);
        app.notices
            .record(TransientNotice::warning("A newer report."));
        let (mut replies, focused) = apply_to_header(&app, &mut tree, &renderer, retirement);
        assert_eq!(focused, [NOTICES_CONTROL_ID.to_owned()]);
        assert!(matches!(
            replies.as_slice(),
            [Message::NoticeControlRetired]
        ));
        let cleanup = app.update(replies.pop().unwrap());
        let (_, focused) = apply_to_header(&app, &mut tree, &renderer, cleanup);
        assert_eq!(focused, [NOTICES_CONTROL_ID.to_owned()]);
        assert_eq!(app.notices.len(), 1);
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
            assert_eq!(app.card_chrome[&id].title, format!("{expected} (1)"));
            assert_eq!(app.core.window.header_title, format!("{expected} (1)"));
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

    pub(super) fn notices_are_open(&self) -> bool {
        self.popup == Some(Popup::Notices)
    }

    pub(super) fn close(&mut self) {
        self.popup = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Popup {
    NewTab,
    Notices,
    Attention,
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
    ToggleAttention,
    AttentionPage(usize),
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
    MoveTo(crate::model::ChannelId),
    MoveToChannel,
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
            PaneMenuCommand::Fork(target) => {
                return Message::ChatUi(chat_ui::Action::BranchPicker(target, true));
            }
            PaneMenuCommand::ReviewChanges(target) => {
                return Message::ChatUi(chat_ui::Action::ChangesPicker(target, true));
            }
            PaneMenuCommand::MoveTo(channel) => {
                return Message::MovePaneToChannel { card: id, channel };
            }
            PaneMenuCommand::MoveToChannel => {
                return Message::Palette(palette_ui::Action::MovePane(id));
            }
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

const NOTICES_CONTROL_ID: &str = "chrome.notices";
const RETIRING_NOTICES_ID: &str = "chrome.notices.retiring";
const NOTICES_CLOSE_ID: &str = "chrome.notices.close";
const NOTICES_DISMISS_ALL_ID: &str = "chrome.notices.dismiss";

/// Check ownership and transfer focus in one runtime operation chain. An
/// asynchronous boolean reply would become stale if another field, dialog or
/// popup took focus before the application handled it.
struct RetireNoticeFocus {
    source: widget::Id,
    marker: widget::Id,
    target: widget::Id,
    retiring: bool,
    focused: bool,
    target_present: bool,
}

impl iced::advanced::widget::Operation for RetireNoticeFocus {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation)) {
        visit(self);
    }

    fn container(&mut self, id: Option<&widget::Id>, _: iced::Rectangle) {
        self.retiring |= id == Some(&self.marker);
    }

    fn focusable(
        &mut self,
        id: Option<&widget::Id>,
        _: iced::Rectangle,
        state: &mut dyn iced::advanced::widget::operation::Focusable,
    ) {
        if id == Some(&self.source) {
            self.focused |= state.is_focused();
        }
        self.target_present |= id == Some(&self.target);
    }

    fn finish(&self) -> iced::advanced::widget::operation::Outcome<()> {
        use iced::advanced::widget::operation::{Outcome, focusable};
        if self.retiring && self.focused && self.target_present {
            // Iced applies chained operations synchronously, without an
            // intervening application message that could change the owner.
            Outcome::Chain(Box::new(focusable::focus(self.target.clone())))
        } else {
            Outcome::None
        }
    }
}

/// The deadline a notice expiry timer is armed for. As the subscription's
/// identity, an earlier deadline replaces the timer and an unchanged one
/// keeps it.
#[derive(Hash)]
struct NoticeExpiry(Instant);

fn notice_expiry_stream(
    expiry: &NoticeExpiry,
) -> impl cosmic::iced::futures::Stream<Item = Message> + use<> {
    let deadline = tokio::time::Instant::from_std(expiry.0);
    cosmic::iced::futures::stream::once(async move {
        tokio::time::sleep_until(deadline).await;
        Message::ExpireNotices
    })
}

fn notice_dismiss_id(id: u64) -> String {
    format!("chrome.notices.dismiss.{id}")
}

fn notice_details_id(id: u64) -> String {
    format!("chrome.notices.details.{id}")
}

fn notice_open_id(id: u64) -> String {
    format!("chrome.notices.open.{id}")
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
            Popup::Attention => {
                let mut ring = vec![Cow::Borrowed("attention.close")];
                let notices = if self.attention_backend_current() {
                    self.attention_notices()
                } else {
                    Vec::new()
                };
                let page = self
                    .chrome_ui
                    .attention_page
                    .min(notices.len().saturating_sub(1) / 20);
                ring.extend(
                    notices
                        .iter()
                        .skip(page * 20)
                        .take(20)
                        .map(|notice| Cow::Owned(format!("attention.open.{}", notice.id))),
                );
                if page > 0 {
                    ring.push(Cow::Borrowed("attention.previous"));
                }
                if (page + 1) * 20 < notices.len() {
                    ring.push(Cow::Borrowed("attention.next"));
                }
                ring
            }
            Popup::Notices => {
                let mut ring = Vec::new();
                if !self.notices.is_empty() {
                    ring.push(Cow::Borrowed(NOTICES_DISMISS_ALL_ID));
                }
                ring.push(Cow::Borrowed(NOTICES_CLOSE_ID));
                for entry in self.notices.recent() {
                    if self.notice_card_target(entry.id).is_some() {
                        ring.push(Cow::Owned(notice_open_id(entry.id)));
                    }
                    if entry.notice.details.is_some() {
                        ring.push(Cow::Owned(notice_details_id(entry.id)));
                    }
                    ring.push(Cow::Owned(notice_dismiss_id(entry.id)));
                }
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
        if matches!(action, Action::ToggleAttention | Action::AttentionPage(_))
            && self.focused_repository_is_saving()
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
                chat_ui::invalidate_copy_intent();
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
            Action::AttentionPage(page) => {
                if !self.attention_is_open() || !self.attention_backend_current() {
                    return Task::none();
                }
                self.chrome_ui.attention_page =
                    page.min(self.attention_notices().len().saturating_sub(1) / 20);
                self.chrome_ui.focus_index = 0;
                self.focus_popup_control(self.popup_focus_ring(Popup::Attention))
            }
            Action::ToggleNewTab
            | Action::ToggleNotices
            | Action::ToggleAttention
            | Action::ShowNoticesFor(_) => {
                let popup = if matches!(action, Action::ToggleNewTab) {
                    Popup::NewTab
                } else if matches!(action, Action::ToggleAttention) {
                    Popup::Attention
                } else {
                    Popup::Notices
                };
                if self.chrome_ui.popup == Some(popup) {
                    if matches!(action, Action::ShowNoticesFor(_)) {
                        return Task::none();
                    }
                    return self.update_chrome(Action::Close);
                }
                if popup == Popup::Attention {
                    self.chrome_ui.attention_page = 0;
                }
                self.chrome_ui.popup = Some(popup);
                // The notices popup opens on its close control, which is not
                // the first stop when "Dismiss all" precedes it.
                let ring = self.popup_focus_ring(popup);
                self.chrome_ui.focus_index = match popup {
                    Popup::NewTab | Popup::Attention => 0,
                    Popup::Notices => ring
                        .iter()
                        .position(|id| id == NOTICES_CLOSE_ID)
                        .unwrap_or_default(),
                };
                self.focus_popup_control(ring)
            }
            Action::Close => {
                let popup = self.chrome_ui.popup.take();
                if popup == Some(Popup::Attention)
                    && (self.modal_is_open() || self.focused_repository_is_saving())
                {
                    // A queued outside-close cannot unfocus a dialog/editor
                    // that acquired the keyboard after this overlay was drawn.
                    return Task::none();
                }
                popup.map_or_else(Task::none, |popup| {
                    iced::widget::operation::focus(widget::Id::new(match popup {
                        Popup::NewTab => TOOLBAR_FOCUS_ID,
                        Popup::Notices => NOTICES_CONTROL_ID,
                        Popup::Attention => "attention.control",
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

    /// Wakes the shell when the earliest confirmation is due to retire, and
    /// never otherwise: with nothing waiting to retire, or the countdown
    /// paused, there is no timer.
    pub(super) fn notice_expiry_subscription(&self) -> Subscription<Message> {
        self.notices
            .next_expiry()
            .map_or_else(Subscription::none, |deadline| {
                Subscription::run_with(NoticeExpiry(deadline), notice_expiry_stream)
            })
    }

    /// Confirmations count down only while someone could be reading them as
    /// they appear: with the main window in the foreground and the notices
    /// closed. While the notices are open nothing retires, so an entry does
    /// not vanish from under the pointer or the keyboard; and a confirmation
    /// raised behind another window is still there when the user returns.
    /// Either way the countdown carries on from where it stopped.
    pub(super) fn sync_notice_countdown(&mut self) {
        let now = Instant::now();
        if self.refresh_ui.focused && self.chrome_ui.popup != Some(Popup::Notices) {
            self.notices.resume(now);
        } else {
            self.notices.pause(now);
        }
    }

    /// The notices control goes with the last notice, whatever removed it:
    /// a confirmation retiring, a dismissal, a success resolving a failure,
    /// a pane closing. Holding the keyboard, it would take the keyboard with
    /// it. So it stays until the widget operation conditionally hands focus
    /// to the inspector toggle beside it. The cleanup reply never moves focus.
    /// `before` is the control's severity before this update, if it showed.
    ///
    /// With the notices open, `reconcile_notice_popup` closes them and moves
    /// focus the same way.
    pub(super) fn retire_notice_control(
        &mut self,
        before: Option<NoticeSeverity>,
    ) -> Task<Message> {
        let Some(severity) = before else {
            return Task::none();
        };
        if self.has_chrome_notices()
            || self.chrome_ui.popup == Some(Popup::Notices)
            || self.chrome_ui.retiring_notice_control.is_some()
        {
            return Task::none();
        }
        self.chrome_ui.retiring_notice_control = Some(severity);
        // Collect even an empty operation outcome so an absent control still
        // retires. Focus is settled in the widget tree, never by this reply.
        iced::runtime::task::widget(RetireNoticeFocus {
            source: widget::Id::new(NOTICES_CONTROL_ID),
            marker: widget::Id::new(RETIRING_NOTICES_ID),
            target: context_inspector_toggle_id(),
            retiring: false,
            focused: false,
            target_present: false,
        })
        .collect()
        .map(|_| cosmic::Action::App(Message::NoticeControlRetired))
    }

    pub(super) fn notice_control_retired(&mut self) -> Task<Message> {
        self.chrome_ui.retiring_notice_control = None;
        Task::none()
    }

    pub(super) fn reconcile_notice_popup(&mut self) -> Task<Message> {
        if self.chrome_ui.popup == Some(Popup::Notices) && !self.has_chrome_notices() {
            self.chrome_ui.close();
            iced::widget::operation::focus(context_inspector_toggle_id()).map(cosmic::Action::App)
        } else {
            Task::none()
        }
    }

    /// The severity the notices control shows, or `None` when there is no
    /// control because there is nothing to show.
    pub(super) fn notice_control_severity(&self) -> Option<NoticeSeverity> {
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
        Some(recorded_severity.unwrap_or(NoticeSeverity::Warning))
    }

    pub(super) fn attention_is_open(&self) -> bool {
        self.chrome_ui.popup == Some(Popup::Attention)
    }

    /// The journal's own keyboard layer must not veto its explicit action.
    /// Inspect the unchanged navigation predicate with only that popup absent;
    /// restore it immediately, without focus operations or event processing.
    pub(super) fn with_attention_dismissed(&mut self, allowed: impl FnOnce(&Self) -> bool) -> bool {
        if !self.attention_is_open() {
            return false;
        }
        let popup = self.chrome_ui.popup.take();
        let admitted = allowed(self);
        self.chrome_ui.popup = popup;
        admitted
    }

    pub(super) fn close_attention(&mut self) {
        if self.attention_is_open() {
            self.chrome_ui.close();
        }
    }

    fn attention_and_notice_controls(&self) -> Element<'_, Message> {
        widget::row::with_children(
            std::iter::once(self.attention_control()).chain(self.notice_control()),
        )
        .spacing(space::XXS)
        .into()
    }

    fn attention_control(&self) -> Element<'_, Message> {
        let control = crate::ui_controls::icon_button(Icon::NotificationSettings, "Attention")
            .id(widget::Id::new("attention.control"))
            .selected(self.attention_is_open())
            .on_press_maybe(
                (!self.modal_is_open()).then_some(Message::ChromeUi(Action::ToggleAttention)),
            );
        let content =
            (self.attention_is_open() && !self.modal_is_open()).then(|| self.attention_journal());
        crate::ui_controls::popover(
            control,
            content,
            widget::popover::Position::Bottom,
            Message::ChromeUi(Action::Close),
        )
    }

    pub(super) fn attention_journal(&self) -> Element<'_, Message> {
        let notices = if self.attention_backend_current() {
            self.attention_notices()
        } else {
            Vec::new()
        };
        let page = self
            .chrome_ui
            .attention_page
            .min(notices.len().saturating_sub(1) / 20);
        let mut entries = Vec::new();
        if !self.attention_backend_current() {
            entries.push(design::caption("The notification backend is not connected. Retained entries cannot be opened until it reconnects.")
                .width(Length::Fill).wrapping(iced::widget::text::Wrapping::WordOrGlyph).into());
        } else if notices.is_empty() {
            entries.push(
                design::caption("No attention entries.")
                    .width(Length::Fill)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
            );
        }
        for (index, notice) in notices.iter().skip(page * 20).take(20).enumerate() {
            let message = crate::notifications::presentation::message(
                &notice.event,
                self.settings.notifications.message_previews,
            );
            let mut paragraphs = vec![
                design::heading(message.title.clone())
                    .width(Length::Fill)
                    .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                    .into(),
            ];
            if !message.body.is_empty() && message.body != message.title {
                paragraphs.push(
                    design::body(message.body)
                        .width(Length::Fill)
                        .wrapping(iced::widget::text::Wrapping::WordOrGlyph)
                        .into(),
                );
            }
            paragraphs.push(
                crate::ui_controls::button::quiet(message.action)
                    .id(widget::Id::new(format!("attention.open.{}", notice.id)))
                    .name(format!(
                        "{} — {} — entry {}",
                        message.action,
                        message.title,
                        page * 20 + index + 1
                    ))
                    .on_press(self.attention_notice_action(notice))
                    .into(),
            );
            let body = widget::column::with_children(paragraphs)
                .spacing(space::XS)
                .width(Length::Fill);
            entries.push(crate::ui_controls::owned(
                self.attention_notice_owner(notice),
                widget::container(body)
                    .padding(space::S)
                    .width(Length::Fill),
            ));
        }
        let mut navigation = Vec::new();
        if page > 0 {
            navigation.push(
                crate::ui_controls::button::quiet("Previous entries")
                    .id(widget::Id::new("attention.previous"))
                    .on_press(Message::ChromeUi(Action::AttentionPage(page - 1)))
                    .into(),
            );
        }
        if (page + 1) * 20 < notices.len() {
            navigation.push(
                crate::ui_controls::button::quiet("More entries")
                    .id(widget::Id::new("attention.next"))
                    .on_press(Message::ChromeUi(Action::AttentionPage(page + 1)))
                    .into(),
            );
        }
        let header = widget::row::with_children(vec![
            design::heading("Attention").width(Length::Fill).into(),
            command(
                Icon::Close,
                "Close Attention",
                widget::Id::new("attention.close"),
                Some(Message::ChromeUi(Action::Close)),
            ),
        ])
        .align_y(Alignment::Center)
        .spacing(space::S);
        widget::container(
            widget::column::with_children(vec![
                header.into(),
                // Shrink children are measured in order. Reserve the page
                // controls before the list can consume the remaining height.
                widget::row::with_children(navigation)
                    .spacing(space::S)
                    .into(),
                widget::scrollable(
                    widget::column::with_children(entries)
                        .spacing(space::XS)
                        .padding(design::with_vertical_rail(iced::Padding::new(0.0))),
                )
                .direction(iced::widget::scrollable::Direction::Vertical(
                    design::scrollbar(),
                ))
                .height(Length::Shrink)
                .into(),
            ])
            .spacing(space::S),
        )
        .padding(space::S)
        .width((self.window_width - 16.0).clamp(160.0, 420.0))
        .max_height(408)
        .into()
    }

    fn notice_control(&self) -> Option<Element<'_, Message>> {
        let severity = self
            .notice_control_severity()
            .or(self.chrome_ui.retiring_notice_control)?;
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
        .id(widget::Id::new(NOTICES_CONTROL_ID))
        .tone(design::Tone::Themed(match severity {
            NoticeSeverity::Success => design::success_color,
            NoticeSeverity::Warning => design::warning_color,
            NoticeSeverity::Error => design::destructive_color,
        }));
        let content =
            (self.chrome_ui.popup == Some(Popup::Notices) && !self.modal_is_open()).then(|| {
                let mut notices = Vec::with_capacity(self.notices.len() + 1);
                for entry in self.notices.recent() {
                    let severity = entry.notice.severity;
                    let mut lines = vec![super::notice_body(&entry.notice.message)];
                    let mut actions = Vec::new();
                    if let Some(card) = self.notice_card_target(entry.id) {
                        let name = self
                            .notice_pane_name(card)
                            .unwrap_or_else(|| "the related pane".into());
                        actions.push(super::notice_surface_action(
                            severity,
                            "Open pane",
                            format!("Open {name}"),
                            widget::Id::new(notice_open_id(entry.id)),
                            Message::OpenNoticePane(entry.id),
                        ));
                    }
                    if entry.notice.details.is_some() {
                        actions.push(super::notice_surface_action(
                            severity,
                            if entry.details_open {
                                "Collapse details"
                            } else {
                                "Expand details"
                            },
                            if entry.details_open {
                                "Collapse diagnostic details"
                            } else {
                                "Expand full diagnostic details"
                            },
                            widget::Id::new(notice_details_id(entry.id)),
                            Message::ToggleNoticeDetails(entry.id),
                        ));
                    }
                    if !actions.is_empty() {
                        lines.push(
                            widget::column::with_children(actions)
                                .spacing(space::XS)
                                .into(),
                        );
                    }
                    if let Some(details) = &entry.notice.details {
                        lines.push(if entry.details_open {
                            super::notice_surface_detail(severity, details)
                        } else {
                            super::notice_surface_details(severity, details)
                        });
                    }
                    if entry.repeats > 1 {
                        lines.push(super::notice_surface_detail(
                            severity,
                            format!("Repeated {} times", entry.repeats),
                        ));
                    }
                    let body: Element<'_, Message> = if lines.len() > 1 {
                        widget::column::with_children(lines)
                            .spacing(space::XS)
                            .into()
                    } else {
                        lines.pop().expect("a notice has its message")
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
            // Keep the wrapper and button shape stable through retirement so
            // their focus state survives. Only a still-retiring control with
            // no newer keyboard layer advertises the handoff marker.
            widget::container(control).id(widget::Id::new(
                if self.chrome_ui.retiring_notice_control.is_some()
                    && !self.has_chrome_notices()
                    && !self.modal_is_open()
                    && !self.palette.is_open()
                    && self.chrome_ui.popup.is_none()
                    && self.chrome_ui.rename.is_none()
                {
                    RETIRING_NOTICES_ID
                } else {
                    "chrome.notices.anchor"
                },
            )),
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
        let attention = if !self.context_inspector_visible() {
            design::PRIMARY_CONTROL_SIZE + 2.0
        } else {
            0.0
        };
        let width = navigator + controls + update + notice + attention + 16.0;
        if self.workspace.viewport_width() > 0.0 {
            width.min(self.workspace.viewport_width())
        } else {
            width
        }
    }

    fn tab_strip_geometry(&self) -> (f32, f32) {
        let available = (self.workspace.viewport_width()
            - self.workspace_header_width()
            - design::PRIMARY_CONTROL_SIZE
            - space::XXS)
            .max(0.0);
        // Reordering uses one shared width. Derive that width from the titles
        // instead of giving every agent the same eight-character title budget.
        let desired = self
            .workspace
            .order()
            .filter_map(|id| self.card_chrome.get(&id))
            .map(|chrome| chrome.title_width + 144.0)
            .fold(design::TAB_MIN_WIDTH, f32::max)
            .clamp(design::TAB_MIN_WIDTH, design::TAB_MAX_WIDTH);
        let width = if self.workspace.viewport_width() > 0.0
            && self.workspace.viewport_width() < design::NARROW_WORKSPACE_WIDTH
        {
            // Keep four compact tabs visible in a tiled workspace. Full names
            // remain available in each tab's tooltip.
            let visible = self.workspace.order().count().clamp(1, 4) as f32;
            let fitting = (available / visible
                - crate::tab_strip::SPACING
                - 2.0 * crate::tab_strip::EDGE_INSET / visible)
                .max(56.0);
            if visible < 4.0 {
                desired.min(fitting)
            } else {
                fitting.min(112.0)
            }
        } else {
            desired
        };
        (available, width)
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
        let header = if self.channel_work_is_main() {
            let actions = widget::row::with_children(self.workflow_header()).spacing(space::XXS);
            self.native_header(
                iced::widget::Space::new().into(),
                Some(actions.into()),
                true,
            )
        } else {
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
            self.native_header(
                tabs.into(),
                Some(actions.into()),
                !self.context_inspector_open,
            )
        };
        let mut rows = vec![header];
        if let Some(channel) = self.channel_home_bar() {
            rows.push(channel);
        }
        rows.push(crate::design::horizontal_rule().into());
        rows.push(content);
        widget::column::with_children(rows)
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
    }

    fn channel_home_bar(&self) -> Option<Element<'_, Message>> {
        let (home, title) = self.channel_home()?;
        let generation = self.channel_home_generation();
        let workspace = self.workspace_view_generation;
        let work = self.channel_work_is_main();
        let compact = self.window_width < 800.0;
        let mut navigation = Vec::new();
        for (label, surface, selected) in [
            ("Conversations", channel_work::Surface::Conversations, !work),
            ("Work", channel_work::Surface::Work, work),
        ] {
            let action = Message::ChannelWorkNavigation(channel_work::Navigation {
                home,
                generation,
                workspace,
                surface,
            });
            navigation.push(crate::ui_controls::owned(
                (home, generation, workspace, surface),
                crate::ui_controls::view_tab(
                    crate::ui_controls::ViewTab {
                        label: label.into(),
                        description: format!("{label} for channel {title}").into(),
                        count: None,
                        selected,
                        id: None,
                        on_press: (!self.modal_is_open()).then_some(action),
                    },
                    compact,
                    None,
                ),
            ));
        }
        let navigation = widget::row::with_children(navigation)
            .spacing(space::XXS)
            .align_y(Alignment::Center);
        let identity = widget::column::with_children(vec![
            design::caption("CHANNEL").into(),
            design::heading(if title.is_empty() {
                "Untitled channel".to_owned()
            } else {
                title
            })
            .width(Length::Fill)
            .into(),
        ])
        .spacing(space::XXS)
        .width(Length::Fill);
        let content: Element<'_, Message> = if work {
            // Work's fixed leaf header already displays the captured channel
            // title. Do not repeat it or narrow it beside the navigation.
            navigation.into()
        } else if compact {
            widget::column::with_children(vec![identity.into(), navigation.into()])
                .spacing(space::XS)
                .into()
        } else {
            widget::row::with_children(vec![identity.into(), navigation.into()])
                .spacing(space::S)
                .align_y(Alignment::Center)
                .into()
        };
        Some(
            widget::container(content)
                .padding([space::XS, space::S])
                .width(Length::Fill)
                .into(),
        )
    }

    pub(super) fn workflow_header(&self) -> Vec<Element<'_, Message>> {
        if self.context_inspector_is_overlay() {
            return Vec::new();
        }
        let mut actions = Vec::with_capacity(8);
        let total = if self.channel_work_is_main() {
            0
        } else {
            self.workspace.cards().len()
        };
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
                (!panel_owns_shared_controls).then(|| self.attention_and_notice_controls()),
            )
            .into(),
            widget::row::with_children(
                (!panel_owns_shared_controls)
                    .then(|| self.update_control())
                    .flatten(),
            )
            .into(),
            widget::row::with_children(
                (!panel_owns_shared_controls && !self.channel_work_is_main())
                    .then(|| self.sidebar_toggle()),
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
        let compact = width < design::TAB_MIN_WIDTH;
        let suffix = (!compact)
            .then(|| chrome.and_then(|chrome| chrome.terminal_ordinal_suffix.as_deref()))
            .flatten();
        let visible_title = if compact {
            chrome
                .and_then(|chrome| chrome.compact_title.as_deref())
                .unwrap_or(title)
        } else {
            suffix
                .and_then(|suffix| title.strip_suffix(suffix))
                .unwrap_or(title)
        };
        let mut title_children = vec![
            self.card_icon(id).icon().size(design::ROW_ICON_SIZE).into(),
            design::body(visible_title)
                .wrapping(iced::widget::text::Wrapping::None)
                .ellipsize(iced::widget::text::Ellipsize::End(
                    EllipsizeHeightLimit::Lines(1),
                ))
                .width(Length::Fill)
                .into(),
        ];
        if let Some(suffix) = suffix {
            title_children.push(
                design::caption(suffix.trim())
                    .wrapping(iced::widget::text::Wrapping::None)
                    .into(),
            );
        }
        let title_control = widget::button::custom(
            widget::row::with_children(title_children)
                .spacing(if compact { space::XS } else { space::S })
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
        let attention = self.sidebar_state.card_attention(id);
        let tip = if attention != sidebar::AttentionSummary::default() {
            let mut tip = format!("{tip}\n{}", attention.description());
            if attention.unread != 0 && self.settings.notifications.message_previews {
                if let Some(preview) = card
                    .and_then(super::notification_ui::card_session)
                    .and_then(|session| self.notification_ui.previews.get(&session))
                {
                    tip.push('\n');
                    tip.push_str(&preview.text);
                }
            }
            Cow::Owned(tip)
        } else {
            tip
        };
        let mut children = vec![tooltip(crate::reorder::handle(title_control, id), tip)];
        let status = match self.cards.get(&id) {
            _ if self.pane_transfer_pending(id) => Some(("Moving to channel", true)),
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
        if let Some(indicator) = sidebar::attention_indicator(attention, false) {
            children.push(indicator);
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
        if (focused || self.chrome_ui.hovered_tab == Some(id)) && width >= 112.0 {
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
            .id(chrome.map_or_else(
                || card_control_id(id, "tab"),
                |chrome| chrome.tab_id.clone(),
            ))
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
            let destinations = self.pane_move_destinations(id);
            if !destinations.is_empty() {
                entries.push(widget::menu::Item::Divider);
            }
            if destinations.len() <= 5 {
                entries.extend(destinations.into_iter().map(|channel| {
                    widget::menu::Item::Button(
                        format!("Move to {}", self.pane_move_destination_label(channel)),
                        None,
                        PaneMenu(id, PaneMenuCommand::MoveTo(channel.id)),
                    )
                }));
            } else {
                entries.push(widget::menu::Item::Button(
                    "Move to channel…".into(),
                    None,
                    PaneMenu(id, PaneMenuCommand::MoveToChannel),
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
        if !self.channel_work_is_main() && self.channel_work.view.is_open() {
            let end = widget::row::with_children(vec![
                self.attention_and_notice_controls(),
                widget::row::with_children(self.update_control()).into(),
            ])
            .spacing(space::XXS)
            .align_y(Alignment::Center)
            .into();
            let heading = self.native_header(self.sidebar_toggle(), Some(end), true);
            return widget::column::with_children(vec![
                heading,
                crate::interaction::focused_escape(
                    self.channel_work
                        .view
                        .view(
                            width,
                            (self.window_height - design::HEADER_CONTENT_HEIGHT).max(0.0),
                        )
                        .map(Message::ChannelWork),
                    || Message::ChannelWorkEscape,
                ),
            ])
            .width(Length::Fill)
            .height(Length::Fill)
            .into();
        }
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
            self.attention_and_notice_controls(),
            widget::row::with_children(self.update_control()).into(),
        ])
        .spacing(space::XXS)
        .align_y(Alignment::Center)
        .into();
        let heading = self.native_header(self.sidebar_toggle(), Some(end), true);
        let tabs = widget::row::with_children(vec![
            tabs.into(),
            crate::ui_controls::button::secondary("Channel work")
                .name("Open current channel work")
                .on_press_maybe(
                    self.durable
                        .active_channel
                        .filter(|_| !self.modal_is_open())
                        .map(Message::OpenChannelWork),
                )
                .into(),
        ])
        .align_y(Alignment::Center)
        .spacing(space::XS);
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
