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
    }
    Probe { close, toggle }
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
