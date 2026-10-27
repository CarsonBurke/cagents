use super::*;
use cosmic::Application as _;
use iced::advanced::{
    clipboard,
    widget::{Operation, operation},
};
use iced::runtime::{UserInterface, user_interface::Cache};

fn gallery() -> Gallery {
    Gallery {
        identity: next_gallery_identity(),
        entries: (0..10)
            .map(|index| Entry {
                id: format!("image-{index}"),
                label: format!("Image {}", index + 1),
                handle: iced::widget::image::Handle::from_rgba(1, 1, vec![index, 0, 0, 255]),
                external: Url::from_file_path(format!("/fixture/image-{index}.png")).ok(),
            })
            .collect(),
        selected: 0,
        reset: 0,
        actual_size: false,
        target: Some(Target {
            card: CardId::new(),
            generation: 0,
        }),
        agent: None,
    }
}
fn label(command: ViewerCommand) -> String {
    match command {
        ViewerCommand::Close => "Close image (Esc)".into(),
        ViewerCommand::Next(true) => "Next image (Right)".into(),
        ViewerCommand::Next(false) => "Previous image (Left)".into(),
        ViewerCommand::Fit => "Fit".into(),
        ViewerCommand::ActualSize => "100%".into(),
        ViewerCommand::External(_) => "Open externally".into(),
        ViewerCommand::Select(index) => {
            format!("View Image {} · image {} of 10", index + 1, index + 1)
        }
    }
}
#[derive(Default)]
struct Focus(Option<widget::Id>);
impl Operation for Focus {
    fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
        visit(self);
    }
    fn focusable(
        &mut self,
        id: Option<&widget::Id>,
        _: iced::Rectangle,
        state: &mut dyn operation::Focusable,
    ) {
        if state.is_focused() {
            assert!(self.0.is_none(), "one native control owns focus");
            self.0 = id.cloned();
        }
    }
}
fn bounds(
    ui: &UserInterface<'_, Message, cosmic::Theme, cosmic::Renderer>,
    command: ViewerCommand,
) -> iced_accessibility::accesskit::Rect {
    let nodes = ui.a11y_nodes(iced::mouse::Cursor::Unavailable);
    nodes
        .root()
        .iter()
        .chain(nodes.children())
        .find(|node| {
            node.node().role() == iced_accessibility::accesskit::Role::Button
                && node.node().label() == Some(label(command).as_str())
                && !node.node().is_disabled()
        })
        .unwrap_or_else(|| panic!("enabled native gallery control {}", label(command)))
        .node()
        .bounds()
        .unwrap()
}

#[test]
fn gallery_native_press_and_focus_do_not_cross_replacement_or_recycled_sources() {
    let mut renderer = crate::app::tests::headless_renderer();
    let viewport = iced::Size::new(800.0, 600.0);
    let commands = [
        ViewerCommand::Close,
        ViewerCommand::Next(false),
        ViewerCommand::Next(true),
        ViewerCommand::Fit,
        ViewerCommand::ActualSize,
        ViewerCommand::External(0),
        ViewerCommand::Select(0),
    ];
    // replacement resets every command; ordinary redraw retains each real
    // native press and focus. Additional transitions keep the same gallery
    // while replacing External's source or recycling the thumbnail strip.
    for (command, transition) in commands
        .into_iter()
        .flat_map(|command| [(command, 0), (command, 1)])
        .chain([
            (ViewerCommand::External(0), 2),
            (ViewerCommand::Select(0), 3),
        ])
    {
        let mut first = gallery();
        let first_identity = first.identity;
        let first_handle = first.selected_handle_id();
        let second_handle = first.entries[1].handle.id();
        let eighth_handle = first.entries[7].handle.id();
        let (cache, pointer) = {
            let mut ui = UserInterface::build(first.view(), viewport, Cache::new(), &mut renderer);
            let hit = bounds(&ui, command);
            assert!(
                hit.x0 >= 0.0
                    && hit.y0 >= 0.0
                    && hit.x1 <= f64::from(viewport.width)
                    && hit.y1 <= f64::from(viewport.height)
                    && hit.x1 > hit.x0
                    && hit.y1 > hit.y0
            );
            let pointer = iced::Point::new(
                ((hit.x0 + hit.x1) / 2.0) as f32,
                ((hit.y0 + hit.y1) / 2.0) as f32,
            );
            let mut messages = Vec::new();
            ui.update(
                &[iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                    iced::mouse::Button::Left,
                ))],
                iced::mouse::Cursor::Available(pointer),
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert!(
                messages.is_empty(),
                "gallery mouseDown must not invoke a command: {messages:?}"
            );
            let id = first.control_id(command);
            ui.operate(
                &renderer,
                &mut operation::focusable::focus::<()>(id.clone()),
            );
            let mut focus = Focus::default();
            ui.operate(&renderer, &mut focus);
            assert_eq!(focus.0, Some(id));
            (ui.into_cache(), pointer)
        };
        let current_command = match transition {
            0 => command,
            1 => {
                let target = first.target;
                first = gallery();
                first.target = target;
                assert_ne!(first.identity, first_identity);
                assert_ne!(first.selected_handle_id(), first_handle);
                command
            }
            2 => {
                first.select(1);
                ViewerCommand::External(1)
            }
            3 => {
                first.select(7);
                ViewerCommand::Select(3)
            }
            _ => unreachable!(),
        };
        match transition {
            0 => assert_eq!(first.selected_handle_id(), first_handle),
            2 => assert_eq!(first.selected_handle_id(), second_handle),
            3 => assert_eq!(first.selected_handle_id(), eighth_handle),
            _ => {}
        }
        let mut ui = UserInterface::build(first.view(), viewport, cache, &mut renderer);
        let hit = bounds(&ui, current_command);
        assert!(
            f64::from(pointer.x) >= hit.x0
                && f64::from(pointer.x) <= hit.x1
                && f64::from(pointer.y) >= hit.y0
                && f64::from(pointer.y) <= hit.y1,
            "same actual hit geometry required for {command:?} transition {transition}: {hit:?}"
        );
        let mut focus = Focus::default();
        ui.operate(&renderer, &mut focus);
        assert_eq!(
            focus.0,
            (transition == 0).then(|| first.control_id(current_command))
        );
        let mut messages = Vec::new();
        ui.update(
            &[iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            ))],
            iced::mouse::Cursor::Available(pointer),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        let invoked: Vec<_> = messages
            .iter()
            .map(|message| match message {
                Message::ChatUi(Action::ImageViewer(identity, command)) => (*identity, *command),
                message => panic!("unexpected gallery release message: {message:?}"),
            })
            .collect();
        if transition == 0 {
            assert_eq!(
                invoked,
                vec![(first_identity, command)],
                "unchanged gallery remains usable"
            );
        } else {
            assert!(
                invoked.is_empty(),
                "old native gesture cannot invoke a replacement command: {invoked:?}"
            );
        }
    }
}

#[test]
fn queued_external_callback_cannot_open_a_newly_selected_image() {
    let mut app = crate::app::tests::context_inspector_test_app(false);
    drop(app.new_native_chat(Provider::Codex, Some(PathBuf::from("/fixture"))));
    let card = app.workspace.focused().unwrap();
    drop(app.update(Message::Viewport {
        generation: app.workspace_view_generation,
        event: WorkspaceViewportEvent::Resized(800.0),
    }));
    assert_eq!(
        app.workspace.visibility(card),
        Some(VisibilityClass::Visible)
    );
    let CardRuntime::Chat(runtime) = app.cards.get_mut(&card).unwrap() else {
        panic!("draft chat")
    };
    let target = runtime.target;
    assert!(runtime.visible.load(Ordering::Relaxed));
    runtime.composer = text_editor::Content::with_text("Keep this gallery feedback");
    runtime.attachments = vec![Attachment {
        path: PathBuf::from("/fixture/attached.png"),
        reference: None,
    }];
    let mut source = gallery();
    source.target = Some(target);
    assert!(
        source.accepts(source.identity, Some(runtime), ViewerCommand::External(0)),
        "the current source is admitted"
    );
    let queued = source.callback(ViewerCommand::External(0));
    let original = source.selected_handle_id();
    source.select(1);
    let selected = source.selected_handle_id();
    assert_ne!(original, selected);
    assert!(!source.accepts(source.identity, Some(runtime), ViewerCommand::External(0)));
    assert!(source.accepts(source.identity, Some(runtime), ViewerCommand::External(1)));
    assert_eq!(
        source.url().unwrap().to_file_path().unwrap(),
        PathBuf::from("/fixture/image-1.png")
    );
    app.chat_ui.image_viewer = Some(source);
    assert!(
        app.image_viewer_owns_input(),
        "global input ownership admits the actual gallery"
    );
    let task = app.update(Message::ChatUi(queued));
    assert_eq!(
        task.units(),
        0,
        "a stale source must not schedule an OS external-open future or focus operation"
    );
    drop(task);
    assert!(app.image_viewer_owns_input());
    let current = app
        .chat_ui
        .image_viewer
        .as_ref()
        .unwrap()
        .callback(ViewerCommand::External(1));
    let task = app.update(Message::ChatUi(current));
    assert_eq!(
        task.units(),
        1,
        "the actual current source schedules exactly its external-open future"
    );
    // Never poll this future: no OS/file action belongs in this regression.
    drop(task);
    let source = app.chat_ui.image_viewer.as_ref().unwrap();
    assert_eq!(source.selected_handle_id(), selected);
    assert_eq!(source.reset, 1);
    let CardRuntime::Chat(runtime) = &app.cards[&card] else {
        unreachable!()
    };
    assert_eq!(runtime.composer.text(), "Keep this gallery feedback");
    assert_eq!(runtime.attachments.len(), 1);
    assert_eq!(
        runtime.attachments[0].path,
        PathBuf::from("/fixture/attached.png")
    );
}
