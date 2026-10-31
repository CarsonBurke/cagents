//! A leaf with observable native widget contracts for wrapper regression tests.
use cosmic::iced::advanced::clipboard::DndDestinationRectangles;
use cosmic::iced::advanced::{
    Layout, Widget, layout, overlay, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::clipboard::dnd::{DndAction, DndDestinationRectangle, Rectangle as DndRectangle};
use cosmic::iced::{Length, Rectangle, Size, Vector, mouse};
use cosmic::{Element, Renderer, Theme};
use std::{cell::RefCell, rc::Rc};

#[derive(Clone, Default)]
pub(crate) struct Visits(Rc<RefCell<Vec<(&'static str, Vector)>>>);
impl Visits {
    pub(crate) fn offsets(&self, action: &str) -> Vec<Vector> {
        self.0
            .borrow()
            .iter()
            .filter_map(|(kind, offset)| (*kind == action).then_some(*offset))
            .collect()
    }
}
struct Leaf {
    size: Size,
    destinations: Vec<(u128, Rectangle)>,
    visits: Visits,
}
pub(crate) fn leaf<Message: 'static>(
    size: Size,
    destinations: Vec<(u128, Rectangle)>,
) -> (Element<'static, Message>, Visits) {
    let visits = Visits::default();
    (
        Element::new(Leaf {
            size,
            destinations,
            visits: visits.clone(),
        }),
        visits,
    )
}
impl<Message> Widget<Message, Theme, Renderer> for Leaf {
    fn size(&self) -> Size<Length> {
        Size::new(
            Length::Fixed(self.size.width),
            Length::Fixed(self.size.height),
        )
    }
    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        _limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(self.size)
    }
    fn draw(
        &self,
        _tree: &Tree,
        _renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
    }
    fn operate(
        &mut self,
        _tree: &mut Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        _operation: &mut dyn Operation,
    ) {
        self.visits
            .0
            .borrow_mut()
            .push(("operate", layout.virtual_offset()));
    }
    fn overlay<'a>(
        &'a mut self,
        _tree: &'a mut Tree,
        layout: Layout<'a>,
        _renderer: &Renderer,
        _viewport: &Rectangle,
        _translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.visits
            .0
            .borrow_mut()
            .push(("overlay", layout.virtual_offset()));
        None
    }
    fn drag_destinations(
        &self,
        _tree: &Tree,
        layout: Layout<'_>,
        _renderer: &Renderer,
        destinations: &mut DndDestinationRectangles,
    ) {
        self.visits
            .0
            .borrow_mut()
            .push(("drag", layout.virtual_offset()));
        for (id, bounds) in &self.destinations {
            destinations.push(DndDestinationRectangle {
                id: *id,
                rectangle: DndRectangle {
                    x: f64::from(layout.bounds().x + bounds.x),
                    y: f64::from(layout.bounds().y + bounds.y),
                    width: f64::from(bounds.width),
                    height: f64::from(bounds.height),
                },
                mime_types: Vec::new(),
                actions: DndAction::Copy,
                preferred: DndAction::Copy,
            });
        }
    }
}
pub(crate) struct Traverse;
impl Operation for Traverse {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
}
pub(crate) fn renderer() -> Renderer {
    use cosmic::iced::advanced::renderer::Headless;
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("executor")
        .block_on(Renderer::new(
            cosmic::iced::Font::default(),
            cosmic::iced::Pixels(14.0),
            Some("tiny-skia"),
        ))
        .expect("headless renderer")
}

pub(crate) type Press = (&'static str, Vector, Rectangle);

pub(crate) fn native_button(label: &'static str, size: Size) -> Element<'static, Press> {
    cosmic::widget::button::custom(cosmic::widget::text(label))
        .name(label)
        .width(size.width)
        .height(size.height)
        .padding(0)
        .on_press_with_rectangle(move |offset, bounds| (label, offset, bounds))
        .into()
}

/// Inspect actual advertised controls and dispatch their native Click actions.
/// This exercises widget routing, not the compositor or accessibility bridge.
pub(crate) fn native_controls(
    element: &mut Element<'_, Press>,
    tree: &mut Tree,
    node: &layout::Node,
    offset: Vector,
    renderer: &Renderer,
) -> Vec<(String, iced_accessibility::accesskit::Rect, Vec<Press>)> {
    use iced_accessibility::accesskit::{Action, ActionRequest, NodeId, Role, TreeId};

    let layout = Layout::new(node).with_virtual_offset(offset);
    let nodes = element
        .as_widget()
        .a11y_nodes(layout, tree, mouse::Cursor::Unavailable);
    let targets: Vec<_> = nodes
        .root()
        .iter()
        .chain(nodes.children())
        .filter(|node| node.node().role() == Role::Button)
        .map(|node| {
            (
                node.node().label().unwrap().to_owned(),
                node.node().bounds().unwrap(),
                NodeId::from(node.id().clone()),
            )
        })
        .collect();
    targets
        .into_iter()
        .map(|(label, bounds, target)| {
            let mut messages = Vec::new();
            element.as_widget_mut().update(
                tree,
                &cosmic::iced::Event::A11y(
                    cosmic::widget::Id::from(target.0),
                    ActionRequest {
                        action: Action::Click,
                        target_tree: TreeId::ROOT,
                        target_node: target,
                        data: None,
                    },
                ),
                layout,
                mouse::Cursor::Unavailable,
                renderer,
                &mut cosmic::iced::advanced::clipboard::Null,
                &mut cosmic::iced::advanced::Shell::new(&mut messages),
                &node.bounds(),
            );
            (label, bounds, messages)
        })
        .collect()
}
