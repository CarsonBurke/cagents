//! A visible target or action instance owns the native interaction state beneath it.
//! Keeping a static button ID across a replacement must not carry an earlier
//! pointer press into the new instance's release action.
use super::*;

/// Retain native interaction state only while the target and action instance match.
///
/// Include the target identity, generation, and action role in `key` when these can
/// change beneath a retained widget. An unchanged key preserves presses and focus;
/// a replacement key rebuilds the child state before it can act on the new target.
pub fn owned_surface<'a, Message: 'a, Key: Copy + Eq + 'static>(
    content: impl Into<Element<'a, Message>>,
    key: Key,
) -> Element<'a, Message> {
    Element::new(OwnedSurface {
        content: content.into(),
        key,
    })
}
struct OwnedSurface<'a, Message, Key> {
    content: Element<'a, Message>,
    key: Key,
}
struct OwnerState<Key>(Key);

impl<Message, Key: Copy + Eq + 'static> Widget<Message, Theme, Renderer>
    for OwnedSurface<'_, Message, Key>
{
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<OwnerState<Key>>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(OwnerState(self.key))
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        let changed = tree.state.downcast_ref::<OwnerState<Key>>().0 != self.key;
        if changed {
            let child = Tree::new(&self.content);
            // UserInterface extracts named states BEFORE diff and restores
            // them AFTER it. Evict this modal's old/new names as well as
            // rebuilding, or a static button ID resurrects pressed state.
            fn evict(tree: &Tree) {
                if let Some(Id(cosmic::iced::core::id::Internal::Custom(_, name))) = &tree.id {
                    tree::NAMED.with(|named| {
                        named.borrow_mut().remove(name);
                    });
                }
                for child in &tree.children {
                    evict(child);
                }
            }
            for child in &tree.children {
                evict(child);
            }
            evict(&child);
            tree.state = tree::State::new(OwnerState(self.key));
            tree.children = vec![child];
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
