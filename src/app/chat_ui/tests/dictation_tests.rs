//! Synthetic lifecycle/editor regressions. No Prepared assets, model download,
//! physical microphone, provider or daemon process is created by these tests.
use super::*;
use crate::app::chat_ui::dictation;

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
    for control in ["chat-dictation-stop", "chat-dictation-cancel"] {
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
                &mut operation::focusable::focus::<()>(card_control_id(card, control)),
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
    write(&mut app, card, "base");
    let target = chat_runtime(&app, card).target;
    let queued_start = start(&app, target);
    drop(app.update(Message::Palette(crate::app::palette_ui::Action::Open)));
    drop(app.update(Message::ChatUi(queued_start)));
    assert!(!app.chat_ui.dictation_setup.shown(target));
    app.chat_ui.dictation_setup.show(target);
    let queued_install = install(&app, target);
    let task = app.update(Message::ChatUi(queued_install));
    assert!(iced::runtime::task::into_stream(task).is_none());
    assert!(app.chat_ui.dictation_setup.phase().is_none());
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
