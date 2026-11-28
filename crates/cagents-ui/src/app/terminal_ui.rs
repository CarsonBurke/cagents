use super::*;

/// The multiline paste question's answers.
const PASTE_CANCEL: &str = "terminal-paste-cancel";
const PASTE_CONFIRM: &str = "terminal-paste-confirm";

#[derive(Clone, Debug)]
pub enum Action {
    Open(CardId),
    Close(CardId),
    Query(CardId, String),
    Case(CardId),
    Next(CardId, bool),
    Focus(CardId, bool),
    Context(
        CardId,
        iced::Point,
        Option<crate::terminal::SearchMatch>,
        Option<Url>,
    ),
    /// Closes the menu, leaving the keyboard where it is.
    CloseContext(CardId),
    /// Escape closed the menu: the keyboard goes back to `opener`.
    DismissContext(CardId, widget::Id),
    Copy(CardId, String),
    CopySelection(CardId, crate::terminal::SearchMatch),
    PasteClipboard(CardId),
    Paste(CardId, String),
    ConfirmPaste(CardId, bool),
    SelectAll(CardId),
    Clear(CardId),
    Refresh(CardId, u64),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn closed_find_rejects_delayed_results_for_its_current_request() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let card = CardId::new();
        app.terminal_find.insert(
            card,
            Find {
                query: "needle".into(),
                request: 7,
                ..Find::default()
            },
        );
        app.terminal_search_results(
            card,
            7,
            "needle".into(),
            vec![crate::terminal::SearchMatch {
                start_row: 12,
                start_column: 0,
                end_row: 12,
                end_column: 6,
            }],
            true,
        );
        let find = &app.terminal_find[&card];
        assert!(find.matches.is_empty());
        assert!(!find.truncated);
    }

    /// The terminal's menu is the native menu's rows, not accent text
    /// buttons that hug their labels (#3442): each row is as wide as the
    /// menu, Copy waits for a selection, and a link adds its own actions.
    #[test]
    fn the_terminal_menu_draws_native_rows_with_copy_waiting_for_a_selection() {
        use cosmic::iced::advanced::{layout, widget::Tree};
        use cosmic::iced::{Size, mouse};

        let mut app = super::super::tests::context_inspector_test_app(false);
        let card = CardId::new();
        let rows = |app: &Cagents| {
            let items = app.terminal_menu(card).expect("an open menu");
            let mut menu = crate::ui_controls::menu_rows(items);
            let mut tree = Tree::new(&menu);
            let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let node = menu.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(1000.0, 1000.0)),
            );
            let nodes = menu.as_widget().a11y_nodes(
                layout::Layout::new(&node),
                &tree,
                mouse::Cursor::Unavailable,
            );
            nodes
                .root()
                .iter()
                .chain(nodes.children())
                .filter(|node| node.node().role() == iced_accessibility::accesskit::Role::Button)
                .map(|node| {
                    let node = node.node();
                    let bounds = node.bounds().expect("a placed row");
                    (
                        node.label().unwrap_or_default().to_owned(),
                        !node.is_disabled(),
                        bounds.x1 - bounds.x0,
                    )
                })
                .collect::<Vec<_>>()
        };
        app.terminal_context = Some(Context {
            card,
            selection: None,
            link: Some(Url::parse("https://example.com/").unwrap()),
            point: iced::Point::ORIGIN,
        });
        let menu = rows(&app);
        assert_eq!(
            menu.iter()
                .map(|(label, enabled, _)| (label.as_str(), *enabled))
                .collect::<Vec<_>>(),
            [
                ("Copy", false),
                ("Paste", true),
                ("Select all", true),
                ("Clear scrollback", true),
                ("Find…", true),
                ("Open link", true),
                ("Copy link", true),
            ]
        );
        // "Open link" and its hint estimate 241.5 pixels, which the native
        // menu truncates; every row takes the widest row's width.
        assert!(
            menu.iter().all(|(_, _, width)| *width == 241.0),
            "every row is the menu's width: {menu:?}"
        );
        app.terminal_context = Some(Context {
            card,
            selection: Some(crate::terminal::SearchMatch {
                start_row: 0,
                start_column: 0,
                end_row: 0,
                end_column: 4,
            }),
            link: None,
            point: iced::Point::ORIGIN,
        });
        let menu = rows(&app);
        assert_eq!(menu[0].0, "Copy");
        assert!(menu[0].1, "a selection can be copied");
        assert_eq!(menu.len(), 5, "no link, no link actions");
        assert!(
            menu.iter().all(|(_, _, width)| *width == 240.0),
            "the native menu's narrowest width: {menu:?}"
        );
    }

    /// Escape closes the menu before the terminal's program can take it,
    /// and opening the link from it closes it like its other rows.
    #[test]
    fn escape_and_the_link_row_close_the_terminal_menu() {
        use cosmic::iced::advanced::{Shell, clipboard, layout, widget::Tree};
        use cosmic::iced::{Size, keyboard, mouse};

        let mut app = super::super::tests::context_inspector_test_app(false);
        let card = CardId::new();
        let link = Url::parse("https://example.com/").unwrap();
        app.terminal_context = Some(Context {
            card,
            selection: None,
            link: Some(link.clone()),
            point: iced::Point::ORIGIN,
        });
        let opener = app.card_keyboard_focus(card);
        {
            let items = app.terminal_menu(card).expect("an open menu");
            let mut menu = crate::ui_controls::escape_dismisses(
                crate::ui_controls::menu_rows(items),
                Message::TerminalFind(Action::DismissContext(card, opener.clone())),
            );
            let mut tree = Tree::new(&menu);
            let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let node = menu.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(1000.0, 1000.0)),
            );
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            let escape = keyboard::Key::Named(keyboard::key::Named::Escape);
            menu.as_widget_mut().update(
                &mut tree,
                &iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: escape.clone(),
                    modified_key: escape,
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                }),
                layout::Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &iced::Rectangle::with_size(Size::new(1000.0, 1000.0)),
            );
            assert!(shell.is_event_captured(), "the terminal never sees it");
            drop(shell);
            assert!(
                matches!(
                    messages.as_slice(),
                    [Message::TerminalFind(Action::DismissContext(dismissed, to))]
                        if *dismissed == card && *to == opener
                ),
                "Escape dismisses the menu, back to the terminal"
            );
        }
        let task = app.update(Message::TerminalFind(Action::DismissContext(
            card,
            opener.clone(),
        )));
        assert!(app.terminal_context.is_none());
        assert_eq!(
            crate::app::tests::task_focuses(task, &opener),
            (true, false),
            "the keyboard goes back to the terminal"
        );
        app.terminal_context = Some(Context {
            card,
            selection: None,
            link: Some(link.clone()),
            point: iced::Point::ORIGIN,
        });
        drop(app.update(Message::OpenLink(
            card,
            link,
            settings::LinkOpenGesture::ControlClick,
        )));
        assert!(
            app.terminal_context.is_none(),
            "the link row closes the menu"
        );
    }

    fn output_snapshot(sequence: u64) -> crate::terminal::RenderSnapshot {
        crate::terminal::RenderSnapshot {
            activity: Some(crate::model::TerminalOutputActivity {
                sequence,
                last_output_at_unix_ms: 0,
            }),
            columns: 80,
            rows: 24,
            ..crate::terminal::RenderSnapshot::empty()
        }
    }

    #[test]
    fn live_find_coalesces_output_and_refreshes_the_latest_content() {
        let first = SearchContent::of(&output_snapshot(1));
        let mut find = Find {
            open: true,
            query: "needle".into(),
            searched_content: Some(first),
            ..Find::default()
        };
        assert_eq!(find.schedule_refresh(first), None);
        let second = SearchContent::of(&output_snapshot(2));
        let pending = find.schedule_refresh(second).unwrap();
        let latest = SearchContent::of(&output_snapshot(3));
        assert_eq!(find.schedule_refresh(latest), None);
        assert!(find.begin_refresh(pending, latest));
        assert_eq!(find.searched_content, Some(latest));
        assert!(find.background_results);
        assert_eq!(find.schedule_refresh(latest), None);
        assert!(!find.begin_refresh(pending, latest));
        assert!(
            find.schedule_refresh(SearchContent::of(&output_snapshot(4)))
                .is_some()
        );
    }

    #[test]
    fn live_find_observes_reflow_but_ignores_viewport_and_frame_changes() {
        let mut snapshot = output_snapshot(7);
        let original = SearchContent::of(&snapshot);
        let mut find = Find {
            open: true,
            query: "needle".into(),
            searched_content: Some(original),
            ..Find::default()
        };
        snapshot.generation += 1;
        snapshot.scroll.offset = 20;
        assert_eq!(find.schedule_refresh(SearchContent::of(&snapshot)), None);
        snapshot.columns = 40;
        let pending = find.schedule_refresh(SearchContent::of(&snapshot)).unwrap();
        assert!(find.begin_refresh(pending, SearchContent::of(&snapshot)));
        snapshot.rows = 12;
        let pending = find.schedule_refresh(SearchContent::of(&snapshot)).unwrap();
        assert!(find.begin_refresh(pending, SearchContent::of(&snapshot)));
        snapshot.scroll.total += 1;
        assert!(
            find.schedule_refresh(SearchContent::of(&snapshot))
                .is_some()
        );
    }

    #[test]
    fn live_find_rejects_stale_timers_and_does_not_search_empty_or_closed_queries() {
        let changed = SearchContent::of(&output_snapshot(2));
        let mut find = Find {
            open: true,
            query: "needle".into(),
            ..Find::default()
        };
        let stale = find.schedule_refresh(changed).unwrap();
        find.invalidate_refresh();
        assert!(!find.begin_refresh(stale, changed));
        find.query.clear();
        assert_eq!(find.schedule_refresh(changed), None);
        find.query = "needle".into();
        let stale = find.schedule_refresh(changed).unwrap();
        find.open = false;
        assert!(!find.begin_refresh(stale, changed));
        assert_eq!(find.schedule_refresh(changed), None);
    }

    #[test]
    fn background_find_results_preserve_the_selected_match_and_do_not_steal_focus() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let card = CardId::new();
        let matched = |row| crate::terminal::SearchMatch {
            start_row: row,
            start_column: 0,
            end_row: row,
            end_column: 6,
        };
        app.terminal_find.insert(
            card,
            Find {
                open: true,
                query: "needle".into(),
                request: 7,
                matches: vec![matched(10), matched(20)].into(),
                selected: 1,
                background_results: true,
                ..Find::default()
            },
        );
        app.terminal_search_results(
            card,
            7,
            "needle".into(),
            vec![matched(5), matched(10), matched(20), matched(30)],
            false,
        );
        let find = &app.terminal_find[&card];
        assert_eq!(find.selected, 2);
        assert_eq!(find.matches[find.selected], matched(20));
        assert!(!find.focused);
        assert!(!find.background_results);

        let find = app.terminal_find.get_mut(&card).unwrap();
        find.request = 8;
        find.background_results = true;
        app.terminal_search_results(card, 8, "needle".into(), vec![matched(30)], false);
        assert_eq!(app.terminal_find[&card].selected, 0);
        assert_eq!(app.terminal_find[&card].matches.len(), 1);
    }

    #[test]
    fn live_find_refresh_consumes_its_timer_while_the_terminal_is_reconnecting() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let card = CardId::new();
        let content = SearchContent::of(&output_snapshot(2));
        let mut find = Find {
            open: true,
            query: "needle".into(),
            searched_content: Some(SearchContent::of(&output_snapshot(1))),
            ..Find::default()
        };
        let pending = find.schedule_refresh(content).unwrap();
        app.terminal_find.insert(card, find);
        app.cards.insert(card, CardRuntime::Loading);
        drop(app.handle_terminal_find(Action::Refresh(card, pending)));
        let find = app.terminal_find.get_mut(&card).unwrap();
        assert_eq!(find.refresh_pending, None);
        assert_eq!(find.searched_content, None);
        let replacement = find.schedule_refresh(content).unwrap();
        assert_ne!(replacement, pending);
        assert!(find.begin_refresh(replacement, content));
    }

    #[test]
    fn live_find_refresh_is_a_completion_during_modals_and_update_preparation() {
        let card = CardId::new();
        let refresh = Message::TerminalFind(Action::Refresh(card, 7));
        assert!(!user_workspace_message_blocked_while_modal(&refresh));
        assert!(!user_message_blocked_during_update(&refresh));
        let query = Message::TerminalFind(Action::Query(card, "needle".into()));
        assert!(user_workspace_message_blocked_while_modal(&query));
        assert!(user_message_blocked_during_update(&query));
    }

    fn escape_event() -> keyboard::Event {
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        }
    }

    #[test]
    fn captured_escape_preserves_underlying_terminal_find() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        let directory = tempfile::tempdir().unwrap();
        app.cards.insert(
            card,
            CardRuntime::Terminal(super::super::tests::unconnected_surface(
                directory.path(),
                TerminalId::new(),
            )),
        );
        app.terminal_find.insert(
            card,
            Find {
                open: true,
                ..Find::default()
            },
        );
        drop(app.update(Message::KeyEvent {
            event: escape_event(),
            captured: true,
        }));
        assert!(app.terminal_find[&card].open);
        // With no focused field answering the key, Escape on the terminal
        // surface still closes its open find bar.
        drop(app.update(Message::KeyEvent {
            event: escape_event(),
            captured: false,
        }));
        assert!(!app.terminal_find[&card].open);
    }

    #[test]
    fn focused_terminal_find_closes_on_its_first_escape_before_native_blur() {
        use cosmic::iced::{
            Event, Rectangle,
            advanced::{
                Layout, Shell, clipboard, layout,
                widget::{Tree, operation},
            },
            mouse,
        };
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.pane_inspectors = pane_inspector::PaneInspectors::default();
        let card = CardId::new();
        app.terminal_find.insert(
            card,
            Find {
                open: true,
                ..Find::default()
            },
        );
        let renderer = super::super::tests::headless_renderer();
        let mut element = app.terminal_find_view(card).unwrap();
        let mut tree = Tree::new(&element);
        let bounds = Rectangle::with_size(iced::Size::new(600.0, 48.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, bounds.size()),
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(input_id(card)),
        );
        let mut published = Vec::new();
        let mut shell = Shell::new(&mut published);
        element.as_widget_mut().update(
            &mut tree,
            &Event::Keyboard(escape_event()),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut shell,
            &bounds,
        );
        assert!(shell.is_event_captured());
        assert!(published.iter().any(|message| matches!(
            message,
            Message::TerminalFind(Action::Close(target)) if *target == card
        )));
        assert!(
            !published
                .iter()
                .any(|message| matches!(message, Message::TerminalFind(Action::Focus(_, false))))
        );
        drop(element);
        let directory = tempfile::tempdir().unwrap();
        let handle = super::super::tests::unconnected_surface(directory.path(), TerminalId::new());
        app.cards
            .insert(card, CardRuntime::Terminal(handle.clone()));
        let task = app.update(published.pop().unwrap());
        assert!(!app.terminal_find[&card].open);
        assert!(!app.terminal_find[&card].focused);
        let focused = super::super::tests::focused_element(
            TerminalWidget::new(handle)
                .id(workspace_widget::card_focus_id(card))
                .model_focused(true)
                .into(),
            task,
        );
        assert_eq!(
            focused,
            Some(workspace_widget::card_focus_id(card)),
            "closing find returns native focus to its terminal"
        );
    }

    #[test]
    fn local_clipboard_refusals_report_direct_and_confirmed_pastes_without_losing_focus_or_runtime()
    {
        use iced::futures::StreamExt;
        for full in [false, true] {
            for confirmed in [false, true] {
                let mut app = super::super::tests::context_inspector_test_app(true);
                let card = app.workspace.focused().unwrap();
                let CardKind::Terminal(session) = &app.card_state(card).unwrap().kind else {
                    panic!("terminal fixture");
                };
                let terminal = session.id;
                let directory = tempfile::tempdir().unwrap();
                let surface = super::super::tests::unconnected_surface(directory.path(), terminal);
                if full {
                    // Existing hosted::COMMAND_CAPACITY is 256. A missing
                    // socket can carry exactly one extra command while it
                    // retries, so observe all 257 admissions before asserting
                    // Full. Stopping at the first Full would race that take.
                    let deadline = Instant::now() + Duration::from_secs(3);
                    let mut accepted = 0;
                    while accepted < 257 {
                        assert!(Instant::now() < deadline, "one carried command must settle");
                        match surface.input(b"queued".to_vec()) {
                            Ok(()) => accepted += 1,
                            Err(crate::terminal::TerminalSendError::Full) => {
                                std::thread::yield_now()
                            }
                            Err(error) => panic!("unexpected local admission: {error}"),
                        }
                    }
                    assert_eq!(
                        surface.input(b"still full".to_vec()),
                        Err(crate::terminal::TerminalSendError::Full)
                    );
                }
                app.cards
                    .insert(card, CardRuntime::Terminal(surface.clone()));
                app.settings.confirm_multiline_paste = confirmed;
                let text = if full {
                    "queued\npaste".to_owned()
                } else {
                    "x".repeat(crate::terminal_ipc::MAX_INPUT_BYTES) + "\n"
                };
                let expected = if full {
                    crate::terminal::TerminalSendError::Full.to_string()
                } else {
                    crate::terminal::TerminalSendError::PasteTooLarge(text.len()).to_string()
                };
                let task = app.handle_terminal_find(Action::Paste(card, text));
                let task = if confirmed {
                    assert!(app.terminal_paste.is_some());
                    drop(task); // initial modal-focus task; confirmation follows
                    app.handle_terminal_find(Action::ConfirmPaste(card, true))
                } else {
                    task
                };
                let mut notices = 0;
                let mut focus_operations = 0;
                iced::futures::executor::block_on(async {
                    let mut stream = iced::runtime::task::into_stream(task)
                        .expect("refused paste keeps its notice task");
                    while let Some(action) = stream.next().await {
                        match action {
                            iced::runtime::Action::Output(cosmic::Action::App(
                                Message::TerminalError {
                                    card: target,
                                    message,
                                },
                            )) => {
                                assert_eq!(target, card);
                                assert_eq!(message, expected);
                                notices += 1;
                                drop(app.update(Message::TerminalError {
                                    card: target,
                                    message,
                                }));
                            }
                            iced::runtime::Action::Widget(operation) => {
                                focus_operations += 1;
                                let target =
                                    crate::ui_controls::field::text("Paste focus target", "")
                                        .id(workspace_widget::card_focus_id(card))
                                        .on_input(|_| Message::LifecycleSweep)
                                        .into();
                                let focused = super::super::tests::focused_element(
                                    target,
                                    iced::runtime::task::widget(operation)
                                        .map(|()| cosmic::Action::App(Message::LifecycleSweep)),
                                );
                                assert_eq!(
                                    focused.map(|id| id.to_string()),
                                    Some(workspace_widget::card_focus_id(card).to_string()),
                                    "actual confirmation task must restore its original card target"
                                );
                            }
                            _ => panic!("unexpected paste refusal task action"),
                        }
                    }
                });
                assert_eq!(notices, 1, "no silent Full/PasteTooLarge");
                assert_eq!(focus_operations, usize::from(confirmed));
                assert!(app.terminal_paste.is_none());
                assert!(
                    matches!(app.cards.get(&card), Some(CardRuntime::Terminal(current))
                    if current.terminal_id() == terminal)
                );
                assert!(
                    app.notices
                        .entries
                        .iter()
                        .any(|entry| entry.notice.message == expected)
                );
                assert!(app.ended_terminal(card).is_none());
                surface.stop_for_test();
            }
        }
    }

    #[test]
    fn paste_confirmation_dismisses_after_its_runtime_has_retired() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let card = CardId::new();
        app.terminal_paste = Some((card, "command\ncommand".into()));
        app.cards.insert(card, CardRuntime::Loading);
        drop(app.handle_terminal_find(Action::ConfirmPaste(card, false)));
        assert!(app.terminal_paste.is_none());
    }
}

#[derive(Default)]
pub(super) struct Find {
    pub open: bool,
    query: String,
    request: u64,
    case_sensitive: bool,
    pub matches: Arc<[crate::terminal::SearchMatch]>,
    pub selected: usize,
    truncated: bool,
    pub focused: bool,
    pub select_all_request: u64,
    pub selection_reset: u64,
    pub pending_copy: Option<uuid::Uuid>,
    searched_content: Option<SearchContent>,
    refresh_generation: u64,
    refresh_pending: Option<u64>,
    background_results: bool,
}

/// Content changes independently of viewport movement and published frame
/// generations. A search scrolling to a match must not schedule another scan.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SearchContent {
    output: Option<u64>,
    columns: u16,
    rows: u16,
    total: u64,
    alternate_screen: bool,
}

impl SearchContent {
    fn of(snapshot: &crate::terminal::RenderSnapshot) -> Self {
        Self {
            output: snapshot.activity.map(|activity| activity.sequence),
            columns: snapshot.columns,
            rows: snapshot.rows,
            total: snapshot.scroll.total,
            alternate_screen: snapshot.scroll.alternate_screen,
        }
    }
}

impl Find {
    fn invalidate_refresh(&mut self) {
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        self.refresh_pending = None;
        self.background_results = false;
    }

    fn schedule_refresh(&mut self, content: SearchContent) -> Option<u64> {
        if !self.open
            || self.query.is_empty()
            || self.searched_content == Some(content)
            || self.refresh_pending.is_some()
        {
            return None;
        }
        self.refresh_generation = self.refresh_generation.wrapping_add(1);
        self.refresh_pending = Some(self.refresh_generation);
        self.refresh_pending
    }

    fn begin_refresh(&mut self, generation: u64, content: SearchContent) -> bool {
        if self.refresh_pending != Some(generation) {
            return false;
        }
        self.refresh_pending = None;
        if !self.open || self.query.is_empty() || self.searched_content == Some(content) {
            return false;
        }
        self.searched_content = Some(content);
        self.request = self.request.wrapping_add(1);
        self.background_results = true;
        true
    }
}

pub(super) struct Context {
    pub card: CardId,
    pub selection: Option<crate::terminal::SearchMatch>,
    pub link: Option<Url>,
    pub point: iced::Point,
}

fn input_id(card: CardId) -> iced::widget::Id {
    iced::widget::Id::from(format!("terminal-find-{card}"))
}

impl Cagents {
    pub(super) fn handle_terminal_find(&mut self, action: Action) -> Task<Message> {
        if let Action::ConfirmPaste(card, confirmed) = &action {
            let pending = self.terminal_paste.take();
            let mut error_notice = Task::none();
            if *confirmed
                && let Some((target, text)) = pending
                && target == *card
                && self.ended_terminal(*card).is_none()
                && !self.terminal_close_pending(*card)
                && let Some(CardRuntime::Terminal(handle)) = self.cards.get(card)
                && let Err(error) = handle.paste(&text, crate::terminal::PasteOrigin::Clipboard)
            {
                error_notice = cosmic::task::message(Message::TerminalError {
                    card: *card,
                    message: error.to_string(),
                });
            }
            return Task::batch([
                iced::widget::operation::focus(workspace_widget::card_focus_id(*card))
                    .map(cosmic::Action::App),
                error_notice,
            ]);
        }
        // The menu closes even when its terminal has gone meanwhile.
        let action = match action {
            Action::CloseContext(_) => {
                self.terminal_context = None;
                return Task::none();
            }
            Action::DismissContext(_, opener) => {
                self.terminal_context = None;
                return crate::ui_controls::return_focus(opener).map(cosmic::Action::App);
            }
            action => action,
        };
        let card = match &action {
            Action::Open(card)
            | Action::Close(card)
            | Action::Query(card, _)
            | Action::Case(card)
            | Action::Next(card, _)
            | Action::Focus(card, _)
            | Action::Context(card, ..)
            | Action::Copy(card, _)
            | Action::CopySelection(card, _)
            | Action::PasteClipboard(card)
            | Action::Paste(card, _)
            | Action::ConfirmPaste(card, _)
            | Action::SelectAll(card)
            | Action::Clear(card)
            | Action::Refresh(card, _) => *card,
            Action::CloseContext(_) | Action::DismissContext(..) => unreachable!("handled above"),
        };
        let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card) else {
            // Recovery may have parked the card while this timer was asleep.
            // Consume it even then so the replacement handle can refresh find.
            if let Action::Refresh(_, generation) = action
                && let Some(find) = self.terminal_find.get_mut(&card)
                && find.refresh_pending == Some(generation)
            {
                find.refresh_pending = None;
                find.searched_content = None;
            }
            return Task::none();
        };
        let opening = matches!(&action, Action::Open(_));
        let restore_query_focus = opening || matches!(&action, Action::Case(_));
        let find = self.terminal_find.entry(card).or_default();
        match action {
            Action::Refresh(_, generation) => {
                if find.begin_refresh(generation, SearchContent::of(&handle.snapshot()))
                    && let Err(error) =
                        handle.search(find.request, find.query.clone(), find.case_sensitive)
                {
                    self.status = Some(TransientNotice::error(error.to_string()));
                }
                return Task::none();
            }
            Action::CloseContext(_) | Action::DismissContext(..) => {
                unreachable!("handled above")
            }
            Action::Context(_, point, selection, link) => {
                self.terminal_context = Some(Context {
                    card,
                    selection,
                    link,
                    point,
                });
                return Task::none();
            }
            Action::Copy(_, text) => {
                self.terminal_context = None;
                chat_ui::invalidate_copy_intent();
                return iced::clipboard::write(text).map(cosmic::Action::App);
            }
            Action::CopySelection(_, selection) => {
                self.terminal_context = None;
                let request = uuid::Uuid::new_v4();
                find.pending_copy = Some(request);
                if let Err(error) = handle.copy_selection(request, selection) {
                    return cosmic::task::message(Message::TerminalError {
                        card,
                        message: error.to_string(),
                    });
                }
                return Task::none();
            }
            Action::PasteClipboard(_) => {
                self.terminal_context = None;
                return iced::clipboard::read().and_then(move |text| {
                    cosmic::task::message(Message::TerminalFind(Action::Paste(card, text)))
                });
            }
            Action::Paste(_, text) => {
                self.terminal_context = None;
                if self.ended_terminal(card).is_some() || self.terminal_close_pending(card) {
                    return Task::none();
                }
                if self.settings.confirm_multiline_paste
                    && !handle.snapshot().scroll.bracketed_paste
                    && text.contains(['\n', '\r'])
                {
                    self.terminal_paste = Some((card, text));
                    return dialog_ui::Question::PasteMultipleLines.focus(
                        widget::Id::from(PASTE_CANCEL),
                        widget::Id::from(PASTE_CONFIRM),
                    );
                }
                if let Err(error) = handle.paste(&text, crate::terminal::PasteOrigin::Clipboard) {
                    return cosmic::task::message(Message::TerminalError {
                        card,
                        message: error.to_string(),
                    });
                }
                return Task::none();
            }
            Action::ConfirmPaste(..) => {
                unreachable!("paste confirmation handled before runtime lookup")
            }
            Action::SelectAll(_) => {
                self.terminal_context = None;
                find.select_all_request = find.select_all_request.wrapping_add(1);
                return Task::none();
            }
            Action::Clear(_) => {
                self.terminal_context = None;
                let _ = handle.clear_scrollback();
                // History rows are rebased by clearing scrollback.
                find.selection_reset = find.selection_reset.wrapping_add(1);
                find.invalidate_refresh();
                // Clearing rebases history; the acknowledgement's snapshot
                // will schedule a refresh even when no new PTY output arrives.
                find.searched_content = None;
                find.request = find.request.wrapping_add(1);
                find.matches = Arc::default();
                find.selected = 0;
                find.truncated = false;
                if find.open {
                    let _ = handle.search(find.request, find.query.clone(), find.case_sensitive);
                }
                return Task::none();
            }
            Action::Open(_) => {
                self.terminal_context = None;
                find.open = true;
                // Reopening must search the current live or retained history, rather
                // than reuse matches from before output changed or completion.
            }
            Action::Close(_) => {
                find.invalidate_refresh();
                find.open = false;
                find.focused = false;
                return iced::widget::operation::focus(workspace_widget::card_focus_id(card))
                    .map(cosmic::Action::App);
            }
            Action::Focus(_, focused) => {
                find.focused = focused;
                return Task::none();
            }
            Action::Next(_, backwards) => {
                find.focused = true;
                if !find.matches.is_empty() {
                    find.selected = if backwards {
                        (find.selected + find.matches.len() - 1) % find.matches.len()
                    } else {
                        (find.selected + 1) % find.matches.len()
                    };
                    let _ = handle.scroll_to_row(find.matches[find.selected].start_row);
                }
                return iced::widget::operation::focus(input_id(card)).map(cosmic::Action::App);
            }
            Action::Query(_, query) => find.query = query,
            Action::Case(_) => find.case_sensitive = !find.case_sensitive,
        }
        find.invalidate_refresh();
        find.searched_content = Some(SearchContent::of(&handle.snapshot()));
        find.request = find.request.wrapping_add(1);
        find.matches = Arc::default();
        find.selected = 0;
        find.truncated = false;
        let focus = if restore_query_focus {
            find.focused = true;
            let focus = iced::widget::operation::focus(input_id(card));
            if opening {
                focus.chain(iced::widget::operation::select_all(input_id(card)))
            } else {
                focus
            }
            .map(cosmic::Action::App)
        } else {
            Task::none()
        };
        if let Err(error) = handle.search(find.request, find.query.clone(), find.case_sensitive) {
            return Task::batch([
                focus,
                cosmic::task::message(Message::TerminalError {
                    card,
                    message: error.to_string(),
                }),
            ]);
        }
        focus
    }

    pub(super) fn refresh_completed_terminal_find(&mut self, card: CardId) {
        let find = self.terminal_find.entry(card).or_default();
        find.invalidate_refresh();
        find.selection_reset = find.selection_reset.wrapping_add(1);
        find.request = find.request.wrapping_add(1);
        find.matches = Arc::default();
        find.selected = 0;
        find.pending_copy = None;
        if find.open
            && let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card)
        {
            let _ = handle.search(find.request, find.query.clone(), find.case_sensitive);
        }
    }

    pub(super) fn terminal_search_results(
        &mut self,
        card: CardId,
        request: u64,
        query: String,
        matches: Vec<crate::terminal::SearchMatch>,
        truncated: bool,
    ) {
        let Some(find) = self.terminal_find.get_mut(&card) else {
            return;
        };
        if !find.open || find.request != request || find.query != query {
            return;
        }
        let previous = find.matches.get(find.selected).copied();
        let background = std::mem::take(&mut find.background_results);
        let selected = background
            .then(|| {
                previous.and_then(|previous| matches.iter().position(|item| *item == previous))
            })
            .flatten()
            .unwrap_or_else(|| {
                if background {
                    find.selected.min(matches.len().saturating_sub(1))
                } else {
                    0
                }
            });
        find.matches = matches.into();
        find.selected = selected;
        find.truncated = truncated;
        if !background
            && let Some(first) = find.matches.first()
            && let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card)
        {
            let _ = handle.scroll_to_row(first.start_row);
        }
    }

    pub(super) fn refresh_live_terminal_find(&mut self, card: CardId) -> Task<Message> {
        let Some(find) = self.terminal_find.get_mut(&card) else {
            return Task::none();
        };
        let Some(CardRuntime::Terminal(handle)) = self.cards.get(&card) else {
            return Task::none();
        };
        let Some(generation) = find.schedule_refresh(SearchContent::of(&handle.snapshot())) else {
            return Task::none();
        };
        // Coalesce continuous output without postponing search indefinitely.
        cosmic::task::future(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            Message::TerminalFind(Action::Refresh(card, generation))
        })
    }

    pub(super) fn terminal_find_view(&self, card: CardId) -> Option<Element<'_, Message>> {
        let find = self.terminal_find.get(&card).filter(|find| find.open)?;
        let message = |action| Message::TerminalFind(action);
        // A foreground surface owns Enter even if a click refocuses find.
        let can_step = !self.workspace_inputs_blocked();
        let input = crate::interaction::focused_key(
            crate::ui_controls::field::search("Find in terminal scrollback", &find.query)
                .id(input_id(card))
                .on_input(move |query| Message::TerminalFind(Action::Query(card, query)))
                .on_focus(message(Action::Focus(card, true)))
                .on_unfocus(message(Action::Focus(card, false)))
                .width(Length::Fill),
            move |event| match event {
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                } => Some(Message::TerminalFind(Action::Close(card))),
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    modifiers,
                    ..
                } if can_step => Some(Message::TerminalFind(Action::Next(card, modifiers.shift()))),
                _ => None,
            },
        );
        let count = if find.query.is_empty() {
            String::new()
        } else if find.matches.is_empty() {
            "No matches".into()
        } else {
            format!(
                "{} of {}{}",
                find.selected + 1,
                find.matches.len(),
                if find.truncated { "+" } else { "" }
            )
        };
        Some(
            widget::row::with_children(vec![
                input,
                widget::text(count).into(),
                crate::ui_controls::button::quiet("Aa")
                    .instead("Aa ✓", find.case_sensitive)
                    .on_press(message(Action::Case(card)))
                    .into(),
                crate::ui_controls::icon_button(
                    Icon::ChevronUp,
                    crate::interaction::find_tips::previous(),
                )
                .on_press(message(Action::Next(card, true)))
                .into(),
                crate::ui_controls::icon_button(
                    Icon::ChevronDown,
                    crate::interaction::find_tips::next(),
                )
                .on_press(message(Action::Next(card, false)))
                .into(),
                crate::ui_controls::icon_button(
                    Icon::Close,
                    crate::interaction::find_tips::close(),
                )
                .on_press(message(Action::Close(card)))
                .into(),
            ])
            .spacing(space::XS)
            .align_y(Alignment::Center)
            .padding(space::XS)
            .into(),
        )
    }

    /// The terminal's context menu, open where the terminal took a right
    /// click as its own: the clipboard and find, then the link under the
    /// pointer. Copy waits for a selection.
    pub(super) fn terminal_menu(
        &self,
        card: CardId,
    ) -> Option<Vec<crate::ui_controls::MenuItem<'_, Message>>> {
        use crate::ui_controls::{menu_divider, menu_item};
        let context = self
            .terminal_context
            .as_ref()
            .filter(|context| context.card == card)?;
        let command = |label, action| menu_item(label, None, Some(Message::TerminalFind(action)));
        let mut items = vec![
            menu_item(
                "Copy",
                None,
                context
                    .selection
                    .map(|selection| Message::TerminalFind(Action::CopySelection(card, selection))),
            ),
            command("Paste", Action::PasteClipboard(card)),
            command("Select all", Action::SelectAll(card)),
            command("Clear scrollback", Action::Clear(card)),
            command("Find…", Action::Open(card)),
        ];
        if let Some(link) = &context.link {
            items.push(menu_divider());
            items.push(menu_item(
                if link.scheme() == "file" {
                    "Open file"
                } else {
                    "Open link"
                },
                Some(if cfg!(target_os = "macos") {
                    "Cmd+click"
                } else {
                    "Ctrl+click"
                }),
                Some(Message::OpenLink(
                    card,
                    link.clone(),
                    settings::LinkOpenGesture::ControlClick,
                )),
            ));
            items.push(command(
                "Copy link",
                Action::Copy(card, link.as_str().to_owned()),
            ));
        }
        Some(items)
    }

    pub(super) fn terminal_paste_dialog(&self) -> Option<Element<'_, Message>> {
        let (card, text) = self.terminal_paste.as_ref()?;
        let card = *card;
        Some(dialog_ui::overlay(dialog_ui::confirmation_surface(dialog_ui::Question::PasteMultipleLines, widget::column::with_children(vec![
            crate::design::title("Paste multiple lines?").into(),
            notice_body("This program does not use bracketed paste. Each newline may run a command immediately."),
            widget::text(format!("{} lines · {} characters", text.lines().count(), text.chars().count())).into(),
        ]).spacing(space::M), widget::row::with_children(vec![
            crate::ui_controls::button::secondary("Cancel").id(widget::Id::from(PASTE_CANCEL))
                .on_press(Message::TerminalFind(Action::ConfirmPaste(card, false))).into(),
            crate::ui_controls::button::primary("Paste").id(widget::Id::from(PASTE_CONFIRM)).on_press(Message::TerminalFind(Action::ConfirmPaste(card, true))).into(),
        ]).spacing(space::S), 480.0), Some(Message::TerminalFind(Action::ConfirmPaste(card, false))), 1.0))
    }
}
