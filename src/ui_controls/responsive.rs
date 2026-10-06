//! Accessibility-preserving responsive layout, backported from the pinned Iced widget.
use core::layout::{self, Layout};
use core::mouse;
use core::overlay;
use core::renderer;
use core::widget;
use core::widget::Tree;
use core::{Clipboard, Shell, Widget};
use cosmic::iced::advanced as core;
use cosmic::iced::widget::space;
use cosmic::iced::{Element, Event, Length, Rectangle, Size, Vector};

/// A widget that is aware of its dimensions.
///
/// A [`Responsive`] widget will always try to fill all the available space of
/// its parent.
pub struct Responsive<'a, Message, Theme = cosmic::Theme, Renderer = cosmic::Renderer> {
    view: Box<dyn Fn(Size) -> Element<'a, Message, Theme, Renderer> + 'a>,
    width: Length,
    height: Length,
    content: Element<'a, Message, Theme, Renderer>,
}

impl<'a, Message, Theme, Renderer> Responsive<'a, Message, Theme, Renderer>
where
    Renderer: core::Renderer,
{
    /// Creates a new [`Responsive`] widget with a closure that produces its
    /// contents.
    ///
    /// The `view` closure will receive the maximum available space for
    /// the [`Responsive`] during layout. You can use this [`Size`] to
    /// conditionally build the contents.
    pub fn new(view: impl Fn(Size) -> Element<'a, Message, Theme, Renderer> + 'a) -> Self {
        Self {
            view: Box::new(view),
            width: Length::Fill,
            height: Length::Fill,
            content: Element::new(space()),
        }
    }

    /// Sets the width of the [`Responsive`].
    pub fn width(mut self, width: impl Into<Length>) -> Self {
        self.width = width.into();
        self
    }

    /// Sets the height of the [`Responsive`].
    pub fn height(mut self, height: impl Into<Length>) -> Self {
        self.height = height.into();
        self
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer>
    for Responsive<'_, Message, Theme, Renderer>
where
    Renderer: core::Renderer,
{
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let Some(child) = layout.children().next() else {
            return Default::default();
        };
        let Some(state) = tree.children.first() else {
            return Default::default();
        };
        self.content.as_widget().a11y_nodes(
            child.with_virtual_offset(layout.virtual_offset()),
            state,
            cursor,
        )
    }

    fn diff(&mut self, _tree: &mut Tree) {
        // Diff is deferred to layout
    }

    fn size(&self) -> Size<Length> {
        Size {
            width: self.width,
            height: self.height,
        }
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let limits = limits.width(self.width).height(self.height);
        let size = limits.max();

        self.content = (self.view)(size);
        tree.diff_children(std::slice::from_mut(&mut self.content));

        let node =
            self.content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, &limits.loose());

        let size = limits.resolve(self.width, self.height, node.size());

        layout::Node::with_children(size, vec![node])
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
            layout
                .children()
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
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
            layout
                .children()
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            cursor,
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
            layout
                .children()
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            cursor,
            viewport,
            renderer,
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn widget::Operation,
    ) {
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout
                .children()
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            renderer,
            operation,
        );
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout
                .children()
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            renderer,
            viewport,
            translation,
        )
    }

    fn drag_destinations(
        &self,
        state: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        dnd_rectangles: &mut core::clipboard::DndDestinationRectangles,
    ) {
        if let Some(l) = layout.children().next() {
            self.content.as_widget().drag_destinations(
                &state.children[0],
                l.with_virtual_offset(layout.virtual_offset()),
                renderer,
                dnd_rectangles,
            );
        }
    }
}

impl<'a, Message, Theme, Renderer> From<Responsive<'a, Message, Theme, Renderer>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: core::Renderer + 'a,
{
    fn from(responsive: Responsive<'a, Message, Theme, Renderer>) -> Self {
        Self::new(responsive)
    }
}

pub fn responsive<'a, Message: 'a>(
    view: impl Fn(Size) -> cosmic::Element<'a, Message> + 'a,
) -> Responsive<'a, Message> {
    Responsive::new(view)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn responsive_preserves_accessibility_after_layout_and_retained_diffs() {
        let mut responsive =
            Responsive::<()>::new(|_| Element::new(super::super::accessibility_probe::Probe));
        let mut tree = Tree::empty();
        let node = layout::Node::with_children(
            Size::new(80.0, 40.0),
            vec![layout::Node::new(Size::new(80.0, 40.0))],
        );
        let bounds = Layout::new(&node).with_virtual_offset(Vector::new(0.0, 37.0));
        assert!(
            responsive
                .a11y_nodes(bounds, &tree, mouse::Cursor::Unavailable)
                .root()
                .is_empty()
        );
        responsive.content = (responsive.view)(Size::new(80.0, 40.0));
        tree.diff_children(std::slice::from_mut(&mut responsive.content));
        let first = responsive.a11y_nodes(bounds, &tree, mouse::Cursor::Unavailable);
        responsive.content = (responsive.view)(Size::new(100.0, 60.0));
        tree.diff_children(std::slice::from_mut(&mut responsive.content));
        let second = responsive.a11y_nodes(bounds, &tree, mouse::Cursor::Unavailable);
        assert_eq!(first.root()[0].id(), second.root()[0].id());
        assert_eq!(second.root()[0].node().label(), Some("Retained child"));
    }
}
