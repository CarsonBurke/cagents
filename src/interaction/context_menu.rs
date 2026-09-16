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
    })
}

/// How many items the menu had when the native widget last measured it.
///
/// This is the only quantity the retained menu geometry is indexed by: the
/// native widget stores one `MenuState` per open level, and its `MenuBounds`
/// holds a position and a size per item of the list that level was opened on.
/// Our menus are flat, so the top-level item count is their entire shape.
struct MenuItems(usize);

struct MenuSurface<'a, Message> {
    native: widget::ContextMenu<'a, Message>,
    items: usize,
}

impl<Message: Clone + 'static> MenuSurface<'_, Message> {
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
        tree::State::new(MenuItems(self.items))
    }

    fn children(&self) -> Vec<Tree> {
        vec![self.native_tree()]
    }

    fn diff(&mut self, tree: &mut Tree) {
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
        let remeasured = measured.0 != self.items;
        measured.0 = self.items;
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
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        _translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        // The anchor is already a window-space point, which is the space the
        // runtime drives overlays in, so the native menu needs no translation:
        // it derives one offset for its layout and the opposite one for its
        // hit tests, and the two only agree at none at all.
        self.native.overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            Vector::ZERO,
        )
    }
}
