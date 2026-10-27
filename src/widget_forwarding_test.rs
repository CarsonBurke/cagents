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
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        _tree: &Tree,
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.visits
            .0
            .borrow_mut()
            .push(("a11y", layout.virtual_offset()));
        iced_accessibility::A11yTree::default()
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
