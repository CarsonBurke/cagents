//! Actual App view/native keyboard routing, not a direct KeyEvent substitute.
use super::*;
use cosmic::iced::{advanced::{clipboard, mouse, widget::operation::focusable}, runtime::{UserInterface, user_interface::Cache}, Size};

fn key(character: &str, code: keyboard::key::Code, modifiers: keyboard::Modifiers, text: Option<&str>) -> iced::Event {
    let key = keyboard::Key::Character(character.to_owned().into());
    iced::Event::Keyboard(keyboard::Event::KeyPressed {
        key: key.clone(), modified_key: key, physical_key: keyboard::key::Physical::Code(code), location: keyboard::Location::Standard,
        modifiers, text: text.map(Into::into), repeat: false,
    })
}
fn native(app: &Cagents, renderer: &mut cosmic::Renderer, cache: Cache, focus: Option<widget::Id>, events: &[iced::Event]) -> (Cache, Vec<Message>) {
    let mut ui = UserInterface::build(app.view(), Size::new(1600.0, 1000.0), cache, renderer);
    if let Some(focus) = focus { ui.operate(renderer, &mut focusable::focus::<()>(focus)); }
    let mut messages = Vec::new();
    let _ = ui.update(events, mouse::Cursor::Unavailable, renderer, &mut clipboard::Null, &mut messages);
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
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else { panic!("editable repository") };
    runtime.editor_text().unwrap()
}
fn dispatch(app: &mut Cagents, messages: Vec<Message>) {
    for message in messages { drop(app.update(message)); }
}
fn seed_reopen(app: &mut Cagents, source: CardId) {
    let mut closed = app.card_state(source).unwrap().clone();
    closed.id = CardId::new();
    closed.kind = CardKind::Settings;
    app.recently_closed.push((closed, app.active_workspace_destination()));
}

#[test]
fn actual_app_editor_undo_and_shift_redo_keep_exact_text_without_reopening_a_pane() {
    use keyboard::key::Code;
    let (mut app, card) = super::tests::editing_repository_app();
    prepare(&mut app);
    seed_reopen(&mut app, card);
    let before = text(&app, card);
    let mut renderer = super::tests::headless_renderer();
    let (cache, messages) = native(&app, &mut renderer, Cache::new(), Some(repository_widget::editor_focus_id(card)), &[key("x", Code::KeyX, keyboard::Modifiers::empty(), Some("x"))]);
    assert!(messages.iter().any(|m| matches!(m, Message::RoutedKeyEvent { region, widget: Some(id), .. } if region.pane == workspace_widget::FocusedPane::Editor && region.card == Some(card) && id == &repository_widget::editor_focus_id(card))));
    dispatch(&mut app, messages);
    let edited = text(&app, card);
    assert_ne!(edited, before, "the actual native editor must positively edit");
    let (cache, messages) = native(&app, &mut renderer, cache, None, &[key("z", Code::KeyZ, keyboard::Modifiers::CTRL, None)]);
    dispatch(&mut app, messages);
    assert_eq!(text(&app, card), before);
    let (_cache, messages) = native(&app, &mut renderer, cache, None, &[key("z", Code::KeyZ, keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT, None)]);
    dispatch(&mut app, messages);
    assert_eq!(text(&app, card), edited, "redo must restore the exact edited value");
    assert_eq!(app.recently_closed.len(), 1, "a real reopen candidate must remain untouched");
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![card]);
}

#[test]
fn actual_sidebar_field_owns_find_new_tab_redo_and_save_while_the_editor_is_behind_it() {
    use keyboard::key::Code;
    let (mut app, card) = super::tests::editing_repository_app();
    app.sidebar_model.projects.push(sidebar::SidebarProject::new(ProjectId::new(), RepositoryId::new(), "Keyboard fixture".into(), "/keyboard-fixture".into(), Vec::new(), Vec::new()).with_kind(ProjectKind::Folder));
    app.sidebar_state.apply(&SidebarAction::ToggleControls);
    prepare(&mut app);
    assert!(sidebar::search_is_reachable(&app.sidebar_model, app.projects_sidebar_mode()));
    seed_reopen(&mut app, card);
    let before = text(&app, card);
    let mut renderer = super::tests::headless_renderer();
    let events = [
        key("f", Code::KeyF, keyboard::Modifiers::CTRL, None),
        key("t", Code::KeyT, keyboard::Modifiers::CTRL, None),
        key("z", Code::KeyZ, keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT, None),
        key("s", Code::KeyS, keyboard::Modifiers::CTRL, None),
        key("s", Code::KeyS, keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT, None),
        iced::Event::InputMethod(iced::advanced::input_method::Event::Commit("native-query".into())),
    ];
    let (_cache, messages) = native(&app, &mut renderer, Cache::new(), Some(sidebar::search_focus_id()), &events);
    let routed: Vec<_> = messages.iter().filter_map(|m| if let Message::RoutedKeyEvent { region, widget, .. } = m { Some((region, widget)) } else { None }).collect();
    assert_eq!(routed.len(), 5, "each actual keyboard event is published once");
    for (region, id) in routed {
        assert_eq!(region.pane, workspace_widget::FocusedPane::TextInput);
        assert_eq!(region.card, None);
        assert_eq!(id.as_ref(), Some(&sidebar::search_focus_id()));
    }
    dispatch(&mut app, messages);
    assert_eq!(app.sidebar_state.query(), "native-query", "the intended field must positively accept IME");
    assert_eq!(text(&app, card), before);
    assert_eq!(app.recently_closed.len(), 1);
    assert_eq!(app.workspace.order().collect::<Vec<_>>(), vec![card]);
    let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else { unreachable!() };
    assert!(!runtime.find_is_open());
    assert!(!runtime.is_saving());
}

#[test]
fn actual_empty_workspace_actions_are_nontext_controls_with_no_background_pane_owner() {
    use keyboard::key::Code;
    let mut app = super::tests::context_inspector_test_app(false);
    prepare(&mut app);
    let mut renderer = super::tests::headless_renderer();
    let mut ids: Vec<_> = ["empty.new-conversation", "empty.resume-conversation", "empty.new-terminal", "empty.new-browser"].into_iter().map(widget::Id::new).collect();
    ids.extend(crate::agent::Provider::ALL.into_iter().map(|provider| widget::Id::new(format!("empty.provider.{}", provider.label()))));
    for id in ids {
        let (_cache, messages) = native(&app, &mut renderer, Cache::new(), Some(id.clone()), &[key("t", Code::KeyT, keyboard::Modifiers::CTRL, None)]);
        let routed: Vec<_> = messages.iter().filter_map(|m| if let Message::RoutedKeyEvent { event, region, widget, .. } = m { Some((event, region, widget)) } else { None }).collect();
        assert_eq!(routed.len(), 1, "{id:?}");
        let (event, region, actual) = routed[0];
        assert_eq!(actual.as_ref(), Some(&id), "the actual control must be mounted and focusable");
        assert_eq!(region.pane, workspace_widget::FocusedPane::Other);
        assert_eq!(region.card, None);
        assert_eq!(workspace_widget::global_shortcut(event, &app.settings.keyboard_shortcuts, region.pane), Some(workspace_widget::GlobalShortcut::NewThread));
    }
}


#[test]
fn routed_owner_is_immutable_across_a_newer_sample_and_a_closed_owner_is_refused() {
    use keyboard::key::Code;
    let (mut app, card) = super::tests::editing_repository_app();
    prepare(&mut app);
    seed_reopen(&mut app, card);
    let other_id = widget::Id::unique();
    let sample = crate::input_trace::KeyboardRegion { pane: workspace_widget::FocusedPane::Other, card: None };
    let current = app.input_trace.scoped_keyboard_focus(sample, Some(other_id.clone()));
    let iced::Event::Keyboard(event) = key("z", Code::KeyZ, keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT, None) else { unreachable!() };
    drop(app.update(Message::RoutedKeyEvent {
        event: event.clone(), captured: false,
        region: crate::input_trace::KeyboardRegion { pane: workspace_widget::FocusedPane::TextInput, card: None }, widget: Some(sidebar::search_focus_id()),
    }));
    assert_eq!(app.recently_closed.len(), 1, "the older immutable field event cannot borrow the newer nontext context");
    assert_eq!(app.input_trace.keyboard_widget_id(), Some(other_id));
    assert_eq!(app.input_trace.keyboard_focus(), Some(workspace_widget::FocusedPane::Other));
    drop(app.update(Message::RoutedKeyEvent {
        event, captured: false,
        region: crate::input_trace::KeyboardRegion { pane: workspace_widget::FocusedPane::Other, card: Some(CardId::new()) }, widget: None,
    }));
    assert_eq!(app.recently_closed.len(), 1, "an already-closed native owner cannot target the currently selected pane");
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
    app.workspace.add_card(
        crate::workspace::WorkspaceCard::new(selected, "Other pane", crate::workspace::CardWidth::Fraction(0.4)).unwrap(),
        1, true,
    ).unwrap();
    prepare(&mut app);
    let header = app.card_header_widget_id(owner);
    assert_ne!(header, app.card_keyboard_focus(owner), "the retained native owner must differ from the pane's default editor target");
    assert_eq!(app.workspace.focused(), Some(selected));
    let mut renderer = super::tests::headless_renderer();
    let (cache, messages) = native(&app, &mut renderer, Cache::new(), Some(header.clone()), &[key("q", Code::KeyQ, keyboard::Modifiers::empty(), None)]);
    let routed = messages.into_iter().find(|m| matches!(m, Message::RoutedKeyEvent { widget: Some(id), region, .. } if id == &header && region.card == Some(owner))).expect("actual mounted nonselected header owns the event");
    let task = app.update(routed);
    assert_eq!(app.workspace.focused(), Some(owner));
    let mut ui = UserInterface::build(app.view(), Size::new(1600.0, 1000.0), cache, &mut renderer);
    let executor = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    executor.block_on(async {
        if let Some(mut stream) = iced::runtime::task::into_stream(task) {
            // Exercise the actual ready effects, including any erroneous
            // default-focus operation. Maintenance timers are unrelated.
            while let Ok(Some(action)) = tokio::time::timeout(Duration::from_millis(100), stream.next()).await {
                if let iced::runtime::Action::Widget(operation) = action {
                    let mut next = Some(operation);
                    while let Some(mut operation) = next.take() {
                        ui.operate(&renderer, operation.as_mut());
                        if let iced::advanced::widget::operation::Outcome::Chain(chained) = operation.finish() { next = Some(chained); }
                    }
                }
            }
        }
    });
    executor.shutdown_background();
    let mut messages = Vec::new();
    let _ = ui.update(&[key("q", Code::KeyQ, keyboard::Modifiers::empty(), None)], mouse::Cursor::Unavailable, &mut renderer, &mut clipboard::Null, &mut messages);
    assert!(messages.iter().any(|m| matches!(m, Message::RoutedKeyEvent { widget: Some(id), region, .. } if id == &header && region.card == Some(owner))), "logical reconciliation and its actual widget operations must preserve the nondefault native owner");
}
