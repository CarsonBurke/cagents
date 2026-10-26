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
    drop(app.handle_chat_ui(Action::Dictation(target, dictation::Action::Start)));
    assert!(app.chat_ui.dictation_setup.shown(target));
    assert!(
        app.chat_ui.dictation_setup.phase().is_none(),
        "opening setup never begins download"
    );
    assert!(app.chat_ui.dictation_setup.prepared().is_none());
    assert_eq!(chat_runtime(&app, card).composer.text(), "keep this draft");
    assert!(chat_runtime(&app, card).dictation.phase().is_none());
    assert!(chat_runtime(&app, card).pending.is_none());
    drop(app.handle_chat_ui(Action::Dictation(target, dictation::Action::DismissSetup)));
    assert!(!app.chat_ui.dictation_setup.shown(target));
}

#[test]
fn queued_start_and_install_cannot_reopen_hidden_or_parked_microphone_owner() {
    for parked in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
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
        drop(app.handle_chat_ui(Action::Dictation(target, dictation::Action::Start)));
        assert!(
            !app.chat_ui.dictation_setup.shown(target),
            "hidden queued Start must not even expose setup"
        );
        // A previously open setup panel can outlive its editor's visibility.
        app.chat_ui.dictation_setup.show(target);
        let task = app.handle_chat_ui(Action::Dictation(target, dictation::Action::Install));
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
