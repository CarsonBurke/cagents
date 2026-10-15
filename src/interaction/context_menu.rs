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
    surface_keyed((), menu)
}

pub(super) fn surface_keyed<'a, Message: Clone + 'static, Key: Clone + Eq + 'static>(
    identity: Key,
    menu: Vec<widget::menu::Tree<Message>>,
) -> Element<'a, Message> {
    let items = menu.len();
    Element::new(MenuSurface {
        identity,
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
    lazy_surface_keyed((), builder)
}

pub(super) fn lazy_surface_keyed<'a, Message: Clone + 'static, Key: Clone + Eq + 'static>(
    identity: Key,
    builder: impl Fn() -> Vec<widget::menu::Tree<Message>> + 'a,
) -> Element<'a, Message> {
    Element::new(MenuSurface {
        identity,
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
struct MenuItems<Key> {
    identity: Key,
    count: usize,
    anchor: Point,
}

struct MenuSurface<'a, Message, Key> {
    identity: Key,
    native: widget::ContextMenu<'a, Message>,
    items: usize,
    builder: Option<Box<dyn Fn() -> Vec<widget::menu::Tree<Message>> + 'a>>,
    footprint: layout::Node,
}

impl<Message: Clone + 'static, Key> MenuSurface<'_, Message, Key> {
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

impl<Message: Clone + 'static, Key: Clone + Eq + 'static> Widget<Message, Theme, Renderer> for MenuSurface<'_, Message, Key> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<MenuItems<Key>>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(MenuItems {
            identity: self.identity.clone(),
            count: self.items,
            anchor: Point::ORIGIN,
        })
    }

    fn children(&self) -> Vec<Tree> {
        vec![self.native_tree()]
    }

    fn diff(&mut self, tree: &mut Tree) {
        if tree.state.downcast_ref::<MenuItems<Key>>().identity != self.identity {
            // Geometry and actions belong to the original target. Recycled
            // rows and refreshed models must not silently retarget an open
            // menu merely because the new menu has the same number of items.
            tree.state = self.state();
            tree.children = vec![self.native_tree()];
        }
        if tree.state.downcast_ref::<MenuItems<Key>>().count > 0 {
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
        let measured = tree.state.downcast_mut::<MenuItems<Key>>();
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
                tree.state.downcast_mut::<MenuItems<Key>>().count = self.items;
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
            tree.state.downcast_mut::<MenuItems<Key>>().anchor =
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
        let anchor = tree.state.downcast_ref::<MenuItems<Key>>().anchor;
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
            tree.state.downcast_mut::<MenuItems<Key>>().count = 0;
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
    fn diff_context_menu_identity_closes_same_count_recycled_targets() {
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all().build().unwrap()
            .block_on(Renderer::new(cosmic::iced::Font::default(), cosmic::iced::Pixels(14.0), Some("tiny-skia")))
            .unwrap();
        fn menu() -> Vec<widget::menu::Tree<()>> {
            widget::menu::items(&std::collections::HashMap::new(), vec![widget::menu::Item::Button("Action", None, Action)])
        }
        let make = |lazy, key| -> Element<'static, ()> {
            if lazy { lazy_surface_keyed(key, menu) }
            else { surface_keyed(key, menu()) }
        };
        let window = Size::new(400.0, 200.0);
        let viewport = Rectangle::with_size(window);
        let cursor = mouse::Cursor::Available(Point::new(20.0, 16.0));
        // The key models (source revision, row, selection revision). Equal
        // item counts deliberately cannot distinguish these target changes.
        let original = (1_u64, 10_usize, 2_u64);
        for lazy in [false, true] {
            for replacement in [(2,10,2), (1,11,2), (1,10,3)] {
                let mut element = make(lazy, original);
                let widget = element.as_widget_mut();
                let mut tree = Tree::new(&*widget);
                widget.diff(&mut tree);
                let row = widget.layout(&mut tree, &renderer, &layout::Limits::new(window, window));
                let mut messages = Vec::new();
                widget.update(&mut tree, &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)), Layout::new(&row), cursor, &renderer, &mut clipboard::Null, &mut Shell::new(&mut messages), &viewport);
                {
                    let mut popup = widget.overlay(&mut tree, Layout::new(&row), &renderer, &viewport, Vector::ZERO).expect("menu opens for original target");
                    let popup_node = popup.as_overlay_mut().layout(&renderer, window);
                    popup.as_overlay_mut().update(&Event::Window(window::Event::RedrawRequested(Instant::now())), Layout::new(&popup_node), cursor, &renderer, &mut clipboard::Null, &mut Shell::new(&mut messages));
                }
                for key in [original, replacement] {
                    let mut next = make(lazy, key);
                    let widget = next.as_widget_mut();
                    widget.diff(&mut tree);
                    let row = widget.layout(&mut tree, &renderer, &layout::Limits::new(window, window));
                    let open = widget.overlay(&mut tree, Layout::new(&row), &renderer, &viewport, Vector::ZERO).is_some();
                    assert_eq!(open, key == original, "lazy={lazy}, key={key:?}: unrelated frames preserve the menu; changed targets close it");
                }
            }
        }
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
