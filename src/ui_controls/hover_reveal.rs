//! A control that takes the place of quieter content while the pointer is
//! over the row around it, or while the control has keyboard focus.
//!
//! A list row's trailing marks, such as its checks glyph and number, give
//! way to an action, such as opening the row in a browser, the way a mail
//! list swaps a message's date for its actions. The row is wrapped in a
//! [`region`] and its trailing slot holds a [`reveal`] sharing that region,
//! so hovering anywhere on the row reveals the action, not only hovering
//! the trailing slot itself.
//!
//! The action stays in the focus order and the accessibility tree while it
//! is hidden, so the keyboard reaches it the same way the pointer does and
//! focusing it reveals it.

use std::{cell::Cell, rc::Rc};

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree, operation, tree},
};
use cosmic::iced::{Event, Length, Point, Rectangle, Size, Vector, mouse, window};
use cosmic::{Element, Renderer, Theme};

/// Whether the pointer is over a row, shared by the row's [`region`] and
/// the [`reveal`] inside it. The region writes it before its content sees
/// each event and each draw, so the reveal reads it in the same pass.
#[derive(Clone, Default)]
pub struct Region(Rc<Cell<bool>>);

impl Region {
    pub fn new() -> Self {
        Self::default()
    }
}

/// `content`, whose bounds reveal the [`reveal`]s built with `region`.
pub fn region<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    region: &Region,
) -> Element<'a, Message> {
    Element::new(HoverRegion {
        content: content.into(),
        hovered: region.clone(),
    })
}

/// `idle` until the pointer is over `region` or `active` holds focus, then
/// `active` in its place. Both are laid out, so the slot keeps one size and
/// the row never reflows as the pointer crosses it; each is aligned to the
/// slot's end.
pub fn reveal<'a, Message: 'a>(
    idle: impl Into<Element<'a, Message>>,
    active: impl Into<Element<'a, Message>>,
    region: &Region,
) -> Element<'a, Message> {
    Element::new(Reveal {
        children: [idle.into(), active.into()],
        hovered: region.clone(),
    })
}

struct HoverRegion<'a, Message> {
    content: Element<'a, Message>,
    hovered: Region,
}

#[derive(Default)]
struct RegionState {
    hovered: bool,
}

impl<Message> HoverRegion<'_, Message> {
    fn track(&self, layout: Layout<'_>, cursor: mouse::Cursor, viewport: &Rectangle) -> bool {
        let hovered = cursor.is_over(layout.bounds()) && cursor.is_over(*viewport);
        self.hovered.0.set(hovered);
        hovered
    }
}

impl<Message> Widget<Message, Theme, Renderer> for HoverRegion<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<RegionState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(RegionState::default())
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

    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let content = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits);
        layout::Node::with_children(content.size(), vec![content])
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
            child(layout),
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
        let hovered = self.track(layout, cursor, viewport);
        let state = tree.state.downcast_mut::<RegionState>();
        if state.hovered != hovered {
            state.hovered = hovered;
            shell.request_redraw();
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            child(layout),
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
            child(layout),
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
        self.track(layout, cursor, viewport);
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            child(layout),
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
            child(layout),
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
            .a11y_nodes(child(layout), &tree.children[0], cursor)
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
            child(layout),
            renderer,
            destinations,
        );
    }
}

struct Reveal<'a, Message> {
    /// The idle content, then the action that replaces it.
    children: [Element<'a, Message>; 2],
    hovered: Region,
}

#[derive(Default)]
struct RevealState {
    /// Whether the action holds keyboard focus, read on each redraw.
    focused: bool,
    /// Whether the action showed at the last event, which decides whether
    /// its overlay, such as its tooltip, may show.
    revealed: bool,
}

const IDLE: usize = 0;
const ACTIVE: usize = 1;

impl<Message> Reveal<'_, Message> {
    fn revealed(&self, tree: &Tree) -> bool {
        self.hovered.0.get() || tree.state.downcast_ref::<RevealState>().focused
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Reveal<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<RevealState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(RevealState::default())
    }

    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
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
        let loose = limits.loose();
        let nodes: Vec<layout::Node> = self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .map(|(child, tree)| child.as_widget_mut().layout(tree, renderer, &loose))
            .collect();
        let natural = nodes.iter().fold(Size::ZERO, |size, node| {
            Size::new(
                size.width.max(node.size().width),
                size.height.max(node.size().height),
            )
        });
        let size = limits.resolve(Length::Shrink, Length::Shrink, natural);
        let nodes = nodes
            .into_iter()
            .map(|node| {
                let offset = Point::new(
                    (size.width - node.size().width).max(0.0),
                    ((size.height - node.size().height) / 2.0).max(0.0),
                );
                node.move_to(offset)
            })
            .collect();
        layout::Node::with_children(size, nodes)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        // Only the action can take focus or be found by an operation; the
        // idle content is what it covers.
        self.children[ACTIVE].as_widget_mut().operate(
            &mut tree.children[ACTIVE],
            nth(layout, ACTIVE),
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
        if matches!(event, Event::Window(window::Event::RedrawRequested(_))) {
            let mut count = operation::focusable::count();
            self.children[ACTIVE].as_widget_mut().operate(
                &mut tree.children[ACTIVE],
                nth(layout, ACTIVE),
                renderer,
                &mut operation::black_box(&mut count),
            );
            tree.state.downcast_mut::<RevealState>().focused = match count.finish() {
                operation::Outcome::Some(count) => count.focused.is_some(),
                _ => false,
            };
        }
        let revealed = self.revealed(tree);
        tree.state.downcast_mut::<RevealState>().revealed = revealed;
        // Hidden, the action answers the keyboard, which reaches it only
        // through focus, but not the pointer, which presses the row beneath.
        let pointer = matches!(event, Event::Mouse(_) | Event::Touch(_));
        let cursor = if revealed || !pointer {
            cursor
        } else {
            mouse::Cursor::Unavailable
        };
        self.children[ACTIVE].as_widget_mut().update(
            &mut tree.children[ACTIVE],
            event,
            nth(layout, ACTIVE),
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
        if !self.revealed(tree) {
            return mouse::Interaction::None;
        }
        self.children[ACTIVE].as_widget().mouse_interaction(
            &tree.children[ACTIVE],
            nth(layout, ACTIVE),
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
        let shown = if self.revealed(tree) { ACTIVE } else { IDLE };
        self.children[shown].as_widget().draw(
            &tree.children[shown],
            renderer,
            theme,
            style,
            nth(layout, shown),
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
        if !tree.state.downcast_ref::<RevealState>().revealed {
            return None;
        }
        let [_, active] = &mut self.children;
        active.as_widget_mut().overlay(
            &mut tree.children[ACTIVE],
            nth(layout, ACTIVE),
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
        // The row's own name already says what the idle marks show.
        self.children[ACTIVE].as_widget().a11y_nodes(
            nth(layout, ACTIVE),
            &tree.children[ACTIVE],
            cursor,
        )
    }
}

fn child(layout: Layout<'_>) -> Layout<'_> {
    nth(layout, 0)
}

fn nth(layout: Layout<'_>, index: usize) -> Layout<'_> {
    layout
        .children()
        .nth(index)
        .expect("laid out child")
        .with_virtual_offset(layout.virtual_offset())
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::widget;

    fn laid_out(mut element: Element<'_, ()>) -> (Tree, layout::Node) {
        let renderer = super::super::accessibility_probe::headless_renderer();
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(200.0, 40.0)),
        );
        (tree, node)
    }

    #[test]
    fn the_slot_fits_both_and_ends_each_at_its_edge() {
        let region = Region::new();
        let (_, node) = laid_out(reveal(
            widget::Space::new().width(48).height(16),
            widget::Space::new().width(24).height(24),
            &region,
        ));
        assert_eq!(node.size(), Size::new(48.0, 24.0));
        let [idle, active] = node.children() else {
            panic!("two children");
        };
        assert_eq!(idle.bounds().x + idle.bounds().width, 48.0);
        assert_eq!(active.bounds().x + active.bounds().width, 48.0);
        assert_eq!(idle.bounds().y, 4.0);
    }

    #[test]
    fn the_region_says_whether_the_pointer_is_over_it() {
        let shared = Region::new();
        let content = region::<()>(widget::Space::new().width(100).height(20), &shared);
        let mut content = content;
        let renderer = super::super::accessibility_probe::headless_renderer();
        let mut tree = Tree::new(&content);
        let node = content.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(100.0, 20.0)),
        );
        let layout = Layout::new(&node);
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        let viewport = Rectangle::new(Point::ORIGIN, Size::new(100.0, 20.0));
        let moved = Event::Mouse(mouse::Event::CursorMoved {
            position: Point::new(50.0, 10.0),
        });
        for (position, over) in [
            (Point::new(50.0, 10.0), true),
            (Point::new(150.0, 10.0), false),
        ] {
            content.as_widget_mut().update(
                &mut tree,
                &moved,
                layout,
                mouse::Cursor::Available(position),
                &renderer,
                &mut cosmic::iced::advanced::clipboard::Null,
                &mut shell,
                &viewport,
            );
            assert_eq!(shared.0.get(), over, "{position:?}");
        }
    }
}
