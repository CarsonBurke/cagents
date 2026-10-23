//! Keep native child state with the immutable action identity it belongs to.
use cosmic::{Element, Renderer, Theme, iced};
use iced::advanced::widget::{Operation, Tree, tree};
use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
use iced::{Event, Length, Rectangle, Size, Vector};

/// Preserve a child's native state only while `key` still identifies its action.
///
/// A changed key discards the child tree before diffing the replacement, so a
/// held press or focus cannot migrate to a new callback in the same row.
/// Unchanged keys retain ordinary native interaction state.
pub fn owned<'a, Key: Clone + Eq + 'static, Message: 'a>(
    key: Key,
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Owned {
        key,
        content: content.into(),
    })
}

struct Owned<'a, Key, Message> {
    key: Key,
    content: Element<'a, Message>,
}

// A distinct tag avoids colliding with another widget that stores `Key`.
struct KeyState<Key>(Key);

impl<Key: Clone + Eq + 'static, Message> Widget<Message, Theme, Renderer>
    for Owned<'_, Key, Message>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<KeyState<Key>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(KeyState(self.key.clone()))
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        if tree.state.downcast_ref::<KeyState<Key>>().0 != self.key {
            tree.state = self.state();
            tree.children = self.children();
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
        self.content
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
        renderer: &Renderer,
        destinations: &mut iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout,
            renderer,
            destinations,
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
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui_controls::accessibility_probe::headless_renderer;
    use cosmic::iced::Point;
    use cosmic::iced::advanced::clipboard;
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
    use cosmic::widget;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Action {
        Original,
        Replacement,
    }

    // Exercise the runtime's real cache and native button state, including its
    // named-state extraction/restore path. The label and child count stay fixed.
    struct Frames {
        cache: Cache,
        renderer: Renderer,
        named: bool,
    }

    impl Frames {
        fn new(named: bool) -> Self {
            Self {
                cache: Cache::new(),
                renderer: headless_renderer(),
                named,
            }
        }

        fn frame(
            &mut self,
            owner: u64,
            action: Option<Action>,
            event: Option<mouse::Event>,
        ) -> Vec<Action> {
            let button = widget::button::custom(crate::design::body("Open"))
                .width(Length::Fill)
                .height(40)
                .on_press_maybe(action);
            let button = if self.named {
                button.id(widget::Id::new("owned.native-action"))
            } else {
                button
            };
            let mut ui = UserInterface::build(
                owned(owner, button),
                Size::new(240.0, 80.0),
                std::mem::replace(&mut self.cache, Cache::new()),
                &mut self.renderer,
            );
            let mut messages = Vec::new();
            let events = event.map(Event::Mouse).into_iter().collect::<Vec<_>>();
            let _ = ui.update(
                &events,
                // Probe the fill-width part beyond the label, so the positive
                // controls also require the wrapper's native sizing contract.
                mouse::Cursor::Available(Point::new(220.0, 20.0)),
                &mut self.renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            self.cache = ui.into_cache();
            messages
        }

        fn press(&mut self, owner: u64, action: Action) {
            assert!(
                self.frame(
                    owner,
                    Some(action),
                    Some(mouse::Event::ButtonPressed(mouse::Button::Left)),
                )
                .is_empty()
            );
        }

        fn release(&mut self, owner: u64, action: Action) -> Vec<Action> {
            self.frame(
                owner,
                Some(action),
                Some(mouse::Event::ButtonReleased(mouse::Button::Left)),
            )
        }
    }

    #[test]
    fn changed_owner_cannot_lend_a_held_native_press_to_the_same_label_replacement() {
        for named in [false, true] {
            let mut frames = Frames::new(named);
            frames.press(1, Action::Original);
            assert!(frames.release(2, Action::Replacement).is_empty());
            frames.press(2, Action::Replacement);
            assert_eq!(
                frames.release(2, Action::Replacement),
                [Action::Replacement]
            );
        }
    }

    #[test]
    fn unchanged_owner_preserves_a_held_native_press_across_cached_frames() {
        for named in [false, true] {
            let mut frames = Frames::new(named);
            frames.press(1, Action::Original);
            assert!(frames.frame(1, Some(Action::Original), None).is_empty());
            assert_eq!(frames.release(1, Action::Original), [Action::Original]);
        }
    }

    #[test]
    fn returning_to_a_previous_owner_does_not_restore_its_old_native_press() {
        for named in [false, true] {
            let mut frames = Frames::new(named);
            frames.press(1, Action::Original);
            assert!(frames.frame(2, Some(Action::Replacement), None).is_empty());
            assert!(frames.release(1, Action::Original).is_empty());
            frames.press(1, Action::Original);
            assert_eq!(frames.release(1, Action::Original), [Action::Original]);
        }
    }

    #[test]
    fn disabled_then_enabled_replacement_requires_a_fresh_native_press() {
        for named in [false, true] {
            let mut frames = Frames::new(named);
            frames.press(1, Action::Original);
            assert!(frames.frame(2, None, None).is_empty());
            assert!(frames.release(3, Action::Replacement).is_empty());
            frames.press(3, Action::Replacement);
            assert_eq!(
                frames.release(3, Action::Replacement),
                [Action::Replacement]
            );
        }
    }
}
