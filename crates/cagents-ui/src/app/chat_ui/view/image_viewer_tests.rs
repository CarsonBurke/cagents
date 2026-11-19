//! An image opens over its own pane's transcript rather than as a dialog over
//! the window (#3191). The pane's composer stays usable below it, sending
//! leaves it open, the rest of the workspace stays interactive, and a key
//! goes to whatever holds the keyboard.
use super::super::tests::{chat_runtime, draft_chat, live_chat};
use super::*;
use crate::app::dialog_ui::tests::{focus_after_key, key_control, key_press};
use images::{Gallery, ViewerHost, viewer_focus};
use keyboard::key::Named;

/// Shows `count` decoded images in `runtime`'s transcript, returning their
/// item ids and the pixels each is drawn with.
fn show_images(runtime: &mut Runtime, count: u8) -> Vec<(String, iced::advanced::image::Id)> {
    (0..count)
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

/// A focused live conversation showing two images, the first one open.
fn viewing(app: &mut Cagents) -> (CardId, Target, Vec<(String, iced::advanced::image::Id)>) {
    let card = live_chat(app, Some("session"));
    let target = chat_runtime(app, card).target;
    let runtime = app.chat_runtime_mut(target).unwrap();
    Arc::make_mut(&mut runtime.snapshot).busy = false;
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
    assert!(runtime.pending.is_some(), "the message went");
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
        let viewer = open.controls["image-viewer"];
        assert!(
            viewer.x >= 0.0 && viewer.x + viewer.width <= width + 0.5,
            "the image fits the pane at {width}: {viewer:?}"
        );
        assert!(
            viewer.y + viewer.height <= composer.y,
            "the image stays above the composer at {width}"
        );
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
    let mut element = gallery.pane_view(runtime.target, window.width);
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
