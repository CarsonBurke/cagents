//! Real App/native-widget routing. Tasks which would start providers are never polled.
use super::*;
use crate::input_trace::KeyboardOwner;
use cosmic::iced::{
    Size,
    advanced::{clipboard, mouse, widget::operation::focusable},
    runtime::{UserInterface, user_interface::Cache},
};
use workspace_widget::FocusedPane;

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
fn release(event: &iced::Event) -> iced::Event {
    let iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key,
        modified_key,
        physical_key,
        location,
        modifiers,
        ..
    }) = event
    else {
        panic!("press")
    };
    iced::Event::Keyboard(keyboard::Event::KeyReleased {
        key: key.clone(),
        modified_key: modified_key.clone(),
        physical_key: *physical_key,
        location: *location,
        modifiers: *modifiers,
    })
}
pub(super) fn native(
    app: &Cagents,
    renderer: &mut cosmic::Renderer,
    cache: Cache,
    focus: Option<widget::Id>,
    events: &[iced::Event],
) -> (Cache, Vec<Message>) {
    let mut ui = UserInterface::build(app.view(), Size::new(1600.0, 1000.0), cache, renderer);
    if let Some(id) = focus {
        ui.operate(renderer, &mut focusable::focus::<()>(id));
    }
    let mut messages = Vec::new();
    let (_, statuses) = ui.update(
        events,
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    // The actual runtime broadcasts the same events to listen_with after the
    // native pass. Include that path to prove it cannot duplicate a command.
    messages.extend(events.iter().zip(statuses).filter_map(|(event, status)| {
        if let iced::Event::Keyboard(event) = event {
            Some(Message::KeyEventFallback {
                event: event.clone(),
                captured: status == iced::event::Status::Captured,
            })
        } else {
            None
        }
    }));
    (ui.into_cache(), messages)
}
fn dispatch(app: &mut Cagents, messages: Vec<Message>) {
    for message in messages {
        drop(app.update(message));
    }
}
pub(super) fn prepare(app: &mut Cagents) {
    app.window_width = 1600.0;
    app.window_height = 1000.0;
    app.context_inspector_open = false;
    app.refresh_ui.focused = true;
    let _ = app.workspace.resize_viewport(1200.0);
    drop(app.sync_chrome());
    assert!(!app.workspace_inputs_blocked());
}
fn editor_app() -> (Cagents, CardId) {
    let mut app = super::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    Arc::make_mut(&mut app.durable)
        .cards
        .iter_mut()
        .find(|state| state.id == card)
        .unwrap()
        .kind = CardKind::Repository(RepositoryView {
        selected_file: None,
        project: ProjectId::new(),
        repository: "/repo/.git".into(),
        worktree: "/repo".into(),
    });
    app.cards.insert(
        card,
        CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
    );
    prepare(&mut app);
    (app, card)
}
fn editor_text(app: &Cagents, card: CardId) -> String {
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        panic!("editor")
    };
    runtime.editor_text().unwrap()
}
fn seed_reopen(app: &mut Cagents, card: CardId) {
    let mut closed = app.card_state(card).unwrap().clone();
    closed.id = CardId::new();
    closed.kind = CardKind::Settings;
    app.recently_closed
        .push((closed, app.active_workspace_destination()));
}
fn add_search(app: &mut Cagents) {
    app.sidebar_model.projects.push(
        sidebar::SidebarProject::new(
            ProjectId::new(),
            crate::repository::RepositoryId::new(),
            "Keyboard fixture".into(),
            "/keyboard-fixture".into(),
            Vec::new(),
            Vec::new(),
        )
        .with_kind(ProjectKind::Folder),
    );
    app.sidebar_state.apply(&SidebarAction::ToggleControls);
    assert!(sidebar::search_is_reachable(
        &app.sidebar_model,
        app.projects_sidebar_mode()
    ));
}

#[test]
fn native_editor_undo_shift_redo_and_persisted_collisions_reach_the_real_editor() {
    use keyboard::key::Code;
    let (mut app, card) = editor_app();
    seed_reopen(&mut app, card);
    let before = editor_text(&app, card);
    let mut renderer = super::tests::headless_renderer();
    let insert = key("x", Code::KeyX, keyboard::Modifiers::empty(), Some("x"));
    let (mut cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(repository_widget::editor_focus_id(card)),
        &[insert.clone(), release(&insert)],
    );
    dispatch(&mut app, messages);
    let edited = editor_text(&app, card);
    assert_ne!(edited, before, "positive native edit control");
    for (character, code, modifiers, expected, redo) in [
        (
            "z",
            Code::KeyZ,
            keyboard::Modifiers::CTRL,
            before.clone(),
            false,
        ),
        (
            "z",
            Code::KeyZ,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            edited.clone(),
            true,
        ),
        (
            "z",
            Code::KeyZ,
            keyboard::Modifiers::CTRL,
            before.clone(),
            false,
        ),
        (
            "y",
            Code::KeyY,
            keyboard::Modifiers::CTRL,
            edited.clone(),
            true,
        ),
    ] {
        // A persisted collision is also protected, not only default Reopen.
        app.settings.keyboard_shortcuts.clone_pane = vec!["Ctrl+Y".into()];
        let press = key(character, code, modifiers, None);
        let (next, messages) = native(
            &app,
            &mut renderer,
            cache,
            None,
            &[press.clone(), release(&press)],
        );
        cache = next;
        assert!(messages.iter().any(|message| matches!(message,
            Message::Repository(RepositoryAction::Undo { card: found }) if !redo && *found == card
        ) || matches!(message, Message::Repository(RepositoryAction::Redo { card: found }) if redo && *found == card)), "real history callback must reach App");
        assert_eq!(messages.iter().filter(|m| matches!(m, Message::NativeKeyEvent { owner: KeyboardOwner { pane: FocusedPane::Editor, card: Some(found), .. }, .. } if *found == card)).count(), 2);
        dispatch(&mut app, messages);
        assert_eq!(editor_text(&app, card), expected);
        assert_eq!(
            app.recently_closed.len(),
            1,
            "Reopen callback must not execute"
        );
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            vec![card],
            "Clone callback must not execute"
        );
    }
}

#[test]
fn native_editor_shifted_select_all_and_cut_keep_persisted_workspace_collisions() {
    use keyboard::key::Code;
    let (mut app, card) = editor_app();
    let before = editor_text(&app, card);
    assert!(!before.trim().is_empty(), "positive cut control has text");
    app.settings.keyboard_shortcuts.clone_pane = vec!["Ctrl+Shift+A".into()];
    app.settings.keyboard_shortcuts.close_focused_column = vec!["Ctrl+Shift+X".into()];
    let mut renderer = super::tests::headless_renderer();
    let modifiers = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
    let select_all = key("a", Code::KeyA, modifiers, None);
    let (cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(repository_widget::editor_focus_id(card)),
        &[select_all.clone(), release(&select_all)],
    );
    dispatch(&mut app, messages);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![card]);
    let cut = key("x", Code::KeyX, modifiers, None);
    let (_, messages) = native(
        &app,
        &mut renderer,
        cache,
        None,
        &[cut.clone(), release(&cut)],
    );
    dispatch(&mut app, messages);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![card]);
    assert!(
        editor_text(&app, card).trim().is_empty(),
        "the real editor must select and cut its text instead of cloning or closing the pane"
    );
}

#[test]
fn native_field_scope_is_per_event_even_when_focus_changes_before_app_dispatch() {
    use keyboard::key::Code;
    let (mut app, card) = editor_app();
    add_search(&mut app);
    seed_reopen(&mut app, card);
    let mut renderer = super::tests::headless_renderer();
    let insert = key("x", Code::KeyX, keyboard::Modifiers::empty(), Some("x"));
    let (cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(repository_widget::editor_focus_id(card)),
        &[insert.clone(), release(&insert)],
    );
    dispatch(&mut app, messages);
    let edited = editor_text(&app, card);
    let undo = key("z", Code::KeyZ, keyboard::Modifiers::CTRL, None);
    let (cache, mut queued) = native(
        &app,
        &mut renderer,
        cache,
        None,
        &[undo.clone(), release(&undo)],
    );
    // A real number-row split must retain603's symbol fallback while refusing
    // its pane command when a sidebar field owns this particular event.
    app.settings.keyboard_shortcuts.clone_pane = vec!["Ctrl+Shift+!".into()];
    let mut shifted = key(
        "1",
        Code::Digit1,
        keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        None,
    );
    if let iced::Event::Keyboard(keyboard::Event::KeyPressed { modified_key, .. }) = &mut shifted {
        *modified_key = keyboard::Key::Character("!".into());
    }
    let redo = key(
        "z",
        Code::KeyZ,
        keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        None,
    );
    let (_cache, field) = native(
        &app,
        &mut renderer,
        cache,
        Some(sidebar::search_focus_id()),
        &[
            redo.clone(),
            release(&redo),
            shifted.clone(),
            release(&shifted),
            key("q", Code::KeyQ, keyboard::Modifiers::empty(), Some("q")),
        ],
    );
    assert!(queued.iter().any(|m| matches!(
        m,
        Message::NativeKeyEvent {
            owner: KeyboardOwner {
                pane: FocusedPane::Editor,
                ..
            },
            ..
        }
    )));
    assert!(field.iter().any(|m| matches!(
        m,
        Message::NativeKeyEvent {
            owner: KeyboardOwner {
                pane: FocusedPane::TextInput,
                card: None,
                ..
            },
            ..
        }
    )));
    assert!(field.iter().any(|message| matches!(message,
        Message::NativeKeyEvent { event, owner: KeyboardOwner { pane: FocusedPane::TextInput, card: None, .. }, .. }
            if iced::Event::Keyboard(event.clone()) == shifted
    )), "split-symbol event retains its actual field owner");
    queued.extend(field);
    dispatch(&mut app, queued);
    assert_ne!(
        editor_text(&app, card),
        edited,
        "queued editor Undo retained its own owner"
    );
    assert_eq!(
        app.sidebar_state.query(),
        "q",
        "actual field positively accepted text"
    );
    assert_eq!(
        app.recently_closed.len(),
        1,
        "field Redo never reopens a background pane"
    );
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        vec![card],
        "neither shifted-symbol fallback nor field Redo may clone/reopen a pane"
    );
}

#[test]
fn native_chat_composer_keeps_redo_and_clipboard_custom_bindings() {
    use keyboard::key::Code;
    let mut app = super::tests::context_inspector_test_app(false);
    drop(app.new_native_chat(
        crate::agent::Provider::Codex,
        Some("/keyboard-fixture".into()),
    ));
    let card = app.workspace.focused().unwrap();
    prepare(&mut app);
    seed_reopen(&mut app, card);
    assert!(matches!(app.cards.get(&card), Some(CardRuntime::Chat(_))));
    app.settings.keyboard_shortcuts.clone_pane = vec!["Ctrl+C".into()];
    let mut renderer = super::tests::headless_renderer();
    let redo = key(
        "z",
        Code::KeyZ,
        keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        None,
    );
    let copy = key("c", Code::KeyC, keyboard::Modifiers::CTRL, None);
    let (_cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(card_control_id(card, "chat-composer")),
        &[redo.clone(), release(&redo), copy.clone(), release(&copy)],
    );
    assert_eq!(messages.iter().filter(|message| matches!(message, Message::NativeKeyEvent { owner: KeyboardOwner { pane: FocusedPane::Chat, card: Some(found), .. }, .. } if *found == card)).count(), 4);
    dispatch(&mut app, messages);
    assert_eq!(app.recently_closed.len(), 1);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![card]);
    assert!(
        !app.chat_start_pending(card),
        "no prompt or provider is started"
    );
}

#[test]
fn native_nontext_other_reopens_once_and_stale_owner_does_not_dispatch() {
    use keyboard::key::Code;
    let (mut app, card) = editor_app();
    seed_reopen(&mut app, card);
    let mut renderer = super::tests::headless_renderer();
    let redo = key(
        "z",
        Code::KeyZ,
        keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        None,
    );
    let (_cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(widget::Id::new("sidebar.settings")),
        &[redo.clone(), release(&redo)],
    );
    assert!(messages.iter().any(|m| matches!(
        m,
        Message::NativeKeyEvent {
            owner: KeyboardOwner {
                pane: FocusedPane::Other,
                card: None,
                ..
            },
            ..
        }
    )));
    dispatch(&mut app, messages);
    assert!(
        app.recently_closed.is_empty(),
        "one ordinary Reopen remains available on a nontext control"
    );
    assert_eq!(
        app.workspace.order().count(),
        2,
        "subscription fallback did not reopen twice"
    );
    seed_reopen(&mut app, card);
    let iced::Event::Keyboard(event) = redo else {
        unreachable!()
    };
    drop(app.update(Message::NativeKeyEvent {
        event,
        captured: true,
        owner: KeyboardOwner {
            pane: FocusedPane::Other,
            card: Some(card),
            generation: app.card_state(card).map(|state| state.generation),
        },
        generation: app.workspace_view_generation.wrapping_add(1),
    }));
    assert_eq!(app.recently_closed.len(), 1);
    assert_eq!(app.workspace.order().count(), 2);
    let stale_card = app.card_state(card).unwrap().generation.wrapping_add(1);
    drop(app.update(Message::NativeKeyEvent {
        event: match key(
            "z",
            Code::KeyZ,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            None,
        ) {
            iced::Event::Keyboard(event) => event,
            _ => unreachable!(),
        },
        captured: true,
        owner: KeyboardOwner {
            pane: FocusedPane::Other,
            card: Some(card),
            generation: Some(stale_card),
        },
        generation: app.workspace_view_generation,
    }));
    assert_eq!(
        app.recently_closed.len(),
        1,
        "replacement card incarnation refuses an old native event"
    );

    // Invalid queued owners must refuse global commands too, not only the
    // pane commands which OutsideWorkspace independently withholds.
    let original_order = app.workspace.order().collect::<Vec<_>>();
    let current_generation = app.workspace_view_generation;
    let current_card = app.card_state(card).unwrap().generation;
    for (owner, generation) in [
        (KeyboardOwner::UNKNOWN, current_generation.wrapping_add(1)),
        (
            KeyboardOwner {
                pane: FocusedPane::Other,
                card: Some(card),
                generation: Some(current_card.wrapping_add(1)),
            },
            current_generation,
        ),
        (
            KeyboardOwner {
                pane: FocusedPane::Other,
                card: Some(CardId::new()),
                generation: None,
            },
            current_generation,
        ),
    ] {
        for (character, code, modifiers) in [
            ("t", Code::KeyT, keyboard::Modifiers::CTRL),
            (
                "p",
                Code::KeyP,
                keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            ),
        ] {
            let iced::Event::Keyboard(event) = key(character, code, modifiers, None) else {
                unreachable!()
            };
            let task = app.update(Message::NativeKeyEvent {
                event,
                captured: true,
                owner,
                generation,
            });
            assert_eq!(task.units(), 0, "stale global input emits no effect");
            assert_eq!(app.workspace.order().collect::<Vec<_>>(), original_order);
            assert!(
                !app.palette.is_open(),
                "stale global input cannot open palette"
            );
            assert_eq!(app.recently_closed.len(), 1);
        }
    }
    let iced::Event::Keyboard(event) = key(
        "p",
        Code::KeyP,
        keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        None,
    ) else {
        unreachable!()
    };
    drop(app.update(Message::NativeKeyEvent {
        event,
        captured: true,
        owner: KeyboardOwner::UNKNOWN,
        generation: current_generation,
    }));
    assert!(
        app.palette.is_open(),
        "a current unknown native owner retains ordinary global palette access"
    );
}

#[test]
fn unobserved_chrome_escape_still_uses_the_existing_overlay_fallback() {
    let (mut app, _) = editor_app();
    app.window_width = 800.0;
    app.context_inspector_open = true;
    assert!(app.context_inspector_is_overlay());
    let event = keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(keyboard::key::Named::Escape),
        modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
        physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: None,
        repeat: false,
    };
    let task = app.update(Message::KeyEventFallback {
        event,
        captured: false,
    });
    assert!(matches!(
        super::tests::task_app_message(task),
        Message::ContextInspector(InspectorAction::Close)
    ));
}

#[cfg(feature = "integrated-browser")]
#[test]
fn integrated_browser_model_focus_keeps_ctrl_l_but_native_outside_field_owns_its_keys() {
    use keyboard::key::Code;
    let mut app = super::tests::context_inspector_test_app(true);
    let card = app.workspace.focused().unwrap();
    let url = Url::parse("https://example.test/").unwrap();
    app.card_state_mut(card).unwrap().kind = CardKind::Browser(crate::model::BrowserSession {
        id: crate::model::BrowserId::new(),
        current_url: url.clone(),
        history: vec![url.clone()],
        history_index: 0,
        scroll_x: 0.0,
        scroll_y: 0.0,
        fidelity: crate::model::BrowserRestoreFidelity::HistoryAndScroll,
    });
    app.cards.insert(
        card,
        CardRuntime::Browser(BrowserRuntime {
            handle: BrowserHandle::fixture(),
            ime: None,
            lifecycle: BrowserLifecycle::Visible,
            address: BrowserAddress::location(&url),
            address_focused: false,
            address_error: false,
            progress_phase: 0.0,
            load_state: None,
        }),
    );
    prepare(&mut app);
    add_search(&mut app);
    let mut renderer = super::tests::headless_renderer();
    let chord = key("l", Code::KeyL, keyboard::Modifiers::CTRL, None);
    // No Iced Focusable represents Servo's existing model-owned surface.
    let (cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        None,
        &[chord.clone(), release(&chord)],
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, Message::FocusBrowserAddress(found) if *found == card))
            .count(),
        1
    );
    assert!(messages.iter().any(|m| matches!(m, Message::NativeKeyEvent { owner: KeyboardOwner { pane: FocusedPane::Other, card: Some(found), .. }, .. } if *found == card)));
    // Do not execute the focus-address task before this explicit outside-field
    // control: test the native field against the still model-focused browser.
    dispatch(
        &mut app,
        messages
            .into_iter()
            .filter(|m| !matches!(m, Message::FocusBrowserAddress(_)))
            .collect(),
    );
    let (_cache, messages) = native(
        &app,
        &mut renderer,
        cache,
        Some(sidebar::search_focus_id()),
        &[chord.clone(), release(&chord)],
    );
    assert!(messages.iter().all(|m| !matches!(
        m,
        Message::FocusBrowserAddress(_) | Message::BrowserReload(_)
    )));
    assert!(messages.iter().any(|m| matches!(
        m,
        Message::NativeKeyEvent {
            owner: KeyboardOwner {
                pane: FocusedPane::TextInput,
                card: None,
                ..
            },
            ..
        }
    )));
    dispatch(&mut app, messages);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![card]);
}

#[test]
fn actual_focused_terminal_keeps_default_reopen_without_forwarding_the_reserved_key() {
    use keyboard::key::Code;
    // Existing UI-side client fixture: no daemon or terminal child is started.
    let (mut app, card, _directory) = super::tests::live_agent_app("/bin/sh");
    prepare(&mut app);
    seed_reopen(&mut app, card);
    let mut renderer = super::tests::headless_renderer();
    let redo = key(
        "z",
        Code::KeyZ,
        keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        None,
    );
    let (_cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(app.card_keyboard_focus(card)),
        &[redo.clone(), release(&redo)],
    );
    assert_eq!(messages.iter().filter(|m| matches!(m, Message::NativeKeyEvent { captured: true, owner: KeyboardOwner { pane: FocusedPane::Terminal, card: Some(found), .. }, .. } if *found == card)).count(), 2);
    dispatch(&mut app, messages);
    assert!(app.recently_closed.is_empty());
    assert_eq!(app.workspace.order().count(), 2);
    assert!(
        matches!(app.cards.get(&card), Some(CardRuntime::Terminal(_))),
        "original terminal remains mounted"
    );
}

fn core_native(
    app: &Cagents,
    renderer: &mut cosmic::Renderer,
    cache: Cache,
    focus: widget::Id,
    events: &[iced::Event],
) -> (Cache, Vec<Message>, Vec<iced::event::Status>) {
    // The real COSMIC root injects Cagents::dialog outside App.view's observer.
    let mut ui = UserInterface::build(app.view_main(), Size::new(1600.0, 1000.0), cache, renderer);
    ui.operate(renderer, &mut focusable::focus::<()>(focus));
    let mut all = Vec::new();
    let (_, statuses) = ui.update(
        events,
        mouse::Cursor::Unavailable,
        renderer,
        &mut clipboard::Null,
        &mut all,
    );
    let mut messages: Vec<_> = all
        .into_iter()
        .filter_map(|action| match action {
            cosmic::Action::App(message) => Some(message),
            _ => None,
        })
        .collect();
    messages.extend(events.iter().zip(&statuses).filter_map(|(event, status)| {
        if let iced::Event::Keyboard(event) = event {
            Some(Message::KeyEventFallback {
                event: event.clone(),
                captured: *status == iced::event::Status::Captured,
            })
        } else {
            None
        }
    }));
    (ui.into_cache(), messages, statuses)
}
fn named(named: keyboard::key::Named, code: keyboard::key::Code) -> iced::Event {
    iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key: keyboard::Key::Named(named),
        modified_key: keyboard::Key::Named(named),
        physical_key: keyboard::key::Physical::Code(code),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: None,
        repeat: false,
    })
}

#[test]
fn real_cosmic_dialog_keeps_modal_arrows_enter_and_captured_query_without_native_duplicates() {
    use keyboard::key::{Code, Named};
    let (mut app, card) = editor_app();
    seed_reopen(&mut app, card);
    app.daemon_health_ui.confirmation = true; // Only the existing Cancel control is enabled.
    assert!(app.modal_is_open());
    let mut renderer = super::tests::headless_renderer();
    let (cache, messages, _) = core_native(
        &app,
        &mut renderer,
        Cache::new(),
        widget::Id::new("daemon-health.cancel"),
        &[named(Named::ArrowRight, Code::ArrowRight)],
    );
    assert!(
        messages
            .iter()
            .all(|m| !matches!(m, Message::NativeKeyEvent { .. }))
    );
    let mut focus_task = None;
    for message in messages {
        let is_key = matches!(message, Message::KeyEventFallback { .. });
        let task = app.update(message);
        if is_key {
            focus_task = Some(task);
        }
    }
    assert_eq!(
        super::tests::focused_element(
            app.dialog().unwrap(),
            focus_task.expect("one modal arrow fallback")
        )
        .map(|id| id.to_string()),
        Some("daemon-health.cancel".into())
    );
    let (_cache, messages, _) = core_native(
        &app,
        &mut renderer,
        cache,
        widget::Id::new("daemon-health.cancel"),
        &[named(Named::Enter, Code::Enter)],
    );
    assert_eq!(
        messages
            .iter()
            .filter(|m| matches!(m, Message::DaemonHealthUi(daemon_health_ui::Action::Cancel)))
            .count(),
        1,
        "actual mounted Cancel activates once"
    );
    assert!(
        messages
            .iter()
            .all(|m| !matches!(m, Message::NativeKeyEvent { .. }))
    );
    dispatch(&mut app, messages);
    assert!(!app.daemon_health_ui.confirmation);
    assert_eq!(app.recently_closed.len(), 1);

    // A captured field key also remains on the modal's legacy chain; it must
    // not be marked captured by an observer underneath the COSMIC dialog.
    app.resume_ui.open = true;
    let (_cache, messages, statuses) = core_native(
        &app,
        &mut renderer,
        Cache::new(),
        widget::Id::new("conversation-resume.search"),
        &[key(
            "q",
            Code::KeyQ,
            keyboard::Modifiers::empty(),
            Some("q"),
        )],
    );
    assert_eq!(statuses, vec![iced::event::Status::Captured]);
    assert!(
        messages
            .iter()
            .all(|m| !matches!(m, Message::NativeKeyEvent { .. }))
    );
    assert_eq!(messages.iter().filter(|m| matches!(m, Message::ResumeConversation(resume_ui::Action::Query(value)) if value == "q")).count(), 1);
    dispatch(&mut app, messages);
    assert_eq!(app.resume_ui.query, "q");
    assert_eq!(app.recently_closed.len(), 1);
}

#[test]
fn native_editor_f3_keeps_open_find_navigation_after_query_loses_focus() {
    let (mut app, card) = editor_app();
    drop(app.update(Message::Repository(RepositoryAction::OpenFind { card })));
    drop(
        app.update(Message::Repository(RepositoryAction::FindInputFocus {
            card,
            focused: false,
        })),
    );
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
        panic!("repository");
    };
    assert!(runtime.find_is_open());
    assert!(!runtime.find_input_is_focused());
    let mut renderer = super::tests::headless_renderer();
    let press = named(keyboard::key::Named::F3, keyboard::key::Code::F3);
    let (_cache, messages) = native(
        &app,
        &mut renderer,
        Cache::new(),
        Some(repository_widget::editor_focus_id(card)),
        &[press],
    );
    let mut navigated = 0;
    for message in messages {
        if matches!(&message, Message::NativeKeyEvent {
            event: keyboard::Event::KeyPressed { .. },
            owner: KeyboardOwner {pane: FocusedPane::Editor, card: Some(found), ..},
            ..
        } if *found == card)
        {
            let action = super::tests::task_app_message(app.update(message));
            assert!(matches!(action,
                Message::Repository(RepositoryAction::FindNext { card: found }) if found == card));
            navigated += 1;
        } else {
            drop(app.update(message));
        }
    }
    assert_eq!(
        navigated, 1,
        "real editor-owned F3 retains Main's navigation"
    );
    // A queued owner for a different card still cannot navigate this editor.
    let iced::Event::Keyboard(event) = named(keyboard::key::Named::F3, keyboard::key::Code::F3)
    else {
        unreachable!()
    };
    let generation = app.workspace_view_generation;
    let task = app.update(Message::NativeKeyEvent {
        event,
        captured: false,
        owner: KeyboardOwner {
            pane: FocusedPane::Editor,
            card: Some(CardId::new()),
            generation: None,
        },
        generation,
    });
    assert_eq!(
        task.units(),
        0,
        "a foreign event cannot use the current Find bar"
    );
}

#[test]
fn native_font_zoom_stays_with_the_field_or_editor_that_owns_focus() {
    use keyboard::key::Code;
    let (mut app, card) = editor_app();
    app.settings.editor_font_size = Some(19.0);
    add_search(&mut app);
    let mut renderer = super::tests::headless_renderer();
    let events = [
        key("0", Code::Digit0, keyboard::Modifiers::CTRL, None),
        key("=", Code::Equal, keyboard::Modifiers::CTRL, None),
        key("-", Code::Minus, keyboard::Modifiers::CTRL, None),
    ];
    let mut field_events = Vec::new();
    for event in &events {
        field_events.extend([event.clone(), release(event)]);
    }
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
    assert!(messages.iter().any(|message| matches!(
        message,
        Message::NativeKeyEvent {
            owner: KeyboardOwner {
                pane: FocusedPane::TextInput,
                card: None,
                ..
            },
            ..
        }
    )));
    dispatch(&mut app, messages);
    assert_eq!(app.sidebar_state.query(), "font-owner-query");
    assert_eq!(app.workspace.focused(), Some(card));
    assert_eq!(app.settings.editor_font_size, Some(19.0));
    let (_cache, messages) = native(
        &app,
        &mut renderer,
        cache,
        Some(repository_widget::editor_focus_id(card)),
        &field_events[..field_events.len() - 1],
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
    assert_eq!(app.sidebar_state.query(), "font-owner-query");
}

#[test]
fn an_earlier_mounted_inactive_editor_does_not_capture_the_terminal_font_chord() {
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
    // Reuse the existing UI-side missing-socket client fixture. No daemon,
    // provider or terminal process starts, and this fixture makes no wire claim.
    let (mut app, terminal, _directory) = super::tests::live_agent_app("/bin/sh");
    let (editor, repository) = editor_app();
    let second_repository = CardId::new();
    let mut first = editor.card_state(repository).unwrap().clone();
    first.width_fraction = 0.25;
    let mut second = first.clone();
    second.id = second_repository;
    {
        let durable = Arc::make_mut(&mut app.durable);
        durable.cards[0].width_fraction = 0.5;
        durable.cards.splice(0..0, [first, second]);
    }
    for card in [repository, second_repository] {
        app.cards.insert(
            card,
            CardRuntime::Repository(Box::new(RepositoryRuntime::dirty_fixture(card))),
        );
    }
    app.workspace = Cagents::rebuild_workspace(&app.durable);
    app.workspace.take_widget_focus_request();
    app.settings.editor_font_size = Some(19.0);
    app.settings.terminal_font_size = 18.0;
    prepare(&mut app);
    assert_eq!(
        app.workspace.order().collect::<Vec<_>>(),
        [repository, second_repository, terminal]
    );
    let mut renderer = super::tests::headless_renderer();
    let mut ui = UserInterface::build(
        app.view(),
        Size::new(1600.0, 1000.0),
        Cache::new(),
        &mut renderer,
    );
    for card in [repository, second_repository] {
        let mut editor = EditorBounds {
            id: repository_widget::editor_focus_id(card),
            bounds: None,
        };
        ui.operate(&renderer, &mut editor);
        let bounds = editor
            .bounds
            .expect("both earlier repository editors must actually be mounted");
        assert!(bounds.width > 0.0 && bounds.height > 0.0);
        assert!(bounds.x >= 0.0 && bounds.x + bounds.width <= 1600.0);
    }
    let terminal_id = app.card_keyboard_focus(terminal);
    ui.operate(&renderer, &mut focusable::focus::<()>(terminal_id));
    let press = key("0", Code::Digit0, keyboard::Modifiers::CTRL, None);
    let mut messages = Vec::new();
    let _ = ui.update(
        &[press.clone(), release(&press)],
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
    assert!(messages.iter().any(|message| matches!(message, Message::NativeKeyEvent { owner: KeyboardOwner { pane: FocusedPane::Terminal, card: Some(card), .. }, .. } if *card == terminal)));
    dispatch(&mut app, messages);
    assert_eq!(
        app.settings.terminal_font_size,
        AppSettings::default().terminal_font_size
    );
    assert_eq!(app.settings.editor_font_size, Some(19.0));
    assert_eq!(app.workspace.focused(), Some(terminal));
}
