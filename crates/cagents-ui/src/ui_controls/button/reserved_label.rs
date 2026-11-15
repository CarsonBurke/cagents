//! Measure alternate labels without drawing or announcing them.

use std::borrow::Cow;

use cosmic::iced::advanced::{
    Layout, Widget, layout, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::{Length, Point, Rectangle, Size, mouse};
use cosmic::{Element, Renderer, Theme};

use crate::design;

pub(super) fn view<'a, Message: 'a>(
    current: Cow<'a, str>,
    alternatives: Vec<Cow<'a, str>>,
) -> Element<'a, Message> {
    let heading = |label| {
        design::heading(label)
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .into()
    };
    if alternatives.is_empty() {
        heading(current)
    } else {
        Element::new(ReservedLabel {
            labels: std::iter::once(current)
                .chain(alternatives)
                .map(heading)
                .collect(),
        })
    }
}

struct ReservedLabel<'a, Message> {
    // Only the first label is visible and accessible. The others participate
    // in layout with their own cached text state, never as transparent text.
    labels: Vec<Element<'a, Message>>,
}

impl<Message> Widget<Message, Theme, Renderer> for ReservedLabel<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        self.labels.iter().map(Tree::new).collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.labels);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let nodes: Vec<_> = self
            .labels
            .iter_mut()
            .zip(&mut tree.children)
            .map(|(label, state)| {
                label
                    .as_widget_mut()
                    .layout(state, renderer, &limits.loose())
            })
            .collect();
        let width = nodes
            .iter()
            .map(|node| node.size().width)
            .fold(0.0, f32::max);
        let mut current = nodes
            .into_iter()
            .next()
            .expect("the current label is present");
        let size = limits.resolve(
            Length::Shrink,
            Length::Shrink,
            Size::new(width, current.size().height),
        );
        current.move_to_mut(Point::new((size.width - current.size().width) / 2.0, 0.0));
        layout::Node::with_children(size, vec![current])
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
        self.labels[0].as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout
                .children()
                .next()
                .expect("the visible label is laid out")
                .with_virtual_offset(layout.virtual_offset()),
            cursor,
            viewport,
        );
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        self.labels[0].as_widget_mut().operate(
            &mut tree.children[0],
            layout
                .children()
                .next()
                .expect("the visible label is laid out")
                .with_virtual_offset(layout.virtual_offset()),
            renderer,
            operation,
        );
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.labels[0].as_widget().a11y_nodes(
            layout
                .children()
                .next()
                .expect("the visible label is laid out")
                .with_virtual_offset(layout.virtual_offset()),
            &tree.children[0],
            cursor,
        )
    }
}
