use super::*;
use iced::advanced::{
    Layout, Shell, clipboard, layout, mouse,
    widget::{Operation, Tree, operation},
};
use iced::{Event, Rectangle, Size};

fn key(key: keyboard::Key, text: Option<&str>) -> Event {
    Event::Keyboard(keyboard::Event::KeyPressed {
        modified_key: key.clone(),
        key,
        physical_key: keyboard::key::Physical::Unidentified(
            keyboard::key::NativeCode::Unidentified,
        ),
        location: keyboard::Location::Standard,
        modifiers: keyboard::Modifiers::empty(),
        text: text.map(Into::into),
        repeat: false,
    })
}

#[test]
fn gallery_and_the_existing_composer_keep_separate_keyboard_focus_and_geometry() {
    let item = chat::Item {
        id: "preview".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Image {
            path: "/fixture/image.png".into(),
            description: "Fixture image".into(),
            reference: None,
        },
        started_at: None,
        at: None,
    };
    let (mut runtime, _handle) = frame_cost::live_runtime(&[item]);
    runtime.composer = text_editor::Content::with_text("Keep my draft");
    runtime.cache.get_mut("preview").unwrap().image = Some(iced::widget::image::Handle::from_rgba(
        1,
        1,
        vec![12, 34, 56, 255],
    ));
    let gallery = images::Gallery::new(&runtime, "preview").unwrap();
    let renderer = crate::app::tests::headless_renderer();
    let shortcuts = crate::settings::KeyboardShortcuts::default();
    let editor_id = card_control_id(runtime.target.card, "chat-composer");
    for width in [280.0, 600.0, 1000.0] {
        let viewport = Rectangle::with_size(Size::new(width, 800.0));
        let mut element = runtime.view_with_gallery(
            None,
            false,
            true,
            &shortcuts,
            width,
            Place {
                channel: None,
                name: "Fixture".into(),
                branch: None,
            },
            true,
            None,
            vec![],
            Some(&gallery),
            None,
        );
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        struct Bounds {
            id: widget::Id,
            found: Option<Rectangle>,
        }
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: Rectangle,
                _: &mut dyn operation::Focusable,
            ) {
                if id == Some(&self.id) {
                    self.found = Some(bounds);
                }
            }
        }
        let mut bounds = Bounds {
            id: editor_id.clone(),
            found: None,
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut bounds);
        let composer = bounds
            .found
            .expect("the original composer remains mounted below the gallery");
        assert!(composer.y > 0.0 && composer.y + composer.height <= viewport.height);
        assert!(composer.x >= 0.0 && composer.x + composer.width <= viewport.width);
        // Use the actual native focus operation. No simulated focus flag or
        // replacement input field participates in this interaction test.
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(editor_id.clone()),
        );
        for event in [
            key(keyboard::Key::Named(keyboard::key::Named::ArrowLeft), None),
            key(keyboard::Key::Named(keyboard::key::Named::ArrowRight), None),
        ] {
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            assert!(
                !messages.iter().any(|message| matches!(
                    message,
                    Message::ChatUi(Action::ImageViewer(
                        _,
                        images::ViewerCommand::Next(_) | images::ViewerCommand::Close
                    ))
                )),
                "composer arrows must edit the draft without browsing the gallery"
            );
        }
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &key(keyboard::Key::Character("x".into()), Some("x")),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, Message::ChatUi(Action::Composer(_, _)))),
            "composer remains editable"
        );
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(images::canvas_focus_id()),
        );
        messages.clear();
        element.as_widget_mut().update(
            &mut tree,
            &key(keyboard::Key::Named(keyboard::key::Named::ArrowRight), None),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert!(
            messages.iter().any(|message| matches!(
                message,
                Message::ChatUi(Action::ImageViewer(_, images::ViewerCommand::Next(true)))
            )),
            "focused gallery owns image navigation"
        );
        assert_eq!(runtime.composer.text(), "Keep my draft");
    }
}
