use super::*;
use crate::splitter::{Edge, Splitter};
use iced::advanced::widget::{Id, operation};

struct NativeViewport {
    id: Id,
    sample: Option<(Rectangle, Rectangle, Vector)>,
    center: bool,
    visits: usize,
}

impl Operation for NativeViewport {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    fn scrollable(
        &mut self,
        id: Option<&Id>,
        viewport: Rectangle,
        content: Rectangle,
        translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if id != Some(&self.id) {
            return;
        }
        self.visits += 1;
        self.sample = Some((viewport, content, translation));
        if self.center {
            state.snap_to(iced::widget::scrollable::RelativeOffset {
                x: None,
                y: Some(0.5),
            });
        }
    }
}

fn native_viewport(
    element: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &cosmic::Renderer,
    target: Target,
    center: bool,
) -> (Rectangle, Rectangle, Vector) {
    let mut probe = NativeViewport {
        id: card_control_id(target.card, "chat-transcript"),
        sample: None,
        center,
        visits: 0,
    };
    element
        .as_widget_mut()
        .operate(tree, Layout::new(node), renderer, &mut probe);
    assert_eq!(
        probe.visits, 1,
        "operate the mounted native transcript once"
    );
    probe.sample.expect("the production transcript Scrollable")
}

fn dispatch(
    element: &mut Element<'_, Message>,
    tree: &mut Tree,
    node: &layout::Node,
    renderer: &cosmic::Renderer,
    event: mouse::Event,
    point: Point,
) -> (Vec<Message>, bool) {
    let mut messages = Vec::new();
    let mut shell = Shell::new(&mut messages);
    element.as_widget_mut().update(
        tree,
        &Event::Mouse(event),
        Layout::new(node),
        mouse::Cursor::Available(point),
        renderer,
        &mut iced::advanced::clipboard::Null,
        &mut shell,
        &node.bounds(),
    );
    let captured = shell.is_event_captured();
    (messages, captured)
}

fn is_resize(message: &Message) -> bool {
    matches!(
        message,
        Message::ResizeCard { .. } | Message::ResizeCardFinished(_)
    )
}

fn fixture(target: Target, width: f32) -> Element<'static, Message> {
    let rows = (1..=100)
        .map(|key| {
            virtualized::Row::new(key, 0, 40.0, None, || {
                widget::Space::new().width(Length::Fill).height(40.0).into()
            })
        })
        .collect();
    let body = transcript_viewport(selection::transcript(
        scroll::transcript_observed(
            transcript_column(virtualized::column(rows, Default::default())),
            target,
            None,
            vec![],
            false,
            vec![],
            Default::default(),
            Default::default(),
            false,
            Default::default(),
            None,
        ),
        target,
        None,
    ));
    let footer = widget::flex_row(vec![
        widget::Space::new().width(260.0).height(24.0).into(),
        widget::Space::new().width(180.0).height(24.0).into(),
        widget::Space::new().width(120.0).height(24.0).into(),
    ])
    .spacing(8.0)
    .into();
    let edge = Splitter::new(
        card_control_id(target.card, "resize"),
        "Resize synthetic transcript pane",
        width,
        160.0,
        2_000.0,
        Edge::Trailing,
        false,
        move |width| Message::ResizeCard {
            card: target.card,
            width,
        },
        move || Message::ResizeCardFinished(target.card),
    );
    // Match App's actual front-first resize layer; a naked Scrollable cannot
    // detect the pane-edge splitter stealing the thumb's press.
    crate::interaction::layer(
        pane(body, footer),
        widget::container(edge)
            .align_x(Alignment::End)
            .width(Length::Fill)
            .height(Length::Fill)
            .into(),
    )
}

#[test]
fn pane_edge_thumb_and_resize_seam_keep_their_native_gestures_separate() {
    let renderer = crate::app::tests::headless_renderer();
    for initial_width in [1_440.0, 720.0, 360.0] {
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let mut element = fixture(target, initial_width);
        let mut tree = Tree::new(&element);
        let mut final_node = None;
        // The second layout retains the actual Scrollable, Anchored reader,
        // virtual rows and splitter while shrinking and wrapping the footer.
        for (width, height) in [(initial_width, 480.0), (initial_width - 80.0, 360.0)] {
            let node = element
                .as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(width, height)),
                )
                .move_to(Point::new(137.25, 63.5));
            native_viewport(&mut element, &mut tree, &node, &renderer, target, true);
            let (viewport, content, before) =
                native_viewport(&mut element, &mut tree, &node, &renderer, target, false);
            assert!(content.height > viewport.height * 3.0);
            assert!(viewport.height > 80.0, "the wrapped footer leaves a body");

            // At the native midpoint the thumb's center is the viewport's
            // center in Y. X comes from the production rail's measured edge.
            // Outcomes below come from actual event dispatch, not a mock rail.
            let thumb = Point::new(
                viewport.x + viewport.width - design::SCROLLBAR_GUTTER / 2.0,
                viewport.center_y(),
            );
            let moved = Point::new(thumb.x, thumb.y + 35.0);
            let mut gesture = Vec::new();
            for (event, point, must_capture) in [
                (
                    mouse::Event::ButtonPressed(mouse::Button::Left),
                    thumb,
                    true,
                ),
                (mouse::Event::CursorMoved { position: moved }, moved, true),
                (
                    mouse::Event::ButtonReleased(mouse::Button::Left),
                    moved,
                    false,
                ),
            ] {
                let (messages, captured) =
                    dispatch(&mut element, &mut tree, &node, &renderer, event, point);
                if must_capture {
                    assert!(captured, "the actual native thumb owns its gesture");
                }
                gesture.extend(messages);
            }
            let (_, _, after) =
                native_viewport(&mut element, &mut tree, &node, &renderer, target, false);
            assert!(
                after.y > before.y + 1.0,
                "dragging down scrolls the transcript"
            );
            assert!(!gesture.iter().any(is_resize), "thumb drag must not resize");
            assert!(gesture.iter().any(|message| matches!(message,
                Message::ChatUi(Action::Scrolled(actual, None, _, _)) if *actual == target
            )));
            let released = Point::new(moved.x, moved.y + 20.0);
            let (messages, _) = dispatch(
                &mut element,
                &mut tree,
                &node,
                &renderer,
                mouse::Event::CursorMoved { position: released },
                released,
            );
            let (_, _, after_release) =
                native_viewport(&mut element, &mut tree, &node, &renderer, target, false);
            assert_eq!(
                after_release, after,
                "release retires the native thumb grab"
            );
            assert!(!messages.iter().any(is_resize));

            // This point lies in the newly usable wide-pane margin, rather
            // than the readable text column. Ordinary native wheel input must
            // still reach the same transcript reader there.
            let (messages, captured) = dispatch(
                &mut element,
                &mut tree,
                &node,
                &renderer,
                mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Lines { x: 0.0, y: -1.0 },
                },
                Point::new(viewport.x + 1.0, viewport.center_y()),
            );
            let (_, _, after_wheel) =
                native_viewport(&mut element, &mut tree, &node, &renderer, target, false);
            assert!(captured && after_wheel.y > after.y);
            assert!(!messages.iter().any(is_resize));
            final_node = Some(node);
        }

        let node = final_node.unwrap();
        let (viewport, _, before) =
            native_viewport(&mut element, &mut tree, &node, &renderer, target, false);
        let seam = Point::new(
            node.bounds().x + node.size().width - crate::splitter::WIDTH / 2.0,
            viewport.center_y(),
        );
        let moved = Point::new(seam.x + 24.0, seam.y);
        let mut gesture = Vec::new();
        for (event, point) in [
            (mouse::Event::ButtonPressed(mouse::Button::Left), seam),
            (mouse::Event::CursorMoved { position: moved }, moved),
            (mouse::Event::ButtonReleased(mouse::Button::Left), moved),
        ] {
            let (messages, captured) =
                dispatch(&mut element, &mut tree, &node, &renderer, event, point);
            assert!(captured, "the actual splitter owns its gesture");
            gesture.extend(messages);
        }
        assert!(gesture.iter().any(|message| matches!(message,
            Message::ResizeCard { card, width } if *card == target.card && *width > initial_width
        )));
        assert!(gesture.iter().any(|message| matches!(message,
            Message::ResizeCardFinished(card) if *card == target.card
        )));
        assert!(
            !gesture
                .iter()
                .any(|message| matches!(message, Message::ChatUi(Action::Scrolled(_, _, _, _))))
        );
        let (_, _, after) =
            native_viewport(&mut element, &mut tree, &node, &renderer, target, false);
        assert_eq!(after, before, "resize seam must not move transcript scroll");
    }
}
