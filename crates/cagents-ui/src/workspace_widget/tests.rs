use super::*;
use crate::settings::parse_chords;
use crate::workspace::{CardWidth, WorkspaceCard};
use std::collections::HashSet;

#[test]
fn workspace_surface_forwards_drop_targets_in_native_collection_order() {
    use super::forwarding_test::{leaf, renderer};
    use cosmic::iced::advanced::clipboard::DndDestinationRectangles;
    use cosmic::iced::clipboard::dnd::{DndAction, Rectangle as DndRectangle};

    let renderer = renderer();
    let shortcuts = KeyboardShortcuts::default();
    let offset = Vector::new(7.0, -22.0);
    for wrapper in [
        "plain",
        "surface",
        "scroll",
        "file drop",
        "scroll and file drop",
        "file drop at 2x",
    ] {
        let scale = if wrapper.ends_with("2x") { 2.0 } else { 1.0 };
        let (content, visits) = leaf::<()>(
            Size::new(100.0, 60.0),
            vec![
                (
                    1,
                    Rectangle::new(Point::new(-30.0, 0.0), Size::new(100.0, 40.0)),
                ),
                (
                    2,
                    Rectangle::new(Point::new(10.0, 0.0), Size::new(70.0, 40.0)),
                ),
                (
                    3,
                    Rectangle::new(Point::new(200.0, 0.0), Size::new(50.0, 40.0)),
                ),
            ],
        );
        let content: Element<'_, ()> = match wrapper {
            "surface" => {
                WorkspaceSurface::new(content, &shortcuts, FocusedPane::Other, 100.0, |_| ()).into()
            }
            "scroll" | "scroll and file drop" => cosmic::widget::scrollable(content)
                .width(Length::Fill)
                .height(Length::Fill)
                .into(),
            _ => content,
        };
        let mut surface: Element<'_, ()> =
            WorkspaceSurface::new(content, &shortcuts, FocusedPane::Other, 100.0, |_| ()).into();
        if wrapper.contains("file drop") {
            surface = crate::file_drop::surface(surface, scale);
        }
        let mut tree = Tree::new(&surface);
        let node = surface
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(100.0, 60.0)),
            )
            .move_to(Point::new(20.0, 30.0));
        let mut destinations = DndDestinationRectangles::new();
        // A preceding root sibling is already registered. The surface
        // must preserve its own later child order within that collection.
        destinations.push(DndDestinationRectangle {
            id: 99,
            rectangle: DndRectangle {
                x: 0.0,
                y: 0.0,
                width: 10.0,
                height: 10.0,
            },
            mime_types: Vec::new(),
            actions: DndAction::Copy,
            preferred: DndAction::Copy,
        });
        surface.as_widget().drag_destinations(
            &tree,
            Layout::new(&node).with_virtual_offset(offset),
            &renderer,
            &mut destinations,
        );
        assert_eq!(
            destinations
                .as_ref()
                .iter()
                .map(|target| target.id)
                .collect::<Vec<_>>(),
            vec![99, 1, 2],
            "each internal wrapper preserves registration order: {wrapper}"
        );
        // This is the exact extraction performed by the iced native
        // consumer, after all enclosing widgets finished registration.
        let actual: Vec<_> = destinations
            .into_native_rectangles()
            .into_iter()
            .map(|target| {
                (
                    target.id,
                    target.rectangle.x,
                    target.rectangle.y,
                    target.rectangle.width,
                    target.rectangle.height,
                )
            })
            .collect();
        // The scale wrapper converts logical targets to physical ones;
        // the sibling registered before it is not scaled.
        let scaled = |id, x: f64, y: f64, width: f64, height: f64| {
            let scale = f64::from(scale);
            (id, x * scale, y * scale, width * scale, height * scale)
        };
        assert_eq!(
            actual,
            vec![
                scaled(2, 30.0, 30.0, 70.0, 40.0),
                scaled(1, 20.0, 30.0, 70.0, 40.0),
                (99, 0.0, 0.0, 10.0, 10.0)
            ],
            "{wrapper}"
        );
        assert_eq!(visits.offsets("drag"), vec![offset]);
    }
}

#[test]
fn drop_targets_end_at_the_edges_of_the_strip() {
    use cosmic::iced::clipboard::dnd::{DndAction, Rectangle as DndRectangle};

    let destination = |id, x| DndDestinationRectangle {
        id,
        rectangle: DndRectangle {
            x,
            y: 0.0,
            width: 400.0,
            height: 600.0,
        },
        mime_types: Vec::new(),
        actions: DndAction::Copy,
        preferred: DndAction::Copy,
    };
    // A strip beside a 240-wide sidebar, under a 40-high header.
    let strip = Rectangle::new(Point::new(240.0, 40.0), Size::new(800.0, 500.0));
    let mut rectangles = vec![
        destination(1, 0.0),
        destination(2, 400.0),
        destination(3, 1040.0),
    ];
    clip_drag_destinations(&mut rectangles, strip);
    let kept: Vec<_> = rectangles
        .iter()
        .map(|destination| {
            let rectangle = &destination.rectangle;
            (
                destination.id,
                rectangle.x,
                rectangle.y,
                rectangle.width,
                rectangle.height,
            )
        })
        .collect();
    assert_eq!(
        kept,
        // The card scrolled half out of view keeps only its visible part;
        // the one wholly past the edge takes no drops at all.
        vec![
            (1, 240.0, 40.0, 160.0, 500.0),
            (2, 400.0, 40.0, 400.0, 500.0)
        ]
    );
}

#[test]
fn card_cursor_uses_translated_child_coordinates_without_reviving_blocked_hover() {
    use cosmic::iced::advanced::{clipboard, renderer::Headless};
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
    let content = cosmic::widget::text_editor::Content::with_text("draft");
    let editor = widget::text_editor::TextEditor::new(&content)
        .width(100.0)
        .height(Length::Fill)
        .on_action(|_| "edit");
    let button = cosmic::widget::button::text("Action")
        .width(Length::Fixed(100.0))
        .height(Length::Fill)
        .on_press("click");
    let row =
        cosmic::widget::row::with_children(vec![editor.into(), button.into()]).height(Length::Fill);
    let mut surface: Element<'_, &str> = CardSurface::new(row).on_pointer_rest(Some("rest")).into();
    let mut tree = Tree::new(&surface);
    let node = surface
        .as_widget_mut()
        .layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::new(200.0, 80.0), Size::new(200.0, 80.0)),
        )
        .move_to(Point::new(100.0, 0.0));
    let viewport = Rectangle::new(Point::new(100.0, 0.0), Size::new(200.0, 80.0));
    let mut messages = Vec::new();
    let row_layout = Layout::new(&node).children().next().unwrap();
    let mut controls = row_layout.children();
    let editor_center = controls.next().unwrap().bounds().center();
    let button_center = controls.next().unwrap().bounds().center();
    for (position, expected) in [
        (editor_center, mouse::Interaction::Text),
        (button_center, mouse::Interaction::Pointer),
    ] {
        let cursor = mouse::Cursor::Available(position);
        surface.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::CursorMoved {
                position: position - Vector::new(100.0, 0.0),
            }),
            Layout::new(&node),
            cursor,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert_eq!(
            surface.as_widget().mouse_interaction(
                &tree,
                Layout::new(&node),
                cursor,
                &viewport,
                &renderer,
            ),
            expected
        );
        for blocked in [mouse::Cursor::Unavailable, cursor.levitate()] {
            assert_eq!(
                surface.as_widget().mouse_interaction(
                    &tree,
                    Layout::new(&node),
                    blocked,
                    &viewport,
                    &renderer,
                ),
                mouse::Interaction::None
            );
        }
    }
    // Moving about inside the pane never crossed into it.
    assert!(messages.is_empty());
}

/// Drives one [`CardSurface`] spanning x 100..300 of the window, beneath
/// the window's input trace, in the window's own coordinates unless a
/// sample says otherwise.
struct RestProbe {
    renderer: Renderer,
    surface: Element<'static, &'static str>,
    tree: Tree,
    node: layout::Node,
    messages: Vec<&'static str>,
    input: InputTrace,
    /// The cursor as the pane last saw it, which frames report too.
    cursor: mouse::Cursor,
}

impl RestProbe {
    const OUTSIDE: Point = Point::new(50.0, 40.0);
    const INSIDE: Point = Point::new(150.0, 40.0);
    /// Where the pointer is over the next pane along.
    const NEIGHBOUR: Point = Point::new(350.0, 40.0);

    fn new() -> Self {
        use cosmic::iced::advanced::renderer::Headless;
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
        let input = InputTrace::default();
        let mut surface = crate::input_trace::observe(
            CardSurface::new(
                widget::Space::new()
                    .width(Length::Fill)
                    .height(Length::Fill),
            )
            .traced_by(&input)
            .on_pointer_focus("click", "other click")
            .on_pointer_rest(Some("rest")),
            &input,
        );
        let mut tree = Tree::new(&surface);
        let node = surface
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::new(200.0, 80.0), Size::new(200.0, 80.0)),
            )
            .move_to(Point::new(100.0, 0.0));
        Self {
            renderer,
            surface,
            tree,
            node,
            messages: Vec::new(),
            input,
            cursor: mouse::Cursor::Unavailable,
        }
    }

    fn send(&mut self, event: Event, cursor: mouse::Cursor) {
        use cosmic::iced::advanced::clipboard;
        self.surface.as_widget_mut().update(
            &mut self.tree,
            &event,
            Layout::new(&self.node),
            cursor,
            &self.renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut self.messages),
            &Rectangle::new(Point::new(100.0, 0.0), Size::new(200.0, 80.0)),
        );
    }

    /// Input the window delivered somewhere other than the pane: to an
    /// overlay that captured it, or to a widget that captured it before
    /// the strip, or while the pane was not mounted.
    fn elsewhere(&self, event: &Event) {
        self.input.record(event);
    }

    /// The pointer at `window` in the window, which the pane sees as
    /// `local` once the strip's scroll translation is applied.
    fn move_translated(&mut self, window: Point, local: mouse::Cursor) {
        self.cursor = local;
        self.send(
            Event::Mouse(mouse::Event::CursorMoved { position: window }),
            local,
        );
    }

    fn move_to(&mut self, position: Point) {
        self.move_translated(position, mouse::Cursor::Available(position));
    }

    /// Delivers the frame `after` now.
    fn frame(&mut self, after: Duration) {
        self.frame_with(after, self.cursor);
    }

    /// Delivers the frame `after` now, with the pane seeing the cursor at
    /// `cursor` by then.
    fn frame_with(&mut self, after: Duration, cursor: mouse::Cursor) {
        self.send(
            Event::Window(window::Event::RedrawRequested(Instant::now() + after)),
            cursor,
        );
    }

    /// Frames before and after the dwell elapses, and everything the pane
    /// published since the last look.
    fn rest(&mut self) -> Vec<&'static str> {
        self.frame(Duration::ZERO);
        self.frame(HOVER_ACTIVATION_DWELL * 2);
        self.frame(HOVER_ACTIVATION_DWELL * 3);
        mem::take(&mut self.messages)
    }
}

fn moved(position: Point) -> Event {
    Event::Mouse(mouse::Event::CursorMoved { position })
}

#[test]
fn a_pane_activates_on_hover_only_after_the_pointer_crosses_in_and_rests() {
    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::OUTSIDE);
    probe.move_to(RestProbe::INSIDE);
    probe.frame(Duration::ZERO);
    assert!(probe.messages.is_empty(), "activation waits for the dwell");
    // Moving about inside the pane keeps the dwell it started, and the
    // rest is reported once however many frames follow.
    probe.move_to(Point::new(180.0, 60.0));
    assert_eq!(probe.rest(), ["rest"]);

    // Sweeping through on the way somewhere else activates nothing.
    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::OUTSIDE);
    probe.move_to(RestProbe::INSIDE);
    probe.move_to(RestProbe::NEIGHBOUR);
    assert!(probe.rest().is_empty());
}

#[test]
fn typing_or_clicking_before_the_dwell_keeps_focus_where_it_is() {
    let key = Event::Keyboard(key_press(
        "a",
        keyboard::key::Code::KeyA,
        keyboard::Modifiers::empty(),
        false,
    ));
    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::OUTSIDE);
    probe.move_to(RestProbe::INSIDE);
    probe.send(key.clone(), mouse::Cursor::Available(RestProbe::INSIDE));
    // Further drift inside the pane does not re-arm what typing cancelled.
    probe.move_to(Point::new(160.0, 40.0));
    assert!(probe.rest().is_empty());

    // A shortcut the workspace captures, or a key a focused input
    // elsewhere takes, never reaches the pane, and cancels it all the same.
    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::OUTSIDE);
    probe.move_to(RestProbe::INSIDE);
    probe.elsewhere(&key);
    assert!(probe.rest().is_empty());

    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::OUTSIDE);
    probe.move_to(RestProbe::INSIDE);
    probe.send(
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        mouse::Cursor::Available(RestProbe::INSIDE),
    );
    // The click activates the pane itself; the dwell adds nothing.
    assert_eq!(probe.rest(), ["click"]);
}

/// The pane was entered, but by the time the dwell ends it is no longer
/// where the pointer rests.
#[test]
fn a_pane_that_left_the_pointer_during_the_dwell_is_not_activated() {
    // Keyboard navigation or a new pane moves the strip, and the pointer,
    // which never moved, now rests over a neighbour.
    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::OUTSIDE);
    probe.move_to(RestProbe::INSIDE);
    probe.frame_with(
        HOVER_ACTIVATION_DWELL * 2,
        mouse::Cursor::Available(RestProbe::NEIGHBOUR),
    );
    // Nor does it activate once scrolled back under the pointer.
    assert!(probe.rest().is_empty());

    // The pane scrolled out of view, where it receives no frames, and its
    // first one back comes long after the pointer stopped over it.
    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::OUTSIDE);
    probe.move_to(RestProbe::INSIDE);
    probe.frame(HOVER_ACTIVATION_DWELL + HOVER_ACTIVATION_LATENESS * 2);
    assert!(probe.rest().is_empty());
}

/// A pane that comes out from under something the pointer was already
/// over was not entered: the reader never moved toward it.
#[test]
fn uncovering_a_pane_under_a_still_pointer_is_not_an_entry() {
    // A dropdown drawn over the pane leaves it the motions beneath, but
    // with no cursor, and then closes under the pointer.
    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::OUTSIDE);
    probe.move_translated(RestProbe::INSIDE, mouse::Cursor::Unavailable);
    assert!(probe.rest().is_empty(), "an overlay covers the pane");
    probe.move_to(Point::new(151.0, 40.0));
    assert!(probe.rest().is_empty());

    // A context menu opened from outside the pane captures every motion
    // over it, and then an item is chosen over the pane.
    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::OUTSIDE);
    probe.elsewhere(&moved(RestProbe::INSIDE));
    probe.move_to(Point::new(151.0, 40.0));
    assert!(probe.rest().is_empty());

    // A popup surface outside the window closes, returning the pointer.
    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::OUTSIDE);
    probe.send(
        Event::Mouse(mouse::Event::CursorLeft),
        mouse::Cursor::Unavailable,
    );
    probe.move_to(RestProbe::INSIDE);
    assert!(probe.rest().is_empty());

    // The strip scrolls the pane under a pointer that stays put.
    let mut probe = RestProbe::new();
    probe.move_translated(
        RestProbe::INSIDE,
        mouse::Cursor::Available(RestProbe::NEIGHBOUR),
    );
    probe.move_to(Point::new(151.0, 40.0));
    assert!(probe.rest().is_empty());

    // A pane mounted beneath the pointer has no sample to cross from.
    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::INSIDE);
    assert!(probe.rest().is_empty());

    // A pane remounted after a far jump last saw the pointer where it was
    // before; the pointer has since moved on beneath other panes and
    // stopped where the pane now comes to lie.
    let mut probe = RestProbe::new();
    probe.move_to(RestProbe::OUTSIDE);
    probe.elsewhere(&moved(Point::new(149.0, 40.0)));
    probe.elsewhere(&moved(RestProbe::INSIDE));
    probe.move_to(Point::new(151.0, 40.0));
    assert!(probe.rest().is_empty());
}

#[test]
fn parked_presentation_reclaims_named_state_before_releasing_its_subtree() {
    let ids = [CardId::new(), CardId::new()];
    let retained = Arc::new(HashMap::from([(ids[0], 0), (ids[1], 1)]));
    let make = |index: usize| -> Element<'_, ()> {
        VirtualStrip {
            total_width: 320.0,
            viewport_width: 320.0,
            retained: retained.clone(),
            focused: Some(ids[index]),
            cards: vec![VirtualCard {
                id: ids[index],
                x: 0.0,
                width: 320.0,
                content_width: 320.0,
                on_screen: true,
                content: cosmic::widget::id_container(
                    crate::ui_memory::gate(widget::Space::new()),
                    widget::Id::new(format!("disposable-presentation-{index}")),
                )
                .into(),
            }],
        }
        .into()
    };
    let mut element = make(0);
    let mut tree = Tree::new(&element);
    let owner = Arc::new(());
    let released = Arc::downgrade(&owner);
    // Model a retained/shaped allocation owned by the presentation tree.
    // The named ancestor moves this entire subtree into iced's pool.
    tree.children[0].children[0].children[0].state = tree::State::new(owner);
    tree::NAMED.with(|cache| *cache.borrow_mut() = tree.take_all_named());
    assert!(released.upgrade().is_some());
    element = make(1);
    tree.diff(&mut element);
    assert!(
        released.upgrade().is_none(),
        "named extraction must not leave a presentation owner behind"
    );
    let parked = &tree.state.downcast_ref::<VirtualStripState>().detached[&ids[0]];
    assert!(parked.children[0].children.is_empty());
    // A real remount reconstructs the emptied gate through normal diffing.
    tree::NAMED.with(|cache| *cache.borrow_mut() = tree.take_all_named());
    element = make(0);
    tree.diff(&mut element);
    assert_eq!(tree.children[0].children[0].children.len(), 1);
    tree::NAMED.with(|cache| cache.borrow_mut().clear());
}

#[test]
fn parked_editor_preserves_focus_across_named_state_extraction() {
    use cosmic::iced::advanced::{renderer::Headless, widget::operation};
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
    let ids = [CardId::new(), CardId::new()];
    let retained = Arc::new(HashMap::from([(ids[0], 0), (ids[1], 1)]));
    let content = cosmic::widget::text_editor::Content::with_text("retained draft");
    let editor = widget::Id::new("parked-editor");
    let make = |index: usize| -> Element<'_, ()> {
        let child = widget::text_editor::TextEditor::new(&content)
            .id(if index == 0 {
                editor.clone()
            } else {
                widget::Id::new("other-editor")
            })
            .on_action(|_| ());
        VirtualStrip {
            total_width: 320.0,
            viewport_width: 320.0,
            retained: retained.clone(),
            focused: Some(ids[0]),
            cards: vec![VirtualCard {
                id: ids[index],
                x: 0.0,
                width: 320.0,
                content_width: 320.0,
                on_screen: true,
                content: cosmic::widget::id_container(
                    child,
                    widget::Id::new(format!("parked-card-{index}")),
                )
                .into(),
            }],
        }
        .into()
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(320.0, 200.0));
    let mut element = make(0);
    let mut tree = Tree::new(&element);
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::focusable::focus::<()>(editor.clone()),
    );
    for index in [1, 1, 0] {
        // Reproduce the real runtime, which extracts names before every
        // view diff and drops unclaimed names at the next frame.
        tree::NAMED.with(|cache| *cache.borrow_mut() = tree.take_all_named());
        element = make(index);
        tree.diff(&mut element);
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
    }
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut focused = operation::focusable::find_focused();
    let mut probe = operation::black_box::<_, ()>(&mut focused);
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
    drop(probe);
    assert!(matches!(focused.finish(), operation::Outcome::Some(id) if id == editor));
    tree::NAMED.with(|cache| cache.borrow_mut().clear());
}

#[test]
fn remounting_a_retargeted_editor_does_not_restore_stale_focus() {
    use cosmic::iced::advanced::{renderer::Headless, widget::operation};

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
    let ids = [CardId::new(), CardId::new()];
    let editors = [
        widget::Id::new("retarget-old"),
        widget::Id::new("retarget-new"),
    ];
    let retained = Arc::new(HashMap::from([(ids[0], 0), (ids[1], 1)]));
    let contents = [
        cosmic::widget::text_editor::Content::with_text("old destination draft"),
        cosmic::widget::text_editor::Content::with_text("new destination draft"),
    ];
    let make = |indices: &[usize], focused: usize| -> Element<'_, ()> {
        VirtualStrip {
            total_width: 640.0,
            viewport_width: 640.0,
            retained: retained.clone(),
            focused: Some(ids[focused]),
            cards: indices
                .iter()
                .map(|&index| VirtualCard {
                    id: ids[index],
                    x: index as f32 * 320.0,
                    width: 320.0,
                    content_width: 320.0,
                    on_screen: true,
                    content: cosmic::widget::id_container(
                        widget::text_editor::TextEditor::new(&contents[index])
                            .id(editors[index].clone())
                            .on_action(|_| ()),
                        widget::Id::new(format!("retarget-card-{index}")),
                    )
                    .into(),
                })
                .collect(),
        }
        .into()
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(640.0, 200.0));
    let mut element = make(&[0], 0);
    let mut tree = Tree::new(&element);
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::focusable::focus::<()>(editors[0].clone()),
    );

    // Retargeting parks the old destination before the focus operation
    // visits the newly mounted destination. The parked tree is unreachable.
    tree::NAMED.with(|cache| *cache.borrow_mut() = tree.take_all_named());
    element = make(&[1], 1);
    tree.diff(&mut element);
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::focusable::focus::<()>(editors[1].clone()),
    );

    // Camera movement brings the old destination back without requesting
    // widget focus. Both editors are now traversable in the same strip.
    tree::NAMED.with(|cache| *cache.borrow_mut() = tree.take_all_named());
    element = make(&[0, 1], 1);
    tree.diff(&mut element);
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut focus_count = FocusedWidgets::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut focus_count);
    assert_eq!(focus_count.0, vec![editors[1].clone()]);

    // The diff implementation also temporarily parks cards that remain
    // mounted while their neighbours leave. That storage shuffle must
    // not blur a control that never left the traversable widget tree.
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::focusable::focus::<()>(editors[0].clone()),
    );
    tree::NAMED.with(|cache| *cache.borrow_mut() = tree.take_all_named());
    element = make(&[0], 1);
    tree.diff(&mut element);
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut focus_count = FocusedWidgets::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut focus_count);
    assert_eq!(focus_count.0, vec![editors[0].clone()]);
    tree::NAMED.with(|cache| cache.borrow_mut().clear());
}

#[test]
fn loading_focus_preserves_anonymous_and_sidebar_controls() {
    use cosmic::iced::advanced::widget::operation::{Focusable, Outcome};
    use cosmic::iced::futures::{StreamExt, executor::block_on};

    struct Focus(bool, usize);
    impl Focusable for Focus {
        fn is_focused(&self) -> bool {
            self.0
        }
        fn focus(&mut self) {
            self.0 = true;
        }
        fn unfocus(&mut self) {
            self.0 = false;
            self.1 += 1;
        }
    }

    for (foreign_id, foreign_focused, placeholder_focused) in [
        (None, true, false),
        (Some(widget::Id::new("sidebar.search")), true, false),
        (None, false, false),
        (None, false, true),
    ] {
        let placeholder = widget::Id::new("loading-pane.header");
        let target = widget::Id::new("loaded-pane.editor");
        let mut header = Focus(placeholder_focused, 0);
        let mut foreign = Focus(foreign_focused, 0);
        let mut editor = Focus(false, 0);
        let mut stream = cosmic::iced::runtime::task::into_stream(focus_after_loading::<()>(
            placeholder.clone(),
            target.clone(),
        ))
        .expect("loading handoff must issue a widget operation");
        while let Some(action) = block_on(stream.next()) {
            let cosmic::iced::runtime::Action::Widget(mut operation) = action else {
                continue;
            };
            loop {
                let bounds = Rectangle::default();
                operation.focusable(Some(&placeholder), bounds, &mut header);
                operation.focusable(foreign_id.as_ref(), bounds, &mut foreign);
                operation.focusable(Some(&target), bounds, &mut editor);
                match operation.finish() {
                    Outcome::Chain(next) => operation = next,
                    _ => break,
                }
            }
        }
        assert_eq!(foreign.0, foreign_focused, "keep the chosen control");
        assert_eq!(
            editor.0, !foreign_focused,
            "handoff only without foreign focus"
        );
        assert_eq!(header.1, usize::from(placeholder_focused));
        assert_eq!(foreign.1, 0, "do not unfocus an already unfocused control");
        assert_eq!(editor.1, 0, "do not clear the target's selection");
    }
}

#[test]
fn hover_and_activation_focus_keep_a_control_the_person_is_using() {
    use cosmic::iced::advanced::widget::operation::{Focusable, Outcome};
    use cosmic::iced::futures::{StreamExt, executor::block_on};

    struct Focus(bool);
    impl Focusable for Focus {
        fn is_focused(&self) -> bool {
            self.0
        }
        fn focus(&mut self) {
            self.0 = true;
        }
        fn unfocus(&mut self) {
            self.0 = false;
        }
    }

    let workspace = widget::Id::new("workspace.strip");
    let sidebar_id = widget::Id::new("sidebar.search");
    let other_id = widget::Id::new("left-pane.composer");
    let target = widget::Id::new("right-pane.composer");
    // (held, mounted, rest focuses, activation focuses)
    for (held, mounted, after_rest, after_activation) in [
        (None, true, true, true),
        (Some(&sidebar_id), true, false, false),
        (Some(&other_id), true, true, false),
        (None, false, false, false),
    ] {
        for activation in [false, true] {
            let mut sidebar = Focus(held == Some(&sidebar_id));
            let mut other = Focus(held == Some(&other_id));
            let mut editor = Focus(false);
            let task = if activation {
                focus_if_unheld::<()>(target.clone())
            } else {
                focus_unless_held_outside::<()>(workspace.clone(), target.clone())
            };
            let mut stream = cosmic::iced::runtime::task::into_stream(task)
                .expect("the handoff must issue a widget operation");
            while let Some(action) = block_on(stream.next()) {
                let cosmic::iced::runtime::Action::Widget(mut operation) = action else {
                    continue;
                };
                loop {
                    let bounds = Rectangle::default();
                    operation.container(Some(&widget::Id::new("sidebar")), bounds);
                    operation.traverse(&mut |operation| {
                        operation.focusable(Some(&sidebar_id), bounds, &mut sidebar);
                    });
                    operation.container(Some(&workspace), bounds);
                    operation.traverse(&mut |operation| {
                        operation.focusable(Some(&other_id), bounds, &mut other);
                        if mounted {
                            operation.focusable(Some(&target), bounds, &mut editor);
                        }
                    });
                    match operation.finish() {
                        Outcome::Chain(next) => operation = next,
                        _ => break,
                    }
                }
            }
            let expected = if activation {
                after_activation
            } else {
                after_rest
            };
            assert_eq!(
                editor.0, expected,
                "held={held:?} mounted={mounted} activation={activation}"
            );
            assert_eq!(
                sidebar.0,
                held == Some(&sidebar_id),
                "an outside field keeps the keyboard"
            );
            if !mounted {
                assert_eq!(other.0, held == Some(&other_id), "nothing is blurred");
            }
        }
    }
}

#[derive(Default)]
struct FocusedWidgets(Vec<widget::Id>);

impl Operation for FocusedWidgets {
    fn focusable(
        &mut self,
        id: Option<&widget::Id>,
        _bounds: Rectangle,
        state: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
    ) {
        if state.is_focused()
            && let Some(id) = id
        {
            self.0.push(id.clone());
        }
    }

    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
}

#[test]
fn projection_never_exposes_offscreen_cards_to_the_widget_builder() {
    let mut workspace = Workspace::default();
    workspace.resize_viewport(320.0);
    for index in 0..20 {
        workspace
            .add_card(
                WorkspaceCard::new(
                    CardId::new(),
                    format!("Card {index}"),
                    CardWidth::Fixed(320.0),
                )
                .unwrap(),
                index,
                false,
            )
            .unwrap();
    }
    workspace.native_scroll(3_200.0, 320.0);
    let projection = workspace.projection(320.0);
    assert!(projection.mounted.len() < workspace.cards().len());
    assert!(
        projection
            .mounted
            .iter()
            .all(|mounted| mounted.visibility.is_mounted())
    );
}
#[test]
fn an_opening_card_draws_and_takes_the_pointer_only_inside_its_slot() {
    let viewport = Rectangle::new(Point::ORIGIN, Size::new(900.0, 600.0));
    let card = |width, content_width| VirtualCard::<()> {
        id: CardId::new(),
        x: 300.0,
        width,
        content_width,
        on_screen: true,
        content: widget::Space::new().into(),
    };
    let bounds = |card: &VirtualCard<'_, ()>| {
        Rectangle::new(
            Point::new(card.x, 0.0),
            Size::new(card.content_width, 600.0),
        )
    };
    let inside = mouse::Cursor::Available(Point::new(320.0, 50.0));
    let beside = mouse::Cursor::Available(Point::new(420.0, 50.0));

    let opening = card(60.0, 300.0);
    let slot = opening.viewport(bounds(&opening), &viewport);
    assert_eq!(
        slot,
        Rectangle::new(Point::new(300.0, 0.0), Size::new(60.0, 600.0))
    );
    assert_eq!(opening.cursor(&slot, inside), inside);
    // Its contents reach under the neighbour there, which owns the pointer.
    assert!(beside.is_over(bounds(&opening)));
    assert_eq!(opening.cursor(&slot, beside), mouse::Cursor::Unavailable);

    // A slot past the viewport's edge shows and receives nothing.
    let hidden = Rectangle::new(Point::ORIGIN, Size::new(250.0, 600.0));
    let slot = opening.viewport(bounds(&opening), &hidden);
    assert!(!bounds(&opening).intersects(&slot));
    assert_eq!(opening.cursor(&slot, inside), mouse::Cursor::Unavailable);

    // A settled card keeps the strip's viewport and every pointer event,
    // including the moves and releases that end a drag outside it.
    let settled = card(300.0, 300.0);
    assert_eq!(settled.viewport(bounds(&settled), &viewport), viewport);
    let outside = mouse::Cursor::Available(Point::new(10.0, 50.0));
    assert_eq!(settled.cursor(&viewport, outside), outside);
}

#[test]
fn mounted_column_layout_and_clips_are_pairwise_disjoint() {
    let mut workspace = Workspace::default();
    workspace.resize_viewport(700.0);
    for (index, width) in [
        CardWidth::Fraction(1.0),
        CardWidth::Fixed(900.0),
        CardWidth::Fraction(0.45),
    ]
    .into_iter()
    .enumerate()
    {
        workspace
            .add_card(
                WorkspaceCard::new(CardId::new(), format!("Card {index}"), width).unwrap(),
                index,
                index == 0,
            )
            .unwrap();
    }
    workspace.native_scroll(500.0, 700.0);
    let projection = workspace.projection(700.0);
    let viewport = Rectangle::new(Point::new(500.0, 0.0), Size::new(700.0, 640.0));
    let columns: Vec<_> = projection
        .mounted
        .iter()
        .map(|card| {
            Rectangle::new(
                Point::new(card.start, 0.0),
                Size::new(card.width, viewport.height),
            )
        })
        .collect();

    assert!(projection.mounted_are_disjoint());
    assert!(
        columns
            .windows(2)
            .all(|pair| pair[0].x + pair[0].width <= pair[1].x)
    );
    for (column, clip) in columns
        .iter()
        .filter_map(|column| clipped_viewport(*column, &viewport).map(|clip| (column, clip)))
    {
        assert!(clip.x >= column.x);
        assert!(clip.x + clip.width <= column.x + column.width);
        assert!(clip.y >= column.y);
        assert!(clip.y + clip.height <= column.y + column.height);
        assert!(clip.x >= viewport.x);
        assert!(clip.x + clip.width <= viewport.x + viewport.width);
    }
}

#[test]
fn card_body_starts_below_header_and_remains_inside_its_column_clip() {
    const HEADER_HEIGHT: f32 = 44.0;
    let column = Rectangle::new(Point::new(12.0, 0.0), Size::new(480.0, 640.0));
    let viewport = Rectangle::new(Point::new(0.0, 0.0), Size::new(700.0, 640.0));
    let card_clip = clipped_viewport(column, &viewport).unwrap();
    let body = Rectangle::new(
        Point::new(column.x, column.y + HEADER_HEIGHT),
        Size::new(column.width, column.height - HEADER_HEIGHT),
    );
    let body_clip = clipped_viewport(body, &card_clip).unwrap();

    assert!(body_clip.y >= column.y + HEADER_HEIGHT);
    assert!(body_clip.x >= card_clip.x);
    assert!(body_clip.x + body_clip.width <= card_clip.x + card_clip.width);
    assert!(body_clip.y + body_clip.height <= card_clip.y + card_clip.height);
}

#[test]
fn frame_filter_rejects_other_windows() {
    let expected = window::Id::unique();
    let other = window::Id::unique();
    let instant = Instant::now();
    let match_frame = |(wanted, (actual, at)): (window::Id, (window::Id, Instant))| {
        (wanted == actual).then_some(at)
    };
    assert_eq!(match_frame((expected, (expected, instant))), Some(instant));
    assert_eq!(match_frame((expected, (other, instant))), None);
}

#[test]
fn card_container_and_title_focus_ids_are_stable_and_distinct() {
    let first = CardId::new();
    let second = CardId::new();
    assert_eq!(
        card_widget_id(first).to_string(),
        card_widget_id(first).to_string()
    );
    assert_eq!(
        card_focus_id(first).to_string(),
        card_focus_id(first).to_string()
    );
    assert_ne!(
        card_widget_id(first).to_string(),
        card_focus_id(first).to_string()
    );
    assert_ne!(
        card_focus_id(first).to_string(),
        card_focus_id(second).to_string()
    );
}

#[test]
fn viewport_events_keep_scroll_offsets_out_of_resize_notifications() {
    let scrolled = WorkspaceViewportEvent::Scrolled(ViewportSample {
        offset: 240.0,
        width: 800.0,
        user_input: true,
    });
    let resized = WorkspaceViewportEvent::Resized(800.0);

    assert_eq!(
        scrolled,
        WorkspaceViewportEvent::Scrolled(ViewportSample {
            offset: 240.0,
            width: 800.0,
            user_input: true,
        })
    );
    assert_eq!(resized, WorkspaceViewportEvent::Resized(800.0));
    assert_ne!(scrolled, resized);
}

fn key_press(
    character: &str,
    code: keyboard::key::Code,
    modifiers: keyboard::Modifiers,
    repeat: bool,
) -> keyboard::Event {
    let key = keyboard::Key::Character(character.into());
    keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Code(code),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat,
    }
}

fn named_key_press(
    named: keyboard::key::Named,
    code: keyboard::key::Code,
    modifiers: keyboard::Modifiers,
    repeat: bool,
) -> keyboard::Event {
    let key = keyboard::Key::Named(named);
    keyboard::Event::KeyPressed {
        key: key.clone(),
        modified_key: key,
        physical_key: keyboard::key::Physical::Code(code),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat,
    }
}

#[test]
fn global_shortcuts_match_configured_chords_and_repeat_presses() {
    use keyboard::key::Code;

    let shortcuts = KeyboardShortcuts::default();
    let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
    for (character, code, modifiers, action) in [
        ("T", Code::KeyT, ctrl_shift, GlobalShortcut::NewTerminal),
        ("G", Code::KeyG, ctrl_shift, GlobalShortcut::NewChannel),
        ("D", Code::KeyD, ctrl_shift, GlobalShortcut::ClonePane),
        (
            "W",
            Code::KeyW,
            keyboard::Modifiers::CTRL,
            GlobalShortcut::CloseFocusedCard,
        ),
    ] {
        assert_eq!(
            global_shortcut(
                &key_press(character, code, modifiers, false),
                &shortcuts,
                FocusedPane::Terminal,
            ),
            Some(action)
        );
        assert_eq!(
            global_shortcut(
                &key_press(character, code, modifiers, true),
                &shortcuts,
                FocusedPane::Terminal,
            ),
            None,
            "holding a creation or close shortcut must not repeat it"
        );
        assert!(is_global_shortcut_chord(
            &keyboard::Key::Character(character.into()),
            keyboard::key::Physical::Code(code),
            modifiers,
            &shortcuts,
            FocusedPane::Terminal,
        ));
    }
    assert_eq!(
        global_shortcut(
            &key_press(
                "F",
                Code::KeyF,
                keyboard::Modifiers::CTRL | keyboard::Modifiers::CAPS_LOCK,
                false,
            ),
            &shortcuts,
            FocusedPane::Other,
        ),
        Some(GlobalShortcut::FindInPane)
    );
    assert_eq!(
        global_shortcut(
            &key_press("f", Code::KeyF, keyboard::Modifiers::CTRL, true),
            &shortcuts,
            FocusedPane::Other,
        ),
        None,
        "held Ctrl shortcuts must not repeat"
    );
    for repeat in [false, true] {
        assert_eq!(
            global_shortcut(
                &key_press("A", Code::KeyA, keyboard::Modifiers::ALT, repeat),
                &shortcuts,
                FocusedPane::Other,
            ),
            None,
            "Alt+A must be forwarded on initial and repeated key presses"
        );
    }

    // Column navigation repeats while held, and is reserved against the
    // process inside a card.
    for (named, code, direction) in [
        (
            keyboard::key::Named::PageUp,
            Code::PageUp,
            FocusDirection::Left,
        ),
        (
            keyboard::key::Named::PageDown,
            Code::PageDown,
            FocusDirection::Right,
        ),
    ] {
        for repeat in [false, true] {
            let event = named_key_press(named, code, keyboard::Modifiers::CTRL, repeat);
            assert_eq!(
                global_shortcut(&event, &shortcuts, FocusedPane::Terminal),
                Some(GlobalShortcut::FocusAdjacent(direction)),
                "Ctrl+Page must navigate on initial and repeated presses"
            );
            assert!(is_global_shortcut_chord(
                &keyboard::Key::Named(named),
                keyboard::key::Physical::Code(code),
                keyboard::Modifiers::CTRL,
                &shortcuts,
                FocusedPane::Terminal,
            ));
        }
        // The unmodified and shifted paging keys stay the terminal's.
        for modifiers in [keyboard::Modifiers::empty(), keyboard::Modifiers::SHIFT] {
            assert!(!is_global_shortcut_chord(
                &keyboard::Key::Named(named),
                keyboard::key::Physical::Code(code),
                modifiers,
                &shortcuts,
                FocusedPane::Terminal,
            ));
        }
    }

    let mut customized = shortcuts;
    customized.new_terminal = vec!["Ctrl+Shift+N".to_owned()];
    customized.new_native_thread.clear();
    assert_eq!(
        global_shortcut(
            &key_press("n", Code::KeyN, ctrl_shift, false),
            &customized,
            FocusedPane::Other,
        ),
        Some(GlobalShortcut::NewTerminal)
    );
    customized.new_terminal = vec!["Ctrl+Shift+1".to_owned()];
    assert_eq!(
        global_shortcut(
            &key_press("!", Code::Digit1, ctrl_shift, false),
            &customized,
            FocusedPane::Other,
        ),
        Some(GlobalShortcut::NewTerminal)
    );
}

#[test]
fn new_tab_chord_is_reserved_in_every_pane_and_does_not_repeat() {
    use keyboard::key::Code;

    let shortcuts = KeyboardShortcuts::default();
    for focused in [
        FocusedPane::Other,
        FocusedPane::Terminal,
        FocusedPane::Review,
    ] {
        assert!(is_global_shortcut_chord(
            &keyboard::Key::Character("t".into()),
            keyboard::key::Physical::Code(Code::KeyT),
            keyboard::Modifiers::CTRL,
            &shortcuts,
            focused,
        ));
        for repeat in [false, true] {
            assert_eq!(
                global_shortcut(
                    &key_press("t", Code::KeyT, keyboard::Modifiers::CTRL, repeat),
                    &shortcuts,
                    focused,
                ),
                (!repeat).then_some(GlobalShortcut::NewThread),
            );
        }
    }
}

/// Shell chords outside the workspace's defaults reach a focused terminal.
#[test]
fn default_shortcuts_leave_shell_keys_to_a_focused_terminal() {
    use keyboard::key::{Code, Named};

    let shortcuts = KeyboardShortcuts::default();
    let character = |character: &str, code, modifiers| {
        (
            keyboard::Key::Character(character.into()),
            keyboard::key::Physical::Code(code),
            modifiers,
        )
    };
    let named = |named, code, modifiers| {
        (
            keyboard::Key::Named(named),
            keyboard::key::Physical::Code(code),
            modifiers,
        )
    };
    let ctrl = keyboard::Modifiers::CTRL;
    let alt = keyboard::Modifiers::ALT;
    for (key, physical, modifiers) in [
        character("r", Code::KeyR, ctrl),
        character("e", Code::KeyE, ctrl),
        character("g", Code::KeyG, ctrl),
        character("p", Code::KeyP, ctrl),
        character("q", Code::KeyQ, alt),
        character("w", Code::KeyW, alt),
        character("e", Code::KeyE, alt),
        character("g", Code::KeyG, alt),
        character("f", Code::KeyF, alt),
        character("b", Code::KeyB, alt),
        character(".", Code::Period, alt),
        named(Named::ArrowLeft, Code::ArrowLeft, alt),
        named(Named::ArrowRight, Code::ArrowRight, alt),
        named(Named::ArrowLeft, Code::ArrowLeft, ctrl),
        named(Named::ArrowRight, Code::ArrowRight, ctrl),
        named(Named::ArrowUp, Code::ArrowUp, alt),
        named(Named::ArrowDown, Code::ArrowDown, alt),
    ] {
        assert!(
            !is_global_shortcut_chord(&key, physical, modifiers, &shortcuts, FocusedPane::Terminal),
            "{modifiers:?}+{key:?} was withheld from a focused terminal"
        );
        let press = keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key.clone(),
            physical_key: physical,
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        };
        assert_eq!(
            global_shortcut(&press, &shortcuts, FocusedPane::Terminal),
            None,
            "{modifiers:?}+{key:?} acted over a focused terminal"
        );
    }
}

/// Project search and pane find are distinct commands from every pane.
#[test]
fn project_search_and_pane_find_have_distinct_workspace_chords() {
    use keyboard::key::Code;

    let shortcuts = KeyboardShortcuts::default();
    for focused in [
        FocusedPane::Terminal,
        FocusedPane::Other,
        FocusedPane::Review,
        FocusedPane::Chat,
    ] {
        for (modifiers, action) in [
            (keyboard::Modifiers::CTRL, GlobalShortcut::FindInPane),
            (
                keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                GlobalShortcut::FocusProjectSearch,
            ),
        ] {
            let event = key_press("f", Code::KeyF, modifiers, false);
            assert_eq!(global_shortcut(&event, &shortcuts, focused), Some(action));
            assert!(is_global_shortcut_chord(
                &keyboard::Key::Character("f".into()),
                keyboard::key::Physical::Code(Code::KeyF),
                modifiers,
                &shortcuts,
                focused,
            ));
        }
    }

    // A custom close chord releases the default to the terminal.
    let customized = KeyboardShortcuts {
        close_focused_column: vec!["Ctrl+Shift+W".into()],
        ..KeyboardShortcuts::default()
    };
    assert_eq!(
        global_shortcut(
            &key_press(
                "w",
                Code::KeyW,
                keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                false
            ),
            &customized,
            FocusedPane::Terminal,
        ),
        Some(GlobalShortcut::CloseFocusedCard)
    );
    assert_eq!(
        global_shortcut(
            &key_press("w", Code::KeyW, keyboard::Modifiers::CTRL, false),
            &customized,
            FocusedPane::Terminal
        ),
        None
    );
}

#[test]
fn pane_close_and_width_defaults_apply_even_over_a_terminal() {
    use keyboard::key::Code;
    let shortcuts = KeyboardShortcuts::default();
    for focused in [
        FocusedPane::Terminal,
        FocusedPane::Review,
        FocusedPane::Other,
    ] {
        for (character, code, modifiers, action) in [
            (
                "w",
                Code::KeyW,
                keyboard::Modifiers::CTRL,
                GlobalShortcut::CloseFocusedCard,
            ),
            (
                "r",
                Code::KeyR,
                keyboard::Modifiers::ALT,
                GlobalShortcut::CyclePaneWidth,
            ),
        ] {
            assert!(is_global_shortcut_chord(
                &keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                modifiers,
                &shortcuts,
                focused
            ));
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, modifiers, false),
                    &shortcuts,
                    focused
                ),
                Some(action)
            );
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, modifiers, true),
                    &shortcuts,
                    focused
                ),
                (action == GlobalShortcut::CyclePaneWidth).then_some(action),
                "width cycling repeats while closing a pane does not"
            );
        }
    }
}

#[test]
fn customized_creation_shortcuts_release_defaults_and_reserve_only_bound_chords() {
    use keyboard::key::Code;

    let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
    let mut shortcuts = KeyboardShortcuts {
        clone_pane: vec!["Alt+Shift+E".into()],
        new_channel: vec!["Ctrl+Shift+N".into()],
        new_native_thread: vec![],
        ..KeyboardShortcuts::default()
    };
    for (character, code, default_character, default_code, action) in [
        ("e", Code::KeyE, "d", Code::KeyD, GlobalShortcut::ClonePane),
        ("n", Code::KeyN, "g", Code::KeyG, GlobalShortcut::NewChannel),
    ] {
        let modifiers = if action == GlobalShortcut::ClonePane {
            keyboard::Modifiers::ALT | keyboard::Modifiers::SHIFT
        } else {
            ctrl_shift
        };
        assert_eq!(
            global_shortcut(
                &key_press(character, code, modifiers, false),
                &shortcuts,
                FocusedPane::Other,
            ),
            Some(action)
        );
        assert_eq!(
            global_shortcut(
                &key_press(character, code, modifiers, true),
                &shortcuts,
                FocusedPane::Other,
            ),
            None
        );
        assert!(is_global_shortcut_chord(
            &keyboard::Key::Character(character.into()),
            keyboard::key::Physical::Code(code),
            modifiers,
            &shortcuts,
            FocusedPane::Terminal,
        ));
        assert!(!is_global_shortcut_chord(
            &keyboard::Key::Character(default_character.into()),
            keyboard::key::Physical::Code(default_code),
            ctrl_shift,
            &shortcuts,
            FocusedPane::Terminal,
        ));
    }

    shortcuts.clone_pane.clear();
    shortcuts.new_channel.clear();
    for (character, code, modifiers) in [
        (
            "e",
            Code::KeyE,
            keyboard::Modifiers::ALT | keyboard::Modifiers::SHIFT,
        ),
        ("n", Code::KeyN, ctrl_shift),
    ] {
        assert!(!is_global_shortcut_chord(
            &keyboard::Key::Character(character.into()),
            keyboard::key::Physical::Code(code),
            modifiers,
            &shortcuts,
            FocusedPane::Terminal,
        ));
    }
}

#[test]
fn captured_typing_is_not_forwarded_to_raw_card_children() {
    assert!(!should_forward_to_raw_children(true));
    assert!(should_forward_to_raw_children(false));
}

#[test]
fn shifted_native_symbols_preserve_primary_dispatch_and_card_input() {
    use cosmic::iced::advanced::{clipboard, renderer::Headless};
    use keyboard::key::{Code, Physical};

    let modifiers = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
    // iced-winit supplies key_without_modifiers as key and logical_key as
    // modified_key. Setting both to ! hides the native number-row case.
    let press = keyboard::Event::KeyPressed {
        key: keyboard::Key::Character("1".into()),
        modified_key: keyboard::Key::Character("!".into()),
        physical_key: Physical::Code(Code::Digit1),
        location: keyboard::Location::Standard,
        modifiers,
        text: None,
        repeat: false,
    };
    for chord in ["Ctrl+Shift+1", "Ctrl+Shift+!"] {
        assert!(shortcut_event_matches(
            parse_shortcut(chord).unwrap(),
            &press
        ));
    }
    assert!(!shortcut_event_matches_unmodified(
        parse_shortcut("Ctrl+Shift+!").unwrap(),
        &press
    ));
    let mut shortcuts = KeyboardShortcuts {
        chat_model: vec!["Ctrl+Shift+!".into()],
        rename_pane: vec!["Ctrl+Shift+1".into()],
        ..KeyboardShortcuts::default()
    };
    // ChatModel appears earlier in the registry, but its new fallback must
    // not displace an action that already matched the physical digit.
    for focused in [FocusedPane::Chat, FocusedPane::Other] {
        assert_eq!(
            global_shortcut(&press, &shortcuts, focused),
            Some(GlobalShortcut::RenamePane)
        );
        assert!(is_global_shortcut_press(&press, &shortcuts, focused));
    }
    shortcuts.rename_pane.clear();
    assert_eq!(
        global_shortcut(&press, &shortcuts, FocusedPane::Chat),
        Some(GlobalShortcut::ChatModel)
    );
    assert_eq!(
        global_shortcut(&press, &shortcuts, FocusedPane::Other),
        None
    );
    assert!(!is_global_shortcut_press(
        &press,
        &shortcuts,
        FocusedPane::Other
    ));

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
    let card = CardId::new();
    let release = keyboard::Event::KeyReleased {
        key: keyboard::Key::Character("1".into()),
        modified_key: keyboard::Key::Character("!".into()),
        physical_key: Physical::Code(Code::Digit1),
        location: keyboard::Location::Standard,
        modifiers,
    };
    assert!(!shortcut_event_matches(
        parse_shortcut("Ctrl+Shift+!").unwrap(),
        &release
    ));
    for (focused, event, captured, delivered) in [
        (FocusedPane::Chat, press.clone(), true, false),
        (FocusedPane::Other, press.clone(), false, true),
        // Adding symbol press support must not introduce release capture.
        (FocusedPane::Chat, release, false, false),
    ] {
        let mut element: Element<'_, Option<CardId>> = WorkspaceSurface::new(
            Element::new(KeyProbe(card)),
            &shortcuts,
            focused,
            400.0,
            |_| None,
        )
        .into();
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(400.0, 300.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(viewport.size(), viewport.size()),
        );
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &Event::Keyboard(event),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut shell,
            &viewport,
        );
        assert_eq!(shell.is_event_captured(), captured);
        assert_eq!(messages.contains(&Some(card)), delivered);
    }
}

#[test]
fn wheel_scrolls_card_content_without_moving_the_workspace() {
    use cosmic::iced::advanced::{clipboard, renderer::Headless};
    use cosmic::iced::{Font, Pixels};

    #[derive(Debug, Clone)]
    enum Observation {
        Content(f32),
        Workspace(WorkspaceViewportEvent),
    }

    let renderer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(Renderer::new(
            Font::default(),
            Pixels(14.0),
            Some("tiny-skia"),
        ))
        .unwrap();
    let mut workspace = Workspace::default();
    workspace.resize_viewport(400.0);
    for index in 0..3 {
        workspace
            .add_card(
                WorkspaceCard::new(CardId::new(), "Scrollable", CardWidth::Fixed(320.0)).unwrap(),
                index,
                index == 0,
            )
            .unwrap();
    }
    let shortcuts = KeyboardShortcuts::default();
    let mut element = view(
        &workspace,
        &shortcuts,
        FocusedPane::Other,
        true,
        &InputTrace::default(),
        400.0,
        widget::Id::unique(),
        |_| {
            widget::scrollable(widget::Space::new().width(Length::Fill).height(2_000.0))
                .height(Length::Fill)
                .on_scroll(|viewport| Observation::Content(viewport.absolute_offset().y))
                .into()
        },
        card_widget_id,
        Events {
            viewport: Observation::Workspace,
        },
    );
    let mut tree = Tree::new(&element);
    let viewport = Rectangle::with_size(Size::new(400.0, 300.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(viewport.size(), viewport.size()),
    );
    let mut messages = Vec::new();
    let mut clipboard = clipboard::Null;
    for delta in [
        mouse::ScrollDelta::Pixels {
            x: -120.0,
            y: -120.0,
        },
        mouse::ScrollDelta::Pixels { x: -120.0, y: 0.0 },
        mouse::ScrollDelta::Lines { x: -3.0, y: 0.0 },
    ] {
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled { delta }),
            Layout::new(&node),
            mouse::Cursor::Available(Point::new(100.0, 100.0)),
            &renderer,
            &mut clipboard,
            &mut shell,
            &viewport,
        );
    }
    assert!(
        messages
            .iter()
            .any(|message| matches!(message, Observation::Content(offset) if *offset > 0.0))
    );
    assert!(!messages.iter().any(|message| matches!(
        message,
        Observation::Workspace(WorkspaceViewportEvent::Scrolled(_))
    )));
}

/// A leaf that reports every key press it is shown, as its card.
struct KeyProbe(CardId);

impl Widget<Option<CardId>, Theme, Renderer> for KeyProbe {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }
    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
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
    fn update(
        &mut self,
        _tree: &mut Tree,
        event: &Event,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Option<CardId>>,
        _viewport: &Rectangle,
    ) {
        if matches!(event, Event::Keyboard(keyboard::Event::KeyPressed { .. })) {
            shell.publish(Some(self.0));
        }
    }
}

/// A far jump hands its destination the keyboard at once, but the pane
/// only enters the viewport when the camera gets there. The strip showed
/// keyboard events to panes inside the viewport alone, so what was typed
/// during the journey reached nobody.
#[test]
fn keys_typed_while_the_camera_travels_reach_the_pane_it_is_travelling_to() {
    use cosmic::iced::advanced::{clipboard, renderer::Headless};
    use cosmic::iced::{Font, Pixels};

    let renderer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(Renderer::new(
            Font::default(),
            Pixels(14.0),
            Some("tiny-skia"),
        ))
        .unwrap();
    let mut workspace = Workspace::default();
    workspace.resize_viewport(400.0);
    for index in 0..8 {
        workspace
            .add_card(
                WorkspaceCard::new(CardId::new(), "Pane", CardWidth::Fixed(480.0)).unwrap(),
                index,
                index == 0,
            )
            .unwrap();
    }
    let ids: Vec<_> = workspace.order().collect();
    workspace.focus(ids[6]).unwrap();
    assert_eq!(workspace.camera().offset(), 0.0);

    let shortcuts = KeyboardShortcuts::default();
    let mut element = view(
        &workspace,
        &shortcuts,
        FocusedPane::Other,
        false,
        &InputTrace::default(),
        400.0,
        widget::Id::unique(),
        |context| Element::new(KeyProbe(context.id)),
        card_widget_id,
        // Whatever the strip reports about itself is not a key press.
        Events { viewport: |_| None },
    );
    let mut tree = Tree::new(&element);
    let viewport = Rectangle::with_size(Size::new(400.0, 300.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(viewport.size(), viewport.size()),
    );
    let mut messages = Vec::new();
    let mut clipboard = clipboard::Null;
    let mut shell = Shell::new(&mut messages);
    element.as_widget_mut().update(
        &mut tree,
        &Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("a".into()),
            modified_key: keyboard::Key::Character("a".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyA),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("a".into()),
            repeat: false,
        }),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut clipboard,
        &mut shell,
        &viewport,
    );
    let mut reached: Vec<_> = messages
        .into_iter()
        .flatten()
        .map(|id| ids.iter().position(|candidate| *candidate == id).unwrap())
        .collect();
    reached.sort_unstable();
    // The pane on screen sees the key as before, and so does the
    // destination. The pane parked in the overscan beside it does not.
    assert_eq!(
        workspace
            .projection(400.0)
            .mounted_ids()
            .collect::<Vec<_>>(),
        [ids[0], ids[1], ids[6]]
    );
    assert_eq!(reached, [0, 6]);
}

/// The shell re-runs one `RedrawRequested` through update and layout while
/// any pass publishes a message, rebuilding the view in between, and warns
/// once a frame has cost three of those passes. The strip surface reports
/// its own measured width from that pass, so the report has to be a fixed
/// point: once the owner holds the reported width, a further pass over the
/// same frame must publish nothing - including a pass whose height changed,
/// and a pass whose strip was remounted with fresh widget state, neither of
/// which changes the width the owner reads.
#[test]
fn native_scroll_samples_distinguish_redraw_feedback_from_user_wheels() {
    use cosmic::iced::advanced::{clipboard, renderer::Headless};
    use cosmic::iced::{Font, Pixels};
    use std::time::Instant;

    let renderer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(Renderer::new(
            Font::default(),
            Pixels(14.0),
            Some("tiny-skia"),
        ))
        .unwrap();
    let shortcuts = KeyboardShortcuts::default();
    let scroll_input = Rc::new(Cell::new(false));
    let callback_input = Rc::clone(&scroll_input);
    // Exercise the native scrollable directly: VirtualStrip deliberately
    // consumes wheels over pane content, which has its own regression.
    let scrollable = cosmic::widget::scrollable::horizontal(
        widget::Space::new().width(1_280.0).height(Length::Fill),
    )
    .direction(widget::scrollable::Direction::Horizontal(
        widget::scrollable::Scrollbar::hidden(),
    ))
    .width(Length::Fill)
    .height(Length::Fill)
    .on_scroll(move |viewport| {
        WorkspaceViewportEvent::Scrolled(ViewportSample {
            offset: viewport.absolute_offset().x,
            width: viewport.bounds().width,
            user_input: callback_input.get(),
        })
    });
    let mut element: Element<'_, WorkspaceViewportEvent> = WorkspaceSurface::new(
        scrollable,
        &shortcuts,
        FocusedPane::Other,
        400.0,
        WorkspaceViewportEvent::Resized,
    )
    .with_scroll_input(scroll_input)
    .into();
    let mut tree = Tree::new(&element);
    let viewport = Rectangle::with_size(Size::new(400.0, 300.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(viewport.size(), viewport.size()),
    );
    let mut messages = Vec::new();
    for event in [
        Event::Window(window::Event::RedrawRequested(Instant::now())),
        Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Pixels { x: -120.0, y: 0.0 },
        }),
    ] {
        element.as_widget_mut().update(
            &mut tree,
            &event,
            Layout::new(&node),
            mouse::Cursor::Available(Point::new(100.0, 100.0)),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
    }
    let samples: Vec<_> = messages
        .into_iter()
        .filter_map(|event| match event {
            WorkspaceViewportEvent::Scrolled(sample) => Some(sample),
            _ => None,
        })
        .collect();
    assert!(samples.iter().any(|sample| !sample.user_input));
    assert!(
        samples
            .iter()
            .any(|sample| sample.user_input && sample.offset > 0.0)
    );
}

#[test]
fn a_reported_viewport_width_is_not_reported_twice_in_one_frame() {
    use cosmic::iced::advanced::{clipboard, renderer::Headless};
    use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
    use cosmic::iced::{Font, Pixels};
    use std::time::Instant;

    let mut renderer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(Renderer::new(
            Font::default(),
            Pixels(14.0),
            Some("tiny-skia"),
        ))
        .unwrap();
    let mut workspace = Workspace::default();
    for index in 0..4 {
        workspace
            .add_card(
                WorkspaceCard::new(CardId::new(), "Card", CardWidth::Fraction(0.5)).unwrap(),
                index,
                index == 0,
            )
            .unwrap();
    }
    let shortcuts = KeyboardShortcuts::default();
    let scroll_id = widget::Id::unique();
    let redraw = Event::Window(window::Event::RedrawRequested(Instant::now()));

    // One pass of the shell's loop: build the view the owner would build
    // for the width it currently holds, deliver the frame to it, and
    // report back what the surface asked of the owner.
    let mut pass = |workspace: &Workspace, cache: Cache, height: f32| {
        let mut messages: Vec<WorkspaceViewportEvent> = Vec::new();
        let mut interface = UserInterface::build(
            view(
                workspace,
                &shortcuts,
                FocusedPane::Other,
                false,
                &InputTrace::default(),
                workspace.viewport_width(),
                scroll_id.clone(),
                |_| {
                    widget::Space::new()
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into()
                },
                card_widget_id,
                Events {
                    viewport: |event| event,
                },
            ),
            Size::new(900.0, height),
            cache,
            &mut renderer,
        );
        let _ = interface.update(
            std::slice::from_ref(&redraw),
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        let widths: Vec<f32> = messages
            .iter()
            .filter_map(|message| match message {
                WorkspaceViewportEvent::Resized(width) => Some(*width),
                _ => None,
            })
            .collect();
        (interface.into_cache(), widths)
    };

    let (cache, reported) = pass(&workspace, Cache::new(), 600.0);
    assert_eq!(
        reported,
        vec![900.0],
        "a surface whose measured width disagrees with the owner's has to say so once",
    );
    for width in reported {
        let _ = workspace.resize_viewport(width);
    }

    let (cache, settled) = pass(&workspace, cache, 600.0);
    assert!(
        settled.is_empty(),
        "the owner already holds this width, so a second pass over the same frame must \
             publish nothing; it reported {settled:?}",
    );

    let (_, after_height_change) = pass(&workspace, cache, 400.0);
    assert!(
        after_height_change.is_empty(),
        "a shorter surface is the same viewport width, so reporting it again buys the owner \
             nothing and costs the frame a whole extra pass; it reported {after_height_change:?}",
    );

    let (_, after_remount) = pass(&workspace, Cache::new(), 600.0);
    assert!(
        after_remount.is_empty(),
        "a strip remounted with fresh widget state still measures the width the owner holds; \
             it reported {after_remount:?}",
    );
}
struct AccessibleProbe {
    node: u64,
    expected_virtual_offset: Vector,
}

impl Widget<(), Theme, Renderer> for AccessibleProbe {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<u64>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(self.node)
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
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

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        state: &Tree,
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        use iced_accessibility::accesskit::{Node, Role};

        assert_eq!(layout.virtual_offset(), self.expected_virtual_offset);
        iced_accessibility::A11yTree::leaf(
            Node::new(Role::Button),
            *state.state.downcast_ref::<u64>(),
        )
    }
}

#[test]
fn scrolling_restores_card_state_but_closed_cards_do_not_keep_it() {
    use iced_accessibility::A11yId;

    let first = CardId::new();
    let second = CardId::new();
    let retained = Arc::new(HashMap::from([(first, 0), (second, 1)]));
    let make_strip = |retained, id, node| VirtualStrip {
        total_width: 200.0,
        viewport_width: 200.0,
        retained,
        focused: Some(id),
        cards: vec![VirtualCard {
            id,
            x: 0.0,
            width: 100.0,
            content_width: 100.0,
            on_screen: true,
            content: Element::new(AccessibleProbe {
                node,
                expected_virtual_offset: Vector::ZERO,
            }),
        }],
    };
    let mut strip: Element<'_, ()> = Element::new(make_strip(Arc::clone(&retained), first, 1));
    let mut tree = Tree::new(&strip);
    let layout = layout::Node::with_children(
        Size::new(200.0, 80.0),
        vec![layout::Node::new(Size::new(100.0, 80.0))],
    );
    for (id, initial, expected) in [
        (first, 2, 1), // The same band diffs in place.
        (second, 3, 3),
        (first, 4, 1), // Remount restores the first card's state.
    ] {
        strip = Element::new(make_strip(Arc::clone(&retained), id, initial));
        tree.diff(&mut strip);
        assert!(
            strip
                .as_widget()
                .a11y_nodes(Layout::new(&layout), &tree, mouse::Cursor::Unavailable,)
                .contains(&A11yId::from(expected as u64))
        );
    }

    // Close the first card while only the second is mounted. Reusing its
    // identity afterwards must create fresh state, not revive a stale tree.
    strip = Element::new(make_strip(Arc::clone(&retained), second, 5));
    tree.diff(&mut strip);
    strip = Element::new(make_strip(
        Arc::new(HashMap::from([(second, 0)])),
        second,
        6,
    ));
    tree.diff(&mut strip);
    strip = Element::new(make_strip(retained, first, 7));
    tree.diff(&mut strip);
    assert!(
        strip
            .as_widget()
            .a11y_nodes(Layout::new(&layout), &tree, mouse::Cursor::Unavailable,)
            .contains(&A11yId::from(7_u64))
    );
}

#[test]
fn workspace_wrappers_preserve_accessible_card_descendants() {
    use iced_accessibility::A11yId;

    let cards = [CardId::new(), CardId::new()];
    let strip = VirtualStrip {
        total_width: 200.0,
        viewport_width: 200.0,
        focused: Some(cards[0]),
        retained: Arc::new(
            cards
                .into_iter()
                .enumerate()
                .map(|(index, id)| (id, index))
                .collect(),
        ),
        cards: cards
            .into_iter()
            .enumerate()
            .map(|(index, id)| VirtualCard {
                id,
                x: index as f32 * 100.0,
                width: 100.0,
                content_width: 100.0,
                on_screen: true,
                content: CardSurface::new(Element::new(AccessibleProbe {
                    node: index as u64 + 1,
                    expected_virtual_offset: Vector::ZERO,
                }))
                .into(),
            })
            .collect(),
    };
    let shortcuts = KeyboardShortcuts::default();
    let surface: Element<'_, ()> = Element::new(WorkspaceSurface::new(
        strip,
        &shortcuts,
        FocusedPane::Other,
        0.0,
        |_| (),
    ));
    let tree = Tree::new(&surface);
    let card_node = || {
        layout::Node::with_children(
            Size::new(100.0, 80.0),
            vec![layout::Node::new(Size::new(100.0, 80.0))],
        )
    };
    let strip_node = layout::Node::with_children(
        Size::new(200.0, 80.0),
        vec![card_node(), card_node().move_to(Point::new(100.0, 0.0))],
    );
    let surface_node = layout::Node::with_children(Size::new(200.0, 80.0), vec![strip_node]);
    let accessible = surface.as_widget().a11y_nodes(
        Layout::new(&surface_node).with_virtual_offset(Vector::new(32.0, 0.0)),
        &tree,
        mouse::Cursor::Unavailable,
    );

    assert!(accessible.contains(&A11yId::from(1_u64)));
    assert!(accessible.contains(&A11yId::from(2_u64)));
    assert_eq!(accessible.root().len(), 2);
}

#[test]
fn opening_card_contents_are_built_before_their_space_is_ready() {
    let mut workspace = Workspace::default();
    workspace.resize_viewport(900.0);
    let first = CardId::new();
    let opening = CardId::new();
    workspace
        .add_card(
            WorkspaceCard::new(first, "First", CardWidth::Fixed(300.0)).unwrap(),
            0,
            true,
        )
        .unwrap();
    workspace
        .add_card(
            WorkspaceCard::new(opening, "Opening", CardWidth::Fixed(300.0)).unwrap(),
            1,
            true,
        )
        .unwrap();
    let built = std::cell::RefCell::new(Vec::new());
    let shortcuts = KeyboardShortcuts::default();
    let input = InputTrace::default();
    let render = |workspace: &Workspace| {
        let element: Element<'_, ()> = view(
            workspace,
            &shortcuts,
            FocusedPane::Other,
            false,
            &input,
            900.0,
            widget::Id::unique(),
            |context| {
                built.borrow_mut().push(context.id);
                widget::Space::new().into()
            },
            card_widget_id,
            Events { viewport: |_| () },
        );
        drop(element);
        built.take()
    };
    assert_eq!(render(&workspace), [first, opening]);
    workspace.tick(Duration::from_millis(220));
    assert_eq!(render(&workspace), [first, opening]);
}

#[test]
fn composed_workspace_view_exposes_only_visible_card_bodies_to_a_screen_reader() {
    use cosmic::iced::advanced::renderer::Headless;
    use iced_accessibility::A11yId;

    const VIEWPORT_WIDTH: f32 = 700.0;
    const HEIGHT: f32 = 480.0;

    let mut workspace = Workspace::default();
    workspace.resize_viewport(VIEWPORT_WIDTH);
    let cards: Vec<CardId> = (0..8).map(|_| CardId::new()).collect();
    for (index, id) in cards.iter().copied().enumerate() {
        workspace
            .add_card(
                WorkspaceCard::new(id, format!("Card {index}"), CardWidth::Fixed(320.0)).unwrap(),
                index,
                index == 0,
            )
            .unwrap();
    }
    let projection = workspace.projection(VIEWPORT_WIDTH);
    let mounted: Vec<CardId> = projection.mounted_ids().collect();
    assert!(mounted.len() < cards.len());

    let probe_id = |id: CardId| {
        cards
            .iter()
            .position(|card| *card == id)
            .expect("probe for a card the workspace holds") as u64
            + 1
    };
    let shortcuts = KeyboardShortcuts::default();
    let mut composed: Element<'_, ()> = view(
        &workspace,
        &shortcuts,
        FocusedPane::Other,
        true,
        &InputTrace::default(),
        VIEWPORT_WIDTH,
        widget::Id::unique(),
        |context| {
            Element::new(AccessibleProbe {
                node: probe_id(context.id),
                expected_virtual_offset: Vector::ZERO,
            })
        },
        card_widget_id,
        Events { viewport: |_| () },
    );
    let mut tree = Tree::new(&composed);
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
    let surface_node = composed.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(VIEWPORT_WIDTH, HEIGHT)),
    );
    let accessible = composed.as_widget().a11y_nodes(
        Layout::new(&surface_node),
        &tree,
        mouse::Cursor::Unavailable,
    );

    assert!(
        !accessible.root().is_empty(),
        "the workspace must reach a screen reader through every wrapper the view builds"
    );
    let visible = |id: CardId| {
        projection.mounted.iter().any(|card| {
            card.id == id
                && card.start - projection.offset < VIEWPORT_WIDTH
                && card.end() - projection.offset > 0.0
        })
    };
    for id in &mounted {
        assert_eq!(
            accessible.contains(&A11yId::from(probe_id(*id))),
            visible(*id),
            "only the visible body of mounted card {id} belongs in the accessibility tree"
        );
    }
    assert_eq!(
        cards
            .iter()
            .filter(|id| accessible.contains(&A11yId::from(probe_id(**id))))
            .count(),
        mounted.iter().filter(|id| visible(**id)).count(),
        "retaining a body must not make it visible to a screen reader"
    );
}

struct A11yWorkspaceFixture {
    element: Element<'static, u8>,
    tree: Tree,
    node: layout::Node,
    renderer: Renderer,
    scroll_id: widget::Id,
    buttons: Vec<widget::Id>,
    late_buttons: Vec<widget::Id>,
    editors: Vec<widget::Id>,
}

impl A11yWorkspaceFixture {
    fn new(slots: &[(f32, f32, f32)], total_width: f32) -> Self {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let ids: Vec<_> = slots.iter().map(|_| CardId::new()).collect();
        let buttons: Vec<_> = slots.iter().map(|_| widget::Id::unique()).collect();
        let late_buttons: Vec<_> = slots.iter().map(|_| widget::Id::unique()).collect();
        let editors: Vec<_> = slots.iter().map(|_| widget::Id::unique()).collect();
        let cards = slots
            .iter()
            .enumerate()
            .map(|(index, &(x, width, content_width))| {
                let content = widget::Column::new()
                    .push(
                        widget::Row::new()
                            .push(
                                cosmic::widget::button::text("Accounts")
                                    .name(format!("action-{index}"))
                                    .id(buttons[index].clone())
                                    .width(100.0)
                                    .on_press(index as u8),
                            )
                            .push(widget::Space::new().width(100.0))
                            .push(
                                cosmic::widget::button::text("Add account")
                                    .name(format!("late-action-{index}"))
                                    .id(late_buttons[index].clone())
                                    .width(100.0)
                                    .on_press(10 + index as u8),
                            ),
                    )
                    .push(cosmic::widget::text(format!("native-body-{index}")).width(Length::Fill))
                    .push(
                        crate::ui_controls::field::TextInput::new(
                            format!("draft-{index}"),
                            format!("retained-{index}"),
                        )
                        .id(editors[index].clone())
                        .on_input(move |_| 20 + index as u8),
                    )
                    .width(Length::Fill);
                VirtualCard {
                    id: ids[index],
                    x,
                    width,
                    content_width,
                    // Include a travelling focus destination, not merely
                    // the projection's current geometric visible band.
                    on_screen: true,
                    content: CardSurface::new(content).into(),
                }
            })
            .collect();
        let strip = VirtualStrip {
            total_width,
            viewport_width: 320.0,
            retained: Arc::new(ids.iter().enumerate().map(|(i, id)| (*id, i)).collect()),
            focused: ids.last().copied(),
            cards,
        };
        let scroll_id = widget::Id::unique();
        let mut element: Element<'static, u8> = widget::scrollable(strip)
            .direction(widget::scrollable::Direction::Horizontal(
                widget::scrollable::Scrollbar::hidden(),
            ))
            .id(scroll_id.clone())
            .width(320.0)
            .height(180.0)
            .into();
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(320.0, 180.0)),
            )
            .move_to(Point::new(260.0, 44.0));
        Self {
            element,
            tree,
            node,
            renderer,
            scroll_id,
            buttons,
            late_buttons,
            editors,
        }
    }

    fn scroll(&mut self, x: f32) {
        use cosmic::iced::advanced::widget::operation;
        self.element.as_widget_mut().operate(
            &mut self.tree,
            Layout::new(&self.node),
            &self.renderer,
            &mut operation::scrollable::scroll_to::<()>(
                self.scroll_id.clone(),
                operation::scrollable::AbsoluteOffset {
                    x: Some(x),
                    y: None,
                },
            ),
        );
    }

    fn accessible(&self) -> iced_accessibility::A11yTree {
        self.element.as_widget().a11y_nodes(
            Layout::new(&self.node),
            &self.tree,
            mouse::Cursor::Unavailable,
        )
    }

    fn dispatch(
        &mut self,
        id: widget::Id,
        action: iced_accessibility::accesskit::Action,
    ) -> Vec<u8> {
        use iced_accessibility::{
            A11yId,
            accesskit::{ActionRequest, TreeId},
        };
        let mut messages = Vec::new();
        self.element.as_widget_mut().update(
            &mut self.tree,
            &Event::A11y(
                id.clone(),
                ActionRequest {
                    action,
                    target_tree: TreeId::ROOT,
                    target_node: A11yId::from(id).into(),
                    data: None,
                },
            ),
            Layout::new(&self.node),
            mouse::Cursor::Unavailable,
            &self.renderer,
            &mut cosmic::iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &self.node.bounds(),
        );
        messages
    }
}

fn a11y_node<'a>(
    tree: &'a iced_accessibility::A11yTree,
    id: &widget::Id,
) -> Option<&'a iced_accessibility::A11yNode> {
    let id = iced_accessibility::A11yId::from(id.clone());
    tree.root()
        .iter()
        .chain(tree.children())
        .find(|node| iced_accessibility::IdEq::eq(node.id(), &id))
}

#[test]
fn workspace_a11y_camera_moves_real_button_text_and_editable_bounds_once() {
    let mut fixture = A11yWorkspaceFixture::new(
        &[
            (0.0, 320.0, 320.0),
            (479.0, 320.0, 320.0),
            (1437.0, 320.0, 320.0),
        ],
        2000.0,
    );
    for (index, camera) in [(1, 479.0), (2, 1437.0)] {
        fixture.scroll(camera);
        let tree = fixture.accessible();
        for id in [&fixture.buttons[index], &fixture.editors[index]] {
            let node = a11y_node(&tree, id).unwrap().node();
            assert!(!node.is_hidden());
            assert_eq!(node.bounds().unwrap().x0, 260.0);
        }
        let label = format!("native-body-{index}");
        let native_text = tree
            .root()
            .iter()
            .chain(tree.children())
            .find(|node| node.node().label() == Some(label.as_str()))
            .unwrap()
            .node();
        assert_eq!(native_text.bounds().unwrap().x0, 260.0);
        assert!(!native_text.is_hidden());
    }
}

#[test]
fn workspace_a11y_culls_travelling_bodies_and_preserves_focus_on_return() {
    use cosmic::iced::advanced::widget::operation;
    use iced_accessibility::accesskit::Action;
    let mut fixture = A11yWorkspaceFixture::new(
        &[
            (0.0, 320.0, 320.0),
            (479.0, 320.0, 320.0),
            (1437.0, 320.0, 320.0),
        ],
        2000.0,
    );
    fixture.scroll(479.0);
    let before = fixture.accessible();
    assert!(a11y_node(&before, &fixture.buttons[0]).is_none());
    assert!(a11y_node(&before, &fixture.buttons[2]).is_none());
    let original_editor = a11y_node(&before, &fixture.editors[1])
        .unwrap()
        .id()
        .clone();
    fixture.element.as_widget_mut().operate(
        &mut fixture.tree,
        Layout::new(&fixture.node),
        &fixture.renderer,
        &mut operation::focusable::focus::<()>(fixture.editors[1].clone()),
    );
    fixture.scroll(1437.0);
    assert!(a11y_node(&fixture.accessible(), &fixture.editors[1]).is_none());
    assert!(
        fixture
            .dispatch(fixture.buttons[1].clone(), Action::Click)
            .is_empty()
    );
    assert_eq!(
        fixture.dispatch(fixture.buttons[2].clone(), Action::Click),
        [2]
    );
    fixture.scroll(479.0);
    let returned = fixture.accessible();
    let editor = a11y_node(&returned, &fixture.editors[1]).unwrap();
    assert!(iced_accessibility::IdEq::eq(editor.id(), &original_editor));
    assert_eq!(editor.node().value(), Some("retained-1"));
    let mut focused = FocusedWidgets::default();
    fixture.element.as_widget_mut().operate(
        &mut fixture.tree,
        Layout::new(&fixture.node),
        &fixture.renderer,
        &mut focused,
    );
    assert_eq!(focused.0, [fixture.editors[1].clone()]);
}

#[test]
fn workspace_a11y_opening_slot_clips_controls_and_rejects_stale_actions() {
    use iced_accessibility::accesskit::Action;
    let mut fixture =
        A11yWorkspaceFixture::new(&[(0.0, 120.0, 320.0), (120.0, 320.0, 320.0)], 600.0);
    fixture.scroll(80.0);
    let tree = fixture.accessible();
    let partial = a11y_node(&tree, &fixture.buttons[0]).unwrap().node();
    assert!(!partial.is_hidden());
    let bounds = partial.bounds().unwrap();
    assert_eq!((bounds.x0, bounds.x1), (260.0, 280.0));
    assert!(
        a11y_node(&tree, &fixture.late_buttons[0])
            .unwrap()
            .node()
            .is_hidden()
    );
    let editor = a11y_node(&tree, &fixture.editors[0])
        .unwrap()
        .node()
        .bounds()
        .unwrap();
    assert_eq!((editor.x0, editor.x1), (260.0, 300.0));
    assert!(
        fixture
            .dispatch(fixture.late_buttons[0].clone(), Action::Click)
            .is_empty()
    );
    assert_eq!(
        fixture.dispatch(fixture.buttons[0].clone(), Action::Click),
        [0]
    );
    assert_eq!(
        fixture.dispatch(fixture.buttons[1].clone(), Action::Click),
        [1]
    );
}

#[test]
fn workspace_a11y_outer_camera_preserves_nested_editable_scroll_translation() {
    use cosmic::iced::advanced::widget::operation;
    let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let editor = widget::Id::unique();
    let below = widget::Id::unique();
    let inner_id = widget::Id::unique();
    let outer_id = widget::Id::unique();
    let card_id = CardId::new();
    let inner = widget::scrollable(
        widget::Column::new()
            .push(widget::Space::new().height(80.0))
            .push(
                crate::ui_controls::field::TextInput::new("nested draft", "kept")
                    .id(editor.clone())
                    .on_input(|_| 1_u8),
            )
            .push(widget::Space::new().height(400.0))
            .push(
                crate::ui_controls::field::TextInput::new("below the fold", "")
                    .id(below.clone())
                    .on_input(|_| 2_u8),
            ),
    )
    .id(inner_id.clone())
    .width(Length::Fill)
    .height(Length::Fill);
    let strip = VirtualStrip {
        total_width: 1000.0,
        viewport_width: 320.0,
        retained: Arc::new(HashMap::from([(card_id, 0)])),
        focused: Some(card_id),
        cards: vec![VirtualCard {
            id: card_id,
            x: 479.0,
            width: 320.0,
            content_width: 320.0,
            on_screen: true,
            content: CardSurface::new(inner).into(),
        }],
    };
    let mut element: Element<'_, u8> = widget::scrollable(strip)
        .direction(widget::scrollable::Direction::Horizontal(
            widget::scrollable::Scrollbar::hidden(),
        ))
        .id(outer_id.clone())
        .width(320.0)
        .height(180.0)
        .into();
    let mut tree = Tree::new(&element);
    let node = element
        .as_widget_mut()
        .layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(320.0, 180.0)),
        )
        .move_to(Point::new(260.0, 44.0));
    for (id, x, y) in [(outer_id, Some(479.0), None), (inner_id, None, Some(40.0))] {
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::scrollable::scroll_to::<()>(
                id,
                operation::scrollable::AbsoluteOffset { x, y },
            ),
        );
    }
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    let field = a11y_node(&accessible, &editor).unwrap().node();
    assert!(!field.is_hidden());
    let bounds = field.bounds().unwrap();
    assert_eq!((bounds.x0, bounds.y0), (260.0, 84.0));
    assert_eq!(field.value(), Some("kept"));
    // The camera hides only what it moves out of view sideways; a field
    // the pane's own scrollable holds below the fold stays reachable.
    let below = a11y_node(&accessible, &below).unwrap().node();
    assert!(!below.is_hidden());
    assert_eq!(below.bounds().unwrap().x0, 260.0);
    assert!(below.bounds().unwrap().y0 > 224.0);
}

#[test]
fn workspace_a11y_reports_a_pane_popover_where_it_is_painted() {
    use cosmic::iced::advanced::widget::operation;
    let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let item = widget::Id::unique();
    let outer_id = widget::Id::unique();
    let card_id = CardId::new();
    let popover = cosmic::widget::popover(
        cosmic::widget::button::text("Add account")
            .name("Add account")
            .on_press(1_u8),
    )
    .popup(
        cosmic::widget::button::text("Claude")
            .name("Claude")
            .id(item.clone())
            .width(80.0)
            .on_press(2_u8),
    )
    .position(cosmic::widget::popover::Position::Bottom);
    let strip = VirtualStrip {
        total_width: 1000.0,
        viewport_width: 320.0,
        retained: Arc::new(HashMap::from([(card_id, 0)])),
        focused: Some(card_id),
        cards: vec![VirtualCard {
            id: card_id,
            x: 479.0,
            width: 320.0,
            content_width: 320.0,
            on_screen: true,
            content: CardSurface::new(popover).into(),
        }],
    };
    let mut element: Element<'_, u8> = widget::scrollable(strip)
        .direction(widget::scrollable::Direction::Horizontal(
            widget::scrollable::Scrollbar::hidden(),
        ))
        .id(outer_id.clone())
        .width(320.0)
        .height(180.0)
        .into();
    let mut tree = Tree::new(&element);
    let node = element
        .as_widget_mut()
        .layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(320.0, 180.0)),
        )
        .move_to(Point::new(260.0, 44.0));
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::scrollable::scroll_to::<()>(
            outer_id,
            operation::scrollable::AbsoluteOffset {
                x: Some(479.0),
                y: None,
            },
        ),
    );
    let painted = {
        let viewport = node.bounds();
        let mut overlay = element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &viewport,
                Vector::ZERO,
            )
            .expect("the open popover draws its popup over the pane");
        // The strip groups its cards' overlays; the first is the popup.
        overlay
            .as_overlay_mut()
            .layout(&renderer, Size::new(1200.0, 750.0))
            .children()[0]
            .bounds()
    };
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    let item = a11y_node(&accessible, &item).unwrap().node();
    assert!(!item.is_hidden());
    let bounds = item.bounds().unwrap();
    assert_eq!(
        (bounds.x0, bounds.y0),
        (f64::from(painted.x), f64::from(painted.y))
    );
    assert!((260.0..580.0).contains(&bounds.x0));
}

#[test]
fn workspace_a11y_keeps_a_popup_reachable_where_it_spills_over_a_neighbour() {
    let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
    let item = widget::Id::unique();
    let neighbour = CardId::new();
    let owner = CardId::new();
    // A popup far wider than its pane, opened from the right-hand pane,
    // draws across the left-hand one, as menus near a pane's edge do.
    let popover = cosmic::widget::popover(
        cosmic::widget::button::text("Model")
            .name("Model")
            .on_press(1_u8),
    )
    .popup(
        cosmic::widget::button::text("A long menu item")
            .name("A long menu item")
            .id(item.clone())
            .width(300.0)
            .on_press(2_u8),
    )
    .position(cosmic::widget::popover::Position::Bottom);
    let card = |id, x, content: Element<'static, u8>| VirtualCard {
        id,
        x,
        width: 160.0,
        content_width: 160.0,
        on_screen: true,
        content: CardSurface::new(content).into(),
    };
    let strip = VirtualStrip {
        total_width: 320.0,
        viewport_width: 320.0,
        retained: Arc::new(HashMap::from([(neighbour, 0), (owner, 1)])),
        focused: Some(owner),
        cards: vec![
            card(neighbour, 0.0, cosmic::widget::text("Neighbour").into()),
            card(owner, 160.0, popover.into()),
        ],
    };
    let mut element: Element<'_, u8> = widget::scrollable(strip)
        .direction(widget::scrollable::Direction::Horizontal(
            widget::scrollable::Scrollbar::hidden(),
        ))
        .width(320.0)
        .height(180.0)
        .into();
    let mut tree = Tree::new(&element);
    let node = element
        .as_widget_mut()
        .layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(320.0, 180.0)),
        )
        .move_to(Point::new(260.0, 44.0));
    let painted = {
        let viewport = node.bounds();
        let mut overlay = element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &viewport,
                Vector::ZERO,
            )
            .expect("the open popover draws its popup over the strip");
        overlay
            .as_overlay_mut()
            .layout(&renderer, Size::new(1200.0, 750.0))
            .children()[0]
            .bounds()
    };
    let owner_left = 260.0 + 160.0;
    assert!(
        painted.x < owner_left,
        "the fixture's popup must reach across the neighbouring pane: {painted:?}"
    );
    let accessible =
        element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
    let item = a11y_node(&accessible, &item).unwrap().node();
    assert!(!item.is_hidden());
    let bounds = item.bounds().unwrap();
    assert_eq!(bounds.x0, f64::from(painted.x));
}

#[test]
fn only_configured_global_chords_are_reserved_from_card_children() {
    use keyboard::key::{Code, Physical};

    let shortcuts = KeyboardShortcuts::default();
    let reserved = |character, code, modifiers| {
        is_global_shortcut_chord(
            &keyboard::Key::Character(character),
            Physical::Code(code),
            modifiers,
            &shortcuts,
            FocusedPane::Other,
        )
    };
    let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
    assert!(reserved("t".into(), Code::KeyT, ctrl_shift));
    assert!(reserved("t".into(), Code::KeyT, keyboard::Modifiers::CTRL));
    assert!(!reserved("q".into(), Code::KeyQ, keyboard::Modifiers::CTRL));
    assert!(!reserved("q".into(), Code::KeyQ, keyboard::Modifiers::ALT));
    assert!(!reserved("a".into(), Code::KeyA, ctrl_shift));
    assert!(reserved("w".into(), Code::KeyW, keyboard::Modifiers::CTRL));
    assert!(!reserved("w".into(), Code::KeyW, ctrl_shift));
    assert!(!reserved("w".into(), Code::KeyW, keyboard::Modifiers::ALT));
    assert!(reserved("r".into(), Code::KeyR, keyboard::Modifiers::ALT));
    assert!(!reserved("r".into(), Code::KeyR, ctrl_shift));
    assert!(!reserved(
        "t".into(),
        Code::KeyT,
        keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT,
    ));
    assert!(!reserved(
        "t".into(),
        Code::KeyT,
        ctrl_shift | keyboard::Modifiers::ALT,
    ));

    let customized = KeyboardShortcuts {
        new_native_thread: vec!["Ctrl+Shift+N".into()],
        ..shortcuts
    };
    assert!(!is_global_shortcut_chord(
        &keyboard::Key::Character("t".into()),
        Physical::Code(Code::KeyT),
        keyboard::Modifiers::CTRL,
        &customized,
        FocusedPane::Other,
    ));
}

/// The review chords are arrow chords a terminal program may well use,
/// so they are withheld from a card's process only while a pull request
/// pane, which is the only thing that acts on them, has focus.
#[test]
fn review_chords_are_reserved_only_while_a_review_pane_has_focus() {
    use keyboard::key::{Code, Physical};

    let shortcuts = KeyboardShortcuts::default();
    let reserved = |named, code, modifiers, focused| {
        is_global_shortcut_chord(
            &keyboard::Key::Named(named),
            Physical::Code(code),
            modifiers,
            &shortcuts,
            focused,
        )
    };
    for (named, code) in [
        (keyboard::key::Named::ArrowUp, Code::ArrowUp),
        (keyboard::key::Named::ArrowDown, Code::ArrowDown),
    ] {
        for focused in [FocusedPane::Terminal, FocusedPane::Other] {
            assert!(
                !reserved(named, code, keyboard::Modifiers::ALT, focused),
                "{named:?} reached no {focused:?} pane"
            );
        }
        assert!(
            reserved(named, code, keyboard::Modifiers::ALT, FocusedPane::Review),
            "{named:?} bypassed the review pane"
        );
    }
    // Workspace chords stay reserved either way.
    for focused in [
        FocusedPane::Terminal,
        FocusedPane::Review,
        FocusedPane::Other,
    ] {
        assert!(reserved(
            keyboard::key::Named::PageUp,
            Code::PageUp,
            keyboard::Modifiers::CTRL,
            focused,
        ));
    }
    let step = named_key_press(
        keyboard::key::Named::ArrowDown,
        Code::ArrowDown,
        keyboard::Modifiers::ALT,
        true,
    );
    assert_eq!(
        global_shortcut(&step, &shortcuts, FocusedPane::Review),
        Some(GlobalShortcut::ReviewNextFile),
        "stepping files repeats while held"
    );
    assert_eq!(
        global_shortcut(&step, &shortcuts, FocusedPane::Terminal),
        None,
        "a review chord acts on no other pane"
    );
}

#[test]
fn held_page_navigation_crosses_four_cards_and_stops_at_edges() {
    use keyboard::key::{Code, Named};

    let mut workspace = Workspace::default();
    let cards = [CardId::new(), CardId::new(), CardId::new(), CardId::new()];
    for (index, id) in cards.into_iter().enumerate() {
        workspace
            .add_card(
                WorkspaceCard::new(id, format!("Card {index}"), CardWidth::Fixed(320.0)).unwrap(),
                index,
                false,
            )
            .unwrap();
    }
    workspace.focus(cards[0]).unwrap();

    let shortcuts = KeyboardShortcuts::default();
    for repeat in [false, true, true, true] {
        let shortcut = global_shortcut(
            &named_key_press(
                Named::PageDown,
                Code::PageDown,
                keyboard::Modifiers::CTRL,
                repeat,
            ),
            &shortcuts,
            FocusedPane::Terminal,
        );
        let Some(GlobalShortcut::FocusAdjacent(direction)) = shortcut else {
            panic!("Ctrl+PageDown must remain a navigation shortcut on repeated key presses");
        };
        workspace.focus_adjacent(direction).unwrap();
    }
    assert_eq!(workspace.focused(), Some(cards[3]));

    for repeat in [false, true, true, true] {
        let shortcut = global_shortcut(
            &named_key_press(
                Named::PageUp,
                Code::PageUp,
                keyboard::Modifiers::CTRL,
                repeat,
            ),
            &shortcuts,
            FocusedPane::Terminal,
        );
        let Some(GlobalShortcut::FocusAdjacent(direction)) = shortcut else {
            panic!("Ctrl+PageUp must remain a navigation shortcut on repeated key presses");
        };
        workspace.focus_adjacent(direction).unwrap();
    }
    assert_eq!(workspace.focused(), Some(cards[0]));
}

/// The inspector shortcut acts in every pane with an inspector, a
/// terminal included, and leaves the chord to an application page, which
/// has none, and to a window with no pane focused.
#[test]
fn the_inspector_shortcut_acts_only_where_there_is_an_inspector() {
    use keyboard::key::Code;
    let shortcuts = KeyboardShortcuts::default();
    let modifiers = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
    for (focused, acts) in [
        (FocusedPane::Terminal, true),
        (FocusedPane::Review, true),
        (FocusedPane::Chat, true),
        (FocusedPane::Other, true),
        (FocusedPane::NoInspector, false),
        (FocusedPane::None, false),
    ] {
        assert_eq!(
            global_shortcut(
                &key_press("i", Code::KeyI, modifiers, false),
                &shortcuts,
                focused
            ),
            acts.then_some(GlobalShortcut::ToggleInspector),
            "{focused:?}"
        );
        assert_eq!(
            ShortcutScope::Inspector.applies_to(focused),
            acts,
            "{focused:?}"
        );
    }
}

#[test]
fn chat_selection_shortcuts_only_capture_keys_in_native_chat() {
    use keyboard::key::Code;
    let shortcuts = KeyboardShortcuts::default();
    let modifiers = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
    for (key, code, action) in [
        ("a", Code::KeyA, GlobalShortcut::ChatAccount),
        ("m", Code::KeyM, GlobalShortcut::ChatModel),
        ("o", Code::KeyO, GlobalShortcut::ChatPermissions),
    ] {
        for focused in [
            FocusedPane::Terminal,
            FocusedPane::Review,
            FocusedPane::Other,
            FocusedPane::Chat,
        ] {
            assert_eq!(
                global_shortcut(&key_press(key, code, modifiers, false), &shortcuts, focused),
                (focused == FocusedPane::Chat).then_some(action),
            );
            assert_eq!(
                is_global_shortcut_chord(
                    &keyboard::Key::Character(key.into()),
                    keyboard::key::Physical::Code(code),
                    modifiers,
                    &shortcuts,
                    focused
                ),
                focused == FocusedPane::Chat,
            );
            assert_eq!(
                global_shortcut(&key_press(key, code, modifiers, true), &shortcuts, focused),
                None
            );
        }
    }
    let customized = KeyboardShortcuts {
        chat_model: vec!["Alt+M".into()],
        ..shortcuts
    };
    assert_eq!(
        global_shortcut(
            &key_press("m", Code::KeyM, keyboard::Modifiers::ALT, false),
            &customized,
            FocusedPane::Chat
        ),
        Some(GlobalShortcut::ChatModel)
    );
    assert_eq!(
        global_shortcut(
            &key_press("m", Code::KeyM, modifiers, false),
            &customized,
            FocusedPane::Chat
        ),
        None
    );
}

#[test]
fn shortcut_tooltips_follow_customized_and_unbound_bindings() {
    let mut shortcuts = KeyboardShortcuts::default();
    shortcuts.search_projects = vec!["Alt+S".into(), "Ctrl+Shift+F".into()];
    assert_eq!(
        shortcut_tooltip(GlobalShortcut::FocusProjectSearch, &shortcuts),
        "Search projects — Alt+S, Ctrl+Shift+F"
    );
    shortcuts.close_focused_column.clear();
    assert_eq!(
        shortcut_tooltip_named(
            GlobalShortcut::CloseFocusedCard,
            &shortcuts,
            "Close terminal"
        ),
        "Close terminal — unbound"
    );
}

#[test]
fn every_shortcut_descriptor_has_a_unique_configurable_binding() {
    let shortcuts = KeyboardShortcuts::default();
    let mut bindings = HashSet::new();
    for descriptor in GLOBAL_SHORTCUT_DESCRIPTORS {
        assert!(!descriptor.label.is_empty());
        let keys = shortcut_keys(descriptor.action, &shortcuts);
        assert!(!keys.is_empty());
        for chord in keys {
            assert!(parse_shortcut(chord).is_ok());
            // Distinct across every action, not merely within one, or a
            // chord would dispatch to whichever descriptor came first.
            assert!(bindings.insert(chord.clone()));
        }
        // The one-line form the dialog shows has to read back as the same
        // chords, or editing a two-chord action there would drop one.
        assert_eq!(
            parse_chords(&shortcut_chords(descriptor.action, &shortcuts)),
            *keys
        );
    }
}

#[test]
fn frame_clock_bounds_throttled_frames_without_rewinding_for_stale_frames() {
    let start = Instant::now();
    let mut clock = FrameClock::default();
    assert_eq!(clock.elapsed(start), None);
    assert_eq!(
        clock.elapsed(start + Duration::from_millis(16)),
        Some(Duration::from_millis(16))
    );
    assert_eq!(
        clock.elapsed(start + Duration::from_secs(2)),
        Some(Duration::from_millis(250))
    );
    assert_eq!(clock.elapsed(start + Duration::from_secs(1)), None);
    assert_eq!(
        clock.elapsed(start + Duration::from_millis(2_016)),
        Some(Duration::from_millis(16))
    );
}

#[test]
fn a_started_frame_clock_advances_on_its_first_frame() {
    let start = Instant::now();
    let mut clock = FrameClock::default();
    clock.start(start);
    assert_eq!(clock.frame(), Some(start));
    assert_eq!(
        clock.elapsed(start + Duration::from_millis(9)),
        Some(Duration::from_millis(9))
    );
    // Restarting discards whatever was measured before, and a frame the
    // shell drew before the restart measures nothing.
    clock.start(start + Duration::from_secs(5));
    assert_eq!(clock.elapsed(start + Duration::from_secs(4)), None);
    assert_eq!(
        clock.elapsed(start + Duration::from_millis(5_016)),
        Some(Duration::from_millis(16))
    );
}

/// A leaf that always raises an overlay anchored to its own translated
/// bounds, so a test can tell whether the pane chain forwards `overlay`
/// and with which translation.
struct OverlayProbe;

struct ProbeOverlay {
    anchor: Rectangle,
}

impl Widget<(), Theme, Renderer> for OverlayProbe {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }
    fn layout(
        &mut self,
        _tree: &mut Tree,
        _renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
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
    fn overlay<'a>(
        &'a mut self,
        _tree: &'a mut Tree,
        layout: Layout<'a>,
        _renderer: &Renderer,
        _viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, (), Theme, Renderer>> {
        Some(overlay::Element::new(Box::new(ProbeOverlay {
            anchor: layout.bounds() + translation,
        })))
    }
}

impl overlay::Overlay<(), Theme, Renderer> for ProbeOverlay {
    fn layout(&mut self, _renderer: &Renderer, _bounds: Size) -> layout::Node {
        layout::Node::new(Size::new(10.0, 10.0)).move_to(self.anchor.position())
    }
    fn draw(
        &self,
        _renderer: &mut Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        _layout: Layout<'_>,
        _cursor: mouse::Cursor,
    ) {
    }
}

/// Every card an overlay can be raised from, paired with the screen
/// position its overlay has to land on.
fn expected_overlay_positions(projection: &ViewportProjection, viewport: f32) -> Vec<f32> {
    let mut positions: Vec<f32> = projection
        .mounted
        .iter()
        .map(|card| card.start - projection.offset)
        .filter(|x| *x < viewport && x + 400.0 > 0.0)
        .collect();
    positions.sort_by(f32::total_cmp);
    positions
}

#[test]
fn restored_camera_initializes_native_scroll_once_before_the_first_frame() {
    use cosmic::iced::advanced::{renderer::Headless, widget::operation};
    const VIEWPORT: f32 = 600.0;
    const HEIGHT: f32 = 400.0;
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
    let mut workspace = Workspace::default();
    workspace.resize_viewport(VIEWPORT);
    for index in 0..4 {
        workspace
            .add_card(
                WorkspaceCard::new(
                    CardId::new(),
                    format!("Pane {index}"),
                    CardWidth::Fixed(400.0),
                )
                .unwrap(),
                index,
                false,
            )
            .unwrap();
    }
    // Park the camera at the far end. The cards now on screen sit at
    // content-space positions well outside the visible rect, which is the
    // only arrangement that tells a correct visibility test from one that
    // compares content space against screen space: a workspace of one or
    // two cards has the two spaces on top of each other.
    let offset = workspace.geometry().max_offset(VIEWPORT);
    assert!(
        offset > VIEWPORT,
        "the fixture must scroll past a screenful"
    );
    workspace.native_scroll(offset, VIEWPORT);
    let projection = workspace.projection(VIEWPORT);
    let expected = expected_overlay_positions(&projection, VIEWPORT);
    assert!(
        expected.iter().any(|x| *x > 0.0),
        "the fixture must place a card the camera moved into view"
    );

    let scroll_id = widget::Id::new("workspace.scroll");
    let shortcuts = KeyboardShortcuts::default();
    let mut element: Element<'_, ()> = view(
        &workspace,
        &shortcuts,
        FocusedPane::Other,
        false,
        &InputTrace::default(),
        VIEWPORT,
        scroll_id.clone(),
        |_| Element::new(OverlayProbe),
        card_widget_id,
        Events { viewport: |_| () },
    );
    let mut tree = Tree::new(&element);
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(VIEWPORT, HEIGHT)),
    );
    // No startup scroll task or resize message is delivered. The initial
    // width already matches the model, as it can after restoring a window.
    let visible = Rectangle::new(Point::ORIGIN, Size::new(VIEWPORT, HEIGHT));
    let mut restored_overlay = element
        .as_widget_mut()
        .overlay(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &visible,
            Vector::ZERO,
        )
        .expect("a mounted card raised an overlay");
    let raised = restored_overlay
        .as_overlay_mut()
        .layout(&renderer, visible.size());
    let mut positions: Vec<f32> = raised
        .children()
        .iter()
        .map(|child| child.bounds().x)
        .collect();
    positions.sort_by(f32::total_cmp);
    assert_eq!(positions, expected);
    drop(raised);
    drop(restored_overlay);

    // A later native scroll must survive relayout with the same widget
    // state, even before its feedback has reached the workspace model.
    let moved_offset = offset - 100.0;
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut operation::scrollable::scroll_to::<()>(
            scroll_id,
            operation::scrollable::AbsoluteOffset {
                x: Some(moved_offset),
                y: None,
            },
        ),
    );
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, Size::new(VIEWPORT, HEIGHT)),
    );
    let mut raised = element
        .as_widget_mut()
        .overlay(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &visible,
            Vector::ZERO,
        )
        .expect("native-scrolled panes retain their overlays");
    let raised = raised.as_overlay_mut().layout(&renderer, visible.size());
    let mut positions: Vec<f32> = raised
        .children()
        .iter()
        .map(|child| child.bounds().x)
        .collect();
    positions.sort_by(f32::total_cmp);
    let mut moved = projection;
    moved.offset = moved_offset;
    assert_eq!(positions, expected_overlay_positions(&moved, VIEWPORT));
}
