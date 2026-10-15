//! Cheap nested reader positions survive viewport and hidden-tree eviction.
use super::*;
use iced::Point;
use iced::advanced::clipboard::DndDestinationRectangles;

pub(super) type Key = (Option<String>, u64);
#[derive(Clone, Copy)]
pub(super) struct Saved {
    position: Point,
    at_end: bool,
    used: u64,
}
#[derive(Default)]
pub(super) struct Positions {
    pub(super) entries: HashMap<Key, Saved>,
    clock: u64,
}

pub(in crate::app::chat_ui) fn block<'a>(
    content: impl Into<Element<'a, Message>>,
    memory: Memory,
    context: Option<String>,
    identity: u64,
    bottom: bool,
) -> Element<'a, Message> {
    Element::new(Block {
        content: content.into(),
        memory,
        key: (context, identity),
        bottom,
    })
}
struct Block<'a> {
    content: Element<'a, Message>,
    memory: Memory,
    key: Key,
    bottom: bool,
}
struct Mounted(Option<Key>);
struct Position {
    restore: Option<Saved>,
    bottom: bool,
    saved: Option<Saved>,
    changed: bool,
}
impl Operation for Position {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        if self.saved.is_none() {
            visit(self);
        }
    }
    fn scrollable(
        &mut self,
        _: Option<&Id>,
        viewport: Rectangle,
        content: Rectangle,
        translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if self.saved.is_some() {
            return;
        }
        let travel = (content.height - viewport.height).max(0.0);
        if let Some(saved) = self.restore.take() {
            let top = if saved.at_end {
                travel
            } else {
                saved.position.y.min(travel)
            };
            let x = saved
                .position
                .x
                .min((content.width - viewport.width).max(0.0));
            if (x - translation.x).abs() > 0.5 || (top - translation.y).abs() > 0.5 {
                state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                    x: Some(x),
                    y: Some(if self.bottom { travel - top } else { top }),
                });
                self.changed = true;
            }
        }
        self.saved = Some(Saved {
            position: Point::new(translation.x, translation.y),
            at_end: travel - translation.y <= 0.5,
            used: 0,
        });
    }
}
impl Block<'_> {
    fn observe(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &cosmic::Renderer) {
        let mut read = Position {
            restore: None,
            bottom: self.bottom,
            saved: None,
            changed: false,
        };
        self.content
            .as_widget_mut()
            .operate(tree, layout, renderer, &mut read);
        if let Some(mut saved) = read.saved {
            let mut memory = self.memory.1.borrow_mut();
            memory.clock = memory.clock.wrapping_add(1);
            saved.used = memory.clock;
            if !memory.entries.contains_key(&self.key) && memory.entries.len() >= 1024 {
                if let Some(key) = memory
                    .entries
                    .iter()
                    .min_by_key(|(_, saved)| saved.used)
                    .map(|(key, _)| key.clone())
                {
                    memory.entries.remove(&key);
                }
            }
            memory.entries.insert(self.key.clone(), saved);
        }
    }
}
impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Block<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Mounted>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Mounted(None))
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        let mounted = tree.state.downcast_mut::<Mounted>();
        if mounted.0.as_ref().is_some_and(|key| key != &self.key) {
            // A reused tree belongs to another logical reader. Its default
            // is native top-left/bottom-follow, never the preceding key's offset.
            tree.children = vec![Tree::new(&self.content)];
            mounted.0 = None;
        } else {
            tree.diff_children(std::slice::from_mut(&mut self.content));
        }
    }
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let mut node = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        let mounted = tree.state.downcast_mut::<Mounted>();
        if mounted.0.as_ref() != Some(&self.key) {
            mounted.0 = Some(self.key.clone());
            let saved = self.memory.1.borrow().entries.get(&self.key).copied();
            let mut restore = Position {
                restore: saved,
                bottom: self.bottom,
                saved: None,
                changed: false,
            };
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Layout::new(&node),
                renderer,
                &mut restore,
            );
            if restore.changed {
                node = self
                    .content
                    .as_widget_mut()
                    .layout(&mut tree.children[0], renderer, limits);
            }
        }
        self.observe(&mut tree.children[0], Layout::new(&node), renderer);
        node
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
        self.observe(&mut tree.children[0], layout, renderer);
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
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        self.observe(&mut tree.children[0], layout, renderer);
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
            layout,
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
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>> {
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
        renderer: &cosmic::Renderer,
        rectangles: &mut DndDestinationRectangles,
    ) {
        self.content
            .as_widget()
            .drag_destinations(&tree.children[0], layout, renderer, rectangles);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Move;
    impl Operation for Move {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            _: Option<&Id>,
            _: Rectangle,
            _: Rectangle,
            _: Vector,
            state: &mut dyn operation::Scrollable,
        ) {
            state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                x: Some(100.0),
                y: Some(300.0),
            });
        }
    }
    #[test]
    fn nested_positions_survive_tree_eviction_without_retaining_widget_state() {
        let renderer = crate::app::tests::headless_renderer();
        let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 200.0));
        for bottom in [false, true] {
            let memory = Memory::default();
            let make = |context: Option<String>| {
                let scroll = widget::scrollable(widget::Space::new().width(2000).height(2000))
                    .direction(iced::widget::scrollable::Direction::Both {
                        vertical: design::scrollbar(),
                        horizontal: design::scrollbar(),
                    })
                    .width(Length::Fill)
                    .height(Length::Fill);
                block(
                    if bottom {
                        scroll.anchor_bottom()
                    } else {
                        scroll
                    },
                    memory.clone(),
                    context,
                    7,
                    bottom,
                )
            };
            let mut element = make(None);
            let mut tree = Tree::new(&element);
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut Move);
            let expected = memory.1.borrow().entries[&(None, 7)];
            assert_eq!(expected.position.x, 100.0);
            assert!(!expected.at_end);
            drop(tree);
            drop(element);
            let mut revealed = make(None);
            let mut fresh = Tree::new(&revealed);
            let node = revealed
                .as_widget_mut()
                .layout(&mut fresh, &renderer, &limits);
            let mut read = Position {
                restore: None,
                bottom,
                saved: None,
                changed: false,
            };
            revealed
                .as_widget_mut()
                .operate(&mut fresh, Layout::new(&node), &renderer, &mut read);
            assert_eq!(read.saved.unwrap().position, expected.position);
            let mut child = make(Some("child".into()));
            fresh.diff(&mut child);
            child.as_widget_mut().layout(&mut fresh, &renderer, &limits);
            assert_eq!(
                memory.1.borrow().entries[&(None, 7)].position,
                expected.position
            );
            assert_eq!(
                memory.1.borrow().entries[&(Some("child".into()), 7)]
                    .position
                    .x,
                0.0,
                "same logical block ID in another transcript has independent scroll state"
            );
            let mut parent = make(None);
            fresh.diff(&mut parent);
            let node = parent
                .as_widget_mut()
                .layout(&mut fresh, &renderer, &limits);
            let mut read = Position {
                restore: None,
                bottom,
                saved: None,
                changed: false,
            };
            parent
                .as_widget_mut()
                .operate(&mut fresh, Layout::new(&node), &renderer, &mut read);
            assert_eq!(
                read.saved.unwrap().position,
                expected.position,
                "switching back through the same tree restores the original logical reader"
            );
        }
    }
}
