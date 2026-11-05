//! A menu that opens upward from the composer, over the timeline.
//!
//! COSMIC's popover centers its popup on the anchor and clamps it to the
//! window, so a menu opened from the composer's leading `+` spilled over the
//! sidebar. This one keeps the popup's leading edge on the anchor's and its
//! foot just above it, so the composer and the footer beneath never move. The
//! popup stays one widget tree, measured in place, so assistive technology
//! finds its choices before and after the overlay is first laid out.
use super::*;

/// The gap between the popup's foot and the top of its anchor.
const GAP: f32 = space::XS;

/// `anchor`, with `popup` drawn above its top-left corner while it is `Some`.
///
/// Any press outside the popup closes it with `on_close`, the anchor
/// included, so a press on the control that opened it closes it rather than
/// reopening it.
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
    /// The popup's last layout, in window coordinates once `global`.
    popup: Option<layout::Node>,
    global: bool,
}

struct ComposerPopover<'a, Message> {
    anchor: Element<'a, Message>,
    popup: Option<Element<'a, Message>>,
    on_close: Message,
}

/// Where a popup of `size` goes for an anchor at `anchor` in a window of
/// `bounds`: above the anchor, sharing its leading edge, kept on screen.
fn place(anchor: Point, size: Size, bounds: Size) -> Point {
    Point::new(
        anchor
            .x
            .clamp(0.0, (bounds.width - size.width).max(0.0))
            .round(),
        (anchor.y - GAP - size.height)
            .clamp(0.0, (bounds.height - size.height).max(0.0))
            .round(),
    )
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
        // Measure the popup here too: accessibility can be queried before
        // the runtime first lays out the overlay, which then replaces this
        // with its window-constrained geometry.
        let geometry = tree.state.downcast_mut::<Geometry>();
        geometry.popup = self.popup.as_mut().map(|popup| {
            popup.as_widget_mut().layout(
                &mut tree.children[1],
                renderer,
                &layout::Limits::new(Size::ZERO, limits.max()),
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
        // The composer's own overlays, such as its controls' tooltips, stay
        // beneath the menu rather than vanishing while it is open.
        let (anchor_tree, popup_tree) = tree.children.split_at_mut(1);
        let anchor = self.anchor.as_widget_mut().overlay(
            &mut anchor_tree[0],
            layout,
            renderer,
            viewport,
            translation,
        );
        let Some(popup) = &mut self.popup else {
            return anchor;
        };
        let popup = overlay::Element::new(Box::new(Popup {
            content: popup,
            tree: &mut popup_tree[0],
            geometry: tree.state.downcast_mut::<Geometry>(),
            anchor: layout.bounds().position() + translation,
        }));
        Some(match anchor {
            Some(anchor) => overlay::Group::with_children(vec![anchor, popup]).overlay(),
            None => popup,
        })
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
                    let bounds = layout.bounds();
                    node.clone().move_to(place(
                        bounds.position(),
                        node.size(),
                        Size::new(f32::INFINITY, f32::INFINITY),
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
        let node = node
            .clone()
            .move_to(place(self.anchor, node.size(), bounds));
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
    use cosmic::iced::advanced::clipboard;
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};

    #[derive(Debug, Clone, PartialEq)]
    enum Action {
        Close,
        Choose,
        Toggle,
    }

    /// A composer-like anchor at the foot of a window, offset from its left
    /// edge as a pane beside a sidebar is.
    fn view(open: bool) -> Element<'static, Action> {
        let popup = open.then(|| {
            crate::ui_controls::button::secondary("Plan mode")
                .on_press(Action::Choose)
                .width(220)
                .into()
        });
        widget::container(composer_popover(
            widget::container(
                crate::ui_controls::icon_button(crate::ui_controls::Icon::Plus, "Actions")
                    .on_press(Action::Toggle),
            )
            .width(300),
            popup,
            Action::Close,
        ))
        .padding(cosmic::iced::Padding::from([8.0, 8.0]).left(120.0))
        .width(Length::Fill)
        .height(Length::Fill)
        .align_y(cosmic::iced::Alignment::End)
        .into()
    }

    fn bounds_of(tree: &iced_accessibility::A11yTree, label: &str) -> Rectangle {
        let node = tree
            .root()
            .iter()
            .chain(tree.children())
            .find(|node| node.node().label() == Some(label))
            .unwrap_or_else(|| panic!("{label} is in the accessibility tree"));
        let rect = node.node().bounds().unwrap();
        Rectangle::new(
            Point::new(rect.x0 as f32, rect.y0 as f32),
            Size::new((rect.x1 - rect.x0) as f32, (rect.y1 - rect.y0) as f32),
        )
    }

    #[test]
    fn menu_opens_above_its_anchor_sharing_its_leading_edge() {
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let size = Size::new(500.0, 300.0);
        let mut closed = UserInterface::build(view(false), size, Cache::new(), &mut renderer);
        let anchor = bounds_of(&closed.a11y_nodes(mouse::Cursor::Unavailable), "Actions");
        let mut open = UserInterface::build(view(true), size, closed.into_cache(), &mut renderer);
        // The first semantic query, before the overlay is laid out, already
        // reaches the choices.
        let early = bounds_of(&open.a11y_nodes(mouse::Cursor::Unavailable), "Plan mode");
        let mut messages = Vec::new();
        let _ = open.update(
            &[],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        let tree = open.a11y_nodes(mouse::Cursor::Unavailable);
        assert_eq!(
            bounds_of(&tree, "Actions"),
            anchor,
            "opening never moves it"
        );
        let choice = bounds_of(&tree, "Plan mode");
        assert_eq!(choice, early, "both queries agree on where the menu is");
        // The popup's frame insets its content by its padding.
        assert_eq!(choice.x - space::XS, anchor.x);
        assert!(
            choice.y + choice.height <= anchor.y - GAP,
            "{choice:?} {anchor:?}"
        );
    }

    #[test]
    fn a_press_outside_the_menu_closes_it_and_a_choice_inside_does_not() {
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let size = Size::new(500.0, 300.0);
        let mut ui = UserInterface::build(view(true), size, Cache::new(), &mut renderer);
        let mut messages = Vec::new();
        let _ = ui.update(
            &[],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let choice = bounds_of(&tree, "Plan mode").center();
        let anchor = bounds_of(&tree, "Actions").center();
        for (at, expected) in [
            (choice, vec![Action::Choose]),
            (anchor, vec![Action::Close]),
            (Point::new(490.0, 10.0), vec![Action::Close]),
        ] {
            messages.clear();
            let _ = ui.update(
                &[
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                ],
                mouse::Cursor::Available(at),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(messages, expected, "press at {at:?}");
        }
    }
}
