//! Focused contracts against the UI's vendored iced core.

// Keep shared-cache artifacts tied to the checkout even outside the test harness.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

#[cfg(test)]
mod reconciliation {
    use iced::widget::{
        Id, Tree,
        tree::{NAMED, State, Tag},
    };
    use iced::{Layout, Widget, layout, mouse, renderer};
    use iced_core as iced;

    // These contracts reconcile state trees without creating or using a renderer.
    // An uninhabited renderer keeps that boundary explicit in debug and release.
    enum Renderer {}

    impl renderer::Renderer for Renderer {
        fn start_layer(&mut self, _: iced::Rectangle) {
            match *self {}
        }

        fn end_layer(&mut self) {
            match *self {}
        }

        fn start_transformation(&mut self, _: iced::Transformation) {
            match *self {}
        }

        fn end_transformation(&mut self) {
            match *self {}
        }

        fn fill_quad(&mut self, _: renderer::Quad, _: impl Into<iced::Background>) {
            match *self {}
        }

        fn reset(&mut self, _: iced::Rectangle) {
            match *self {}
        }

        fn allocate_image(
            &mut self,
            _: &iced::image::Handle,
            _: impl FnOnce(Result<iced::image::Allocation, iced::image::Error>) + Send + 'static,
        ) {
            match *self {}
        }
    }

    include!("../../../tests/support/iced_tree.rs");
}

/// A named widget rebuilt with a fresh numeric ID keeps the ID it already
/// published, so a queued native action still reaches the same control.
#[cfg(test)]
mod native_identity {
    use iced::widget::{
        Id, Tree, Widget,
        tree::{NAMED, State, Tag},
    };
    use iced::{Clipboard, Event, Layout, Length, Rectangle, Shell, Size, layout, mouse, renderer};
    use iced_accessibility::{A11yTree, IdEq, accesskit};
    use iced_core as iced;
    use std::marker::PhantomData;

    struct CacheReset;

    impl CacheReset {
        fn new() -> Self {
            NAMED.with(|named| named.borrow_mut().clear());
            Self
        }
    }

    impl Drop for CacheReset {
        fn drop(&mut self) {
            NAMED.with(|named| named.borrow_mut().clear());
        }
    }

    struct OriginalKind;
    struct ReplacementKind;

    struct ControlState(u32);

    /// A native-action-capable widget using the same numeric ID conversion
    /// and matching contract as the Winit backend and COSMIC buttons.
    struct Control<Kind> {
        id: Id,
        initial: u32,
        kind: PhantomData<Kind>,
    }

    impl<Kind> Control<Kind> {
        fn new(name: &str, initial: u32) -> Self {
            Self {
                id: Id::new(name.to_owned()),
                initial,
                kind: PhantomData,
            }
        }
    }

    impl<Kind: 'static> Widget<u32, (), ()> for Control<Kind> {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Shrink, Length::Shrink)
        }

        fn layout(
            &mut self,
            _tree: &mut Tree,
            _renderer: &(),
            _limits: &layout::Limits,
        ) -> layout::Node {
            layout::Node::new(Size::new(80.0, 24.0))
        }

        fn draw(
            &self,
            _tree: &Tree,
            _renderer: &mut (),
            _theme: &(),
            _style: &renderer::Style,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _viewport: &Rectangle,
        ) {
        }

        fn tag(&self) -> Tag {
            Tag::of::<(Kind, ControlState)>()
        }

        fn state(&self) -> State {
            State::new(ControlState(self.initial))
        }

        fn id(&self) -> Option<Id> {
            Some(self.id.clone())
        }

        fn set_id(&mut self, id: Id) {
            self.id = id;
        }

        fn a11y_nodes(
            &self,
            _layout: Layout<'_>,
            _tree: &Tree,
            _cursor: mouse::Cursor,
        ) -> A11yTree {
            let mut node = accesskit::Node::new(accesskit::Role::Button);
            node.add_action(accesskit::Action::Click);
            A11yTree::leaf(node, self.id.clone())
        }

        fn update(
            &mut self,
            tree: &mut Tree,
            event: &Event,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _renderer: &(),
            _clipboard: &mut dyn Clipboard,
            shell: &mut Shell<'_, u32>,
            _viewport: &Rectangle,
        ) {
            if let Event::A11y(id, request) = event
                && IdEq::eq(id, &self.id)
                && request.action == accesskit::Action::Click
            {
                let count = &mut tree.state.downcast_mut::<ControlState>().0;
                *count += 1;
                shell.publish(*count);
                shell.capture_event();
            }
        }
    }

    fn published_id(control: &dyn Widget<u32, (), ()>, tree: &Tree) -> accesskit::NodeId {
        let layout = layout::Node::new(Size::new(80.0, 24.0));
        let nodes = control.a11y_nodes(Layout::new(&layout), tree, mouse::Cursor::Unavailable);
        assert_eq!(nodes.root().len(), 1);
        nodes.root()[0].id().clone().into()
    }

    fn click(
        control: &mut dyn Widget<u32, (), ()>,
        tree: &mut Tree,
        published: accesskit::NodeId,
    ) -> Vec<u32> {
        let request = accesskit::ActionRequest {
            action: accesskit::Action::Click,
            target_tree: accesskit::TreeId::ROOT,
            target_node: published,
            data: None,
        };
        // Exactly the native conversion: the request has only the numeric ID,
        // not the logical name used when the widget was originally built.
        let event = Event::A11y(Id::from(u128::from(request.target_node.0) as u64), request);
        let layout = layout::Node::new(Size::new(80.0, 24.0));
        let mut messages = Vec::new();
        control.update(
            tree,
            &event,
            Layout::new(&layout),
            mouse::Cursor::Unavailable,
            &(),
            &mut iced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &Rectangle::with_size(Size::new(80.0, 24.0)),
        );
        messages
    }

    #[test]
    fn named_rebuild_keeps_published_native_action_and_state() {
        let _reset = CacheReset::new();
        for extract in [false, true] {
            let original = Control::<OriginalKind>::new("same-control", 7);
            let mut tree = Tree::new(&original as &dyn Widget<u32, (), ()>);
            let published = published_id(&original, &tree);
            tree.state.downcast_mut::<ControlState>().0 = 41;
            if extract {
                NAMED.with(|named| *named.borrow_mut() = tree.take_all_named());
            }
            let mut rebuilt = Control::<OriginalKind>::new("same-control", 99);
            assert_ne!(published_id(&rebuilt, &tree), published);
            tree.diff(&mut rebuilt as &mut dyn Widget<u32, (), ()>);
            assert_eq!(tree.state.downcast_ref::<ControlState>().0, 41);
            // Before the fix the state survives, but this native numeric
            // request matches no current widget and publishes no message.
            assert_eq!(click(&mut rebuilt, &mut tree, published), vec![42]);
            assert_eq!(published_id(&rebuilt, &tree), published);
            assert_eq!(tree.state.downcast_ref::<ControlState>().0, 42);
            assert!(NAMED.with(|named| named.borrow().is_empty()));
        }
    }

    #[test]
    fn different_named_owner_does_not_inherit_native_action_or_state() {
        let _reset = CacheReset::new();
        let original = Control::<OriginalKind>::new("old-control", 7);
        let mut tree = Tree::new(&original as &dyn Widget<u32, (), ()>);
        let published = published_id(&original, &tree);
        NAMED.with(|named| *named.borrow_mut() = tree.take_all_named());
        let mut replacement = Control::<OriginalKind>::new("new-control", 99);
        tree.diff(&mut replacement as &mut dyn Widget<u32, (), ()>);
        assert_eq!(tree.state.downcast_ref::<ControlState>().0, 99);
        assert_ne!(published_id(&replacement, &tree), published);
        assert!(click(&mut replacement, &mut tree, published).is_empty());
        let current = published_id(&replacement, &tree);
        assert_eq!(click(&mut replacement, &mut tree, current), vec![100]);
    }

    #[test]
    fn different_widget_tag_does_not_inherit_same_named_native_action() {
        let _reset = CacheReset::new();
        let original = Control::<OriginalKind>::new("same-name", 7);
        let mut tree = Tree::new(&original as &dyn Widget<u32, (), ()>);
        let published = published_id(&original, &tree);
        NAMED.with(|named| *named.borrow_mut() = tree.take_all_named());
        let mut replacement = Control::<ReplacementKind>::new("same-name", 99);
        tree.diff(&mut replacement as &mut dyn Widget<u32, (), ()>);
        assert_eq!(tree.tag, Tag::of::<(ReplacementKind, ControlState)>());
        assert_eq!(tree.state.downcast_ref::<ControlState>().0, 99);
        assert_ne!(published_id(&replacement, &tree), published);
        assert!(click(&mut replacement, &mut tree, published).is_empty());
        let current = published_id(&replacement, &tree);
        assert_eq!(click(&mut replacement, &mut tree, current), vec![100]);
    }

    #[test]
    fn unmounted_named_owner_does_not_reuse_retired_native_action() {
        let _reset = CacheReset::new();
        let original = Control::<OriginalKind>::new("reused-name", 7);
        let mut original_tree = Tree::new(&original as &dyn Widget<u32, (), ()>);
        let retired = published_id(&original, &original_tree);
        NAMED.with(|named| *named.borrow_mut() = original_tree.take_all_named());
        drop(original_tree);
        let mut tree = Tree::empty();
        let mut replacement = Control::<OriginalKind>::new("reused-name", 99);
        tree.diff(&mut replacement as &mut dyn Widget<u32, (), ()>);
        assert_eq!(tree.state.downcast_ref::<ControlState>().0, 99);
        assert_ne!(published_id(&replacement, &tree), retired);
        assert!(click(&mut replacement, &mut tree, retired).is_empty());
        let current = published_id(&replacement, &tree);
        assert_eq!(click(&mut replacement, &mut tree, current), vec![100]);
    }
}
