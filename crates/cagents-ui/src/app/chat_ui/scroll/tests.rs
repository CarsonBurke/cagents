use super::*;

#[test]
fn a_pane_inspector_keeps_modifier_releases_without_scrolling_covered_history() {
    struct Position {
        id: Id,
        top: Option<f32>,
        bottom: Option<f32>,
    }
    impl Operation for Position {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&Id>,
            _: Rectangle,
            _: Rectangle,
            translation: Vector,
            state: &mut dyn operation::Scrollable,
        ) {
            if id != Some(&self.id) {
                return;
            }
            self.top = Some(translation.y);
            if let Some(bottom) = self.bottom.take() {
                state.scroll_to(iced::widget::scrollable::AbsoluteOffset {
                    x: None,
                    y: Some(bottom),
                });
            }
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let position = || Position {
        id: card_control_id(target.card, "chat-transcript"),
        top: None,
        bottom: None,
    };
    let memory = Memory::default();
    let bounds = Rectangle::with_size(Size::new(720.0, 400.0));
    let limits = layout::Limits::new(Size::ZERO, bounds.size());
    let make = |covered: bool| {
        crate::interaction::exclusive_layer(
            transcript_observed(
                item(
                    "body",
                    widget::Space::new().height(2_000).width(Length::Fill),
                ),
                target,
                None,
                vec![],
                true,
                vec![(item_id("body"), "body")],
                Observations::default(),
                memory.clone(),
                false,
                HashSet::new(),
                None,
            ),
            covered.then(|| {
                widget::container(widget::Space::new())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .into()
            }),
        )
    };
    let send =
        |element: &mut Element<'_, Message>, tree: &mut Tree, node: &layout::Node, event: Event| {
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(node),
                mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut vec![]),
                &bounds,
            );
        };
    let read = |element: &mut Element<'_, Message>, tree: &mut Tree, node: &layout::Node| {
        let mut position = position();
        element
            .as_widget_mut()
            .operate(tree, Layout::new(node), &renderer, &mut position);
        position.top.expect("native transcript scroll position")
    };
    let modifiers = |value| Event::Keyboard(keyboard::Event::ModifiersChanged(value));
    let wheel = || {
        Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
        })
    };
    let mut exposed = make(false);
    let mut tree = Tree::new(&exposed);
    let node = exposed
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    exposed.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut Position {
            bottom: Some(500.0),
            ..position()
        },
    );
    send(
        &mut exposed,
        &mut tree,
        &node,
        Event::Window(iced::window::Event::RedrawRequested(
            iced::time::Instant::now(),
        )),
    );
    send(
        &mut exposed,
        &mut tree,
        &node,
        modifiers(keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT),
    );
    let before = read(&mut exposed, &mut tree, &node);
    assert!(before > 500.0);
    let mut covered = make(true);
    tree.diff(covered.as_widget_mut());
    let node = covered
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    send(
        &mut covered,
        &mut tree,
        &node,
        modifiers(keyboard::Modifiers::empty()),
    );
    send(&mut covered, &mut tree, &node, wheel());
    let mut exposed = make(false);
    tree.diff(exposed.as_widget_mut());
    let node = exposed
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert_eq!(read(&mut exposed, &mut tree, &node), before);
    send(&mut exposed, &mut tree, &node, wheel());
    assert!(
        read(&mut exposed, &mut tree, &node) > before,
        "released Shift must not redirect vertical wheel input to the horizontal axis"
    );
}

#[test]
fn input_capture_and_anchor_preserve_accessible_children() {
    let captured = Rc::new(Cell::new(false));
    let child = Element::new(InputCapture {
        content: Element::new(crate::ui_controls::accessibility_probe::Probe),
        captured,
    });
    let element = tool_output(child);
    let tree = Tree::new(&element);
    let node = layout::Node::new(Size::new(80.0, 40.0));
    let nodes = element.as_widget().a11y_nodes(
        Layout::new(&node).with_virtual_offset(Vector::new(0.0, 37.0)),
        &tree,
        mouse::Cursor::Unavailable,
    );
    assert_eq!(nodes.root().len(), 1);
    assert_eq!(nodes.root()[0].node().label(), Some("Retained child"));
}

// The draw counter observes actual widget traversal. It does not supply
// viewport answers or keep any virtual row/tree alive.
struct PresentationProbe {
    label: &'static str,
    height: f32,
    draws: Rc<std::cell::RefCell<Vec<&'static str>>>,
    clicks: Rc<Cell<usize>>,
}
impl Widget<Message, cosmic::Theme, cosmic::Renderer> for PresentationProbe {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<iced_accessibility::Id>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(iced_accessibility::Id::unique())
    }
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fixed(self.height))
    }
    fn layout(
        &mut self,
        _: &mut Tree,
        _: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(Size::new(limits.max().width, self.height))
    }
    fn draw(
        &self,
        _: &Tree,
        _: &mut cosmic::Renderer,
        _: &cosmic::Theme,
        _: &renderer::Style,
        _: Layout<'_>,
        _: mouse::Cursor,
        _: &Rectangle,
    ) {
        self.draws.borrow_mut().push(self.label);
    }
    fn update(
        &mut self,
        _: &mut Tree,
        event: &Event,
        _: Layout<'_>,
        _: mouse::Cursor,
        _: &cosmic::Renderer,
        _: &mut dyn Clipboard,
        _: &mut Shell<'_, Message>,
        _: &Rectangle,
    ) {
        if matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_))) {
            self.clicks.set(self.clicks.get() + 1);
        }
    }
    fn mouse_interaction(
        &self,
        _: &Tree,
        _: Layout<'_>,
        _: mouse::Cursor,
        _: &Rectangle,
        _: &cosmic::Renderer,
    ) -> mouse::Interaction {
        mouse::Interaction::Pointer
    }
    fn a11y_nodes(
        &self,
        _: Layout<'_>,
        tree: &Tree,
        _: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        let mut node =
            iced_accessibility::accesskit::Node::new(iced_accessibility::accesskit::Role::Button);
        node.set_label(self.label);
        iced_accessibility::A11yTree::leaf(
            node,
            tree.state.downcast_ref::<iced_accessibility::Id>().clone(),
        )
    }
}

fn presentation_probe(
    label: &'static str,
    height: f32,
    draws: &Rc<std::cell::RefCell<Vec<&'static str>>>,
    clicks: &Rc<Cell<usize>>,
) -> Element<'static, Message> {
    Element::new(PresentationProbe {
        label,
        height,
        draws: draws.clone(),
        clicks: clicks.clone(),
    })
}

#[test]
fn a_reader_at_the_end_follows_growth_whose_estimate_is_corrected_by_measurement() {
    // Rows are estimated taller than they measure, so each growth changes
    // the content height again within the same layout.
    fn transcript(rows: u64) -> Element<'static, Message> {
        let draws = Rc::new(std::cell::RefCell::new(Vec::new()));
        let clicks = Rc::new(Cell::new(0));
        let column = virtualized::column(
            (0..rows)
                .map(|key| {
                    let draws = draws.clone();
                    let clicks = clicks.clone();
                    virtualized::Row::new(key, 0, 100.0, None, move || {
                        presentation_probe("line", 60.0, &draws, &clicks)
                    })
                })
                .collect(),
            Default::default(),
        );
        transcript_observed(
            column,
            Target {
                card: CardId::new(),
                generation: 0,
            },
            None,
            vec![],
            true,
            vec![],
            Observations::default(),
            Memory::default(),
            false,
            HashSet::new(),
            None,
        )
    }
    let renderer = crate::app::tests::headless_renderer();
    let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
    let mut element = transcript(40);
    let mut tree = Tree::new(&element);
    let end_distance = |element: &mut Element<'static, Message>, tree: &mut Tree| {
        let node = element.as_widget_mut().layout(tree, &renderer, &limits);
        let mut top = ReadTop::default();
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), &renderer, &mut top);
        let travel = node.children()[0].size().height - node.size().height;
        travel - top.0.expect("a transcript scrollable")
    };
    assert!(end_distance(&mut element, &mut tree).abs() <= 0.5);
    for rows in [80, 120, 200] {
        let mut grown = transcript(rows);
        tree.diff(&mut grown);
        assert!(
            end_distance(&mut grown, &mut tree).abs() <= 0.5,
            "growing to {rows} rows left the reader above the end"
        );
        element = grown;
    }
    drop(element);
}

#[test]
fn cold_reader_draw_waits_for_more_than_four_actual_virtual_viewport_passes() {
    fn nested(
        depth: u64,
        draws: Rc<std::cell::RefCell<Vec<&'static str>>>,
        clicks: Rc<Cell<usize>>,
    ) -> Element<'static, Message> {
        if depth == 0 {
            return presentation_probe("restored beginning", 400.0, &draws, &clicks);
        }
        let nested_draws = draws.clone();
        let nested_clicks = clicks.clone();
        let mut first = virtualized::Row::new(depth * 2, 0, 12_000.0, None, move || {
            nested(depth - 1, nested_draws.clone(), nested_clicks.clone())
        });
        first.anchor = true;
        virtualized::column(
            vec![
                first,
                virtualized::Row::new(depth * 2 + 1, 0, 6_000.0, None, move || {
                    presentation_probe("provisional end", 6_000.0, &draws, &clicks)
                }),
            ],
            Default::default(),
        )
    }
    let mut renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let memory = Memory::default();
    memory.request_edge(None, Edge::Start);
    let draws = Rc::new(std::cell::RefCell::new(Vec::new()));
    let clicks = Rc::new(Cell::new(0));
    let mut element = transcript_observed(
        nested(10, draws.clone(), clicks.clone()),
        target,
        None,
        vec![],
        true,
        vec![],
        Observations::default(),
        memory,
        false,
        HashSet::new(),
        None,
    );
    let mut tree = Tree::new(&element);
    let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
    let limits = layout::Limits::new(Size::ZERO, viewport.size());
    let mut node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert!(tree.state.downcast_ref::<ReaderState>().revealing);
    assert!(
        tree.state.downcast_ref::<ReaderState>().needs_layout,
        "real nested windows exceed the unchanged four-pass layout bound"
    );
    for _ in 0..8 {
        element.as_widget().draw(
            &tree,
            &mut renderer,
            &cosmic::Theme::dark(),
            &renderer::Style::default(),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &viewport,
        );
        assert!(
            draws.borrow().is_empty(),
            "provisional windows must not paint"
        );
        assert!(
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable)
                .root()
                .is_empty()
        );
        assert_eq!(
            element.as_widget().mouse_interaction(
                &tree,
                Layout::new(&node),
                mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                &viewport,
                &renderer
            ),
            mouse::Interaction::None
        );
        assert!(
            element
                .as_widget_mut()
                .overlay(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &viewport,
                    Vector::ZERO
                )
                .is_none()
        );
        let mut messages = vec![];
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut shell,
            &viewport,
        );
        assert_eq!(clicks.get(), 0, "unpainted text is not clickable");
        element.as_widget_mut().update(
            &mut tree,
            &Event::Window(iced::window::Event::RedrawRequested(
                iced::time::Instant::now(),
            )),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut shell,
            &viewport,
        );
        assert!(
            shell.is_layout_invalid(),
            "cap exhaustion must make native progress"
        );
        assert_eq!(
            shell.redraw_request(),
            iced::window::RedrawRequest::NextFrame
        );
        node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        if !tree.state.downcast_ref::<ReaderState>().revealing {
            break;
        }
    }
    assert!(
        !tree.state.downcast_ref::<ReaderState>().revealing,
        "actual nested windows must eventually converge"
    );
    assert!(!tree.state.downcast_ref::<ReaderState>().needs_layout);
    let mut top = ReadTop::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut top);
    assert_eq!(top.0, Some(0.0));
    element.as_widget().draw(
        &tree,
        &mut renderer,
        &cosmic::Theme::dark(),
        &renderer::Style::default(),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &viewport,
    );
    assert_eq!(&*draws.borrow(), &["restored beginning"]);
    assert_eq!(
        element.as_widget().mouse_interaction(
            &tree,
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &viewport,
            &renderer
        ),
        mouse::Interaction::Pointer
    );
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        Layout::new(&node),
        mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut vec![]),
        &viewport,
    );
    assert!(
        clicks.get() > 0,
        "coherent content becomes interactive again"
    );
    assert!(
        !element
            .as_widget()
            .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable)
            .root()
            .is_empty()
    );
}

#[test]
fn cold_pending_reader_is_unpainted_without_spinning_but_find_and_empty_settle() {
    let mut renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let memory = Memory::default();
    let draws = Rc::new(std::cell::RefCell::new(Vec::new()));
    let clicks = Rc::new(Cell::new(0));
    let navigation = find_navigation::State::default();
    let guard = navigation.guard();
    let make = |pending, height| {
        transcript_observed(
            item("body", presentation_probe("body", height, &draws, &clicks)),
            target,
            None,
            vec![],
            true,
            vec![(item_id("body"), "body")],
            Observations::default(),
            memory.clone(),
            pending,
            HashSet::new(),
            Some(guard.clone()),
        )
    };
    let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
    let limits = layout::Limits::new(Size::ZERO, viewport.size());
    {
        let mut ready = make(false, 2_000.0);
        let mut tree = Tree::new(&ready);
        let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
        ready.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(500.0),
        );
    }
    let mut pending = make(true, 2_000.0);
    let mut tree = Tree::new(&pending);
    let node = pending
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert!(tree.state.downcast_ref::<ReaderState>().revealing);
    pending.as_widget().draw(
        &tree,
        &mut renderer,
        &cosmic::Theme::dark(),
        &renderer::Style::default(),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &viewport,
    );
    assert!(draws.borrow().is_empty());
    let mut messages = vec![];
    let mut shell = Shell::new(&mut messages);
    pending.as_widget_mut().update(
        &mut tree,
        &Event::Window(iced::window::Event::RedrawRequested(
            iced::time::Instant::now(),
        )),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut shell,
        &viewport,
    );
    assert!(
        !shell.is_layout_invalid(),
        "source-pending alone cannot spin native layouts"
    );
    assert_eq!(shell.redraw_request(), iced::window::RedrawRequest::Wait);
    let mut find = ApplyFind {
        target,
        scope: TranscriptScope::default(),
        scroll: card_control_id(target.card, "chat-transcript"),
        location: LogicalLocation {
            offset: 800.0,
            exact: true,
            stage: 3,
            shape_pending: None,
            horizontal: vec![],
        },
        intent: guard.clone(),
        document: Some(navigation.document_epoch()),
    };
    pending
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut find);
    assert!(guard.current());
    assert!(
        !tree.state.downcast_ref::<ReaderState>().restore,
        "Find operations remain active while presentation is hidden"
    );
    // Find can accept a native correction before source readiness. Its
    // next layout must not turn that correction into presentation readiness.
    let mut still_pending = make(true, 2_000.0);
    tree.diff(still_pending.as_widget_mut());
    let pending_node = still_pending
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert!(tree.state.downcast_ref::<ReaderState>().revealing);
    still_pending.as_widget().draw(
        &tree,
        &mut renderer,
        &cosmic::Theme::dark(),
        &renderer::Style::default(),
        Layout::new(&pending_node),
        mouse::Cursor::Unavailable,
        &viewport,
    );
    assert!(draws.borrow().is_empty());
    assert!(
        still_pending
            .as_widget()
            .a11y_nodes(
                Layout::new(&pending_node),
                &tree,
                mouse::Cursor::Unavailable
            )
            .root()
            .is_empty()
    );
    let mut shell = Shell::new(&mut messages);
    still_pending.as_widget_mut().update(
        &mut tree,
        &Event::Window(iced::window::Event::RedrawRequested(
            iced::time::Instant::now(),
        )),
        Layout::new(&pending_node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut shell,
        &viewport,
    );
    assert!(!shell.is_layout_invalid());
    assert_ne!(
        shell.redraw_request(),
        iced::window::RedrawRequest::NextFrame
    );
    let mut ready = make(false, 2_000.0);
    tree.diff(ready.as_widget_mut());
    let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
    assert!(!tree.state.downcast_ref::<ReaderState>().revealing);
    let mut top = ReadTop::default();
    ready
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut top);
    assert_eq!(top.0, Some(800.0));
    ready.as_widget().draw(
        &tree,
        &mut renderer,
        &cosmic::Theme::dark(),
        &renderer::Style::default(),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &viewport,
    );
    assert_eq!(&*draws.borrow(), &["body"]);
    // A pending explicit edge in an already visible context must not blank it.
    memory.request_edge(None, Edge::Latest);
    let mut visible_pending = make(true, 2_000.0);
    tree.diff(visible_pending.as_widget_mut());
    visible_pending
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert!(!tree.state.downcast_ref::<ReaderState>().revealing);
    // A new, genuinely empty reader has no geometry to wait for.
    let mut empty = transcript_observed(
        widget::column::with_children(Vec::<Element<'_, Message>>::new()),
        target,
        None,
        vec![],
        true,
        vec![],
        Observations::default(),
        memory.clone(),
        false,
        HashSet::new(),
        None,
    );
    let mut empty_tree = Tree::new(&empty);
    empty
        .as_widget_mut()
        .layout(&mut empty_tree, &renderer, &limits);
    assert!(!empty_tree.state.downcast_ref::<ReaderState>().revealing);
    assert!(!empty_tree.state.downcast_ref::<ReaderState>().needs_layout);
    assert_eq!(memory.edge(None), None);
    assert!(memory.0.borrow().contexts[0].rows.is_empty());
}

#[test]
fn wheel_during_cold_reveal_moves_from_saved_reader_not_provisional_end() {
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
    let limits = layout::Limits::new(Size::ZERO, viewport.size());
    let mut final_tops = vec![];
    for pending in [false, true] {
        let memory = Memory::default();
        let make = |pending| {
            transcript_observed(
                item(
                    "body",
                    widget::Space::new().height(2_000).width(Length::Fill),
                ),
                target,
                None,
                vec![],
                true,
                vec![(item_id("body"), "body")],
                Observations::default(),
                memory.clone(),
                pending,
                HashSet::new(),
                None,
            )
        };
        {
            let mut initial = make(false);
            let mut tree = Tree::new(&initial);
            let node = initial
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            initial.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut SetBottom(500.0),
            );
        }
        let mut reader = make(pending);
        let mut tree = Tree::new(&reader);
        let node = reader.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let mut before = ReadTop::default();
        reader
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
        assert_eq!(
            before.0,
            Some(if pending { 1_600.0 } else { 1_100.0 }),
            "cold native default is deliberately different from the saved middle"
        );
        reader.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut vec![]),
            &viewport,
        );
        let mut ready = make(false);
        tree.diff(ready.as_widget_mut());
        let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let mut after = ReadTop::default();
        ready
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
        final_tops.push(after.0.unwrap());
    }
    assert!(
        final_tops[0] > 1_100.0 && final_tops[0] < 1_600.0,
        "real native wheel actually moves the visible control toward the end"
    );
    assert_eq!(
        final_tops[0], final_tops[1],
        "pending return preserves exactly the same reader-relative wheel movement"
    );
}

#[test]
fn cold_reader_wheel_reports_saved_geometry_and_dispatches_real_history_edges() {
    let renderer = crate::app::tests::headless_renderer();
    let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
    let limits = layout::Limits::new(Size::ZERO, viewport.size());
    // The third case's conflicting axes require the modifier cached while
    // source-pending; using the old unshifted y would miss Earlier.
    for (bottom, shifted) in [(false, false), (true, false), (false, true)] {
        if shifted && cfg!(target_os = "macos") {
            continue;
        }
        let (mut live, handle) = view::prepared_runtime_for_test(&[]);
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let mut target = live.target;
        target.card = app.workspace.focused().unwrap();
        live.target = target;
        app.card_state_mut(target.card).unwrap().kind = CardKind::Chat(live.session.clone());
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.has_earlier = true;
        snapshot.has_later = true;
        handle.replace_snapshot_for_test(snapshot);
        live.refresh();
        let memory = live.scroll_memory.clone();
        let observations = live.observed_scroll.clone();
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(live)));
        let make = |pending, height| {
            transcript_observed(
                item(
                    "body",
                    widget::Space::new().height(height).width(Length::Fill),
                ),
                target,
                None,
                vec![],
                true,
                vec![(item_id("body"), "body")],
                observations.clone(),
                memory.clone(),
                pending,
                HashSet::new(),
                None,
            )
        };
        {
            let mut ready = make(false, 2_000.0);
            let mut tree = Tree::new(&ready);
            let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
            ready.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut SetBottom(if bottom { 0.0 } else { 1_600.0 }),
            );
        }
        // A fitting cold placeholder cannot be mistaken for authoritative
        // fitting history: outward paging uses retained 2000/400 geometry.
        let mut pending = make(true, 20.0);
        let mut tree = Tree::new(&pending);
        let node = pending
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut messages = vec![];
        if shifted {
            pending.as_widget_mut().update(
                &mut tree,
                &Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
                    iced::keyboard::Modifiers::SHIFT,
                )),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            assert!(memory.0.borrow().modifiers.shift());
        }
        pending.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines {
                    x: if shifted { 1.0 } else { 0.0 },
                    y: if bottom || shifted { -1.0 } else { 1.0 },
                },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        let reports: Vec<_> = messages
            .into_iter()
            .filter(|message| {
                matches!(message,
                    Message::ChatUi(Action::Scrolled(actual, None, _, Some(_))) if *actual == target
                )
            })
            .collect();
        assert_eq!(
            reports.len(),
            1,
            "outward wheel must publish real reader intent"
        );
        let Message::ChatUi(Action::Scrolled(_, _, scroll, _)) = &reports[0] else {
            unreachable!()
        };
        assert_eq!(scroll.content(), 2_000.0);
        assert_eq!(scroll.window_height(), 400.0);
        assert_eq!(scroll.from_top(), if bottom { 1_600.0 } else { 0.0 });
        assert_eq!(observations.borrow()[&None], *scroll);
        drop(pending);
        drop(app.update(reports.into_iter().next().unwrap()));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(
            runtime.pending.as_ref().and_then(|pending| pending.page),
            Some(if bottom {
                view::Page::Newer
            } else {
                view::Page::Earlier
            }),
            "actual App consumes the emitted wheel intent and starts the matching provider page"
        );
    }
}

fn history_reader(
    target: Target,
    observations: Observations,
    memory: Memory,
    entries: Vec<(u64, &'static str, f32)>,
    follow_end: bool,
) -> Element<'static, Message> {
    let ids = entries
        .iter()
        .map(|(_, id, _)| (item_id(id), *id))
        .collect();
    let rows = entries
        .into_iter()
        .map(|(key, id, height)| {
            let mut row = virtualized::Row::new(
                key,
                u64::from(height.to_bits()),
                height,
                Some(item_id(id)),
                move || item(id, widget::Space::new().width(Length::Fill).height(height)),
            );
            row.anchor = true;
            row
        })
        .collect();
    transcript_observed(
        virtualized::column(rows, Default::default()),
        target,
        None,
        vec![],
        follow_end,
        ids,
        observations,
        memory,
        false,
        HashSet::new(),
        None,
    )
}

#[test]
fn native_paging_shrinks_to_loaded_content_and_wheels_choose_each_edge() {
    let renderer = crate::app::tests::headless_renderer();
    let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
    let limits = layout::Limits::new(Size::ZERO, viewport.size());
    for (height, top, wheel, expected) in [
        (200.0, 0.0, 1.0, view::Page::Earlier),
        (200.0, 0.0, -1.0, view::Page::Newer),
        (2000.0, 20.0, 1.0, view::Page::Earlier),
        (2000.0, 1580.0, -1.0, view::Page::Newer),
    ] {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (_, target, handle) = super::super::tests::history_fixture(&mut app);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.has_later = true;
        handle.publish_snapshot_for_test(snapshot);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.refresh();
        let observations = runtime.observed_scroll.clone();
        let memory = runtime.scroll_memory.clone();
        let make = |height| {
            history_reader(
                target,
                observations.clone(),
                memory.clone(),
                vec![(1, "loaded", height)],
                false,
            )
        };
        let mut element = make(2000.0);
        let mut tree = Tree::new(&element);
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        // A byte-bounded older window may contain mostly collapsed tools.
        // Its actual rendered extent must replace the tall previous page.
        element = make(height);
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(node.children()[0].size().height, height);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom((height - viewport.height).max(0.0) - top),
        );
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        // SetBottom positions this fixture; only the following real wheel
        // is the reader gesture under test.
        memory.0.borrow_mut().pending_input = None;
        let mut messages = vec![];
        let update = |event: Event, element: &mut Element<'_, Message>, tree: &mut Tree| {
            let mut messages = vec![];
            element.as_widget_mut().update(
                tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(iced::Point::new(50.0, 350.0)),
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            messages
        };
        messages.extend(update(
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            &mut element,
            &mut tree,
        ));
        assert!(messages.is_empty(), "fitting geometry must not auto-page");
        assert_eq!(observations.borrow()[&None].content(), height);
        for _ in 0..2 {
            messages.extend(update(
                Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Pixels { x: 0.0, y: wheel },
                }),
                &mut element,
                &mut tree,
            ));
            assert!(
                matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(_, None, scroll, Some(page)))]
                if *page == expected && scroll.content() == height)
            );
            if height > viewport.height {
                let Message::ChatUi(Action::Scrolled(_, _, scroll, _)) = &messages[0] else {
                    unreachable!()
                };
                assert!(
                    scroll.from_top() > 0.0 && scroll.from_top() < height - viewport.height,
                    "prefetch must start before the physical edge"
                );
            }
            drop(app.update(messages.pop().unwrap()));
            let runtime = app.chat_runtime_mut(target).unwrap();
            let pending = runtime
                .pending
                .as_ref()
                .expect("native wheel starts a page");
            assert_eq!(pending.page, Some(expected));
            let request = pending.id;
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            handle.publish_snapshot_for_test(snapshot);
            drop(app.update(Message::ChatUi(Action::CommandFinished(
                target,
                request,
                Ok(()),
            ))));
            assert!(
                app.chat_runtime_mut(target).unwrap().pending.is_none(),
                "arrival of an equal-height page must wait for the next gesture"
            );
        }
    }
}

#[test]
fn native_newer_page_keeps_the_surviving_exchange_at_its_screen_position() {
    let renderer = crate::app::tests::headless_renderer();
    let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
    for (fitting, latest) in [(false, false), (false, true), (true, false), (true, true)] {
        let memory = Memory::default();
        let observations = Observations::default();
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let entries = if fitting {
            vec![(2, "boundary", 200.0)]
        } else {
            vec![(1, "older", 800.0), (2, "boundary", 500.0)]
        };
        let make = |entries, follow_end| {
            history_reader(
                target,
                observations.clone(),
                memory.clone(),
                entries,
                follow_end,
            )
        };
        let mut element = make(entries, false);
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(0.0),
        );
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let screen_y = || {
            let anchor = memory.0.borrow();
            let context = &anchor.contexts[0];
            context
                .rows
                .iter()
                .find(|row| row.id == "boundary")
                .unwrap()
                .bounds
                .y
                - context.top.unwrap()
        };
        let before = screen_y();
        element = make(
            vec![
                (2, "boundary", if fitting { 200.0 } else { 500.0 }),
                (3, "newer", 1200.0),
            ],
            latest,
        );
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut top = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut top);
        assert_eq!(top.0, Some(if fitting { 0.0 } else { 100.0 }));
        assert_eq!(
            screen_y(),
            before,
            "newer history must not follow the new end"
        );
    }
}

#[test]
fn fitting_native_wheel_defers_its_direction_and_can_reverse_before_command_ack() {
    let renderer = crate::app::tests::headless_renderer();
    let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
    let limits = layout::Limits::new(Size::ZERO, viewport.size());
    for first in [view::Page::Earlier, view::Page::Newer] {
        for reverse in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let (_, target, handle) = super::super::tests::history_fixture(&mut app);
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            snapshot.has_later = true;
            handle.publish_snapshot_for_test(snapshot);
            let runtime = app.chat_runtime_mut(target).unwrap();
            runtime.refresh();
            let mut element = history_reader(
                target,
                runtime.observed_scroll.clone(),
                runtime.scroll_memory.clone(),
                vec![(1, "collapsed", 200.0)],
                false,
            );
            let mut tree = Tree::new(&element);
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            drop(app.chat_send(
                target,
                Command::SetPermissionMode {
                    mode: "manual".into(),
                },
                false,
            ));
            let request = app
                .chat_runtime_mut(target)
                .unwrap()
                .pending
                .as_ref()
                .unwrap()
                .id;
            let opposite = if first == view::Page::Earlier {
                view::Page::Newer
            } else {
                view::Page::Earlier
            };
            let directions = if reverse {
                vec![first, opposite]
            } else {
                vec![first]
            };
            for direction in directions {
                let mut messages = vec![];
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Mouse(mouse::Event::WheelScrolled {
                        delta: mouse::ScrollDelta::Lines {
                            x: 0.0,
                            y: if direction == view::Page::Earlier {
                                1.0
                            } else {
                                -1.0
                            },
                        },
                    }),
                    Layout::new(&node),
                    mouse::Cursor::Available(iced::Point::new(50.0, 350.0)),
                    &renderer,
                    &mut iced::advanced::clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
                assert_eq!(messages.len(), 1);
                drop(app.update(messages.pop().unwrap()));
                assert_eq!(
                    app.chat_runtime_mut(target).unwrap().deferred_page,
                    Some(if direction == view::Page::Earlier {
                        HistoryNavigation::EarlierEdge
                    } else {
                        HistoryNavigation::NewerEdge
                    })
                );
            }
            drop(app.update(Message::ChatUi(Action::CommandFinished(
                target,
                request,
                Ok(()),
            ))));
            assert_eq!(
                app.chat_runtime_mut(target)
                    .unwrap()
                    .pending
                    .as_ref()
                    .unwrap()
                    .page,
                Some(if reverse { opposite } else { first })
            );
        }
    }
}

fn geometry(height: f32) -> Geometry {
    Geometry {
        viewport: Size::new(400.0, 400.0),
        content: Size::new(400.0, height),
    }
}
fn rows<'a>(entries: &[(&'a str, f32, f32)]) -> Vec<(&'a str, Rectangle)> {
    entries
        .iter()
        .map(|(id, y, height)| {
            (
                *id,
                Rectangle {
                    x: 0.0,
                    y: *y,
                    width: 400.0,
                    height: *height,
                },
            )
        })
        .collect()
}
fn reading() -> Anchor {
    let mut anchor = Anchor::default();
    assert_eq!(
        anchor.layout(
            None,
            &rows(&[
                ("a", 0.0, 500.0),
                ("b", 500.0, 500.0),
                ("c", 1000.0, 1000.0)
            ]),
            geometry(2000.0),
            None
        ),
        Some(0.0)
    );
    anchor
}

#[test]
fn dropping_and_recreating_native_trees_preserves_only_the_reader_anchor_memory() {
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let memory = Memory::default();
    let observations = Observations::default();
    let make = || {
        transcript_observed(
            widget::column::with_children(
                ["first", "middle", "last"]
                    .into_iter()
                    .map(|id| item(id, widget::Space::new().height(500).width(Length::Fill)))
                    .collect::<Vec<_>>(),
            ),
            target,
            None,
            vec![],
            true,
            ["first", "middle", "last"]
                .into_iter()
                .map(|id| (item_id(id), id))
                .collect(),
            observations.clone(),
            memory.clone(),
            false,
            HashSet::new(),
            None,
        )
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
    {
        let mut element = make();
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(500.0),
        );
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
    }
    assert!(
        memory.retained_rows() > 0,
        "only stable row IDs and geometry survive tree drop"
    );
    let mut recreated = make();
    let mut tree = Tree::new(&recreated);
    let node = recreated
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut actual = ReadTop::default();
    recreated
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
    assert_eq!(
        actual.0,
        Some(600.0),
        "revealing a hidden transcript does not jump to its end"
    );
}

#[test]
fn cold_reader_restore_ignores_empty_and_same_item_placeholder_layouts() {
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let memory = Memory::default();
    let observations = Observations::default();
    let make = |pending, height| {
        let ids = if height == 0 {
            vec![]
        } else {
            vec!["first", "middle", "last"]
        };
        transcript_observed(
            widget::column::with_children(
                ids.iter()
                    .map(|id| item(id, widget::Space::new().height(height).width(Length::Fill)))
                    .collect::<Vec<_>>(),
            ),
            target,
            None,
            vec![],
            true,
            ids.into_iter().map(|id| (item_id(id), id)).collect(),
            observations.clone(),
            memory.clone(),
            pending,
            HashSet::new(),
            None,
        )
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
    {
        let mut element = make(false, 500);
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(500.0),
        );
        // Deliberately drop before another layout: operations must commit
        // actual native geometry when it changes, just as wheel input does.
    }
    assert_eq!(memory.0.borrow().contexts[0].top, Some(600.0));
    let mut pending = make(true, 0);
    let mut tree = Tree::new(&pending);
    for height in [0, 20, 20] {
        pending = make(true, height);
        tree.diff(pending.as_widget_mut());
        let node = pending
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        pending.as_widget_mut().update(
            &mut tree,
            &Event::Window(iced::window::Event::RedrawRequested(
                iced::time::Instant::now(),
            )),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut vec![]),
            &Rectangle::with_size(Size::new(720.0, 400.0)),
        );
        assert_eq!(memory.0.borrow().contexts[0].top, Some(600.0));
        assert_eq!(
            memory.0.borrow().contexts[0]
                .geometry
                .unwrap()
                .content
                .height,
            1500.0
        );
        assert!(tree.state.downcast_ref::<ReaderState>().restore);
        assert!(
            observations.borrow().is_empty(),
            "temporary geometry cannot replace runtime observations"
        );
    }
    let mut ready = make(false, 500);
    tree.diff(ready.as_widget_mut());
    let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
    let mut actual = ReadTop::default();
    ready
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
    assert_eq!(actual.0, Some(600.0));
    // An authoritative empty replacement is different from hydration wait.
    let mut empty = make(false, 0);
    tree.diff(empty.as_widget_mut());
    empty.as_widget_mut().layout(&mut tree, &renderer, &limits);
    assert_eq!(memory.0.borrow().contexts[0].top, Some(0.0));
    assert!(memory.0.borrow().contexts[0].rows.is_empty());
    let mut replacement = make(false, 500);
    tree.diff(replacement.as_widget_mut());
    let node = replacement
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut actual = ReadTop::default();
    replacement
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
    assert_eq!(
        actual.0,
        Some(1100.0),
        "a genuinely empty transcript resets end-follow"
    );
}

#[test]
fn actual_native_wheel_survives_tree_drop_before_the_next_layout() {
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let memory = Memory::default();
    let observations = Observations::default();
    let make = || {
        transcript_observed(
            item(
                "body",
                widget::Space::new().height(2000).width(Length::Fill),
            ),
            target,
            None,
            vec![],
            true,
            vec![(item_id("body"), "body")],
            observations.clone(),
            memory.clone(),
            false,
            HashSet::new(),
            None,
        )
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
    {
        let mut element = make();
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 5.0 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(100.0, 100.0)),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut vec![]),
            &Rectangle::with_size(Size::new(720.0, 400.0)),
        );
    }
    let expected = observations.borrow()[&None].from_top();
    assert!(
        expected < 1600.0 && expected > 0.0,
        "the real native wheel must move the reader"
    );
    let mut restored = make();
    let mut tree = Tree::new(&restored);
    let node = restored
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut actual = ReadTop::default();
    restored
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
    assert_eq!(actual.0, Some(expected));
}

#[test]
fn pending_child_switch_and_current_find_preserve_their_own_native_reader_context() {
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 3,
    };
    let memory = Memory::default();
    let navigation = find_navigation::State::default();
    let make = |context: Option<&'static str>, pending, height| {
        transcript_observed(
            widget::column::with_children(
                ["first", "middle", "last"]
                    .into_iter()
                    .map(|id| item(id, widget::Space::new().height(height).width(Length::Fill)))
                    .collect::<Vec<_>>(),
            ),
            target,
            context,
            vec![],
            true,
            ["first", "middle", "last"]
                .into_iter()
                .map(|id| (item_id(id), id))
                .collect(),
            Observations::default(),
            memory.clone(),
            pending,
            HashSet::new(),
            Some(navigation.guard()),
        )
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
    let mut element = make(None, false, 500);
    let mut tree = Tree::new(&element);
    for (context, bottom) in [(None, 500.0), (Some("child"), 300.0)] {
        element = make(context, false, 500);
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(bottom),
        );
    }
    assert!(
        !memory.restoration_pending(None, false, |id| id == "last"),
        "an unrelated cold tail cannot block the saved window"
    );
    assert!(memory.restoration_pending(None, false, |id| id == "middle"));
    // This is a context switch within a retained native Tree, not merely
    // restoration after creating a fresh Tree.
    element = make(None, true, 20);
    tree.diff(element.as_widget_mut());
    element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert_eq!(
        memory
            .0
            .borrow()
            .contexts
            .iter()
            .find(|c| c.subagent.is_none())
            .unwrap()
            .top,
        Some(600.0)
    );
    assert_eq!(
        memory
            .0
            .borrow()
            .contexts
            .iter()
            .find(|c| c.subagent.as_deref() == Some("child"))
            .unwrap()
            .top,
        Some(800.0)
    );
    element = make(None, false, 500);
    tree.diff(element.as_widget_mut());
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut actual = ReadTop::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
    assert_eq!(actual.0, Some(600.0));
    drop(tree);
    memory.request_edge(None, Edge::End);
    let mut pending = make(None, true, 500);
    let mut tree = Tree::new(&pending);
    let node = pending
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let stale = navigation.document_epoch();
    navigation.changed_document();
    let apply = |document| ApplyFind {
        target,
        scope: TranscriptScope::default(),
        scroll: card_control_id(target.card, "chat-transcript"),
        location: LogicalLocation {
            offset: 200.0,
            exact: true,
            stage: 0,
            shape_pending: None,
            horizontal: vec![],
        },
        intent: navigation.guard(),
        document: Some(document),
    };
    pending
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut apply(stale));
    assert!(
        tree.state.downcast_ref::<ReaderState>().restore,
        "stale Find cannot replace the pending saved reader"
    );
    assert_eq!(memory.edge(None), Some(Edge::End));
    pending.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut apply(navigation.document_epoch()),
    );
    assert!(
        !tree.state.downcast_ref::<ReaderState>().restore,
        "an accepted current Find owns the new native position"
    );
    assert_eq!(
        memory.edge(None),
        None,
        "only a current accepted Find supersedes the pending edge"
    );
    let mut ready = make(None, false, 500);
    tree.diff(ready.as_widget_mut());
    let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
    let mut actual = ReadTop::default();
    ready
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
    assert_eq!(
        actual.0,
        Some(900.0),
        "cold completion must not undo the accepted Find correction"
    );
}

#[test]
fn superseded_latest_refresh_keeps_native_find_and_overlap_without_waiting_for_unrelated_cold_rows()
{
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 3,
    };
    let navigation = find_navigation::State::default();
    let memory = Memory::default();
    let make = |context, ids: Vec<&'static str>, height, preparing| {
        transcript_observed(
            widget::column::with_children(
                ids.iter()
                    .map(|id| item(id, widget::Space::new().height(height).width(Length::Fill)))
                    .collect::<Vec<_>>(),
            ),
            target,
            context,
            vec![],
            true,
            ids.into_iter().map(|id| (item_id(id), id)).collect(),
            Observations::default(),
            memory.clone(),
            false,
            preparing,
            Some(navigation.guard()),
        )
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
    let old = vec!["first", "middle", "last"];
    let mut element = make(None, old.clone(), 500, HashSet::new());
    let mut tree = Tree::new(&element);
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut SetBottom(500.0),
    );
    memory.preserve_refresh(42, 10);
    let document = navigation.document_epoch();
    navigation.changed_document();
    let apply = |document| ApplyFind {
        target,
        scope: TranscriptScope::default(),
        scroll: card_control_id(target.card, "chat-transcript"),
        location: LogicalLocation {
            offset: 200.0,
            exact: true,
            stage: 0,
            shape_pending: None,
            horizontal: vec![],
        },
        intent: navigation.guard(),
        document: Some(document),
    };
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut apply(document),
    );
    let mut actual = ReadTop::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
    assert_eq!(actual.0, Some(600.0), "stale Find cannot change reader");
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut apply(navigation.document_epoch()),
    );
    let mut actual = ReadTop::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
    assert_eq!(actual.0, Some(900.0), "current accepted native Find wins");
    memory.latest_source(Some(9), 11, false, false);
    assert!(
        !memory.preserve_arrived(None),
        "wrong refresh epoch cannot authorize replacement fallback"
    );
    memory.cancel_preserve(41);
    memory.latest_source(Some(10), 11, true, false);
    assert!(
        memory.preserve_arrived(None),
        "intermediate replacement also preserves reader"
    );
    memory.latest_source(None, 12, false, false);
    assert!(memory.preserve_arrived(None));
    assert!(!memory.preserve_arrived(Some("child")));
    element = make(Some("child"), old.clone(), 500, HashSet::new());
    tree.diff(element.as_widget_mut());
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut SetBottom(300.0),
    );
    let replacement = vec![
        "replacement-first",
        "replacement-middle",
        "replacement-last",
    ];
    element = make(
        None,
        replacement.clone(),
        20,
        replacement.iter().copied().collect(),
    );
    tree.diff(element.as_widget_mut());
    element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert!(memory.preserve_arrived(None));
    assert_eq!(
        memory
            .0
            .borrow()
            .contexts
            .iter()
            .find(|context| context.subagent.is_none())
            .unwrap()
            .top,
        Some(900.0),
        "cold replacement cannot overwrite the accepted Find geometry"
    );
    element = make(None, replacement, 500, HashSet::from(["replacement-first"]));
    tree.diff(element.as_widget_mut());
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut actual = ReadTop::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
    assert_eq!(
        actual.0,
        Some(900.0),
        "unrelated cold first row cannot block Ready replacement viewport"
    );
    assert!(
        !memory.preserve_arrived(None),
        "one arrived replacement consumes the bounded marker"
    );
    // With overlapping IDs, row anchoring takes precedence over numeric fallback.
    memory.clear();
    element = make(None, old.clone(), 500, HashSet::new());
    tree.diff(element.as_widget_mut());
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut SetBottom(500.0),
    );
    memory.preserve_refresh(43, 11);
    memory.latest_source(Some(11), 12, false, false);
    element = make(
        None,
        vec!["prefix", "first", "middle", "last"],
        500,
        HashSet::new(),
    );
    tree.diff(element.as_widget_mut());
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut actual = ReadTop::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
    assert_eq!(
        actual.0,
        Some(1100.0),
        "surviving row moves by prepend delta, not saved numeric top"
    );
    memory.preserve_refresh(44, 12);
    memory.cancel_preserve(44);
    memory.latest_source(Some(12), 13, false, false);
    assert!(
        !memory.preserve_arrived(None),
        "failed exact refresh cannot authorize later unrelated source"
    );
}

#[test]
fn cold_fitting_native_home_end_and_wheel_keep_explicit_context_intent() {
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 9,
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
    for context in [None, Some("child")] {
        for (edge, wheel_cancels) in [(Edge::Start, false), (Edge::End, false), (Edge::End, true)] {
            let memory = Memory::default();
            let make = |pending, height| {
                transcript_observed(
                    widget::column::with_children(
                        ["first", "middle", "last"]
                            .into_iter()
                            .map(|id| {
                                item(id, widget::Space::new().height(height).width(Length::Fill))
                            })
                            .collect::<Vec<_>>(),
                    ),
                    target,
                    context,
                    vec![],
                    true,
                    ["first", "middle", "last"]
                        .into_iter()
                        .map(|id| (item_id(id), id))
                        .collect(),
                    Observations::default(),
                    memory.clone(),
                    pending,
                    HashSet::new(),
                    None,
                )
            };
            {
                let mut element = make(false, 500);
                let mut tree = Tree::new(&element);
                let node = element
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut SetBottom(500.0),
                );
            }
            memory.request_edge(context, edge);
            let mut pending = make(true, 20);
            let mut tree = Tree::new(&pending);
            let node = pending
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            let mut snap = operation::scrollable::snap_to::<()>(
                card_control_id(target.card, "chat-transcript"),
                iced::widget::scrollable::RelativeOffset {
                    x: None,
                    y: Some(if edge == Edge::Start { 1.0 } else { 0.0 }),
                },
            );
            let mut before = ReadTop::default();
            pending
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
            pending
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut snap);
            let mut after = ReadTop::default();
            pending
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
            assert_eq!(
                (before.0, after.0),
                (Some(0.0), Some(0.0)),
                "fitting native operation has no translation change"
            );
            assert_eq!(memory.edge(context), Some(edge));
            assert!(tree.state.downcast_ref::<ReaderState>().revealing);
            if wheel_cancels {
                pending.as_widget_mut().update(
                    &mut tree,
                    &Event::Mouse(mouse::Event::WheelScrolled {
                        delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
                    }),
                    Layout::new(&node),
                    mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
                    &renderer,
                    &mut iced::advanced::clipboard::Null,
                    &mut Shell::new(&mut vec![]),
                    &Rectangle::with_size(Size::new(720.0, 400.0)),
                );
                assert_eq!(
                    memory.edge(context),
                    None,
                    "a later deliberate native wheel supersedes the pending edge"
                );
            }
            if !wheel_cancels {
                let mut fitting_ready = make(false, 20);
                tree.diff(fitting_ready.as_widget_mut());
                fitting_ready
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
                assert_eq!(
                    memory.edge(context),
                    Some(edge),
                    "authoritative fitting child/parent Ready keeps either explicit edge through later growth"
                );
            }
            let mut ready = make(false, 500);
            tree.diff(ready.as_widget_mut());
            let node = ready.as_widget_mut().layout(&mut tree, &renderer, &limits);
            let mut actual = ReadTop::default();
            ready
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
            assert_eq!(
                actual.0,
                Some(if wheel_cancels {
                    600.0
                } else if edge == Edge::Start {
                    0.0
                } else {
                    1100.0
                })
            );
            assert_eq!(memory.edge(context), None);
            memory.request_edge(context, Edge::End);
            let mut empty = transcript_observed(
                widget::Space::new().height(0).width(Length::Fill),
                target,
                context,
                vec![],
                true,
                vec![],
                Observations::default(),
                memory.clone(),
                false,
                HashSet::new(),
                None,
            );
            tree.diff(empty.as_widget_mut());
            empty.as_widget_mut().layout(&mut tree, &renderer, &limits);
            assert_eq!(
                memory.edge(context),
                None,
                "an authoritative truly empty transcript consumes the pending edge"
            );
            memory.request_edge(Some("child"), Edge::Start);
            memory.request_edge(None, Edge::End);
            assert_eq!(memory.edge(Some("child")), Some(Edge::Start));
            memory.clear();
            assert_eq!(memory.edge(Some("child")), None);
            assert_eq!(memory.edge(context), None);
        }
    }
}

#[test]
fn queued_edge_operation_refuses_new_generation_and_source_document() {
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 2,
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(720.0, 400.0));
    for changed_generation in [false, true] {
        let navigation = find_navigation::State::default();
        let mut queued = ApplyEdge {
            target,
            scope: TranscriptScope::default(),
            edge: Edge::Start,
            intent: navigation.guard(),
            document: navigation.document_epoch(),
        };
        let current = if changed_generation {
            Target {
                generation: target.generation + 1,
                ..target
            }
        } else {
            navigation.changed_document();
            target
        };
        let mut element = transcript_observed(
            item(
                "body",
                widget::Space::new().height(2000).width(Length::Fill),
            ),
            current,
            None,
            vec![],
            true,
            vec![(item_id("body"), "body")],
            Observations::default(),
            Memory::default(),
            false,
            HashSet::new(),
            None,
        );
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(500.0),
        );
        let mut before = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut queued);
        let mut after = ReadTop::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
        assert_eq!(before.0, Some(1100.0));
        assert_eq!(
            after.0, before.0,
            "a held typed edge task cannot scroll a newer target or source document"
        );
    }
}

fn native_logical_location(
    element: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &cosmic::Renderer,
    target: Target,
    identity: u64,
    byte: usize,
) -> Option<LogicalLocation> {
    let result = Arc::new(Mutex::new(None));
    let sink = result.clone();
    let mut locate = operation::map(
        LocateLogical {
            target,
            scroll: card_control_id(target.card, "chat-transcript"),
            item: item_id("opaque-item"),
            identity,
            byte,
            content: None,
            viewport: None,
            found: None,
            frames: Vec::new(),
            pending: None,
            horizontal: Vec::new(),
            shape_pending: None,
            scope: TranscriptScope::default(),
        },
        move |location| {
            *sink.lock().unwrap() = location;
        },
    );
    element
        .as_widget_mut()
        .operate(tree, Layout::new(node), renderer, &mut locate);
    let _ = locate.finish();
    result.lock().unwrap().clone()
}

#[test]
fn find_geometry_and_corrections_stay_in_the_exact_mounted_transcript() {
    #[derive(Default)]
    struct Offsets(HashMap<Id, Vector>);
    impl Operation for Offsets {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
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
                self.0.insert(id.clone(), translation);
            }
        }
    }
    let renderer = crate::app::tests::headless_renderer();
    let wanted = Target {
        card: CardId::new(),
        generation: 1,
    };
    let neighbor = Target {
        card: CardId::new(),
        generation: 1,
    };
    let identity = selection::logical_identity("opaque-item", &[0, 0]);
    let byte = "wanted line\n".len() * 1500;
    let wanted_state = find_navigation::State::default();
    let neighbor_state = find_navigation::State::default();
    let neighbor_guard = neighbor_state.guard();
    let horizontal_id = block_id(neighbor, None, identity);
    let wanted_body = prose::code_text(
        Arc::from("wanted line\n".repeat(5000)),
        None,
        iced::Pixels(14.0),
        iced::Font::MONOSPACE,
        wanted,
        None,
        Some(selectable_prose::Logical { identity, start: 0 }),
        None,
        None,
    );
    let owned_horizontal_id = block_id(wanted, None, identity);
    let wanted_body: Element<'_, Message> = widget::scrollable(widget::row::with_children(vec![
        widget::Space::new().width(1500).into(),
        wanted_body,
    ]))
    .direction(iced::widget::scrollable::Direction::Horizontal(
        design::scrollbar(),
    ))
    .id(owned_horizontal_id.clone())
    .width(Length::Fill)
    .into();
    let neighbor_body = widget::scrollable(widget::row::with_children(vec![
        widget::Space::new().width(3000).into(),
        selectable_prose::logical_text(
            vec![iced::widget::text::Span::new("neighbor glyph".to_owned())].into(),
            iced::Pixels(14.0),
            iced::Font::MONOSPACE,
            neighbor,
            None,
            selectable_prose::Logical {
                identity,
                start: byte,
            },
        ),
    ]))
    .direction(iced::widget::scrollable::Direction::Horizontal(
        design::scrollbar(),
    ))
    .id(horizontal_id.clone());
    let pane = |body, target, guard| {
        widget::container(transcript_observed(
            item("opaque-item", body),
            target,
            None,
            vec![],
            true,
            vec![],
            Observations::default(),
            Memory::default(),
            false,
            HashSet::new(),
            Some(guard),
        ))
        .width(400)
        .height(400)
        .into()
    };
    let mut element: Element<'_, Message> = widget::row::with_children(vec![
        pane(wanted_body, wanted, wanted_state.guard()),
        pane(neighbor_body.into(), neighbor, neighbor_guard.clone()),
    ])
    .into();
    let limits = layout::Limits::new(Size::ZERO, Size::new(800.0, 400.0));
    let mut tree = Tree::new(&element);
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let location = native_logical_location(
        &mut element,
        &mut tree,
        &node,
        &renderer,
        wanted,
        identity,
        byte,
    )
    .expect("wanted actual transcript geometry");
    assert!(
        !location.exact && location.stage == 1,
        "an unrelated mounted exact glyph cannot replace the wanted virtual descriptor"
    );
    assert!(
        location
            .horizontal
            .iter()
            .all(|(id, _)| *id != horizontal_id),
        "neighbor reader IDs never enter the wanted result"
    );
    let mut before = Offsets::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
    let mut defense = location.clone();
    defense.horizontal.push((horizontal_id.clone(), 1500.0));
    let mut apply = ApplyFind {
        target: wanted,
        scope: TranscriptScope::default(),
        scroll: card_control_id(wanted.card, "chat-transcript"),
        location: defense,
        intent: wanted_state.guard(),
        document: Some(wanted_state.document_epoch()),
    };
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut apply);
    let mut after = Offsets::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
    assert_ne!(
        before.0[&card_control_id(wanted.card, "chat-transcript")].y,
        after.0[&card_control_id(wanted.card, "chat-transcript")].y,
        "positive control changes the wanted native reader"
    );
    assert_eq!(
        before.0[&horizontal_id], after.0[&horizontal_id],
        "even a captured foreign reader ID cannot scroll a neighboring card"
    );
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut messages = Vec::new();
    element.as_widget_mut().update(
        &mut tree,
        &Event::Window(iced::window::Event::RedrawRequested(
            iced::time::Instant::now(),
        )),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &Rectangle::with_size(Size::new(800.0, 400.0)),
    );
    assert!(
        neighbor_guard.current(),
        "neighboring Find intent is unchanged"
    );
    assert!(
        !messages.iter().any(|message| matches!(message,
        Message::ChatUi(Action::Scrolled(target, _, _, Some(_))) if *target == neighbor)),
        "a correction cannot mark neighboring navigation as user input"
    );
    let mut own_marker = FindCorrection {
        target: Some(wanted),
        corrected: false,
    };
    let mut foreign_marker = FindCorrection {
        target: Some(neighbor),
        corrected: false,
    };
    <ApplyFind as Operation<()>>::custom(&mut apply, None, Rectangle::default(), &mut own_marker);
    <ApplyFind as Operation<()>>::custom(
        &mut apply,
        None,
        Rectangle::default(),
        &mut foreign_marker,
    );
    assert!(
        own_marker.corrected && !foreign_marker.corrected,
        "the pre-transcript correction marker is bound to its exact Target"
    );
    let stale_target = Target {
        generation: wanted.generation + 1,
        ..wanted
    };
    assert!(
        native_logical_location(
            &mut element,
            &mut tree,
            &node,
            &renderer,
            stale_target,
            identity,
            byte
        )
        .is_none(),
        "a stale-generation tree supplies no coarse, virtual or exact Find geometry"
    );
    let mut stale = ApplyFind {
        target: stale_target,
        scope: TranscriptScope::default(),
        scroll: card_control_id(wanted.card, "chat-transcript"),
        location: LogicalLocation {
            offset: 0.0,
            ..location
        },
        intent: wanted_state.guard(),
        document: Some(wanted_state.document_epoch()),
    };
    let mut before = Offsets::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut stale);
    let mut after = Offsets::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
    assert_eq!(
        before.0, after.0,
        "a stale-generation operation cannot mutate native readers"
    );
    let mut marker = FindCorrection {
        target: Some(wanted),
        corrected: false,
    };
    <ApplyFind as Operation<()>>::custom(&mut stale, None, Rectangle::default(), &mut marker);
    assert!(
        !marker.corrected,
        "a stale-generation operation cannot mark a correction"
    );
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    use iced::futures::StreamExt;
    let mut task = iced::runtime::task::into_stream(find_anchor(
        stale_target,
        item_id("opaque-item"),
        wanted_state.guard(),
    ))
    .unwrap();
    let effect = executor.block_on(async {
        tokio::time::timeout(Duration::from_secs(1), task.next())
            .await
            .unwrap()
            .unwrap()
    });
    let iced::runtime::Action::Widget(mut coarse) = effect else {
        panic!("native anchor operation");
    };
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, coarse.as_mut());
    assert!(
        matches!(coarse.finish(), operation::Outcome::None),
        "a stale-generation coarse anchor cannot queue a scroll correction"
    );
    executor.shutdown_background();
    for (offset, horizontal) in [
        (f32::INFINITY, None),
        (f32::NEG_INFINITY, None),
        (f32::NAN, None),
        (0.0, Some(f32::INFINITY)),
        (0.0, Some(f32::NEG_INFINITY)),
        (0.0, Some(f32::NAN)),
    ] {
        let mut invalid = ApplyFind {
            target: wanted,
            scope: TranscriptScope::default(),
            scroll: card_control_id(wanted.card, "chat-transcript"),
            location: LogicalLocation {
                offset,
                exact: false,
                stage: 0,
                shape_pending: None,
                horizontal: horizontal
                    .map(|x| vec![(owned_horizontal_id.clone(), x)])
                    .unwrap_or_default(),
            },
            intent: wanted_state.guard(),
            document: Some(wanted_state.document_epoch()),
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut invalid);
        let mut rejected = Offsets::default();
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut rejected);
        assert_eq!(
            after.0, rejected.0,
            "nonfinite Find offsets cannot mutate a native reader"
        );
        let mut marker = FindCorrection {
            target: Some(wanted),
            corrected: false,
        };
        <ApplyFind as Operation<()>>::custom(&mut invalid, None, Rectangle::default(), &mut marker);
        assert!(!marker.corrected);
    }
}

#[test]
fn appended_flow_prefix_is_not_exact_find_geometry_before_redraw_requests_current_source() {
    fn paragraph(content: &markdown::Content) -> markdown::Text {
        let markdown::Item::Paragraph(text) = &content.items()[0] else {
            panic!("paragraph");
        };
        text.clone()
    }
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 1,
    };
    let identity = selection::logical_identity("opaque-item", &[0, 0]);
    let style = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE).style;
    let mut content = markdown::Content::parse(&"prefix needle words ".repeat(300));
    let cache = flow_layout::Cache::default();
    let entry = cache.markdown_entry(identity, paragraph(&content), style);
    let make = |entry| {
        transcript(
            item(
                "opaque-item",
                selectable_prose::flow_source(
                    iced::Pixels(14.0),
                    iced::Font::default(),
                    target,
                    None,
                    selectable_prose::Logical { identity, start: 0 },
                    entry,
                ),
            ),
            target,
            None,
            vec![],
            true,
            vec![],
        )
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
    let mut element = make(entry.clone());
    let mut tree = Tree::new(&element);
    let _ = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert!(
        cache.complete_fixture(),
        "initial real geometry fixture completed"
    );
    element = make(entry.clone());
    tree.diff(element.as_widget_mut());
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let ready = native_logical_location(
        &mut element,
        &mut tree,
        &node,
        &renderer,
        target,
        identity,
        7,
    )
    .expect("actual transcript geometry");
    assert!(ready.exact && ready.shape_pending.is_none());
    content.push_str(" appended words reflow the current source");
    let current = cache.markdown_entry(identity, paragraph(&content), style);
    assert!(Rc::ptr_eq(&entry, &current));
    element = make(current.clone());
    tree.diff(element.as_widget_mut());
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    // No Redraw/update has requested the new revision. The old prefix glyph
    // is genuinely mounted, but it cannot finish the current Find intent.
    let pending = native_logical_location(
        &mut element,
        &mut tree,
        &node,
        &renderer,
        target,
        identity,
        7,
    )
    .expect("actual transcript geometry");
    assert_eq!(
        pending.stage, 3,
        "old prefix glyph remains available for display"
    );
    assert!(
        pending.shape_pending.is_some() && !pending.exact,
        "pre-redraw old prefix geometry cannot certify the current source"
    );
    assert!(
        cache.complete_fixture(),
        "the current revision still needs a real request"
    );
    element = make(current);
    tree.diff(element.as_widget_mut());
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let exact = native_logical_location(
        &mut element,
        &mut tree,
        &node,
        &renderer,
        target,
        identity,
        7,
    )
    .expect("actual transcript geometry");
    assert!(exact.exact && exact.shape_pending.is_none());
}

#[test]
fn queued_find_operations_check_live_intent_and_document_before_native_scroll() {
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 1,
    };
    let state = find_navigation::State::default();
    let guard = state.guard();
    let epoch = state.document_epoch();
    let location = LogicalLocation {
        offset: 600.0,
        exact: true,
        stage: 3,
        shape_pending: None,
        horizontal: Vec::new(),
    };
    let mut element = transcript(
        widget::Space::new().height(5000).width(Length::Fill),
        target,
        None,
        vec![],
        true,
        vec![],
    );
    let mut tree = Tree::new(&element);
    let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut apply = ApplyFind {
        target,
        scope: TranscriptScope::default(),
        scroll: card_control_id(target.card, "chat-transcript"),
        location: location.clone(),
        intent: guard.clone(),
        document: Some(epoch),
    };
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut apply);
    let mut before = ReadTop::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut before);
    assert!(
        before.0.is_some_and(|top| top > 0.0),
        "positive control actually changed native scroll"
    );
    state.changed_document();
    let mut stale = ApplyFind {
        target,
        scope: TranscriptScope::default(),
        scroll: card_control_id(target.card, "chat-transcript"),
        location: LogicalLocation {
            offset: 0.0,
            ..location.clone()
        },
        intent: guard.clone(),
        document: Some(epoch),
    };
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut stale);
    let mut after = ReadTop::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
    assert_eq!(
        before.0, after.0,
        "document changed after task creation rejects actual mutation"
    );
    guard.cancel();
    let mut canceled = ApplyFind {
        target,
        scope: TranscriptScope::default(),
        scroll: card_control_id(target.card, "chat-transcript"),
        location: LogicalLocation {
            offset: 0.0,
            ..location
        },
        intent: guard,
        document: None,
    };
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut canceled);
    let mut after = ReadTop::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut after);
    assert_eq!(
        before.0, after.0,
        "even a coarse queued operation cannot override newer input"
    );
}

#[test]
fn find_corrections_preserve_their_intent_but_native_nested_wheels_cancel_it() {
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 1,
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
    let viewport = Rectangle::with_size(Size::new(400.0, 400.0));
    for nested in [false, true] {
        let state = find_navigation::State::default();
        let guard = state.guard();
        let content: Element<'_, Message> = if nested {
            widget::scrollable(widget::Space::new().height(10_000).width(400))
                .height(200)
                .into()
        } else {
            widget::Space::new().height(10_000).width(400).into()
        };
        let mut element = transcript_observed(
            content,
            target,
            None,
            vec![],
            true,
            vec![],
            Observations::default(),
            Memory::default(),
            false,
            HashSet::new(),
            Some(guard.clone()),
        );
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut apply = ApplyFind {
            target,
            scope: TranscriptScope::default(),
            scroll: card_control_id(target.card, "chat-transcript"),
            location: LogicalLocation {
                offset: 600.0,
                exact: true,
                stage: 3,
                shape_pending: None,
                horizontal: Vec::new(),
            },
            intent: guard.clone(),
            document: Some(state.document_epoch()),
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut apply);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &Event::Window(iced::window::Event::RedrawRequested(
                iced::time::Instant::now(),
            )),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert!(guard.current());
        assert!(
            !messages.iter().any(|message| matches!(
                message,
                Message::ChatUi(Action::Scrolled(_, _, _, Some(_)))
            )),
            "Find's own correction must not claim user input"
        );
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
            }),
            Layout::new(&node),
            mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert!(
            !guard.current(),
            "a wheel cancels before either outer or nested reader consumes it"
        );
    }
}

#[test]
fn logical_find_realizes_a_distant_chunk_and_centers_its_mounted_glyph() {
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let identity = 9876;
    let line = "measured source line αβ\n";
    let source: Arc<str> = line.repeat(5000).into();
    let byte = line.len() * 1500;
    let text = prose::code_text(
        source,
        None,
        design::TEXT_BODY.into(),
        iced::Font::MONOSPACE,
        target,
        None,
        Some(selectable_prose::Logical { identity, start: 0 }),
        None,
        None,
    );
    let mut cases = vec![(text, identity, byte)];
    let list = format!(
        "{}{}",
        "prefix\n\n".repeat(4),
        (0..10000)
            .map(|index| format!("- bullet {index} target\n"))
            .collect::<String>()
    );
    let table = format!(
        "{}| First | Second |\n| --- | --- |\n{}",
        "prefix\n\n".repeat(4),
        (0..10000)
            .map(|index| format!("| row {index} | target {index} |\n"))
            .collect::<String>()
    );
    for (source, path) in [
        (list, vec![0, 4, 9999, 0]),
        (table, vec![0, 4, 10000, 1, 0]),
    ] {
        let cached = CachedItem::new(
            &ItemContent::Text {
                text: source,
                thinking: false,
                phase: chat::Phase::Final,
            },
            crate::settings::EditorSyntaxTheme::Auto,
        );
        let identity = selection::logical_identity(&format!("unbound-{}", cached.revision), &path);
        cases.push((
            prose::view(
                &cached,
                design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE),
                target,
                None,
            ),
            identity,
            0,
        ));
    }
    for (text, identity, byte) in cases {
        let before = virtualized::constructed_rows();
        let mut element = transcript(
            widget::column::with_children(vec![
                widget::Space::new().height(10000).into(),
                item("answer", text),
                widget::Space::new().height(10000).into(),
            ]),
            target,
            None,
            vec![],
            true,
            vec![],
        );
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(720.0, 400.0));
        let limits = layout::Limits::new(Size::ZERO, viewport.size());
        let mut node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(f32::MAX),
        );
        node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut exact = false;
        for _ in 0..6 {
            let found = Arc::new(std::sync::Mutex::new(None));
            let sink = found.clone();
            let mut locate = operation::map(
                LocateLogical {
                    target,
                    scroll: card_control_id(target.card, "chat-transcript"),
                    item: item_id("answer"),
                    identity,
                    byte,
                    content: None,
                    viewport: None,
                    found: None,
                    frames: Vec::new(),
                    pending: None,
                    horizontal: Vec::new(),
                    shape_pending: None,
                    scope: TranscriptScope::default(),
                },
                move |location| {
                    *sink.lock().unwrap() = location;
                },
            );
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut locate);
            let _ = locate.finish();
            let location = found
                .lock()
                .unwrap()
                .take()
                .expect("offscreen descriptors remain navigable");
            exact = location.exact;
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut SetBottom(location.offset),
            );
            node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            if exact {
                break;
            }
        }
        assert!(
            exact,
            "estimate navigation converges to actual mounted glyph geometry"
        );
        struct Visible {
            scroll: Id,
            identity: u64,
            byte: usize,
            translation: Vector,
            pending: Option<Vector>,
            viewport: Option<Rectangle>,
            glyph: Option<Rectangle>,
        }
        impl Operation for Visible {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                let before = self.translation;
                if let Some(translation) = self.pending.take() {
                    self.translation += translation;
                }
                visit(self);
                self.translation = before;
            }
            fn scrollable(
                &mut self,
                id: Option<&Id>,
                viewport: Rectangle,
                _: Rectangle,
                translation: Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                if id == Some(&self.scroll) {
                    self.viewport = Some(viewport);
                }
                self.pending = Some(translation);
            }
            fn custom(&mut self, _: Option<&Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
                if let Some(prose) = state.downcast_ref::<selectable_prose::State>()
                    && let Some(local) = prose.logical_byte_bounds(self.identity, self.byte)
                {
                    self.glyph = Some(Rectangle {
                        x: bounds.x + local.x - self.translation.x,
                        y: bounds.y + local.y - self.translation.y,
                        ..local
                    });
                }
            }
        }
        let mut visible = Visible {
            scroll: card_control_id(target.card, "chat-transcript"),
            identity,
            byte,
            translation: Vector::ZERO,
            pending: None,
            viewport: None,
            glyph: None,
        };
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut visible);
        let glyph = visible.glyph.expect("the requested chunk is mounted");
        let viewport = visible.viewport.unwrap();
        assert!(glyph.y >= viewport.y && glyph.y + glyph.height <= viewport.y + viewport.height);
        assert!(
            (glyph.y + glyph.height / 2.0 - viewport.y - viewport.height / 2.0).abs()
                <= glyph.height + 1.0,
            "mounted match glyph is centered: {glyph:?}, {viewport:?}"
        );
        assert!(
            virtualized::constructed_rows() - before < 1500,
            "staged navigation must not instantiate ten thousand hidden rows"
        );
    }
}

#[test]
fn native_geometry_updates_the_reader_without_echoing_a_model_message() {
    let renderer = crate::app::tests::headless_renderer();
    let observations: Observations = Default::default();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let make = |height| {
        transcript_observed(
            widget::Space::new().height(height).width(Length::Fill),
            target,
            None,
            vec![],
            true,
            vec![],
            observations.clone(),
            Memory::default(),
            false,
            HashSet::new(),
            None,
        )
    };
    let bounds = Rectangle::with_size(Size::new(400.0, 400.0));
    let limits = layout::Limits::new(Size::ZERO, bounds.size());
    let mut element = make(100.0);
    let mut tree = Tree::new(&element);
    let mut messages = Vec::new();
    for height in [100.0, 2000.0, 4000.0] {
        element = make(height);
        tree.diff(element.as_widget_mut());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut iced::advanced::clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert!(
            messages.is_empty(),
            "streamed geometry does not enqueue another model/view cycle"
        );
        let scroll = observations.borrow()[&None];
        assert_eq!(scroll.content(), height);
        assert!(
            scroll.at_bottom(),
            "the direct observation keeps authoritative native geometry"
        );
    }
    let node = element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
        }),
        Layout::new(&node),
        mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(
        messages.is_empty(),
        "leaving the end updates direct observations without paging"
    );
    assert!(
        !observations.borrow()[&None].at_bottom(),
        "the real wheel moved away from the end"
    );
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
        }),
        Layout::new(&node),
        mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(
        messages.iter().any(|message| matches!(
            message,
            Message::ChatUi(Action::Scrolled(_, None, _, Some(_)))
        )),
        "wheel navigation toward the history edge still drives paging"
    );
}

#[test]
fn native_layout_reports_fitting_content_and_anchors_real_column_children() {
    use iced::advanced::renderer::Headless;
    let renderer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(cosmic::Renderer::new(
            iced::Font::default(),
            iced::Pixels(14.0),
            Some("tiny-skia"),
        ))
        .unwrap();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let element = |entries: Vec<(&'static str, f32)>| {
        let anchors = entries
            .iter()
            .enumerate()
            .map(|(index, (id, _))| (index, *id))
            .collect();
        let children: Vec<Element<'static, Message>> = entries
            .into_iter()
            .map(|(_, height)| {
                widget::Space::new()
                    .width(Length::Fill)
                    .height(height)
                    .into()
            })
            .collect();
        transcript(
            widget::column::with_children(children).width(Length::Fill),
            target,
            None,
            anchors,
            true,
            Vec::new(),
        )
    };
    let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(400.0, 400.0));
    let limits = layout::Limits::new(Size::ZERO, bounds.size());
    let mut fitting = element(vec![("short", 100.0)]);
    let mut tree = Tree::new(fitting.as_widget());
    let node = fitting
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut messages = Vec::new();
    let event = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
    fitting.as_widget_mut().update(
        &mut tree,
        &event,
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(
        matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(actual, None, scroll, None))]
        if *actual == target && *scroll == Scroll::new(0.0, 100.0, 400.0))
    );

    messages.clear();
    fitting.as_widget_mut().update(
        &mut tree,
        &event,
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(
        messages.is_empty(),
        "unchanged geometry must not trigger automatic paging loops"
    );
    fitting.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
        }),
        Layout::new(&node),
        mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(
        matches!(
            messages.as_slice(),
            [Message::ChatUi(Action::Scrolled(_, None, _, Some(_)))]
        ),
        "a wheel gesture at a pinned edge must permit retry"
    );

    let mut initial = element(vec![("a", 500.0), ("b", 500.0), ("c", 1000.0)]);
    let mut tree = Tree::new(initial.as_widget());
    let node = initial
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    initial.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut SetBottom(1000.0),
    );
    initial.as_widget_mut().update(
        &mut tree,
        &event,
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    messages.clear();
    let mut updated = element(vec![
        ("older", 300.0),
        ("a", 500.0),
        ("b", 500.0),
        ("c", 1200.0),
    ]);
    tree.diff(updated.as_widget_mut());
    let node = updated
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let mut measured = ReadTop::default();
    updated
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut measured);
    assert_eq!(measured.0, Some(900.0));
    messages.clear();
    updated.as_widget_mut().update(
        &mut tree,
        &event,
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(
        matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(_, None, scroll, None))]
        if *scroll == Scroll::new(900.0, 2500.0, 400.0))
    );

    // Equal-height replacement changes the offset through layout, not
    // through a gesture. Treating it as input bounces older pages to live.
    let mut replaced = element(vec![("d", 500.0), ("e", 500.0), ("f", 1500.0)]);
    tree.diff(replaced.as_widget_mut());
    let node = replaced
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    messages.clear();
    replaced.as_widget_mut().update(
        &mut tree,
        &event,
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(
        matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(_, None, scroll, None))]
        if *scroll == Scroll::new(2100.0, 2500.0, 400.0))
    );
    let mut home = operation::scrollable::snap_to::<()>(
        card_control_id(target.card, "chat-transcript"),
        iced::widget::scrollable::RelativeOffset {
            x: None,
            y: Some(1.0),
        },
    );
    replaced
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut home);
    messages.clear();
    replaced.as_widget_mut().update(
        &mut tree,
        &event,
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(
        matches!(messages.as_slice(), [Message::ChatUi(Action::Scrolled(_, None, scroll, Some(_)))]
        if *scroll == Scroll::new(0.0, 2500.0, 400.0)),
        "Home's external operation is input"
    );
    messages.clear();
    replaced.as_widget_mut().update(
        &mut tree,
        &event,
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(messages.is_empty(), "input signal must be consumed once");
    replaced.as_widget_mut().update(
        &mut tree,
        &Event::Keyboard(iced::keyboard::Event::ModifiersChanged(
            iced::keyboard::Modifiers::SHIFT,
        )),
        Layout::new(&node),
        mouse::Cursor::Unavailable,
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    replaced.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
        }),
        Layout::new(&node),
        mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
        &renderer,
        &mut iced::advanced::clipboard::Null,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(
        messages.is_empty(),
        "Shift-wheel remaps to horizontal input and must not page"
    );
}

#[test]
fn nested_output_consumes_wheel_without_paging_transcript() {
    use iced::advanced::renderer::Headless;
    let renderer = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(cosmic::Renderer::new(
            iced::Font::default(),
            iced::Pixels(14.0),
            Some("tiny-skia"),
        ))
        .unwrap();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let nested = tool_output(
        widget::scrollable(widget::Space::new().height(1000.0))
            .height(200.0)
            .width(Length::Fill)
            .anchor_bottom(),
    );
    let mut element = transcript(
        widget::column::with_children(vec![nested, widget::Space::new().height(1400.0).into()])
            .width(Length::Fill),
        target,
        None,
        vec![],
        true,
        Vec::new(),
    );
    let mut tree = Tree::new(element.as_widget());
    let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(400.0, 400.0));
    let node = element.as_widget_mut().layout(
        &mut tree,
        &renderer,
        &layout::Limits::new(Size::ZERO, bounds.size()),
    );
    element.as_widget_mut().operate(
        &mut tree,
        Layout::new(&node),
        &renderer,
        &mut SetBottom(1200.0),
    );
    let mut messages = Vec::new();
    let mut clipboard = iced::advanced::clipboard::Null;
    // Consume the deliberate outer navigation before wheeling its child.
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::CursorMoved {
            position: iced::Point::new(50.0, 50.0),
        }),
        Layout::new(&node),
        mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
        &renderer,
        &mut clipboard,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    messages.clear();
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
        }),
        Layout::new(&node),
        mouse::Cursor::Available(iced::Point::new(50.0, 50.0)),
        &renderer,
        &mut clipboard,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(
        messages.is_empty(),
        "output scroll must not become outer pagination: {messages:?}"
    );
    let mut measured = ReadTop::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), &renderer, &mut measured);
    assert_eq!(measured.0, Some(0.0));
    // The same upward gesture over plain transcript is a retry at its edge.
    element.as_widget_mut().update(
        &mut tree,
        &Event::Mouse(mouse::Event::WheelScrolled {
            delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
        }),
        Layout::new(&node),
        mouse::Cursor::Available(iced::Point::new(50.0, 300.0)),
        &renderer,
        &mut clipboard,
        &mut Shell::new(&mut messages),
        &bounds,
    );
    assert!(matches!(
        messages.as_slice(),
        [Message::ChatUi(Action::Scrolled(_, None, _, Some(_)))]
    ));
}

#[test]
fn wheel_navigation_respects_direction_and_nested_capture() {
    let top = Scroll::new(10.0, 1000.0, 400.0);
    assert!(navigation_input(Some(20.0), 10.0, top, Some(1.0), false).is_some());
    assert!(navigation_input(Some(0.0), 10.0, top, Some(-1.0), false).is_none());
    assert!(navigation_input(Some(10.0), 10.0, top, Some(1.0), true).is_none());
    let bottom = Scroll::new(590.0, 1000.0, 400.0);
    assert!(navigation_input(Some(580.0), 590.0, bottom, Some(-1.0), false).is_some());
    assert!(navigation_input(Some(600.0), 590.0, bottom, Some(1.0), false).is_none());
    assert!(navigation_input(Some(590.0), 590.0, bottom, Some(-1.0), true).is_none());
    assert!(navigation_input(Some(100.0), 200.0, bottom, None, false).is_some());
    assert!(navigation_input(Some(10.0), 10.0, top, Some(0.0), false).is_none());
}

/// A wheel heading for a loaded edge asks for its page a couple of screens
/// ahead of it, so the page is there when the reader is; reading elsewhere
/// asks for nothing (#3102).
#[test]
fn wheel_toward_a_loaded_edge_fetches_ahead_of_the_reader() {
    let near_top = Scroll::new(600.0, 10_000.0, 800.0);
    assert!(!near_top.at_top() && near_top.prefetch_earlier());
    assert_eq!(
        navigation_input(Some(660.0), 600.0, near_top, Some(1.0), false),
        Some(view::Page::Earlier)
    );
    // Wheeling away from that edge is reading.
    assert_eq!(
        navigation_input(Some(540.0), 600.0, near_top, Some(-1.0), false),
        None
    );
    let middle = Scroll::new(4_600.0, 10_000.0, 800.0);
    assert_eq!(
        navigation_input(Some(4_660.0), 4_600.0, middle, Some(1.0), false),
        None
    );
    let near_bottom = Scroll::new(8_600.0, 10_000.0, 800.0);
    assert_eq!(
        navigation_input(Some(8_540.0), 8_600.0, near_bottom, Some(-1.0), false),
        Some(view::Page::Newer)
    );
}

#[test]
fn fitting_geometry_keeps_wheel_direction_without_inventing_input() {
    let fitting = Scroll::new(0.0, 200.0, 400.0);
    assert!(fitting.at_top() && fitting.at_bottom());
    assert_eq!(
        navigation_input(Some(0.0), 0.0, fitting, Some(1.0), false),
        Some(view::Page::Earlier)
    );
    assert_eq!(
        navigation_input(Some(0.0), 0.0, fitting, Some(-1.0), false),
        Some(view::Page::Newer)
    );
    for wheel in [None, Some(0.0)] {
        assert_eq!(
            navigation_input(Some(0.0), 0.0, fitting, wheel, false),
            None
        );
    }
    assert_eq!(
        navigation_input(Some(0.0), 0.0, fitting, Some(-1.0), true),
        None
    );
}

#[test]
fn thumb_touch_and_keyboard_moves_away_from_edges_do_not_page() {
    let top = Scroll::new(10.0, 1000.0, 400.0);
    assert!(navigation_input(Some(0.0), 10.0, top, None, false).is_none());
    assert!(navigation_input(Some(20.0), 10.0, top, None, false).is_some());
    let bottom = Scroll::new(590.0, 1000.0, 400.0);
    assert!(navigation_input(Some(600.0), 590.0, bottom, None, false).is_none());
    assert!(navigation_input(Some(580.0), 590.0, bottom, None, false).is_some());
}

#[test]
fn fitting_page_preserves_exchange_when_both_ends_grow() {
    let mut anchor = Anchor::default();
    anchor.layout(None, &rows(&[("short", 0.0, 300.0)]), geometry(300.0), None);
    let next = rows(&[
        ("older", 0.0, 500.0),
        ("short", 500.0, 300.0),
        ("newer", 800.0, 600.0),
    ]);
    assert_eq!(
        anchor.layout(None, &next, geometry(1400.0), Some(0.0)),
        Some(500.0)
    );
    assert_eq!(anchor.contexts[0].top, Some(500.0));
}

#[test]
fn simultaneous_prepend_and_streaming_append_preserve_visible_exchange() {
    let next = rows(&[
        ("older", 0.0, 300.0),
        ("a", 300.0, 500.0),
        ("b", 800.0, 500.0),
        ("c", 1300.0, 1200.0),
    ]);
    assert_eq!(
        reading().layout(None, &next, geometry(2500.0), Some(600.0)),
        Some(1200.0)
    );
}

#[test]
fn prepend_at_home_freezes_the_previous_first_exchange() {
    let mut anchor = reading();
    let next = rows(&[
        ("older", 0.0, 300.0),
        ("a", 300.0, 500.0),
        ("b", 800.0, 500.0),
        ("c", 1300.0, 1000.0),
    ]);
    assert_eq!(
        anchor.layout(None, &next, geometry(2300.0), Some(0.0)),
        Some(1600.0)
    );
    assert_eq!(
        anchor.layout(None, &next, geometry(2300.0), Some(300.0)),
        None
    );
}

#[test]
fn growth_above_and_front_eviction_preserve_surviving_rows() {
    let next = rows(&[
        ("a", 0.0, 800.0),
        ("b", 800.0, 500.0),
        ("c", 1300.0, 1000.0),
    ]);
    assert_eq!(
        reading().layout(None, &next, geometry(2300.0), Some(600.0)),
        Some(1000.0)
    );
    let next = rows(&[("b", 0.0, 500.0), ("c", 500.0, 1000.0)]);
    assert_eq!(
        reading().layout(None, &next, geometry(1500.0), Some(600.0)),
        Some(1000.0)
    );
    assert_eq!(
        reading().layout(None, &next, geometry(1500.0), Some(100.0)),
        Some(1100.0)
    );
}

#[test]
fn following_reader_stays_at_the_end_when_a_long_answer_window_slides() {
    // A streaming answer longer than one transcript window is presented
    // as parts. Parts outside the mounted range report their group's
    // placeholder bounds, so the first remembered row can be a later part
    // at the group's top.
    let mut anchor = Anchor::default();
    let parts = rows(&[
        ("part-9", 0.0, 9000.0),
        ("part-1", 0.0, 9000.0),
        ("part-24", 8600.0, 400.0),
    ]);
    anchor.layout(None, &parts, geometry(9000.0), None);
    assert_eq!(anchor.contexts[0].top, Some(8600.0));
    // The window evicts the first part. The later part is now measured
    // below its old placeholder, which is no history above the reader.
    let slid = rows(&[
        ("part-2", 0.0, 400.0),
        ("part-9", 2800.0, 400.0),
        ("part-25", 8500.0, 300.0),
    ]);
    assert_eq!(
        anchor.layout(None, &slid, geometry(8800.0), Some(8600.0)),
        None
    );
    assert_eq!(anchor.contexts[0].top, Some(8400.0));
}

#[test]
fn viewport_resize_and_live_end_use_the_current_native_offset() {
    let next = rows(&[
        ("a", 0.0, 500.0),
        ("b", 500.0, 500.0),
        ("c", 1000.0, 1200.0),
    ]);
    let mut resized = geometry(2200.0);
    resized.viewport = Size::new(300.0, 600.0);
    assert_eq!(
        reading().layout(None, &next, resized, Some(900.0)),
        Some(700.0)
    );
    assert_eq!(reading().layout(None, &next, resized, Some(1600.0)), None);
}

#[test]
fn shortened_exchange_clamps_its_internal_offset() {
    let next = rows(&[("a", 0.0, 500.0), ("b", 500.0, 50.0), ("c", 550.0, 1000.0)]);
    assert_eq!(
        reading().layout(None, &next, geometry(1550.0), Some(800.0)),
        Some(600.0)
    );
}

#[test]
fn separate_transcripts_restore_positions_and_have_deliberate_initial_edges() {
    let mut anchor = reading();
    let child = rows(&[("child", 0.0, 1600.0)]);
    assert_eq!(
        anchor.layout(Some("worker"), &child, geometry(1600.0), Some(600.0)),
        Some(1200.0)
    );
    let main = rows(&[
        ("a", 0.0, 500.0),
        ("b", 500.0, 500.0),
        ("c", 1000.0, 1200.0),
    ]);
    assert_eq!(
        anchor.layout(None, &main, geometry(2200.0), Some(300.0)),
        Some(1200.0)
    );
    assert_eq!(
        anchor.layout(Some("worker"), &child, geometry(1600.0), Some(600.0)),
        Some(900.0)
    );
}

#[test]
fn short_subagent_content_preserves_its_initial_top_when_it_grows() {
    let mut anchor = Anchor::default();
    let initial = rows(&[("child", 0.0, 100.0)]);
    assert_eq!(
        anchor.layout(Some("worker"), &initial, geometry(100.0), None),
        Some(0.0)
    );
    let grown = rows(&[("child", 0.0, 1600.0)]);
    assert_eq!(
        anchor.layout(Some("worker"), &grown, geometry(1600.0), Some(0.0)),
        Some(1200.0)
    );
    assert_eq!(
        anchor.layout(
            Some("worker"),
            &rows(&[("child", 0.0, 1800.0)]),
            geometry(1800.0),
            Some(1200.0)
        ),
        None,
        "End explicitly follows the live end after content becomes scrollable"
    );
}

#[test]
fn bounded_contexts_keep_main_even_after_many_workers() {
    let mut anchor = reading();
    let child = rows(&[("child", 0.0, 1600.0)]);
    for i in 0..64 {
        anchor.layout(
            Some(&format!("worker-{i}")),
            &child,
            geometry(1600.0),
            Some(600.0),
        );
    }
    assert_eq!(anchor.contexts.len(), 33);
    let main = rows(&[
        ("a", 0.0, 500.0),
        ("b", 500.0, 500.0),
        ("c", 1000.0, 1000.0),
    ]);
    assert_eq!(
        anchor.layout(None, &main, geometry(2000.0), Some(300.0)),
        Some(1000.0)
    );
}

#[test]
fn tool_output_without_message_anchors_preserves_reading_offset() {
    let mut anchor = Anchor::default();
    assert_eq!(anchor.layout(None, &[], geometry(2000.0), None), Some(0.0));
    assert_eq!(
        anchor.layout(None, &[], geometry(2200.0), Some(600.0)),
        Some(1200.0)
    );
    assert_eq!(
        anchor.layout(None, &[], geometry(2400.0), Some(1800.0)),
        None
    );
}

#[test]
fn rebuilt_named_anchors_match_the_active_widget_id_hash_map() {
    // This uses Iced's selected ID implementation, including its a11y
    // reexport, rather than an unused feature-gated copy of the type.
    let first = item_id("persisted-answer");
    let rebuilt = item_id("persisted-answer");
    assert_eq!(first, rebuilt);
    let mut anchors = std::collections::HashMap::new();
    anchors.insert(first, 300.0);
    assert_eq!(anchors.get(&rebuilt), Some(&300.0));
}

#[test]
fn regrouped_answer_and_tool_keep_their_measured_reading_position() {
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let element = |entries: Vec<(&'static str, f32)>, group| {
        let ids = entries.iter().map(|(id, _)| (item_id(id), *id)).collect();
        let children: Vec<Element<'static, Message>> = entries
            .into_iter()
            .map(|(id, height)| item(id, widget::Space::new().width(Length::Fill).height(height)))
            .collect();
        transcript(
            widget::container(widget::column::with_children(children)).id(Id::new(group)),
            target,
            None,
            Vec::new(),
            false,
            ids,
        )
    };
    let limits = layout::Limits::new(Size::ZERO, Size::new(400.0, 400.0));
    for top in [450.0, 850.0] {
        let mut initial = element(
            vec![("prompt", 300.0), ("answer", 500.0), ("tool", 600.0)],
            "old-group",
        );
        let mut tree = Tree::new(&initial);
        let node = initial
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        initial.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut SetBottom(1000.0 - top),
        );
        // A second layout captures the native reading position before the
        // prompt is evicted and the answer becomes the group's first row.
        initial
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let memory = tree.state.downcast_ref::<ReaderState>().memory.0.borrow();
        let context = &memory.contexts[0];
        assert_eq!(
            context
                .rows
                .iter()
                .find(|row| row.id == "answer")
                .map(|row| row.bounds.y),
            Some(300.0)
        );
        drop(memory);
        let mut next = element(
            vec![("answer", 500.0), ("tool", 600.0), ("newer", 1000.0)],
            "new-group",
        );
        tree.diff(next.as_widget_mut());
        let node = next.as_widget_mut().layout(&mut tree, &renderer, &limits);
        let mut actual = ReadTop::default();
        next.as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut actual);
        let memory = tree.state.downcast_ref::<ReaderState>().memory.0.borrow();
        let context = &memory.contexts[0];
        assert_eq!(
            actual.0,
            Some(top - 300.0),
            "rows={:?}, geometry={:?}, remembered={:?}",
            context
                .rows
                .iter()
                .map(|row| (&row.id, row.bounds))
                .collect::<Vec<_>>(),
            context.geometry,
            context.top
        );
    }
}

#[test]
fn forward_history_preserves_boundary_instead_of_following_new_page_end() {
    let mut anchor = Anchor::default();
    let previous = rows(&[("older", 0.0, 800.0), ("boundary", 800.0, 500.0)]);
    anchor.layout_measured(None, &previous, geometry(1300.0), None, false);
    let next = rows(&[("boundary", 0.0, 500.0), ("newer", 500.0, 1200.0)]);
    assert_eq!(
        anchor.layout_measured(None, &next, geometry(1700.0), Some(900.0), true),
        Some(1200.0)
    );
    assert_eq!(anchor.contexts[0].top, Some(100.0));
}

#[test]
fn backward_history_from_live_bottom_preserves_overlapping_exchange() {
    let mut anchor = Anchor::default();
    let previous = rows(&[("boundary", 0.0, 500.0), ("live", 500.0, 700.0)]);
    anchor.layout_measured(None, &previous, geometry(1200.0), None, true);
    let next = rows(&[("older", 0.0, 700.0), ("boundary", 700.0, 500.0)]);
    assert_eq!(
        anchor.layout_measured(None, &next, geometry(1200.0), Some(0.0), false),
        Some(100.0)
    );
    assert_eq!(anchor.contexts[0].top, Some(700.0));
}

#[test]
fn entirely_replaced_window_starts_at_its_newest_boundary() {
    assert_eq!(
        reading().layout(
            None,
            &rows(&[("older", 0.0, 900.0)]),
            geometry(900.0),
            Some(0.0)
        ),
        Some(0.0)
    );
}
