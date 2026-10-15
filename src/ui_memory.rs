//! Explicit residency boundaries for disposable presentation widget state.
//!
//! A parked workspace tree preserves inputs and other interactive state. A
//! gated subtree instead rebuilds from its model when mounted again; this
//! releases retained elements, shaping buffers and layout caches while hidden.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget,
    clipboard::DndDestinationRectangles,
    layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector, mouse};
use cosmic::{Element, Renderer, Theme};

/// Mark presentation state that can be discarded while its workspace card is
/// parked. Logical reader state belongs in the model outside this boundary.
pub fn gate<'a, Message: 'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    scoped_gate(content, 0)
}

/// Recreate only this disposable subtree on an actual transcript reset.
/// The model must keep the scope stable across publication, reflow and hide.
/// The composer and other durable interaction state belong outside this gate.
pub fn scoped_gate<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    scope: u64,
) -> Element<'a, Message> {
    Element::new(Gate {
        content: content.into(),
        scope,
    })
}

struct Gate<'a, Message> {
    content: Element<'a, Message>,
    scope: u64,
}
struct Residency {
    scope: u64,
}

/// Called after reclaiming named state, before parking a tree. Traversal stops
/// at each boundary: dropping its children releases the whole presentation.
pub(crate) fn release_parked(tree: &mut Tree) {
    if tree.tag == tree::Tag::of::<Residency>() {
        tree.children = Vec::new();
    } else {
        for child in &mut tree.children {
            release_parked(child);
        }
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Gate<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Residency>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(Residency { scope: self.scope })
    }

    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }

    fn diff(&mut self, tree: &mut Tree) {
        if tree.state.downcast_ref::<Residency>().scope != self.scope {
            // UserInterface extracts named descendants before diff. Consume
            // only this old subtree's extracted owners before discarding it,
            // otherwise same-ID restoration can resurrect pre-reset readers.
            cosmic::iced::runtime::user_interface::restore_cached_named_states(tree);
            tree.state.downcast_mut::<Residency>().scope = self.scope;
            tree.children = vec![Tree::new(&self.content)];
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
        renderer: &Renderer,
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
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::{Arc, Weak};

    struct Probe(Rc<RefCell<Weak<()>>>);

    impl Widget<(), Theme, Renderer> for Probe {
        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<Arc<()>>()
        }

        fn state(&self) -> tree::State {
            let owner = Arc::new(());
            *self.0.borrow_mut() = Arc::downgrade(&owner);
            tree::State::new(owner)
        }

        fn size(&self) -> Size<Length> {
            Size::new(Length::Shrink, Length::Shrink)
        }

        fn layout(&mut self, _: &mut Tree, _: &Renderer, _: &layout::Limits) -> layout::Node {
            layout::Node::new(Size::ZERO)
        }

        fn draw(
            &self,
            _: &Tree,
            _: &mut Renderer,
            _: &Theme,
            _: &renderer::Style,
            _: Layout<'_>,
            _: mouse::Cursor,
            _: &Rectangle,
        ) {
        }
    }

    #[test]
    fn parking_drops_only_disposable_state_and_remount_rebuilds_it() {
        let presentation = Rc::new(RefCell::new(Weak::new()));
        let composer = Rc::new(RefCell::new(Weak::new()));
        let mut gated = gate(Element::new(Probe(presentation.clone())));
        let input = Element::new(Probe(composer.clone()));
        let mut root = Tree::empty();
        root.children = vec![Tree::new(&gated), Tree::new(&input)];
        let old_presentation = presentation.borrow().clone();
        let old_composer = composer.borrow().clone();
        assert!(old_presentation.upgrade().is_some());
        assert!(
            root.children[0].id.is_none(),
            "the gate never aliases its child's widget ID"
        );
        release_parked(&mut root);
        assert!(old_presentation.upgrade().is_none());
        assert!(old_composer.upgrade().is_some());
        // Repeated parking is harmless; remount runs normal child diffing.
        release_parked(&mut root);
        root.children[0].diff(&mut gated);
        assert!(presentation.borrow().upgrade().is_some());
        assert!(old_presentation.upgrade().is_none());
        assert!(old_composer.upgrade().is_some());
    }
    #[test]
    fn actual_scope_change_discards_extracted_named_descendants_but_preserves_composer() {
        let presentation = Rc::new(RefCell::new(Weak::new()));
        let composer = Rc::new(RefCell::new(Weak::new()));
        let make = |scope| {
            scoped_gate(
                cosmic::widget::container(Element::new(Probe(presentation.clone())))
                    .id(cosmic::widget::Id::new("scope.transcript.reader")),
                scope,
            )
        };
        let mut gate = make(7);
        let input: Element<'_, ()> = cosmic::widget::container(Element::new(Probe(composer)))
            .id(cosmic::widget::Id::new("scope.composer"))
            .into();
        let mut root = Tree::empty();
        root.children = vec![Tree::new(&gate), Tree::new(&input)];
        let old_reader = presentation.borrow().clone();
        let old_composer = root.children[1].children[0]
            .state
            .downcast_ref::<Arc<()>>()
            .clone();
        let weak_composer = Arc::downgrade(&old_composer);
        drop(old_composer);
        tree::NAMED.with(|named| *named.borrow_mut() = root.take_all_named());
        root.children[0].diff(&mut gate);
        cosmic::iced::runtime::user_interface::restore_cached_named_states(&mut root);
        assert!(
            old_reader.upgrade().is_some(),
            "same-scope publication preserves native state"
        );
        assert!(weak_composer.upgrade().is_some());
        // Identical named IDs pass through the real extraction/restoration
        // cycle; resetting unnamed Tree children alone cannot prove this.
        tree::NAMED.with(|named| *named.borrow_mut() = root.take_all_named());
        let mut reset = make(8);
        root.children[0].diff(&mut reset);
        cosmic::iced::runtime::user_interface::restore_cached_named_states(&mut root);
        assert!(
            old_reader.upgrade().is_none(),
            "same-ID old named reader cannot resurrect after Reset"
        );
        assert!(
            presentation.borrow().upgrade().is_some(),
            "fresh reader installed"
        );
        assert!(
            weak_composer.upgrade().is_some(),
            "composer remains outside disposable scope"
        );
        assert!(tree::NAMED.with(|named| named.borrow().is_empty()));
    }
}
