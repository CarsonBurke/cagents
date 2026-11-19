//! An image opens over its own pane's transcript rather than as a dialog over
//! the window (#3191). The pane's composer stays usable below it, sending
//! leaves it open, the rest of the workspace stays interactive, and a key
//! goes to whatever holds the keyboard.
use super::super::tests::{chat_runtime, draft_chat, live_chat, task_focuses};
use super::*;
use crate::app::dialog_ui::tests::{focus_after_key, key_control, key_press};
use images::{Gallery, ViewerHost, viewer_focus};
use keyboard::key::Named;

/// Shows `count` more decoded images in `runtime`'s transcript, returning
/// their item ids and the pixels each is drawn with.
fn show_images(runtime: &mut Runtime, count: u8) -> Vec<(String, iced::advanced::image::Id)> {
    let shown = runtime
        .snapshot
        .items
        .iter()
        .filter(|item| matches!(item.content, ItemContent::Image { .. }))
        .count() as u8;
    (shown..shown + count)
        .map(|index| {
            let id = format!("image-{index}");
            let content = ItemContent::Image {
                path: format!("/fixture/image-{index}.png"),
                description: format!("Chart {index}"),
                reference: None,
            };
            let handle = iced::widget::image::Handle::from_rgba(1, 1, vec![index, 0, 0, 255]);
            let mut cached = CachedItem::new(&content, runtime.syntax_theme);
            cached.image = Some(handle.clone());
            runtime.cache.insert(id.clone(), cached);
            Arc::make_mut(&mut runtime.snapshot)
                .items
                .push(Arc::new(chat::Item {
                    id: id.clone(),
                    role: chat::Role::Assistant,
                    started_at: None,
                    at: None,
                    content,
                }));
            (id, handle.id())
        })
        .collect()
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
        .map(|gallery| gallery.selected_image(host))
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

#[test]
fn escape_in_the_composer_hands_the_keyboard_to_the_viewer() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, _) = viewing(&mut app);
    let composer = card_control_id(card, "chat-composer");
    let (messages, captured) = key_control(
        app.chat_view(card),
        composer.clone(),
        key_press(Named::Escape),
    );
    assert!(
        captured,
        "the press that leaves the draft doesn't also close the viewer"
    );
    assert!(
        matches!(messages.as_slice(), [Message::ChatUi(Action::FocusImage(found))] if *found == target),
        "{messages:?}"
    );
    // The viewer takes the keyboard and leaves Escape for the app to close
    // it with.
    assert_eq!(
        focus_after_key(
            app.chat_view(card),
            viewer_focus(card),
            key_press(Named::Escape)
        ),
        (Some(viewer_focus(card)), false)
    );

    // With no viewer, the composer's Escape is still only a blur.
    drop(
        app.update(Message::ChatUi(Action::CloseImage(ViewerHost::Pane(
            target,
        )))),
    );
    let (messages, captured) = key_control(app.chat_view(card), composer, key_press(Named::Escape));
    assert!(!captured && messages.is_empty(), "{messages:?}");
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

    // An image in the reply joins the viewer once it is decoded, so it can be
    // browsed to without closing it; the shown image stays.
    let runtime = app.chat_runtime_mut(target).unwrap();
    let arrived = show_images(runtime, 1).pop().unwrap();
    images::sync(runtime);
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, images[1].1))
    );
    named(&mut app, Named::ArrowRight, false);
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, arrived.1))
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
/// it, and joins the open viewer then.
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
    assert_eq!(
        step(&mut app, target, 2),
        Some(images::ImageRef::Viewer(host, images[0].1)),
        "an image still decoding can't be shown yet"
    );

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
        Some(images::ImageRef::Viewer(host, images[0].1))
    );
    assert_eq!(
        step(&mut app, target, 2),
        Some(images::ImageRef::Viewer(host, decoded))
    );
}

/// The viewer holds the pixels of the image it shows, even once newer items
/// page it out of the transcript, and borrows the rest from the transcript:
/// an image the transcript lets go leaves the viewer too, as does a paged-out
/// one once the viewer moves off it.
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
    images::sync(runtime);
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(host, images[0].1)),
        "the paged-out image stays shown"
    );
    assert_eq!(
        step(&mut app, target, 2),
        Some(images::ImageRef::Viewer(host, arrived)),
        "and newer images still join after it"
    );

    images::sync(app.chat_runtime_mut(target).unwrap());
    assert_eq!(
        step(&mut app, target, 1),
        Some(images::ImageRef::Viewer(host, images[1].1)),
        "the paged-out image left once the viewer moved off it"
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
        Some(images::ImageRef::Viewer(host, arrived)),
        "the image the transcript let go is gone from the viewer"
    );
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
            images::ImageRef::Viewer(host, images[0].1)
        );
        assert_eq!(
            step(&mut app, target, 1),
            Some(images::ImageRef::Viewer(host, images[0].1)),
            "the image the viewer doesn't show is gone from it"
        );
    }
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

/// The composer keeps its own keys while an image is open above it: the
/// arrows move its caret, and while a turn runs its Escape still interrupts
/// rather than handing the keyboard to the viewer.
#[test]
fn a_focused_composer_keeps_its_arrows_and_its_interrupt() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    let (card, target, images) = viewing(&mut app);
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

    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).busy = true;
    let (messages, captured) = key_control(app.chat_view(card), composer, key_press(Named::Escape));
    assert!(captured);
    assert!(
        matches!(messages.as_slice(), [Message::ChatUi(Action::Interrupt(found))] if *found == target),
        "{messages:?}"
    );
    assert_eq!(
        shown(&app, target),
        Some(images::ImageRef::Viewer(
            ViewerHost::Pane(target),
            images[0].1
        ))
    );
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
