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
        self.0.update(
            tree, event, layout, cursor, renderer, clipboard, shell, viewport,
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
        self.0
            .draw(tree, renderer, theme, style, layout, cursor, viewport);
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.0
            .overlay(tree, layout, renderer, viewport, translation)
    }
}
