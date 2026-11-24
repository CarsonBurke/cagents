//! One attachment in the composer's tray: an image's chip or a file's, that
//! the keyboard reaches with Tab.
//!
//! At rest it shows only its face, which names the attachment itself. While
//! the pointer is over it, while it holds keyboard focus, or while the
//! composer's caret is on its `[Image N]` reference, it also shows its
//! overlay, where one is given, and its remove (×) button. Click,
//! Enter or Space opens it; Delete or Backspace removes it; the message's
//! undo and redo chords reach the message's history. The × sits
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
    /// The face, then the × button drawn over it.
    children: [Element<'a, Message>; 2],
    name: String,
    marked: bool,
    on_open: Option<Message>,
    on_remove: Option<Message>,
    /// The message's undo and redo, which a focused tile passes on so the
    /// removal it just made can be taken back without leaving the tray.
    on_undo: Option<Message>,
    on_redo: Option<Message>,
}

pub(super) fn tile<'a, Message: Clone + 'a>(
    id: widget::Id,
    face: impl Into<Element<'a, Message>>,
    close: impl Into<Element<'a, Message>>,
    name: String,
    on_remove: Option<Message>,
) -> Tile<'a, Message> {
    Tile {
        id,
        children: [face.into(), close.into()],
        name,
        marked: false,
        on_open: None,
        on_remove,
        on_undo: None,
        on_redo: None,
    }
}

impl<'a, Message> Tile<'a, Message> {
    /// What Click, Enter and Space do; a tile without it only removes.
    pub(super) fn on_open(mut self, message: Option<Message>) -> Self {
        self.on_open = message;
        self
    }

    /// What Ctrl+Z, and Ctrl+Shift+Z or Ctrl+Y, do while the tile has the
    /// keyboard; a chord without one is left to the workspace.
    pub(super) fn on_history(mut self, undo: Option<Message>, redo: Option<Message>) -> Self {
        self.on_undo = undo;
        self.on_redo = redo;
        self
    }

    /// Shows the × as hover does: the composer's caret is on this
    /// attachment's reference.
    pub(super) fn marked(mut self, marked: bool) -> Self {
        self.marked = marked;
        self
    }
}

struct State {
    focused: bool,
    hovered: bool,
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
const CLOSE: usize = 1;

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
        let [face, close] = &mut self.children;
        let [face_tree, close_tree] = &mut tree.children[..] else {
            unreachable!("a tile has two parts");
        };
        let face = face
            .as_widget_mut()
            .layout(face_tree, renderer, &limits.loose());
        let size = face.size();
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
        layout::Node::with_children(size, vec![face, close])
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
        // An assistive technology acts on the tile through its accessible
        // node, which shares the tile's id.
        if let Event::A11y(id, request) = event {
            if *id != self.id {
                return;
            }
            match request.action {
                A11yAction::Focus => state.focused = true,
                A11yAction::Click => {
                    if let Some(open) = &self.on_open {
                        shell.publish(open.clone());
                    }
                }
                _ => return,
            }
            shell.request_redraw();
            return;
        }
        let revealed = self.revealed(state);
        if revealed {
            // Only the × taking the event ends it here: a press another tile
            // already took still has to take focus away from this one.
            let captured = shell.is_event_captured();
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
            if !captured && shell.is_event_captured() {
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
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                ..
            }) if state.focused && modifiers.command() && !modifiers.alt() => {
                let message = match key
                    .to_latin(*physical_key)
                    .map(|letter| letter.to_ascii_lowercase())
                {
                    Some('z') if modifiers.shift() => self.on_redo.clone(),
                    Some('z') => self.on_undo.clone(),
                    Some('y') if !modifiers.shift() => self.on_redo.clone(),
                    _ => None,
                };
                if let Some(message) = message {
                    shell.publish(message);
                    shell.capture_event();
                }
            }
            Event::Keyboard(keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                ..
            }) if state.focused && !modifiers.command() && !modifiers.alt() => {
                use keyboard::key::{Code, Named, Physical};
                // The space bar has no named key here; its physical code is
                // the reliable signal, and its text the fallback.
                let space = matches!(physical_key, Physical::Code(Code::Space))
                    || matches!(key.as_ref(), keyboard::Key::Character(" "));
                let message = match key.as_ref() {
                    keyboard::Key::Named(Named::Enter) => self.on_open.clone(),
                    keyboard::Key::Named(Named::Delete | Named::Backspace) => {
                        self.on_remove.clone()
                    }
                    _ if space => self.on_open.clone(),
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
        let close = parts.next().expect("a tile's ×");
        // The face is clipped to the tile's rounded square by its own
        // container; the × and ring draw over it.
        self.children[FACE].as_widget().draw(
            &tree.children[FACE],
            renderer,
            theme,
            style,
            face,
            cursor,
            viewport,
        );
        let revealed = self.revealed(state);
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
        if !revealed && ring.is_none() {
            return;
        }
        let outset = if state.focused { RING } else { 0.0 };
        let ringed = Rectangle {
            x: bounds.x - outset,
            y: bounds.y - outset,
            width: bounds.width + 2.0 * outset,
            height: bounds.height + 2.0 * outset,
        };
        let Some(clip) = ringed.intersection(viewport) else {
            return;
        };
        // A layer of its own: within one layer the renderer puts images
        // above quads, so the thumbnail would hide the × and the ring.
        renderer.with_layer(clip, |renderer| {
            if revealed {
                self.children[CLOSE].as_widget().draw(
                    &tree.children[CLOSE],
                    renderer,
                    theme,
                    style,
                    close,
                    cursor,
                    viewport,
                );
            }
            if let Some(color) = ring {
                renderer.fill_quad(
                    renderer::Quad {
                        bounds: ringed,
                        border: Border {
                            color,
                            width: if state.focused { RING } else { 1.0 },
                            radius: (crate::design::RADIUS_S + outset).into(),
                        },
                        ..Default::default()
                    },
                    Color::TRANSPARENT,
                );
            }
        });
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        _tree: &Tree,
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let bounds = layout.bounds() - layout.virtual_offset();
        let mut node = Node::new(Role::Button);
        node.set_label(self.name.clone());
        match (self.on_open.is_some(), self.on_remove.is_some()) {
            (true, true) => node.set_description("Enter opens it, Delete removes it"),
            (true, false) => node.set_description("Enter opens it"),
            (false, true) => node.set_description("Delete removes it"),
            (false, false) => {}
        }
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
        iced_accessibility::A11yTree::leaf(node, self.id.clone())
    }

    fn id(&self) -> Option<widget::Id> {
        Some(self.id.clone())
    }

    fn set_id(&mut self, id: widget::Id) {
        self.id = id;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::advanced::{clipboard, widget::operation::Focusable};
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};

    /// The ids of the tiles that hold keyboard focus.
    #[derive(Default)]
    struct Focused(Vec<widget::Id>);
    impl Operation for Focused {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(&mut self, id: Option<&widget::Id>, _: Rectangle, state: &mut dyn Focusable) {
            if state.is_focused() {
                self.0.extend(id.cloned());
            }
        }
    }

    /// Tiles as the composer's tray lays them out: each under the tooltip
    /// naming its file, in a row that wraps.
    fn tray(ids: &[widget::Id]) -> Element<'static, usize> {
        let tiles = ids.iter().enumerate().map(|(index, id)| {
            cosmic::widget::tooltip(
                tile(
                    id.clone(),
                    cosmic::widget::Space::new().width(56).height(56),
                    cosmic::widget::button::custom(
                        cosmic::widget::Space::new().width(18).height(18),
                    )
                    .on_press(100 + index),
                    format!("Image {}", index + 1),
                    Some(200 + index),
                )
                .on_open(Some(index))
                // The message has a step to undo and none to redo.
                .on_history(Some(300), None),
                cosmic::widget::text("screenshot.png"),
                cosmic::widget::tooltip::Position::Top,
            )
            .into()
        });
        cosmic::widget::row::with_children(tiles.collect::<Vec<_>>())
            .spacing(8)
            .wrap()
            .into()
    }

    #[test]
    fn assistive_tools_find_every_tile_in_the_tray() {
        let ids = [widget::Id::unique(), widget::Id::unique()];
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let ui = UserInterface::build(
            tray(&ids),
            Size::new(400.0, 100.0),
            Cache::new(),
            &mut renderer,
        );
        let nodes = ui.a11y_nodes(mouse::Cursor::Unavailable);
        for id in ids {
            assert!(nodes.contains(&iced_accessibility::A11yId::from(id)));
        }
    }

    #[test]
    fn clicking_a_tile_takes_focus_from_the_one_that_had_it() {
        let ids = [widget::Id::unique(), widget::Id::unique()];
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = UserInterface::build(
            tray(&ids),
            Size::new(400.0, 100.0),
            Cache::new(),
            &mut renderer,
        );
        let mut focus = operation::focusable::focus(ids[1].clone());
        ui.operate(&renderer, &mut focus);

        // Tile 2 shows its × while focused, so the press reaches its × and
        // is already taken, by tile 1, when it gets there.
        let position = Point::new(28.0, 40.0);
        let mut messages = Vec::new();
        let _ = ui.update(
            &[
                Event::Mouse(mouse::Event::CursorMoved { position }),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ],
            mouse::Cursor::Available(position),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(messages, [0], "the press opens tile 1");
        let mut focused = Focused::default();
        ui.operate(&renderer, &mut focused);
        assert_eq!(
            focused.0,
            [ids[0].clone()],
            "only the clicked tile holds focus"
        );
    }

    #[test]
    fn a_focused_tile_passes_the_messages_undo_on_and_leaves_a_missing_redo() {
        use keyboard::key::{Code, Physical};
        let ids = [widget::Id::unique(), widget::Id::unique()];
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = UserInterface::build(
            tray(&ids),
            Size::new(400.0, 100.0),
            Cache::new(),
            &mut renderer,
        );
        let mut focus = operation::focusable::focus(ids[0].clone());
        ui.operate(&renderer, &mut focus);
        let chord = |letter: &str, code, modifiers| {
            Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(letter.into()),
                modified_key: keyboard::Key::Character(letter.into()),
                physical_key: Physical::Code(code),
                location: keyboard::Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            })
        };
        let control = keyboard::Modifiers::CTRL;
        for (event, expected) in [
            (chord("z", Code::KeyZ, control), vec![300]),
            // With nothing to redo, the workspace keeps Ctrl+Shift+Z.
            (
                chord("z", Code::KeyZ, control | keyboard::Modifiers::SHIFT),
                vec![],
            ),
            (chord("y", Code::KeyY, control), vec![]),
        ] {
            let mut messages = Vec::new();
            let (_, statuses) = ui.update(
                &[event],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            let captured = statuses[0] == cosmic::iced::event::Status::Captured;
            assert_eq!(captured, !expected.is_empty());
            assert_eq!(messages, expected);
        }
    }
}
