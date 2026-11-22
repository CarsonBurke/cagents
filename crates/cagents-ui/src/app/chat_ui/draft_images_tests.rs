//! A draft's `[Image N]` references and its attachments stay one object
//! through the composer's real edit, undo, paste and tray paths (#3259).
use super::tests::{
    chat_runtime, composer_key_messages, draft_chat, draft_chat_for_provider, task_focuses, write,
};
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

        // One Backspace takes the whole reference, and its image with it;
        // the others keep their numbers.
        caret_after(runtime, &token(&second));
        edit(runtime, text_editor::Edit::Backspace);
        let reference = token(&second);
        assert_eq!(
            runtime.composer.text(),
            full.replacen(&reference, "", 1),
            "the whole reference is gone"
        );
        assert_eq!(runtime.attachments, [first.clone(), third.clone()]);
        assert_eq!(sent_attachments(runtime), [first.clone(), third.clone()]);

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

        drop(app.handle_chat_ui(Action::RemoveAttachment(target, 0, Removal::Key)));
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
fn a_key_keeps_the_keyboard_in_the_tray_and_the_x_returns_it_to_the_message() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("second.png");
        let third = chat::attachment_assets::test_image("third.png");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "Look ", &[first, second, third]);
        let composer = card_control_id(target.card, "chat-composer");

        // Delete on the first tile moves the keyboard to the one that took
        // its place, so the next Delete removes that.
        let task = app.handle_chat_ui(Action::RemoveAttachment(target, 0, Removal::Key));
        let tile = view::attachment_tile_id(target.card, 0);
        assert_eq!(task_focuses(task, &tile), (true, false));

        // A press of the × leaves the tray to the pointer: the next key
        // edits the message, and Ctrl+Z there takes the removal back.
        let task = app.handle_chat_ui(Action::RemoveAttachment(target, 0, Removal::Pointer));
        assert_eq!(task_focuses(task, &composer), (true, false));
        assert_eq!(app.chat_runtime_mut(target).unwrap().attachments.len(), 1);

        // Emptying the tray by key returns the keyboard to the message too.
        let task = app.handle_chat_ui(Action::RemoveAttachment(target, 0, Removal::Key));
        assert_eq!(task_focuses(task, &composer), (true, false));
    });
}

#[test]
fn undoing_a_removal_at_the_attachment_limit_brings_back_the_image_it_removed() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|first| {
        let limit = chat::AttachmentRules::for_provider(Provider::Claude)
            .max_count
            .unwrap();
        let mut images = vec![first];
        images
            .extend((2..=limit).map(|n| chat::attachment_assets::test_image(&format!("{n}.png"))));
        let waiting = chat::attachment_assets::test_image("waiting.png");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let card = draft_chat_for_provider(&mut app, Provider::Claude);
        let target = chat_runtime(&app, card).target;
        write(&mut app, card, "Look ");
        let runtime = app.chat_runtime_mut(target).unwrap();
        let insert = |runtime: &mut Runtime, image: &Attachment| {
            let ticket = runtime.begin_attachment_insert();
            assert!(runtime.finish_attachment_insert(ticket.id, "", vec![image.clone()]));
        };
        insert(runtime, &waiting);
        for image in &images[..limit - 1] {
            insert(runtime, image);
        }
        // The reader deletes one reference, and another image fills the
        // chat to its limit.
        caret_after(runtime, &token(&waiting));
        edit(runtime, text_editor::Edit::Backspace);
        let draft = runtime.composer.text();
        caret_after(runtime, &draft);
        insert(runtime, &images[limit - 1]);
        assert_eq!(runtime.attachments, images);

        // Pasting the deleted reference back leaves it text, and says why.
        let draft = runtime.composer.text();
        caret_after(runtime, &draft);
        edit(runtime, text_editor::Edit::Paste(Arc::new(token(&waiting))));
        assert_eq!(runtime.attachments, images);
        let note = runtime.composer_note.clone().unwrap();
        assert!(note.contains(&waiting.label()), "{note}");

        // Removing a tile doesn't let the refused image in, so undoing the
        // removal brings back exactly the image it removed.
        drop(app.handle_chat_ui(Action::RemoveAttachment(target, 0, Removal::Pointer)));
        assert_eq!(chat_runtime(&app, card).attachments, images[1..]);
        drop(app.handle_chat_ui(Action::ComposerUndo(target, false)));
        assert_eq!(chat_runtime(&app, card).attachments, images);
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
    drop(app.handle_chat_ui(Action::RemoveAttachment(target, 0, Removal::Key)));
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

#[test]
fn undo_keeps_the_line_endings_each_step_recorded() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let card = draft_chat(&mut app);
    let target = chat_runtime(&app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    edit(
        runtime,
        text_editor::Edit::Paste(Arc::new("\r\nbody".into())),
    );
    let pasted = runtime.composer.text();
    runtime.composer_undo.seal();
    attachment_draft::move_to(
        &mut runtime.composer,
        text_editor::Cursor {
            position: text_editor::Position { line: 0, column: 0 },
            selection: None,
        },
    );
    edit(runtime, text_editor::Edit::Enter);
    runtime.composer_action(text_editor::Action::SelectAll);
    edit(runtime, text_editor::Edit::Insert('x'));
    assert_eq!(runtime.composer.text(), "x");

    // Undoing the replacement puts back a newline the person typed beside
    // a pasted CR LF, and undoing that takes out only the typed one.
    for _ in 0..2 {
        drop(app.handle_chat_ui(Action::ComposerUndo(target, false)));
    }
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert_eq!(runtime.composer.text(), pasted);
}

#[test]
fn only_the_image_opened_last_shows_when_loads_finish_out_of_order() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("second.png");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "Compare ", &[first.clone(), second.clone()]);
        drop(app.handle_chat_ui(Action::OpenAttachment(target, 0)));
        drop(app.handle_chat_ui(Action::OpenAttachment(target, 1)));
        let failed = || images::Loaded::Failed("could not read it".into());
        drop(app.handle_chat_ui(Action::AttachmentOpened(target, first.clone(), failed())));
        assert_eq!(
            app.chat_runtime_mut(target).unwrap().error,
            None,
            "the earlier load is ignored"
        );
        drop(app.handle_chat_ui(Action::AttachmentOpened(target, second.clone(), failed())));
        assert_eq!(
            app.chat_runtime_mut(target).unwrap().error.as_deref(),
            Some("could not read it")
        );
    });
}

#[test]
fn a_name_put_in_from_the_menu_is_an_edit_like_any_other() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "See ", std::slice::from_ref(&image));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let full = runtime.composer.text();
        // The caret inside "[Image N]": the name splits the reference, and
        // its image leaves with it.
        caret_after(runtime, "[Image");
        runtime.follow_caret_reference();
        assert!(runtime.caret_reference.is_some(), "the tile is marked");
        runtime.name_in_draft("notes.md", completion::Kind::File);
        assert!(runtime.composer.text().contains("notes.md"));
        assert!(runtime.attachments.is_empty());
        assert_eq!(runtime.caret_reference, None, "no tile stays marked");
        assert!(sent_attachments(runtime).is_empty());
        // One undo takes the name out and brings the image back.
        assert!(runtime.undo_composer(false));
        assert_eq!(runtime.composer.text(), full);
        assert_eq!(runtime.attachments, [image]);
    });
}

#[test]
fn a_draft_image_still_loading_stays_closed_once_another_image_opens() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "See ", std::slice::from_ref(&image));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let content = ItemContent::Image {
            path: "/fixture/image.png".into(),
            description: "Generated image".into(),
            reference: None,
        };
        let mut cached = CachedItem::new(&content, runtime.syntax_theme);
        cached.image = Some(iced::widget::image::Handle::from_rgba(
            1,
            1,
            vec![0, 0, 0, 255],
        ));
        runtime.cache.insert("image".into(), cached);
        Arc::make_mut(&mut runtime.snapshot).items = vec![
            chat::Item {
                id: "image".into(),
                role: chat::Role::Assistant,
                content,
                started_at: None,
                at: None,
            }
            .into(),
        ];
        // The tile is asked for, then a transcript image is opened and
        // closed before the tile's image has loaded.
        drop(app.update(Message::ChatUi(Action::OpenAttachment(target, 0))));
        drop(app.update(Message::ChatUi(Action::OpenImage(target, "image".into()))));
        let pane = images::ViewerHost::Pane(target);
        assert!(app.image_viewer(pane).is_some());
        drop(app.update(Message::ChatUi(Action::CloseImage(pane))));
        let loaded = images::attachment_preview(&image);
        assert!(matches!(loaded, images::Loaded::Ready(_)));
        drop(app.update(Message::ChatUi(Action::AttachmentOpened(
            target, image, loaded,
        ))));
        assert!(app.image_viewer(pane).is_none());
    });
}

#[test]
fn a_tile_opens_in_its_own_pane_beside_another_panes_image() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("second.png");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let left = draft_with_images(&mut app, "Compare ", std::slice::from_ref(&first));
        let right = draft_with_images(&mut app, "With ", std::slice::from_ref(&second));
        assert_ne!(left.card, right.card);
        drop(app.update(Message::ChatUi(Action::OpenAttachment(left, 0))));
        drop(app.update(Message::ChatUi(Action::OpenAttachment(right, 0))));
        // Each pane has its own viewer, so neither load is superseded by
        // the other's, whichever finishes first, and no dialog covers the
        // workspace.
        for (target, image) in [(right, &second), (left, &first)] {
            let loaded = images::attachment_preview(image);
            assert!(matches!(loaded, images::Loaded::Ready(_)));
            drop(app.update(Message::ChatUi(Action::AttachmentOpened(
                target,
                image.clone(),
                loaded,
            ))));
            assert!(app.image_viewer(images::ViewerHost::Pane(target)).is_some());
        }
        assert!(app.image_viewer(images::ViewerHost::Dialog).is_none());
        assert!(!app.modal_is_open());
    });
}

/// Places the cursor at byte `position` of the one-line draft, selecting
/// from `anchor` when given, as an assistive tool or a click would.
fn place(app: &mut Cagents, target: Target, position: usize, anchor: Option<usize>) {
    let at = |column| text_editor::Position { line: 0, column };
    drop(app.handle_chat_ui(Action::ComposerCursor(
        target,
        text_editor::Cursor {
            position: at(position),
            selection: anchor.map(at),
        },
    )));
}

fn cursor(runtime: &Runtime) -> (usize, Option<usize>) {
    let cursor = runtime.composer.cursor();
    (
        cursor.position.column,
        cursor.selection.map(|anchor| anchor.column),
    )
}

#[test]
fn the_caret_steps_over_a_reference_and_never_stops_inside_it() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "See ", &[image.clone()]);
        let reference = token(&image);
        let runtime = app.chat_runtime_mut(target).unwrap();
        let start = runtime.composer.text().find(&reference).unwrap();
        let end = start + reference.len();
        let step = |app: &mut Cagents, action| {
            drop(app.handle_chat_ui(Action::Composer(target, action)));
            cursor(app.chat_runtime_mut(target).unwrap())
        };
        let right = text_editor::Action::Move(text_editor::Motion::Right);
        let left = text_editor::Action::Move(text_editor::Motion::Left);

        place(&mut app, target, start, None);
        assert_eq!(
            step(&mut app, right.clone()),
            (end, None),
            "Right crosses it"
        );
        assert_eq!(
            step(&mut app, left.clone()),
            (start, None),
            "Left crosses back"
        );
        // A caret put inside goes to the nearer side.
        place(&mut app, target, start + 2, None);
        assert_eq!(cursor(app.chat_runtime_mut(target).unwrap()), (start, None));
        place(&mut app, target, end - 2, None);
        assert_eq!(cursor(app.chat_runtime_mut(target).unwrap()), (end, None));
        // Selecting into it takes all of it.
        place(&mut app, target, start, None);
        let select = text_editor::Action::Select(text_editor::Motion::Right);
        assert_eq!(step(&mut app, select), (end, Some(start)));
        // Selecting back lets go of all of it, rather than stick at its end.
        let unselect = text_editor::Action::Select(text_editor::Motion::Left);
        assert_eq!(step(&mut app, unselect.clone()).0, start);
        let selected = |app: &mut Cagents| {
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.composer.selection().filter(|text| !text.is_empty())
        };
        assert_eq!(selected(&mut app), None);
        // And the same leftward: from the end, over it and back.
        place(&mut app, target, end, None);
        assert_eq!(step(&mut app, unselect), (start, Some(end)));
        let reselect = text_editor::Action::Select(text_editor::Motion::Right);
        assert_eq!(step(&mut app, reselect).0, end);
        assert_eq!(selected(&mut app), None);
        assert_eq!(app.chat_runtime_mut(target).unwrap().attachments, [image]);
    });
}

#[test]
fn delete_or_a_partial_selection_takes_the_whole_reference_and_its_image() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|first| {
        let second = chat::attachment_assets::test_image("second.png");
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "A ", &[first.clone(), second.clone()]);
        let runtime = app.chat_runtime_mut(target).unwrap();
        let full = runtime.composer.text();
        let (one, two) = (token(&first), token(&second));
        let start = full.find(&one).unwrap();

        // Forward Delete just before a reference takes all of it.
        place(&mut app, target, start, None);
        let runtime = app.chat_runtime_mut(target).unwrap();
        edit(runtime, text_editor::Edit::Delete);
        assert_eq!(runtime.composer.text(), full.replacen(&one, "", 1));
        assert_eq!(runtime.attachments, [second.clone()]);
        assert!(runtime.undo_composer(false));
        assert_eq!(runtime.composer.text(), full);
        assert_eq!(runtime.attachments, [first.clone(), second.clone()]);

        // Typing over a selection that took the end of one reference and the
        // start of the next replaces both whole: no `[Imag` is left.
        let at = full.find(&two).unwrap();
        place(&mut app, target, at + 3, Some(start + one.len() - 2));
        let runtime = app.chat_runtime_mut(target).unwrap();
        edit(runtime, text_editor::Edit::Insert('x'));
        let typed = format!("{}x{}", &full[..start], &full[at + two.len()..]);
        assert_eq!(runtime.composer.text(), typed);
        assert!(runtime.attachments.is_empty());
        assert!(runtime.undo_composer(false));
        assert_eq!(runtime.composer.text(), full);
        assert_eq!(runtime.attachments, [first, second]);
    });
}

#[test]
fn text_that_only_looks_like_a_reference_edits_as_text() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "[Image 999] and ", &[image.clone()]);
        place(&mut app, target, "[Image 999]".len(), None);
        let runtime = app.chat_runtime_mut(target).unwrap();
        edit(runtime, text_editor::Edit::Backspace);
        assert!(runtime.composer.text().starts_with("[Image 999 and "));
        assert_eq!(runtime.attachments, [image]);
    });
}

#[test]
fn copying_a_reference_into_another_draft_brings_its_image() {
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let source = draft_with_images(&mut app, "Here ", &[image.clone()]);
        let reference = token(&image);
        let full = app.chat_runtime_mut(source).unwrap().composer.text();
        let start = full.find(&reference).unwrap();
        place(&mut app, source, start + reference.len(), Some(start));
        drop(app.handle_chat_ui(Action::ComposerCopied(source)));

        let card = draft_chat(&mut app);
        let other = chat_runtime(&app, card).target;
        let paste = |app: &mut Cagents, text: &str| {
            let ticket = app
                .chat_runtime_mut(other)
                .unwrap()
                .begin_attachment_insert();
            drop(app.handle_chat_ui(Action::PastedText(other, ticket, Some(text.to_owned()))));
        };
        // Text from anywhere else that merely says the same names nothing.
        paste(&mut app, &format!("{reference} too"));
        let runtime = app.chat_runtime_mut(other).unwrap();
        assert!(runtime.attachments.is_empty());
        assert!(runtime.undo_composer(false));
        paste(&mut app, &reference);
        let runtime = app.chat_runtime_mut(other).unwrap();
        assert_eq!(runtime.composer.text(), reference);
        assert_eq!(runtime.attachments, [image.clone()]);
        // Undoing the paste takes the image back out with its reference.
        assert!(runtime.undo_composer(false));
        assert_eq!(runtime.composer.text(), "");
        assert!(runtime.attachments.is_empty());
        assert!(runtime.undo_composer(true));
        assert_eq!(runtime.attachments, [image.clone()]);
        // The draft it came from still has it.
        assert_eq!(app.chat_runtime_mut(source).unwrap().attachments, [image]);
    });
}

#[test]
fn copy_and_cut_chords_note_what_the_composer_copies() {
    use iced::keyboard::{self, key::Code, key::Physical};
    let _admission = attachment_draft::test_admission();
    chat::attachment_assets::with_test_image(|image| {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let target = draft_with_images(&mut app, "Here ", &[image]);
        let renderer = crate::app::tests::headless_renderer();
        for (character, code) in [("c", Code::KeyC), ("x", Code::KeyX)] {
            let messages = composer_key_messages(
                &app,
                target.card,
                720.0,
                keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character(character.into()),
                    modified_key: keyboard::Key::Character(character.into()),
                    physical_key: Physical::Code(code),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::CTRL,
                    text: None,
                    repeat: false,
                },
                &renderer,
            );
            assert!(
                matches!(
                    messages.first(),
                    Some(Message::ChatUi(Action::ComposerCopied(found))) if *found == target
                ),
                "Ctrl+{character}"
            );
        }
    });
}
