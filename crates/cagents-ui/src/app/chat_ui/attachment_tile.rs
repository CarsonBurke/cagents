//! One attachment in the composer's tray: a thumbnail, or a chip for a file,
//! that the keyboard reaches with Tab.
//!
//! At rest it shows only its face. While the pointer is over it, while it
//! holds keyboard focus, or while the composer's caret is on its `[Image N]`
//! reference, it also shows its label and its remove (×) button. Click,
//! Enter or Space opens it; Delete or Backspace removes it. The × sits
//! inside the tile, so it never reaches over a neighbour or the tray's edge.
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, renderer,
    widget::{self, Operation, Tree, operation, tree},
};
use cosmic::iced::{Border, Color, Event, Length, Point, Rectangle, Size, keyboard};
use cosmic::{Element, Renderer, Theme};
use iced_accessibility::accesskit::{Action as A11yAction, Node, Rect, Role};

/// How far the × sits in from the tile's top-right corner.
const CLOSE_INSET: f32 = 3.0;
/// The focus ring's width and its gap outside the tile.
const RING: f32 = 2.0;

pub(super) struct Tile<'a, Message> {
    id: widget::Id,
    /// The face, then the label shown over it, then the × button.
    children: [Element<'a, Message>; 3],
    name: String,
    marked: bool,
    on_open: Option<Message>,
    on_remove: Message,
}

pub(super) fn tile<'a, Message: Clone + 'a>(
    id: widget::Id,
    face: impl Into<Element<'a, Message>>,
    label: impl Into<Element<'a, Message>>,
    close: impl Into<Element<'a, Message>>,
    name: String,
    on_remove: Message,
) -> Tile<'a, Message> {
    Tile {
        id,
        children: [face.into(), label.into(), close.into()],
        name,
        marked: false,
        on_open: None,
        on_remove,
    }
}

impl<'a, Message> Tile<'a, Message> {
    /// What Click, Enter and Space do; a tile without it only removes.
    pub(super) fn on_open(mut self, message: Option<Message>) -> Self {
        self.on_open = message;
        self
    }

    /// Shows the label and × as hover does: the composer's caret is on this
    /// attachment's reference.
    pub(super) fn marked(mut self, marked: bool) -> Self {
        self.marked = marked;
        self
    }
}

struct State {
    focused: bool,
    hovered: bool,
    /// The accessible node keeps one identity across frames.
    a11y: iced_accessibility::Id,
}

impl operation::Focusable for State {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn focus(&mut self) {
        self.focused = true;
    }
    fn unfocus(&mut self) {
        self.focused = false;
    }
}

const FACE: usize = 0;
const LABEL: usize = 1;
const CLOSE: usize = 2;

impl<Message> Tile<'_, Message> {
    fn revealed(&self, state: &State) -> bool {
        state.hovered || state.focused || self.marked
    }
}

impl<'a, Message: Clone + 'a> From<Tile<'a, Message>> for Element<'a, Message> {
    fn from(tile: Tile<'a, Message>) -> Self {
        Element::new(tile)
    }
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for Tile<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State {
            focused: false,
            hovered: false,
            a11y: iced_accessibility::Id::unique(),
        })
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
        let [face, label, close] = &mut self.children;
        let [face_tree, label_tree, close_tree] = &mut tree.children[..] else {
            unreachable!("a tile has three parts");
        };
        let face = face
            .as_widget_mut()
            .layout(face_tree, renderer, &limits.loose());
        let size = face.size();
        let label = label.as_widget_mut().layout(
            label_tree,
            renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let close = close.as_widget_mut().layout(
            close_tree,
            renderer,
            &layout::Limits::new(Size::ZERO, size),
        );
        let close_size = close.size();
        let close = close.move_to(Point::new(
            (size.width - close_size.width - CLOSE_INSET).max(0.0),
            CLOSE_INSET,
        ));
        layout::Node::with_children(size, vec![face, label, close])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        // The tile is one stop in the focus order: the × is for the pointer,
        // and Delete does its work from the keyboard.
        let state = tree.state.downcast_mut::<State>();
        operation.focusable(Some(&self.id), layout.bounds(), state);
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
        let bounds = layout.bounds();
        let hovered = cursor.is_over(bounds) && cursor.is_over(*viewport);
        let state = tree.state.downcast_mut::<State>();
        if state.hovered != hovered {
            state.hovered = hovered;
            shell.request_redraw();
        }
        let revealed = self.revealed(state);
        if revealed {
            let close = layout.children().nth(CLOSE).expect("a tile's ×");
            self.children[CLOSE].as_widget_mut().update(
                &mut tree.children[CLOSE],
                event,
                close,
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
            if shell.is_event_captured() {
                return;
            }
        }
        let state = tree.state.downcast_mut::<State>();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                if hovered {
                    state.focused = true;
                    if let Some(open) = &self.on_open {
                        shell.publish(open.clone());
                    }
                    shell.capture_event();
                    shell.request_redraw();
                } else if state.focused {
                    state.focused = false;
                    shell.request_redraw();
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. })
                if state.focused && !modifiers.command() && !modifiers.alt() =>
            {
                use keyboard::key::Named;
                let message = match key.as_ref() {
                    keyboard::Key::Named(Named::Enter) => self.on_open.clone(),
                    keyboard::Key::Character(" ") => self.on_open.clone(),
                    keyboard::Key::Named(Named::Delete | Named::Backspace) => {
                        Some(self.on_remove.clone())
                    }
                    _ => return,
                };
                if let Some(message) = message {
                    shell.publish(message);
                }
                shell.capture_event();
            }
            _ => {}
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
        let state = tree.state.downcast_ref::<State>();
        if self.revealed(state) {
            let close = self.children[CLOSE].as_widget().mouse_interaction(
                &tree.children[CLOSE],
                layout.children().nth(CLOSE).expect("a tile's ×"),
                cursor,
                viewport,
                renderer,
            );
            if close != mouse::Interaction::None {
                return close;
            }
        }
        if self.on_open.is_some() && cursor.is_over(layout.bounds()) {
            mouse::Interaction::Pointer
        } else {
            mouse::Interaction::None
        }
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
        use cosmic::iced::advanced::Renderer as _;
        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds();
        let mut parts = layout.children();
        let face = parts.next().expect("a tile's face");
        let label = parts.next().expect("a tile's label");
        let close = parts.next().expect("a tile's ×");
        // The face is clipped to the tile's rounded square by its own
        // container; the label and × draw over it.
        self.children[FACE].as_widget().draw(
            &tree.children[FACE],
            renderer,
            theme,
            style,
            face,
            cursor,
            viewport,
        );
        if self.revealed(state) {
            for (index, part) in [(LABEL, label), (CLOSE, close)] {
                self.children[index].as_widget().draw(
                    &tree.children[index],
                    renderer,
                    theme,
                    style,
                    part,
                    cursor,
                    viewport,
                );
            }
        }
        let ring = if state.focused {
            Some(crate::design::accent_color(theme))
        } else if state.hovered || self.marked {
            Some(Color {
                a: 0.5,
                ..crate::design::muted_color(theme)
            })
        } else {
            None
        };
        if let Some(color) = ring {
            let width = if state.focused { RING } else { 1.0 };
            let outset = if state.focused { RING } else { 0.0 };
            renderer.fill_quad(
                renderer::Quad {
                    bounds: Rectangle {
                        x: bounds.x - outset,
                        y: bounds.y - outset,
                        width: bounds.width + 2.0 * outset,
                        height: bounds.height + 2.0 * outset,
                    },
                    border: Border {
                        color,
                        width,
                        radius: (crate::design::RADIUS_S + outset).into(),
                    },
                    ..Default::default()
                },
                Color::TRANSPARENT,
            );
        }
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let state = tree.state.downcast_ref::<State>();
        let bounds = layout.bounds() - layout.virtual_offset();
        let mut node = Node::new(Role::Button);
        node.set_label(self.name.clone());
        node.set_description(if self.on_open.is_some() {
            "Enter opens it, Delete removes it"
        } else {
            "Delete removes it"
        });
        node.add_action(A11yAction::Focus);
        if self.on_open.is_some() {
            node.add_action(A11yAction::Click);
        }
        node.set_bounds(Rect::new(
            f64::from(bounds.x),
            f64::from(bounds.y),
            f64::from(bounds.x + bounds.width),
            f64::from(bounds.y + bounds.height),
        ));
        iced_accessibility::A11yTree::leaf(node, state.a11y.clone())
    }
}
