//! Transparent native menu surface with a faithful widget-state tree.
//!
//! In pinned libcosmic caec74c, `ContextMenu::children` includes an unused
//! legacy menu subtree, but `ContextMenu::diff` retains only the content child.
//! Menus actually reconcile their items in `LocalState.menu_bar_state.inner`.
//! Declare the real stateless Space child here from the first frame, and leave
//! event handling, menu reconciliation and overlays to the native widget.

use super::*;

pub(super) fn surface<'a, Message: Clone + 'static>(
    menu: Vec<widget::menu::Tree<Message>>,
) -> Element<'a, Message> {
    Element::new(MenuSurface(widget::context_menu(
        cosmic::iced::widget::Space::new()
            .width(Length::Fill)
            .height(Length::Fill),
        Some(menu),
    )))
}

struct MenuSurface<'a, Message>(widget::ContextMenu<'a, Message>);

impl<Message: Clone + 'static> Widget<Message, Theme, Renderer> for MenuSurface<'_, Message> {
    fn tag(&self) -> tree::Tag {
        self.0.tag()
    }

    fn state(&self) -> tree::State {
        self.0.state()
    }

    fn children(&self) -> Vec<Tree> {
        // The only content is Space, whose native tree is stateless and empty.
        vec![Tree::empty()]
    }

    fn diff(&mut self, tree: &mut Tree) {
        self.0.diff(tree);
    }

    fn size(&self) -> Size<Length> {
        self.0.size()
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.0.layout(tree, renderer, limits)
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
            self.0.diff(tree);
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
        self.0.update(
            tree,
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
        self.0
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
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
        self.0
            .overlay(tree, layout, renderer, viewport, Vector::ZERO)
    }
}
