//! Real popup input and pointer/keyboard parity without a portal or provider.
use super::*;
use iced::advanced::{clipboard, widget::operation};
use iced::runtime::{UserInterface, user_interface::Cache};
use iced_accessibility::accesskit::{Action as AxAction, ActionData, ActionRequest, Role, TreeId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Expected {
    Attach,
    Goal,
    Plan,
    Skill,
    Fork,
    Compact,
    Steering,
}
fn observed(
    messages: Vec<Message>,
    target: Target,
    phase: &str,
) -> (Option<Expected>, Vec<Action>) {
    let mut found = None;
    let mut cleanup = Vec::new();
    for message in messages {
        let Message::ChatUi(action) = message else {
            panic!("a menu must only emit its existing UI action: {message:?}")
        };
        let (owner, kind) = match &action {
            Action::Composer(owner, text_editor::Action::ClearSelection) => {
                assert_eq!(*owner, target, "{phase}: blur belongs to this composer");
                assert!(
                    cleanup.is_empty(),
                    "{phase}: at most one native blur per event"
                );
                cleanup.push(action);
                continue;
            }
            Action::Composer(_, event) => panic!(
                "{phase}: only native blur is allowed, received {}",
                composer_event_name(event)
            ),
            Action::Attach(owner) => (*owner, Expected::Attach),
            Action::ComposerAction(owner, insertion) if insertion == "/goal" => {
                (*owner, Expected::Goal)
            }
            Action::ComposerAction(owner, insertion) if insertion == "/plan" => {
                (*owner, Expected::Plan)
            }
            Action::ComposerAction(owner, insertion) if insertion == "$review" => {
                (*owner, Expected::Skill)
            }
            Action::BranchPicker(owner, true) => (*owner, Expected::Fork),
            Action::Command(owner, Command::Compact) => (*owner, Expected::Compact),
            Action::Command(owner, Command::SetInterruptMode { mode }) if mode == "wait" => {
                (*owner, Expected::Steering)
            }
            _ => panic!("{phase}: unexpected menu dispatch: {action:?}"),
        };
        assert_eq!(owner, target);
        assert!(
            found.replace(kind).is_none(),
            "one activation emits one action"
        );
    }
    (found, cleanup)
}
fn composer_event_name(event: &text_editor::Action) -> &'static str {
    match event {
        text_editor::Action::Click(_) => "Composer::Click",
        text_editor::Action::Drag(_) => "Composer::Drag",
        text_editor::Action::Move(_) => "Composer::Move",
        text_editor::Action::Select(_) => "Composer::Select",
        text_editor::Action::SelectWord => "Composer::SelectWord",
        text_editor::Action::SelectLine => "Composer::SelectLine",
        text_editor::Action::SelectAll => "Composer::SelectAll",
        text_editor::Action::ClearSelection => "Composer::ClearSelection",
        text_editor::Action::Scroll { .. } => "Composer::Scroll",
        text_editor::Action::Edit(_) => "Composer::Edit (payload omitted)",
    }
}

#[test]
fn popup_search_enter_matches_first_enabled_visible_native_or_provider_action() {
    let mut renderer = crate::app::tests::headless_renderer();
    // Query, attachment capability, chooser pending, busy, read-only, disconnected,
    // expected first enabled native row's accessible name and existing action.
    let cases = [
        (
            "",
            true,
            false,
            false,
            false,
            false,
            Some(("Attach files or images…", Expected::Attach)),
        ),
        (
            "images",
            true,
            false,
            false,
            false,
            false,
            Some(("Attach files or images…", Expected::Attach)),
        ),
        (
            "",
            true,
            true,
            false,
            false,
            false,
            Some(("Plan mode", Expected::Plan)),
        ),
        (
            "",
            true,
            true,
            true,
            false,
            false,
            Some(("Goal…", Expected::Goal)),
        ),
        ("images", true, true, false, false, false, None),
        ("attach", false, false, false, false, false, None),
        (
            "fork",
            true,
            false,
            false,
            false,
            false,
            Some(("Fork conversation…", Expected::Fork)),
        ),
        (
            "compact",
            true,
            false,
            false,
            false,
            false,
            Some((
                "Compact the conversation: summarize earlier turns to free context",
                Expected::Compact,
            )),
        ),
        ("fork", true, false, true, false, false, None),
        ("plan", true, false, true, false, false, None),
        (
            "goal",
            true,
            false,
            true,
            false,
            false,
            Some(("Goal…", Expected::Goal)),
        ),
        (
            "review",
            true,
            false,
            true,
            false,
            false,
            Some(("Skill · review", Expected::Skill)),
        ),
        ("plan", true, false, false, true, false, None),
        (
            "timing",
            true,
            false,
            true,
            false,
            false,
            Some(("Steering: after tools", Expected::Steering)),
        ),
        ("", true, false, false, false, true, None),
    ];
    for provider in [Provider::Codex, Provider::Claude, Provider::Omp] {
        for (query, attachments, pending, busy, read_only, disconnected, expected) in cases {
            let directory = tempfile::tempdir().unwrap();
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.context_inspector_open = false;
            drop(app.new_native_chat(provider, Some(directory.path().into())));
            let card = app.workspace.focused().unwrap();
            drop(app.update(Message::Viewport {
                generation: app.workspace_view_generation,
                event: WorkspaceViewportEvent::Resized(620.0),
            }));
            let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
                unreachable!()
            };
            let target = runtime.target;
            assert!(runtime.visible.load(Ordering::Relaxed));
            runtime.replace_draft(text_editor::Content::with_text("Keep 🦀 this unsent draft"));
            attachment_draft::move_to(
                &mut runtime.composer,
                text_editor::Cursor {
                    position: text_editor::Position { line: 0, column: 4 },
                    selection: Some(text_editor::Position { line: 0, column: 0 }),
                },
            );
            runtime
                .attachments
                .push(Attachment::file(directory.path().join("kept.txt")));
            runtime.attachment_chooser_pending = pending.then(uuid::Uuid::new_v4);
            runtime.disconnected = disconnected;
            let snapshot = Arc::make_mut(&mut runtime.snapshot);
            snapshot.ready = true;
            snapshot.busy = busy;
            snapshot.read_only_reason = read_only.then(|| "Fixture is read-only".into());
            snapshot.capabilities.attachments = attachments;
            snapshot.capabilities.branch = true;
            snapshot.capabilities.compact = true;
            snapshot.capabilities.interrupt_modes = vec!["immediate".into(), "wait".into()];
            snapshot.commands = vec![chat::SlashCommand {
                name: "plan".into(),
                description: "Plan mode".into(),
            }];
            snapshot.skills = vec![chat::Skill {
                name: "review".into(),
                description: "Review changes".into(),
                invocation: "$review".into(),
            }];
            let before = (
                runtime.composer.text(),
                runtime.composer.cursor(),
                runtime.attachments.clone(),
                runtime.next_command,
                runtime.attachment_chooser_pending,
                runtime.revision,
            );
            drop(app.handle_chat_ui(Action::ComposerActions(target, true)));
            let viewport = iced::Size::new(620.0, 760.0);
            let mut ui =
                UserInterface::build(app.chat_view(card), viewport, Cache::new(), &mut renderer);
            let search = card_control_id(card, "chat-actions-search");
            ui.operate(
                &renderer,
                &mut operation::focusable::focus::<()>(search.clone()),
            );
            let mut messages = Vec::new();
            ui.update(
                &[iced::Event::A11y(
                    search.clone(),
                    ActionRequest {
                        action: AxAction::SetValue,
                        target_tree: TreeId::ROOT,
                        target_node: iced_accessibility::A11yId::from(search.clone()).into(),
                        data: Some(ActionData::Value(query.into())),
                    },
                )],
                iced::mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert!(
                matches!(messages.as_slice(), [Message::ChatUi(Action::ComposerActionsQuery(owner, value))] if *owner == target && value == query)
            );
            let cache = ui.into_cache();
            let Message::ChatUi(query_action) = messages.pop().unwrap() else {
                unreachable!()
            };
            drop(app.handle_chat_ui(query_action));
            let mut ui = UserInterface::build(app.chat_view(card), viewport, cache, &mut renderer);
            ui.operate(&renderer, &mut operation::focusable::focus::<()>(search));
            let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
            let enabled: Vec<_> = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .filter(|node| node.node().role() == Role::Button && !node.node().is_disabled())
                .collect();
            // Scope to real menu rows; the custom popup exports the underlying
            // pane too, so unrelated composer/footer controls are not matches.
            let labels: Vec<_> = enabled
                .iter()
                .filter_map(|node| node.node().label())
                .collect();
            let pointer = expected.map(|(label, _)| {
                let bounds = enabled
                    .iter()
                    .find(|node| node.node().label() == Some(label))
                    .unwrap_or_else(|| panic!("visible enabled menu action {label}: {labels:?}"))
                    .node()
                    .bounds()
                    .unwrap();
                assert!(bounds.x1 > bounds.x0 && bounds.y1 > bounds.y0);
                assert!(bounds.x0 >= 0.0 && bounds.x1 <= f64::from(viewport.width));
                assert!(bounds.y0 >= 0.0 && bounds.y1 <= f64::from(viewport.height));
                iced::Point::new(
                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                )
            });
            if query == "images" || query == "attach" {
                assert!(!labels.contains(&"Fork conversation…"));
                assert!(!labels.contains(
                    &"Compact the conversation: summarize earlier turns to free context"
                ));
            }
            let mut entered = Vec::new();
            ui.update(
                &[iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                })],
                iced::mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut entered,
            );
            let (entered, mut cleanup) = observed(entered, target, "search Enter");
            assert_eq!(
                entered,
                expected.map(|(_, kind)| kind),
                "{provider:?} query={query:?} pending={pending} busy={busy} read_only={read_only} disconnected={disconnected}"
            );
            if let Some(pointer) = pointer {
                // Feed the same complete gesture one event at a time without
                // rebuilding its native tree. Unfocus schedules one cleanup;
                // an outside press can separately emit one more native blur.
                for (phase, event, expected_action) in [
                    (
                        "pointer move",
                        iced::mouse::Event::CursorMoved { position: pointer },
                        None,
                    ),
                    (
                        "pointer down",
                        iced::mouse::Event::ButtonPressed(iced::mouse::Button::Left),
                        None,
                    ),
                    (
                        "pointer release",
                        iced::mouse::Event::ButtonReleased(iced::mouse::Button::Left),
                        expected.map(|(_, kind)| kind),
                    ),
                ] {
                    let mut clicked = Vec::new();
                    ui.update(
                        &[iced::Event::Mouse(event)],
                        iced::mouse::Cursor::Available(pointer),
                        &mut renderer,
                        &mut clipboard::Null,
                        &mut clicked,
                    );
                    let (invoked, blurred) = observed(clicked, target, phase);
                    assert_eq!(
                        invoked, expected_action,
                        "{phase}: pointer and search Enter dispatch the same existing action"
                    );
                    cleanup.extend(blurred);
                }
            }
            drop(ui);
            for blur in cleanup {
                let task = app.handle_chat_ui(blur);
                assert_eq!(task.units(), 0, "native blur schedules no effects");
                drop(task);
            }
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                unreachable!()
            };
            assert_eq!(
                (
                    runtime.composer.text(),
                    runtime.composer.cursor(),
                    runtime.attachments.clone(),
                    runtime.next_command,
                    runtime.attachment_chooser_pending,
                    runtime.revision,
                ),
                before
            );
            assert!(runtime.pending.is_none() && runtime.queued.is_none());
            // Deliberately do not dispatch Attach: doing so would request an
            // actual platform chooser. No menu/provider task is polled here.
        }
    }
}
