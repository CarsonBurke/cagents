use super::*;
use crate::workspace_widget::forwarding_test::{self, Traverse};
use cosmic::iced::advanced::widget::{Id, operation};
use cosmic::iced::advanced::{Shell, clipboard};
use cosmic::iced::{Point, widget::scrollable::AbsoluteOffset};
use std::cell::Cell;
use std::rc::Rc;

type Message = ();

fn layouts() -> usize {
    LAYOUTS.with(Cell::get)
}

fn builds() -> usize {
    BUILDS.with(Cell::get)
}

fn fill() -> Size<Length> {
    Size::new(Length::Fill, Length::Fill)
}

/// A fixed-height leaf that fills its width, built from its key.
fn bar(height: &u32) -> Element<'static, Message> {
    cosmic::iced::widget::Space::new()
        .width(Length::Fill)
        .height(Length::Fixed(*height as f32))
        .into()
}

/// A frame of one widget, as the runtime gives it: the new element is
/// reconciled with the kept tree, then laid out.
struct Frames {
    renderer: Renderer,
    tree: Option<Tree>,
}

impl Frames {
    fn new() -> Self {
        Self {
            renderer: forwarding_test::renderer(),
            tree: None,
        }
    }

    fn frame(&mut self, mut element: Element<'_, Message>, width: f32) -> layout::Node {
        let tree = self.tree.get_or_insert_with(|| Tree::new(&element));
        tree.diff(element.as_widget_mut());
        element.as_widget_mut().layout(
            tree,
            &self.renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, 400.0)),
        )
    }
}

fn keyed_bar(key: u32) -> Element<'static, Message> {
    keyed(key, Size::new(Length::Fill, Length::Fixed(key as f32)), bar)
}

#[test]
fn an_unchanged_key_is_neither_built_nor_laid_out_again() {
    let mut frames = Frames::new();
    let (built, laid) = (builds(), layouts());
    let node = frames.frame(keyed_bar(20), 300.0);
    assert_eq!((builds() - built, layouts() - laid), (1, 1));
    assert_eq!(node.size(), Size::new(300.0, 20.0));
    for _ in 0..3 {
        frames.frame(keyed_bar(20), 300.0);
    }
    assert_eq!(
        (builds() - built, layouts() - laid),
        (1, 1),
        "the same key again"
    );
    let node = frames.frame(keyed_bar(30), 300.0);
    assert_eq!(
        (builds() - built, layouts() - laid),
        (2, 2),
        "a new key rebuilds"
    );
    assert_eq!(node.size(), Size::new(300.0, 30.0));
    let node = frames.frame(keyed_bar(30), 200.0);
    assert_eq!(
        (builds() - built, layouts() - laid),
        (2, 3),
        "new room lays the kept subtree out again without building it"
    );
    assert_eq!(node.size(), Size::new(200.0, 30.0));
}

#[test]
fn the_window_layout_holds_only_the_size() {
    let mut frames = Frames::new();
    let node = frames.frame(
        keyed(2u8, fill(), |_| {
            cosmic::widget::column::with_children(vec![bar(&10), bar(&20)]).into()
        }),
        300.0,
    );
    assert!(node.children().is_empty(), "the kept layout is not copied");
    let kept = content_layout::<Message, u8>(frames.tree.as_ref().unwrap());
    assert_eq!(kept.children().len(), 2);
}

#[test]
fn a_tree_of_another_kind_builds_once() {
    let mut frames = Frames::new();
    frames.frame(bar(&5), 300.0);
    let built = builds();
    frames.frame(keyed_bar(20), 300.0);
    assert_eq!(
        builds() - built,
        1,
        "state and the diff after it share one build"
    );
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "declares the size")]
fn a_subtree_declaring_another_size_is_refused() {
    let mut frames = Frames::new();
    frames.frame(keyed(1u8, fill(), |_| bar(&4)), 300.0);
}

/// A leaf that asks for a new layout on every event, as a widget that changed
/// its own size does, and whose overlay does the same.
struct Resizing(Rc<Cell<usize>>);
struct Menu;

impl Widget<Message, Theme, Renderer> for Resizing {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fixed(20.0))
    }

    fn layout(&mut self, _: &mut Tree, _: &Renderer, limits: &layout::Limits) -> layout::Node {
        self.0.set(self.0.get() + 1);
        layout::Node::new(limits.resolve(Length::Fill, Length::Fixed(20.0), Size::ZERO))
    }

    fn update(
        &mut self,
        _: &mut Tree,
        _: &Event,
        _: Layout<'_>,
        _: mouse::Cursor,
        _: &Renderer,
        _: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _: &Rectangle,
    ) {
        shell.invalidate_layout();
    }

    fn draw(
        &self,
        _: &Tree,
        _: &mut Renderer,
        _: &Theme,
        _: &renderer::Style,
        _: Layout<'_>,
        _: mouse::Cursor,
        _: &Rectangle,
    ) {
    }

    fn overlay<'a>(
        &'a mut self,
        _: &'a mut Tree,
        _: Layout<'a>,
        _: &Renderer,
        _: &Rectangle,
        _: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        Some(overlay::Element::new(Box::new(Menu)))
    }
}

impl overlay::Overlay<Message, Theme, Renderer> for Menu {
    fn layout(&mut self, _: &Renderer, bounds: Size) -> layout::Node {
        layout::Node::new(bounds)
    }

    fn draw(
        &self,
        _: &mut Renderer,
        _: &Theme,
        _: &renderer::Style,
        _: Layout<'_>,
        _: mouse::Cursor,
    ) {
    }

    fn update(
        &mut self,
        _: &Event,
        _: Layout<'_>,
        _: mouse::Cursor,
        _: &Renderer,
        _: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        shell.invalidate_layout();
    }
}

fn resizing(counter: &Rc<Cell<usize>>) -> Element<'static, Message> {
    let counter = Rc::clone(counter);
    vouched(
        1u8,
        Size::new(Length::Fill, Length::Fixed(20.0)),
        move || Element::new(Resizing(counter)),
    )
}

fn moved() -> Event {
    Event::Mouse(mouse::Event::CursorMoved {
        position: Point::new(4.0, 4.0),
    })
}

#[test]
fn a_widget_inside_that_resizes_itself_is_laid_out_again() {
    let counter = Rc::default();
    let mut frames = Frames::new();
    let node = frames.frame(resizing(&counter), 300.0);
    frames.frame(resizing(&counter), 300.0);
    assert_eq!(counter.get(), 1);

    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    let mut element = resizing(&counter);
    let tree = frames.tree.as_mut().unwrap();
    tree.diff(element.as_widget_mut());
    element.as_widget_mut().update(
        tree,
        &moved(),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &frames.renderer,
        &mut clipboard::Null,
        &mut shell,
        &Rectangle::with_size(node.size()),
    );
    assert!(shell.is_layout_invalid(), "the window lays out again");
    frames.frame(resizing(&counter), 300.0);
    assert_eq!(counter.get(), 2, "and so does the subtree");
}

#[test]
fn an_overlay_inside_that_asks_lays_the_subtree_out_again() {
    let counter = Rc::default();
    let mut frames = Frames::new();
    let node = frames.frame(resizing(&counter), 300.0);
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    let mut element = resizing(&counter);
    let tree = frames.tree.as_mut().unwrap();
    tree.diff(element.as_widget_mut());
    {
        let viewport = Rectangle::with_size(Size::new(300.0, 400.0));
        let mut menu = element
            .as_widget_mut()
            .overlay(
                tree,
                Layout::new(&node),
                &frames.renderer,
                &viewport,
                Vector::ZERO,
            )
            .expect("the overlay is forwarded");
        let menu_node = menu
            .as_overlay_mut()
            .layout(&frames.renderer, viewport.size());
        menu.as_overlay_mut().update(
            &moved(),
            Layout::new(&menu_node),
            mouse::Cursor::Unavailable,
            &frames.renderer,
            &mut clipboard::Null,
            &mut shell,
        );
    }
    assert!(shell.is_layout_invalid());
    frames.frame(resizing(&counter), 300.0);
    assert_eq!(counter.get(), 2);
}

#[test]
fn another_widgets_relayout_keeps_the_kept_layout() {
    let mut frames = Frames::new();
    let node = frames.frame(keyed_bar(20), 300.0);
    let laid = layouts();
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    shell.invalidate_layout();
    let mut element = keyed_bar(20);
    let tree = frames.tree.as_mut().unwrap();
    tree.diff(element.as_widget_mut());
    element.as_widget_mut().update(
        tree,
        &moved(),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &frames.renderer,
        &mut clipboard::Null,
        &mut shell,
        &Rectangle::with_size(node.size()),
    );
    assert!(shell.is_layout_invalid(), "the earlier request stands");
    frames.frame(keyed_bar(20), 300.0);
    assert_eq!(layouts(), laid);
}

/// What an operation finds: focusable widgets by id with their bounds and
/// focus, and scrollables by id with their offsets.
#[derive(Default)]
struct Found {
    focusable: Vec<(Id, Rectangle, bool)>,
    scrolled: Vec<(Id, Vector)>,
}

impl operation::Operation for Found {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn operation::Operation)) {
        operate(self);
    }

    fn focusable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if let Some(id) = id {
            self.focusable
                .push((id.clone(), bounds, state.is_focused()));
        }
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        _: Rectangle,
        _: Rectangle,
        translation: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        if let Some(id) = id {
            self.scrolled.push((id.clone(), translation));
        }
    }
}

/// A named scrollable over a tall list, with a focusable button first, as
/// `key` lists `rows` rows.
fn list(rows: &usize) -> Element<'static, Message> {
    let mut items = vec![
        cosmic::widget::button::text("First")
            .id(Id::new("retained.test.first"))
            .on_press(())
            .into(),
    ];
    items.extend((0..*rows).map(|_| bar(&40)));
    cosmic::iced::widget::scrollable(cosmic::widget::column::with_children(items))
        .id(Id::new("retained.test.scroll"))
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// The list placed below a fixed bar, so the window lays the subtree out away
/// from the origin.
fn placed(rows: usize) -> Element<'static, Message> {
    cosmic::widget::column::with_children(vec![bar(&30), keyed(rows, fill(), list)]).into()
}

fn operate(
    frames: &mut Frames,
    element: &mut Element<'_, Message>,
    node: &layout::Node,
    operation: &mut dyn Operation,
) {
    element.as_widget_mut().operate(
        frames.tree.as_mut().unwrap(),
        Layout::new(node),
        &frames.renderer,
        operation,
    );
}

#[test]
fn operations_reach_inside_where_the_window_placed_it() {
    let mut frames = Frames::new();
    let mut element = placed(20);
    let node = frames.frame(placed(20), 300.0);
    frames.tree.as_mut().unwrap().diff(element.as_widget_mut());
    let first = Id::new("retained.test.first");
    operate(
        &mut frames,
        &mut element,
        &node,
        &mut operation::focusable::focus::<()>(first.clone()),
    );
    let mut found = Found::default();
    operate(&mut frames, &mut element, &node, &mut found);
    let (_, bounds, focused) = found
        .focusable
        .iter()
        .find(|(id, ..)| *id == first)
        .expect("the button inside is reached");
    assert!(focused, "focus reaches inside");
    assert_eq!(bounds.y, 30.0, "at the place the window gave the subtree");
}

#[test]
fn named_state_inside_is_kept_through_a_rebuild_and_not_swept() {
    let mut frames = Frames::new();
    let node = frames.frame(placed(20), 300.0);
    let mut element = placed(20);
    frames.tree.as_mut().unwrap().diff(element.as_widget_mut());
    let scroll = Id::new("retained.test.scroll");
    operate(
        &mut frames,
        &mut element,
        &node,
        &mut operation::scrollable::scroll_to::<()>(
            scroll.clone(),
            AbsoluteOffset {
                x: None,
                y: Some(100.0),
            },
        ),
    );
    assert!(
        frames.tree.as_mut().unwrap().take_all_named().is_empty(),
        "the runtime's sweep does not enter the subtree"
    );
    // A new key rebuilds the list, which reconciles its own named states.
    let mut element = placed(21);
    let node = frames.frame(placed(21), 300.0);
    frames.tree.as_mut().unwrap().diff(element.as_widget_mut());
    let mut found = Found::default();
    operate(&mut frames, &mut element, &node, &mut found);
    assert_eq!(found.scrolled, vec![(scroll, Vector::new(0.0, 100.0))]);
}

#[test]
fn drawing_events_accessibility_and_drops_reach_inside_where_placed() {
    let renderer = forwarding_test::renderer();
    let destinations = vec![(7, Rectangle::new(Point::new(1.0, 2.0), Size::new(5.0, 6.0)))];
    let (leaf, visits) =
        forwarding_test::leaf::<Message>(Size::new(40.0, 20.0), destinations.clone());
    let leaf = Rc::new(RefCell::new(Some(leaf)));
    let make = || {
        let leaf = Rc::clone(&leaf);
        vouched(
            1u8,
            Size::new(Length::Fixed(40.0), Length::Fixed(20.0)),
            move || leaf.borrow_mut().take().expect("built once"),
        )
    };
    let mut element: Element<'_, Message> =
        cosmic::widget::column::with_children(vec![bar(&30), make()]).into();
    let mut tree = Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(300.0, 400.0)),
    );
    let offset = Vector::new(3.0, 0.0);
    let layout = Layout::new(&node).with_virtual_offset(offset);
    let viewport = Rectangle::with_size(Size::new(300.0, 400.0));
    let mut messages = Vec::new();
    element.as_widget_mut().update(
        &mut tree,
        &moved(),
        layout,
        mouse::Cursor::Unavailable,
        &renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &viewport,
    );
    let _ = element.as_widget().mouse_interaction(
        &tree,
        layout,
        mouse::Cursor::Unavailable,
        &viewport,
        &renderer,
    );
    element
        .as_widget_mut()
        .operate(&mut tree, layout, &renderer, &mut Traverse);
    let mut drops = DndDestinationRectangles::new();
    element
        .as_widget()
        .drag_destinations(&tree, layout, &renderer, &mut drops);
    for action in ["update", "mouse_interaction", "operate", "drag"] {
        assert_eq!(visits.offsets(action), vec![offset], "{action}");
    }
    let rectangle = &drops.into_rectangles()[0];
    assert_eq!(rectangle.id, 7);
    assert_eq!((rectangle.rectangle.x, rectangle.rectangle.y), (1.0, 32.0));
}

#[test]
fn accessible_controls_inside_answer_where_placed() {
    let renderer = forwarding_test::renderer();
    let make = || {
        vouched(
            1u8,
            Size::new(Length::Fixed(80.0), Length::Fixed(24.0)),
            || forwarding_test::native_button("Inside", Size::new(80.0, 24.0)),
        )
    };
    let mut element: Element<'_, forwarding_test::Press> =
        cosmic::widget::column::with_children(vec![
            cosmic::iced::widget::Space::new()
                .height(Length::Fixed(30.0))
                .into(),
            make(),
        ])
        .into();
    let mut tree = Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(300.0, 400.0)),
    );
    let controls =
        forwarding_test::native_controls(&mut element, &mut tree, &node, Vector::ZERO, &renderer);
    assert_eq!(controls.len(), 1);
    let (label, bounds, presses) = &controls[0];
    assert_eq!(label, "Inside");
    assert_eq!(bounds.y0, 30.0);
    assert_eq!(presses.len(), 1, "a native click presses the button inside");
}
