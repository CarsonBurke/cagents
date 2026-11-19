//! A draft's `[Image N]` references and its attachments stay one object
//! through the composer's real edit, undo, paste and tray paths (#3259).
use super::tests::{chat_runtime, composer_key_messages, draft_chat, write};
use super::*;

/// A draft chat holding `images` pasted at the end of `prefix`, through the
/// asynchronous insertion path a clipboard paste takes.
fn draft_with_images(app: &mut Cagents, prefix: &str, images: &[Attachment]) -> Target {
    let card = draft_chat(app);
    let target = chat_runtime(app, card).target;
    write(app, card, prefix);
    let runtime = app.chat_runtime_mut(target).unwrap();
    for image in images {
        let ticket = runtime.begin_attachment_insert();
        assert!(runtime.finish_attachment_insert(ticket.id, "", vec![image.clone()]));
    }
    target
}

fn token(image: &Attachment) -> String {
    image.reference.as_ref().unwrap().token()
}

fn numbers(attachments: &[Attachment]) -> Vec<u64> {
    attachments
        .iter()
        .map(|attachment| attachment.reference.as_ref().unwrap().number)
        .collect()
}

/// Places the caret just after the first occurrence of `text` in the draft.
fn caret_after(runtime: &mut Runtime, text: &str) {
    let draft = runtime.composer.text();
    let at = draft.find(text).unwrap() + text.len();
    let line = draft[..at].matches('\n').count();
    let column = at - draft[..at].rfind('\n').map_or(0, |newline| newline + 1);
    runtime.composer_undo.seal();
    attachment_draft::move_to(
        &mut runtime.composer,
        text_editor::Cursor {
            position: text_editor::Position { line, column },
            selection: None,
        },
    );
}

fn edit(runtime: &mut Runtime, edit: text_editor::Edit) {
    runtime.composer_action(text_editor::Action::Edit(edit));
}

fn sent_attachments(runtime: &Runtime) -> Vec<Attachment> {
    let Command::Prompt { attachments, .. } = submission(
        &runtime.composer.text(),
        &runtime.snapshot,
        &runtime.attachments,
        PromptMode::Send,
    )
    .unwrap() else {
        panic!("a prompt");
    };
    attachments
}

#[test]
fn deleting_a_reference_removes_its_image_and_undo_brings_both_back() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("second.png");
        let third = chat::attachment_assets::test_image("third.png");
        let all = [first.clone(), second.clone(), third.clone()];
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "Compare ", &all);
        let runtime = app.chat_runtime_mut(target).unwrap();
        let full = runtime.composer.text();
        assert_eq!(numbers(&runtime.attachments), numbers(&all));

        // One Backspace breaks the reference; the image goes with it, and
        // the others keep their numbers.
        caret_after(runtime, &token(&second));
        edit(runtime, text_editor::Edit::Backspace);
        assert_eq!(runtime.attachments, [first.clone(), third.clone()]);
        assert_eq!(sent_attachments(runtime), [first.clone(), third.clone()]);
        // The rest of the reference goes in the same run of deletes.
        let reference = token(&second);
        for _ in 1..reference.len() {
            edit(runtime, text_editor::Edit::Backspace);
        }
        assert_eq!(
            runtime.composer.text(),
            full.replacen(&reference, "", 1),
            "the whole reference is gone"
        );
        assert_eq!(runtime.attachments, [first.clone(), third.clone()]);

        // Undo restores the text and the same receipt, in its place.
        drop(app.handle_chat_ui(Action::ComposerUndo(target, false)));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(runtime.composer.text(), full);
        assert_eq!(runtime.attachments, all);
        assert_eq!(sent_attachments(runtime), all);

        // Redo takes both out again.
        drop(app.handle_chat_ui(Action::ComposerUndo(target, true)));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(!runtime.composer.text().contains(&token(&second)));
        assert_eq!(runtime.attachments, [first.clone(), third.clone()]);
    });
}

#[test]
fn removing_a_tile_removes_every_reference_to_it_and_undo_restores_both() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("second.png");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "Look ", &[first.clone(), second.clone()]);
        let runtime = app.chat_runtime_mut(target).unwrap();
        // The reader mentions the first image twice.
        let end = runtime.composer.text().len();
        attachment_draft::move_to(
            &mut runtime.composer,
            text_editor::Cursor {
                position: text_editor::Position {
                    line: 0,
                    column: end,
                },
                selection: None,
            },
        );
        edit(
            runtime,
            text_editor::Edit::Paste(Arc::new(format!("then {} again", token(&first)))),
        );
        let full = runtime.composer.text();

        drop(app.handle_chat_ui(Action::RemoveAttachment(target, 0)));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(!runtime.composer.text().contains(&token(&first)));
        assert!(runtime.composer.text().contains(&token(&second)));
        assert_eq!(runtime.attachments, [second.clone()]);

        // One undo puts back every reference the removal took, and the tile.
        drop(app.handle_chat_ui(Action::ComposerUndo(target, false)));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(runtime.composer.text(), full);
        assert_eq!(runtime.attachments, [first.clone(), second.clone()]);
    });
}

#[test]
fn a_file_tile_goes_without_touching_the_text() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    write(&mut app, card, "Read this");
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.attach([PathBuf::from("/project/notes.pdf")]);
    drop(app.handle_chat_ui(Action::RemoveAttachment(target, 0)));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.attachments.is_empty());
    assert_eq!(runtime.composer.text(), "Read this");
}

#[test]
fn undoing_an_image_paste_takes_out_its_reference_and_its_image() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "See ", &[image.clone()]);
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(runtime.composer.text().contains(&token(&image)));
        assert!(runtime.undo_composer(false));
        assert_eq!(runtime.composer.text(), "See ");
        assert!(runtime.attachments.is_empty());
        assert!(runtime.undo_composer(true));
        assert!(runtime.composer.text().contains(&token(&image)));
        assert_eq!(runtime.attachments, [image.clone()]);
        // Typing undoes a word at a time, and the image stays until its own
        // step comes up.
        caret_after(runtime, &token(&image));
        for character in " and here".chars() {
            edit(runtime, text_editor::Edit::Insert(character));
        }
        assert!(runtime.undo_composer(false));
        assert!(runtime.undo_composer(false));
        assert_eq!(runtime.attachments, [image.clone()]);
    });
}

#[test]
fn a_reference_cut_and_pasted_elsewhere_keeps_its_image() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "First ", &[image.clone()]);
        let runtime = app.chat_runtime_mut(target).unwrap();
        let reference = token(&image);
        let draft = runtime.composer.text();
        let at = draft.find(&reference).unwrap();
        attachment_draft::move_to(
            &mut runtime.composer,
            text_editor::Cursor {
                position: text_editor::Position {
                    line: 0,
                    column: at + reference.len(),
                },
                selection: Some(text_editor::Position {
                    line: 0,
                    column: at,
                }),
            },
        );
        edit(runtime, text_editor::Edit::Paste(Arc::new(String::new())));
        assert!(runtime.attachments.is_empty());
        attachment_draft::move_to(
            &mut runtime.composer,
            text_editor::Cursor {
                position: text_editor::Position { line: 0, column: 0 },
                selection: None,
            },
        );
        edit(
            runtime,
            text_editor::Edit::Paste(Arc::new(reference.clone())),
        );
        assert!(runtime.composer.text().starts_with(&reference));
        assert_eq!(runtime.attachments, [image.clone()]);
    });
}

#[test]
fn take_back_and_history_walks_keep_the_mapping() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|queued| {
        let draft = chat::attachment_assets::test_image("draft.png");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "Also ", &[draft.clone()]);
        let runtime = app.chat_runtime_mut(target).unwrap();
        let unsent = runtime.composer.text();
        runtime.remember_with_attachments(format!("Sent {}", token(&queued)), vec![queued.clone()]);
        // A walk through sent messages and back keeps the draft's undo.
        runtime.recall(true);
        assert_eq!(runtime.attachments, [queued.clone()]);
        runtime.recall(false);
        assert_eq!(runtime.composer.text(), unsent);
        assert!(runtime.undo_composer(false));
        assert!(runtime.attachments.is_empty(), "undo took back the paste");
        assert!(runtime.undo_composer(true));
        assert_eq!(runtime.attachments, [draft.clone()]);

        // A queued message taken back joins the draft with its image; its
        // reference still decides whether the image goes.
        let queued_text = format!("Queued {}", token(&queued));
        runtime.take_back(&queued_text, vec![queued.clone()], PromptMode::Queue);
        assert_eq!(runtime.attachments, [queued.clone(), draft.clone()]);
        caret_after(runtime, &token(&queued));
        edit(runtime, text_editor::Edit::Backspace);
        assert_eq!(runtime.attachments, [draft.clone()]);
        assert!(runtime.undo_composer(false));
        assert_eq!(runtime.attachments, [queued.clone(), draft.clone()]);
        assert_eq!(sent_attachments(runtime), [queued.clone(), draft.clone()]);
        assert_eq!(
            numbers(&sent_attachments(runtime)),
            numbers(&[queued, draft])
        );
    });
}

#[test]
fn an_image_only_message_brought_back_keeps_its_image_while_text_is_typed() {
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat(&mut app);
        let target = chat_runtime(&app, card).target;
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.take_back("", vec![image.clone()], PromptMode::Queue);
        edit(runtime, text_editor::Edit::Insert('x'));
        assert_eq!(
            runtime.attachments,
            [image.clone()],
            "no edit removed a reference it never had"
        );
    });
}

#[test]
fn the_caret_on_a_reference_marks_its_tile() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("second.png");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "A ", &[first.clone(), second.clone()]);
        let runtime = app.chat_runtime_mut(target).unwrap();
        caret_after(runtime, &token(&first));
        runtime.follow_caret_reference();
        assert_eq!(
            runtime.caret_reference,
            first.reference.as_ref().map(|reference| reference.id)
        );
        attachment_draft::move_to(
            &mut runtime.composer,
            text_editor::Cursor {
                position: text_editor::Position { line: 0, column: 0 },
                selection: None,
            },
        );
        runtime.follow_caret_reference();
        assert_eq!(runtime.caret_reference, None);
    });
}

#[test]
fn the_focused_composer_takes_undo_and_redo_chords() {
    use iced::keyboard::{self, key::Code, key::Physical};
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    write(&mut app, card, "draft");
    let renderer = crate::app::tests::headless_renderer();
    let chord = |character: &str, code, modifiers| keyboard::Event::KeyPressed {
        key: keyboard::Key::Character(character.into()),
        modified_key: keyboard::Key::Character(character.into()),
        physical_key: Physical::Code(code),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat: false,
    };
    let control = keyboard::Modifiers::CTRL;
    for (event, redo) in [
        (chord("z", Code::KeyZ, control), false),
        (
            chord("z", Code::KeyZ, control | keyboard::Modifiers::SHIFT),
            true,
        ),
        (chord("y", Code::KeyY, control), true),
    ] {
        let messages = composer_key_messages(&app, card, 720.0, event, &renderer);
        assert!(
            messages.iter().any(|message| matches!(
                message,
                Message::ChatUi(Action::ComposerUndo(found, found_redo))
                    if *found == target && *found_redo == redo
            )),
            "redo: {redo}"
        );
    }
    let messages = composer_key_messages(
        &app,
        card,
        720.0,
        chord("z", Code::KeyZ, control),
        &renderer,
    );
    for message in messages {
        drop(app.update(message));
    }
    assert_eq!(chat_runtime(&app, card).composer.text(), "");
}
