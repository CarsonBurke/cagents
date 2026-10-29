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
    let width = menu
        .iter()
        .map(widget::menu::Tree::preferred_width)
        .max()
        .unwrap_or(240);
    Element::new(MenuSurface {
        native: widget::context_menu(
            cosmic::iced::widget::Space::new()
                .width(Length::Fill)
                .height(Length::Fill),
            Some(menu),
        ),
        items,
        width,
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
        width: 240,
        builder: Some(Box::new(builder)),
        footprint: layout::Node::new(Size::ZERO),
    })
}

/// The shape used when the native widget last measured this flat menu.
///
/// Native `MenuState` stores positions and sizes for the open list. A changed
/// item count or preferred label width invalidates that retained geometry.
/// Keep the click in window space as well, to size the popup's anchor.
struct MenuItems {
    count: usize,
    width: u16,
    anchor: Point,
    open: bool,
}

struct MenuSurface<'a, Message> {
    native: widget::ContextMenu<'a, Message>,
    items: usize,
    width: u16,
    builder: Option<Box<dyn Fn() -> Vec<widget::menu::Tree<Message>> + 'a>>,
    footprint: layout::Node,
}

impl<Message: Clone + 'static> MenuSurface<'_, Message> {
    fn build_menu(&mut self) {
        if let Some(builder) = &self.builder {
            let menu = builder();
            self.items = menu.len();
            self.width = menu
                .iter()
                .map(widget::menu::Tree::preferred_width)
                .max()
                .unwrap_or(240);
            self.native = widget::context_menu(
                cosmic::iced::widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill),
                Some(menu),
            );
        }
    }
    /// Whether the native menu is open, from the native state itself.
    ///
    /// `LocalState` is private to libcosmic, but its `overlay` returns a menu
    /// exactly when that state is open, and building one has no side effects.
    fn menu_open(&mut self, tree: &mut Tree, renderer: &Renderer, viewport: &Rectangle) -> bool {
        if self.builder.is_some() && self.items == 0 {
            return false;
        }
        let footprint = layout::Node::new(Size::ZERO);
        self.native
            .overlay(
                &mut tree.children[0],
                Layout::new(&footprint),
                renderer,
                viewport,
                Vector::ZERO,
            )
            .is_some()
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
            width: self.width,
            anchor: Point::ORIGIN,
            open: false,
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
        let remeasured = measured.count != self.items || measured.width != self.width;
        measured.count = self.items;
        measured.width = self.width;
        if remeasured || tree.children.is_empty() {
            measured.open = false;
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
        // Native MenuBounds remembers its opening window. After a resize its
        // cached position can be outside the new window even though this
        // frame's viewport is smaller. Dismiss that geometry; a fresh opening
        // measures and clamps against the new window instead of moving an
        // already open action under the pointer.
        if matches!(event, Event::Window(window::Event::Resized(_)))
            && tree.state.downcast_ref::<MenuItems>().open
        {
            tree.state.downcast_mut::<MenuItems>().open = false;
            tree.children = vec![self.native_tree()];
            shell.invalidate_layout();
            shell.request_redraw();
        }
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
                tree.state.downcast_mut::<MenuItems>().width = self.width;
                tree.children = vec![self.native_tree()];
            }
            self.native.diff(&mut tree.children[0]);
        }
        // The native widget closes an open menu on Escape but leaves the key
        // uncaptured, so the dialog or pane beneath would also take it as its
        // own Escape. The menu is what Escape dismisses; claim it for the menu.
        //
        // Ask the native widget whether its menu is open *now*, rather than
        // remembering what the last `overlay` call saw. A menu also closes
        // without any later `overlay` call, for instance when its item opens
        // a modal dialog, which then hides every other overlay; a remembered
        // flag would stay set and swallow the dialog's first Escape.
        let dismissing = matches!(
            event,
            Event::Keyboard(cosmic::iced::keyboard::Event::KeyPressed {
                key: cosmic::iced::keyboard::Key::Named(cosmic::iced::keyboard::key::Named::Escape),
                ..
            })
        ) && self.menu_open(tree, renderer, viewport);
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
            tree.state.downcast_mut::<MenuItems>().open = true;
            shell.invalidate_layout();
            shell.request_redraw();
        }
        if dismissing {
            shell.capture_event();
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
        // Supply the preferred popup footprint instead. Its capped menu width
        // lets overlapping placement align either the left or right edges.
        // Shortening this footprint near the right edge makes both choices
        // keep the popup inside the window, including clicks near its center
        // where neither side of a point anchor has enough room.
        let anchor = tree.state.downcast_ref::<MenuItems>().anchor;
        let menu_width = f32::from(self.width).min(viewport.width);
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
        if native.is_none() {
            let measured = tree.state.downcast_mut::<MenuItems>();
            measured.open = false;
            if self.builder.is_some() {
                measured.count = 0;
                self.items = 0;
            }
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

    #[derive(Clone, Copy, PartialEq, Eq)]
    struct RowAction(usize);
    impl widget::menu::Action for RowAction {
        type Message = usize;
        fn message(&self) -> usize {
            self.0
        }
    }

    #[test]
    fn keyboard_menu_selects_enabled_actions_without_pointer_motion() {
        use cosmic::iced::keyboard::{self, key::Named};
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let window = Size::new(900.0, 300.0);
        let viewport = Rectangle::with_size(window);
        let anchor = mouse::Cursor::Available(Point::new(850.0, 250.0));
        let ordinary = || {
            let mut rows = widget::menu::items(
                &std::collections::HashMap::new(),
                vec![
                    widget::menu::Item::ButtonDisabled("Disabled".to_owned(), None, RowAction(90)),
                    widget::menu::Item::Divider,
                    widget::menu::Item::Folder("Empty".to_owned(), Vec::new()),
                    widget::menu::Item::Button("First".to_owned(), None, RowAction(1)),
                    widget::menu::Item::ButtonDisabled(
                        "Disabled again".to_owned(),
                        None,
                        RowAction(91),
                    ),
                    widget::menu::Item::Button("Second".to_owned(), None, RowAction(2)),
                ],
            );
            rows.insert(
                0,
                widget::menu::Tree::from(Element::from(widget::text("Explanation"))),
            );
            rows
        };
        let submenu = widget::menu::items(
            &std::collections::HashMap::new(),
            vec![widget::menu::Item::Folder(
                "Folder".to_owned(),
                vec![
                    widget::menu::Item::ButtonDisabled("Disabled".to_owned(), None, RowAction(90)),
                    widget::menu::Item::Divider,
                    widget::menu::Item::Button("Child first".to_owned(), None, RowAction(3)),
                    widget::menu::Item::Button("Child second".to_owned(), None, RowAction(4)),
                ],
            )],
        );
        let tall = widget::menu::items(
            &std::collections::HashMap::new(),
            (0..40)
                .map(|i| widget::menu::Item::Button(format!("Action {i}"), None, RowAction(i)))
                .collect(),
        );
        let checkbox = widget::menu::items(
            &std::collections::HashMap::new(),
            vec![widget::menu::Item::CheckBox(
                "Check".to_owned(),
                None,
                false,
                RowAction(5),
            )],
        );
        let inert = widget::menu::items(
            &std::collections::HashMap::new(),
            vec![
                widget::menu::Item::ButtonDisabled("Disabled".to_owned(), None, RowAction(90)),
                widget::menu::Item::Divider,
                widget::menu::Item::Folder("Empty".to_owned(), Vec::new()),
            ],
        );
        let named = |keys: Vec<Named>| {
            keys.into_iter()
                .map(keyboard::Key::Named)
                .collect::<Vec<keyboard::Key>>()
        };
        for (rows, keys, expected, visible) in [
            (
                ordinary(),
                named(vec![Named::ArrowDown, Named::Enter]),
                vec![1],
                Some("First"),
            ),
            (
                ordinary(),
                named(vec![Named::ArrowDown, Named::ArrowDown, Named::Enter]),
                vec![2],
                Some("Second"),
            ),
            (
                ordinary(),
                named(vec![Named::ArrowUp, Named::Enter]),
                vec![2],
                Some("Second"),
            ),
            (
                checkbox.clone(),
                vec![
                    keyboard::Key::Named(Named::ArrowDown),
                    keyboard::Key::Character(" ".into()),
                ],
                vec![5],
                Some("Check"),
            ),
            (
                checkbox,
                named(vec![Named::ArrowDown, Named::Enter]),
                vec![5],
                Some("Check"),
            ),
            (
                submenu,
                named(vec![
                    Named::ArrowDown,
                    Named::ArrowRight,
                    Named::ArrowDown,
                    Named::ArrowLeft,
                    Named::ArrowRight,
                    Named::Enter,
                ]),
                vec![3],
                Some("Child first"),
            ),
            (
                tall,
                named(vec![Named::End, Named::Enter]),
                vec![39],
                Some("Action 39"),
            ),
            (
                ordinary(),
                named(vec![Named::End, Named::ArrowDown, Named::Enter]),
                vec![1],
                Some("First"),
            ),
            (
                ordinary(),
                named(vec![Named::End, Named::Home, Named::Enter]),
                vec![1],
                Some("First"),
            ),
            (
                inert,
                named(vec![Named::ArrowDown, Named::ArrowUp, Named::Enter]),
                Vec::new(),
                None,
            ),
        ] {
            let mut element = surface(rows);
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(window, window),
            );
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
                Layout::new(&node),
                anchor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            let mut popup = element
                .as_widget_mut()
                .overlay(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &viewport,
                    Vector::ZERO,
                )
                .expect("right click opens the real menu");
            let mut measured = popup.as_overlay_mut().layout(&renderer, window);
            popup.as_overlay_mut().update(
                &Event::Window(window::Event::RedrawRequested(Instant::now())),
                Layout::new(&measured),
                anchor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
            );
            measured = popup.as_overlay_mut().layout(&renderer, window);
            for key in keys {
                if let Some(visible) = visible.filter(|_| {
                    matches!(key, keyboard::Key::Named(Named::Enter))
                        || matches!(&key, keyboard::Key::Character(text) if text.as_str() == " ")
                }) {
                    #[derive(Default)]
                    struct Texts(Vec<String>);
                    impl Operation for Texts {
                        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                            visit(self);
                        }
                        fn text(&mut self, _: Option<&Id>, _: Rectangle, value: &str) {
                            self.0.push(value.to_owned());
                        }
                    }
                    let mut texts = Texts::default();
                    popup
                        .as_overlay_mut()
                        .operate(Layout::new(&measured), &renderer, &mut texts);
                    assert!(
                        texts.0.iter().any(|text| text == visible),
                        "selected action must be in the visible slice: {:?}",
                        texts.0
                    );
                }
                if matches!(&key, keyboard::Key::Character(text) if text.as_str() == " ") {
                    // A release while the popup is open belongs to it too;
                    // this must not activate or reach the underlying pane.
                    let release = Event::Keyboard(keyboard::Event::KeyReleased {
                        key: key.clone(),
                        modified_key: key.clone(),
                        physical_key: keyboard::key::Physical::Unidentified(
                            keyboard::key::NativeCode::Unidentified,
                        ),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::empty(),
                    });
                    let mut shell = Shell::new(&mut messages);
                    popup.as_overlay_mut().update(
                        &release,
                        Layout::new(&measured),
                        mouse::Cursor::Unavailable,
                        &renderer,
                        &mut clipboard::Null,
                        &mut shell,
                    );
                    assert!(
                        shell.is_event_captured(),
                        "native Space release stays in the menu"
                    );
                    assert!(
                        messages.is_empty(),
                        "release never activates the selected action"
                    );
                }
                let event = Event::Keyboard(keyboard::Event::KeyPressed {
                    key: key.clone(),
                    modified_key: key,
                    physical_key: keyboard::key::Physical::Unidentified(
                        keyboard::key::NativeCode::Unidentified,
                    ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                });
                let mut shell = Shell::new(&mut messages);
                popup.as_overlay_mut().update(
                    &event,
                    Layout::new(&measured),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut shell,
                );
                assert!(
                    shell.is_event_captured(),
                    "menu navigation must not reach the workspace"
                );
                measured = popup.as_overlay_mut().layout(&renderer, window);
            }
            drop(popup);
            assert_eq!(messages, expected);
            // An inert menu cannot activate anything, but Escape still closes
            // it through the unchanged native ContextMenu event path.
            if expected.is_empty() {
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Keyboard(keyboard::Event::KeyPressed {
                        key: keyboard::Key::Named(Named::Escape),
                        modified_key: keyboard::Key::Named(Named::Escape),
                        physical_key: keyboard::key::Physical::Unidentified(
                            keyboard::key::NativeCode::Unidentified,
                        ),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::empty(),
                        text: None,
                        repeat: false,
                    }),
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
                assert!(messages.is_empty());
            }
            assert!(
                element
                    .as_widget_mut()
                    .overlay(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &viewport,
                        Vector::ZERO,
                    )
                    .is_none(),
                "keyboard activation closes the menu"
            );
        }
    }

    #[test]
    fn generated_menu_actions_retain_their_captured_local_message_for_keyboard() {
        use cosmic::iced::keyboard::{self, key::Named};
        use std::rc::Rc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // Native popup builders require these auto traits even when the
        // generated widget/message belongs to its original UI thread.
        fn popup_compatible<T: Send + Sync>() {}
        popup_compatible::<widget::menu::Tree<Rc<usize>>>();

        static CALLS: AtomicUsize = AtomicUsize::new(0);
        #[derive(Clone, Copy, PartialEq, Eq)]
        struct CountedAction;
        impl widget::menu::Action for CountedAction {
            type Message = Rc<usize>;

            fn message(&self) -> Self::Message {
                Rc::new(CALLS.fetch_add(1, Ordering::Relaxed))
            }
        }

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let window = Size::new(900.0, 300.0);
        let viewport = Rectangle::with_size(window);
        let anchor = mouse::Cursor::Available(Point::new(850.0, 250.0));
        for checkbox in [false, true] {
            for activation in [
                keyboard::Key::Named(Named::Enter),
                keyboard::Key::Character(" ".into()),
            ] {
                let captured = CALLS.load(Ordering::Relaxed);
                let item = if checkbox {
                    widget::menu::Item::CheckBox("Local action", None, false, CountedAction)
                } else {
                    widget::menu::Item::Button("Local action", None, CountedAction)
                };
                let mut element = surface(widget::menu::items(
                    &std::collections::HashMap::new(),
                    vec![item],
                ));
                assert_eq!(CALLS.load(Ordering::Relaxed), captured + 1);
                let widget = element.as_widget_mut();
                let mut tree = Tree::new(&*widget);
                widget.diff(&mut tree);
                let row = widget.layout(&mut tree, &renderer, &layout::Limits::new(window, window));
                let mut messages = Vec::new();
                widget.update(
                    &mut tree,
                    &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
                    Layout::new(&row),
                    anchor,
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
                    .expect("opened local-message menu");
                let mut node = popup.as_overlay_mut().layout(&renderer, window);
                popup.as_overlay_mut().update(
                    &Event::Window(window::Event::RedrawRequested(Instant::now())),
                    Layout::new(&node),
                    anchor,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                );
                node = popup.as_overlay_mut().layout(&renderer, window);
                for key in [keyboard::Key::Named(Named::ArrowDown), activation] {
                    let event = Event::Keyboard(keyboard::Event::KeyPressed {
                        key: key.clone(),
                        modified_key: key,
                        physical_key: keyboard::key::Physical::Unidentified(
                            keyboard::key::NativeCode::Unidentified,
                        ),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::empty(),
                        text: None,
                        repeat: false,
                    });
                    let mut shell = Shell::new(&mut messages);
                    popup.as_overlay_mut().update(
                        &event,
                        Layout::new(&node),
                        mouse::Cursor::Unavailable,
                        &renderer,
                        &mut clipboard::Null,
                        &mut shell,
                    );
                    assert!(shell.is_event_captured());
                    node = popup.as_overlay_mut().layout(&renderer, window);
                }
                assert_eq!(
                    messages.iter().map(|message| **message).collect::<Vec<_>>(),
                    [captured],
                    "keyboard activation must publish the originally generated message"
                );
                assert_eq!(
                    CALLS.load(Ordering::Relaxed),
                    captured + 1,
                    "keyboard activation must not regenerate the menu action"
                );
            }
        }
    }

    #[test]
    fn resized_window_dismisses_old_geometry_and_reopens_a_reachable_action() {
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let mut element = surface(widget::menu::items(
            &std::collections::HashMap::new(),
            vec![widget::menu::Item::Button(
                "New worktree from this branch…".to_owned(),
                None,
                RowAction(7),
            )],
        ));
        let mut tree = Tree::new(&element);
        let mut messages = Vec::new();
        let old_size = Size::new(1200.0, 900.0);
        let new_size = Size::new(800.0, 650.0);
        // Keep the actual widget/Tree through native resize and reopening.
        for (index, size) in [old_size, new_size].into_iter().enumerate() {
            let viewport = Rectangle::with_size(size);
            let row = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(size, size),
            );
            let anchor =
                mouse::Cursor::Available(Point::new(size.width - 20.0, size.height - 20.0));
            if index == 1 {
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Window(window::Event::Resized(size)),
                    Layout::new(&row),
                    anchor,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
                assert!(
                    element
                        .as_widget_mut()
                        .overlay(
                            &mut tree,
                            Layout::new(&row),
                            &renderer,
                            &viewport,
                            Vector::ZERO,
                        )
                        .is_none(),
                    "an old right-edge popup cannot keep geometry outside the resized window",
                );
                assert!(
                    messages.is_empty(),
                    "resize must not activate an old action"
                );
            }
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
                Layout::new(&row),
                anchor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            let mut popup = element
                .as_widget_mut()
                .overlay(
                    &mut tree,
                    Layout::new(&row),
                    &renderer,
                    &viewport,
                    Vector::ZERO,
                )
                .expect("native menu can open after resize");
            let first = popup.as_overlay_mut().layout(&renderer, size);
            popup.as_overlay_mut().update(
                &Event::Window(window::Event::RedrawRequested(Instant::now())),
                Layout::new(&first),
                anchor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
            );
            let measured = popup.as_overlay_mut().layout(&renderer, size);
            let bounds = measured.children().first().expect("visible popup").bounds();
            assert!(bounds.is_within(&viewport));
            if index == 1 {
                for event in [
                    mouse::Event::ButtonPressed(mouse::Button::Left),
                    mouse::Event::ButtonReleased(mouse::Button::Left),
                ] {
                    popup.as_overlay_mut().update(
                        &Event::Mouse(event),
                        Layout::new(&measured),
                        mouse::Cursor::Available(bounds.center()),
                        &renderer,
                        &mut clipboard::Null,
                        &mut Shell::new(&mut messages),
                    );
                }
            }
        }
        assert_eq!(messages, [7], "the new visible action remains clickable");
    }

    #[test]
    fn tall_menu_scroll_reaches_its_last_action_at_the_bottom_edge() {
        use cosmic::iced::keyboard::{self, key::Named};

        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let window = Size::new(900.0, 900.0);
        let viewport = Rectangle::with_size(window);
        let anchor = Point::new(850.0, 850.0);
        let chords = vec!["Ctrl+Alt+F5".to_owned()];
        let bindings = menu_shortcuts([(chords.as_slice(), RowAction(39))]);
        let hint = bindings.keys().next().unwrap().to_string();
        for delta in [
            mouse::ScrollDelta::Lines { x: 0.0, y: -100.0 },
            mouse::ScrollDelta::Pixels { x: 0.0, y: -6000.0 },
        ] {
            for activation in [
                None,
                Some(keyboard::Key::Named(Named::Enter)),
                Some(keyboard::Key::Character(" ".into())),
            ] {
                let mut element = surface(widget::menu::items(
                    &bindings,
                    (0..40)
                        .map(|index| {
                            widget::menu::Item::Button(
                                if index == 0 {
                                    "New worktree from this branch…".into()
                                } else {
                                    format!("Action {index}")
                                },
                                None,
                                RowAction(index),
                            )
                        })
                        .collect(),
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
                    mouse::Cursor::Available(anchor),
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
                    .unwrap();
                let node = popup.as_overlay_mut().layout(&renderer, window);
                popup.as_overlay_mut().update(
                    &Event::Window(window::Event::RedrawRequested(Instant::now())),
                    Layout::new(&node),
                    mouse::Cursor::Available(anchor),
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                );
                let node = popup.as_overlay_mut().layout(&renderer, window);
                let menu = node.children().first().unwrap().bounds();
                assert!(menu.x >= 0.0 && menu.x + menu.width <= window.width);
                assert!(
                    menu.width > 240.0,
                    "long labels grow the shared menu beyond its old fixed width"
                );
                let pointer =
                    mouse::Cursor::Available(Point::new(menu.x + 30.0, window.height - 18.0));
                // No pointer motion is required before the first wheel event.
                popup.as_overlay_mut().update(
                    &Event::Mouse(mouse::Event::WheelScrolled { delta }),
                    Layout::new(&node),
                    pointer,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                );
                let node = popup.as_overlay_mut().layout(&renderer, window);
                #[derive(Default)]
                struct Texts(Vec<String>);
                impl Operation for Texts {
                    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                        visit(self);
                    }
                    fn text(&mut self, _: Option<&Id>, _: Rectangle, text: &str) {
                        self.0.push(text.to_owned());
                    }
                }
                let mut texts = Texts::default();
                popup
                    .as_overlay_mut()
                    .operate(Layout::new(&node), &renderer, &mut texts);
                assert!(
                    texts.0.iter().any(|text| text == &hint),
                    "the rendered trailing action must expose its configured chord: {:?}",
                    texts.0
                );
                assert!(texts.0.iter().any(|text| text == "Action 39"));
                assert!(
                    !texts
                        .0
                        .iter()
                        .any(|text| text == "New worktree from this branch…"),
                    "operations must follow the visible scroll slice, not the hidden first row"
                );
                assert!(messages.is_empty(), "scrolling must not activate a row");
                if let Some(key) = activation {
                    // No motion or click may repair the pre-scroll pointer selection
                    // before Enter or the Linux Character-space event activates it.
                    let event = Event::Keyboard(keyboard::Event::KeyPressed {
                        key: key.clone(),
                        modified_key: key.clone(),
                        physical_key: keyboard::key::Physical::Unidentified(
                            keyboard::key::NativeCode::Unidentified,
                        ),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::empty(),
                        text: None,
                        repeat: false,
                    });
                    let mut shell = Shell::new(&mut messages);
                    popup.as_overlay_mut().update(
                        &event,
                        Layout::new(&node),
                        pointer,
                        &renderer,
                        &mut clipboard::Null,
                        &mut shell,
                    );
                    assert!(shell.is_event_captured());
                    assert_eq!(
                        messages,
                        [39],
                        "{key:?} after {delta:?} must activate the row now beneath the stationary pointer"
                    );
                } else {
                    for event in [
                        mouse::Event::ButtonPressed(mouse::Button::Left),
                        mouse::Event::ButtonReleased(mouse::Button::Left),
                    ] {
                        popup.as_overlay_mut().update(
                            &Event::Mouse(event),
                            Layout::new(&node),
                            pointer,
                            &renderer,
                            &mut clipboard::Null,
                            &mut Shell::new(&mut messages),
                        );
                    }
                    assert_eq!(
                        messages,
                        [39],
                        "the destructive trailing item must remain reachable"
                    );
                }
            }
        }
    }

    #[test]
    fn touch_tap_uses_its_own_position_without_prior_menu_motion() {
        use cosmic::iced::{
            keyboard::{self, key::Named},
            touch,
        };

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let window = Size::new(400.0, 300.0);
        let viewport = Rectangle::with_size(window);
        let anchor = Point::new(390.0, 250.0);
        for keyboard_first in [false, true] {
            for stale_mouse in [false, true] {
                for release_outside in [false, true] {
                    let mut element = surface(widget::menu::items(
                        &std::collections::HashMap::new(),
                        (0..3)
                            .map(|index| {
                                widget::menu::Item::Button(
                                    format!("Action {index}"),
                                    None,
                                    RowAction(index),
                                )
                            })
                            .collect(),
                    ));
                    let widget = element.as_widget_mut();
                    let mut tree = Tree::new(&*widget);
                    widget.diff(&mut tree);
                    let row =
                        widget.layout(&mut tree, &renderer, &layout::Limits::new(window, window));
                    let mut messages = Vec::new();
                    widget.update(
                        &mut tree,
                        &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
                        Layout::new(&row),
                        mouse::Cursor::Available(anchor),
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
                        .expect("opened context menu");
                    let node = popup.as_overlay_mut().layout(&renderer, window);
                    popup.as_overlay_mut().update(
                        &Event::Window(window::Event::RedrawRequested(Instant::now())),
                        Layout::new(&node),
                        mouse::Cursor::Available(anchor),
                        &renderer,
                        &mut clipboard::Null,
                        &mut Shell::new(&mut messages),
                    );
                    let mut node = popup.as_overlay_mut().layout(&renderer, window);
                    if keyboard_first {
                        popup.as_overlay_mut().update(
                            &Event::Keyboard(keyboard::Event::KeyPressed {
                                key: keyboard::Key::Named(Named::ArrowDown),
                                modified_key: keyboard::Key::Named(Named::ArrowDown),
                                physical_key: keyboard::key::Physical::Unidentified(
                                    keyboard::key::NativeCode::Unidentified,
                                ),
                                location: keyboard::Location::Standard,
                                modifiers: keyboard::Modifiers::empty(),
                                text: None,
                                repeat: false,
                            }),
                            Layout::new(&node),
                            mouse::Cursor::Unavailable,
                            &renderer,
                            &mut clipboard::Null,
                            &mut Shell::new(&mut messages),
                        );
                        node = popup.as_overlay_mut().layout(&renderer, window);
                    }
                    let menu = Layout::new(&node).children().next().expect("measured menu");
                    let target = menu
                        .children()
                        .nth(2)
                        .expect("third action")
                        .bounds()
                        .center();
                    let mouse = if stale_mouse {
                        mouse::Cursor::Available(menu.children().next().unwrap().bounds().center())
                    } else {
                        mouse::Cursor::Unavailable
                    };
                    let release = if release_outside {
                        Point::new(1.0, 1.0)
                    } else {
                        target
                    };
                    // No FingerMoved or mouse motion precedes this tap. The
                    // keyboard selection and mouse position may name another row.
                    for event in [
                        touch::Event::FingerPressed {
                            id: touch::Finger(7),
                            position: target,
                        },
                        touch::Event::FingerLifted {
                            id: touch::Finger(7),
                            position: release,
                        },
                    ] {
                        popup.as_overlay_mut().update(
                            &Event::Touch(event),
                            Layout::new(&node),
                            mouse,
                            &renderer,
                            &mut clipboard::Null,
                            &mut Shell::new(&mut messages),
                        );
                    }
                    drop(popup);
                    assert_eq!(
                        messages,
                        if release_outside { vec![] } else { vec![2] },
                        "touch must dispatch only its own released row; keyboard={keyboard_first}, stale_mouse={stale_mouse}, outside={release_outside}"
                    );
                    assert_eq!(
                        widget
                            .overlay(
                                &mut tree,
                                Layout::new(&row),
                                &renderer,
                                &viewport,
                                Vector::ZERO,
                            )
                            .is_none(),
                        !release_outside,
                        "activation dismisses; a release dragged outside keeps the menu"
                    );
                }
            }
        }
    }

    #[test]
    fn escape_that_dismisses_the_menu_is_not_left_for_the_surface_beneath() {
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
        let window = Size::new(400.0, 400.0);
        let viewport = Rectangle::new(Point::ORIGIN, window);
        let mut element: Element<'_, ()> = surface(widget::menu::items(
            &std::collections::HashMap::new(),
            vec![widget::menu::Item::Button("Action", None, Action)],
        ));
        let widget = element.as_widget_mut();
        let mut tree = Tree::new(&*widget);
        widget.diff(&mut tree);
        let row = widget.layout(&mut tree, &renderer, &layout::Limits::new(window, window));
        let cursor = mouse::Cursor::Available(Point::new(40.0, 40.0));
        let escape = Event::Keyboard(cosmic::iced::keyboard::Event::KeyPressed {
            key: cosmic::iced::keyboard::Key::Named(cosmic::iced::keyboard::key::Named::Escape),
            modified_key: cosmic::iced::keyboard::Key::Named(
                cosmic::iced::keyboard::key::Named::Escape,
            ),
            physical_key: cosmic::iced::keyboard::key::Physical::Code(
                cosmic::iced::keyboard::key::Code::Escape,
            ),
            location: cosmic::iced::keyboard::Location::Standard,
            modifiers: cosmic::iced::keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        });
        let send =
            |widget: &mut dyn Widget<(), Theme, Renderer>, tree: &mut Tree, event: &Event| {
                let mut messages = Vec::new();
                let mut shell = Shell::new(&mut messages);
                widget.update(
                    tree,
                    event,
                    Layout::new(&row),
                    cursor,
                    &renderer,
                    &mut clipboard::Null,
                    &mut shell,
                    &viewport,
                );
                shell.is_event_captured()
            };
        // A closed menu leaves Escape to whatever owns it.
        assert!(!send(widget, &mut tree, &escape));
        send(
            widget,
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
        );
        assert!(
            widget
                .overlay(
                    &mut tree,
                    Layout::new(&row),
                    &renderer,
                    &viewport,
                    Vector::ZERO
                )
                .is_some(),
            "opened menu"
        );
        assert!(send(widget, &mut tree, &escape), "Escape closed the menu");
        assert!(
            widget
                .overlay(
                    &mut tree,
                    Layout::new(&row),
                    &renderer,
                    &viewport,
                    Vector::ZERO
                )
                .is_none(),
            "the menu is closed"
        );
        assert!(!send(widget, &mut tree, &escape));

        // A menu that closes without another `overlay` query, as when its item
        // opens a modal dialog that hides every other overlay, leaves the
        // dialog its first Escape.
        send(
            widget,
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
        );
        assert!(
            widget
                .overlay(
                    &mut tree,
                    Layout::new(&row),
                    &renderer,
                    &viewport,
                    Vector::ZERO
                )
                .is_some(),
            "reopened menu"
        );
        send(
            widget,
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        );
        assert!(
            !send(widget, &mut tree, &escape),
            "Escape after the menu closed unseen belongs to what is beneath"
        );
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
        // Touch coordinates belong to the finger, independently of the last
        // mouse position. Native menu buttons use this cursor for hit tests.
        let cursor = match event {
            Event::Touch(
                cosmic::iced::touch::Event::FingerPressed { position, .. }
                | cosmic::iced::touch::Event::FingerMoved { position, .. }
                | cosmic::iced::touch::Event::FingerLifted { position, .. }
                | cosmic::iced::touch::Event::FingerLost { position, .. },
            ) => mouse::Cursor::Available(*position),
            _ => cursor,
        };
        let press = match event {
            Event::Mouse(
                mouse::Event::ButtonPressed(mouse::Button::Left)
                | mouse::Event::WheelScrolled { .. },
            ) => cursor.position(),
            Event::Touch(cosmic::iced::touch::Event::FingerPressed { position, .. }) => {
                Some(*position)
            }
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
