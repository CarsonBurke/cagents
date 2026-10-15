//! Wrapped native decisions with the same child tree for input and accessibility.
//!
//! iced's current Row::Wrapping omits accessibility forwarding. Keep this
//! small answer-only layout local until upstream provides that contract.
use super::*;
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree}};
use iced::{Event, Rectangle, Size, Vector};

pub(super) fn row<'a>(children: Vec<Element<'a, Message>>) -> Element<'a, Message> {
    Element::new(Answers { children })
}
struct Answers<'a> { children: Vec<Element<'a, Message>> }
impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Answers<'_> {
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(|child| Tree::new(child.as_widget())).collect()
    }
    fn diff(&mut self, tree: &mut Tree) { tree.diff_children(&self.children); }
    fn size(&self) -> Size<Length> { Size::new(Length::Shrink, Length::Shrink) }
    fn layout(&mut self, tree: &mut Tree, renderer: &cosmic::Renderer, limits: &layout::Limits) -> layout::Node {
        let limits = limits.width(Length::Shrink).height(Length::Shrink);
        let child_limits = limits.loose();
        let mut children: Vec<layout::Node> = Vec::with_capacity(self.children.len());
        let (mut x, mut y, mut height, mut width) = (0.0f32, 0.0f32, 0.0f32, 0.0f32);
        let mut start = 0;
        let center = |children: &mut [layout::Node], height: f32| {
            for node in children { node.translate_mut(Vector::new(0.0, (height - node.size().height) / 2.0)); }
        };
        for (index, child) in self.children.iter_mut().enumerate() {
            let node = child.as_widget_mut().layout(&mut tree.children[index], renderer, &child_limits);
            let size = node.size();
            if x > 0.0 && x + size.width > limits.max().width {
                width = width.max(x - step::GAP_S);
                center(&mut children[start..], height);
                y += height + step::GAP_S;
                x = 0.0;
                height = 0.0;
                start = index;
            }
            height = height.max(size.height);
            children.push(node.move_to((x, y)));
            x += size.width + step::GAP_S;
        }
        if x > 0.0 { width = width.max(x - step::GAP_S); }
        center(&mut children[start..], height);
        layout::Node::with_children(limits.resolve(Length::Shrink, Length::Shrink, Size::new(width, y + height)), children)
    }
    fn operate(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &cosmic::Renderer, operation: &mut dyn Operation) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((child, tree), node) in self.children.iter_mut().zip(&mut tree.children).zip(layout.children()) {
                child.as_widget_mut().operate(tree, node.with_virtual_offset(layout.virtual_offset()), renderer, operation);
            }
        });
    }
    fn update(&mut self, tree: &mut Tree, event: &Event, layout: Layout<'_>, cursor: mouse::Cursor, renderer: &cosmic::Renderer, clipboard: &mut dyn Clipboard, shell: &mut Shell<'_, Message>, viewport: &Rectangle) {
        for ((child, tree), node) in self.children.iter_mut().zip(&mut tree.children).zip(layout.children()) {
            child.as_widget_mut().update(tree, event, node.with_virtual_offset(layout.virtual_offset()), cursor, renderer, clipboard, shell, viewport);
        }
    }
    fn mouse_interaction(&self, tree: &Tree, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle, renderer: &cosmic::Renderer) -> mouse::Interaction {
        self.children.iter().zip(&tree.children).zip(layout.children()).map(|((child, tree), node)| child.as_widget().mouse_interaction(tree, node.with_virtual_offset(layout.virtual_offset()), cursor, viewport, renderer)).max().unwrap_or_default()
    }
    fn draw(&self, tree: &Tree, renderer: &mut cosmic::Renderer, theme: &cosmic::Theme, style: &renderer::Style, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle) {
        for ((child, tree), node) in self.children.iter().zip(&tree.children).zip(layout.children()) {
            let node = node.with_virtual_offset(layout.virtual_offset());
            if node.bounds().intersects(viewport) { child.as_widget().draw(tree, renderer, theme, style, node, cursor, viewport); }
        }
    }
    fn overlay<'b>(&'b mut self, tree: &'b mut Tree, layout: Layout<'b>, renderer: &cosmic::Renderer, viewport: &Rectangle, translation: Vector) -> Option<overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>> {
        overlay::from_children(&mut self.children, tree, layout, renderer, viewport, translation)
    }
    fn a11y_nodes(&self, layout: Layout<'_>, tree: &Tree, cursor: mouse::Cursor) -> iced_accessibility::A11yTree {
        iced_accessibility::A11yTree::join(self.children.iter().zip(&tree.children).zip(layout.children()).map(|((child, tree), node)| child.as_widget().a11y_nodes(node.with_virtual_offset(layout.virtual_offset()), tree, cursor)))
    }
    fn drag_destinations(&self, tree: &Tree, layout: Layout<'_>, renderer: &cosmic::Renderer, destinations: &mut iced::advanced::clipboard::DndDestinationRectangles) {
        for ((child, tree), node) in self.children.iter().zip(&tree.children).zip(layout.children()) {
            child.as_widget().drag_destinations(tree, node.with_virtual_offset(layout.virtual_offset()), renderer, destinations);
        }
    }
}
