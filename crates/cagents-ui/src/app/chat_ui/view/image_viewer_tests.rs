//! An image opens over its own pane's transcript rather than as a dialog over
//! the window (#3191). The pane's composer stays usable below it, sending
//! leaves it open, the rest of the workspace stays interactive, and a key
//! goes to whatever holds the keyboard.
use super::super::tests::{chat_runtime, draft_chat, live_chat, task_focuses};
use super::*;
use crate::app::dialog_ui::tests::{key_control, key_press};
use images::{Gallery, ViewerHost, viewer_focus};
use keyboard::key::Named;

/// Shows `count` more decoded images in `runtime`'s transcript, after any it
/// shows already and so in the same gallery, returning their item ids and the
/// pixels each is drawn with.
fn show_images(runtime: &mut Runtime, count: u8) -> Vec<(String, iced::advanced::image::Id)> {
    let shown = runtime
        .snapshot
        .items
        .iter()
        .filter(|item| matches!(item.content, ItemContent::Image { .. }))
        .count() as u8;
    let images = (shown..shown + count)
        .map(|index| {
            let id = format!("image-{index}");
            let content = ItemContent::Image {
                path: format!("/fixture/image-{index}.png"),
                description: format!("Chart {index}"),
                reference: None,
            };
            let handle = iced::widget::image::Handle::from_rgba(1, 1, vec![index, 0, 0, 255]);
            let item = Arc::new(chat::Item {
                id: id.clone(),
                role: chat::Role::Assistant,
                started_at: None,
                at: None,
                content: content.clone(),
            });
            // Decoded for its item, as a read leaves it, so a snapshot that
            // carries the item over keeps its pixels.
            let mut cached = CachedItem::new(&content, runtime.syntax_theme);
            cached.image = Some(handle.clone());
            cached.image_item = Some(item.clone());
            runtime.cache.insert(id.clone(), cached);
            Arc::make_mut(&mut runtime.snapshot).items.push(item);
            (id, handle.id())
        })
        .collect();
    // As a snapshot regroups them, so a gallery holds its pictures.
    runtime.turns = runtime.transcript_turns(&runtime.snapshot).into();
    images
}

/// A focused live conversation, shown on screen, with two images, the first
/// one open.
fn viewing(app: &mut Cagents) -> (CardId, Target, Vec<(String, iced::advanced::image::Id)>) {
    let card = live_chat(app, Some("session"));
    let target = chat_runtime(app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot).busy = false;
    runtime.visible.store(true, Ordering::Relaxed);
    let images = show_images(runtime, 2);
    drop(app.update(Message::ChatUi(Action::OpenImage(
        target,
        images[0].0.clone(),
    ))));
    (card, target, images)
}

fn shown(app: &Cagents, target: Target) -> Option<images::ImageRef> {
    let host = ViewerHost::Pane(target);
    app.image_viewer(host)
        .and_then(|gallery| gallery.selected_image(host))
}

fn press(
    app: &mut Cagents,
    key: keyboard::Key,
    modifiers: keyboard::Modifiers,
    text: Option<&str>,
    captured: bool,
) {
    drop(app.update(Message::KeyEvent {
        event: keyboard::Event::KeyPressed {
            modified_key: key.clone(),
            key,
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers,
            text: text.map(Into::into),
            repeat: false,
        },
        captured,
    }));
}

fn named(app: &mut Cagents, key: Named, captured: bool) {
    press(
        app,
        keyboard::Key::Named(key),
        keyboard::Modifiers::empty(),
        None,
        captured,
    );
}

#[test]
fn an_open_image_blocks_nothing_outside_its_own_transcript() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let other = draft_chat(&mut app);
    let (card, target, images) = viewing(&mut app);
    let host = ViewerHost::Pane(target);
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, images[0].1)),
        "the viewer reuses the transcript's pixels"
    );
    assert!(!app.modal_is_open() && !app.workspace_inputs_blocked());
    assert!(!app.pane_content_inputs_blocked(card));
    assert!(app.dialog().is_none(), "nothing covers the window");
    // Its controls are the pane's, so a dialog over the pane holds them too.
    assert!(!Action::ImageNext(host, true).allowed_while_modal());
    assert!(!Action::CloseImage(host).allowed_while_modal());
    assert!(Action::CloseImage(ViewerHost::Dialog).allowed_while_modal());

    // Another pane takes the keyboard as usual, and its keys are its own.
    drop(app.workspace.focus(other));
    assert_eq!(app.workspace.focused(), Some(other));
    let draft = chat_runtime(&app, other).composer.text();
    press(
        &mut app,
        keyboard::Key::Character("y".into()),
        keyboard::Modifiers::empty(),
        Some("y"),
        false,
    );
    assert_eq!(
        chat_runtime(&app, other).composer.text().trim_end(),
        format!("{}y", draft.trim_end())
    );
    named(&mut app, Named::ArrowRight, false);
    named(&mut app, Named::Escape, false);
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, images[0].1)),
        "keys in another pane leave this viewer alone"
    );
}

#[test]
fn keys_go_to_the_composer_or_the_viewer_by_where_the_keyboard_is() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, images) = viewing(&mut app);
    let host = ViewerHost::Pane(target);
    let at = |index: usize| Some(images::ImageRef::Viewer(host, images[index].1));

    // Typing with no text field holding the keyboard, as when the viewer
    // has it, starts the draft; nothing is swallowed.
    press(
        &mut app,
        keyboard::Key::Character("W".into()),
        keyboard::Modifiers::SHIFT,
        Some("W"),
        false,
    );
    assert_eq!(chat_runtime(&app, card).composer.text().trim_end(), "W");
    assert_eq!(shown(&app, target), at(0));

    // An arrow the composer took moves its caret, not the gallery.
    named(&mut app, Named::ArrowRight, true);
    assert_eq!(shown(&app, target), at(0));
    // One nothing took browses, wrapping, and chords are left alone.
    named(&mut app, Named::ArrowRight, false);
    assert_eq!(shown(&app, target), at(1));
    named(&mut app, Named::ArrowRight, false);
    assert_eq!(shown(&app, target), at(0));
    named(&mut app, Named::ArrowLeft, false);
    assert_eq!(shown(&app, target), at(1));
    press(
        &mut app,
        keyboard::Key::Named(Named::ArrowLeft),
        keyboard::Modifiers::CTRL,
        None,
        false,
    );
    assert_eq!(shown(&app, target), at(1));

    // Escape a control claimed, such as the composer's, or a menu's over the
    // image, keeps the viewer; the next one closes it, keeping the draft.
    named(&mut app, Named::Escape, true);
    assert_eq!(shown(&app, target), at(1));
    named(&mut app, Named::Escape, false);
    assert!(shown(&app, target).is_none());
    assert_eq!(chat_runtime(&app, card).composer.text().trim_end(), "W");
}

/// Escape in the composer closes the topmost layer first (#3435): an open
/// image goes before a running turn, which the next Escape interrupts. With
/// neither, Escape only leaves the draft.
#[test]
fn escape_in_the_composer_closes_the_image_before_interrupting() {
    for busy in [false, true] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, _) = viewing(&mut app);
        Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).busy = busy;
        let composer = card_control_id(card, "chat-composer");
        let (messages, captured) = key_control(
            app.chat_view(card),
            composer.clone(),
            key_press(Named::Escape),
        );
        assert!(captured, "busy {busy}: the press goes no further");
        assert!(
            matches!(
                messages.as_slice(),
                [Message::ChatUi(Action::CloseImage(ViewerHost::Pane(found)))] if *found == target
            ),
            "busy {busy}: {messages:?}"
        );
        for message in messages {
            let task = app.update(message);
            assert!(
                task_focuses(task, &composer).0,
                "busy {busy}: the keyboard stays in the composer"
            );
        }
        assert!(shown(&app, target).is_none(), "busy {busy}");

        let (messages, captured) =
            key_control(app.chat_view(card), composer, key_press(Named::Escape));
        if busy {
            assert!(captured);
            assert!(
                matches!(messages.as_slice(), [Message::ChatUi(Action::Interrupt(found))] if *found == target),
                "{messages:?}"
            );
        } else {
            assert!(!captured && messages.is_empty(), "{messages:?}");
        }
    }
}

#[test]
fn sending_and_new_activity_leave_the_viewer_open() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, images) = viewing(&mut app);
    let host = ViewerHost::Pane(target);
    named(&mut app, Named::ArrowRight, false);
    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.replace_draft(text_editor::Content::with_text(
        "What does this chart show?",
    ));
    drop(app.update(Message::ChatUi(Action::Submit(target, PromptMode::Send))));
    let runtime = chat_runtime(&app, card);
    assert!(runtime.commands.in_flight().is_some(), "the message went");
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, images[1].1))
    );

    // The conversation moving on, a reply and all, doesn't close it either.
    let mut snapshot = (*chat_runtime(&app, card).snapshot).clone();
    snapshot.busy = true;
    for (id, role, text) in [
        ("sent", chat::Role::User, "What does this chart show?"),
        ("reply", chat::Role::Assistant, "Weekly totals, rising."),
    ] {
        snapshot.items.push(Arc::new(chat::Item {
            id: id.into(),
            role,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
        }));
    }
    super::super::show_chat_snapshot_for_test(&mut app, card, snapshot);
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, images[1].1))
    );

    // An image in the reply is another gallery's: the shown image stays, and
    // the viewer still browses its own.
    let runtime = app.chat_runtime_mut(target).unwrap();
    let (_, arrived) = show_images(runtime, 1).pop().unwrap();
    images::sync(runtime);
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, images[1].1))
    );
    named(&mut app, Named::ArrowRight, false);
    assert_eq!(app.image_viewer(host).unwrap().position(), (1, 2));
    assert_ne!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, arrived))
    );
}

/// Sending from the composer hands the keyboard back to the image, so the
/// Escape that follows closes the viewer rather than interrupt the reply.
#[test]
fn sending_hands_the_keyboard_back_to_the_image() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = viewing(&mut app);
    let composer = card_control_id(card, "chat-composer");
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("Why does it drop?"));
    let (messages, captured) = key_control(
        app.chat_view(card),
        composer.clone(),
        key_press(Named::Enter),
    );
    assert!(captured);
    assert!(
        matches!(
            messages.as_slice(),
            [
                Message::ChatUi(Action::Submit(sent, _)),
                Message::ChatUi(Action::FocusImage(focused)),
            ] if *sent == target && *focused == target
        ),
        "{messages:?}"
    );

    // Ctrl+End stays in the draft instead of scrolling the covered transcript.
    let mut end = key_press(Named::End);
    if let keyboard::Event::KeyPressed { modifiers, .. } = &mut end {
        *modifiers = keyboard::Modifiers::CTRL;
    }
    let (messages, _) = key_control(app.chat_view(card), composer.clone(), end);
    assert!(
        !messages
            .iter()
            .any(|message| matches!(message, Message::ChatUi(Action::Latest(_)))),
        "{messages:?}"
    );

    // So does the Send button, which otherwise hands it to the composer.
    let task = app.handle_chat_ui(Action::SubmitControl(target, PromptMode::Send));
    assert!(
        chat_runtime(&app, card).commands.in_flight().is_some(),
        "the message went"
    );
    assert!(task_focuses(task, &viewer_focus(card)).0);
}

/// The viewer moves `steps` images forward and returns the one it shows.
fn step(app: &mut Cagents, target: Target, steps: usize) -> Option<images::ImageRef> {
    for _ in 0..steps {
        named(app, Named::ArrowRight, false);
    }
    shown(app, target)
}

/// A reply's image usually finishes decoding after the snapshot that brought
/// it. It joins the open viewer's gallery at once, so the count and the steps
/// don't change under the reader, and shows as loading until its pixels
/// arrive, then shows them in the same place.
#[test]
fn an_image_decoded_after_its_snapshot_joins_the_viewer() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (_, target, images) = viewing(&mut app);
    let host = ViewerHost::Pane(target);
    let runtime = app.chat_runtime_mut(target).unwrap();
    let (id, _) = show_images(runtime, 1).pop().unwrap();
    let item = runtime.snapshot.items.last().unwrap().clone();
    let cached = runtime.cache.get_mut(&id).unwrap();
    cached.image = None;
    cached.image_source = Some(item.clone());
    cached.image_item = Some(item.clone());
    images::sync(runtime);
    let attempt = runtime.cache[&id].image_attempt;
    let position = |app: &Cagents| app.image_viewer(host).unwrap().position();
    assert_eq!(position(&app), (1, 3), "it joins while still decoding");
    assert_eq!(
        step(&mut app, target, 2),
        None,
        "it is browsed to, and shows as loading"
    );
    assert_eq!(position(&app), (3, 3));

    let handle = iced::widget::image::Handle::from_rgba(1, 1, vec![9, 0, 0, 255]);
    let decoded = handle.id();
    drop(app.handle_chat_ui(Action::ImageLoaded(
        target,
        item,
        attempt,
        images::Loaded::Ready(handle),
    )));
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, decoded)),
        "its pixels show once decoded"
    );
    assert_eq!(position(&app), (3, 3), "in the same place");
    assert_eq!(
        step(&mut app, target, 1),
        Some(images::ImageRef::Viewer(host, images[0].1))
    );
}

/// The viewer holds the pixels of the image it shows, even once newer items
/// page it out of the transcript, and borrows the rest from the transcript:
/// a paged-out image leaves the viewer once it moves off it, and one whose
/// pixels the transcript let go holds none in the viewer either, until it is
/// shown and read again whole.
#[test]
fn the_viewer_keeps_its_image_and_borrows_the_rest() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (_, target, images) = viewing(&mut app);
    let host = ViewerHost::Pane(target);
    let runtime = app.chat_runtime_mut(target).unwrap();
    let (_, arrived) = show_images(runtime, 1).pop().unwrap();
    Arc::make_mut(&mut runtime.snapshot)
        .items
        .retain(|item| item.id != images[0].0);
    runtime.turns = runtime.transcript_turns(&runtime.snapshot).into();
    images::sync(runtime);
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, images[0].1)),
        "the paged-out image stays shown"
    );
    assert_eq!(
        app.image_viewer(host).unwrap().position(),
        (1, 3),
        "in its place"
    );
    assert_eq!(
        step(&mut app, target, 2),
        Some(images::ImageRef::Viewer(host, arrived)),
        "and newer images still join after it"
    );

    assert_eq!(
        step(&mut app, target, 1),
        Some(images::ImageRef::Viewer(host, images[1].1)),
        "the paged-out image left as the viewer moved off it"
    );
    step(&mut app, target, 1);

    let runtime = app.chat_runtime_mut(target).unwrap();
    runtime.cache.get_mut(&images[1].0).unwrap().image = None;
    images::sync(runtime);
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, arrived))
    );
    assert_eq!(
        step(&mut app, target, 1),
        None,
        "the image the transcript let go has no pixels in the viewer"
    );
    assert_eq!(app.image_viewer(host).unwrap().position(), (1, 2));
}

/// Pixels the transcript sheds, under memory pressure, once its pane is
/// hidden or as it syncs, leave the viewer at once, not with the next snapshot: it keeps
/// only the image it shows.
#[test]
fn the_viewer_lets_go_of_the_pixels_the_transcript_sheds() {
    let shed: [fn(&mut Runtime); 3] = [
        |runtime| assert_eq!(images::evict_ready(runtime, usize::MAX), usize::MAX - 8),
        Runtime::release_transcript_caches,
        // A hidden pane's transcript wants no images.
        |runtime| {
            runtime.visible.store(false, Ordering::Relaxed);
            images::sync(runtime);
        },
    ];
    for shed in shed {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (_, target, images) = viewing(&mut app);
        let host = ViewerHost::Pane(target);
        shed(app.chat_runtime_mut(target).unwrap());
        let gallery = app.image_viewer(host).unwrap();
        assert_eq!(
            gallery.selected_image(host),
            Some(images::ImageRef::Viewer(host, images[0].1))
        );
        assert_ne!(
            step(&mut app, target, 1),
            Some(images::ImageRef::Viewer(host, images[1].1)),
            "the image the viewer didn't show has no pixels in it"
        );
    }
}

/// The shown image keeps its pixels through memory pressure only while it is
/// shown: stepping off it lets them go, so they don't hold the budget a
/// newer image's decode waits on.
#[test]
fn stepping_off_an_evicted_image_lets_its_pixels_go() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (_, target, images) = viewing(&mut app);
    let host = ViewerHost::Pane(target);
    // The oldest image, the one shown, is the first to go.
    let runtime = app.chat_runtime_mut(target).unwrap();
    assert_eq!(images::evict_ready(runtime, 1), 0);
    assert!(runtime.cache[&images[0].0].image.is_none());
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, images[0].1)),
        "the viewer keeps the image it shows"
    );
    assert_eq!(
        step(&mut app, target, 1),
        Some(images::ImageRef::Viewer(host, images[1].1))
    );
    assert_eq!(
        step(&mut app, target, 1),
        None,
        "the evicted image's pixels left as the viewer moved off it"
    );
    assert_eq!(app.image_viewer(host).unwrap().position(), (1, 2));
}

#[test]
fn find_closes_the_viewer_over_the_transcript_it_searches() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = viewing(&mut app);
    assert!(app.open_chat_find().is_some());
    assert!(chat_runtime(&app, card).find.is_some());
    assert!(shown(&app, target).is_none());
}

#[test]
fn the_viewer_holds_the_transcript_area_and_keeps_the_composer_in_place() {
    let roles = ["chat-composer", "image-viewer"];
    let (mut runtime, _handle) = super::tests::furnished_runtime();
    let images = show_images(&mut runtime, 2);
    runtime.composer = text_editor::Content::with_text("What does this chart show?");
    for width in [360.0_f32, 900.0] {
        runtime.image_viewer = None;
        let closed = super::tests::lay_out(&runtime, width, None, &roles);
        assert!(!closed.controls.contains_key("image-viewer"));
        runtime.image_viewer = Gallery::new(&runtime, &images[0].0);
        assert!(runtime.image_viewer.is_some());
        let open = super::tests::lay_out(&runtime, width, None, &roles);
        let composer = open.controls["chat-composer"];
        assert_eq!(
            composer, closed.controls["chat-composer"],
            "the composer doesn't move as the viewer opens at {width}"
        );
        assert!(
            open.controls.contains_key("image-viewer"),
            "it holds the keyboard"
        );
        // Everything the viewer draws, from its toolbar to its hint, fits the
        // pane above the composer.
        for needle in ["1 / 2", "Scroll to zoom"] {
            let (bounds, _) = open
                .placed_text
                .iter()
                .find(|(_, text)| text.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} drawn at {width}"));
            assert!(
                bounds.x >= 0.0 && bounds.x + bounds.width <= width + 0.5,
                "{needle:?} fits the pane at {width}: {bounds:?}"
            );
            assert!(
                bounds.y + bounds.height <= composer.y,
                "{needle:?} stays above the composer at {width}: {bounds:?}"
            );
        }
        assert!(open.text.iter().any(|text| text == "1 / 2"));
        // A narrow pane keeps every command by naming them with icons.
        assert_eq!(
            open.text.iter().any(|text| text == "Open externally"),
            width > 520.0,
            "{width}: {:?}",
            open.text
        );
    }
}

/// An open context menu over the image claims the Escape that closes it,
/// which is how the app's key handler knows to leave the viewer open.
#[test]
fn the_image_menu_claims_the_escape_that_closes_it() {
    use iced::advanced::layout::{Layout, Limits};
    use iced::advanced::widget::Tree;
    use iced::advanced::{Shell, clipboard, mouse};
    let (mut runtime, _handle) = super::tests::furnished_runtime();
    let images = show_images(&mut runtime, 1);
    let gallery = Gallery::new(&runtime, &images[0].0).unwrap();
    let renderer = crate::app::tests::headless_renderer();
    let window = iced::Size::new(900.0, 700.0);
    let viewport = iced::Rectangle::with_size(window);
    let mut element = gallery.pane_view(runtime.target, window.width, 0.0);
    let mut tree = Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &Limits::new(iced::Size::ZERO, window),
    );
    let pointer = mouse::Cursor::Available(viewport.center());
    let mut send = |element: &mut Element<'_, Message>, event: iced::Event| {
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            pointer,
            &renderer,
            &mut clipboard::Null,
            &mut shell,
            &viewport,
        );
        shell.is_event_captured()
    };
    let escape = iced::Event::Keyboard(key_press(Named::Escape));
    assert!(!send(&mut element, escape.clone()), "no menu, no claim");
    send(
        &mut element,
        iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
    );
    assert!(send(&mut element, escape.clone()), "the menu takes Escape");
    assert!(!send(&mut element, escape), "and is gone");
}

/// The composer keeps its arrows while an image is open above it: they move
/// its caret rather than browse.
#[test]
fn a_focused_composer_keeps_its_arrows() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = viewing(&mut app);
    let composer = card_control_id(card, "chat-composer");
    app.chat_runtime_mut(target)
        .unwrap()
        .replace_draft(text_editor::Content::with_text("Which chart"));
    for key in [Named::ArrowLeft, Named::ArrowRight] {
        let (messages, captured) =
            key_control(app.chat_view(card), composer.clone(), key_press(key));
        assert!(captured, "{key:?} stays in the draft");
        assert!(
            !messages
                .iter()
                .any(|message| matches!(message, Message::ChatUi(Action::ImageNext(..)))),
            "{messages:?}"
        );
    }
}

/// The images belong to the transcript they came from, so showing a
/// subagent's transcript instead closes the viewer over it.
#[test]
fn switching_to_a_subagent_closes_the_viewer_of_its_parent() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (_, target, _) = viewing(&mut app);
    let runtime = app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot).subagents = vec![super::tests::native_child_fixture()];
    let child = runtime.snapshot.subagents[0].id.clone();
    drop(app.update(Message::ChatUi(Action::SelectSubagent(target, None))));
    assert!(
        shown(&app, target).is_some(),
        "the same transcript keeps it"
    );
    drop(app.update(Message::ChatUi(Action::SelectSubagent(target, Some(child)))));
    assert!(shown(&app, target).is_none());
}

/// A device's file, which no conversation shows, still opens as a dialog over
/// the window, and that dialog answers Escape itself.
#[test]
fn a_device_file_opens_as_a_dialog_that_escape_closes() {
    use iced::advanced::layout::{Layout, Limits};
    use iced::advanced::widget::Tree;
    use iced::advanced::{Shell, clipboard, mouse};
    let mut app = crate::app::tests::context_inspector_test_app(false);
    app.chat_ui.image_dialog = Some(Gallery::file(
        "/fixture/photo.png".into(),
        None,
        iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
    ));
    assert!(app.modal_is_open() && app.dialog().is_some());
    let renderer = crate::app::tests::headless_renderer();
    let window = iced::Size::new(900.0, 700.0);
    let viewport = iced::Rectangle::with_size(window);
    let mut element = app.chat_ui.image_dialog.as_ref().unwrap().dialog_view();
    let mut tree = Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &Limits::new(iced::Size::ZERO, window),
    );
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    element.as_widget_mut().update(
        &mut tree,
        &iced::Event::Keyboard(key_press(Named::Escape)),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut clipboard::Null,
        &mut shell,
        &viewport,
    );
    drop(element);
    assert!(
        matches!(
            messages.as_slice(),
            [Message::ChatUi(Action::CloseImage(ViewerHost::Dialog))]
        ),
        "{messages:?}"
    );
    drop(app.update(messages.remove(0)));
    assert!(app.chat_ui.image_dialog.is_none() && !app.modal_is_open());
}

/// With the pane's inspector closed, its toggle floats over the pane's
/// top-right corner; the viewer's toolbar ends before it, as the transcript
/// does, so Close is never under the toggle.
#[test]
fn the_toolbar_clears_the_inspector_toggle() {
    use iced::advanced::widget::{Operation, Tree, operation::Focusable};
    use iced::advanced::{Layout, layout::Limits};
    struct Texts(Vec<(iced::Rectangle, String)>);
    impl Operation for Texts {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn container(&mut self, _: Option<&widget::Id>, _: iced::Rectangle) {}
        fn focusable(&mut self, _: Option<&widget::Id>, _: iced::Rectangle, _: &mut dyn Focusable) {
        }
        fn text(&mut self, _: Option<&widget::Id>, bounds: iced::Rectangle, text: &str) {
            self.0.push((bounds, text.to_owned()));
        }
    }
    let (mut runtime, _handle) = super::tests::furnished_runtime();
    let images = show_images(&mut runtime, 2);
    let gallery = Gallery::new(&runtime, &images[0].0).unwrap();
    let renderer = crate::app::tests::headless_renderer();
    let clearance = crate::app::pane_inspector::TOGGLE_CLEARANCE;
    let width = 640.0;
    let mut element = gallery.pane_view(runtime.target, width, clearance);
    let mut tree = Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &Limits::new(iced::Size::ZERO, iced::Size::new(width, 600.0)),
    );
    let mut texts = Texts(Vec::new());
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut texts);
    let (open, _) = texts
        .0
        .iter()
        .find(|(_, text)| text == "Open externally")
        .expect("the wide toolbar names its commands");
    // Close follows the last named command on the toolbar's row, and the
    // row ends where the clearance begins.
    let row_end = node.bounds().width - clearance;
    assert!(
        open.x + open.width <= row_end,
        "{open:?} reaches past {row_end}"
    );
    let toolbar = node.children()[0].bounds();
    assert!(
        toolbar.x + toolbar.width <= row_end + 0.5,
        "the toolbar {toolbar:?} reaches under the toggle at {row_end}"
    );
}

/// The viewer browses the gallery the reader opened, not every picture of the
/// transcript: one that arrives after more of the reply's text is another
/// gallery's, and stays out of it.
#[test]
fn a_picture_of_another_gallery_stays_out_of_the_viewer() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (_, target, images) = viewing(&mut app);
    let host = ViewerHost::Pane(target);
    let runtime = app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot)
        .items
        .push(Arc::new(chat::Item {
            id: "between".into(),
            role: chat::Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: "And here is another view:".into(),
                thinking: false,
                phase: Default::default(),
            },
        }));
    show_images(runtime, 1);
    images::sync(runtime);
    assert_eq!(
        step(&mut app, target, 1),
        Some(images::ImageRef::Viewer(host, images[1].1))
    );
    assert_eq!(
        step(&mut app, target, 1),
        Some(images::ImageRef::Viewer(host, images[0].1)),
        "the next picture after the gallery's last is its first again"
    );
}
