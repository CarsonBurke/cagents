//! How an image is sized where a conversation shows it: an attachment in a
//! prompt and a Markdown image in an answer alike. It keeps its aspect ratio
//! and is drawn at its natural size, shrunk to the width available and
//! [`MAX_HEIGHT`] but never enlarged. It sits at the start of its line, and
//! what holds it, such as the button that opens it, hugs the picture.
use super::*;
use iced::advanced::{
    Layout, Widget, layout, renderer,
    widget::{Tree, tree},
};
use iced::{Rectangle, Size, mouse};

/// The tallest an image is drawn in a conversation.
pub(in crate::app::chat_ui) const MAX_HEIGHT: f32 = 360.0;

/// `handle`'s pixels, sized as a conversation draws an image.
pub(in crate::app::chat_ui) fn image(
    handle: iced::widget::image::Handle,
) -> Element<'static, Message> {
    bounded(
        iced::widget::image(handle)
            .width(Length::Shrink)
            .height(Length::Shrink)
            .content_fit(iced::ContentFit::Contain),
    )
}

/// The room an image `width` by `height` pixels takes once drawn, held while
/// its pixels are read again so the conversation keeps its place.
pub(in crate::app::chat_ui) fn room(width: u32, height: u32) -> Element<'static, Message> {
    bounded(
        widget::container(Element::new(Room(Size::new(width as f32, height as f32)))).class(
            design::surface(Some(design::sunken), [design::RADIUS_S; 4], false),
        ),
    )
}

fn bounded<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    widget::container(content).max_height(MAX_HEIGHT).into()
}

/// Space of an image's natural size, fit within its limits as an image of
/// that size shrinking to them would be.
struct Room(Size);

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Room {
    fn tag(&self) -> tree::Tag {
        tree::Tag::stateless()
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(
        &mut self,
        _: &mut Tree,
        _: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // The same steps an image of this size takes with `Shrink` and
        // `Contain`, so the room matches the preview that replaces it.
        let bounds = limits.resolve(Length::Shrink, Length::Shrink, self.0);
        let fit = iced::ContentFit::Contain.fit(self.0, bounds);
        layout::Node::new(Size::new(
            bounds.width.min(fit.width),
            bounds.height.min(fit.height),
        ))
    }

    fn draw(
        &self,
        _: &Tree,
        _: &mut cosmic::Renderer,
        _: &cosmic::Theme,
        _: &renderer::Style,
        _: Layout<'_>,
        _: mouse::Cursor,
        _: &Rectangle,
    ) {
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn laid_out(mut element: Element<'static, Message>, width: f32) -> Size {
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(element.as_widget());
        element
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(width, 4_000.0)),
            )
            .size()
    }

    fn pixels(width: u32, height: u32) -> iced::widget::image::Handle {
        iced::widget::image::Handle::from_rgba(
            width,
            height,
            vec![0; (width * height * 4) as usize],
        )
    }

    #[test]
    fn an_image_keeps_its_ratio_within_the_width_and_height_cap_and_is_never_enlarged() {
        for (width, height, available, drawn) in [
            (200, 100, 600.0, Size::new(200.0, 100.0)),
            (2400, 300, 600.0, Size::new(600.0, 75.0)),
            (400, 2000, 600.0, Size::new(72.0, MAX_HEIGHT)),
            (1600, 900, 600.0, Size::new(600.0, 337.5)),
            (1600, 900, 1200.0, Size::new(640.0, MAX_HEIGHT)),
        ] {
            let size = laid_out(image(pixels(width, height)), available);
            assert!(
                (size.width - drawn.width).abs() < 0.5 && (size.height - drawn.height).abs() < 0.5,
                "{width}x{height} in {available}: {size:?}, not {drawn:?}"
            );
            assert_eq!(
                laid_out(room(width, height), available),
                size,
                "{width}x{height} keeps its room while it is read again"
            );
        }
    }
}
