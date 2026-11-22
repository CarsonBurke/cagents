//! Focused contracts against the UI's vendored iced core and graphics.

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

/// The renderer keeps a group's prepared glyphs while its paragraphs and
/// editors are the ones it last drew. One reshaped, edited or dropped since is
/// not. Cached text the group drew stays shaped while the group is kept.
#[cfg(test)]
mod drawn_identity {
    use iced_core::text::{self, Editor as _, Paragraph as _, editor};
    use iced_core::{Font, Pixels, Size, alignment};
    use iced_graphics::text::{Editor, Paragraph, cache};

    fn paragraph(content: &str) -> Paragraph {
        Paragraph::with_text(text::Text {
            content,
            bounds: Size::new(200.0, 100.0),
            size: Pixels(14.0),
            line_height: text::LineHeight::default(),
            font: Font::DEFAULT,
            align_x: text::Alignment::Default,
            align_y: alignment::Vertical::Top,
            shaping: text::Shaping::Basic,
            wrapping: text::Wrapping::Word,
            ellipsize: text::Ellipsize::default(),
        })
    }

    #[test]
    fn a_drawn_paragraph_stays_the_same_only_until_reshaped_or_dropped() {
        let mut current = paragraph("Parser cache work");
        let drawn = current.downgrade();
        assert!(current.downgrade().is_same_live(&drawn));
        assert!(
            !paragraph("Parser cache work")
                .downgrade()
                .is_same_live(&drawn)
        );

        // Reshaping while the last frame still refers to the paragraph must
        // not leave that frame's glyphs looking current.
        current.resize(Size::new(80.0, 100.0));
        assert!(!current.downgrade().is_same_live(&drawn));

        let reshaped = current.downgrade();
        drop(current);
        assert!(!reshaped.is_same_live(&reshaped.clone()));
    }

    #[test]
    fn a_drawn_editor_stays_the_same_only_until_edited_or_dropped() {
        let mut current = Editor::with_text("Parser cache work");
        let drawn = current.downgrade();
        assert!(current.downgrade().is_same_live(&drawn));
        assert!(
            !Editor::with_text("Parser cache work")
                .downgrade()
                .is_same_live(&drawn)
        );

        current.perform(editor::Action::Edit(editor::Edit::Insert('!')));
        assert!(!current.downgrade().is_same_live(&drawn));

        let edited = current.downgrade();
        drop(current);
        assert!(!edited.is_same_live(&edited.clone()));
    }

    #[test]
    fn kept_cached_text_survives_a_trim() {
        let mut font_system = iced_graphics::text::font_system()
            .write()
            .expect("Write font system");
        let mut cache = iced_graphics::text::Cache::new();
        let key = |content| cache::Key {
            content,
            size: 14.0,
            line_height: 18.0,
            font: Font::DEFAULT,
            bounds: Size::new(200.0, 100.0),
            shaping: text::Shaping::Basic,
            align_x: text::Alignment::Default,
        };

        let (kept, _) = cache.allocate(font_system.raw(), key("Kept"));
        let (dropped, _) = cache.allocate(font_system.raw(), key("Dropped"));
        cache.trim();

        cache.keep(kept);
        cache.trim();
        assert!(cache.get(&kept).is_some());
        assert!(cache.get(&dropped).is_none());

        cache.trim();
        assert!(cache.get(&kept).is_none());
    }
}

/// The renderer presents a layer stack again without preparing it while the
/// stack's revision holds, so the revision must change with anything drawn,
/// clipped or reset. The renderer reads it before merging, the only other
/// change it makes, so merging must leave it alone.
#[cfg(test)]
mod layer_revision {
    use iced_core::{Point, Rectangle, Size};
    use iced_graphics::layer::{self, Stack};

    #[derive(Debug, Default)]
    struct Layer {
        bounds: Rectangle,
        primitives: usize,
    }

    impl layer::Layer for Layer {
        fn with_bounds(bounds: Rectangle) -> Self {
            Self {
                bounds,
                primitives: 0,
            }
        }

        fn bounds(&self) -> Rectangle {
            self.bounds
        }

        fn flush(&mut self) {}

        fn resize(&mut self, bounds: Rectangle) {
            self.bounds = bounds;
        }

        fn reset(&mut self) {
            self.primitives = 0;
        }

        fn start(&self) -> usize {
            usize::from(self.primitives > 0)
        }

        fn end(&self) -> usize {
            usize::from(self.primitives > 0)
        }

        fn merge(&mut self, layer: &mut Self) {
            self.primitives += std::mem::take(&mut layer.primitives);
        }
    }

    fn draw(stack: &mut Stack<Layer>) {
        stack.current_mut().0.primitives += 1;
    }

    #[test]
    fn a_stack_revision_changes_only_when_its_layers_may_have() {
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(8.0, 8.0));
        let mut stack = Stack::<Layer>::new();

        draw(&mut stack);
        stack.push_clip(bounds);
        draw(&mut stack);
        stack.pop_clip();

        let drawn = stack.revision();
        stack.merge();
        assert_eq!(stack.revision(), drawn, "merging is the renderer's own");

        draw(&mut stack);
        assert_ne!(stack.revision(), drawn, "drawing changes the layers");

        let drawn = stack.revision();
        stack.push_clip(bounds);
        assert_ne!(stack.revision(), drawn, "a clip adds a layer");

        let drawn = stack.revision();
        stack.reset(bounds);
        assert_ne!(stack.revision(), drawn, "a reset clears the layers");
    }
}

#[cfg(test)]
mod present_timing {
    use iced_graphics::present::{self, Stage};

    // A shell that does not trace frames never enables timing, and then
    // presenting reads no clock and reports nothing.
    #[test]
    fn untimed_until_enabled() {
        std::thread::spawn(|| {
            present::begin();
            present::lap(Stage::Acquire);
            present::layers(3);
            present::end();
            assert!(present::take().is_none());
        })
        .join()
        .unwrap();
    }

    // Each stage accumulates its laps, the layer count is kept, and a
    // presentation is reported once.
    #[test]
    fn reports_each_stage_once() {
        std::thread::spawn(|| {
            present::enable();
            present::begin();
            present::lap(Stage::Acquire);
            present::lap(Stage::Render);
            present::lap(Stage::Render);
            present::lap(Stage::Finish);
            present::layers(7);
            present::end();
            let timing = present::take().expect("a timed presentation");
            assert!(timing.total >= timing.acquire + timing.render + timing.finish);
            assert_eq!(timing.layers, Some(7));
            assert!(timing.cpu.is_some());
            assert!(present::take().is_none());

            // Laps outside a presentation are ignored.
            present::lap(Stage::Swap);
            assert!(present::take().is_none());
        })
        .join()
        .unwrap();
    }
}

#[cfg(test)]
mod subscription_events {
    //! The runtime's subscription tracker keeps every event for a listener
    //! whose stream waits on the runtime (#3644).

    use futures::channel::mpsc;
    use futures::executor::LocalPool;
    use futures::task::LocalSpawnExt;
    use iced_core::event::{self, Status};
    use iced_core::keyboard::{self, Key, Location, Modifiers, key};
    use iced_core::{Point, mouse, window};
    use iced_futures::subscription::{self, Event, Tracker};
    use std::time::{Duration, Instant};

    fn press(window: window::Id, index: usize) -> Event {
        Event::Interaction {
            window,
            event: event::Event::Keyboard(keyboard::Event::KeyPressed {
                key: Key::Character("a".into()),
                modified_key: Key::Character("a".into()),
                physical_key: key::Physical::Unidentified(key::NativeCode::Unidentified),
                location: Location::Standard,
                modifiers: Modifiers::empty(),
                text: Some(index.to_string().into()),
                repeat: false,
            }),
            status: Status::Ignored,
        }
    }

    fn redraw(window: window::Id, at: Instant) -> Event {
        Event::Interaction {
            window,
            event: event::Event::Window(window::Event::RedrawRequested(at)),
            status: Status::Ignored,
        }
    }

    fn cursor(window: window::Id, x: f32, status: Status) -> Event {
        Event::Interaction {
            window,
            event: event::Event::Mouse(mouse::Event::CursorMoved {
                position: Point::new(x, 0.0),
            }),
            status,
        }
    }

    /// A listener that forwards every event to a runtime that accepts one
    /// message at a time, as the shell's proxy does when it is full.
    struct Listener {
        tracker: Tracker,
        pool: LocalPool,
        messages: mpsc::Receiver<Event>,
    }

    impl Listener {
        fn new() -> Self {
            let (sender, messages) = mpsc::channel(0);
            let mut tracker = Tracker::new();
            let pool = LocalPool::new();
            let recipes = subscription::into_recipes(subscription::filter_map("listener", Some));

            for future in tracker.update(recipes.into_iter(), sender) {
                pool.spawner().spawn_local(future).unwrap();
            }

            Self {
                tracker,
                pool,
                messages,
            }
        }

        /// Runs the listener until it waits, without accepting anything.
        fn stall(&mut self) {
            self.pool.run_until_stalled();
        }

        /// Accepts messages until the listener has nothing more to send.
        fn drain(&mut self) -> Vec<Event> {
            let mut received = Vec::new();

            loop {
                self.pool.run_until_stalled();

                match self.messages.try_recv() {
                    Ok(message) => received.push(message),
                    Err(_) => return received,
                }
            }
        }
    }

    #[test]
    fn a_listener_waiting_on_the_runtime_loses_no_key() {
        let window = window::Id::unique();
        let start = Instant::now();
        let mut listener = Listener::new();

        listener.tracker.broadcast(press(window, 0));
        listener.stall();

        // The upstream tracker held 101 events here and dropped the rest.
        for index in 1..=300 {
            listener.tracker.broadcast(press(window, index));
            listener
                .tracker
                .broadcast(redraw(window, start + Duration::from_millis(index as u64)));
        }

        let keys: Vec<_> = listener
            .drain()
            .into_iter()
            .filter_map(|event| match event {
                Event::Interaction {
                    event: event::Event::Keyboard(keyboard::Event::KeyPressed { text, .. }),
                    ..
                } => text.map(|text| text.parse::<usize>().unwrap()),
                _ => None,
            })
            .collect();

        assert_eq!(keys, (0..=300).collect::<Vec<_>>());
    }

    #[test]
    fn redraws_and_cursor_moves_coalesce_only_across_each_other() {
        let window = window::Id::unique();
        let other = window::Id::unique();
        let start = Instant::now();
        let later = start + Duration::from_millis(16);
        let mut listener = Listener::new();

        listener.tracker.broadcast(press(window, 0));
        listener.stall();

        // A pointer moving during an animation alternates the two kinds.
        let queued = [
            cursor(window, 1.0, Status::Ignored),
            redraw(window, start),
            cursor(window, 2.0, Status::Ignored),
            redraw(other, start),
            cursor(window, 3.0, Status::Captured),
            redraw(window, later),
            press(window, 1),
            cursor(window, 4.0, Status::Captured),
            Event::SystemThemeChanged(iced_core::theme::Mode::Dark),
            Event::SystemThemeChanged(iced_core::theme::Mode::Dark),
        ];

        for event in queued {
            listener.tracker.broadcast(event);
        }

        assert_eq!(
            listener.drain(),
            [
                press(window, 0),
                cursor(window, 2.0, Status::Ignored),
                redraw(other, start),
                cursor(window, 3.0, Status::Captured),
                redraw(window, later),
                press(window, 1),
                cursor(window, 4.0, Status::Captured),
                Event::SystemThemeChanged(iced_core::theme::Mode::Dark),
                Event::SystemThemeChanged(iced_core::theme::Mode::Dark),
            ]
        );
    }
}
