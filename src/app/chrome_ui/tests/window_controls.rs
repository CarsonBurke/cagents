use super::super::*;
use cosmic::iced::advanced::{
    Layout, Shell, clipboard, layout,
    widget::{Operation, Tree, operation::Focusable},
};
use cosmic::iced::{Event, Point, Rectangle, Size, mouse};

#[derive(Default)]
struct Bounds(Vec<(Option<widget::Id>, Rectangle)>);

impl Operation for Bounds {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    fn focusable(&mut self, id: Option<&widget::Id>, bounds: Rectangle, _: &mut dyn Focusable) {
        self.0.push((id.cloned(), bounds));
    }
}

fn fixture(with_card: bool, width: f32) -> (Cagents, window::Id) {
    let mut app = crate::app::tests::context_inspector_test_app(with_card);
    let window = window::Id::unique();
    app.core.set_main_window_id(Some(window));
    app.core.window.show_close = true;
    // Isolate the Close policy from desktop minimize/maximize preferences.
    app.core.window.show_minimize = false;
    app.core.window.show_maximize = false;
    drop(app.update(Message::WindowResized(width, 900.0)));
    (app, window)
}

struct Probe {
    close: Vec<Point>,
    minimize: Vec<Point>,
    maximize: Vec<Point>,
    toggle: Vec<Rectangle>,
}

/// Exercise the whole application, including the overlay's modal hit-test
/// boundary. Header buttons have generated IDs, so discover their actual
/// focusable bounds and classify the messages from real Down/Up gestures.
/// Every candidate gets a fresh tree: an unrelated button cannot carry a
/// pressed/focused state into the next candidate. No message is reduced.
fn probe(app: &Cagents, renderer: &cosmic::Renderer, window: window::Id) -> Probe {
    let viewport = Rectangle::with_size(Size::new(app.window_width, app.window_height));
    let mut element = cosmic::ApplicationExt::view_main(app);
    let mut tree = Tree::new(element.as_widget());
    let node = element.as_widget_mut().layout(
        &mut tree,
        renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    let mut bounds = Bounds::default();
    element
        .as_widget_mut()
        .operate(&mut tree, Layout::new(&node), renderer, &mut bounds);
    drop(element);
    let toggle = bounds
        .0
        .iter()
        .filter(|(id, _)| id.as_ref() == Some(&context_inspector_toggle_id()))
        .map(|(_, bounds)| *bounds)
        .collect();
    let mut points = Vec::new();
    for (_, bounds) in bounds.0 {
        let point = bounds.center();
        if bounds.width > 0.0
            && bounds.height > 0.0
            && point.y < 2.0 * design::HEADER_HEIGHT
            && viewport.contains(point)
            && !points.contains(&point)
        {
            points.push(point);
        }
    }
    let mut close = Vec::new();
    let mut minimize = Vec::new();
    let mut maximize = Vec::new();
    for point in points {
        let mut element = cosmic::ApplicationExt::view_main(app);
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let mut messages = Vec::new();
        for event in [
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(point),
                renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        if messages.iter().any(|message| {
            matches!(
                message,
                cosmic::Action::App(Message::WindowCloseRequested(_))
            )
        }) {
            let closes: Vec<_> = messages
                .iter()
                .filter_map(|message| match message {
                    cosmic::Action::App(Message::WindowCloseRequested(actual)) => Some(*actual),
                    _ => None,
                })
                .collect();
            assert_eq!(closes, [window]);
            assert!(
                !messages.iter().any(|message| matches!(
                    message,
                    cosmic::Action::App(
                        Message::ToggleContextInspector
                            | Message::ContextInspector(InspectorAction::Close)
                    )
                )),
                "Close must not dismiss the overlay or toggle the inspector"
            );
            close.push(point);
        }
        if messages.iter().any(|message| {
            matches!(
                message,
                cosmic::Action::App(Message::NativeWindowAction(cosmic::app::Action::Minimize))
            )
        }) {
            minimize.push(point);
        }
        if messages.iter().any(|message| {
            matches!(
                message,
                cosmic::Action::App(Message::NativeWindowAction(cosmic::app::Action::Maximize))
            )
        }) {
            maximize.push(point);
        }
    }
    Probe {
        close,
        minimize,
        maximize,
        toggle,
    }
}

#[test]
fn empty_open_preference_keeps_a_working_window_close_and_its_width() {
    let renderer = crate::app::tests::headless_renderer();
    let (mut app, window) = fixture(false, 1600.0);
    assert!(app.context_inspector_open);
    assert!(app.sidebar_model.projects.is_empty() && app.workspace.cards().is_empty());
    assert_eq!(app.context_inspector_mount(), None);
    assert_eq!(
        probe(&app, &renderer, window).close.len(),
        1,
        "an unmounted inspector cannot take the empty window's Close"
    );
    let open_width = app.workspace_header_width();
    drop(app.update(Message::ToggleContextInspector));
    assert!(!app.context_inspector_open);
    assert_eq!(app.context_inspector_mount(), None);
    assert_eq!(probe(&app, &renderer, window).close.len(), 1);
    assert_eq!(app.workspace_header_width(), open_width);
}

#[test]
fn docked_collapsing_and_closed_headers_keep_exactly_one_working_close() {
    let renderer = crate::app::tests::headless_renderer();
    let (mut app, window) = fixture(true, 1600.0);
    assert_eq!(app.context_inspector_mount(), Some(InspectorLayout::Docked));
    assert_eq!(probe(&app, &renderer, window).close.len(), 1);
    let docked_width = app.workspace_header_width();
    drop(app.update(Message::ToggleContextInspector));
    let transition = app.context_inspector_transition.expect("a real collapse");
    app.frame_clock
        .start(transition.started_at + transition.duration() / 2);
    assert!(!app.context_inspector_open && app.context_inspector_visible());
    assert_eq!(app.context_inspector_mount(), Some(InspectorLayout::Docked));
    assert_eq!(
        probe(&app, &renderer, window).close.len(),
        1,
        "collapsing headers must not both own window Close"
    );
    assert_eq!(app.workspace_header_width(), docked_width);
    drop(app.advance_context_inspector_transition(transition.started_at + transition.duration()));
    assert_eq!(app.context_inspector_mount(), None);
    assert_eq!(probe(&app, &renderer, window).close.len(), 1);
    assert!(app.workspace_header_width() > docked_width);

    // Opening owns the same in-flight header: a saved-open preference or
    // close-only handoff cannot make the partially revealed copy interactive.
    drop(app.update(Message::ToggleContextInspector));
    let opening = app.context_inspector_transition.expect("a real opening");
    app.frame_clock
        .start(opening.started_at + opening.duration() / 2);
    assert!(app.context_inspector_open);
    assert_eq!(app.context_inspector_mount(), Some(InspectorLayout::Docked));
    assert_eq!(probe(&app, &renderer, window).close.len(), 1);
    assert_eq!(app.workspace_header_width(), docked_width);
    drop(app.advance_context_inspector_transition(opening.started_at + opening.duration()));
    assert_eq!(probe(&app, &renderer, window).close.len(), 1);
}

fn assert_overlay_close_is_above_the_inspector(
    app: &Cagents,
    renderer: &cosmic::Renderer,
    window: window::Id,
) {
    assert_eq!(
        app.context_inspector_mount(),
        Some(InspectorLayout::Overlay)
    );
    let probe = probe(app, renderer, window);
    assert_eq!(
        probe.close.len(),
        1,
        "the modal foreground must expose Close"
    );
    assert_eq!(
        probe.toggle.len(),
        1,
        "only the actual panel toggle is mounted"
    );
    assert!(
        probe.close[0].y < probe.toggle[0].y,
        "overlay Close belongs above the inspector, not inside its header"
    );
}

#[test]
fn overlay_foreground_keeps_workspace_close_above_both_inspector_headings() {
    let renderer = crate::app::tests::headless_renderer();
    let (mut app, window) = fixture(true, 640.0);
    assert_overlay_close_is_above_the_inspector(&app, &renderer, window);
    // The current Work inspector has a second early-return header that the
    // original donor predates. Open only its local view; do not dispatch the
    // returned Read effect or switch to the main Workboard surface.
    let home = crate::channel_work::Home {
        registry: uuid::Uuid::new_v4(),
        channel: ChannelId::new(),
        creation_ordinal: 1,
    };
    assert!(
        app.channel_work
            .view
            .open(home, "Fixture work".into())
            .is_some()
    );
    assert!(!app.channel_work_is_main());
    assert!(app.channel_work.view.is_open());
    assert_overlay_close_is_above_the_inspector(&app, &renderer, window);
}

#[test]
fn actual_close_buttons_respect_core_policy_and_the_original_window_identity() {
    let renderer = crate::app::tests::headless_renderer();
    for (with_card, width) in [(false, 1600.0), (true, 1600.0), (true, 640.0)] {
        let (mut app, window) = fixture(with_card, width);
        app.core.window.show_close = false;
        assert!(probe(&app, &renderer, window).close.is_empty());
        app.core.window.show_close = true;
        app.core.set_main_window_id(None);
        assert!(probe(&app, &renderer, window).close.is_empty());
        app.core.set_main_window_id(Some(window));
        assert_eq!(probe(&app, &renderer, window).close.len(), 1);
    }
}

#[test]
fn docked_transition_controls_survive_zero_and_clipped_width_for_both_inspector_headers() {
    let renderer = crate::app::tests::headless_renderer();
    for work_header in [false, true] {
        let (mut app, window) = fixture(true, 1600.0);
        app.core.window.show_minimize = true;
        app.core.window.show_maximize = true;
        if work_header {
            let home = crate::channel_work::Home {
                registry: uuid::Uuid::new_v4(),
                channel: ChannelId::new(),
                creation_ordinal: 1,
            };
            assert!(
                app.channel_work
                    .view
                    .open(home, "Fixture work".into())
                    .is_some()
            );
            assert!(!app.channel_work_is_main());
        }
        // Close fully, then reopen through the real production toggle. At
        // the opening's first frame the revealed inspector has zero width.
        for opening in [false, true] {
            drop(app.update(Message::ToggleContextInspector));
            let transition = app.context_inspector_transition.expect("a real transition");
            assert_eq!(app.context_inspector_open, opening);
            for fraction in [0.0, 0.5, 0.99] {
                app.frame_clock
                    .start(transition.started_at + transition.duration().mul_f32(fraction));
                assert_eq!(app.context_inspector_mount(), Some(InspectorLayout::Docked));
                let controls = probe(&app, &renderer, window);
                assert_eq!(
                    controls.close.len(),
                    1,
                    "opening={opening}, frame={fraction}"
                );
                assert_eq!(
                    controls.minimize.len(),
                    usize::from(cosmic::config::show_minimize())
                );
                assert_eq!(
                    controls.maximize.len(),
                    usize::from(cosmic::config::show_maximize())
                );
                assert!(controls.close[0].x > app.window_width - design::WINDOW_CONTROL_SIZE);
            }
            // The same foreground owner must honor policy while mounted.
            app.core.window.show_close = false;
            app.core.window.show_minimize = false;
            app.core.window.show_maximize = false;
            let controls = probe(&app, &renderer, window);
            assert!(
                controls.close.is_empty()
                    && controls.minimize.is_empty()
                    && controls.maximize.is_empty()
            );
            app.core.window.show_close = true;
            app.core.window.show_minimize = true;
            app.core.window.show_maximize = true;
            drop(app.advance_context_inspector_transition(
                transition.started_at + transition.duration(),
            ));
            assert_eq!(probe(&app, &renderer, window).close.len(), 1);
        }
    }
}

/// Rebuild with the actual runtime's named-state handoff while retaining the
/// same unnamed widget state too. Splitting Down and Up across this helper is
/// intentional: a fresh Tree for either event would lose the button's press.
fn retained_pointer_event(
    app: &Cagents,
    renderer: &cosmic::Renderer,
    tree: &mut Tree,
    point: Point,
    event: Event,
) -> Vec<cosmic::Action<Message>> {
    use cosmic::iced::advanced::widget::tree::NAMED;

    let viewport = Rectangle::with_size(Size::new(app.window_width, app.window_height));
    let mut element = cosmic::ApplicationExt::view_main(app);
    NAMED.with(|named| *named.borrow_mut() = tree.take_all_named());
    tree.diff(element.as_widget_mut());
    iced::runtime::user_interface::restore_cached_named_states(tree);
    let node = element.as_widget_mut().layout(
        tree,
        renderer,
        &layout::Limits::new(Size::ZERO, viewport.size()),
    );
    NAMED.with(|named| named.borrow_mut().clear());
    let mut messages = Vec::new();
    element.as_widget_mut().update(
        tree,
        &event,
        Layout::new(&node),
        mouse::Cursor::Available(point),
        renderer,
        &mut clipboard::Null,
        &mut Shell::new(&mut messages),
        &viewport,
    );
    messages
}

#[test]
fn window_close_press_survives_docked_transition_start_and_settle() {
    let renderer = crate::app::tests::headless_renderer();
    for work_header in [false, true] {
        for opening in [false, true] {
            for crossing_settle in [false, true] {
                let (mut app, window) = fixture(true, 1600.0);
                if work_header {
                    let home = crate::channel_work::Home {
                        registry: uuid::Uuid::new_v4(),
                        channel: ChannelId::new(),
                        creation_ordinal: 1,
                    };
                    assert!(
                        app.channel_work
                            .view
                            .open(home, "Fixture work".into())
                            .is_some()
                    );
                    assert!(!app.channel_work_is_main());
                }
                if opening {
                    drop(app.update(Message::ToggleContextInspector));
                    let preparation = app.context_inspector_transition.unwrap();
                    drop(app.advance_context_inspector_transition(
                        preparation.started_at + preparation.duration(),
                    ));
                    assert!(!app.context_inspector_visible());
                }
                if crossing_settle {
                    drop(app.update(Message::ToggleContextInspector));
                    let transition = app.context_inspector_transition.unwrap();
                    app.frame_clock
                        .start(transition.started_at + transition.duration().mul_f32(0.99));
                }

                // Discover the actual Close hit region, then press it on the
                // carried Tree. No application close message is reduced.
                let controls = probe(&app, &renderer, window);
                assert_eq!(controls.close.len(), 1);
                let point = controls.close[0];
                let mut tree = Tree::empty();
                let down = retained_pointer_event(
                    &app,
                    &renderer,
                    &mut tree,
                    point,
                    Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                );
                assert!(
                    !down.iter().any(|message| matches!(
                        message,
                        cosmic::Action::App(Message::WindowCloseRequested(_))
                    )),
                    "Close acts on release, not on Down"
                );

                if crossing_settle {
                    let transition = app.context_inspector_transition.unwrap();
                    drop(app.advance_context_inspector_transition(
                        transition.started_at + transition.duration(),
                    ));
                    assert!(app.context_inspector_transition.is_none());
                } else {
                    drop(app.update(Message::ToggleContextInspector));
                    let transition = app.context_inspector_transition.unwrap();
                    app.frame_clock.start(transition.started_at);
                    assert!(app.context_inspector_transition.is_some());
                }
                assert_eq!(app.context_inspector_open, opening);

                let up = retained_pointer_event(
                    &app,
                    &renderer,
                    &mut tree,
                    point,
                    Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
                );
                let closes: Vec<_> = up
                    .iter()
                    .filter_map(|message| match message {
                        cosmic::Action::App(Message::WindowCloseRequested(actual)) => Some(*actual),
                        _ => None,
                    })
                    .collect();
                assert_eq!(
                    closes,
                    [window],
                    "work={work_header}, opening={opening}, settle={crossing_settle}"
                );
                assert!(!up.iter().any(|message| matches!(
                    message,
                    cosmic::Action::App(
                        Message::ToggleContextInspector
                            | Message::ContextInspector(InspectorAction::Close)
                    )
                )));
            }
        }
    }
}
