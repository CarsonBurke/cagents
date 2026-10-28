//! Pointer regressions through the App, its pane splitter, and COSMIC's frame.
use super::*;
use iced::advanced::{
    clipboard,
    widget::{Operation, operation},
};
use iced::runtime::user_interface::{Cache, UserInterface};
use iced::{Event, Point, Rectangle, Size, Vector, mouse};
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
struct Geometry {
    pane: Rectangle,
    viewport: Rectangle,
    content: Rectangle,
    translation: Vector,
    composer: Rectangle,
    answer: Option<Rectangle>,
    splitter: Option<Rectangle>,
    sidebar: Option<(Rectangle, Rectangle, Vector)>,
    sidebar_splitter: Option<Rectangle>,
}

struct Probe {
    card: CardId,
    pane: Option<Rectangle>,
    reader: Option<(Rectangle, Rectangle, Vector)>,
    composer: Option<Rectangle>,
    answer: Option<Rectangle>,
    splitter: Option<Rectangle>,
    sidebar: Option<(Rectangle, Rectangle, Vector)>,
    sidebar_splitter: Option<Rectangle>,
}

impl Operation for Probe {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
        if id == Some(&workspace_widget::card_widget_id(self.card)) {
            self.pane = Some(bounds);
        }
        if id == Some(&scroll::item_id("answer-15")) {
            self.answer = Some(bounds);
        }
    }

    fn focusable(
        &mut self,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        _: &mut dyn operation::Focusable,
    ) {
        if id == Some(&card_control_id(self.card, "chat-composer")) {
            self.composer = Some(bounds);
        }
        if id == Some(&card_control_id(self.card, "resize")) {
            self.splitter = Some(bounds);
        }
        if id == Some(&sidebar::resize_handle_id()) {
            self.sidebar_splitter = Some(bounds);
        }
    }

    fn scrollable(
        &mut self,
        id: Option<&widget::Id>,
        viewport: Rectangle,
        content: Rectangle,
        translation: Vector,
        _: &mut dyn operation::Scrollable,
    ) {
        if id == Some(&card_control_id(self.card, "chat-transcript")) {
            self.reader = Some((viewport, content, translation));
        }
        if id == Some(&widget::Id::new("sidebar.projects.scroll")) {
            self.sidebar = Some((viewport, content, translation));
        }
    }
}

fn fixture(size: Size, split: bool) -> (Cagents, Target, Arc<ClientHandle>) {
    let items: Vec<_> = (0..16)
        .flat_map(|index| {
            [
                chat::Item {
                    id: format!("prompt-{index}"),
                    role: chat::Role::User,
                    content: ItemContent::Text {
                        text: format!("Read section {index}."),
                        thinking: false,
                        phase: chat::Phase::Final,
                    },
                    at: None,
                    started_at: None,
                },
                chat::Item {
                    id: format!("answer-{index}"),
                    role: chat::Role::Assistant,
                    content: ItemContent::Text {
                        text: format!(
                            "Section {index}.\n\n{}",
                            "A short paragraph makes this an overflowing real conversation.\n\n"
                                .repeat(3)
                        ),
                        thinking: false,
                        phase: chat::Phase::Final,
                    },
                    at: None,
                    started_at: None,
                },
            ]
        })
        .collect();
    // Uses a fixture ClientHandle and prepares the real rich transcript. It
    // launches no provider, daemon, socket, or terminal process.
    let (mut runtime, handle) = view::prepared_runtime_for_test(&items);
    let mut app = crate::app::tests::context_inspector_test_app(true);
    app.context_inspector_open = false;
    app.window_width = size.width;
    app.window_height = size.height;
    let card = app.workspace.focused().unwrap();
    runtime.target.card = card;
    let target = runtime.target;
    app.card_state_mut(card).unwrap().kind = CardKind::Chat(runtime.session.clone());
    app.cards.insert(card, CardRuntime::Chat(Box::new(runtime)));
    if split {
        app.settings.default_native_pane_fraction = 0.5;
        app.card_state_mut(card).unwrap().width_fraction = 0.5;
        app.workspace
            .set_card_width(card, CardWidth::Fraction(0.5))
            .unwrap();
        drop(app.add_utility_card(CardKind::Settings));
        drop(app.update(Message::FocusCard(card)));
    }
    (app, target, handle)
}

fn frame(
    app: &Cagents,
    card: CardId,
    size: Size,
    renderer: &mut cosmic::Renderer,
    cache: Cache,
    event: Event,
    pointer: Option<Point>,
) -> (Cache, Geometry, Vec<Message>) {
    let mut ui = UserInterface::build(
        cosmic::ApplicationExt::view_main(app),
        size,
        cache,
        renderer,
    );
    let cursor = pointer.map_or(mouse::Cursor::Unavailable, mouse::Cursor::Available);
    let mut messages = Vec::new();
    ui.update(
        &[event],
        cursor,
        renderer,
        &mut clipboard::Null,
        &mut messages,
    );
    ui.draw(
        renderer,
        &cosmic::theme::active(),
        &iced::advanced::renderer::Style::default(),
        cursor,
    );
    let mut probe = Probe {
        card,
        pane: None,
        reader: None,
        composer: None,
        answer: None,
        splitter: None,
        sidebar: None,
        sidebar_splitter: None,
    };
    ui.operate(renderer, &mut probe);
    let (viewport, content, translation) = probe.reader.expect("actual App transcript");
    let geometry = Geometry {
        pane: probe.pane.expect("actual workspace pane"),
        viewport,
        content,
        translation,
        composer: probe.composer.expect("actual composer"),
        answer: probe.answer,
        splitter: probe.splitter,
        sidebar: probe.sidebar,
        sidebar_splitter: probe.sidebar_splitter,
    };
    let messages = messages
        .into_iter()
        .filter_map(|message| match message {
            cosmic::Action::App(message) => Some(message),
            _ => None,
        })
        .collect();
    (ui.into_cache(), geometry, messages)
}

fn reduce(app: &mut Cagents, messages: Vec<Message>) {
    for message in messages {
        drop(app.update(message));
    }
}

fn settle(
    app: &mut Cagents,
    card: CardId,
    size: Size,
    renderer: &mut cosmic::Renderer,
) -> (Cache, Geometry) {
    let mut cache = Cache::default();
    let mut measured = None;
    // Carry native widget state while visible virtual rows settle and the
    // actual viewport measurement reaches the App's workspace model. Advance
    // the production frame clock too: redraw count does not finish animations.
    let start = Instant::now();
    for index in 0..12 {
        let now = start + Duration::from_millis(index * 250);
        drop(app.update(Message::Frame(now)));
        let (next, geometry, messages) = frame(
            app,
            card,
            size,
            renderer,
            cache,
            Event::Window(iced::window::Event::RedrawRequested(now)),
            None,
        );
        cache = next;
        measured = Some(geometry);
        reduce(app, messages);
    }
    assert!(!app.workspace.is_animating(), "fixture layout must settle");
    (cache, measured.unwrap())
}

fn thumb_center(geometry: Geometry) -> Point {
    reader_thumb_center(geometry.viewport, geometry.content, geometry.translation)
}

fn reader_thumb_center(viewport: Rectangle, content: Rectangle, translation: Vector) -> Point {
    assert!(content.height > viewport.height);
    // Locate the configured native thumb from measured scroll geometry,
    // rather than aiming at a guessed pane or reading-column edge.
    let ratio = viewport.height / content.height;
    let thumb_height = (viewport.height * ratio).max(2.0);
    Point::new(
        viewport.x + viewport.width - design::SCROLLBAR_GUTTER / 2.0,
        viewport.y + translation.y * ratio + thumb_height / 2.0,
    )
}

fn assert_reading_column(geometry: Geometry) {
    // The approved narrow layout reserves sixteen pixels on each side for
    // resizing and the complete rail. Wide prose keeps its 720-pixel measure.
    // These assertions do not reuse the new production clearance/inset values.
    let expected_width = (geometry.pane.width - 32.0)
        .max(0.0)
        .min(design::READABLE_MEASURE);
    let expected_x = geometry.pane.x + (geometry.pane.width - expected_width) / 2.0;
    assert!(
        (geometry.composer.x - expected_x).abs() < 0.1,
        "{geometry:?}"
    );
    assert!(
        (geometry.composer.width - expected_width).abs() < 0.1,
        "{geometry:?}"
    );
    let answer = geometry
        .answer
        .expect("last answer is mounted at the live end");
    assert!((answer.x - geometry.composer.x).abs() < 0.1);
    assert!((answer.width - geometry.composer.width).abs() < 0.1);
}

fn assert_chat_thumb_drag(width: f32, split: bool) {
    let mut renderer = crate::app::tests::headless_renderer();
    let size = Size::new(width, 900.0);
    let (mut app, target, _handle) = fixture(size, split);
    let (mut cache, before) = settle(&mut app, target.card, size, &mut renderer);
    let origin = thumb_center(before);
    let destination = Point::new(origin.x, origin.y - 60.0);
    let mut after = before;
    let mut scrolled = false;
    for (event, point) in [
        (mouse::Event::CursorMoved { position: origin }, origin),
        (mouse::Event::ButtonPressed(mouse::Button::Left), origin),
        (
            mouse::Event::CursorMoved {
                position: destination,
            },
            destination,
        ),
        (
            mouse::Event::ButtonReleased(mouse::Button::Left),
            destination,
        ),
    ] {
        let (next, geometry, messages) = frame(
            &app,
            target.card,
            size,
            &mut renderer,
            cache,
            Event::Mouse(event),
            Some(point),
        );
        cache = next;
        after = geometry;
        for message in &messages {
            assert!(
                !matches!(
                    message,
                    Message::ResizeCard { .. } | Message::ResizeCardFinished(_)
                ),
                "grabbing the transcript thumb must not start a pane resize"
            );
            scrolled |= matches!(message, Message::ChatUi(Action::Scrolled(actual, _, _, true)) if *actual == target);
        }
        reduce(&mut app, messages);
    }
    assert!(
        after.translation.y < before.translation.y - 1.0,
        "the actual thumb drag must move the native reader: {before:?} -> {after:?}"
    );
    assert!(
        scrolled,
        "native drag must publish accepted reader movement"
    );
    if !split {
        // The actual native window reserves its outer eight logical
        // pixels at scale 1 for resizing. Headless UI cannot execute that
        // interceptor, so this assertion is its geometric prerequisite.
        assert!(
            before.viewport.x + before.viewport.width <= size.width - 8.0,
            "the complete rail must clear the native resize band: {before:?}"
        );
    }
    assert_reading_column(before);
}

#[test]
fn native_app_chat_thumb_drag_clears_the_window_resize_band_at_narrow_and_wide_widths() {
    for width in [900.0, 1440.0] {
        assert_chat_thumb_drag(width, false);
    }
}

#[test]
fn native_app_split_chat_thumb_drag_scrolls_without_resizing_the_pane() {
    assert_chat_thumb_drag(1440.0, true);
}

#[test]
fn native_app_chat_keeps_the_outer_pane_resize_gesture() {
    let size = Size::new(1440.0, 900.0);
    let (mut app, target, _handle) = fixture(size, true);
    let mut renderer = crate::app::tests::headless_renderer();
    let (mut cache, before) = settle(&mut app, target.card, size, &mut renderer);
    let origin = before
        .splitter
        .expect("actual trailing pane splitter")
        .center();
    let destination = Point::new(origin.x + 32.0, origin.y);
    let mut resized = false;
    let mut finished = false;
    for (event, point) in [
        (mouse::Event::CursorMoved { position: origin }, origin),
        (mouse::Event::ButtonPressed(mouse::Button::Left), origin),
        (
            mouse::Event::CursorMoved {
                position: destination,
            },
            destination,
        ),
        (
            mouse::Event::ButtonReleased(mouse::Button::Left),
            destination,
        ),
    ] {
        let (next, _, messages) = frame(
            &app,
            target.card,
            size,
            &mut renderer,
            cache,
            Event::Mouse(event),
            Some(point),
        );
        cache = next;
        resized |= messages.iter().any(
            |message| matches!(message, Message::ResizeCard { card, .. } if *card == target.card),
        );
        finished |= messages.iter().any(
            |message| matches!(message, Message::ResizeCardFinished(card) if *card == target.card),
        );
        reduce(&mut app, messages);
    }
    assert!(
        resized,
        "outer splitter drag must still publish pane resizing"
    );
    assert!(
        finished,
        "outer splitter release must still finish its gesture"
    );
    assert!(
        app.workspace.geometry().card(target.card).unwrap().width() > before.pane.width + 1.0,
        "the actual App reducer must apply the pointer-driven width"
    );
}

#[test]
fn native_app_expanded_sidebar_thumb_and_outer_resize_handle_have_distinct_gestures() {
    let size = Size::new(1440.0, 900.0);
    let (mut app, target, _handle) = fixture(size, false);
    app.chrome_ui.projects_collapsed = false;
    app.settings.sidebar_width = 320.0;
    app.sidebar_model = sidebar::SidebarModel::ready(
        sidebar::SidebarMode::Expanded,
        (0..48)
            .map(|index| {
                sidebar::SidebarProject::new(
                    ProjectId::new(),
                    crate::repository::RepositoryId(Uuid::new_v4()),
                    format!("Project {index:02}"),
                    PathBuf::from(format!("/fixture/project-{index:02}")),
                    Vec::new(),
                    Vec::new(),
                )
            })
            .collect(),
    );
    let mut renderer = crate::app::tests::headless_renderer();
    let (mut cache, before) = settle(&mut app, target.card, size, &mut renderer);
    let (viewport, content, translation) = before.sidebar.expect("actual App project list");
    let origin = reader_thumb_center(viewport, content, translation);
    let destination = Point::new(origin.x, origin.y + 60.0);
    let mut after = before;
    for (event, point) in [
        (mouse::Event::CursorMoved { position: origin }, origin),
        (mouse::Event::ButtonPressed(mouse::Button::Left), origin),
        (
            mouse::Event::CursorMoved {
                position: destination,
            },
            destination,
        ),
        (
            mouse::Event::ButtonReleased(mouse::Button::Left),
            destination,
        ),
    ] {
        let (next, geometry, messages) = frame(
            &app,
            target.card,
            size,
            &mut renderer,
            cache,
            Event::Mouse(event),
            Some(point),
        );
        cache = next;
        after = geometry;
        assert!(!messages.iter().any(|message| matches!(
            message,
            Message::Sidebar(
                sidebar::SidebarAction::Resize { .. } | sidebar::SidebarAction::ResizeFinished
            )
        )));
        reduce(&mut app, messages);
    }
    assert!(
        after.sidebar.unwrap().2.y > translation.y + 1.0,
        "the actual project-list thumb must scroll instead of resizing the sidebar"
    );
    assert_eq!(app.settings.sidebar_width, 320.0);

    let origin = after
        .sidebar_splitter
        .expect("actual sidebar resize handle")
        .center();
    let destination = Point::new(origin.x + 24.0, origin.y);
    let mut resized = false;
    let mut finished = false;
    for (event, point) in [
        (mouse::Event::CursorMoved { position: origin }, origin),
        (mouse::Event::ButtonPressed(mouse::Button::Left), origin),
        (
            mouse::Event::CursorMoved {
                position: destination,
            },
            destination,
        ),
        (
            mouse::Event::ButtonReleased(mouse::Button::Left),
            destination,
        ),
    ] {
        let (next, _, messages) = frame(
            &app,
            target.card,
            size,
            &mut renderer,
            cache,
            Event::Mouse(event),
            Some(point),
        );
        cache = next;
        resized |= messages.iter().any(|message| {
            matches!(
                message,
                Message::Sidebar(sidebar::SidebarAction::Resize { .. })
            )
        });
        finished |= messages.iter().any(|message| {
            matches!(
                message,
                Message::Sidebar(sidebar::SidebarAction::ResizeFinished)
            )
        });
        reduce(&mut app, messages);
    }
    assert!(
        resized && finished,
        "outer sidebar resize gesture remains usable"
    );
    assert!(app.settings.sidebar_width > 321.0);
}
