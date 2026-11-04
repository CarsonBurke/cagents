//! An upward anchored menu whose real overlay remains in the accessibility tree.
//!
//! Pinned COSMIC popovers expose only their anchor to assistive technology.
//! Keep a single popup widget/tree and share its measured overlay geometry with
//! ordinary accessibility traversal; opening the menu never reflows the editor.
use super::*;

pub fn composer_popover<'a, Message: Clone + 'static>(
    anchor: impl Into<Element<'a, Message>>,
    popup: Option<Element<'a, Message>>,
    on_close: Message,
) -> Element<'a, Message> {
    Element::new(ComposerPopover {
        anchor: anchor.into(),
        popup: popup.map(|content| {
            opaque(
                widget::container(content)
                    .padding(space::XS)
                    .class(crate::design::elevated()),
            )
        }),
        on_close,
    })
}

#[derive(Default)]
struct Geometry {
    popup: Option<layout::Node>,
    global: bool,
}

struct ComposerPopover<'a, Message> {
    anchor: Element<'a, Message>,
    popup: Option<Element<'a, Message>>,
    on_close: Message,
}

impl<Message: Clone + 'static> Widget<Message, Theme, Renderer> for ComposerPopover<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Geometry>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Geometry::default())
    }
    fn children(&self) -> Vec<Tree> {
        std::iter::once(Tree::new(&self.anchor))
            .chain(self.popup.as_ref().map(Tree::new))
            .collect()
    }
    fn diff(&mut self, tree: &mut Tree) {
        if let Some(popup) = &mut self.popup {
            tree.diff_children(&mut [&mut self.anchor, popup]);
        } else {
            tree.diff_children(&mut [&mut self.anchor]);
            tree.state.downcast_mut::<Geometry>().popup = None;
        }
    }
    fn size(&self) -> Size<Length> {
        self.anchor.as_widget().size()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // Measure an initial semantic tree too: accessibility can be queried
        // before the runtime first lays out the overlay. The actual window-
        // constrained geometry replaces it at the overlay layout boundary.
        let geometry = tree.state.downcast_mut::<Geometry>();
        geometry.popup = self.popup.as_mut().map(|popup| {
            popup.as_widget_mut().layout(
                &mut tree.children[1],
                renderer,
                &layout::Limits::new(Size::ZERO, Size::new(4096.0, 260.0)),
            )
        });
        geometry.global = false;
        self.anchor
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
        self.anchor
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
        if self.popup.is_some()
            && matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(_))
                    | Event::Touch(cosmic::iced::touch::Event::FingerPressed { .. })
            )
            && !cursor.is_over(layout.bounds())
            && !tree
                .state
                .downcast_ref::<Geometry>()
                .popup
                .as_ref()
                .is_some_and(|popup| cursor.is_over(popup.bounds()))
        {
            shell.publish(self.on_close.clone());
            shell.capture_event();
            return;
        }
        self.anchor.as_widget_mut().update(
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
        self.anchor.as_widget().mouse_interaction(
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
        self.anchor.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        let Some(popup) = &mut self.popup else {
            return self.anchor.as_widget_mut().overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            );
        };
        let anchor = layout.bounds();
        Some(overlay::Element::new(Box::new(Popup {
            content: popup,
            tree: &mut tree.children[1],
            geometry: tree.state.downcast_mut::<Geometry>(),
            anchor: anchor.position() + translation,
        })))
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let anchor = self
            .anchor
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor);
        let geometry = tree.state.downcast_ref::<Geometry>();
        let popup = self
            .popup
            .as_ref()
            .zip(geometry.popup.as_ref())
            .map(|(popup, node)| {
                let node = if geometry.global {
                    node.clone()
                } else {
                    node.clone().move_to(Point::new(
                        layout.bounds().x,
                        (layout.bounds().y - node.size().height).max(0.0),
                    ))
                };
                popup
                    .as_widget()
                    .a11y_nodes(Layout::new(&node), &tree.children[1], cursor)
            });
        iced_accessibility::A11yTree::join(std::iter::once(anchor).chain(popup))
    }
}

struct Popup<'a, 'b, Message> {
    content: &'a mut Element<'b, Message>,
    tree: &'a mut Tree,
    geometry: &'a mut Geometry,
    anchor: Point,
}
impl<Message: Clone + 'static> overlay::Overlay<Message, Theme, Renderer>
    for Popup<'_, '_, Message>
{
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        let node = self.content.as_widget_mut().layout(
            self.tree,
            renderer,
            &layout::Limits::new(Size::ZERO, bounds),
        );
        let size = node.size();
        let node = node.move_to(Point::new(
            self.anchor
                .x
                .clamp(0.0, (bounds.width - size.width).max(0.0))
                .round(),
            (self.anchor.y - size.height)
                .clamp(0.0, (bounds.height - size.height).max(0.0))
                .round(),
        ));
        self.geometry.popup = Some(node.clone());
        self.geometry.global = true;
        node
    }
    fn operate(&mut self, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content
            .as_widget_mut()
            .operate(self.tree, layout, renderer, operation);
    }
    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        self.content.as_widget_mut().update(
            self.tree,
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            &layout.bounds(),
        );
    }
    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            self.tree,
            layout,
            cursor,
            &layout.bounds(),
            renderer,
        )
    }
    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.content.as_widget().draw(
            self.tree,
            renderer,
            theme,
            style,
            layout,
            cursor,
            &layout.bounds(),
        );
    }
    fn overlay<'a>(
        &'a mut self,
        layout: Layout<'a>,
        renderer: &Renderer,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            self.tree,
            layout,
            renderer,
            &layout.bounds(),
            Vector::ZERO,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
    use cosmic::iced::{advanced::clipboard, keyboard};

    #[derive(Debug, Clone, PartialEq)]
    enum Action {
        Close,
        Choose,
        Query(String),
        Toggle,
    }

    fn view(open: bool) -> Element<'static, Action> {
        let popup = open.then(|| {
            keyboard_list(
                widget::column::with_children(vec![
                    keyboard_list_entry(
                        crate::ui_controls::field::search("Search actions", "")
                            .id("test-actions-search".into())
                            .on_input(Action::Query),
                    ),
                    keyboard_list_row(
                        crate::ui_controls::button::secondary("Plan mode")
                            .on_press(Action::Choose)
                            .width(Length::Fill),
                    ),
                ])
                .width(220)
                .spacing(space::XS),
                true,
            )
        });
        widget::container(composer_popover(
            crate::ui_controls::icon_button(crate::ui_controls::Icon::Plus, "Conversation actions")
                .compact()
                .on_press(Action::Toggle),
            popup,
            Action::Close,
        ))
        .padding(8)
        .width(Length::Fill)
        .height(Length::Fill)
        .align_y(cosmic::iced::Alignment::End)
        .into()
    }

    fn key(named: keyboard::key::Named) -> Event {
        Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(named),
            modified_key: keyboard::Key::Named(named),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        })
    }

    #[test]
    fn menu_overlay_exposes_native_choices_without_moving_anchor() {
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let size = Size::new(260.0, 300.0);
        let mut closed = UserInterface::build(view(false), size, Cache::new(), &mut renderer);
        let before = closed.a11y_nodes(mouse::Cursor::Unavailable);
        let anchor = before
            .root()
            .iter()
            .chain(before.children())
            .find(|n| n.node().label() == Some("Conversation actions"))
            .unwrap()
            .node()
            .bounds()
            .unwrap();
        let mut open = UserInterface::build(view(true), size, closed.into_cache(), &mut renderer);
        // Even the first semantic query can reach choices before overlay paint.
        assert!(
            open.a11y_nodes(mouse::Cursor::Unavailable)
                .root()
                .iter()
                .chain(open.a11y_nodes(mouse::Cursor::Unavailable).children())
                .any(|node| node.node().label() == Some("Plan mode"))
        );
        let mut messages = Vec::new();
        let _ = open.update(
            &[],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        let after = open.a11y_nodes(mouse::Cursor::Unavailable);
        for label in ["Conversation actions", "Search actions", "Plan mode"] {
            let node = after
                .root()
                .iter()
                .chain(after.children())
                .find(|n| n.node().label() == Some(label))
                .unwrap();
            let bounds = node.node().bounds().unwrap();
            assert!(
                bounds.x0 >= 0.0
                    && bounds.y0 >= 0.0
                    && bounds.x1 <= f64::from(size.width)
                    && bounds.y1 <= f64::from(size.height),
                "{label}: {bounds:?}"
            );
            if label == "Conversation actions" {
                assert_eq!(bounds, anchor);
            }
        }
    }

    #[test]
    fn menu_overlay_retains_keyboard_selection_and_dismisses_outside_click() {
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut ui = UserInterface::build(
            view(true),
            Size::new(260.0, 300.0),
            Cache::new(),
            &mut renderer,
        );
        ui.operate(
            &renderer,
            &mut cosmic::iced::advanced::widget::operation::focusable::focus::<()>(Id::new(
                "test-actions-search",
            )),
        );
        let mut messages = Vec::new();
        let _ = ui.update(
            &[
                key(keyboard::key::Named::ArrowDown),
                key(keyboard::key::Named::Enter),
            ],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert!(messages.contains(&Action::Choose), "{messages:?}");
        messages.clear();
        let _ = ui.update(
            &[Event::Mouse(mouse::Event::ButtonPressed(
                mouse::Button::Left,
            ))],
            mouse::Cursor::Available(Point::new(250.0, 10.0)),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(messages, vec![Action::Close]);
    }
}
