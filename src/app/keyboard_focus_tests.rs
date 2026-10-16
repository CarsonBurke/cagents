//! Actual App view/native keyboard routing, not a direct KeyEvent substitute.
use super::*;
use cosmic::iced::{
    Size,
    advanced::{clipboard, mouse, widget::operation::focusable},
    runtime::{UserInterface, user_interface::Cache},
};

fn key(
    character: &str,
    code: keyboard::key::Code,
    modifiers: keyboard::Modifiers,
    text: Option<&str>,
) -> iced::Event {
    let key = keyboard::Key::Character(character.to_owned().into());
    iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Code(code),
        location: keyboard::Location::Standard,
        modifiers,
        text: text.map(Into::into),
        repeat: false,
    })
}
fn native(
    app: &Cagents,
    renderer: &mut cosmic::Renderer,
    cache: Cache,
    focus: Option<widget::Id>,
    events: &[iced::Event],
) -> (Cache, Vec<Message>) {
    let mut ui = UserInterface::build(app.view(), Size::new(1600.0, 1000.0), cache, renderer);
    if let Some(focus) = focus {
        ui.operate(renderer, &mut focusable::focus::<()>(focus));
    }
    let mut messages = Vec::new();
    let _ = ui.update(
        events,
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    (ui.into_cache(), messages)
}
fn prepare(app: &mut Cagents) {
    app.window_width = 1600.0;
    app.window_height = 1000.0;
    app.context_inspector_open = false;
    app.refresh_ui.focused = true;
    let _ = app.workspace.resize_viewport(1200.0);
    drop(app.sync_chrome());
    assert!(!app.workspace_inputs_blocked());
}
fn text(app: &Cagents, card: CardId) -> String {
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        panic!("editable repository")
    };
    runtime.editor_text().unwrap()
}
fn dispatch(app: &mut Cagents, messages: Vec<Message>) {
    for message in messages {
        drop(app.update(message));
    }
}
fn seed_reopen(app: &mut Cagents, source: CardId) {
    let mut closed = app.card_state(source).unwrap().clone();
    closed.id = CardId::new();
    closed.kind = CardKind::Settings;
    app.recently_closed
        .push((closed, app.active_workspace_destination()));
}

#[test]
fn actual_app_editor_undo_and_shift_redo_keep_exact_text_without_reopening_a_pane() {
    use keyboard::key::Code;
    let (mut app, card) = super::tests::editing_repository_app();
    prepare(&mut app);
    seed_reopen(&mut app, card);
    let before = text(&app, card);
    let mut renderer = super::tests::headless_renderer();
    let (cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(repository_widget::editor_focus_id(card)),
        &[key(
            "x",
            Code::KeyX,
            keyboard::Modifiers::empty(),
            Some("x"),
        )],
    );
    assert!(messages.iter().any(|m| matches!(m, Message::RoutedKeyEvent { region, widget: Some(id), .. } if region.pane == workspace_widget::FocusedPane::Editor && region.card == Some(card) && id == &repository_widget::editor_focus_id(card))));
    dispatch(&mut app, messages);
    let edited = text(&app, card);
    assert_ne!(
        edited, before,
        "the actual native editor must positively edit"
    );
    let (cache, messages) = native(
        &app,
        &mut renderer,
        cache,
        None,
        &[key("z", Code::KeyZ, keyboard::Modifiers::CTRL, None)],
    );
    dispatch(&mut app, messages);
    assert_eq!(text(&app, card), before);
    let (_cache, messages) = native(
        &app,
        &mut renderer,
        cache,
        None,
        &[key(
            "z",
            Code::KeyZ,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            None,
        )],
    );
    dispatch(&mut app, messages);
    assert_eq!(
        text(&app, card),
        edited,
        "redo must restore the exact edited value"
    );
    assert_eq!(
        app.recently_closed.len(),
        1,
        "a real reopen candidate must remain untouched"
    );
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![card]);
}

#[test]
fn actual_sidebar_field_owns_find_new_tab_redo_and_save_while_the_editor_is_behind_it() {
    use keyboard::key::Code;
    let (mut app, card) = super::tests::editing_repository_app();
    app.sidebar_model.projects.push(
        sidebar::SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "Keyboard fixture".into(),
            "/keyboard-fixture".into(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
    );
    app.sidebar_state.apply(&SidebarAction::ToggleControls);
    prepare(&mut app);
    assert!(sidebar::search_is_reachable(
        &app.sidebar_model,
        app.projects_sidebar_mode()
    ));
    seed_reopen(&mut app, card);
    let before = text(&app, card);
    let mut renderer = super::tests::headless_renderer();
    let events = [
        key("f", Code::KeyF, keyboard::Modifiers::CTRL, None),
        key("t", Code::KeyT, keyboard::Modifiers::CTRL, None),
        key(
            "z",
            Code::KeyZ,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            None,
        ),
        key("s", Code::KeyS, keyboard::Modifiers::CTRL, None),
        key(
            "s",
            Code::KeyS,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            None,
        ),
        iced::Event::InputMethod(iced::advanced::input_method::Event::Commit(
            "native-query".into(),
        )),
    ];
    let (_cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(sidebar::search_focus_id()),
        &events,
    );
    let routed: Vec<_> = messages
        .iter()
        .filter_map(|m| {
            if let Message::RoutedKeyEvent { region, widget, .. } = m {
                Some((region, widget))
            } else {
                None
            }
        })
        .collect();
    assert_eq!(
        routed.len(),
        5,
        "each actual keyboard event is published once"
    );
    for (region, id) in routed {
        assert_eq!(region.pane, workspace_widget::FocusedPane::TextInput);
        assert_eq!(region.card, None);
        assert_eq!(id.as_ref(), Some(&sidebar::search_focus_id()));
    }
    dispatch(&mut app, messages);
    assert_eq!(
        app.sidebar_state.query(),
        "native-query",
        "the intended field must positively accept IME"
    );
    assert_eq!(text(&app, card), before);
    assert_eq!(app.recently_closed.len(), 1);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![card]);
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert!(!runtime.find_is_open());
    assert!(!runtime.is_saving());
}

#[test]
fn actual_empty_workspace_actions_are_nontext_controls_with_no_background_pane_owner() {
    use keyboard::key::Code;
    let mut app = super::tests::context_inspector_test_app(false);
    prepare(&mut app);
    let mut renderer = super::tests::headless_renderer();
    let mut ids: Vec<_> = [
        "empty.new-conversation",
        "empty.resume-conversation",
        "empty.new-terminal",
        "empty.new-browser",
    ]
    .into_iter()
    .map(widget::Id::new)
    .collect();
    ids.extend(
        crate::agent::Provider::ALL
            .into_iter()
            .map(|provider| widget::Id::new(format!("empty.provider.{}", provider.label()))),
    );
    for id in ids {
        let (_cache, messages) = native(
            &app,
            &mut renderer,
            Cache::new(),
            Some(id.clone()),
            &[key("t", Code::KeyT, keyboard::Modifiers::CTRL, None)],
        );
        let routed: Vec<_> = messages
            .iter()
            .filter_map(|m| {
                if let Message::RoutedKeyEvent {
                    event,
                    region,
                    widget,
                    ..
                } = m
                {
                    Some((event, region, widget))
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(routed.len(), 1, "{id:?}");
        let (event, region, actual) = routed[0];
        assert_eq!(
            actual.as_ref(),
            Some(&id),
            "the actual control must be mounted and focusable"
        );
        assert_eq!(region.pane, workspace_widget::FocusedPane::Other);
        assert_eq!(region.card, None);
        assert_eq!(
            workspace_widget::global_shortcut(event, &app.settings.keyboard_shortcuts, region.pane),
            Some(workspace_widget::GlobalShortcut::NewThread)
        );
    }
}

#[test]
fn routed_owner_is_immutable_across_a_newer_sample_and_a_closed_owner_is_refused() {
    use keyboard::key::Code;
    let (mut app, card) = super::tests::editing_repository_app();
    prepare(&mut app);
    seed_reopen(&mut app, card);
    let other_id = widget::Id::unique();
    let sample = crate::input_trace::KeyboardRegion {
        pane: workspace_widget::FocusedPane::Other,
        card: None,
    };
    let current = app
        .input_trace
        .scoped_keyboard_focus(sample, Some(other_id.clone()));
    let iced::Event::Keyboard(event) = key(
        "z",
        Code::KeyZ,
        keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        None,
    ) else {
        unreachable!()
    };
    drop(app.update(Message::RoutedKeyEvent {
        event: event.clone(),
        captured: false,
        region: crate::input_trace::KeyboardRegion {
            pane: workspace_widget::FocusedPane::TextInput,
            card: None,
        },
        widget: Some(sidebar::search_focus_id()),
    }));
    assert_eq!(
        app.recently_closed.len(),
        1,
        "the older immutable field event cannot borrow the newer nontext context"
    );
    assert_eq!(app.input_trace.keyboard_widget_id(), Some(other_id));
    assert_eq!(
        app.input_trace.keyboard_focus(),
        Some(workspace_widget::FocusedPane::Other)
    );
    drop(app.update(Message::RoutedKeyEvent {
        event,
        captured: false,
        region: crate::input_trace::KeyboardRegion {
            pane: workspace_widget::FocusedPane::Other,
            card: Some(CardId::new()),
        },
        widget: None,
    }));
    assert_eq!(
        app.recently_closed.len(),
        1,
        "an already-closed native owner cannot target the currently selected pane"
    );
    drop(current);
}

#[test]
fn actual_nonselected_pane_header_keeps_native_focus_through_all_ready_update_operations() {
    use iced::futures::StreamExt;
    use keyboard::key::Code;
    let (mut app, owner) = super::tests::editing_repository_app();
    let mut other = app.card_state(owner).unwrap().clone();
    other.id = CardId::new();
    other.kind = CardKind::Settings;
    let selected = other.id;
    Arc::make_mut(&mut app.durable).cards.push(other);
    app.cards.insert(selected, CardRuntime::Settings);
    app.workspace
        .add_card(
            crate::workspace::WorkspaceCard::new(
                selected,
                "Other pane",
                crate::workspace::CardWidth::Fraction(0.4),
            )
            .unwrap(),
            1,
            true,
        )
        .unwrap();
    prepare(&mut app);
    let header = app.card_header_widget_id(owner);
    assert_ne!(
        header,
        app.card_keyboard_focus(owner),
        "the retained native owner must differ from the pane's default editor target"
    );
    assert_eq!(app.workspace.focused(), Some(selected));
    let mut renderer = super::tests::headless_renderer();
    let (cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(header.clone()),
        &[key("q", Code::KeyQ, keyboard::Modifiers::empty(), None)],
    );
    let routed = messages.into_iter().find(|m| matches!(m, Message::RoutedKeyEvent { widget: Some(id), region, .. } if id == &header && region.card == Some(owner))).expect("actual mounted nonselected header owns the event");
    let task = app.update(routed);
    assert_eq!(app.workspace.focused(), Some(owner));
    let mut ui = UserInterface::build(app.view(), Size::new(1600.0, 1000.0), cache, &mut renderer);
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor.block_on(async {
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            // Exercise the actual ready effects, including any erroneous
            // default-focus operation. Maintenance timers are unrelated.
            while let Ok(Some(action)) =
                tokio::time::timeout(Duration::from_millis(100), stream.next()).await
            {
                if let iced::runtime::Action::Widget(operation) = action {
                    let mut next = Some(operation);
                    while let Some(mut operation) = next.take() {
                        ui.operate(&renderer, operation.as_mut());
                        if let iced::advanced::widget::operation::Outcome::Chain(chained) =
                            operation.finish()
                        {
                            next = Some(chained);
                        }
                    }
                }
            }
        }
    });
    executor.shutdown_background();
    let mut messages = Vec::new();
    let _ = ui.update(
        &[key("q", Code::KeyQ, keyboard::Modifiers::empty(), None)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(messages.iter().any(|m| matches!(m, Message::RoutedKeyEvent { widget: Some(id), region, .. } if id == &header && region.card == Some(owner))), "logical reconciliation and its actual widget operations must preserve the nondefault native owner");
    let cache = ui.into_cache();
    let (_, messages) = native(
        &app,
        &mut renderer,
        cache,
        None,
        &[key("s", Code::KeyS, keyboard::Modifiers::CTRL, None)],
    );
    let routed = messages.into_iter().find(|message| matches!(message, Message::RoutedKeyEvent { widget: Some(id), region, .. } if id == &header && region.card == Some(owner))).unwrap();
    let task = app.update(routed);
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut save = None;
    executor.block_on(async {
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            while let Ok(Some(action)) =
                tokio::time::timeout(Duration::from_millis(100), stream.next()).await
            {
                if let iced::runtime::Action::Output(cosmic::Action::App(Message::Repository(
                    RepositoryAction::Save {
                        card,
                        overwrite: false,
                    },
                ))) = action
                {
                    save = Some(card);
                }
            }
        }
    });
    executor.shutdown_background();
    assert_eq!(
        save,
        Some(owner),
        "ordinary Save must keep targeting the exact repository that owns this nontext control"
    );
}

#[test]
fn actual_workspace_buttons_reopen_a_pane_while_the_native_search_field_keeps_redo() {
    use keyboard::key::Code;
    for (button, inspector_open) in [
        (widget::Id::new("sidebar.settings"), false),
        (widget::Id::new("sidebar.automations"), false),
        (sidebar::search_toggle_focus_id(), false),
        (context_inspector_toggle_id(), false),
        (context_inspector_toggle_id(), true),
    ] {
        let (mut app, card) = super::tests::editing_repository_app();
        app.sidebar_model.projects.push(
            sidebar::SidebarProject::new(
                ProjectId::new(),
                RepositoryId::new(),
                "Keyboard fixture".into(),
                "/keyboard-fixture".into(),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder),
        );
        app.sidebar_state.apply(&SidebarAction::ToggleControls);
        prepare(&mut app);
        app.context_inspector_open = inspector_open;
        seed_reopen(&mut app, card);
        let mut renderer = super::tests::headless_renderer();
        let redo = key(
            "z",
            Code::KeyZ,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            None,
        );
        let (cache, field_messages) = native(
            &app,
            &mut renderer,
            Cache::new(),
            Some(sidebar::search_focus_id()),
            &[redo.clone()],
        );
        assert!(field_messages.iter().any(|message| matches!(message, Message::RoutedKeyEvent { region, widget: Some(id), .. } if region.pane == workspace_widget::FocusedPane::TextInput && region.card.is_none() && id == &sidebar::search_focus_id())));
        dispatch(&mut app, field_messages);
        assert_eq!(
            app.recently_closed.len(),
            1,
            "sidebar text editing must keep redo"
        );
        let (_cache, messages) = native(&app, &mut renderer, cache, Some(button.clone()), &[redo]);
        assert!(messages.iter().any(|message| matches!(message, Message::RoutedKeyEvent { event, region, widget: Some(id), .. } if region.pane == workspace_widget::FocusedPane::Other && region.card.is_none() && id == &button && workspace_widget::global_shortcut(event, &app.settings.keyboard_shortcuts, region.pane) == Some(workspace_widget::GlobalShortcut::ReopenPane))), "actual mounted button {button:?} must positively own nontext redo");
        dispatch(&mut app, messages);
        assert!(
            app.recently_closed.is_empty(),
            "the actual nontext route must consume the real reopen candidate"
        );
        assert_eq!(app.workspace.order().len(), 2);
        assert_eq!(
            app.utility_card(&CardKind::Settings),
            app.workspace.focused()
        );
    }
}

#[test]
fn actual_nontext_sidebar_find_focuses_project_search_instead_of_the_background_editor() {
    use iced::futures::StreamExt;
    use keyboard::key::Code;
    let (mut app, card) = super::tests::editing_repository_app();
    app.sidebar_model.projects.push(
        sidebar::SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "Keyboard fixture".into(),
            "/keyboard-fixture".into(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
    );
    prepare(&mut app);
    assert!(!app.sidebar_state.controls_expanded());
    let mut renderer = super::tests::headless_renderer();
    let (cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(widget::Id::new("sidebar.settings")),
        &[key("f", Code::KeyF, keyboard::Modifiers::CTRL, None)],
    );
    let routed = messages.into_iter().find(|message| matches!(message, Message::RoutedKeyEvent { widget: Some(id), region, .. } if id == &widget::Id::new("sidebar.settings") && region.pane == workspace_widget::FocusedPane::Other && region.card.is_none())).expect("actual sidebar nontext owner");
    let task = app.update(routed);
    assert!(
        app.sidebar_state.controls_expanded(),
        "find must positively open the existing project filter"
    );
    let mut ui = UserInterface::build(app.view(), Size::new(1600.0, 1000.0), cache, &mut renderer);
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    executor.block_on(async {
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            while let Ok(Some(action)) =
                tokio::time::timeout(Duration::from_millis(100), stream.next()).await
            {
                match action {
                    iced::runtime::Action::Widget(operation) => {
                        let mut next = Some(operation);
                        while let Some(mut operation) = next.take() {
                            ui.operate(&renderer, operation.as_mut());
                            if let iced::advanced::widget::operation::Outcome::Chain(chained) =
                                operation.finish()
                            {
                                next = Some(chained);
                            }
                        }
                    }
                    iced::runtime::Action::Output(cosmic::Action::App(Message::Repository(_))) => {
                        panic!("sidebar find cannot schedule a background repository action")
                    }
                    _ => {}
                }
            }
        }
    });
    executor.shutdown_background();
    let mut messages = Vec::new();
    let _ = ui.update(
        &[key("q", Code::KeyQ, keyboard::Modifiers::CTRL, None)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(messages.iter().any(|message| matches!(message, Message::RoutedKeyEvent { widget: Some(id), region, .. } if id == &sidebar::search_focus_id() && region.pane == workspace_widget::FocusedPane::TextInput && region.card.is_none())), "the actual returned focus operation must land in the mounted project search field");
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        unreachable!()
    };
    assert!(!runtime.find_is_open());
}

fn add_inactive_native_card(app: &mut Cagents, kind: CardKind, runtime: CardRuntime) -> CardId {
    let selected = app.workspace.focused().unwrap();
    let mut card = app.card_state(selected).unwrap().clone();
    card.id = CardId::new();
    card.kind = kind;
    let id = card.id;
    Arc::make_mut(&mut app.durable).cards.push(card);
    app.cards.insert(id, runtime);
    app.workspace
        .add_card(
            crate::workspace::WorkspaceCard::new(
                id,
                "Native sibling",
                crate::workspace::CardWidth::Fraction(0.4),
            )
            .unwrap(),
            1,
            false,
        )
        .unwrap();
    assert_eq!(app.workspace.focused(), Some(selected));
    id
}

fn native_sibling_batch(app: &Cagents, card: CardId) -> (Cache, Vec<Message>) {
    let mut renderer = super::tests::headless_renderer();
    // Use the actual App builder, including its current save/modal/prepared
    // permission. The sibling's FocusCard message remains unapplied for the
    // complete native batch, just as in the actual event loop.
    let mut ui = UserInterface::build(
        app.card_view(card, false, false),
        Size::new(640.0, 320.0),
        Cache::new(),
        &mut renderer,
    );
    let mut messages = Vec::new();
    let _ = ui.update(
        &[
            iced::Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            key(
                "a",
                keyboard::key::Code::KeyA,
                keyboard::Modifiers::empty(),
                None,
            ),
            iced::Event::Keyboard(keyboard::Event::KeyReleased {
                key: keyboard::Key::Character("a".into()),
                modified_key: keyboard::Key::Character("a".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyA),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
            }),
            iced::Event::InputMethod(iced::advanced::input_method::Event::Commit(
                "native-sibling-ime".into(),
            )),
        ],
        mouse::Cursor::Available(iced::Point::new(80.0, 160.0)),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    (ui.into_cache(), messages)
}

#[test]
fn actual_app_inactive_terminal_native_admission_obeys_save_and_prepared_selection_refusal() {
    use crate::terminal::ipc::ClientRequest;
    for blocked in ["open", "saving", "prepared"] {
        let peer = crate::terminal::WidgetTestPeer::new();
        let (mut app, selected) = super::tests::editing_repository_app();
        let mut session = fresh_terminal_session(LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: Vec::new(),
            working_directory: "/".into(),
            environment: Default::default(),
            unset_environment: Default::default(),
        });
        session.id = peer.handle().terminal_id();
        let sibling = add_inactive_native_card(
            &mut app,
            CardKind::Terminal(session),
            CardRuntime::Terminal(peer.handle().clone().into()),
        );
        let _save = if blocked == "saving" {
            let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&selected) else {
                unreachable!()
            };
            let task = runtime.request_save(selected, false);
            assert!(
                runtime.is_saving(),
                "actual save admission must precede native input"
            );
            Some(task)
        } else {
            None
        };
        app.update_prepared = blocked == "prepared";
        prepare(&mut app);
        assert_eq!(app.focused_repository_is_saving(), blocked == "saving");
        let (_cache, messages) = native_sibling_batch(&app, sibling);
        let requests = peer.fence();
        let keys: Vec<_> = requests
            .iter()
            .filter_map(|request| match request {
                ClientRequest::Key { input, .. } => Some((input.key, input.action)),
                _ => None,
            })
            .collect();
        if blocked == "open" {
            assert_eq!(
                keys,
                [
                    (
                        crate::terminal::TerminalKey::Character('a'),
                        crate::terminal::KeyAction::Press
                    ),
                    (
                        crate::terminal::TerminalKey::Character('a'),
                        crate::terminal::KeyAction::Release
                    )
                ],
                "{requests:?}"
            );
            assert!(requests.iter().any(|request| matches!(request, ClientRequest::Input { bytes, .. } if bytes == b"native-sibling-ime")), "{requests:?}");
        } else {
            assert!(
                keys.is_empty(),
                "{blocked} accepted an inactive sibling key: {requests:?}"
            );
            assert!(
                !requests.iter().any(|request| matches!(
                    request,
                    ClientRequest::Input { .. } | ClientRequest::Paste { .. }
                )),
                "{blocked} accepted inactive sibling IME: {requests:?}"
            );
        }
        assert_eq!(
            app.workspace.focused(),
            Some(selected),
            "App callback deliberately unapplied during native input"
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, Message::FocusCard(id) if *id == sibling))
        );
        dispatch(&mut app, messages);
        assert_eq!(
            app.workspace.focused(),
            Some(if blocked == "open" { sibling } else { selected })
        );
    }
}

#[cfg(feature = "integrated-browser")]
#[test]
fn actual_app_inactive_browser_native_admission_obeys_save_and_prepared_selection_refusal() {
    use crate::browser::{BrowserInput, ButtonState, ImeInput};
    for blocked in ["open", "saving", "prepared"] {
        let (mut app, selected) = super::tests::editing_repository_app();
        let handle = BrowserHandle::fixture();
        let url = url::Url::parse("https://native-sibling.invalid/").unwrap();
        let sibling = add_inactive_native_card(
            &mut app,
            CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: url.clone(),
                history: vec![url.clone()],
                history_index: 0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                fidelity: BrowserRestoreFidelity::ExactLive,
            }),
            CardRuntime::Browser(BrowserRuntime {
                handle,
                ime: None,
                lifecycle: BrowserLifecycle::Visible,
                address: BrowserAddress::location(&url),
                address_focused: false,
                address_error: false,
                progress_phase: 0.0,
                load_state: None,
            }),
        );
        let _save = if blocked == "saving" {
            let Some(CardRuntime::Repository(runtime)) = app.cards.get_mut(&selected) else {
                unreachable!()
            };
            let task = runtime.request_save(selected, false);
            assert!(runtime.is_saving());
            Some(task)
        } else {
            None
        };
        app.update_prepared = blocked == "prepared";
        prepare(&mut app);
        let (_cache, messages) = native_sibling_batch(&app, sibling);
        let Some(CardRuntime::Browser(browser)) = app.cards.get(&sibling) else {
            unreachable!()
        };
        let input = browser.handle.fixture_inputs();
        let keys: Vec<_> = input
            .iter()
            .filter_map(|input| match input {
                BrowserInput::Key { state, code, .. } => Some((*state, code.as_str())),
                _ => None,
            })
            .collect();
        if blocked == "open" {
            assert_eq!(
                keys,
                [
                    (ButtonState::Pressed, "KeyA"),
                    (ButtonState::Released, "KeyA")
                ],
                "{input:?}"
            );
            assert!(input.iter().any(|input| matches!(input, BrowserInput::Ime(ImeInput::Commit(text)) if text == "native-sibling-ime")), "{input:?}");
        } else {
            assert!(
                keys.is_empty(),
                "{blocked} accepted inactive browser key: {input:?}"
            );
            assert!(
                !input
                    .iter()
                    .any(|input| matches!(input, BrowserInput::Ime(_))),
                "{blocked} accepted inactive browser IME: {input:?}"
            );
        }
        assert_eq!(app.workspace.focused(), Some(selected));
        dispatch(&mut app, messages);
        assert_eq!(
            app.workspace.focused(),
            Some(if blocked == "open" { sibling } else { selected })
        );
    }
}

#[test]
fn prepared_update_refuses_routed_sibling_reconciliation_before_any_workspace_mutation() {
    let (mut app, selected) = super::tests::editing_repository_app();
    let sibling = add_inactive_native_card(&mut app, CardKind::Settings, CardRuntime::Settings);
    prepare(&mut app);
    app.workspace.take_widget_focus_request();
    app.update_prepared = true;
    let before = app.workspace.order().collect::<Vec<_>>();
    let durable_focus = app.durable.focused;
    let task = app.update(Message::RoutedKeyEvent {
        event: match key(
            "a",
            keyboard::key::Code::KeyA,
            keyboard::Modifiers::empty(),
            None,
        ) {
            iced::Event::Keyboard(event) => event,
            _ => unreachable!(),
        },
        captured: false,
        region: crate::input_trace::KeyboardRegion {
            pane: workspace_widget::FocusedPane::Other,
            card: Some(sibling),
        },
        widget: Some(app.card_header_widget_id(sibling)),
    });
    assert_eq!(app.workspace.focused(), Some(selected));
    assert_eq!(app.durable.focused, durable_focus);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), before);
    assert!(app.workspace.take_widget_focus_request().is_none());
    assert!(
        iced::runtime::task::into_stream(task).is_none(),
        "no viewport/native focus effect may escape prepared refusal"
    );
    assert!(
        app.update_prepared,
        "the update preparation acknowledgement remains active"
    );
}

#[test]
fn native_font_zoom_stays_with_the_field_or_editor_that_owns_focus() {
    use keyboard::key::Code;
    let (mut app, card) = super::tests::editing_repository_app();
    app.settings.editor_font_size = Some(19.0);
    app.sidebar_model.projects.push(
        sidebar::SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "Keyboard fixture".into(),
            "/keyboard-fixture".into(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
    );
    app.sidebar_state.apply(&SidebarAction::ToggleControls);
    prepare(&mut app);
    assert!(sidebar::search_is_reachable(
        &app.sidebar_model,
        app.projects_sidebar_mode()
    ));
    let mut renderer = super::tests::headless_renderer();
    let events = [
        key("0", Code::Digit0, keyboard::Modifiers::CTRL, None),
        key("=", Code::Equal, keyboard::Modifiers::CTRL, None),
        key("-", Code::Minus, keyboard::Modifiers::CTRL, None),
    ];
    let mut field_events = events.to_vec();
    field_events.push(iced::Event::InputMethod(
        iced::advanced::input_method::Event::Commit("font-owner-query".into()),
    ));
    let (cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(sidebar::search_focus_id()),
        &field_events,
    );
    assert!(!messages.iter().any(|message| matches!(
        message,
        Message::Repository(RepositoryAction::ZoomFont { .. }) | Message::ZoomFont { .. }
    )));
    assert!(messages.iter().any(|message| matches!(message, Message::RoutedKeyEvent { widget: Some(id), region, .. } if id == &sidebar::search_focus_id() && region.pane == workspace_widget::FocusedPane::TextInput && region.card.is_none())));
    dispatch(&mut app, messages);
    assert_eq!(app.sidebar_state.query(), "font-owner-query");
    assert_eq!(app.workspace.focused(), Some(card));
    assert_eq!(app.settings.editor_font_size, Some(19.0));
    let (_cache, messages) = native(
        &app,
        &mut renderer,
        cache,
        Some(repository_widget::editor_focus_id(card)),
        &events,
    );
    let zoom: Vec<_> = messages
        .iter()
        .filter_map(|message| match message {
            Message::Repository(RepositoryAction::ZoomFont { card, delta }) => {
                Some((*card, *delta))
            }
            _ => None,
        })
        .collect();
    assert_eq!(zoom, [(card, 0), (card, 1), (card, -1)]);
    dispatch(&mut app, messages);
    assert_eq!(
        app.settings.editor_font_size(),
        app.settings.terminal_font_size
    );
}

#[test]
fn an_earlier_mounted_inactive_editor_does_not_capture_the_terminal_font_chord() {
    use crate::terminal::ipc::ClientRequest;
    use keyboard::key::Code;
    struct EditorBounds {
        id: widget::Id,
        bounds: Option<iced::Rectangle>,
    }
    impl iced::advanced::widget::Operation for EditorBounds {
        fn traverse(
            &mut self,
            operate: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation),
        ) {
            operate(self);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _state: &mut dyn iced::advanced::widget::operation::Focusable,
        ) {
            if id == Some(&self.id) {
                self.bounds = Some(bounds);
            }
        }
    }
    let peer = crate::terminal::WidgetTestPeer::new();
    let (mut app, repository) = super::tests::editing_repository_app();
    let mut session = fresh_terminal_session(LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: Vec::new(),
        working_directory: "/".into(),
        environment: Default::default(),
        unset_environment: Default::default(),
    });
    session.id = peer.handle().terminal_id();
    let terminal = add_inactive_native_card(
        &mut app,
        CardKind::Terminal(session),
        CardRuntime::Terminal(peer.handle().clone().into()),
    );
    let update = app.workspace.focus(terminal).unwrap();
    app.workspace.take_widget_focus_request();
    drop(app.apply_viewport(update, false, false));
    app.settings.editor_font_size = Some(19.0);
    app.settings.terminal_font_size = 18.0;
    prepare(&mut app);
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        [repository, terminal]
    );
    let mut renderer = super::tests::headless_renderer();
    let mut ui = UserInterface::build(
        app.view(),
        Size::new(1600.0, 1000.0),
        Cache::new(),
        &mut renderer,
    );
    let mut editor = EditorBounds {
        id: repository_widget::editor_focus_id(repository),
        bounds: None,
    };
    ui.operate(&renderer, &mut editor);
    let bounds = editor
        .bounds
        .expect("the earlier repository editor must actually be mounted");
    assert!(bounds.width > 0.0 && bounds.height > 0.0);
    assert!(bounds.x >= 0.0 && bounds.x + bounds.width <= 1600.0);
    let terminal_id = app.card_keyboard_focus(terminal);
    ui.operate(&renderer, &mut focusable::focus::<()>(terminal_id.clone()));
    let mut messages = Vec::new();
    let _ = ui.update(
        &[key("0", Code::Digit0, keyboard::Modifiers::CTRL, None)],
        mouse::Cursor::Unavailable,
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    let _cache = ui.into_cache();
    assert!(!messages.iter().any(|message| matches!(
        message,
        Message::Repository(RepositoryAction::ZoomFont { .. })
    )));
    assert_eq!(messages.iter().filter(|message| matches!(message, Message::ZoomFont { card, delta: 0, editor: false } if *card == terminal)).count(), 1);
    assert!(messages.iter().any(|message| matches!(message, Message::RoutedKeyEvent { widget: Some(id), region, .. } if id == &terminal_id && region.pane == workspace_widget::FocusedPane::Terminal && region.card == Some(terminal))));
    dispatch(&mut app, messages);
    assert_eq!(
        app.settings.terminal_font_size,
        AppSettings::default().terminal_font_size
    );
    assert_eq!(app.settings.editor_font_size, Some(19.0));
    let requests = peer.fence();
    assert!(
        !requests.iter().any(|request| matches!(
            request,
            ClientRequest::Key { .. } | ClientRequest::Input { .. } | ClientRequest::Paste { .. }
        )),
        "local zoom must not become terminal input: {requests:?}"
    );
}
