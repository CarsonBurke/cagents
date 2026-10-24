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
        // The hover layer stays reachable while it is hidden, as it is for
        // keyboard focus.
        let hover = cosmic::iced::widget::hover(button("Edit"), button("Remove"));
        assert_named(
            &accessible_buttons(hover, max),
            &["Edit", "Remove"],
            "a hover",
        );
    }

    #[test]
    fn hidden_hover_buttons_accept_only_their_targeted_accessibility_click() {
        use cosmic::iced::Event;
        use cosmic::iced::advanced::{clipboard, mouse};
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use iced_accessibility::{
            A11yId,
            accesskit::{Action, ActionRequest, Role, TreeId},
        };

        #[derive(Clone, Debug, PartialEq)]
        enum Message {
            Base,
            Top,
        }

        let element = cosmic::iced::widget::hover(
            crate::ui_controls::button::secondary("Base").on_press(Message::Base),
            crate::ui_controls::button::secondary("Top").on_press(Message::Top),
        );
        let mut renderer = super::headless_renderer();
        let mut ui = UserInterface::build(
            element,
            Size::new(300.0, 200.0),
            Cache::new(),
            &mut renderer,
        );
        // No mouse, focus operation, or redraw precedes either Click. A fresh
        // Hover therefore keeps its top hidden throughout this sequence.
        let accessible = ui.a11y_nodes(mouse::Cursor::Unavailable);
        let target = |label: &str| {
            let nodes = accessible
                .root()
                .iter()
                .chain(accessible.children())
                .filter(|node| {
                    node.node().role() == Role::Button && node.node().label() == Some(label)
                })
                .collect::<Vec<_>>();
            assert_eq!(nodes.len(), 1, "one advertised {label} button");
            assert!(nodes[0].node().supports_action(Action::Click));
            let A11yId::Widget(id) = nodes[0].id() else {
                panic!("button must expose a widget ID");
            };
            id.clone()
        };
        let top = target("Top");
        let base = target("Base");
        assert_ne!(top, base);

        for (id, expected) in [(top, Message::Top), (base, Message::Base)] {
            let mut messages = Vec::new();
            let _ = ui.update(
                &[Event::A11y(
                    id.clone(),
                    ActionRequest {
                        action: Action::Click,
                        target_tree: TreeId::ROOT,
                        target_node: A11yId::from(id).into(),
                        data: None,
                    },
                )],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(messages, vec![expected]);
        }
    }

    #[test]
    fn wrapped_inputs_retain_identity_and_activate_at_accessible_bounds_after_nested_scroll() {
        use cosmic::iced::advanced::{clipboard, mouse, widget::Operation};
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Event, Point, Rectangle, Vector, keyboard};
        use iced_accessibility::{A11yId, accesskit::Role};

        const NAMES: [&str; 6] = ["First", "Second", "Third", "Fourth", "Fifth", "Sixth"];
        type Message = (usize, String);
        type Ui<'a> = UserInterface<'a, Message, cosmic::Theme, cosmic::Renderer>;
        fn view(inner: &widget::Id, outer: &widget::Id) -> Element<'static, Message> {
            let fields =
                widget::row::with_children(NAMES.into_iter().enumerate().map(|(index, name)| {
                    crate::ui_controls::field::text(name, "")
                        .width(160)
                        .on_input(move |value| (index, value))
                        .into()
                }))
                .spacing(8)
                .wrap();
            widget::scrollable(widget::column::with_children(vec![
                cosmic::iced::widget::Space::new().height(32).into(),
                widget::scrollable(fields)
                    .id(inner.clone())
                    .height(90)
                    .into(),
                cosmic::iced::widget::Space::new().height(200).into(),
            ]))
            .id(outer.clone())
            .height(140)
            .into()
        }
        fn fields(ui: &Ui<'_>) -> Vec<(String, A11yId, Rect)> {
            let tree = ui.a11y_nodes(mouse::Cursor::Unavailable);
            tree.root()
                .iter()
                .chain(tree.children())
                .filter(|node| node.node().role() == Role::TextInput)
                .map(|node| {
                    (
                        node.node().label().unwrap().to_owned(),
                        node.id().clone(),
                        node.node().bounds().unwrap(),
                    )
                })
                .collect()
        }
        #[derive(Default)]
        struct Scrolls(Vec<(widget::Id, Rectangle, Rectangle, Vector)>);
        impl Operation for Scrolls {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: Rectangle,
                content: Rectangle,
                translation: Vector,
                _: &mut dyn cosmic::iced::advanced::widget::operation::Scrollable,
            ) {
                self.0
                    .push((id.unwrap().clone(), bounds, content, translation));
            }
        }
        let inner = widget::Id::unique();
        let outer = widget::Id::unique();
        let mut renderer = super::headless_renderer();
        let mut ui = UserInterface::build(
            view(&inner, &outer),
            Size::new(420.0, 140.0),
            Cache::new(),
            &mut renderer,
        );
        ui.draw(
            &mut renderer,
            &cosmic::theme::active(),
            &cosmic::iced::advanced::renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        let wide = fields(&ui);
        assert_eq!(wide.len(), NAMES.len());
        assert_eq!(wide[0].2.y0, wide[1].2.y0, "wide fields share a row");
        let cache = ui.into_cache();
        ui = UserInterface::build(
            view(&inner, &outer),
            Size::new(220.0, 140.0),
            cache,
            &mut renderer,
        );
        ui.draw(
            &mut renderer,
            &cosmic::theme::active(),
            &cosmic::iced::advanced::renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        let narrow = fields(&ui);
        assert_eq!(narrow.len(), NAMES.len());
        for (before, after) in wide.iter().zip(&narrow) {
            assert_eq!((&before.0, &before.1), (&after.0, &after.1));
        }
        assert!(narrow[1].2.y0 > narrow[0].2.y0, "narrow fields reflow");
        let mut messages = Vec::new();
        for (point, distance) in [
            (Point::new(80.0, 70.0), 80.0),
            (Point::new(15.0, 4.0), 24.0),
        ] {
            let _ = ui.update(
                &[
                    Event::Mouse(mouse::Event::CursorMoved { position: point }),
                    Event::Mouse(mouse::Event::WheelScrolled {
                        delta: mouse::ScrollDelta::Pixels {
                            x: 0.0,
                            y: -distance,
                        },
                    }),
                ],
                mouse::Cursor::Available(point),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
        }
        assert!(messages.is_empty(), "scroll emitted {messages:?}");
        let mut scrolls = Scrolls::default();
        ui.operate(&renderer, &mut scrolls);
        assert_eq!(scrolls.0.len(), 2);
        for id in [&inner, &outer] {
            let (_, viewport, content, offset) = scrolls.0.iter().find(|row| &row.0 == id).unwrap();
            assert!(content.height > viewport.height && offset.y > 0.0);
        }
        ui.draw(
            &mut renderer,
            &cosmic::theme::active(),
            &cosmic::iced::advanced::renderer::Style::default(),
            mouse::Cursor::Unavailable,
        );
        let moved = fields(&ui);
        for (before, after) in narrow.iter().zip(&moved) {
            assert_eq!((&before.0, &before.1), (&after.0, &after.1));
            assert!(after.2.y0 < before.2.y0);
        }
        let target = &moved[3];
        assert_eq!(target.0, "Fourth");
        let bounds = Rectangle {
            x: target.2.x0 as f32,
            y: target.2.y0 as f32,
            width: (target.2.x1 - target.2.x0) as f32,
            height: (target.2.y1 - target.2.y0) as f32,
        };
        assert!(bounds.y >= 0.0 && bounds.y + bounds.height <= 140.0);
        // AX geometry is checked by real hit testing, not by reproducing the
        // forwarding offset calculation: this must focus Fourth, not a neighbor.
        let point = bounds.center();
        let _ = ui.update(
            &[
                Event::Mouse(mouse::Event::CursorMoved { position: point }),
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character("z".into()),
                    modified_key: keyboard::Key::Character("z".into()),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyZ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: Some("z".into()),
                    repeat: false,
                }),
            ],
            mouse::Cursor::Available(point),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(messages, vec![(3, "z".into())]);
    }

    #[test]
    fn tables_expose_their_cells_in_row_major_order() {
        use cosmic::iced::widget::table;
        let cells = [["Edit", "Pause"], ["Run now", "Remove"]];
        let table = table::table(
            [("Name", 0), ("State", 1)].map(|(header, column)| {
                table::column(button(header), move |row: usize| button(cells[row][column]))
            }),
            0..cells.len(),
        );
        assert_named(
            &accessible_buttons(table.into(), Size::new(600.0, 600.0)),
            &["Name", "State", "Edit", "Pause", "Run now", "Remove"],
            "a table",
        );
    }
}
