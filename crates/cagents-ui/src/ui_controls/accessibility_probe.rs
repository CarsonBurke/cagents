//! A test child that checks wrapper forwarding against retained widget state.
use cosmic::iced::advanced::{
    Layout, Widget, layout, mouse, renderer,
    widget::{Tree, tree},
};
use cosmic::iced::{Length, Rectangle, Size};
pub(crate) struct Probe;
impl<Message> Widget<Message, cosmic::Theme, cosmic::Renderer> for Probe {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<iced_accessibility::Id>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(iced_accessibility::Id::unique())
    }
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }
    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
    }
    fn draw(
        &self,
        _tree: &Tree,
        _renderer: &mut cosmic::Renderer,
        _theme: &cosmic::Theme,
        _style: &renderer::Style,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let bounds = layout.bounds() - layout.virtual_offset();
        let mut node =
            iced_accessibility::accesskit::Node::new(iced_accessibility::accesskit::Role::Button);
        node.set_label("Retained child");
        node.set_bounds(iced_accessibility::accesskit::Rect::new(
            f64::from(bounds.x),
            f64::from(bounds.y),
            f64::from(bounds.x + bounds.width),
            f64::from(bounds.y + bounds.height),
        ));
        iced_accessibility::A11yTree::leaf(
            node,
            tree.state.downcast_ref::<iced_accessibility::Id>().clone(),
        )
    }
}

pub(crate) fn headless_renderer() -> cosmic::Renderer {
    use cosmic::iced::advanced::renderer::Headless;

    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(<cosmic::Renderer as Headless>::new(
            cosmic::iced::Font::DEFAULT,
            cosmic::iced::Pixels::from(16),
            Some("tiny-skia"),
        ))
        .expect("create a headless renderer")
}
