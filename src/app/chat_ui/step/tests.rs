use super::*;
use iced::advanced::{
    Layout, Shell, clipboard, layout, mouse,
    text::Paragraph as _,
    widget::{Operation, Tree, operation, tree},
};
use iced::{Event, Point, Rectangle, Size, Vector};

type Paragraph = <cosmic::Renderer as iced::advanced::text::Renderer>::Paragraph;
type TextState = iced::advanced::widget::text::State<Paragraph>;

#[derive(Default)]
struct Geometry {
    texts: Vec<(String, Rectangle)>,
    controls: Vec<Rectangle>,
    bodies: Vec<(Rectangle, u64, String, Option<Target>)>,
}

impl Operation for Geometry {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    fn text(&mut self, _: Option<&widget::Id>, bounds: Rectangle, text: &str) {
        self.texts.push((text.to_owned(), bounds));
    }

    fn focusable(
        &mut self,
        _: Option<&widget::Id>,
        bounds: Rectangle,
        _: &mut dyn operation::Focusable,
    ) {
        self.controls.push(bounds);
    }

    fn custom(&mut self, _: Option<&widget::Id>, bounds: Rectangle, state: &mut dyn std::any::Any) {
        if let Some(state) = state.downcast_ref::<selectable_prose::State>() {
            self.bodies.push((
                bounds,
                state.identity,
                state.plain.to_string(),
                state.target,
            ));
        }
    }
}

fn paragraphs(tree: &Tree, label: &str, found: &mut Vec<(Size, Vec<f32>, usize)>) {
    if tree.tag == tree::Tag::of::<TextState>() {
        let paragraph = tree.state.downcast_ref::<TextState>().paragraph.raw();
        let text = paragraph
            .buffer()
            .lines
            .iter()
            .map(|line| line.text())
            .collect::<Vec<_>>()
            .join("\n");
        if text == label {
            found.push((
                paragraph.min_bounds(),
                paragraph
                    .buffer()
                    .layout_runs()
                    .map(|run| run.line_height)
                    .collect(),
                paragraph
                    .buffer()
                    .layout_runs()
                    .map(|run| run.glyphs.iter().map(|glyph| glyph.end).max().unwrap_or(0))
                    .max()
                    .unwrap_or(0),
            ));
        }
    }
    for child in &tree.children {
        paragraphs(child, label, found);
    }
}

fn update(
    element: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &cosmic::Renderer,
    viewport: Rectangle,
    position: Point,
    event: Event,
    messages: &mut Vec<Message>,
) {
    element.as_widget_mut().update(
        tree,
        &event,
        Layout::new(node),
        mouse::Cursor::Available(position),
        renderer,
        &mut clipboard::Null,
        &mut Shell::new(messages),
        &viewport,
    );
}

#[test]
fn retained_code_caption_fits_its_line_box_and_keeps_tooltip_body_and_copy() {
    let _restore = design::typography_test_guard();
    let renderer = crate::app::tests::headless_renderer();
    let target = Target {
        card: CardId::new(),
        generation: 7,
    };
    let item = "caption-fixture-command";
    let body = "printf 'working directory\\n'\nfixture output";
    // A canonical fixture supplies the new button provenance only; these
    // caption/layout cases still do not claim clipboard delivery or provider IO.
    let fixture = chat::Item {
        id: item.into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "Inspect".into(),
            input: body.into(),
            output: body.into(),
            status: chat::ToolStatus::Succeeded,
        },
        at: None,
        started_at: None,
    };
    let (mut runtime, _handle) = view::prepared_runtime_for_test(&[fixture]);
    runtime.retarget(target);
    let spaced_path = "/home/developer/Agent Development Environment/project with spaces/worktrees/feature with a readable long name";
    let unbroken_path = format!("/workspace/{}/checkout", "component".repeat(36));
    // These are the production Input, Output, Patch and Command-cwd roles.
    // Both spaced and unbroken paths must retain their full source in the
    // caption and lazy tooltip; this headless check does not measure ink.
    for (caption, part) in [
        ("Input", CopyPart::Input),
        ("Output", CopyPart::Output),
        ("Patch", CopyPart::Change(2)),
        (spaced_path, CopyPart::Input),
        (unbroken_path.as_str(), CopyPart::Input),
    ] {
        let source = runtime
            .rendered_copy_source(runtime.active_item(item).unwrap(), part.clone())
            .expect("the actual prepared fixture owns its source");
        for width in [240.0, 640.0] {
            let size = Size::new(width, 1200.0);
            let viewport = Rectangle::with_size(size);
            let limits = layout::Limits::new(Size::ZERO, size);
            let mut tree = Tree::empty();
            let mut original_body = None;
            for interface in [13.0, 24.0, 13.0] {
                design::configure_typography("", interface, "", 14.0);
                let mut element = code_surface(
                    body,
                    Some(caption),
                    None,
                    Some(Action::Copy(
                        target,
                        item.into(),
                        part.clone(),
                        Some(source.clone()),
                    )),
                    target,
                );
                tree.diff(element.as_widget_mut());
                let node = element
                    .as_widget_mut()
                    .layout(&mut tree, &renderer, &limits);
                let mut geometry = Geometry::default();
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut geometry,
                );
                // Inspect the actual header and body allocations beneath the
                // unchanged production Hover, rather than mirroring font math.
                let block = Layout::new(&node).children().next().unwrap();
                let mut children = block.children();
                let header = children.next().unwrap().bounds();
                let body_surface = children.next().unwrap().bounds();
                let caption_bounds = geometry
                    .texts
                    .iter()
                    .find(|(text, _)| text == caption)
                    .expect("the original caption remains the text operation's source")
                    .1;
                let mut metrics = Vec::new();
                paragraphs(&tree, caption, &mut metrics);
                assert_eq!(metrics.len(), 1, "tooltip stays lazy before hover");
                let (natural, runs, _) = &metrics[0];
                assert_eq!(runs.len(), 1, "even long cwd captions use one line");
                assert!(natural.height <= caption_bounds.height + 0.05);
                assert!(runs[0] <= caption_bounds.height + 0.05);
                assert!(caption_bounds.y >= header.y);
                assert!(
                    caption_bounds.y + caption_bounds.height <= header.y + header.height + 0.05
                );
                assert!(
                    caption_bounds.x + caption_bounds.width
                        <= header.x + header.width - 24.0 + 0.05
                );
                assert!(header.y + header.height + GAP_XS <= body_surface.y + 0.05);
                if interface == 13.0 {
                    assert_eq!(header.height, 24.0, "default compact header is unchanged");
                } else {
                    assert!(
                        header.height > 24.0,
                        "the actual larger paragraph grows the header"
                    );
                }
                assert_eq!(geometry.bodies.len(), 1);
                let (body_bounds, identity, source, source_target) = &geometry.bodies[0];
                assert_eq!(source, body);
                assert_eq!(*source_target, Some(target));
                assert!(body_bounds.y >= body_surface.y);
                assert_eq!(
                    *original_body.get_or_insert(*identity),
                    *identity,
                    "retained typography must preserve the selectable body owner"
                );

                // The pinned outer Hover has no a11y forwarding. The same
                // production caption without Copy does: verify the full label
                // there without claiming to repair that inherited wrapper.
                let mut accessible = code_surface(body, Some(caption), None, None, target);
                let mut accessible_tree = Tree::new(accessible.as_widget());
                let accessible_node =
                    accessible
                        .as_widget_mut()
                        .layout(&mut accessible_tree, &renderer, &limits);
                let nodes = accessible.as_widget().a11y_nodes(
                    Layout::new(&accessible_node),
                    &accessible_tree,
                    mouse::Cursor::Unavailable,
                );
                assert!(
                    nodes
                        .root()
                        .iter()
                        .chain(nodes.children())
                        .any(|entry| { entry.node().label() == Some(caption) }),
                    "ellipsis must preserve the entire caption's accessible label"
                );

                let mut messages = Vec::new();
                let caption_point = Point::new(caption_bounds.x + 2.0, caption_bounds.center_y());
                update(
                    &mut element,
                    &mut tree,
                    &node,
                    &renderer,
                    viewport,
                    caption_point,
                    Event::Mouse(mouse::Event::CursorMoved {
                        position: caption_point,
                    }),
                    &mut messages,
                );
                // Supply the real redraw clock; do not sleep or reach into the
                // tooltip's private state to manufacture an open overlay.
                update(
                    &mut element,
                    &mut tree,
                    &node,
                    &renderer,
                    viewport,
                    caption_point,
                    Event::Window(iced::window::Event::RedrawRequested(
                        iced::time::Instant::now() + Duration::from_secs(1),
                    )),
                    &mut messages,
                );
                let tooltip_text_bounds;
                let tooltip_bounds;
                {
                    let mut overlay = element
                        .as_widget_mut()
                        .overlay(
                            &mut tree,
                            Layout::new(&node),
                            &renderer,
                            &viewport,
                            Vector::ZERO,
                        )
                        .expect("hovering the caption opens its production tooltip");
                    let tooltip_node = overlay.as_overlay_mut().layout(&renderer, size);
                    // TipOverlay intentionally has no widget-operation
                    // forwarding. Its actual layout is a single tooltip
                    // container/text beneath single-child overlay Groups.
                    let mut text_layout = Layout::new(&tooltip_node);
                    let mut parent_bounds = text_layout.bounds();
                    loop {
                        let mut children = text_layout.children();
                        let Some(child) = children.next() else { break };
                        assert!(
                            children.next().is_none(),
                            "only the caption tooltip is open"
                        );
                        parent_bounds = text_layout.bounds();
                        text_layout = child;
                    }
                    tooltip_bounds = parent_bounds;
                    tooltip_text_bounds = text_layout.bounds();
                    assert!(tooltip_bounds.width <= width + 0.05);
                    assert!(tooltip_text_bounds.x >= tooltip_bounds.x);
                    assert!(tooltip_text_bounds.y >= tooltip_bounds.y);
                    assert!(
                        tooltip_text_bounds.x + tooltip_text_bounds.width
                            <= tooltip_bounds.x + tooltip_bounds.width + 0.05
                    );
                    assert!(
                        tooltip_text_bounds.y + tooltip_text_bounds.height
                            <= tooltip_bounds.y + tooltip_bounds.height + 0.05
                    );
                }
                let mut hovered_metrics = Vec::new();
                paragraphs(&tree, caption, &mut hovered_metrics);
                assert_eq!(
                    hovered_metrics.len(),
                    2,
                    "the laid-out lazy tooltip retains the full, unellipsized source"
                );
                let (tooltip_natural, tooltip_runs, rendered_end) = &hovered_metrics[1];
                assert_eq!(
                    *rendered_end,
                    caption.len(),
                    "the tooltip's actually allocated runs reach the final source byte"
                );
                assert!(
                    tooltip_natural.width <= tooltip_text_bounds.width + 0.05,
                    "the tooltip allocates every rendered run's full width"
                );
                assert!(tooltip_natural.height <= tooltip_text_bounds.height + 0.05);
                assert!(tooltip_runs.iter().sum::<f32>() <= tooltip_text_bounds.height + 0.05);
                if caption == spaced_path || caption == unbroken_path.as_str() {
                    assert!(
                        tooltip_runs.len() > 1,
                        "both spaced and unbroken cwd components wrap in the tooltip"
                    );
                }

                assert_eq!(geometry.controls.len(), 1, "one actual hover Copy control");
                let copy_bounds = geometry.controls[0];
                assert!(copy_bounds.x >= caption_bounds.x + caption_bounds.width - 0.05);
                let copy_point = copy_bounds.center();
                for event in [
                    Event::Mouse(mouse::Event::CursorMoved {
                        position: copy_point,
                    }),
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                ] {
                    update(
                        &mut element,
                        &mut tree,
                        &node,
                        &renderer,
                        viewport,
                        copy_point,
                        event,
                        &mut messages,
                    );
                }
                assert_eq!(
                    messages.len(),
                    1,
                    "caption hover does not consume or replace Copy"
                );
                let Message::ChatUi(Action::Copy(
                    actual_target,
                    actual_item,
                    actual_part,
                    actual_source,
                )) = &messages[0]
                else {
                    panic!("the real hover button must emit its captured Copy action");
                };
                assert_eq!(*actual_target, target);
                assert!(
                    actual_source
                        .as_ref()
                        .is_some_and(|actual| actual == &source)
                );
                assert_eq!(actual_item, item);
                assert!(matches!(
                    (actual_part, &part),
                    (CopyPart::Input, CopyPart::Input)
                        | (CopyPart::Output, CopyPart::Output)
                        | (CopyPart::Change(2), CopyPart::Change(2))
                ));
            }
        }
    }
}

#[test]
fn retained_tool_copy_press_keeps_its_rendered_source_and_part() {
    use iced::runtime::{UserInterface, user_interface::Cache};

    fn frame(
        runtime: &Runtime,
        part: CopyPart,
        cache: &mut Cache,
        renderer: &mut cosmic::Renderer,
        events: &[Event],
        held_point: Option<Point>,
    ) -> (Vec<Message>, Point) {
        let item = runtime.active_item("same-id").unwrap();
        let cached = &runtime.cache[&item.id];
        let caption = if matches!(part, CopyPart::Output) {
            "Output"
        } else {
            "Input"
        };
        let element = runtime.tool_code_surface(item, cached, part, caption, None);
        widget_frame(element, cache, renderer, events, held_point)
    }
    fn widget_frame(
        element: Element<'static, Message>,
        cache: &mut Cache,
        renderer: &mut cosmic::Renderer,
        events: &[Event],
        held_point: Option<Point>,
    ) -> (Vec<Message>, Point) {
        let mut ui = UserInterface::build(
            element,
            Size::new(480.0, 320.0),
            std::mem::replace(cache, Cache::new()),
            renderer,
        );
        let mut geometry = Geometry::default();
        ui.operate(renderer, &mut geometry);
        assert_eq!(
            geometry.controls.len(),
            1,
            "the real hover Copy button remains mounted"
        );
        let point = held_point.unwrap_or_else(|| geometry.controls[0].center());
        let mut messages = Vec::new();
        ui.update(
            events,
            mouse::Cursor::Available(point),
            renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        *cache = ui.into_cache();
        (messages, point)
    }
    fn assert_copy(messages: &[Message], runtime: &Runtime, part: CopyPart) {
        assert_eq!(messages.len(), 1);
        let Message::ChatUi(Action::Copy(target, id, actual_part, Some(source))) = &messages[0]
        else {
            panic!("a fresh native press emits captured rendered Copy");
        };
        assert_eq!(*target, runtime.target);
        assert_eq!(id, "same-id");
        assert_eq!(*actual_part, part);
        assert!(source.matches(runtime, id, &part));
    }
    let tool = |input: &str| chat::Item {
        id: "same-id".into(),
        role: chat::Role::Assistant,
        content: ItemContent::Tool {
            name: "Inspect".into(),
            input: input.into(),
            output: "output source".into(),
            status: chat::ToolStatus::Succeeded,
        },
        at: None,
        started_at: None,
    };
    let (mut runtime, handle) = view::prepared_runtime_for_test(&[tool("parent input")]);
    runtime.expanded.insert("same-id".into());
    runtime.cache.clear();
    let snapshot = runtime.snapshot.clone();
    runtime.prepare_transcript_cache(&snapshot);
    preparation::drain_fixture(&mut runtime);
    let strong = Arc::strong_count(&runtime.snapshot.items[0]);
    let source = runtime
        .rendered_copy_source(&runtime.snapshot.items[0], CopyPart::Input)
        .unwrap();
    let _callback = Action::Copy(
        runtime.target,
        "same-id".into(),
        CopyPart::Input,
        Some(source),
    );
    assert_eq!(
        Arc::strong_count(&runtime.snapshot.items[0]),
        strong,
        "capturing a callback must not retain the canonical payload"
    );
    let mut renderer = crate::app::tests::headless_renderer();
    let mut cache = Cache::new();
    let press = Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left));
    let release = Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left));
    let (messages, point) = frame(
        &runtime,
        CopyPart::Input,
        &mut cache,
        &mut renderer,
        &[press.clone()],
        None,
    );
    assert!(messages.is_empty());

    let replacement = Arc::new(tool("child input"));
    assert!(!Arc::ptr_eq(&runtime.snapshot.items[0], &replacement));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.items = vec![replacement.clone()];
    handle.publish_snapshot_for_test(snapshot);
    // Controlled cache-lag projection: the current canonical item has arrived,
    // while the old prepared body is still what this real code surface draws.
    // No provider/preparation timing or native app behavior is claimed here.
    runtime.snapshot = handle.snapshot();
    runtime.copy_owners = handle.copy_owners_for(&runtime.snapshot).unwrap();
    assert!(
        runtime
            .rendered_copy_source(&replacement, CopyPart::Input)
            .is_none()
    );
    let (messages, _) = frame(
        &runtime,
        CopyPart::Input,
        &mut cache,
        &mut renderer,
        &[release.clone()],
        Some(point),
    );
    assert!(
        messages.is_empty(),
        "old press cannot migrate to an unavailable replacement"
    );
    let (messages, _) = frame(
        &runtime,
        CopyPart::Input,
        &mut cache,
        &mut renderer,
        &[press.clone(), release.clone()],
        Some(point),
    );
    assert!(
        messages.is_empty(),
        "stale prepared body exposes a disabled Copy control"
    );

    let snapshot = runtime.snapshot.clone();
    runtime.prepare_transcript_cache(&snapshot);
    preparation::drain_fixture(&mut runtime);
    let (messages, _) = frame(
        &runtime,
        CopyPart::Input,
        &mut cache,
        &mut renderer,
        &[release.clone()],
        Some(point),
    );
    assert!(
        messages.is_empty(),
        "ready replacement still requires a fresh press"
    );
    let (messages, _) = frame(
        &runtime,
        CopyPart::Input,
        &mut cache,
        &mut renderer,
        &[press.clone(), release.clone()],
        Some(point),
    );
    assert_copy(&messages, &runtime, CopyPart::Input);

    // Also replace one fully prepared, enabled source directly with another:
    // no disabled/intermediate frame can explain this ownership rejection.
    frame(
        &runtime,
        CopyPart::Input,
        &mut cache,
        &mut renderer,
        &[press.clone()],
        Some(point),
    );
    let replacement = Arc::new(tool("newer input"));
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.items = vec![replacement.clone()];
    handle.publish_snapshot_for_test(snapshot);
    runtime.refresh();
    preparation::drain_fixture(&mut runtime);
    assert!(
        runtime
            .rendered_copy_source(&replacement, CopyPart::Input)
            .is_some()
    );
    let (messages, _) = frame(
        &runtime,
        CopyPart::Input,
        &mut cache,
        &mut renderer,
        &[release.clone()],
        Some(point),
    );
    assert!(
        messages.is_empty(),
        "an enabled replacement cannot inherit the old source's press"
    );
    let (messages, _) = frame(
        &runtime,
        CopyPart::Input,
        &mut cache,
        &mut renderer,
        &[press.clone(), release.clone()],
        Some(point),
    );
    assert_copy(&messages, &runtime, CopyPart::Input);

    frame(
        &runtime,
        CopyPart::Input,
        &mut cache,
        &mut renderer,
        &[press.clone()],
        Some(point),
    );
    let mut snapshot = (*handle.snapshot()).clone();
    snapshot.generation += 1;
    snapshot.busy = !snapshot.busy;
    handle.publish_delta_snapshot_for_test(snapshot);
    runtime.refresh();
    assert!(Arc::ptr_eq(&runtime.snapshot.items[0], &replacement));
    let (messages, _) = frame(
        &runtime,
        CopyPart::Input,
        &mut cache,
        &mut renderer,
        &[release.clone()],
        Some(point),
    );
    assert_copy(&messages, &runtime, CopyPart::Input);

    // Drive actual accepted Output deltas through the receiver and retain
    // this same native tree. The unchanged Input and growing Output both
    // keep their gesture; their action still resolves the current full part.
    for part in [CopyPart::Input, CopyPart::Output] {
        let (_, held_at) = frame(
            &runtime,
            part.clone(),
            &mut cache,
            &mut renderer,
            &[press.clone()],
            None,
        );
        let proofs = runtime.scroll_memory.reader_proofs(None);
        assert!(
            preparation::has_tool_reader_proofs(&runtime.snapshot.items[0], &proofs),
            "the real native reader created the preparation trigger"
        );
        let previous = runtime.snapshot.items[0].clone();
        let mut appended = (*previous).clone();
        let ItemContent::Tool { output, .. } = &mut appended.content else {
            unreachable!()
        };
        output.push_str("\ncontinued output");
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.items = vec![Arc::new(appended)];
        handle.publish_delta_snapshot_for_test(snapshot);
        runtime.refresh();
        assert!(runtime.cache["same-id"].preparing);
        assert!(!runtime.cache["same-id"].placeholder);
        assert!(
            Arc::ptr_eq(runtime.cache["same-id"].item.as_ref().unwrap(), &previous),
            "unchanged prepared Input remains available while output is prepared"
        );
        assert!(!Arc::ptr_eq(&previous, &runtime.snapshot.items[0]));
        let (messages, _) = frame(
            &runtime,
            part.clone(),
            &mut cache,
            &mut renderer,
            &[release.clone()],
            Some(held_at),
        );
        assert_copy(&messages, &runtime, part);
        preparation::drain_fixture(&mut runtime);
    }

    frame(
        &runtime,
        CopyPart::Input,
        &mut cache,
        &mut renderer,
        &[press.clone()],
        Some(point),
    );
    let (messages, _) = frame(
        &runtime,
        CopyPart::Output,
        &mut cache,
        &mut renderer,
        &[release.clone()],
        Some(point),
    );
    assert!(
        messages.is_empty(),
        "a held Input press cannot become Output on the same item"
    );
    let (messages, _) = frame(
        &runtime,
        CopyPart::Output,
        &mut cache,
        &mut renderer,
        &[press, release],
        None,
    );
    assert_copy(&messages, &runtime, CopyPart::Output);

    // Adjacent native controls share this same retained-action boundary.
    // These cases inspect their captured actions, not full-message reads or
    // clipboard delivery, whose existing receiver/async guards are unchanged.
    let original = chat::TextCopySource {
        cache: uuid::Uuid::new_v4(),
        scope: chat::TextCopyScope::Conversation,
        token: 7,
        revision: 9,
        bytes: 12,
    };
    let message = |source| {
        small_icon(
            Icon::Copy,
            "Copy this message",
            Action::CopyMessage(runtime.target, "same-id".into(), source),
        )
    };
    for change in ["cache", "scope", "token", "revision", "missing"] {
        let mut changed = original.clone();
        match change {
            "cache" => changed.cache = uuid::Uuid::new_v4(),
            "scope" => changed.scope = chat::TextCopyScope::Subagent { id: "child".into() },
            "token" => changed.token += 1,
            "revision" => changed.revision += 1,
            _ => {}
        }
        let replacement = (change != "missing").then_some(changed);
        let mut cache = Cache::new();
        let (_, point) = widget_frame(
            message(Some(original.clone())),
            &mut cache,
            &mut renderer,
            &[Event::Mouse(mouse::Event::ButtonPressed(
                mouse::Button::Left,
            ))],
            None,
        );
        let (messages, _) = widget_frame(
            message(replacement.clone()),
            &mut cache,
            &mut renderer,
            &[Event::Mouse(mouse::Event::ButtonReleased(
                mouse::Button::Left,
            ))],
            Some(point),
        );
        assert!(
            messages.is_empty(),
            "a held logical-message Copy cannot move to {change}"
        );
        let (messages, _) = widget_frame(
            message(replacement.clone()),
            &mut cache,
            &mut renderer,
            &[
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            ],
            Some(point),
        );
        assert_eq!(messages.len(), 1);
        assert!(
            matches!(&messages[0], Message::ChatUi(Action::CopyMessage(target, id, source))
            if *target == runtime.target && id == "same-id" && source == &replacement)
        );
    }
    let mut cache = Cache::new();
    let (_, point) = widget_frame(
        message(Some(original.clone())),
        &mut cache,
        &mut renderer,
        &[Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Left,
        ))],
        None,
    );
    let mut appended = original.clone();
    appended.bytes += 100;
    let (messages, _) = widget_frame(
        message(Some(appended.clone())),
        &mut cache,
        &mut renderer,
        &[Event::Mouse(mouse::Event::ButtonReleased(
            mouse::Button::Left,
        ))],
        Some(point),
    );
    assert_eq!(
        messages.len(),
        1,
        "a verified growing prefix preserves its held press"
    );
    assert!(
        matches!(&messages[0], Message::ChatUi(Action::CopyMessage(_, _, Some(source))) if source == &appended)
    );

    let first: Arc<str> = Arc::from("canonical raw code λ\n");
    let second: Arc<str> = Arc::from(first.as_ref());
    assert!(!Arc::ptr_eq(&first, &second));
    let shared = |source| {
        code_surface(
            "code body",
            Some("Code"),
            None,
            Some(Action::CopySharedCode(runtime.target, source)),
            runtime.target,
        )
    };
    let mut cache = Cache::new();
    let (_, point) = widget_frame(
        shared(first.clone()),
        &mut cache,
        &mut renderer,
        &[Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Left,
        ))],
        None,
    );
    let (messages, _) = widget_frame(
        shared(second.clone()),
        &mut cache,
        &mut renderer,
        &[Event::Mouse(mouse::Event::ButtonReleased(
            mouse::Button::Left,
        ))],
        Some(point),
    );
    assert!(
        messages.is_empty(),
        "equal bytes from a different canonical Arc do not inherit a raw-code press"
    );
    let (messages, _) = widget_frame(
        shared(second.clone()),
        &mut cache,
        &mut renderer,
        &[
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ],
        Some(point),
    );
    assert_eq!(messages.len(), 1);
    assert!(
        matches!(&messages[0], Message::ChatUi(Action::CopySharedCode(target, source))
        if *target == runtime.target && Arc::ptr_eq(source, &second))
    );
    widget_frame(
        shared(second.clone()),
        &mut cache,
        &mut renderer,
        &[Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Left,
        ))],
        Some(point),
    );
    let (messages, _) = widget_frame(
        shared(second.clone()),
        &mut cache,
        &mut renderer,
        &[Event::Mouse(mouse::Event::ButtonReleased(
            mouse::Button::Left,
        ))],
        Some(point),
    );
    assert_eq!(
        messages.len(),
        1,
        "unchanged canonical raw code preserves its held press"
    );
}
