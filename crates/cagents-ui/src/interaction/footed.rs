//! A body above a footer that is measured first, so the footer keeps its
//! height however short the space is and the body takes only what remains.
//!
//! A shrinking column lays its children out in order, so a tall body would
//! take all the height before the footer is measured and push the footer out
//! of its parent. A dialog's actions must never go where the person cannot
//! reach them.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::{Event, Length, Point, Rectangle, Size, Vector, mouse};
use cosmic::{Element, Renderer, Theme};

/// `body` above `footer`, both as wide as the space. The footer is laid out
/// first, with the whole height; the body gets what is left. The pair is as
/// tall as its parts, so a short body leaves no gap above the footer.
pub fn footed<'a, Message: 'a>(
    body: impl Into<Element<'a, Message>>,
    footer: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Footed {
        children: [body.into(), footer.into()],
    })
}

struct Footed<'a, Message> {
    /// The body, then the footer: the order people read, tab and hear them.
    children: [Element<'a, Message>; 2],
}

impl<Message> Widget<Message, Theme, Renderer> for Footed<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let limits = limits.width(Length::Fill).height(Length::Shrink);
        let max = limits.max();
        let [body, footer] = &mut self.children;
        let [body_tree, footer_tree] = &mut tree.children[..] else {
            unreachable!("a footed layout has two children");
        };
        let footer_limits = layout::Limits::with_compression(Size::ZERO, max, limits.compression());
        let mut footer = footer
            .as_widget_mut()
            .layout(footer_tree, renderer, &footer_limits);
        let body_limits = layout::Limits::with_compression(
            Size::ZERO,
            Size::new(max.width, (max.height - footer.size().height).max(0.0)),
            limits.compression(),
        );
        let body = body
            .as_widget_mut()
            .layout(body_tree, renderer, &body_limits);
        footer.move_to_mut(Point::new(0.0, body.size().height));
        let size = limits.resolve(
            Length::Fill,
            Length::Shrink,
            Size::new(
                body.size().width.max(footer.size().width),
                body.size().height + footer.size().height,
            ),
        );
        layout::Node::with_children(size, vec![body, footer])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((child, tree), child_layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child.as_widget_mut().operate(
                    tree,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    operation,
                );
            }
        });
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
        for ((child, tree), child_layout) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget_mut().update(
                tree,
                event,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, tree), child_layout)| {
                child.as_widget().mouse_interaction(
                    tree,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .max()
            .unwrap_or_default()
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
        if layout.bounds().intersection(viewport).is_none() {
            return;
        }
        for ((child, tree), child_layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child.as_widget().draw(
                tree,
                renderer,
                theme,
                style,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
                viewport,
            );
        }
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
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
        iced_accessibility::A11yTree::join(
            self.children
                .iter()
                .zip(&tree.children)
                .zip(layout.children())
                .map(|((child, tree), child_layout)| {
                    child.as_widget().a11y_nodes(
                        child_layout.with_virtual_offset(layout.virtual_offset()),
                        tree,
                        cursor,
                    )
                }),
        )
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        for ((child, tree), child_layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child.as_widget().drag_destinations(
                tree,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                renderer,
                destinations,
            );
        }
    }
}
