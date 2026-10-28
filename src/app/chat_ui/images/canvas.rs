//! Keep native state with its pixels or gallery publication.
use super::*;
#[cfg(test)]
use iced::{
    Event, Rectangle, Size,
    advanced::{Layout, Shell, layout, widget::Tree},
    mouse,
};

pub(super) fn resettable<'a>(
    publication: uuid::Uuid,
    revision: u64,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    crate::ui_controls::owned(Identity::Gallery(publication, revision), content)
}

pub(super) fn preview<'a>(
    target: PreviewTarget,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    crate::ui_controls::owned(target, content)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Identity {
    Gallery(uuid::Uuid, u64),
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redraw_keeps_zoom_and_pan_but_fit_and_navigation_reset_them() {
        use iced::advanced::renderer::Headless;
        use iced::widget::image::{Handle, Viewer, viewer};
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                iced::Font::default(),
                iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let handle = Handle::from_rgba(1, 1, vec![0, 0, 0, 255]);
        let publication = uuid::Uuid::new_v4();
        let element = |revision| {
            resettable(
                publication,
                revision,
                Viewer::new(handle.clone())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(iced::ContentFit::Contain)
                    .into(),
            )
        };
        let mut first = element(0);
        let mut tree = Tree::new(first.as_widget());
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(300.0, 200.0));
        let node = first.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        let cursor = mouse::Cursor::Available(iced::Point::new(50.0, 50.0));
        let mut messages = Vec::new();
        for event in [
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        ] {
            first.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                cursor,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
        }
        let scale = |tree: &Tree| {
            viewer::scaled_image_size(
                &renderer,
                &handle,
                tree.children[0].state.downcast_ref::<viewer::State>(),
                bounds.size(),
                iced::ContentFit::Contain,
            )
        };
        let zoomed = scale(&tree);
        assert!(zoomed.width > 200.0);
        let mut redraw = element(0);
        tree.diff(redraw.as_widget_mut());
        assert_eq!(scale(&tree), zoomed);
        assert!(
            tree.children[0]
                .state
                .downcast_ref::<viewer::State>()
                .is_cursor_grabbed()
        );
        let mut reset = element(1);
        tree.diff(reset.as_widget_mut());
        assert_eq!(scale(&tree), Size::new(200.0, 200.0));
        assert!(
            !tree.children[0]
                .state
                .downcast_ref::<viewer::State>()
                .is_cursor_grabbed()
        );
    }

    #[test]
    fn replacing_a_gallery_without_an_empty_frame_releases_the_old_drag_and_zoom() {
        use iced::widget::image::{Handle, viewer};
        let renderer = crate::app::tests::headless_renderer();
        let handle = Handle::from_rgba(1, 1, vec![0, 0, 0, 255]);
        let first = Gallery::file("/fixture/same.png".into(), None, handle.clone());
        let mut element = first.canvas();
        let mut tree = Tree::new(element.as_widget());
        let bounds = Rectangle::with_size(Size::new(300.0, 200.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        let mut messages = Vec::new();
        for event in [
            mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            },
            mouse::Event::ButtonPressed(mouse::Button::Left),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(event),
                Layout::new(&node),
                mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
        }
        let scale = |tree: &Tree| {
            viewer::scaled_image_size(
                &renderer,
                &handle,
                tree.children[0].state.downcast_ref::<viewer::State>(),
                bounds.size(),
                iced::ContentFit::Contain,
            )
        };
        assert!(scale(&tree).width > 200.0);
        assert!(
            tree.children[0]
                .state
                .downcast_ref::<viewer::State>()
                .is_cursor_grabbed()
        );
        drop(element);

        // Cached pixels and reset counters can be identical when close/open
        // messages or a ready file preview replace a gallery before redraw.
        let replacement = Gallery::file("/fixture/same.png".into(), None, handle.clone());
        assert_eq!(first.reset, replacement.reset);
        let mut element = replacement.canvas();
        tree.diff(element.as_widget_mut());
        assert_eq!(scale(&tree), Size::new(200.0, 200.0));
        assert!(
            !tree.children[0]
                .state
                .downcast_ref::<viewer::State>()
                .is_cursor_grabbed()
        );
    }
}
