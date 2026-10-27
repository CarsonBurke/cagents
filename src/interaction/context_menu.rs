//! Transparent native menu surface with a faithful widget-state tree.
//!
//! In pinned libcosmic caec74c, `ContextMenu::children` includes an unused
//! legacy menu subtree, but `ContextMenu::diff` retains only the content child.
//! Menus actually reconcile their items in `LocalState.menu_bar_state.inner`.
//! Declare the real stateless Space child here from the first frame, and leave
//! event handling, menu reconciliation and overlays to the native widget.
//!
//! The native widget's whole state therefore lives one level down, under a node
//! of ours that remembers how many items the menu had. See [`MenuSurface::diff`]
//! for why that indirection is load-bearing.

use super::*;

pub(super) fn surface<'a, Message: Clone + 'static>(
    menu: Vec<widget::menu::Tree<Message>>,
) -> Element<'a, Message> {
    let items = menu.len();
    Element::new(MenuSurface {
        native: widget::context_menu(
            cosmic::iced::widget::Space::new()
                .width(Length::Fill)
                .height(Length::Fill),
            Some(menu),
        ),
        items,
        builder: None,
        footprint: layout::Node::new(Size::ZERO),
    })
}

pub(super) fn lazy_surface<'a, Message: Clone + 'static>(
    builder: impl Fn() -> Vec<widget::menu::Tree<Message>> + 'a,
) -> Element<'a, Message> {
    Element::new(MenuSurface {
        native: widget::context_menu(
            cosmic::iced::widget::Space::new()
                .width(Length::Fill)
                .height(Length::Fill),
            Some(Vec::new()),
        ),
        items: 0,
        builder: Some(Box::new(builder)),
        footprint: layout::Node::new(Size::ZERO),
    })
}

/// How many items the menu had when the native widget last measured it.
///
/// This is the only quantity the retained menu geometry is indexed by: the
/// native widget stores one `MenuState` per open level, and its `MenuBounds`
/// holds a position and a size per item of the list that level was opened on.
/// Our menus are flat, so the top-level item count is their entire shape.
/// Retain the click in window space as well, to size the popup's anchor.
struct MenuItems {
    count: usize,
    anchor: Point,
}

struct MenuSurface<'a, Message> {
    native: widget::ContextMenu<'a, Message>,
    items: usize,
    builder: Option<Box<dyn Fn() -> Vec<widget::menu::Tree<Message>> + 'a>>,
    footprint: layout::Node,
}

impl<Message: Clone + 'static> MenuSurface<'_, Message> {
    fn build_menu(&mut self) {
        if let Some(builder) = &self.builder {
            let menu = builder();
            self.items = menu.len();
            self.native = widget::context_menu(
                cosmic::iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill),
                Some(menu),
            );
        }
    }
    /// The native widget's own tree, as `ContextMenu::diff` leaves it.
    ///
    /// `ContextMenu::children` also declares the legacy menu subtree, which the
    /// very first `diff` truncates away after cloning every menu item into it,
    /// so build the surviving shape directly instead.
    fn native_tree(&self) -> Tree {
        Tree {
            id: None,
            tag: self.native.tag(),
            state: self.native.state(),
            // The only content is Space, whose native tree is stateless and empty.
            children: vec![Tree::empty()],
        }
    }
}

impl<Message: Clone + 'static> Widget<Message, Theme, Renderer> for MenuSurface<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<MenuItems>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(MenuItems {
            count: self.items,
            anchor: Point::ORIGIN,
        })
    }

    fn children(&self) -> Vec<Tree> {
        vec![self.native_tree()]
    }

    fn diff(&mut self, tree: &mut Tree) {
        if tree.state.downcast_ref::<MenuItems>().count > 0 {
            self.build_menu();
        }
        // An open menu measures its items once, at the click that opens it, and
        // then keeps that geometry in the state its overlay reads every frame.
        // The roots the overlay walks are this frame's, though: `ContextMenu::
        // diff` reconciles the item widget trees against them and the overlay
        // clones them. A row whose menu gains or loses an item while it is open
        // therefore hands the overlay a list of a different length than the
        // geometry describing it, and the overlay indexes one by the other.
        //
        // Holding the native state in a child makes that unrepresentable rather
        // than merely unlikely: the item count the geometry was measured for is
        // recorded beside that child, and a count that no longer matches
        // discards the geometry and the roots together, leaving a closed menu
        // and nothing stale to read. Reconciling the retained geometry in place
        // is not ours to do, since the native `LocalState` is opaque to us, and
        // an open menu that silently restructures under the pointer is a
        // mis-click waiting to happen either way.
        let measured = tree.state.downcast_mut::<MenuItems>();
        let remeasured = measured.count != self.items;
        measured.count = self.items;
        if remeasured || tree.children.is_empty() {
            tree.children = vec![self.native_tree()];
        }
        let native: &mut dyn Widget<Message, Theme, Renderer> = &mut self.native;
        tree.children[0].diff(native);
    }

    fn size(&self) -> Size<Length> {
        self.native.size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.native.layout(&mut tree.children[0], renderer, limits)
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
        // A freshly mounted row may be right-clicked before a view diff. The
        // native widget initializes its *real* menu item tree only in diff(),
        // not in state()/children(); opening first otherwise leaves an empty
        // tree for the overlay. Reconcile at the opening boundary as well.
        let opening = matches!(
            event,
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right))
                | Event::Touch(cosmic::iced::touch::Event::FingerLifted { .. })
        ) && layout
            .bounds()
            .intersection(viewport)
            .is_some_and(|bounds| cursor.is_over(bounds));
        if opening {
            if self.builder.is_some() && self.items == 0 {
                self.build_menu();
                tree.state.downcast_mut::<MenuItems>().count = self.items;
                tree.children = vec![self.native_tree()];
            }
            self.native.diff(&mut tree.children[0]);
        }
        // The native widget records the anchor it opens at from this cursor,
        // and the runtime drives the overlay it returns in window space. A
        // scrollable, though, hands its content an untranslated layout and a
        // cursor moved into content space, and the two spaces disagree by the
        // scroll offset. Hand the native widget the window-space view of the
        // same row, so that its anchor means to the overlay what the overlay
        // takes it to mean: its menu initializes, paints and hits in one
        // space, and keeps measuring its side, its slice and its own
        // scrolling against the window the pointer is in. Its content here is
        // a `Space`, so a childless node standing in for the row is all the
        // native widget needs.
        //
        // The offset has to come from the layout metadata: the only other
        // place the runtime states it is `overlay`'s translation, and by then
        // the anchor is already recorded. That rests on the invariant
        // `virtual_offset == -translation`, which the scrollable maintains,
        // and on every widget between it and this one *re-applying* the
        // offset to the layout it hands down. Reading it is not enough, and
        // `Layout::children` resets it to zero, so a wrapper that translates
        // an overlay without re-applying the offset would silently anchor
        // that one menu in content space again.
        let scroll = layout.virtual_offset();
        if opening {
            tree.state.downcast_mut::<MenuItems>().anchor =
                cursor.position().unwrap_or_default() - scroll;
        }
        let row = layout.bounds();
        let anchor = layout::Node::new(row.size()).move_to(row.position() - scroll);
        self.native.update(
            &mut tree.children[0],
            event,
            Layout::new(&anchor),
            cursor - scroll,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if opening {
            shell.invalidate_layout();
            shell.request_redraw();
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
        self.native.draw(
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
        self.native.a11y_nodes(layout, &tree.children[0], cursor)
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        _layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        _translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        if self.builder.is_some() && self.items == 0 {
            return None;
        }
        // The anchor is already a window-space point, which is the space the
        // runtime drives overlays in, so the native menu needs no translation:
        // it derives one offset for its layout and the opposite one for its
        // hit tests, and the two only agree at none at all.
        // COSMIC uses the content bounds as the popup's parent footprint,
        // moved to the click. A full-width file row therefore adds its width
        // to a leftward popup, placing its right edge beyond the window.
        // Supply a popup footprint instead. The pinned native menu is 240px
        // wide; its overlapping placement aligns either left or right edges.
        // Shortening this footprint near the right edge makes both choices
        // keep the popup inside the window, including clicks near its center
        // where neither side of a point anchor has enough room.
        let anchor = tree.state.downcast_ref::<MenuItems>().anchor;
        let menu_width = 240.0;
        // A scrollable may supply only its clipped viewport. Its right edge
        // is in window space just like the anchor; its width alone is not.
        let right = viewport.x + viewport.width;
        let left = anchor.x.clamp(0.0, (right - menu_width).max(0.0));
        // Keep the anchor nonempty for native containment checks, but below
        // one pixel high so its integer vertical offset rounds to zero. That
        // lets an upward menu end before the click, even at the bottom edge.
        self.footprint = layout::Node::new(Size::new(
            (left + menu_width - anchor.x).max(f32::EPSILON),
            0.5,
        ));
        let native = self.native.overlay(
            &mut tree.children[0],
            Layout::new(&self.footprint),
            renderer,
            viewport,
            Vector::ZERO,
        );
        if native.is_none() && self.builder.is_some() {
            tree.state.downcast_mut::<MenuItems>().count = 0;
            self.items = 0;
        }
        native.map(|inner| overlay::Element::new(Box::new(PointerAwareMenu { inner, anchor })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::advanced::{clipboard, renderer::Headless};

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct Action;

    impl widget::menu::Action for Action {
        type Message = ();

        fn message(&self) {}
    }

    #[test]
    fn full_width_rows_keep_popups_inside_the_window() {
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("executor")
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .expect("headless renderer");
        for (window, viewport, positions) in [
            (
                Size::new(400.0, 400.0),
                Rectangle::new(Point::ORIGIN, Size::new(400.0, 400.0)),
                vec![
                    Point::new(20.0, 16.0),
                    Point::new(180.0, 16.0),
                    Point::new(220.0, 16.0),
                    Point::new(390.0, 16.0),
                    Point::new(390.0, 380.0),
                    Point::new(399.75, 399.75),
                ],
            ),
            (
                Size::new(1902.0, 2100.0),
                Rectangle::new(Point::new(1519.0, 0.0), Size::new(383.0, 2100.0)),
                vec![Point::new(1586.0, 299.0), Point::new(1901.75, 299.0)],
            ),
        ] {
            for position in positions {
                let mut element: Element<'_, ()> = surface(widget::menu::items(
                    &std::collections::HashMap::new(),
                    vec![widget::menu::Item::Button("Action", None, Action)],
                ));
                let widget = element.as_widget_mut();
                let mut tree = Tree::new(&*widget);
                widget.diff(&mut tree);
                let row = widget.layout(&mut tree, &renderer, &layout::Limits::new(window, window));
                let mut messages = Vec::new();
                widget.update(
                    &mut tree,
                    &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
                    Layout::new(&row),
                    mouse::Cursor::Available(position),
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
                let mut popup = widget
                    .overlay(
                        &mut tree,
                        Layout::new(&row),
                        &renderer,
                        &viewport,
                        Vector::ZERO,
                    )
                    .expect("opened menu");
                let popup_layout = popup.as_overlay_mut().layout(&renderer, window);
                popup.as_overlay_mut().update(
                    &Event::Window(window::Event::RedrawRequested(Instant::now())),
                    Layout::new(&popup_layout),
                    mouse::Cursor::Available(position),
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                );
                let popup_layout = popup.as_overlay_mut().layout(&renderer, window);
                let menu = popup_layout.children().first().expect("measured menu");
                assert!(
                    menu.bounds().is_within(&viewport),
                    "{position:?}: {:?}",
                    menu.bounds()
                );
                let pointer = mouse::Cursor::Available(menu.bounds().center());
                for event in [
                    mouse::Event::ButtonPressed(mouse::Button::Left),
                    mouse::Event::ButtonReleased(mouse::Button::Left),
                ] {
                    popup.as_overlay_mut().update(
                        &Event::Mouse(event),
                        Layout::new(&popup_layout),
                        pointer,
                        &renderer,
                        &mut clipboard::Null,
                        &mut Shell::new(&mut messages),
                    );
                }
                assert_eq!(
                    messages,
                    [()],
                    "the visible popup must remain clickable at {position:?}"
                );
            }
        }
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct MenuAction(usize);

    impl widget::menu::Action for MenuAction {
        type Message = usize;
        fn message(&self) -> usize {
            self.0
        }
    }

    // The real native menu, retained Tree and Overlay layout, with no fake
    // accessibility nodes or dispatch shim. Events follow overlay-before-base
    // delivery just as UserInterface does. No app/daemon/provider is launched.
    struct NativeMenu {
        element: Element<'static, usize>,
        tree: Tree,
        renderer: Renderer,
        row: layout::Node,
        viewport: Rectangle,
        anchor: Point,
    }

    impl NativeMenu {
        fn new(count: usize, viewport: Size, anchor: Point) -> Self {
            let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let entries = (0..count)
                .map(|index| {
                    if index == 1 {
                        widget::menu::Item::ButtonDisabled(
                            "Copy branch (detached HEAD)".to_owned(),
                            None,
                            MenuAction(index),
                        )
                    } else {
                        widget::menu::Item::Button(
                            format!("Action {index}"),
                            None,
                            MenuAction(index),
                        )
                    }
                })
                .collect();
            let mut element = surface(widget::menu::items(
                &std::collections::HashMap::new(),
                entries,
            ));
            let mut tree = Tree::new(element.as_widget());
            element.as_widget_mut().diff(&mut tree);
            let row = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(viewport, viewport),
            );
            Self {
                element,
                tree,
                renderer,
                row,
                viewport: Rectangle::with_size(viewport),
                anchor,
            }
        }

        fn event(&mut self, event: Event, cursor: mouse::Cursor) -> Vec<usize> {
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            {
                let widget = self.element.as_widget_mut();
                if let Some(mut overlay) = widget.overlay(
                    &mut self.tree,
                    Layout::new(&self.row),
                    &self.renderer,
                    &self.viewport,
                    Vector::ZERO,
                ) {
                    let node = overlay
                        .as_overlay_mut()
                        .layout(&self.renderer, self.viewport.size());
                    overlay.as_overlay_mut().update(
                        &event,
                        Layout::new(&node),
                        cursor,
                        &self.renderer,
                        &mut clipboard::Null,
                        &mut shell,
                    );
                }
            }
            if !shell.is_event_captured() {
                self.element.as_widget_mut().update(
                    &mut self.tree,
                    &event,
                    Layout::new(&self.row),
                    cursor,
                    &self.renderer,
                    &mut clipboard::Null,
                    &mut shell,
                    &self.viewport,
                );
            }
            drop(shell);
            self.measure();
            messages
        }

        fn measure(&mut self) {
            self.row = self.element.as_widget_mut().layout(
                &mut self.tree,
                &self.renderer,
                &layout::Limits::new(self.viewport.size(), self.viewport.size()),
            );
            if let Some(mut overlay) = self.element.as_widget_mut().overlay(
                &mut self.tree,
                Layout::new(&self.row),
                &self.renderer,
                &self.viewport,
                Vector::ZERO,
            ) {
                let _ = overlay
                    .as_overlay_mut()
                    .layout(&self.renderer, self.viewport.size());
            }
        }

        fn open(&mut self) {
            assert!(
                self.event(
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
                    mouse::Cursor::Available(self.anchor)
                )
                .is_empty()
            );
            assert!(
                self.event(
                    Event::Window(window::Event::RedrawRequested(Instant::now())),
                    mouse::Cursor::Available(self.anchor)
                )
                .is_empty()
            );
        }

        fn replace_owned(&mut self, owner: u64, offset: usize) {
            let entries = (0..3)
                .map(|index| {
                    widget::menu::Item::Button(
                        format!("Action {index}"),
                        None,
                        MenuAction(index + offset),
                    )
                })
                .collect();
            self.element = crate::ui_controls::owned(
                owner,
                surface(widget::menu::items(
                    &std::collections::HashMap::new(),
                    entries,
                )),
            );
            self.tree.diff(self.element.as_widget_mut());
            self.measure();
        }

        fn nodes(&self) -> iced_accessibility::A11yTree {
            self.element.as_widget().a11y_nodes(
                Layout::new(&self.row),
                &self.tree,
                mouse::Cursor::Unavailable,
            )
        }

        fn named(&self, name: &str) -> iced_accessibility::A11yNode {
            let nodes = self.nodes();
            nodes
                .root()
                .iter()
                .chain(nodes.children())
                .find(|n| n.node().label() == Some(name))
                .expect(name)
                .clone()
        }

        fn action(
            &mut self,
            node: &iced_accessibility::A11yNode,
            action: iced_accessibility::accesskit::Action,
        ) -> Vec<usize> {
            use iced_accessibility::accesskit::{ActionRequest, NodeId, TreeId};
            let target: NodeId = node.id().clone().into();
            self.event(
                Event::A11y(
                    Id::from(target.0),
                    ActionRequest {
                        action,
                        target_tree: TreeId::ROOT,
                        target_node: target,
                        data: None,
                    },
                ),
                mouse::Cursor::Unavailable,
            )
        }
    }

    #[test]
    fn context_menu_exports_named_real_buttons_and_targets_at_without_hover() {
        use iced_accessibility::accesskit::{Action, Role};
        let mut menu = NativeMenu::new(3, Size::new(900.0, 650.0), Point::new(880.0, 620.0));
        assert!(
            menu.nodes().root().is_empty(),
            "closed native menu has no advertised targets"
        );
        menu.open();
        let disabled = menu.named("Copy branch (detached HEAD)");
        assert_eq!(disabled.node().role(), Role::Button);
        assert!(disabled.node().is_disabled());
        let current = menu.named("Action 2");
        assert_eq!(current.node().role(), Role::Button);
        assert!(current.node().supports_action(Action::Click));
        assert_ne!(current.id(), disabled.id());
        assert!(menu.action(&disabled, Action::Click).is_empty());
        assert!(menu.action(&disabled, Action::Focus).is_empty());
        let first = menu.named("Action 0").node().bounds().unwrap();
        let point = Point::new(
            ((first.x0 + first.x1) / 2.0) as f32,
            ((first.y0 + first.y1) / 2.0) as f32,
        );
        assert!(
            menu.event(
                Event::Mouse(mouse::Event::CursorMoved { position: point }),
                mouse::Cursor::Available(point)
            )
            .is_empty()
        );
        // A different row is really hovered. No AXFocus has selected Action2;
        // Click must use its advertised numeric ID with an unavailable cursor.
        assert_eq!(menu.action(&current, Action::Click), [2]);
        assert!(
            menu.nodes().root().is_empty(),
            "ordinary action retires the open menu"
        );
        assert!(menu.action(&current, Action::Click).is_empty());
        menu.open();
        let current = menu.named("Action 2");
        assert!(
            !current.node().supports_action(Action::Focus),
            "Focus is not implemented by this bounded menu AX path"
        );
        assert!(menu.action(&current, Action::Focus).is_empty());
        assert_eq!(menu.named("Action 2").id(), current.id());
        assert_eq!(menu.action(&current, Action::Click), [2]);
    }

    #[test]
    fn context_menu_reopen_renews_actual_ids_but_redraw_keeps_them() {
        use iced_accessibility::accesskit::Action;
        let mut menu = NativeMenu::new(3, Size::new(900.0, 650.0), Point::new(20.0, 20.0));
        menu.open();
        let old = menu.named("Action 0");
        assert!(
            menu.event(
                Event::Window(window::Event::RedrawRequested(Instant::now())),
                mouse::Cursor::Unavailable
            )
            .is_empty()
        );
        assert_eq!(menu.named("Action 0").id(), old.id());
        assert_eq!(menu.action(&old, Action::Click), [0]);
        // Reopen this SAME native widget/Tree, not a newly built fixture.
        menu.open();
        let current = menu.named("Action 0");
        assert_ne!(current.id(), old.id());
        assert!(menu.action(&old, Action::Click).is_empty());
        assert_eq!(menu.action(&current, Action::Click), [0]);
    }

    #[test]
    fn context_menu_scroll_and_resize_publish_only_the_actual_visible_slice() {
        use iced_accessibility::accesskit::Action;
        let mut menu = NativeMenu::new(40, Size::new(360.0, 220.0), Point::new(20.0, 20.0));
        menu.open();
        let first = menu.named("Action 0");
        assert!(
            menu.event(
                Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Lines { x: 0.0, y: -30.0 }
                }),
                mouse::Cursor::Available(Point::new(230.0, 110.0))
            )
            .is_empty()
        );
        assert!(
            menu.action(&first, Action::Click).is_empty(),
            "scrolled-out numeric target is retired"
        );
        menu.viewport = Rectangle::with_size(Size::new(320.0, 180.0));
        menu.measure();
        let nodes = menu.nodes();
        let buttons: Vec<_> = nodes
            .root()
            .iter()
            .chain(nodes.children())
            .filter(|n| {
                n.node().role() == iced_accessibility::accesskit::Role::Button
                    && !n.node().is_disabled()
            })
            .collect();
        assert!(!buttons.is_empty());
        for button in &buttons {
            let b = button.node().bounds().unwrap();
            assert!(
                b.x0 >= 0.0 && b.y0 >= 0.0 && b.x1 <= 320.01 && b.y1 <= 180.01,
                "actual visible bounds: {b:?}"
            );
        }
        let current = (*buttons.last().unwrap()).clone();
        let expected: usize = current
            .node()
            .label()
            .unwrap()
            .strip_prefix("Action ")
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(menu.action(&current, Action::Click), [expected]);
    }
    #[test]
    fn context_menu_exact_owner_retires_same_count_callbacks_and_ids() {
        use iced_accessibility::accesskit::Action;
        let mut menu = NativeMenu::new(3, Size::new(900.0, 650.0), Point::new(20.0, 20.0));
        menu.replace_owned(1, 0);
        menu.open();
        let old = menu.named("Action 0");
        // Real native cache/diff with the same semantic owner retains IDs.
        menu.replace_owned(1, 0);
        assert_eq!(menu.named("Action 0").id(), old.id());
        assert_eq!(menu.action(&old, Action::Click), [0]);
        menu.open();
        let old = menu.named("Action 0");
        // Same labels/count, new owner and callback. The complete menu subtree
        // must retire, not keep its old pointer/AX target in a recycled row.
        menu.replace_owned(2, 100);
        assert!(menu.nodes().root().is_empty());
        assert!(menu.action(&old, Action::Click).is_empty());
        menu.open();
        let current = menu.named("Action 0");
        assert_ne!(old.id(), current.id());
        assert!(menu.action(&old, Action::Focus).is_empty());
        assert_eq!(menu.action(&current, Action::Click), [100]);
    }
}

/// COSMIC dispatches presses only to its last hovered item. Wayland can update
/// the cursor on surface entry without a CursorMoved event, so derive that
/// item from the actual press position instead of requiring prior motion.
struct PointerAwareMenu<'a, Message> {
    inner: overlay::Element<'a, Message, Theme, Renderer>,
    anchor: Point,
}
impl<Message> overlay::Overlay<Message, Theme, Renderer> for PointerAwareMenu<'_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        self.inner.as_overlay_mut().layout(renderer, bounds)
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
        let press = match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => cursor.position(),
            _ => None,
        };
        let mut messages = Vec::new();
        let mut motion = Shell::new(&mut messages);
        // Native menus measure only on their first update, while the cursor
        // is still inside the parent anchor. Motion can arrive before the
        // opening redraw, so initialize at the retained click before passing
        // that motion on. Once measured, the overlay has a menu child and
        // subsequent updates preserve its normal hover and scrolling state.
        if layout.children().next().is_none() {
            self.inner.as_overlay_mut().update(
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: self.anchor,
                }),
                layout,
                mouse::Cursor::Available(self.anchor),
                renderer,
                clipboard,
                &mut motion,
            );
        }
        if let Some(position) = press {
            self.inner.as_overlay_mut().update(
                &Event::Mouse(mouse::Event::CursorMoved { position }),
                layout,
                mouse::Cursor::Available(position),
                renderer,
                clipboard,
                &mut motion,
            );
        }
        // Merge afterwards: motion's capture must not make the item's button
        // skip the real press. Preserve any submenu layout/redraw requests.
        self.inner
            .as_overlay_mut()
            .update(event, layout, cursor, renderer, clipboard, shell);
        shell.merge(motion, std::convert::identity);
    }
    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.inner
            .as_overlay()
            .draw(renderer, theme, style, layout, cursor);
    }
    fn operate(&mut self, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.inner
            .as_overlay_mut()
            .operate(layout, renderer, operation);
    }
    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.inner
            .as_overlay()
            .mouse_interaction(layout, cursor, renderer)
    }
    fn overlay<'a>(
        &'a mut self,
        layout: Layout<'a>,
        renderer: &Renderer,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.inner.as_overlay_mut().overlay(layout, renderer)
    }
    fn index(&self) -> f32 {
        self.inner.as_overlay().index()
    }
}
