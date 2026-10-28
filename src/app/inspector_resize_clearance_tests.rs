//! Full-shell pointer and geometry checks for inspector/body resize clearance.
use super::*;
use iced::advanced::{
    clipboard,
    widget::{Operation, operation},
};
use iced::runtime::user_interface::{Cache, UserInterface};
use iced::{Event, Point, Rectangle, Size, Vector, mouse};
use uuid::Uuid;

const SIZE: Size = Size::new(1440.0, 900.0);

#[derive(Clone, Copy, Debug)]
struct Reader {
    viewport: Rectangle,
    content: Rectangle,
    translation: Vector,
}

#[derive(Debug)]
struct Geometry {
    inspector: Reader,
    inspector_splitter: Rectangle,
    sidebar: Reader,
    sidebar_splitter: Rectangle,
    card: Rectangle,
    editor: Option<Rectangle>,
    toggle: Rectangle,
    tabs: Vec<(String, Rectangle)>,
    close: Rectangle,
}

struct Probe {
    card: CardId,
    readers: Vec<Reader>,
    sidebar: Option<Reader>,
    inspector_splitter: Option<Rectangle>,
    sidebar_splitter: Option<Rectangle>,
    card_bounds: Option<Rectangle>,
    editor: Option<Rectangle>,
    toggle: Option<Rectangle>,
}

impl Operation for Probe {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }

    fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
        if id == Some(&workspace_widget::card_widget_id(self.card)) {
            self.card_bounds = Some(bounds);
        }
    }

    fn focusable(
        &mut self,
        id: Option<&widget::Id>,
        bounds: Rectangle,
        _: &mut dyn operation::Focusable,
    ) {
        if id == Some(&widget::Id::new("context-inspector.resize-handle")) {
            self.inspector_splitter = Some(bounds);
        }
        if id == Some(&sidebar::resize_handle_id()) {
            self.sidebar_splitter = Some(bounds);
        }
        if id == Some(&repository_widget::editor_focus_id(self.card)) {
            self.editor = Some(bounds);
        }
        if id == Some(&context_inspector_toggle_id()) {
            self.toggle = Some(bounds);
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
        let reader = Reader {
            viewport,
            content,
            translation,
        };
        if id == Some(&widget::Id::new("sidebar.projects.scroll")) {
            self.sidebar = Some(reader);
        }
        self.readers.push(reader);
    }
}

fn fixture() -> (Cagents, CardId) {
    let mut app = tests::context_inspector_test_app(true);
    app.window_width = SIZE.width;
    app.window_height = SIZE.height;
    app.core
        .set_main_window_id(Some(iced::window::Id::RESERVED));
    app.context_inspector_open = true;
    app.context_inspector_transition = None;
    app.context_tab = ContextTab::Details;
    app.chrome_ui.projects_collapsed = false;
    app.settings.inspector_width = 360.0;
    app.settings.sidebar_width = 280.0;
    app.settings.show_status_bar = false;
    let card = app.workspace.focused().unwrap();
    let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind else {
        panic!("the App fixture starts with a terminal descriptor");
    };
    // Production inspector facts render this command; no terminal, provider
    // or daemon is started to make a genuinely overflowing Details panel.
    session.launch.arguments = (0..160)
        .map(|index| format!("argument-{index:03}-with-readable-words"))
        .collect();
    app.sidebar_model = sidebar::SidebarModel::ready(
        SidebarMode::Expanded,
        (0..40)
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
    assert_eq!(app.context_inspector_mount(), Some(InspectorLayout::Docked));
    // The supported fixture Core is at actual scale 1. Its scale setter is
    // private to COSMIC; 0.8 remains a native-runner validation requirement.
    assert_eq!(app.core.scale_factor(), 1.0);
    (app, card)
}

fn frame(
    app: &Cagents,
    card: CardId,
    renderer: &mut cosmic::Renderer,
    cache: Cache,
    event: Option<(mouse::Event, Point)>,
) -> (Cache, Geometry, Vec<Message>) {
    let mut ui = UserInterface::build(
        cosmic::ApplicationExt::view_main(app),
        SIZE,
        cache,
        renderer,
    );
    let (event, cursor) = event.map_or_else(
        || {
            (
                Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
                mouse::Cursor::Unavailable,
            )
        },
        |(event, point)| (Event::Mouse(event), mouse::Cursor::Available(point)),
    );
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
        readers: Vec::new(),
        sidebar: None,
        inspector_splitter: None,
        sidebar_splitter: None,
        card_bounds: None,
        editor: None,
        toggle: None,
    };
    ui.operate(renderer, &mut probe);
    let splitter = probe.inspector_splitter.expect("actual inspector splitter");
    let inspector = probe
        .readers
        .iter()
        .copied()
        .filter(|reader| reader.viewport.x >= splitter.x)
        .max_by(|a, b| a.viewport.x.total_cmp(&b.viewport.x))
        .expect("actual Details scroll viewport");
    let toggle = probe.toggle.expect("actual inspector header toggle");
    let accessible = ui.a11y_nodes(mouse::Cursor::Unavailable);
    let mut tabs = Vec::new();
    let mut header_buttons = Vec::new();
    for node in accessible.root().iter().chain(accessible.children()) {
        if node.node().role() != iced_accessibility::accesskit::Role::Button {
            continue;
        }
        let Some(bounds) = node.node().bounds() else {
            continue;
        };
        let bounds = Rectangle::new(
            Point::new(bounds.x0 as f32, bounds.y0 as f32),
            Size::new(
                (bounds.x1 - bounds.x0) as f32,
                (bounds.y1 - bounds.y0) as f32,
            ),
        );
        if bounds.x > splitter.x && bounds.y < toggle.y + toggle.height {
            header_buttons.push(bounds);
        }
        if let Some(label) = node.node().label()
            && label.ends_with(" panel tab")
        {
            tabs.push((label.to_owned(), bounds));
        }
    }
    tabs.sort_by(|a, b| a.0.cmp(&b.0));
    let close = header_buttons
        .into_iter()
        .max_by(|a, b| (a.x + a.width).total_cmp(&(b.x + b.width)))
        .expect("native header's trailing window control");
    let geometry = Geometry {
        inspector,
        inspector_splitter: splitter,
        sidebar: probe.sidebar.expect("actual project-list viewport"),
        sidebar_splitter: probe.sidebar_splitter.expect("actual sidebar splitter"),
        card: probe.card_bounds.expect("actual workspace card bounds"),
        editor: probe.editor,
        toggle,
        tabs,
        close,
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

fn settle(app: &mut Cagents, card: CardId, renderer: &mut cosmic::Renderer) -> (Cache, Geometry) {
    let mut cache = Cache::default();
    let mut geometry = None;
    for _ in 0..3 {
        let (next, measured, messages) = frame(app, card, renderer, cache, None);
        cache = next;
        geometry = Some(measured);
        reduce(app, messages);
    }
    assert!(!app.workspace.is_animating());
    assert!(app.context_inspector_transition.is_none());
    (cache, geometry.unwrap())
}

fn gesture(origin: Point, destination: Point) -> [(mouse::Event, Point); 4] {
    [
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
    ]
}

#[test]
fn docked_inspector_native_thumb_scrolls_clear_of_right_and_bottom_resize_bands() {
    let (mut app, card) = fixture();
    let mut renderer = tests::headless_renderer();
    let (mut cache, before) = settle(&mut app, card, &mut renderer);
    let reader = before.inspector;
    assert!(reader.content.height > reader.viewport.height);
    let ratio = reader.viewport.height / reader.content.height;
    let origin = Point::new(
        reader.viewport.x + reader.viewport.width - design::SCROLLBAR_GUTTER / 2.0,
        reader.viewport.y
            + reader.translation.y * ratio
            + (reader.viewport.height * ratio).max(2.0) / 2.0,
    );
    let destination = Point::new(origin.x, origin.y + 60.0);
    let width = app.settings.inspector_width;
    let mut after = reader;
    for event in gesture(origin, destination) {
        let (next, geometry, messages) = frame(&app, card, &mut renderer, cache, Some(event));
        cache = next;
        after = geometry.inspector;
        assert!(!messages.iter().any(|message| matches!(
            message,
            Message::ContextInspector(
                InspectorAction::Resize { .. } | InspectorAction::ResizeFinished
            )
        )));
        reduce(&mut app, messages);
    }
    assert!(after.translation.y > reader.translation.y + 1.0);
    assert_eq!(app.settings.inspector_width, width);
    // These are prerequisites for native border interception, which headless
    // Iced cannot run. Actual South/East window-resize gestures remain native.
    assert!(reader.viewport.x + reader.viewport.width <= SIZE.width - 8.0);
    assert!(reader.viewport.y + reader.viewport.height <= SIZE.height - 8.0);
    assert!(
        reader.viewport.x
            >= before.inspector_splitter.x + before.inspector_splitter.width + 8.0 - 0.1
    );
}

#[test]
fn docked_inspector_keeps_its_leading_resize_gesture_and_native_close_control() {
    let (mut app, card) = fixture();
    let mut renderer = tests::headless_renderer();
    let (mut cache, before) = settle(&mut app, card, &mut renderer);
    let origin = before.inspector_splitter.center();
    let initial_width = app.settings.inspector_width;
    let mut resized = false;
    let mut finished = false;
    let mut after = before;
    for event in gesture(origin, Point::new(origin.x - 24.0, origin.y)) {
        let (next, measured, messages) = frame(&app, card, &mut renderer, cache, Some(event));
        cache = next;
        after = measured;
        resized |= messages.iter().any(|message| {
            matches!(
                message,
                Message::ContextInspector(InspectorAction::Resize { .. })
            )
        });
        finished |= messages.iter().any(|message| {
            matches!(
                message,
                Message::ContextInspector(InspectorAction::ResizeFinished)
            )
        });
        reduce(&mut app, messages);
    }
    assert!(resized && finished);
    assert!(app.settings.inspector_width > initial_width + 1.0);
    assert_eq!(after.tabs.len(), ContextTab::ALL.len());
    let close = after.close.center();
    let mut closed = false;
    for event in gesture(close, close) {
        let (next, _, messages) = frame(&app, card, &mut renderer, cache, Some(event));
        cache = next;
        closed |= messages.iter().any(|message| matches!(message, Message::WindowCloseRequested(id) if *id == iced::window::Id::RESERVED));
        // Observe the actual native control's publication without closing the
        // test App or executing any window command.
    }
    assert!(closed, "native header Close remains mounted and pressable");
}

#[test]
fn shared_body_bottom_clearance_covers_cards_sidebar_and_inspector_without_a_second_status_gap() {
    let (mut app, card) = fixture();
    // An actual native editor with an overflowing horizontal canvas also
    // reaches the body bottom; this is not a synthetic Space scroll surface.
    let (mut runtime, _) =
        RepositoryRuntime::open_fixture(card, "wide.txt", &"wide token ".repeat(1000));
    runtime.set_wrap_lines(false);
    let navigation = runtime.navigation().expect("loaded fixture navigation");
    app.card_state_mut(card).unwrap().kind = CardKind::Repository(crate::model::RepositoryView {
        project: navigation.project,
        repository: "/repo/.git".into(),
        worktree: navigation.worktree,
        selected_file: navigation.selected_file,
    });
    app.cards
        .insert(card, CardRuntime::Repository(Box::new(runtime)));
    let mut renderer = tests::headless_renderer();
    let (cache, off) = settle(&mut app, card, &mut renderer);
    assert!(off.sidebar.content.height > off.sidebar.viewport.height);
    let editor = off.editor.expect("actual native editor viewport");
    let horizontal = |app: &Cagents| match &app.cards[&card] {
        CardRuntime::Repository(runtime) => {
            runtime
                .navigation()
                .unwrap()
                .editor
                .unwrap()
                .scroll_horizontal
        }
        _ => panic!("the actual editor remains mounted"),
    };
    let initial_horizontal = horizontal(&app);
    let (cache, _, messages) = frame(
        &app,
        card,
        &mut renderer,
        cache,
        Some((
            mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: -80.0, y: 0.0 },
            },
            editor.center(),
        )),
    );
    reduce(&mut app, messages);
    assert!(
        horizontal(&app) > initial_horizontal + 1.0,
        "the actual native editor canvas must scroll horizontally"
    );
    let bottom = |rect: Rectangle| rect.y + rect.height;
    // The real COSMIC floating frame contributes one pixel. OFF reserves the
    // eight-pixel native band once; ON uses the normal 24-pixel status bar.
    for rect in [
        off.card,
        off.sidebar.viewport,
        off.sidebar_splitter,
        off.inspector.viewport,
        off.inspector_splitter,
        editor,
    ] {
        assert!(
            (bottom(rect) - (SIZE.height - 1.0 - 8.0)).abs() < 0.1,
            "{rect:?}"
        );
    }
    app.settings.show_status_bar = true;
    let (_, on, messages) = frame(&app, card, &mut renderer, cache, None);
    reduce(&mut app, messages);
    for rect in [
        on.card,
        on.sidebar.viewport,
        on.sidebar_splitter,
        on.inspector.viewport,
        on.inspector_splitter,
        on.editor.expect("actual native editor viewport"),
    ] {
        assert!(
            (bottom(rect) - (SIZE.height - 1.0 - 24.0)).abs() < 0.1,
            "{rect:?}"
        );
    }
    assert_eq!(off.toggle, on.toggle);
    assert_eq!(off.close, on.close);
    assert_eq!(
        off.tabs, on.tabs,
        "bottom clearance must not move or replace inspector tabs"
    );
}
