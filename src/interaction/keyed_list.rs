//! Stable child state with the standard layout's native accessibility tree.
use super::*;

pub(super) fn view<'a, Message: 'a>(
    rows: Vec<([u8; 32], Element<'a, Message>)>,
) -> Element<'a, Message> {
    let (keys, children): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
    Element::new(KeyedList {
        keys,
        content: widget::column::with_children(children)
            .width(Length::Fill)
            .into(),
    })
}

pub(super) fn horizontal<'a, Message: 'a>(
    rows: Vec<([u8; 32], Element<'a, Message>)>,
    spacing: f32,
) -> Element<'a, Message> {
    let (keys, children): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
    Element::new(KeyedList {
        keys,
        content: widget::row::with_children(children)
            .spacing(spacing)
            .align_y(cosmic::iced::Alignment::Center)
            .into(),
    })
}

struct Keys(Vec<[u8; 32]>);

struct KeyedList<'a, Message> {
    keys: Vec<[u8; 32]>,
    content: Element<'a, Message>,
}

impl<Message> Widget<Message, Theme, Renderer> for KeyedList<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Keys>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Keys(self.keys.clone()))
    }
    fn diff(&mut self, tree: &mut Tree) {
        let previous = &mut tree.state.downcast_mut::<Keys>().0;
        // Consume each retained tree once, in original order for duplicate
        // keys. Direct tab strips may contain many children, so index once
        // instead of searching the previous tree for every new control.
        let mut retained: std::collections::HashMap<_, std::collections::VecDeque<_>> =
            std::collections::HashMap::with_capacity(previous.len());
        for (key, child) in previous
            .iter()
            .copied()
            .zip(std::mem::take(&mut tree.children[0].children))
        {
            retained.entry(key).or_default().push_back(child);
        }
        tree.children[0].children = self
            .keys
            .iter()
            .map(|key| {
                retained
                    .get_mut(key)
                    .and_then(|rows| rows.pop_front())
                    .unwrap_or_else(Tree::empty)
            })
            .collect();
        previous.clone_from(&self.keys);
        // Removed row owners drop here; no shadow widget tree is retained.
        drop(retained);
        self.content.as_widget_mut().diff(&mut tree.children[0]);
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
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout.with_virtual_offset(layout.virtual_offset()),
            renderer,
            operation,
        );
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
            layout.with_virtual_offset(layout.virtual_offset()),
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
            layout.with_virtual_offset(layout.virtual_offset()),
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
            layout.with_virtual_offset(layout.virtual_offset()),
            cursor,
            viewport,
        )
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
            layout.with_virtual_offset(layout.virtual_offset()),
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
            layout.with_virtual_offset(layout.virtual_offset()),
            &tree.children[0],
            cursor,
        )
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
            layout.with_virtual_offset(layout.virtual_offset()),
            renderer,
            destinations,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(keys: &[u8], row: bool) -> Element<'static, &'static str> {
        let controls = keys
            .iter()
            .map(|key| {
                let label = match key {
                    1 => "One",
                    2 => "Two",
                    3 => "Three",
                    4 => "Four",
                    _ => unreachable!(),
                };
                let mut identity = [0; 32];
                identity[0] = *key;
                (
                    identity,
                    widget::button::text(label)
                        .name(label)
                        .on_press(label)
                        .width(Length::Fill)
                        .into(),
                )
            })
            .collect();
        if row {
            horizontal(controls, space::XS)
        } else {
            view(controls)
        }
    }

    fn layout_rows(
        element: &mut Element<'_, &'static str>,
        tree: &mut Tree,
        renderer: &Renderer,
    ) -> layout::Node {
        element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(260.0, 400.0)),
        )
    }

    #[derive(Default)]
    struct Focused {
        index: usize,
        focused: Vec<usize>,
    }
    impl Operation for Focused {
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
            operate(self);
        }
        fn focusable(
            &mut self,
            _id: Option<&Id>,
            _bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if state.is_focused() {
                self.focused.push(self.index);
            }
            self.index += 1;
        }
    }
    fn focused(
        element: &mut Element<'_, &'static str>,
        tree: &mut Tree,
        node: &layout::Node,
        renderer: &Renderer,
    ) -> Vec<usize> {
        let mut operation = Focused::default();
        element
            .as_widget_mut()
            .operate(tree, Layout::new(node), renderer, &mut operation);
        operation.focused
    }
    fn labels(
        element: &Element<'_, &'static str>,
        tree: &Tree,
        node: &layout::Node,
    ) -> Vec<String> {
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(node).with_virtual_offset(Vector::new(0.0, 17.0)),
            tree,
            mouse::Cursor::Unavailable,
        );
        accessible
            .root()
            .iter()
            .chain(accessible.children())
            .filter_map(|node| {
                (node.node().role() == iced_accessibility::accesskit::Role::Button)
                    .then(|| node.node().label().map(str::to_owned))
                    .flatten()
            })
            .collect()
    }

    #[test]
    fn standard_column_accessibility_and_focused_row_follow_keys_through_reorder_and_removal() {
        retained_focus(false);
    }

    #[test]
    fn standard_row_accessibility_and_focused_control_follow_keys_through_reorder_and_removal() {
        retained_focus(true);
    }

    #[test]
    fn duplicate_keys_consume_each_focused_tree_once_in_original_order() {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut element = rows(&[1, 2, 2, 3], true);
        let mut tree = Tree::new(element.as_widget());
        let node = layout_rows(&mut element, &mut tree, &renderer);
        let second_two = tree.children[0].children[2].id.clone().unwrap();
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(second_two),
        );
        let mut reordered = rows(&[2, 1, 3, 2], true);
        tree.diff(reordered.as_widget_mut());
        let node = layout_rows(&mut reordered, &mut tree, &renderer);
        assert_eq!(focused(&mut reordered, &mut tree, &node, &renderer), [3]);
        let mut removed = rows(&[2, 1, 3], true);
        tree.diff(removed.as_widget_mut());
        let node = layout_rows(&mut removed, &mut tree, &renderer);
        assert!(focused(&mut removed, &mut tree, &node, &renderer).is_empty());
    }

    #[test]
    fn many_controls_keep_each_native_identity_during_reverse_reorder() {
        let controls = |reverse: bool| {
            let mut indices: Vec<u64> = (0..512).collect();
            if reverse {
                indices.reverse();
            }
            horizontal(
                indices
                    .into_iter()
                    .map(|index| {
                        let mut key = [0; 32];
                        key[..8].copy_from_slice(&index.to_le_bytes());
                        (
                            key,
                            widget::button::text(index.to_string())
                                .on_press(index)
                                .into(),
                        )
                    })
                    .collect(),
                space::XS,
            )
        };
        let element = controls(false);
        let mut tree = Tree::new(element.as_widget());
        let original: Vec<_> = tree.children[0]
            .children
            .iter()
            .map(|child| child.id.clone())
            .collect();
        let mut reversed = controls(true);
        tree.diff(reversed.as_widget_mut());
        let actual: Vec<_> = tree.children[0]
            .children
            .iter()
            .map(|child| child.id.clone())
            .collect();
        assert_eq!(actual, original.into_iter().rev().collect::<Vec<_>>());
    }

    fn retained_focus(horizontal: bool) {
        use cosmic::iced::{advanced::clipboard, keyboard};
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut element = rows(&[1, 2, 3], horizontal);
        let mut tree = Tree::new(element.as_widget());
        let node = layout_rows(&mut element, &mut tree, &renderer);
        // Use the real focused button's generated ID, not a named ID that
        // could independently recover its state through iced's global map.
        let two = tree.children[0].children[1].id.clone().unwrap();
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(two),
        );
        assert_eq!(focused(&mut element, &mut tree, &node, &renderer), [1]);
        assert_eq!(labels(&element, &tree, &node), ["One", "Two", "Three"]);
        let before =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let two_ax = before
            .root()
            .iter()
            .chain(before.children())
            .find(|node| node.node().label() == Some("Two"))
            .unwrap()
            .id()
            .clone();

        let mut reordered = rows(&[3, 1, 2], horizontal);
        tree.diff(reordered.as_widget_mut());
        let node = layout_rows(&mut reordered, &mut tree, &renderer);
        assert_eq!(
            focused(&mut reordered, &mut tree, &node, &renderer),
            [2],
            "native focus must follow Two, rather than its former index"
        );
        assert_eq!(labels(&reordered, &tree, &node), ["Three", "One", "Two"]);
        let after =
            reordered
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        assert_eq!(
            after
                .root()
                .iter()
                .chain(after.children())
                .find(|node| node.node().label() == Some("Two"))
                .unwrap()
                .id(),
            &two_ax,
            "the native AX identity must follow its retained keyed row"
        );

        let event = Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Unidentified(
                keyboard::key::NativeCode::Unidentified,
            ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        let mut messages = Vec::new();
        reordered.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(Size::new(260.0, 400.0)),
        );
        assert_eq!(
            messages,
            ["Two"],
            "input must reach the retained focused real button"
        );

        let mut replaced = rows(&[3, 1, 4], horizontal);
        tree.diff(replaced.as_widget_mut());
        let node = layout_rows(&mut replaced, &mut tree, &renderer);
        assert!(
            focused(&mut replaced, &mut tree, &node, &renderer).is_empty(),
            "new Four must not inherit removed Two's focused state"
        );
        assert_eq!(labels(&replaced, &tree, &node), ["Three", "One", "Four"]);
    }
}
