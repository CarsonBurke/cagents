//! Synthetic lifecycle/editor regressions. No Prepared assets, model download,
//! physical microphone, provider or daemon process is created by these tests.
use super::*;
use crate::app::chat_ui::dictation;

fn expose(app: &mut Cagents, card: CardId) {
    // A newly created pane has no visible editor until the native viewport
    // publishes its geometry. Exercise the same admission path as the UI.
    drop(app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::Resized(620.0),
    }));
    assert_eq!(
        app.workspace.visibility(card),
        Some(VisibilityClass::Visible)
    );
    assert!(chat_runtime(app, card).visible.load(Ordering::Relaxed));
}

fn assert_no_effects(task: Task<Message>) {
    use iced::futures::{FutureExt, StreamExt};
    // Public dispatch batches its postlude even when every component is empty.
    // An allocated stream is not an emitted effect.
    assert_eq!(task.units(), 0, "refused input must schedule no task units");
    if let Some(mut effects) = iced::runtime::task::into_stream(task) {
        assert!(matches!(effects.next().now_or_never(), Some(None)));
    }
}

fn capture(app: &mut Cagents, target: Target) -> crate::speech::Ticket<Target> {
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime
        .dictation
        .capture_for_test(target, runtime.revision, &runtime.composer)
}
fn start(app: &Cagents, target: Target) -> Action {
    Action::Dictation(
        target,
        dictation::Action::Start {
            epoch: chat_runtime(app, target.card).dictation.epoch(),
            prepared: false,
        },
    )
}
fn install(app: &Cagents, target: Target) -> Action {
    Action::Dictation(
        target,
        dictation::Action::Install(app.chat_ui.dictation_setup.presentation()),
    )
}
fn dismiss(app: &Cagents, target: Target) -> Action {
    Action::Dictation(
        target,
        dictation::Action::DismissSetup(app.chat_ui.dictation_setup.presentation()),
    )
}
fn result(ticket: &crate::speech::Ticket<Target>, text: &str) -> Action {
    Action::Dictation(
        ticket.target,
        dictation::Action::Finished(dictation::Delivery::new(
            ticket.clone(),
            Ok(crate::speech::Transcript {
                ticket: ticket.clone(),
                text: text.into(),
            }),
        )),
    )
}

#[test]
fn microphone_discovery_requires_explicit_setup_and_preserves_draft() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    expose(&mut app, card);
    write(&mut app, card, "keep this draft");
    let target = chat_runtime(&app, card).target;
    drop(app.handle_chat_ui(start(&app, target)));
    assert!(app.chat_ui.dictation_setup.shown(target));
    assert!(
        app.chat_ui.dictation_setup.phase().is_none(),
        "opening setup never begins download"
    );
    assert!(app.chat_ui.dictation_setup.prepared().is_none());
    assert_eq!(chat_runtime(&app, card).composer.text(), "keep this draft");
    assert!(chat_runtime(&app, card).dictation.phase().is_none());
    assert!(chat_runtime(&app, card).pending.is_none());
    drop(app.handle_chat_ui(dismiss(&app, target)));
    assert!(!app.chat_ui.dictation_setup.shown(target));
}

#[test]
fn queued_start_and_install_cannot_reopen_hidden_or_parked_microphone_owner() {
    for parked in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        let queued_start = start(&app, target);
        app.chat_runtime_mut(target)
            .unwrap()
            .set_visible(false)
            .unwrap();
        if parked {
            let Some(CardRuntime::Chat(runtime)) = app.cards.remove(&card) else {
                panic!("chat runtime");
            };
            app.chat_ui.parked.insert(card, runtime);
        }
        drop(app.handle_chat_ui(queued_start));
        assert!(
            !app.chat_ui.dictation_setup.shown(target),
            "hidden queued Start must not even expose setup"
        );
        // A previously open setup panel can outlive its editor's visibility.
        app.chat_ui.dictation_setup.show(target);
        let task = app.handle_chat_ui(install(&app, target));
        assert!(iced::runtime::task::into_stream(task).is_none());
        assert!(app.chat_ui.dictation_setup.phase().is_none());
        assert!(app.chat_ui.dictation_setup.prepared().is_none());
    }
}

#[test]
fn dictation_completion_maps_real_edits_and_pending_image_without_sending() {
    let _admission = attachment_draft::test_admission();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    write(&mut app, card, "before π\nselected\nafter");
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    attachment_draft::move_to(
        &mut runtime.composer,
        text_editor::Cursor {
            position: text_editor::Position { line: 1, column: 3 },
            selection: None,
        },
    );
    let ticket = capture(&mut app, target);
    let runtime = app.chat_runtime_mut(target).unwrap();
    attachment_draft::move_to(
        &mut runtime.composer,
        text_editor::Cursor {
            position: text_editor::Position { line: 1, column: 8 },
            selection: None,
        },
    );
    let pending_image = runtime.try_attachment_insert().unwrap();
    let attachment = Attachment::file("/tmp/dictation-reference.txt");
    runtime.merge_attachments([attachment.clone()]);
    attachment_draft::move_to(
        &mut runtime.composer,
        text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 0 },
            selection: None,
        },
    );
    write(&mut app, card, "🚀\n");
    let current = chat_runtime(&app, card).composer.cursor();
    drop(app.handle_chat_ui(result(&ticket, "voice")));
    let runtime = chat_runtime(&app, card);
    assert_eq!(
        runtime.composer.text(),
        "🚀\nbefore π\nselvoiceected\nafter"
    );
    assert_eq!(runtime.composer.cursor(), current);
    assert_eq!(runtime.attachments, vec![attachment]);
    assert!(runtime.attachment_ticket(pending_image.id).is_some());
    assert_eq!(
        runtime.attachment_bookmarks[&pending_image.id]
            .cursor
            .position,
        text_editor::Position {
            line: 2,
            column: 13
        }
    );
    assert!(runtime.pending.is_none());
    assert!(runtime.queued.is_none());
    assert_eq!(runtime.next_command, 0);
}

#[test]
fn replaced_hidden_and_retargeted_drafts_retire_late_results() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    write(&mut app, card, "old draft");
    let target = chat_runtime(&app, card).target;
    let old = capture(&mut app, target);
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("replacement"));
    drop(app.handle_chat_ui(result(&old, "stale")));
    assert_eq!(chat_runtime(&app, card).composer.text(), "replacement");
    let hidden = capture(&mut app, target);
    app.chat_runtime_mut(target)
        .unwrap()
        .set_visible(false)
        .unwrap();
    drop(app.handle_chat_ui(result(&hidden, "stale")));
    assert!(chat_runtime(&app, card).dictation.phase().is_none());
    assert_eq!(chat_runtime(&app, card).composer.text(), "replacement");
    let old = capture(&mut app, target);
    let newer = Target {
        generation: target.generation + 1,
        ..target
    };
    app.chat_runtime_mut(target).unwrap().retarget(newer);
    drop(app.handle_chat_ui(result(&old, "stale")));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.dictation.phase().is_none());
    assert_eq!(runtime.composer.text(), "replacement");
}

#[test]
fn rejected_empty_send_keeps_capture_but_accepted_send_retires_it() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = live_chat(&mut app, Some("dictation-send"));
    let target = chat_runtime(&app, card).target;
    capture(&mut app, target);
    drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
    assert!(chat_runtime(&app, card).dictation.phase().is_some());
    write(&mut app, card, "send this");
    drop(app.handle_chat_ui(Action::Submit(target, PromptMode::Send)));
    assert!(chat_runtime(&app, card).dictation.phase().is_none());
    assert!(chat_runtime(&app, card).pending.is_some());
}

#[test]
fn focused_native_composer_escape_cancels_dictation_without_interrupting_busy_agent() {
    let renderer = crate::app::tests::headless_renderer();
    for width in [260.0, 620.0] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("dictation-escape"));
        let target = chat_runtime(&app, card).target;
        let ticket = capture(&mut app, target);
        let runtime = app.chat_runtime_mut(target).unwrap();
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        runtime.dictation.progress(
            target,
            &ticket,
            crate::speech::Phase::Recording,
            Instant::now(),
        );
        let before = runtime.next_command;
        composer_history_key(
            &mut app,
            card,
            width,
            keyboard::key::Named::Escape,
            &renderer,
        );
        let runtime = chat_runtime(&app, card);
        assert!(runtime.dictation.phase().is_none());
        assert!(runtime.snapshot.busy);
        assert_eq!(
            runtime.next_command, before,
            "Escape must not dispatch agent Interrupt"
        );
        assert!(runtime.pending.is_none());
    }
}

#[test]
fn dictation_control_native_escape_cancels_without_agent_interrupt() {
    use iced::advanced::{clipboard, widget::operation};
    use iced::runtime::{UserInterface, user_interface::Cache};
    let mut renderer = crate::app::tests::headless_renderer();
    for stop in [true, false] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("dictation-control-escape"));
        let target = chat_runtime(&app, card).target;
        let ticket = capture(&mut app, target);
        let runtime = app.chat_runtime_mut(target).unwrap();
        Arc::make_mut(&mut runtime.snapshot).busy = true;
        runtime.dictation.progress(
            target,
            &ticket,
            crate::speech::Phase::Recording,
            Instant::now(),
        );
        let before = runtime.next_command;
        let messages = {
            let mut ui = UserInterface::build(
                app.chat_view(card),
                iced::Size::new(620.0, 720.0),
                Cache::new(),
                &mut renderer,
            );
            ui.operate(
                &renderer,
                &mut operation::focusable::focus::<()>(if stop {
                    dictation::Action::StopRecording(ticket.clone()).control_id(target)
                } else {
                    dictation::Action::Cancel(ticket.clone()).control_id(target)
                }),
            );
            let mut messages = Vec::new();
            ui.update(
                &[iced::Event::Keyboard(permission_key(
                    keyboard::key::Named::Escape,
                    keyboard::Modifiers::empty(),
                ))],
                iced::mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            messages
        };
        assert!(messages.iter().any(|message| matches!(message,
            Message::ChatUi(Action::Dictation(actual, dictation::Action::Cancel(_))) if *actual == target)));
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, Message::ChatUi(Action::Interrupt(..))))
        );
        for message in messages {
            drop(app.update(message));
        }
        let runtime = chat_runtime(&app, card);
        assert!(runtime.dictation.phase().is_none());
        assert_eq!(runtime.next_command, before);
        assert!(runtime.snapshot.busy);
    }
}

#[test]
fn explicit_record_controls_do_not_return_focus_behind_modal_or_update_freeze() {
    for freeze in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = live_chat(&mut app, Some("dictation-control-modal"));
        let target = chat_runtime(&app, card).target;
        let ticket = capture(&mut app, target);
        app.chat_runtime_mut(target).unwrap().dictation.progress(
            target,
            &ticket,
            crate::speech::Phase::Recording,
            Instant::now(),
        );
        if freeze {
            app.update_prepared = true;
        } else {
            app.device_ui.open = true;
        }
        // Exercise the cleanup reducer directly as an already-admitted callback;
        // its focus guard must remain effective independently of modal routing.
        let stopped =
            app.handle_dictation(target, dictation::Action::StopRecording(ticket.clone()));
        assert!(iced::runtime::task::into_stream(stopped).is_none());
        let cancelled = app.handle_dictation(target, dictation::Action::Cancel(ticket.clone()));
        assert!(iced::runtime::task::into_stream(cancelled).is_none());
        assert!(chat_runtime(&app, card).dictation.phase().is_none());
    }
}

#[test]
fn stale_record_and_retained_controls_cannot_act_on_replacement_operation() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    write(&mut app, card, "base");
    let target = chat_runtime(&app, card).target;
    let first = capture(&mut app, target);
    drop(app.handle_dictation(target, dictation::Action::Cancel(first.clone())));
    let second = capture(&mut app, target);
    app.chat_runtime_mut(target).unwrap().dictation.progress(
        target,
        &second,
        crate::speech::Phase::Recording,
        Instant::now(),
    );
    for stale in [
        dictation::Action::StopRecording(first.clone()),
        dictation::Action::Cancel(first.clone()),
    ] {
        let task = app.handle_chat_ui(Action::Dictation(target, stale));
        assert!(
            iced::runtime::task::into_stream(task).is_none(),
            "stale controls cannot issue focus either"
        );
    }
    let runtime = chat_runtime(&app, card);
    assert!(runtime.dictation.ticket().unwrap().belongs_to(&second));
    assert!(!runtime.dictation.stopping());
    assert_eq!(
        runtime.dictation.phase(),
        Some(crate::speech::Phase::Recording)
    );
    app.chat_runtime_mut(target)
        .unwrap()
        .dictation
        .unmeasured_edit();
    drop(app.handle_chat_ui(result(&second, "first retained")));
    drop(app.handle_chat_ui(Action::Dictation(
        target,
        dictation::Action::DiscardRetained(second.clone()),
    )));
    let third = capture(&mut app, target);
    app.chat_runtime_mut(target)
        .unwrap()
        .dictation
        .unmeasured_edit();
    drop(app.handle_chat_ui(result(&third, "second retained")));
    for stale in [
        dictation::Action::InsertRetained(second.clone()),
        dictation::Action::DiscardRetained(second),
    ] {
        let task = app.handle_chat_ui(Action::Dictation(target, stale));
        assert!(iced::runtime::task::into_stream(task).is_none());
    }
    assert!(
        chat_runtime(&app, card)
            .dictation
            .retained_ticket()
            .unwrap()
            .belongs_to(&third)
    );
    assert_eq!(chat_runtime(&app, card).composer.text(), "base");
}

#[test]
fn stale_setup_cancel_and_presentation_cannot_replace_current_job() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    app.chat_ui.dictation_setup.show(target);
    let old_presentation = app.chat_ui.dictation_setup.presentation();
    let (first, _) = app.chat_ui.dictation_setup.begin().unwrap();
    app.chat_ui.dictation_setup.cancel(first);
    let (second, second_cancel) = app.chat_ui.dictation_setup.begin().unwrap();
    for stale in [
        dictation::Action::CancelSetup(first),
        dictation::Action::Install(old_presentation),
        dictation::Action::DismissSetup(old_presentation),
    ] {
        let task = app.handle_chat_ui(Action::Dictation(target, stale));
        assert!(iced::runtime::task::into_stream(task).is_none());
    }
    assert_eq!(app.chat_ui.dictation_setup.job_ticket(), Some(second));
    assert!(!second_cancel.is_cancelled());
    assert!(app.chat_ui.dictation_setup.shown(target));
    drop(app.handle_chat_ui(Action::Dictation(
        target,
        dictation::Action::CancelSetup(second),
    )));
    assert!(second_cancel.is_cancelled());
}

#[test]
fn palette_blocks_queued_dictation_start_setup_and_retained_insertion() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    expose(&mut app, card);
    write(&mut app, card, "base");
    let target = chat_runtime(&app, card).target;
    // Prove this exact visible editor can discover setup before the palette
    // takes input ownership; a hidden-editor guard must not mask this test.
    drop(app.handle_chat_ui(start(&app, target)));
    assert!(app.chat_ui.dictation_setup.shown(target));
    drop(app.handle_chat_ui(dismiss(&app, target)));
    let queued_start = start(&app, target);
    drop(app.update(Message::Palette(crate::app::palette_ui::Action::Open)));
    assert!(app.palette.is_open());
    assert!(app.workspace_inputs_blocked());
    drop(app.update(Message::ChatUi(queued_start)));
    assert!(!app.chat_ui.dictation_setup.shown(target));
    app.chat_ui.dictation_setup.show(target);
    let queued_install = install(&app, target);
    let task = app.update(Message::ChatUi(queued_install));
    assert!(app.chat_ui.dictation_setup.phase().is_none());
    assert!(app.chat_ui.dictation_setup.job_ticket().is_none());
    assert_no_effects(task);
    let ticket = capture(&mut app, target);
    app.chat_runtime_mut(target)
        .unwrap()
        .dictation
        .unmeasured_edit();
    drop(app.handle_chat_ui(result(&ticket, "retained")));
    drop(app.update(Message::ChatUi(Action::Dictation(
        target,
        dictation::Action::InsertRetained(ticket.clone()),
    ))));
    assert_eq!(chat_runtime(&app, card).composer.text(), "base");
    assert!(
        chat_runtime(&app, card)
            .dictation
            .retained_ticket()
            .unwrap()
            .belongs_to(&ticket)
    );
}

#[test]
fn replaced_draft_rejects_queued_idle_start_epoch() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    let queued = start(&app, target);
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("new draft"));
    drop(app.handle_chat_ui(queued));
    assert!(!app.chat_ui.dictation_setup.shown(target));
    assert_eq!(chat_runtime(&app, card).composer.text(), "new draft");
}

#[test]
fn native_microphone_click_preserves_selected_draft_and_pending_image_range() {
    use iced::advanced::{clipboard, widget::Operation};
    use iced::runtime::{UserInterface, user_interface::Cache};
    struct Bounds {
        ids: [widget::Id; 2],
        values: [Option<iced::Rectangle>; 2],
    }
    impl Operation for Bounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _: &mut dyn iced::advanced::widget::operation::Focusable,
        ) {
            for (wanted, saved) in self.ids.iter().zip(&mut self.values) {
                if id == Some(wanted) {
                    *saved = Some(bounds);
                }
            }
        }
    }
    let _admission = attachment_draft::test_admission();
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    expose(&mut app, card);
    write(&mut app, card, "alpha beta gamma");
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    attachment_draft::move_to(
        &mut runtime.composer,
        text_editor::Cursor {
            position: text_editor::Position {
                line: 0,
                column: 10,
            },
            selection: Some(text_editor::Position { line: 0, column: 6 }),
        },
    );
    let selected = runtime.composer.cursor();
    let revision = runtime.revision;
    let pending_image = runtime.try_attachment_insert().unwrap();
    let attachment = Attachment::file("/tmp/dictation-mic-reference.txt");
    runtime.merge_attachments([attachment.clone()]);
    let Action::Dictation(_, start_action) = start(&app, target) else {
        unreachable!()
    };
    let mut renderer = crate::app::tests::headless_renderer();
    let viewport = iced::Size::new(620.0, 720.0);
    let mut ui = UserInterface::build(app.chat_view(card), viewport, Cache::new(), &mut renderer);
    let mut bounds = Bounds {
        ids: [
            card_control_id(card, "chat-composer"),
            start_action.control_id(target),
        ],
        values: [None; 2],
    };
    ui.operate(&renderer, &mut bounds);
    let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
    let button = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|node| {
            node.node().role() == iced_accessibility::accesskit::Role::Button
                && node.node().label() == Some("Set up local dictation")
                && !node.node().is_disabled()
        })
        .unwrap()
        .node()
        .bounds()
        .unwrap();
    assert!(
        button.x0 >= 0.0
            && button.y0 >= 0.0
            && button.x1 <= f64::from(viewport.width)
            && button.y1 <= f64::from(viewport.height)
    );
    let pointer = iced::Point::new(
        ((button.x0 + button.x1) / 2.0) as f32,
        ((button.y0 + button.y1) / 2.0) as f32,
    );
    assert!(bounds.values[1].unwrap().contains(pointer));
    assert!(!bounds.values[0].unwrap().contains(pointer));
    ui.operate(
        &renderer,
        &mut iced::advanced::widget::operation::focusable::focus::<()>(card_control_id(
            card,
            "chat-composer",
        )),
    );
    let mut messages = Vec::new();
    ui.update(
        &[iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
            iced::mouse::Button::Left,
        ))],
        iced::mouse::Cursor::Available(pointer),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(
        matches!(messages.as_slice(), [Message::ChatUi(Action::Composer(owner, text_editor::Action::ClearSelection))] if *owner == target)
    );
    let cache = ui.into_cache();
    let Message::ChatUi(blur) = messages.pop().unwrap() else {
        unreachable!()
    };
    assert_no_effects(app.handle_chat_ui(blur));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.cursor(), selected);
    assert_eq!(runtime.composer.selection().as_deref(), Some("beta"));
    assert_eq!(runtime.composer.text(), "alpha beta gamma");
    assert_eq!(runtime.revision, revision);
    assert!(!app.chat_ui.dictation_setup.shown(target));
    assert!(runtime.dictation.phase().is_none());
    let mut ui = UserInterface::build(app.chat_view(card), viewport, cache, &mut renderer);
    ui.update(
        &[iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
            iced::mouse::Button::Left,
        ))],
        iced::mouse::Cursor::Available(pointer),
        &mut renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    assert!(
        matches!(messages.as_slice(), [Message::ChatUi(Action::Dictation(owner, dictation::Action::Start { epoch, prepared: false }))]
        if *owner == target && *epoch == chat_runtime(&app, card).dictation.epoch())
    );
    drop(ui);
    let Message::ChatUi(start) = messages.pop().unwrap() else {
        unreachable!()
    };
    drop(app.handle_chat_ui(start));
    assert!(app.chat_ui.dictation_setup.shown(target));
    assert!(app.chat_ui.dictation_setup.phase().is_none());
    assert!(app.chat_ui.dictation_setup.prepared().is_none());
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.cursor(), selected);
    assert_eq!(runtime.attachments, vec![attachment.clone()]);
    assert_eq!(
        runtime.attachment_bookmarks[&pending_image.id].cursor,
        selected
    );
    assert!(runtime.attachment_bookmarks[&pending_image.id].valid);
    assert!(runtime.attachment_ticket(pending_image.id).is_some());
    assert_eq!(runtime.revision, revision);
    assert_eq!(runtime.next_command, 0);
    assert!(runtime.pending.is_none());
    assert!(runtime.queued.is_none());
    // The selected range remains the actual capture insertion bookmark. This
    // synthetic completion creates no audio, model, provider or download work.
    drop(app.handle_chat_ui(dismiss(&app, target)));
    let ticket = capture(&mut app, target);
    drop(app.handle_chat_ui(result(&ticket, "voice")));
    let runtime = chat_runtime(&app, card);
    assert_eq!(runtime.composer.text(), "alpha voice gamma");
    assert_eq!(runtime.attachments, vec![attachment]);
    assert!(runtime.pending.is_none());
    assert!(runtime.queued.is_none());
    assert_eq!(runtime.next_command, 0);
}

#[test]
fn native_record_press_and_focus_do_not_cross_operations_without_intermediate_frame() {
    use iced::advanced::{
        clipboard,
        widget::{Operation, operation},
    };
    use iced::runtime::{UserInterface, user_interface::Cache};
    #[derive(Default)]
    struct Focus(Option<widget::Id>);
    impl Operation for Focus {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _: iced::Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if state.is_focused() {
                self.0 = id.cloned();
            }
        }
    }
    struct HitBounds {
        ids: [widget::Id; 3],
        bounds: [Option<iced::Rectangle>; 3],
    }
    impl Operation for HitBounds {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            _: &mut dyn operation::Focusable,
        ) {
            for (wanted, saved) in self.ids.iter().zip(&mut self.bounds) {
                if id == Some(wanted) {
                    *saved = Some(bounds);
                }
            }
        }
    }
    fn safe_message_kind(message: &Message) -> String {
        if let Message::ChatUi(Action::Composer(_, event)) = message {
            match event {
                text_editor::Action::Click(point) => format!("Composer::Click({point:?})"),
                text_editor::Action::Drag(point) => format!("Composer::Drag({point:?})"),
                text_editor::Action::Move(motion) => format!("Composer::Move({motion:?})"),
                text_editor::Action::Select(motion) => format!("Composer::Select({motion:?})"),
                text_editor::Action::SelectWord => "Composer::SelectWord".into(),
                text_editor::Action::SelectLine => "Composer::SelectLine".into(),
                text_editor::Action::SelectAll => "Composer::SelectAll".into(),
                text_editor::Action::ClearSelection => "Composer::ClearSelection".into(),
                text_editor::Action::Scroll { lines } => format!("Composer::Scroll({lines})"),
                text_editor::Action::Edit(_) => "Composer::Edit(payload omitted)".into(),
            }
        } else {
            format!("{message:?}")
        }
    }
    let mut renderer = crate::app::tests::headless_renderer();
    let viewport = iced::Size::new(620.0, 720.0);
    let _admission = attachment_draft::test_admission();
    for stop in [true, false] {
        for replace in [true, false] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let card = live_chat(&mut app, Some("dictation-held-press"));
            expose(&mut app, card);
            // Visibility requests a full publication. Deliver the fixture's
            // real client snapshot/wake before measuring native controls.
            let handle = chat_runtime(&app, card).handle().unwrap().clone();
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            handle.publish_snapshot_for_test(snapshot);
            let target = chat_runtime(&app, card).target;
            drop(app.handle_chat_ui(Action::Wake(target, Wake::Changed)));
            assert!(chat_runtime(&app, card).snapshot.ready);
            write(&mut app, card, "alpha beta gamma");
            let runtime = app.chat_runtime_mut(target).unwrap();
            attachment_draft::move_to(
                &mut runtime.composer,
                text_editor::Cursor {
                    position: text_editor::Position {
                        line: 0,
                        column: 10,
                    },
                    selection: Some(text_editor::Position { line: 0, column: 6 }),
                },
            );
            let pending_image = runtime.try_attachment_insert().unwrap();
            let attachment = Attachment::file("/tmp/dictation-held-reference.txt");
            runtime.merge_attachments([attachment.clone()]);
            let before = (
                runtime.composer.cursor(),
                runtime.attachment_bookmarks[&pending_image.id],
                runtime.revision,
                runtime.next_command,
            );
            assert_eq!(runtime.composer.selection().as_deref(), Some("beta"));
            let first = capture(&mut app, target);
            app.chat_runtime_mut(target).unwrap().dictation.progress(
                target,
                &first,
                crate::speech::Phase::Recording,
                Instant::now(),
            );
            let control = |ticket: crate::speech::Ticket<Target>| {
                if stop {
                    dictation::Action::StopRecording(ticket)
                } else {
                    dictation::Action::Cancel(ticket)
                }
            };
            let label = if stop {
                "Finish dictation recording and transcribe"
            } else {
                "Cancel dictation and discard recording (Esc)"
            };
            let (cache, pointer, blur) = {
                let mut ui = UserInterface::build(
                    app.chat_view(card),
                    viewport,
                    Cache::new(),
                    &mut renderer,
                );
                let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
                let mut hit = HitBounds {
                    ids: [
                        card_control_id(card, "chat-composer"),
                        dictation::Action::StopRecording(first.clone()).control_id(target),
                        dictation::Action::Cancel(first.clone()).control_id(target),
                    ],
                    bounds: [None; 3],
                };
                ui.operate(&renderer, &mut hit);
                assert!(hit.bounds.iter().all(Option::is_some));
                let bounds = nodes
                    .root()
                    .iter()
                    .chain(nodes.children())
                    .find(|node| {
                        node.node().role() == iced_accessibility::accesskit::Role::Button
                            && node.node().label() == Some(label)
                            && !node.node().is_disabled()
                    })
                    .unwrap()
                    .node()
                    .bounds()
                    .unwrap();
                assert!(
                    bounds.x0 >= 0.0
                        && bounds.y0 >= 0.0
                        && bounds.x1 <= f64::from(viewport.width)
                        && bounds.y1 <= f64::from(viewport.height),
                    "the actual enabled control must be inside the native viewport: {bounds:?}"
                );
                let pointer = iced::Point::new(
                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                );
                let actual = hit.bounds[if stop { 1 } else { 2 }].unwrap();
                assert!(
                    actual.contains(pointer),
                    "AX center must hit the actual control: {bounds:?} versus {actual:?}"
                );
                assert!(
                    !hit.bounds[0].unwrap().contains(pointer),
                    "native editor and dictation controls must not overlap"
                );
                ui.operate(
                    &renderer,
                    &mut operation::focusable::focus::<()>(card_control_id(card, "chat-composer")),
                );
                let mut focused = Focus::default();
                ui.operate(&renderer, &mut focused);
                assert_eq!(focused.0, Some(card_control_id(card, "chat-composer")));
                let mut messages = Vec::new();
                ui.update(
                    &[iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                        iced::mouse::Button::Left,
                    ))],
                    iced::mouse::Cursor::Available(pointer),
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                assert!(
                    matches!(messages.as_slice(), [Message::ChatUi(Action::Composer(owner, text_editor::Action::ClearSelection))] if *owner == target),
                    "mouseDown may only unfocus this editor; release owns dictation: stop={stop} replace={replace} AX={bounds:?} pointer={pointer:?} actual[editor,record,cancel]={:?}; emitted {:?}",
                    hit.bounds,
                    messages.iter().map(safe_message_kind).collect::<Vec<_>>(),
                );
                let blur = messages.pop().unwrap();
                let mut focused = Focus::default();
                ui.operate(&renderer, &mut focused);
                assert_ne!(focused.0, Some(card_control_id(card, "chat-composer")));
                let id = control(first.clone()).control_id(target);
                ui.operate(
                    &renderer,
                    &mut operation::focusable::focus::<()>(id.clone()),
                );
                let mut focused = Focus::default();
                ui.operate(&renderer, &mut focused);
                assert_eq!(focused.0, Some(id));
                (ui.into_cache(), pointer, blur)
            };
            let Message::ChatUi(blur) = blur else {
                unreachable!()
            };
            assert_no_effects(app.handle_chat_ui(blur));
            let runtime = chat_runtime(&app, card);
            assert_eq!(runtime.composer.text(), "alpha beta gamma");
            assert_eq!(runtime.composer.cursor(), before.0);
            assert_eq!(runtime.composer.selection().as_deref(), Some("beta"));
            assert_eq!(runtime.attachments, vec![attachment]);
            assert!(runtime.attachment_ticket(pending_image.id).is_some());
            let bookmark = runtime.attachment_bookmarks[&pending_image.id];
            assert_eq!(bookmark.cursor, before.1.cursor);
            assert_eq!(bookmark.valid, before.1.valid);
            assert_eq!(runtime.revision, before.2);
            assert_eq!(runtime.next_command, before.3);
            assert!(runtime.dictation.ticket().unwrap().belongs_to(&first));
            assert!(runtime.pending.is_none());
            assert!(runtime.queued.is_none());
            let current = if replace {
                drop(app.handle_dictation(target, dictation::Action::Cancel(first.clone())));
                let second = capture(&mut app, target);
                app.chat_runtime_mut(target).unwrap().dictation.progress(
                    target,
                    &second,
                    crate::speech::Phase::Recording,
                    Instant::now(),
                );
                second
            } else {
                first
            };
            let mut ui = UserInterface::build(app.chat_view(card), viewport, cache, &mut renderer);
            let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
            let bounds = nodes
                .root()
                .iter()
                .chain(nodes.children())
                .find(|node| {
                    node.node().role() == iced_accessibility::accesskit::Role::Button
                        && node.node().label() == Some(label)
                })
                .unwrap()
                .node()
                .bounds()
                .unwrap();
            assert!(
                f64::from(pointer.x) >= bounds.x0
                    && f64::from(pointer.x) <= bounds.x1
                    && f64::from(pointer.y) >= bounds.y0
                    && f64::from(pointer.y) <= bounds.y1,
                "same actual control geometry is required; a moved target cannot explain rejection"
            );
            let mut focused = Focus::default();
            ui.operate(&renderer, &mut focused);
            assert_eq!(
                focused.0,
                (!replace).then(|| control(current.clone()).control_id(target))
            );
            let mut messages = Vec::new();
            ui.update(
                &[iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                    iced::mouse::Button::Left,
                ))],
                iced::mouse::Cursor::Available(pointer),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            let invoked = messages
                .iter()
                .filter_map(|message| match message {
                    Message::ChatUi(Action::Dictation(
                        _,
                        dictation::Action::StopRecording(ticket)
                        | dictation::Action::Cancel(ticket),
                    )) => Some(ticket),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if replace {
                assert!(invoked.is_empty(), "A's held native press cannot invoke B");
            } else {
                assert_eq!(
                    invoked,
                    vec![&current],
                    "same-operation held press remains usable"
                );
            }
        }
    }
}
