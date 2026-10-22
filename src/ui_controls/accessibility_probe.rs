//! Test helpers that check accessibility forwarding through wrappers: a child
//! with retained widget state, and a reader of the buttons a view exposes.
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

/// Lays `element` out within `max` and returns the accessible name and bounds
/// of every button it exposes. An unnamed button reports "". Buttons come in
/// `A11yTree` order: every top-level node, then every descendant. Sibling
/// buttons keep their widget order, but this is not reading order in general.
pub(crate) fn accessible_buttons<Message>(
    element: cosmic::Element<'_, Message>,
    max: Size,
) -> Vec<(String, iced_accessibility::accesskit::Rect)> {
    laid_out_buttons(element, max).1
}

fn laid_out_buttons<Message>(
    mut element: cosmic::Element<'_, Message>,
    max: Size,
) -> (
    layout::Node,
    Vec<(String, iced_accessibility::accesskit::Rect)>,
) {
    let renderer = headless_renderer();
    let mut tree = Tree::new(element.as_widget());
    let node =
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &layout::Limits::new(Size::ZERO, max));
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    let buttons = accessible
        .root()
        .iter()
        .chain(accessible.children())
        .map(|node| node.node())
        .filter(|node| node.role() == iced_accessibility::accesskit::Role::Button)
        .map(|node| {
            (
                node.label().unwrap_or_default().to_owned(),
                node.bounds().expect("buttons report bounds"),
            )
        })
        .collect();
    (node, buttons)
}

/// Upstream wrappers must forward their children's accessibility nodes; the
/// trait default reports none, which hides every control inside them.
mod tests {
    use super::{accessible_buttons, laid_out_buttons};
    use cosmic::iced::advanced::Layout;
    use cosmic::iced::{Length, Size};
    use cosmic::{Element, widget};
    use iced_accessibility::accesskit::Rect;

    const LABELS: [&str; 4] = ["Edit", "Pause", "Run now", "Remove"];

    fn button(label: &'static str) -> Element<'static, ()> {
        crate::ui_controls::button::secondary(label)
            .on_press(())
            .into()
    }

    fn buttons() -> Vec<Element<'static, ()>> {
        LABELS.into_iter().map(button).collect()
    }

    fn assert_named(found: &[(String, Rect)], labels: &[&str], wrapper: &str) {
        let names = found
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, labels, "{wrapper} must expose its buttons by name");
    }

    /// Each button reports the bounds its wrapped child layout received, and
    /// the layout really wrapped: the buttons start more than one `line`.
    fn assert_wrapped(
        element: Element<'static, ()>,
        max: Size,
        line: fn(&Rect) -> f64,
        wrapper: &str,
    ) {
        let (node, found) = laid_out_buttons(element, max);
        assert_named(&found, &LABELS, wrapper);
        let laid_out = Layout::new(&node)
            .children()
            .map(|child| {
                let bounds = child.bounds();
                Rect::new(
                    f64::from(bounds.x),
                    f64::from(bounds.y),
                    f64::from(bounds.x + bounds.width),
                    f64::from(bounds.y + bounds.height),
                )
            })
            .collect::<Vec<_>>();
        let reported = found.iter().map(|(_, bounds)| *bounds).collect::<Vec<_>>();
        assert_eq!(reported, laid_out, "{wrapper} reports wrapped bounds");
        let mut lines = laid_out.iter().map(line).collect::<Vec<_>>();
        lines.dedup();
        assert!(lines.len() > 1, "{wrapper} did not wrap: {laid_out:?}");
    }

    #[test]
    fn wrapping_rows_and_columns_expose_children_at_their_wrapped_positions() {
        // No two buttons fit in 60 across or, at 28 high plus spacing, in 50
        // down, so each starts its own line whatever the font metrics.
        assert_wrapped(
            widget::row::with_children(buttons())
                .spacing(8)
                .wrap()
                .into(),
            Size::new(60.0, 600.0),
            |bounds| bounds.y0,
            "a wrapping row",
        );
        assert_wrapped(
            widget::column::with_children(buttons())
                .spacing(8)
                .wrap()
                .into(),
            Size::new(600.0, 50.0),
            |bounds| bounds.x0,
            "a wrapping column",
        );
    }

    #[test]
    fn single_child_and_keyed_wrappers_expose_their_content() {
        let max = Size::new(600.0, 600.0);
        let keyed = cosmic::iced::widget::keyed::Column::with_children(
            LABELS
                .into_iter()
                .enumerate()
                .map(|(key, label)| (key, button(label))),
        )
        .width(Length::Fill);
        assert_named(
            &accessible_buttons(keyed.into(), max),
            &LABELS,
            "a keyed column",
        );
        let lazy = cosmic::iced::widget::lazy(0_u8, |_| button("Edit"));
        assert_named(&accessible_buttons(lazy.into(), max), &["Edit"], "a lazy");
        let tooltip = widget::tooltip(
            button("Edit"),
            widget::text("Edit this automation"),
            widget::tooltip::Position::Top,
        );
        assert_named(
            &accessible_buttons(tooltip.into(), max),
            &["Edit"],
            "a tooltip",
        );
    }
}
