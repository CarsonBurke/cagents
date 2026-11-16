//! A passive glyph that tells assistive technology what it shows.

use std::borrow::Cow;

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector, mouse};
use cosmic::{Element, Renderer, Theme};
use iced_accessibility::accesskit::{Node, Rect, Role};

/// Names `content` for assistive technology with `label`.
///
/// A glyph whose meaning a pointer reads from a tooltip says nothing to a
/// screen reader, so a pane that drops a caption for the glyph alone keeps
/// the caption's words here. The content's own accessible nodes are replaced:
/// wrap only passive content, never a control.
pub fn labelled<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    label: impl Into<Cow<'a, str>>,
) -> Element<'a, Message> {
    Element::new(Labelled {
        content: content.into(),
        label: label.into(),
    })
}

struct Labelled<'a, Message> {
    content: Element<'a, Message>,
    label: Cow<'a, str>,
}

impl<Message> Widget<Message, Theme, Renderer> for Labelled<'_, Message> {
    // The accessible node keeps one identity across frames.
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<iced_accessibility::Id>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(iced_accessibility::Id::unique())
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
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
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let bounds = layout.bounds() - layout.virtual_offset();
        let mut node = Node::new(Role::Image);
        node.set_label(self.label.to_string());
        node.set_bounds(Rect::new(
            f64::from(bounds.x),
            f64::from(bounds.y),
            f64::from(bounds.x + bounds.width),
            f64::from(bounds.y + bounds.height),
        ));
        iced_accessibility::A11yTree::leaf(
            node,
            tree.state.downcast_ref::<iced_accessibility::Id>().clone(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::accessibility_probe::headless_renderer;

    #[test]
    fn a_labelled_glyph_is_named_where_it_is_drawn_and_keeps_its_identity() {
        let renderer = headless_renderer();
        let mut element: Element<'_, ()> = labelled(
            crate::ui_controls::passive_icon(crate::ui_controls::Icon::Avatar.handle(), 16),
            "Account: Work",
        );
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 100.0)),
        );
        let named = |tree: &Tree| {
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), tree, mouse::Cursor::Unavailable)
        };
        let first = named(&tree);
        let [root] = first.root().as_slice() else {
            panic!("one accessible node: {first:?}");
        };
        assert!(first.children().is_empty(), "{first:?}");
        assert_eq!(root.node().label(), Some("Account: Work"));
        assert_eq!(root.node().role(), Role::Image);
        let drawn = root.node().bounds().expect("bounds");
        assert_eq!((drawn.width(), drawn.height()), (16.0, 16.0));
        assert_eq!(
            named(&tree).root()[0].id(),
            root.id(),
            "stable across frames"
        );
    }
}
