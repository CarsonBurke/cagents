//! A pane-wide transcript viewport containing one stable reading column.
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree},
};
use iced::{Event, Point, Rectangle, Size, Vector};

// The pane's trailing splitter has an overlapping pointer target. Keep the
// native thumb clear of it without changing the reading column's centre.
pub(super) const TRANSCRIPT_RESIZE_INSET: f32 = 4.0;

pub(super) fn reading_column<'a>(content: Element<'a, Message>) -> Element<'a, Message> {
    widget::container(
        widget::container(content)
            .width(Length::Fill)
            .max_width(design::READABLE_MEASURE + 2.0 * step::GAP_M),
    )
    .center_x(Length::Fill)
    .into()
}

pub(super) fn transcript_viewport<'a>(content: Element<'a, Message>) -> Element<'a, Message> {
    widget::container(content)
        .padding(iced::Padding::ZERO.right(TRANSCRIPT_RESIZE_INSET))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

pub(super) fn transcript_column<'a>(content: Element<'a, Message>) -> Element<'a, Message> {
    Element::new(TranscriptColumn { content })
}

/// Replaces the transcript's padded container, keeping its single child and
/// layout depth. The last four pixels at narrow widths contain only padding;
/// text, row identities and wrapping retain the original pane-local measure.
struct TranscriptColumn<'a> {
    content: Element<'a, Message>,
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for TranscriptColumn<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let viewport_width = limits.max().width.max(0.0);
        let pane_width = viewport_width + TRANSCRIPT_RESIZE_INSET;
        let column_width = pane_width.min(design::READABLE_MEASURE + 2.0 * step::GAP_M);
        let padding = 2.0 * step::GAP_M;
        let content_width = (column_width - padding).max(0.0);
        let mut content = self.content.as_widget_mut().layout(
            &mut tree.children[0],
            renderer,
            &layout::Limits::new(
                Size::new(content_width, (limits.min().height - padding).max(0.0)),
                Size::new(content_width, (limits.max().height - padding).max(0.0)),
            ),
        );
        let size = limits.resolve(
            Length::Fill,
            Length::Shrink,
            Size::new(viewport_width, content.size().height + padding),
        );
        content.move_to_mut(Point::new(
            (pane_width - column_width) / 2.0 + step::GAP_M,
            step::GAP_M,
        ));
        layout::Node::with_children(size, vec![content])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
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
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &cosmic::Renderer,
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
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
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
        renderer: &cosmic::Renderer,
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

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, cosmic::Theme, cosmic::Renderer>> {
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

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content.as_widget().a11y_nodes(
            layout
                .children()
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            &tree.children[0],
            cursor,
        )
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        destinations: &mut iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout
                .children()
                .next()
                .unwrap()
                .with_virtual_offset(layout.virtual_offset()),
            renderer,
            destinations,
        );
    }
}

pub(super) fn pane<'a>(
    body: Element<'a, Message>,
    footer: Element<'a, Message>,
) -> Element<'a, Message> {
    Element::new(Pane {
        children: [body, footer],
    })
}

struct Pane<'a> {
    children: [Element<'a, Message>; 2],
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Pane<'_> {
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
    }
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // The composer stays at its original readable width while the body's
        // native scroll viewport reaches the pane edge.
        // A permission request, resumed conversation or wider status label must
        // never move the column. The footer's flex row wraps within that room.
        let body_width = limits.max().width.max(0.0);
        let width = limits
            .max()
            .width
            .min(design::READABLE_MEASURE + 2.0 * step::GAP_M)
            .max(0.0);
        let mut footer = self.children[1].as_widget_mut().layout(
            &mut tree.children[1],
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, limits.max().height)),
        );
        let body_height = (limits.max().height - footer.size().height - step::GAP_S).max(0.0);
        let mut body = self.children[0].as_widget_mut().layout(
            &mut tree.children[0],
            renderer,
            &layout::Limits::new(
                Size::new(body_width, body_height),
                Size::new(body_width, body_height),
            ),
        );
        let size = limits.resolve(
            Length::Fill,
            Length::Fill,
            Size::new(body_width, body_height + step::GAP_S + footer.size().height),
        );
        let x = (size.width - width) / 2.0;
        body.move_to_mut(Point::ORIGIN);
        footer.move_to_mut(Point::new(x, body_height + step::GAP_S));
        layout::Node::with_children(size, vec![body, footer])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.traverse(&mut |operation| {
            for ((child, state), child_layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child.as_widget_mut().operate(
                    state,
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
        renderer: &cosmic::Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        for ((child, state), child_layout) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget_mut().update(
                state,
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

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        for ((child, state), child_layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child.as_widget().draw(
                state,
                renderer,
                theme,
                style,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
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
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, state), child_layout)| {
                child.as_widget().mouse_interaction(
                    state,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .max()
            .unwrap_or_default()
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, cosmic::Theme, cosmic::Renderer>> {
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
                .map(|((child, state), child_layout)| {
                    child.as_widget().a11y_nodes(
                        child_layout.with_virtual_offset(layout.virtual_offset()),
                        state,
                        cursor,
                    )
                }),
        )
    }

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        destinations: &mut iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        for ((child, state), child_layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child.as_widget().drag_destinations(
                state,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                renderer,
                destinations,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contents_do_not_change_the_reading_column() {
        let renderer = crate::app::tests::headless_renderer();
        for available in [0.0_f32, 8.0, 24.0, 120.0, 280.0, 600.0, 956.0, 1440.0] {
            let expected = available.min(design::READABLE_MEASURE + 2.0 * step::GAP_M);
            for footer_width in [0.0, 150.25, 900.0, 3000.0] {
                let footer = widget::flex_row(vec![
                    widget::Space::new().width(footer_width).height(20).into(),
                    widget::Space::new().width(150).height(20).into(),
                ])
                .spacing(10)
                .into();
                let mut element = pane(
                    widget::Space::new()
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into(),
                    footer,
                );
                let mut tree = Tree::new(&element);
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(available, 600.0)),
                );
                let body = &node.children()[0];
                let footer = &node.children()[1];
                assert_eq!(body.size().width, available);
                assert_eq!(body.bounds().x, 0.0);
                assert_eq!(footer.bounds().x, (available - expected) / 2.0);
                assert!(footer.size().width <= expected);
                assert_eq!(footer.bounds().y + footer.size().height, 600.0);
            }
        }
    }

    #[test]
    fn pane_edge_transcript_keeps_original_text_wrap_and_control_bounds() {
        use iced::advanced::widget::operation;

        struct Control {
            id: widget::Id,
            focus: bool,
            focused: Option<bool>,
            bounds: Option<Rectangle>,
        }
        impl Operation for Control {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: Rectangle,
                state: &mut dyn operation::Focusable,
            ) {
                if id == Some(&self.id) {
                    if self.focus {
                        state.focus();
                    }
                    self.focused = Some(state.is_focused());
                    self.bounds = Some(bounds);
                }
            }
        }

        let renderer = crate::app::tests::headless_renderer();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let control = widget::Id::new("pane-edge-control");
        let content = || -> Element<'static, Message> {
            widget::column::with_children(vec![
                widget::container(
                    iced::widget::text("A long native conversation keeps the same readable line length when its scroll viewport reaches the pane edge. ".repeat(12))
                        .size(design::TEXT_PROSE)
                        .width(Length::Fill),
                )
                .id(widget::Id::new("pane-edge-text"))
                .width(Length::Fill)
                .into(),
                widget::button::custom(iced::widget::text("Copy"))
                    .id(control.clone())
                    .name("Pane edge fixture control")
                    .on_press(action(Action::Latest(target)))
                    .width(Length::Fill)
                    .into(),
            ])
            .width(Length::Fill)
            .into()
        };
        let mut current = transcript_column(content());
        let mut current_tree = Tree::new(&current);
        for (frame, pane_width) in [120.0_f32, 280.0, 600.0, 744.0, 748.0, 956.0, 1440.0]
            .into_iter()
            .enumerate()
        {
            let mut original: Element<'_, Message> = widget::container(content())
                .padding(step::GAP_M)
                .width(Length::Fill)
                .into();
            current = transcript_column(content());
            current_tree.diff(&mut current);
            let mut original_tree = Tree::new(&original);
            let original_node = original.as_widget_mut().layout(
                &mut original_tree,
                &renderer,
                &layout::Limits::new(
                    Size::ZERO,
                    Size::new(
                        pane_width.min(design::READABLE_MEASURE + 2.0 * step::GAP_M),
                        f32::INFINITY,
                    ),
                ),
            );
            let node = current.as_widget_mut().layout(
                &mut current_tree,
                &renderer,
                &layout::Limits::new(
                    Size::ZERO,
                    Size::new(pane_width - TRANSCRIPT_RESIZE_INSET, f32::INFINITY),
                ),
            );
            // Compare actual native paragraph/button layout with the previous
            // padded container, rather than duplicating the width formula.
            assert_eq!(node.size().height, original_node.size().height);
            assert_eq!(node.children().len(), original_node.children().len());
            let previous_column = &original_node.children()[0];
            let column = &node.children()[0];
            assert_eq!(column.size(), previous_column.size());
            assert_eq!(column.children().len(), 2);
            for (child, previous) in column.children().iter().zip(previous_column.children()) {
                assert_eq!(child.size(), previous.size(), "pane width {pane_width}");
                assert_eq!(child.bounds().y, previous.bounds().y);
                let right = column.bounds().x + child.bounds().x + child.size().width;
                assert!(
                    right <= node.size().width,
                    "text and controls remain inside the viewport at {pane_width}"
                );
            }
            // The direct column child and its rows are still at the same
            // layout depth used by legacy anchors and virtual row operations.
            assert_eq!(
                node.children()[0].children()[0].children().len(),
                original_node.children()[0].children()[0].children().len()
            );
            // Containers forward their child's Tree; the geometry widget owns
            // a child Tree. Exercise the semantic contract across real diffs:
            // the same named control keeps its native focus and hit bounds.
            let mut probe = Control {
                id: control.clone(),
                focus: frame == 0,
                focused: None,
                bounds: None,
            };
            current.as_widget_mut().operate(
                &mut current_tree,
                Layout::new(&node),
                &renderer,
                &mut probe,
            );
            assert_eq!(
                probe.focused,
                Some(true),
                "native focus survives resize at {pane_width}"
            );
            let bounds = probe.bounds.expect("same named native control");
            assert!(bounds.x >= 0.0 && bounds.x + bounds.width <= node.size().width);
            if frame == 6 {
                let key = iced::keyboard::Key::Named(iced::keyboard::key::Named::Enter);
                let mut messages = Vec::new();
                current.as_widget_mut().update(
                    &mut current_tree,
                    &Event::Keyboard(iced::keyboard::Event::KeyPressed {
                        key: key.clone(),
                        modified_key: key,
                        physical_key: iced::keyboard::key::Physical::Code(
                            iced::keyboard::key::Code::Enter,
                        ),
                        location: iced::keyboard::Location::Standard,
                        modifiers: iced::keyboard::Modifiers::empty(),
                        text: None,
                        repeat: false,
                    }),
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut iced::advanced::clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &node.bounds(),
                );
                assert!(
                    matches!(messages.as_slice(), [Message::ChatUi(Action::Latest(actual))] if *actual == target)
                );
            }
        }
    }

    #[test]
    fn transcript_column_retains_accessible_child_identity_and_virtual_offset_on_resize() {
        let renderer = crate::app::tests::headless_renderer();
        let mut element =
            transcript_column(Element::new(crate::ui_controls::accessibility_probe::Probe));
        let mut tree = Tree::new(&element);
        let mut identity = None;
        for width in [1440.0, 280.0, 956.0, 120.0, 1440.0] {
            element =
                transcript_column(Element::new(crate::ui_controls::accessibility_probe::Probe));
            tree.diff(&mut element);
            let node = element
                .as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(
                        Size::ZERO,
                        Size::new(width - TRANSCRIPT_RESIZE_INSET, 120.0),
                    ),
                )
                .move_to(Point::new(83.0, 47.0));
            let offset = Vector::new(0.0, 31.0);
            let accessible = element.as_widget().a11y_nodes(
                Layout::new(&node).with_virtual_offset(offset),
                &tree,
                mouse::Cursor::Unavailable,
            );
            let child = &accessible.root()[0];
            assert_eq!(child.node().label(), Some("Retained child"));
            if let Some(previous) = &identity {
                assert_eq!(previous, child.id());
            } else {
                identity = Some(child.id().clone());
            }
            let bounds = child.node().bounds().unwrap();
            let child_bounds = Layout::new(&node).children().next().unwrap().bounds();
            assert_eq!(bounds.x0, f64::from(child_bounds.x));
            assert_eq!(bounds.y0, f64::from(child_bounds.y - offset.y));
            assert_eq!(bounds.x1 - bounds.x0, f64::from(child_bounds.width));
        }
    }
}

#[cfg(test)]
#[path = "layout/viewport_input_tests.rs"]
mod viewport_input_tests;
