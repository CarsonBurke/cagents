//! A label with trailing controls that move beneath it when a line cannot
//! hold both.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::{Event, Length, Point, Rectangle, Size, Vector, mouse};
use cosmic::{Element, Renderer, Theme};

/// Lays out `label` with `controls` at the end of the same line while the
/// label keeps at least [`min_label`](LabeledControls::min_label) of width,
/// or its natural width if that is less. Otherwise the label takes the whole
/// line and the controls follow beneath it, aligned to the end and wrapped
/// onto further lines as needed.
///
/// Controls keep their natural widths and their order, so an action's keys
/// and buttons are never squeezed out of view by a long label, and a short
/// label is never wrapped to make room for controls that would fit.
pub fn labeled_controls<'a, Message: 'a>(
    label: impl Into<Element<'a, Message>>,
    controls: impl IntoIterator<Item = Element<'a, Message>>,
) -> LabeledControls<'a, Message> {
    let mut children = vec![label.into()];
    children.extend(controls);
    LabeledControls {
        children,
        min_label: 0.0,
        spacing: 0.0,
        label_gap: 0.0,
    }
}

pub struct LabeledControls<'a, Message> {
    /// The label, then each control in order.
    children: Vec<Element<'a, Message>>,
    min_label: f32,
    spacing: f32,
    label_gap: f32,
}

impl<Message> LabeledControls<'_, Message> {
    /// The narrowest the label may become beside the controls before they
    /// move beneath it.
    pub fn min_label(mut self, width: f32) -> Self {
        self.min_label = width;
        self
    }

    /// The gap between controls, and between lines of controls.
    pub fn spacing(mut self, spacing: f32) -> Self {
        self.spacing = spacing;
        self
    }

    /// The gap between the label and the controls beside or beneath it.
    pub fn label_gap(mut self, gap: f32) -> Self {
        self.label_gap = gap;
        self
    }
}

impl<'a, Message: 'a> From<LabeledControls<'a, Message>> for Element<'a, Message> {
    fn from(widget: LabeledControls<'a, Message>) -> Self {
        Element::new(widget)
    }
}

impl<Message> Widget<Message, Theme, Renderer> for LabeledControls<'_, Message> {
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
        let width = limits.max().width;
        let height = limits.max().height;
        let spacing = self.spacing;
        let label_gap = self.label_gap;
        let (label, controls) = self
            .children
            .split_first_mut()
            .expect("a label is always present");
        let (label_tree, control_trees) =
            tree.children.split_first_mut().expect("one tree per child");
        let loose = layout::Limits::new(Size::ZERO, Size::new(width, height));
        let mut nodes: Vec<layout::Node> = controls
            .iter_mut()
            .zip(control_trees.iter_mut())
            .map(|(control, tree)| control.as_widget_mut().layout(tree, renderer, &loose))
            .collect();
        let controls_width = nodes.iter().map(|node| node.size().width).sum::<f32>()
            + spacing * nodes.len().saturating_sub(1) as f32;
        let natural_label = label
            .as_widget_mut()
            .layout(label_tree, renderer, &loose)
            .size()
            .width;
        let beside = if nodes.is_empty() {
            width
        } else {
            width - controls_width - label_gap
        };

        if beside >= natural_label.min(self.min_label) {
            let label_node = label.as_widget_mut().layout(
                label_tree,
                renderer,
                &layout::Limits::new(Size::ZERO, Size::new(beside.max(0.0), height)),
            );
            let line = nodes
                .iter()
                .map(|node| node.size().height)
                .fold(label_node.size().height, f32::max);
            let label_height = label_node.size().height;
            let mut children =
                vec![label_node.move_to(Point::new(0.0, (line - label_height) / 2.0))];
            let mut x = width - controls_width;
            for node in &mut nodes {
                let size = node.size();
                node.move_to_mut(Point::new(x, (line - size.height) / 2.0));
                x += size.width + spacing;
            }
            children.append(&mut nodes);
            return layout::Node::with_children(Size::new(width, line), children);
        }

        let label_node = label.as_widget_mut().layout(label_tree, renderer, &loose);
        let mut y = label_node.size().height + label_gap;
        let mut children = vec![label_node];
        // Fill lines in order, then align each line to the end.
        let mut start = 0;
        while start < nodes.len() {
            let mut end = start;
            let mut used = 0.0;
            while end < nodes.len() {
                let next = nodes[end].size().width + if end > start { spacing } else { 0.0 };
                if end > start && used + next > width {
                    break;
                }
                used += next;
                end += 1;
            }
            let line = nodes[start..end]
                .iter()
                .map(|node| node.size().height)
                .fold(0.0, f32::max);
            let mut x = (width - used).max(0.0);
            for node in &mut nodes[start..end] {
                let size = node.size();
                node.move_to_mut(Point::new(x, y + (line - size.height) / 2.0));
                x += size.width + spacing;
            }
            y += line + spacing;
            start = end;
        }
        children.append(&mut nodes);
        // Without controls the label is always inline, so here `y` ends one
        // gap past the last line of controls.
        layout::Node::with_children(Size::new(width, y - spacing), children)
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
            for ((child, state), layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child
                    .as_widget_mut()
                    .operate(state, layout, renderer, operation);
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
        for ((child, state), layout) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget_mut().update(
                state, event, layout, cursor, renderer, clipboard, shell, viewport,
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
            .map(|((child, state), layout)| {
                child
                    .as_widget()
                    .mouse_interaction(state, layout, cursor, viewport, renderer)
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
        for ((child, state), layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .filter(|(_, layout)| layout.bounds().intersects(viewport))
        {
            child
                .as_widget()
                .draw(state, renderer, theme, style, layout, cursor, viewport);
        }
    }

    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
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
                .zip(layout.children())
                .zip(&tree.children)
                .map(|((child, layout), state)| {
                    child.as_widget().a11y_nodes(layout, state, cursor)
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::accessibility_probe::headless_renderer;
    use cosmic::widget;

    fn control(width: f32) -> Element<'static, ()> {
        widget::Space::new()
            .width(Length::Fixed(width))
            .height(Length::Fixed(20.0))
            .into()
    }

    /// The label's bounds, then each control's, in a line `width` wide.
    fn lay_out(label: &'static str, controls: &[f32], width: f32) -> Vec<Rectangle> {
        let renderer = headless_renderer();
        let mut element: Element<'static, ()> = labeled_controls(
            crate::design::body(label),
            controls.iter().map(|width| control(*width)),
        )
        .min_label(120.0)
        .spacing(4.0)
        .label_gap(8.0)
        .into();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 1000.0)),
        );
        node.children().iter().map(layout::Node::bounds).collect()
    }

    #[test]
    fn controls_end_the_line_while_the_label_keeps_its_room() {
        let bounds = lay_out("Find in pane", &[80.0, 20.0, 20.0], 400.0);
        let [label, keys, record, reset] = bounds[..] else {
            panic!("{bounds:?}");
        };
        assert_eq!(reset.x + reset.width, 400.0);
        assert_eq!(record.x + record.width + 4.0, reset.x);
        assert_eq!(keys.x + keys.width + 4.0, record.x);
        assert!(label.x + label.width <= keys.x, "{bounds:?}");
        assert!(
            (label.center_y() - keys.center_y()).abs() < 0.5,
            "{bounds:?}"
        );
    }

    #[test]
    fn controls_move_beneath_and_wrap_in_order_when_the_line_is_full() {
        let bounds = lay_out("Previous workspace", &[120.0, 140.0, 44.0], 200.0);
        let [label, first, second, buttons] = bounds[..] else {
            panic!("{bounds:?}");
        };
        assert!(first.y >= label.y + label.height, "{bounds:?}");
        assert!(second.y >= first.y + first.height, "{bounds:?}");
        assert_eq!(second.y, buttons.y, "{bounds:?}");
        assert_eq!(buttons.x + buttons.width, 200.0);
        assert_eq!(second.x + second.width + 4.0, buttons.x);
        assert_eq!(first.x + first.width, 200.0);
    }
}
