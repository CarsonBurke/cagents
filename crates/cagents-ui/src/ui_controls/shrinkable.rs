//! A row child that gives up width to its siblings instead of taking theirs.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree},
};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector, mouse};
use cosmic::{Element, Renderer, Theme};

/// Lets `content` narrow into the width its row's other children leave, and
/// never widens it past what it needs.
///
/// A row lays its shrinking children out in order, so a long label early in
/// the row takes the width a later one needs. This child is measured after
/// them instead: it reports a fill width, so its row also fills, then keeps
/// its natural width when that fits. Text inside it should ellipsize.
///
/// Several shrinkables in one row split what is left by fill factor, evenly,
/// not by need: a short one leaves its unused share empty while a long one
/// is cut at its own share. Give a row one shrinkable, the level most likely
/// to run long.
pub fn shrinkable<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Shrinkable {
        content: content.into(),
    })
}

struct Shrinkable<'a, Message> {
    content: Element<'a, Message>,
}

impl<Message> Widget<Message, Theme, Renderer> for Shrinkable<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: Length::Fill,
            height: self.content.as_widget().size().height,
        }
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // A row hands a filling child its whole share as a minimum too.
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, &limits.loose())
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout,
            renderer,
            destinations,
        );
    }

    fn id(&self) -> Option<Id> {
        self.content.as_widget().id()
    }

    fn set_id(&mut self, id: Id) {
        self.content.as_widget_mut().set_id(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::accessibility_probe::headless_renderer;
    use cosmic::widget;

    fn label(text: &str) -> Element<'_, ()> {
        crate::design::body(text)
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .ellipsize(cosmic::iced::widget::text::Ellipsize::End(
                cosmic::iced::advanced::text::EllipsizeHeightLimit::Lines(1),
            ))
            .into()
    }

    /// Each child's bounds in a row `width` wide.
    fn lay_out(mut row: Element<'_, ()>, width: f32) -> Vec<Rectangle> {
        let renderer = headless_renderer();
        let mut tree = Tree::new(row.as_widget());
        let node = row.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 100.0)),
        );
        node.children().iter().map(layout::Node::bounds).collect()
    }

    #[test]
    fn a_shrinkable_keeps_its_width_when_it_fits_and_yields_when_it_does_not() {
        const LONG: &str = "A label long enough to need shortening in a narrow row";
        let trail = || {
            widget::row::with_capacity(2)
                .spacing(4.0)
                .push(shrinkable(label(LONG)))
                .push(label("End"))
                .into()
        };
        let natural = lay_out(
            widget::row::with_children(vec![label(LONG), label("End")])
                .spacing(4.0)
                .into(),
            2000.0,
        );

        let roomy = lay_out(trail(), 2000.0);
        assert_eq!(
            roomy[0].width, natural[0].width,
            "it keeps its natural width"
        );
        assert_eq!(
            roomy[1].x, natural[1].x,
            "and the next child follows it rather than the row's end"
        );

        let narrow = lay_out(trail(), 120.0);
        assert!(
            narrow[0].width < natural[0].width,
            "it gives up width: {narrow:?}"
        );
        assert_eq!(
            narrow[1].width, natural[1].width,
            "the child after it keeps its whole label"
        );
        assert!(narrow[1].x + narrow[1].width <= 120.0, "{narrow:?}");
    }

    #[test]
    fn a_shrinkable_carries_its_contents_identity() {
        let id = widget::Id::new("shrinkable.content");
        let mut wrapped = shrinkable(
            widget::button::custom(label("Named"))
                .id(id.clone())
                .on_press(()),
        );
        assert_eq!(wrapped.as_widget().id(), Some(id));
        let other = widget::Id::new("shrinkable.other");
        wrapped.as_widget_mut().set_id(other.clone());
        assert_eq!(wrapped.as_widget().id(), Some(other));
    }
}
